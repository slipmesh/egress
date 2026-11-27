//! On one node, every `Egress` that names it resolved on its own schedule, with its status and
//! metrics kept. Nothing is installed yet.

use crate::crd::{Egress, EgressSpec};
use crate::resolve::{self, Answer, Asked, Resolver};
use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::Api;
use kube::api::{Patch, PatchParams};
use kube::runtime::watcher;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use serde_json::{Value, json};
use sliplib_kube::{Agent, ApiTimeouts};
use sliplib_net::IpNet;
use sliplib_runtime::Shutdown;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;

/// After a query gave up, the list is tried again after this.
const AFTER_FAILURE: Duration = Duration::from_secs(180);

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ListLabels {
    egress: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct FamilyLabels {
    egress: String,
    family: String,
}

/// Each list's counts, its last resolve and its failures.
pub struct Metrics {
    pub registry: Registry,
    prefixes: Family<FamilyLabels, Gauge>,
    last_resolve: Family<ListLabels, Gauge>,
    errors: Family<ListLabels, Counter>,
}

impl Default for Metrics {
    fn default() -> Self {
        let prefixes = Family::default();
        let last_resolve = Family::default();
        let errors = Family::default();
        let mut registry = Registry::with_prefix("slipmesh_egress");
        registry.register(
            "prefixes",
            "Prefixes of the list, by family",
            prefixes.clone(),
        );
        registry.register(
            "last_resolve_timestamp_seconds",
            "When the list last resolved",
            last_resolve.clone(),
        );
        registry.register("resolve_errors", "Resolves that failed", errors.clone());
        Self {
            registry,
            prefixes,
            last_resolve,
            errors,
        }
    }
}

impl Metrics {
    fn resolved(&self, egress: &str, counts: (u32, u32), at: SystemTime) {
        for (family, count) in [("ipv4", counts.0), ("ipv6", counts.1)] {
            let labels = FamilyLabels {
                egress: egress.to_owned(),
                family: family.to_owned(),
            };
            self.prefixes.get_or_create(&labels).set(count.into());
        }
        let seconds = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let labels = ListLabels {
            egress: egress.to_owned(),
        };
        self.last_resolve
            .get_or_create(&labels)
            .set(i64::try_from(seconds).unwrap_or(i64::MAX));
    }

    fn failed(&self, egress: &str) {
        let labels = ListLabels {
            egress: egress.to_owned(),
        };
        self.errors.get_or_create(&labels).inc();
    }

    fn forget(&self, egress: &str) {
        for family in ["ipv4", "ipv6"] {
            self.prefixes.remove(&FamilyLabels {
                egress: egress.to_owned(),
                family: family.to_owned(),
            });
        }
        let labels = ListLabels {
            egress: egress.to_owned(),
        };
        self.last_resolve.remove(&labels);
        self.errors.remove(&labels);
    }
}

/// What a change to one `Egress` asks of this node.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Resolve it anew: it is new to this node, or its spec changed.
    Start,
    /// Its spec is the one being resolved.
    Keep,
    /// It no longer names this node: stop, and drop this node's status entry.
    Leave,
    /// It is none of this node's.
    Ignore,
}

fn action(running: Option<i64>, generation: i64, names_node: bool, has_entry: bool) -> Action {
    match (names_node, running) {
        (true, Some(running)) if running == generation => Action::Keep,
        (true, _) => Action::Start,
        (false, _) if running.is_some() || has_entry => Action::Leave,
        (false, _) => Action::Ignore,
    }
}

