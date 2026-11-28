//! On one node, every `Egress` that names it resolved on its own schedule, with its status and
//! metrics kept, and its routes installed if it says so.

use crate::crd::{Egress, FamilyCounts, SourceStatus};
use crate::install::{self, Installer};
use crate::resolve::{self, Answer, Asked, Endpoints, Lifetime, Resolver};
use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::core::v1::Node;
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
use sliplib_config::mesh_object::{self, Mesh};
use sliplib_kube::{Agent, ApiTimeouts};
use sliplib_net::IpNet;
use sliplib_runtime::Shutdown;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
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

/// One object's spec: its uid, so a list deleted and made again under the same name is another,
/// and its generation.
type Version = (String, i64);

/// The lists being resolved, by name.
type Lists = HashMap<String, (Version, JoinHandle<()>)>;

/// What every list's task shares.
#[derive(Clone)]
struct Shared {
    api: Api<Egress>,
    resolver: Arc<Resolver>,
    metrics: Arc<Metrics>,
    installer: Arc<Installer>,
    node: String,
    mesh: watch::Receiver<Endpoints>,
}

fn action(
    running: Option<&Version>,
    version: &Version,
    names_node: bool,
    has_entry: bool,
) -> Action {
    match (names_node, running) {
        (true, Some(running)) if running == version => Action::Keep,
        (true, _) => Action::Start,
        (false, _) if running.is_some() || has_entry => Action::Leave,
        (false, _) => Action::Ignore,
    }
}

/// Watches every `Egress`, this node and the `Mesh` until `shutdown`, resolving the lists that name
/// it and installing those that install.
pub async fn run(
    agent: Agent,
    shutdown: Shutdown,
    timeouts: &ApiTimeouts,
    metrics: Arc<Metrics>,
) -> Result<()> {
    let api: Api<Egress> = Api::all(agent.client.clone());
    let installer = Arc::new(Installer::default());
    let (mesh, _) = watch::channel(Endpoints::Unread);
    let shared = Shared {
        api: api.clone(),
        resolver: Arc::new(Resolver::new(resolve::RIPESTAT)?),
        metrics,
        installer: Arc::clone(&installer),
        node: agent.node.clone(),
        mesh: mesh.subscribe(),
    };
    let rt = sliplib_net::route::RtClient::connect().context("failed to open rtnetlink")?;
    let changes = sliplib_net::route::link_route_and_address_changes()
        .context("failed to follow the kernel's routes")?;
    let installing = tokio::spawn({
        let installer = Arc::clone(&installer);
        async move { install::keep_installed(rt, changes, &installer).await }
    });
    let mut node_events = std::pin::pin!(sliplib_kube::watch_one::<Node>(
        Api::all(agent.client.clone()),
        &agent.node,
        timeouts
    ));
    let mut mesh_events = std::pin::pin!(sliplib_kube::watch_one::<Mesh>(
        Api::all(agent.client),
        mesh_object::NAME,
        timeouts
    ));
    let mut relisted = Endpoints::Absent;
    let mut lists = Lists::new();
    let mut listed = HashSet::new();
    let config = watcher::Config::default().timeout(timeouts.watch_secs);
    let mut events = std::pin::pin!(kube::runtime::WatchStreamExt::default_backoff(watcher(
        api.clone(),
        config
    )));
    loop {
        let event = tokio::select! {
            event = events.next() => event,
            Some(event) = node_events.next() => {
                tagged(&installer, event);
                continue;
            }
            Some(event) = mesh_events.next() => {
                read_mesh(&mesh, &mut relisted, event);
                continue;
            }
            () = shutdown.requested() => break,
        };
        let Some(event) = event else { break };
        match event {
            Ok(watcher::Event::Init) => listed.clear(),
            Ok(watcher::Event::InitApply(egress)) => {
                listed.insert(egress.metadata.name.clone().unwrap_or_default());
                changed(&shared, &mut lists, egress).await;
            }
            Ok(watcher::Event::Apply(egress)) => {
                changed(&shared, &mut lists, egress).await;
            }
            Ok(watcher::Event::InitDone) => {
                for name in gone(lists.keys(), &listed) {
                    stop(&mut lists, &shared.metrics, &name).await;
                    installer.forget(&name);
                }
                installer.listed();
            }
            Ok(watcher::Event::Delete(egress)) => {
                let name = egress.metadata.name.unwrap_or_default();
                stop(&mut lists, &shared.metrics, &name).await;
                installer.forget(&name);
            }
            Err(e) => tracing::warn!(error = %e, "watch of the Egress resources failed, retrying"),
        }
    }
    for (_, (_, task)) in lists {
        task.abort();
    }
    installing.abort();
    Ok(())
}