/// Watches every `Egress` until `shutdown`, resolving those that name this node.
pub async fn run(
    agent: Agent,
    shutdown: Shutdown,
    timeouts: &ApiTimeouts,
    metrics: Arc<Metrics>,
) -> Result<()> {
    let api: Api<Egress> = Api::all(agent.client);
    let resolver = Arc::new(Resolver::new(resolve::RIPESTAT)?);
    let node = agent.node;
    let mut lists: HashMap<String, (i64, JoinHandle<()>)> = HashMap::new();
    let mut listed = HashSet::new();
    let config = watcher::Config::default().timeout(timeouts.watch_secs);
    let mut events = std::pin::pin!(kube::runtime::WatchStreamExt::default_backoff(watcher(
        api.clone(),
        config
    )));
    loop {
        let event = tokio::select! {
            event = events.next() => event,
            () = shutdown.requested() => break,
        };
        let Some(event) = event else { break };
        match event {
            Ok(watcher::Event::Init) => listed.clear(),
            Ok(watcher::Event::InitApply(egress)) => {
                listed.insert(egress.metadata.name.clone().unwrap_or_default());
                changed(&api, &resolver, &metrics, &node, &mut lists, egress).await;
            }
            Ok(watcher::Event::Apply(egress)) => {
                changed(&api, &resolver, &metrics, &node, &mut lists, egress).await;
            }
            Ok(watcher::Event::InitDone) => {
                for name in gone(lists.keys(), &listed) {
                    stop(&mut lists, &metrics, &name);
                }
            }
            Ok(watcher::Event::Delete(egress)) => {
                stop(
                    &mut lists,
                    &metrics,
                    &egress.metadata.name.unwrap_or_default(),
                );
            }
            Err(e) => tracing::warn!(error = %e, "watch of the Egress resources failed, retrying"),
        }
    }
    for (_, (_, task)) in lists {
        task.abort();
    }
    Ok(())
}

async fn changed(
    api: &Api<Egress>,
    resolver: &Arc<Resolver>,
    metrics: &Arc<Metrics>,
    node: &str,
    lists: &mut HashMap<String, (i64, JoinHandle<()>)>,
    egress: Egress,
) {
    let name = egress.metadata.name.clone().unwrap_or_default();
    let generation = egress.metadata.generation.unwrap_or_default();
    let (names_node, has_entry) = on_node(&egress, node);
    match action(
        lists.get(&name).map(|(running, _)| *running),
        generation,
        names_node,
        has_entry,
    ) {
        Action::Keep | Action::Ignore => {}
        Action::Start => {
            stop(lists, metrics, &name);
            tracing::info!(egress = name, generation, "resolving");
            let task = tokio::spawn(keep_resolved(
                api.clone(),
                Arc::clone(resolver),
                Arc::clone(metrics),
                node.to_owned(),
                name.clone(),
                generation,
                egress.spec,
            ));
            lists.insert(name, (generation, task));
        }
        Action::Leave => {
            stop(lists, metrics, &name);
            tracing::info!(egress = name, "no longer names this node");
            let unowned = json!({"apiVersion": "slipmesh.net/v1alpha1", "kind": "Egress"});
            if let Err(e) = api
                .patch_status(&name, &field_manager(node), &Patch::Apply(&unowned))
                .await
            {
                tracing::warn!(egress = name, error = %e, "failed to drop this node's status");
            }
        }
    }
}

/// Whether `egress` names `node`, and whether its status has an entry of `node`'s.
fn on_node(egress: &Egress, node: &str) -> (bool, bool) {
    let names_node = egress.spec.nodes.iter().any(|named| named == node);
    let has_entry = egress
        .status
        .as_ref()
        .is_some_and(|status| status.nodes.iter().any(|entry| entry.node == node));
    (names_node, has_entry)
}

/// The lists `running` that a relist, `listed`, no longer holds.
fn gone<'a>(running: impl Iterator<Item = &'a String>, listed: &HashSet<String>) -> Vec<String> {
    running
        .filter(|name| !listed.contains(*name))
        .cloned()
        .collect()
}

fn stop(lists: &mut HashMap<String, (i64, JoinHandle<()>)>, metrics: &Metrics, name: &str) {
    if let Some((_, task)) = lists.remove(name) {
        task.abort();
        metrics.forget(name);
    }
}

fn field_manager(node: &str) -> PatchParams {
    PatchParams::apply(&format!("egress/{node}")).force()
}

/// Resolves `spec` for as long as it runs: each answer again once its lifetime is over, the list
/// composed and its status applied after each round.
async fn keep_resolved(
    api: Api<Egress>,
    resolver: Arc<Resolver>,
    metrics: Arc<Metrics>,
    node: String,
    name: String,
    generation: i64,
    spec: EgressSpec,
) {
    let asked = resolve::asked(&spec);
    let mut answers: Vec<Option<Answer>> = vec![None; asked.len()];
    let mut last = Last::default();
    loop {
        let now = SystemTime::now();
        let outcome = round(&resolver, &asked, &mut answers, now).await;
        let wait = match &outcome {
            Ok(networks) => {
                let counts = counts(networks);
                metrics.resolved(&name, counts, now);
                last.resolved(now, counts);
                next_wake(&answers, SystemTime::now())
            }
            Err(e) => {
                tracing::warn!(egress = name, error = format!("{e:#}"), "resolve failed");
                metrics.failed(&name);
                last.failed(now, format!("{e:#}"));
                AFTER_FAILURE
            }
        };
        let status = status(&node, generation, &last);
        if let Err(e) = api
            .patch_status(&name, &field_manager(&node), &Patch::Apply(&status))
            .await
        {
            tracing::warn!(egress = name, error = %e, "failed to apply the status");
        }
        tokio::time::sleep(wait).await;
    }
}