/// The tag of this node's routes, from its annotations as they change.
fn tagged(installer: &Installer, event: watcher::Result<watcher::Event<Node>>) {
    let node = match event {
        Ok(watcher::Event::Apply(node) | watcher::Event::InitApply(node)) => node,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(error = %e, "watch of this node failed, retrying");
            return;
        }
    };
    let tag = install::tag(&node.metadata.annotations.unwrap_or_default()).unwrap_or_else(|e| {
        tracing::warn!(error = format!("{e:#}"), "no routes are installed");
        None
    });
    installer.set_tag(tag);
}

/// The endpoints of the `Mesh` as its watch reads it, handed on only when they change. A relist
/// reads it in full: one deleted while the watch was down shows only as a relist without it.
fn read_mesh(
    sender: &watch::Sender<Endpoints>,
    relisted: &mut Endpoints,
    event: watcher::Result<watcher::Event<Mesh>>,
) {
    let read = match event {
        Ok(watcher::Event::Init) => {
            *relisted = Endpoints::Absent;
            return;
        }
        Ok(watcher::Event::InitApply(mesh)) => {
            *relisted = Endpoints::of(&mesh.spec);
            return;
        }
        Ok(watcher::Event::InitDone) => std::mem::replace(relisted, Endpoints::Absent),
        Ok(watcher::Event::Apply(mesh)) => Endpoints::of(&mesh.spec),
        Ok(watcher::Event::Delete(_)) => Endpoints::Absent,
        Err(e) => {
            tracing::warn!(error = %e, "watch of the Mesh failed, retrying");
            return;
        }
    };
    sender.send_if_modified(|current| {
        let changed = *current != read;
        *current = read;
        changed
    });
}

async fn changed(shared: &Shared, lists: &mut Lists, egress: Egress) {
    let Shared {
        api,
        metrics,
        installer,
        node,
        ..
    } = shared;
    let name = egress.metadata.name.clone().unwrap_or_default();
    let generation = egress.metadata.generation.unwrap_or_default();
    let version = (egress.metadata.uid.clone().unwrap_or_default(), generation);
    let (names_node, has_entry) = on_node(&egress, node);
    match action(
        lists.get(&name).map(|(running, _)| running),
        &version,
        names_node,
        has_entry,
    ) {
        Action::Keep | Action::Ignore => {}
        Action::Start => {
            stop(lists, metrics, &name).await;
            tracing::info!(
                egress = name,
                generation,
                install = egress.spec.install,
                "resolving"
            );
            // In one step: the installer must not see the list gone in between and delete its
            // routes.
            if egress.spec.install {
                installer.pending(&name);
            } else {
                installer.forget(&name);
            }
            let task = tokio::spawn(keep_resolved(shared.clone(), egress));
            lists.insert(name, (version, task));
        }
        Action::Leave => {
            stop(lists, metrics, &name).await;
            installer.forget(&name);
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

/// Ends the list's task, then drops its series: an aborted task stops only at its next await, and
/// could write them again in between. The installer still holds the list's networks.
async fn stop(lists: &mut Lists, metrics: &Metrics, name: &str) {
    if let Some((_, task)) = lists.remove(name) {
        task.abort();
        let _ = task.await;
        metrics.forget(name);
    }
}

fn field_manager(node: &str) -> PatchParams {
    PatchParams::apply(&format!("egress/{node}")).force()
}

/// Resolves `egress` for as long as it runs: each answer again once its lifetime is over, the
/// list composed less the mesh's endpoints, handed to the installer if it installs, and its status
/// applied after each round. A change of the endpoints starts a round at once.
async fn keep_resolved(shared: Shared, egress: Egress) {
    let Shared {
        api,
        resolver,
        metrics,
        installer,
        node,
        mut mesh,
    } = shared;
    // Before the first read every list would fail, after a restart too.
    if mesh
        .wait_for(|read| *read != Endpoints::Unread)
        .await
        .is_err()
    {
        return;
    }
    let name = egress.metadata.name.unwrap_or_default();
    let generation = egress.metadata.generation.unwrap_or_default();
    let spec = egress.spec;
    let asked = resolve::asked(&spec);
    let mut answers: Vec<Option<Answer>> = vec![None; asked.len()];
    let mut errors: Vec<Option<String>> = vec![None; asked.len()];
    let mut last = Last::default();
    loop {
        let now = SystemTime::now();
        let endpoints = mesh.borrow_and_update().clone();
        let outcome = round(
            &resolver,
            &asked,
            &mut answers,
            &mut errors,
            &endpoints,
            now,
        )
        .await;
        let mut wait = match &outcome {
            Ok(networks) => {
                let counts = counts(networks);
                metrics.resolved(&name, counts, now);
                last.resolved(now, counts);
                if spec.install {
                    installer.resolved(&name, networks.clone());
                }
                next_wake(&answers, SystemTime::now())
            }
            Err(e) => {
                tracing::warn!(egress = name, error = format!("{e:#}"), "resolve failed");
                metrics.failed(&name);
                last.failed(now, format!("{e:#}"));
                AFTER_FAILURE
            }
        };
        let status = status(
            &node,
            generation,
            &last,
            &sources(&asked, &answers, &errors),
        );
        if let Err(e) = api
            .patch_status(&name, &field_manager(&node), &Patch::Apply(&status))
            .await
        {
            tracing::warn!(egress = name, error = %e, "failed to apply the status");
            wait = wait.min(AFTER_FAILURE);
        }
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            Ok(()) = mesh.changed() => {}
        }
    }
}

/// Answers every query `due` at `now` and every endpoint, then composes the list less the
/// endpoints. Every query runs to the end, so one that fails keeps the others' answers.
async fn round(
    resolver: &Resolver,
    asked: &[Asked],
    answers: &mut [Option<Answer>],
    errors: &mut [Option<String>],
    endpoints: &Endpoints,
    now: SystemTime,
) -> Result<Vec<IpNet>> {
    let endpoints = endpoints.queries()?;
    let due = due(answers, now);
    // Both run to the end, so a failed endpoint keeps the fresh answers.
    let (fresh, cuts) = futures::join!(
        futures::future::join_all(
            due.iter()
                .map(|&i| resolver.answer(&asked[i].query, asked[i].lifetime)),
        ),
        // Asked every round, so no lifetime applies.
        futures::future::try_join_all(endpoints.iter().map(|query| async move {
            resolver
                .answer(query, Lifetime::Source(Duration::ZERO))
                .await
                .with_context(|| format!("endpoint {query:?}"))
        })),
    );
    store(answers, errors, &due, fresh)?;
    let answered: Vec<&Answer> = answers.iter().flatten().collect();
    let excluded: Vec<IpNet> = cuts?.into_iter().flat_map(|cut| cut.networks).collect();
    resolve::compose(asked, &answered, &excluded)
}

/// Each of `due`'s `results` kept in its slot: a good answer replaces the last and clears its
/// error, a failure keeps the last good answer beside it. Gives the first failure.
fn store(
    answers: &mut [Option<Answer>],
    errors: &mut [Option<String>],
    due: &[usize],
    results: Vec<Result<Answer>>,
) -> Result<()> {
    let mut first = None;
    for (&i, result) in due.iter().zip(results) {
        match result {
            Ok(answer) => {
                answers[i] = Some(answer);
                errors[i] = None;
            }
            Err(e) => {
                errors[i] = Some(format!("{e:#}"));
                first.get_or_insert(e);
            }
        }
    }
    first.map_or(Ok(()), Err)
}

/// Each source of `asked`, in spec order, from its queries' `answers` and `errors`.
fn sources(
    asked: &[Asked],
    answers: &[Option<Answer>],
    errors: &[Option<String>],
) -> Vec<SourceStatus> {
    let mut start = 0;
    asked
        .chunk_by(|a, b| (a.place, a.index) == (b.place, b.index))
        .map(|source| {
            let range = start..start + source.len();
            start = range.end;
            let answers = &answers[range.clone()];
            let oldest = answers
                .iter()
                .map(Option::as_ref)
                .collect::<Option<Vec<_>>>()
                .and_then(|given| {
                    given
                        .into_iter()
                        .min_by_key(|answer| (answer.resolved_at, answer.ttl))
                });
            let networks: Vec<IpNet> = answers
                .iter()
                .flatten()
                .flat_map(|answer| answer.networks.iter().copied())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let (ipv4, ipv6) = counts(&networks);
            SourceStatus {
                place: source[0].place,
                index: source[0].index,
                label: source[0].label.clone(),
                resolved_at: oldest.map(|answer| time(answer.resolved_at)),
                ttl: oldest.map(|answer| u32::try_from(answer.ttl.as_secs()).unwrap_or(u32::MAX)),
                prefixes: FamilyCounts { ipv4, ipv6 },
                error: errors[range].iter().flatten().next().cloned(),
            }
        })
        .collect()
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

fn time(at: SystemTime) -> Time {
    Time(k8s_openapi::jiff::Timestamp::try_from(at).unwrap_or_default())
}

/// This node's entry of the status, as it is applied.
fn status(node: &str, generation: i64, last: &Last, sources: &[SourceStatus]) -> Value {
    let ok = last.ok == Some(true);
    let mut entry = json!({
        "node": node,
        "observedGeneration": generation,
        "prefixes": {"ipv4": last.counts.0, "ipv6": last.counts.1},
        "sources": sources,
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
        let version = |uid: &str, generation| (uid.to_owned(), generation);
        let running = version("a", 2);
        assert_eq!(action(None, &version("a", 1), true, false), Action::Start);
        assert_eq!(
            action(Some(&version("a", 1)), &running, true, true),
            Action::Start
        );
        assert_eq!(
            action(Some(&running), &version("a", 2), true, true),
            Action::Keep
        );
        // Made again under the same name, at the same generation.
        assert_eq!(
            action(Some(&running), &version("b", 2), true, true),
            Action::Start
        );
        assert_eq!(
            action(Some(&running), &version("a", 3), false, true),
            Action::Leave
        );
        assert_eq!(action(None, &running, false, true), Action::Leave);
        assert_eq!(action(None, &running, false, false), Action::Ignore);
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
            status("node-a", 4, &last, &[])["status"]["nodes"][0],
            json!({
                "node": "node-a",
                "observedGeneration": 4,
                "resolvedAt": "1970-01-01T00:00:20Z",
                "prefixes": {"ipv4": 3, "ipv6": 1},
                "sources": [],
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

    fn answered(secs: u64, networks: &[&str]) -> Option<Answer> {
        held(secs, 86_400, networks)
    }

    fn held(secs: u64, ttl: u64, networks: &[&str]) -> Option<Answer> {
        Some(Answer {
            networks: networks.iter().map(|n| n.parse().unwrap()).collect(),
            resolved_at: at(secs),
            ttl: Duration::from_secs(ttl),
        })
    }

    #[test]
    fn a_failure_keeps_the_last_good_answer_beside_its_error_until_the_query_answers() {
        let mut answers = vec![answered(10, &["192.0.2.0/24"]), None, None];
        let mut errors = vec![None, Some("gave up before".to_owned()), None];
        let ended = store(
            &mut answers,
            &mut errors,
            &[0, 1, 2],
            vec![
                Err(anyhow::anyhow!("gave up")),
                Ok(answered(20, &["198.51.100.0/24"]).unwrap()),
                Err(anyhow::anyhow!("gave up too")),
            ],
        );
        assert_eq!(format!("{:#}", ended.unwrap_err()), "gave up");
        assert_eq!(
            answers,
            [
                answered(10, &["192.0.2.0/24"]),
                answered(20, &["198.51.100.0/24"]),
                None
            ]
        );
        assert_eq!(
            errors,
            [Some("gave up".into()), None, Some("gave up too".into())]
        );
        assert!(store(&mut answers, &mut errors, &[], Vec::new()).is_ok());
    }

    #[test]
    fn each_source_counts_its_distinct_networks_from_its_oldest_answer_with_its_first_error() {
        let spec = sliplib_yaml::from_str(
            "{nodes: [node-a], include: [
                {label: a, asns: [AS64496, AS64497]},
                {hostnames: [a.example.com, b.example.com]},
                {hostnames: [c.example.com, d.example.com]}
            ], exclude: [{prefixes: [192.0.2.0/25]}]}",
        )
        .unwrap();
        let asked = resolve::asked(&spec);
        let answers = [
            answered(100, &["192.0.2.0/24", "2001:db8::/32"]),
            answered(50, &["192.0.2.0/24", "198.51.100.0/24"]),
            held(100, 300, &["203.0.113.1/32"]),
            None,
            held(70, 600, &["203.0.113.3/32"]),
            held(90, 120, &["203.0.113.4/32"]),
            answered(100, &["192.0.2.0/25"]),
        ];
        let errors = [
            None,
            Some("AS64497 gave up".to_owned()),
            None,
            Some("b.example.com gave up".to_owned()),
            None,
            None,
            None,
        ];
        assert_eq!(
            serde_json::to_value(sources(&asked, &answers, &errors)).unwrap(),
            json!([
                {"place": "include", "index": 0, "label": "a",
                 "resolvedAt": "1970-01-01T00:00:50Z", "ttl": 86_400,
                 "prefixes": {"ipv4": 2, "ipv6": 1}, "error": "AS64497 gave up"},
                {"place": "include", "index": 1, "prefixes": {"ipv4": 1, "ipv6": 0},
                 "error": "b.example.com gave up"},
                {"place": "include", "index": 2, "resolvedAt": "1970-01-01T00:01:10Z", "ttl": 600,
                 "prefixes": {"ipv4": 2, "ipv6": 0}},
                {"place": "exclude", "index": 0, "resolvedAt": "1970-01-01T00:01:40Z",
                 "ttl": 86_400, "prefixes": {"ipv4": 1, "ipv6": 0}},
            ])
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