/// Answers every query `due` at `now`, then composes the list.
async fn round(
    resolver: &Resolver,
    asked: &[Asked],
    answers: &mut [Option<Answer>],
    now: SystemTime,
) -> Result<Vec<IpNet>> {
    let due = due(answers, now);
    let fresh = futures::future::try_join_all(
        due.iter()
            .map(|&i| resolver.answer(&asked[i].query, asked[i].lifetime)),
    )
    .await?;
    for (i, answer) in due.into_iter().zip(fresh) {
        answers[i] = Some(answer);
    }
    let answered: Vec<&Answer> = answers.iter().flatten().collect();
    resolve::compose(asked, &answered, &[])
}

/// The answers never given or past their lifetime at `now`.
fn due(answers: &[Option<Answer>], now: SystemTime) -> Vec<usize> {
    answers
        .iter()
        .enumerate()
        .filter(|(_, answer)| answer.as_ref().is_none_or(|answer| expiry(answer) <= now))
        .map(|(i, _)| i)
        .collect()
}

/// How long from `now` until the first answer's lifetime is over.
fn next_wake(answers: &[Option<Answer>], now: SystemTime) -> Duration {
    answers
        .iter()
        .flatten()
        .map(|answer| expiry(answer).duration_since(now).unwrap_or_default())
        .min()
        .unwrap_or_default()
}

fn expiry(answer: &Answer) -> SystemTime {
    answer.resolved_at + answer.ttl
}

fn counts(networks: &[IpNet]) -> (u32, u32) {
    let v4 = networks.iter().filter(|net| net.addr().is_ipv4()).count();
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    (count(v4), count(networks.len() - v4))
}

/// What the status carries between rounds: the last good resolve, and the condition with when it
/// last turned.
#[derive(Debug, Default, Clone, PartialEq)]
struct Last {
    resolved_at: Option<SystemTime>,
    counts: (u32, u32),
    ok: Option<bool>,
    since: Option<SystemTime>,
    message: String,
}

impl Last {
    fn resolved(&mut self, at: SystemTime, counts: (u32, u32)) {
        self.turn(true, at);
        self.resolved_at = Some(at);
        self.counts = counts;
        self.message = String::new();
    }

    fn failed(&mut self, at: SystemTime, message: String) {
        self.turn(false, at);
        self.message = message;
    }

    fn turn(&mut self, ok: bool, at: SystemTime) {
        if self.ok != Some(ok) {
            self.ok = Some(ok);
            self.since = Some(at);
        }
    }
}

/// This node's entry of the status, as it is applied.
fn status(node: &str, generation: i64, last: &Last) -> Value {
    let time =
        |at: SystemTime| Time(k8s_openapi::jiff::Timestamp::try_from(at).unwrap_or_default());
    let ok = last.ok == Some(true);
    let mut entry = json!({
        "node": node,
        "observedGeneration": generation,
        "prefixes": {"ipv4": last.counts.0, "ipv6": last.counts.1},
        "conditions": [{
            "type": "Resolved",
            "status": if ok { "True" } else { "False" },
            "reason": if ok { "Resolved" } else { "ResolveFailed" },
            "message": last.message,
            "observedGeneration": generation,
            "lastTransitionTime": time(last.since.unwrap_or(UNIX_EPOCH)),
        }],
    });
    if let Some(at) = last.resolved_at {
        entry["resolvedAt"] = json!(time(at));
    }
    json!({
        "apiVersion": "slipmesh.net/v1alpha1",
        "kind": "Egress",
        "status": {"nodes": [entry]},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn answer(resolved: u64, ttl: u64) -> Option<Answer> {
        Some(Answer {
            networks: Vec::new(),
            resolved_at: at(resolved),
            ttl: Duration::from_secs(ttl),
        })
    }

    #[test]
    fn a_change_starts_a_list_anew_keeps_the_running_one_and_leaves_one_that_drops_this_node() {
        assert_eq!(action(None, 1, true, false), Action::Start);
        assert_eq!(action(Some(1), 2, true, true), Action::Start);
        assert_eq!(action(Some(2), 2, true, true), Action::Keep);
        assert_eq!(action(Some(2), 3, false, true), Action::Leave);
        assert_eq!(action(None, 3, false, true), Action::Leave);
        assert_eq!(action(None, 3, false, false), Action::Ignore);
    }

    #[test]
    fn an_answer_is_due_unanswered_or_once_its_lifetime_is_over_and_the_first_due_wakes() {
        let answers = [None, answer(100, 60), answer(100, 10), answer(100, 300)];
        assert_eq!(due(&answers, at(110)), [0, 2]);
        assert_eq!(due(&answers, at(109)), [0]);
        assert_eq!(next_wake(&answers[1..], at(110)), Duration::ZERO);
        assert_eq!(
            next_wake(&[answer(100, 60), answer(100, 300)], at(110)),
            Duration::from_secs(50)
        );
    }

    #[test]
    fn the_condition_turns_only_when_the_outcome_does_and_a_failure_keeps_the_last_counts() {
        let mut last = Last::default();
        last.resolved(at(10), (2, 1));
        last.resolved(at(20), (3, 1));
        last.failed(at(30), "gave up".into());
        last.failed(at(40), "gave up again".into());
        assert_eq!(
            status("node-a", 4, &last)["status"]["nodes"][0],
            json!({
                "node": "node-a",
                "observedGeneration": 4,
                "resolvedAt": "1970-01-01T00:00:20Z",
                "prefixes": {"ipv4": 3, "ipv6": 1},
                "conditions": [{
                    "type": "Resolved",
                    "status": "False",
                    "reason": "ResolveFailed",
                    "message": "gave up again",
                    "observedGeneration": 4,
                    "lastTransitionTime": "1970-01-01T00:00:30Z",
                }],
            })
        );
    }

    #[test]
    fn a_list_is_counted_by_family() {
        let networks: Vec<IpNet> = ["192.0.2.0/24", "2001:db8::/32", "3fff::/20"]
            .iter()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(counts(&networks), (1, 2));
    }

    #[test]
    fn a_node_reads_whether_a_list_names_it_and_whether_its_entry_is_there() {
        let egress: Egress = sliplib_yaml::from_str(
            "{apiVersion: slipmesh.net/v1alpha1, kind: Egress, metadata: {name: a},
              spec: {nodes: [node-a], include: [{country: XX}]},
              status: {nodes: [{node: node-b, observedGeneration: 1, prefixes: {ipv4: 0, ipv6: 0}}]}}",
        )
        .unwrap();
        assert_eq!(on_node(&egress, "node-a"), (true, false));
        assert_eq!(on_node(&egress, "node-b"), (false, true));
        let listed = HashSet::from(["a".to_owned()]);
        let running = ["a".to_owned(), "b".to_owned()];
        assert_eq!(gone(running.iter(), &listed), ["b"]);
    }

    #[test]
    fn each_node_applies_as_its_own_manager_and_its_series_go_with_its_list() {
        let params = field_manager("node-a");
        assert_eq!(params.field_manager.as_deref(), Some("egress/node-a"));
        assert!(params.force);

        let metrics = Metrics::default();
        metrics.resolved("a", (2, 1), at(60));
        metrics.failed("a");
        let encoded = || {
            let mut out = String::new();
            prometheus_client::encoding::text::encode(&mut out, &metrics.registry).unwrap();
            out
        };
        let text = encoded();
        for series in [
            r#"slipmesh_egress_prefixes{egress="a",family="ipv4"} 2"#,
            r#"slipmesh_egress_prefixes{egress="a",family="ipv6"} 1"#,
            r#"slipmesh_egress_last_resolve_timestamp_seconds{egress="a"} 60"#,
            r#"slipmesh_egress_resolve_errors_total{egress="a"} 1"#,
        ] {
            assert!(text.contains(series), "{series} in {text}");
        }
        metrics.forget("a");
        assert!(!encoded().contains(r#"egress="a""#), "{}", encoded());
    }
}
