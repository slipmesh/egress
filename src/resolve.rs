//! One `Egress` list resolved: each source asked as one query per ASN, country, host or literal
//! list, each answer with its own resolve time and lifetime, and the answers merged within their
//! label, less every exclusion.

use crate::crd::{EgressSpec, Source};
use anyhow::{Context, Result, bail, ensure};
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::{RData, RecordType};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sliplib_net::IpNet;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::{Duration, Instant, SystemTime};

/// What one query asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Query {
    Asn(String),
    Country(String),
    Host(String),
    Prefixes(Vec<String>),
}

/// The queries of `source`, in its order.
pub fn queries(source: &Source) -> Vec<Query> {
    let mut out = Vec::new();
    out.extend(source.asns.iter().flatten().cloned().map(Query::Asn));
    out.extend(source.country.iter().cloned().map(Query::Country));
    out.extend(source.hostnames.iter().flatten().cloned().map(Query::Host));
    out.extend(source.prefixes.clone().map(Query::Prefixes));
    out
}

/// One query's networks, and how long they hold from when they were resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    pub networks: Vec<IpNet>,
    pub resolved_at: SystemTime,
    pub ttl: Duration,
}

/// How long an answer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    /// The source's own: every answer holds for it, a host's too.
    Source(Duration),
    /// The list's: a RIPEstat or literal answer holds for it, a host's for its records' shortest
    /// TTL, at most this and at least `MIN_HOST_TTL`, which wins over a shorter list.
    List(Duration),
}

/// Every answer of a list, in the order its sources ask, and the networks they make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub answers: Vec<(Query, Answer)>,
    pub networks: Vec<IpNet>,
}

/// RIPEstat's documented limit is 8 concurrent requests per address; this stays under it.
const RIPESTAT_CONCURRENCY: usize = 5;
/// From some networks RIPEstat takes half a minute to answer and close to one to fail.
const RIPESTAT_TIMEOUT: Duration = Duration::from_secs(90);
/// Well above the largest country's list.
const RIPESTAT_BODY_LIMIT: usize = 64 << 20;
/// Its self-identification parameter: letters, digits, `-` and `_`.
const RIPESTAT_SOURCEAPP: &str = "slipmesh-egress";
/// One family of one host.
const DNS_TIMEOUT: Duration = Duration::from_secs(15);
/// A host's answer is asked again no sooner than this, however short its records' TTL.
pub const MIN_HOST_TTL: Duration = Duration::from_secs(60);

pub const RIPESTAT: &str = "https://stat.ripe.net";

pub struct Resolver {
    ripestat: String,
    http: reqwest::Client,
    ripestat_permits: tokio::sync::Semaphore,
    dns: hickory_resolver::TokioResolver,
}

impl Resolver {
    /// RIPEstat at `ripestat`, names through the system's resolver configuration.
    pub fn new(ripestat: &str) -> Result<Self> {
        let (config, options) = hickory_resolver::system_conf::read_system_conf()
            .context("failed to read the system's resolver configuration")?;
        Self::with_dns(ripestat, config, options)
    }

    /// RIPEstat at `ripestat`, names through `config`.
    pub fn with_dns(ripestat: &str, config: ResolverConfig, options: ResolverOpts) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(RIPESTAT_TIMEOUT)
            .user_agent(RIPESTAT_SOURCEAPP)
            .build()
            .context("failed to build the RIPEstat client")?;
        let dns = hickory_resolver::Resolver::builder_with_config(
            config,
            TokioRuntimeProvider::default(),
        )
        .with_options(options)
        .build()
        .context("failed to build the DNS resolver")?;
        Ok(Self {
            ripestat: ripestat.trim_end_matches('/').to_owned(),
            http,
            ripestat_permits: tokio::sync::Semaphore::new(RIPESTAT_CONCURRENCY),
            dns,
        })
    }

    /// Every query of `spec` answered and merged, less `excluded`. One failed query fails the
    /// list, and so does one that outlasts the budget its query count gives.
    pub async fn resolve(&self, spec: &EgressSpec, excluded: &[IpNet]) -> Result<Resolved> {
        let list_ttl = Duration::from_secs(spec.ttl.into());
        let asked: Vec<(bool, &Option<String>, Query, Lifetime)> =
            [(true, &spec.include), (false, &spec.exclude)]
                .into_iter()
                .flat_map(|(include, sources)| {
                    sources.iter().flat_map(move |source| {
                        let lifetime = source.ttl.map_or(Lifetime::List(list_ttl), |ttl| {
                            Lifetime::Source(Duration::from_secs(ttl.into()))
                        });
                        queries(source)
                            .into_iter()
                            .map(move |query| (include, &source.label, query, lifetime))
                    })
                })
                .collect();
        let queries: Vec<(Query, Lifetime)> = asked
            .iter()
            .map(|(.., query, lifetime)| (query.clone(), *lifetime))
            .collect();
        let answers = self
            .answer_all(&queries, budget(queries.iter().map(|(query, _)| query)))
            .await?;
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        for ((included, label, ..), answer) in asked.iter().zip(&answers) {
            if *included {
                include.push(((*label).clone(), answer.networks.clone()));
            } else {
                exclude.extend(&answer.networks);
            }
        }
        Ok(Resolved {
            networks: list(&include, &exclude, excluded)?,
            answers: queries
                .into_iter()
                .map(|(query, _)| query)
                .zip(answers)
                .collect(),
        })
    }

    /// Each query answered, in their order; the first error, or `budget` running out, ends them
    /// all.
    pub async fn answer_all(
        &self,
        queries: &[(Query, Lifetime)],
        budget: Duration,
    ) -> Result<Vec<Answer>> {
        let all = futures::future::try_join_all(
            queries
                .iter()
                .map(|(query, lifetime)| self.answer(query, *lifetime)),
        );
        tokio::time::timeout(budget, all)
            .await
            .with_context(|| format!("the list took longer than {budget:?} to resolve"))?
    }

    /// `query` resolved now, holding for `lifetime`.
    pub async fn answer(&self, query: &Query, lifetime: Lifetime) -> Result<Answer> {
        let resolved_at = SystemTime::now();
        let (networks, records_ttl) = match query {
            Query::Prefixes(prefixes) => (literal(prefixes)?, None),
            Query::Asn(asn) => {
                validate_asn(asn)?;
                let networks = self
                    .ripestat(&format!("announced-prefixes/data.json?resource={asn}"))
                    .await
                    .and_then(|response| announced(&response))
                    .with_context(|| format!("the prefixes {asn} announces"))?;
                if networks.is_empty() {
                    tracing::warn!(asn, "announces no prefix");
                }
                (networks, None)
            }
            Query::Country(country) => {
                let country = validate_country(country)?;
                let networks = self
                    .ripestat(&format!(
                        "country-resource-list/data.json?resource={country}&v4_format=prefix"
                    ))
                    .await
                    .and_then(|response| country_resources(&response))
                    .with_context(|| format!("the networks of country {country}"))?;
                (networks, None)
            }
            Query::Host(host) => match host.parse::<IpAddr>() {
                Ok(address) => (vec![IpNet::from(address)], None),
                Err(_) => {
                    let (networks, valid_until) = self.host(host).await?;
                    (
                        networks,
                        Some(valid_until.saturating_duration_since(Instant::now())),
                    )
                }
            },
        };
        Ok(Answer {
            networks,
            resolved_at,
            ttl: hold(lifetime, records_ttl),
        })
    }

    /// Both families of `host`, each asked on its own, and until when the shorter holds. A family
    /// without records is empty; any other failure of either fails the host.
    async fn host(&self, host: &str) -> Result<(Vec<IpNet>, Instant)> {
        let (v4, v6) = tokio::join!(
            self.family(host, RecordType::A),
            self.family(host, RecordType::AAAA)
        );
        let found: Vec<_> = [v4?, v6?].into_iter().flatten().collect();
        let valid_until = found
            .iter()
            .map(|(_, valid_until)| *valid_until)
            .min()
            .with_context(|| format!("{host} has neither A nor AAAA records"))?;
        Ok((
            found
                .into_iter()
                .flat_map(|(networks, _)| networks)
                .collect(),
            valid_until,
        ))
    }

    async fn family(&self, host: &str, kind: RecordType) -> Result<Option<(Vec<IpNet>, Instant)>> {
        let lookup = match tokio::time::timeout(DNS_TIMEOUT, self.dns.lookup(host, kind)).await {
            Err(_) => bail!("the {kind} lookup of {host} took longer than {DNS_TIMEOUT:?}"),
            Ok(Err(e)) if e.is_no_records_found() => return Ok(None),
            Ok(found) => found.with_context(|| format!("failed to look up {kind} of {host}"))?,
        };
        let networks: Vec<IpNet> = lookup
            .answers()
            .iter()
            .filter_map(|record| match &record.data {
                RData::A(a) => Some(IpNet::from(IpAddr::V4(a.0))),
                RData::AAAA(aaaa) => Some(IpNet::from(IpAddr::V6(aaaa.0))),
                _ => None,
            })
            .collect();
        Ok((!networks.is_empty()).then(|| (networks, lookup.valid_until())))
    }

    async fn ripestat(&self, path: &str) -> Result<Value> {
        let _permit = self
            .ripestat_permits
            .acquire()
            .await
            .context("the RIPEstat request limit is closed")?;
        let url = format!(
            "{}/data/{path}&sourceapp={RIPESTAT_SOURCEAPP}",
            self.ripestat
        );
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("RIPEstat request to {url} failed"))?
            .error_for_status()
            .context("RIPEstat answered with an error")?;
        let body = capped(response, RIPESTAT_BODY_LIMIT).await?;
        let answer: Value =
            serde_json::from_slice(&body).context("RIPEstat's answer is not JSON")?;
        let status = answer.get("status").and_then(Value::as_str);
        ensure!(
            status == Some("ok"),
            "RIPEstat answered status {status:?}: {}",
            answer.get("message").and_then(Value::as_str).unwrap_or("")
        );
        Ok(answer)
    }
}

/// The lifetime of an answer whose records, if any, hold for `records`.
fn hold(lifetime: Lifetime, records: Option<Duration>) -> Duration {
    match (lifetime, records) {
        (Lifetime::Source(ttl), _) | (Lifetime::List(ttl), None) => ttl,
        (Lifetime::List(list), Some(records)) => records.min(list).max(MIN_HOST_TTL),
    }
}

/// What a list's queries may take together: one RIPEstat timeout per round of concurrent
/// requests, and one DNS timeout, since hosts are asked alongside.
fn budget<'a>(queries: impl IntoIterator<Item = &'a Query>) -> Duration {
    let ripestat = queries
        .into_iter()
        .filter(|query| matches!(query, Query::Asn(_) | Query::Country(_)))
        .count();
    let rounds = u32::try_from(ripestat.div_ceil(RIPESTAT_CONCURRENCY)).unwrap_or(u32::MAX);
    RIPESTAT_TIMEOUT.saturating_mul(rounds) + DNS_TIMEOUT
}

/// `response`'s body, an error past `limit` bytes.
async fn capped(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read RIPEstat's answer")?
    {
        ensure!(
            body.len() + chunk.len() <= limit,
            "RIPEstat's answer is longer than {limit} bytes"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// An ASN goes into a request URL: `AS` and digits alone, so it cannot add a parameter.
fn validate_asn(asn: &str) -> Result<()> {
    let digits = asn
        .strip_prefix("AS")
        .with_context(|| format!("ASN {asn:?} does not start with AS"))?;
    ensure!(
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        "ASN {asn:?} is not AS and digits"
    );
    Ok(())
}

/// ISO 3166-1 alpha-2, which goes into a request URL; in upper case, as RIPEstat takes it.
fn validate_country(country: &str) -> Result<String> {
    ensure!(
        country.len() == 2 && country.bytes().all(|b| b.is_ascii_alphabetic()),
        "country {country:?} is not two letters"
    );
    Ok(country.to_ascii_uppercase())
}

fn literal(prefixes: &[String]) -> Result<Vec<IpNet>> {
    prefixes
        .iter()
        .map(|prefix| sliplib_net::parse(prefix).with_context(|| format!("prefix {prefix:?}")))
        .collect()
}

fn announced(response: &Value) -> Result<Vec<IpNet>> {
    let texts = entries(response, "/data/prefixes")?
        .iter()
        .map(|entry| {
            entry
                .get("prefix")
                .and_then(Value::as_str)
                .context("an announced prefix without `prefix`")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(networks(texts))
}

fn country_resources(response: &Value) -> Result<Vec<IpNet>> {
    let mut texts = Vec::new();
    for family in ["ipv4", "ipv6"] {
        for resource in entries(response, &format!("/data/resources/{family}"))? {
            texts.push(
                resource
                    .as_str()
                    .context("a country resource that is no text")?,
            );
        }
    }
    Ok(networks(texts))
}

fn entries<'a>(response: &'a Value, pointer: &str) -> Result<&'a [Value]> {
    response
        .pointer(pointer)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .with_context(|| format!("RIPEstat's answer has no list at {pointer}"))
}

/// RIPEstat writes every network as a prefix; anything else is left out, and said.
fn networks(texts: Vec<&str>) -> Vec<IpNet> {
    texts
        .into_iter()
        .filter_map(|text| match sliplib_net::parse(text) {
            Ok(net) => Some(net),
            Err(e) => {
                tracing::warn!(entry = text, error = %e, "left out of a RIPEstat answer");
                None
            }
        })
        .collect()
}

/// `include`'s networks, each label merged on its own, less every network of `exclude` and
/// `excluded`. A network listed under two labels keeps the first; an empty label is none. Nothing
/// left is an error: an empty list would mean a source that quietly gave nothing.
pub fn list(
    include: &[(Option<String>, Vec<IpNet>)],
    exclude: &[IpNet],
    excluded: &[IpNet],
) -> Result<Vec<IpNet>> {
    let mut first_label: BTreeMap<IpNet, Option<&str>> = BTreeMap::new();
    for (label, networks) in include {
        let label = label.as_deref().filter(|label| !label.is_empty());
        for net in networks {
            first_label.entry(net.trunc()).or_insert(label);
        }
    }
    let mut by_label: BTreeMap<Option<&str>, Vec<IpNet>> = BTreeMap::new();
    for (net, label) in first_label {
        by_label.entry(label).or_default().push(net);
    }
    let cuts: Vec<IpNet> = exclude.iter().chain(excluded).copied().collect();
    let mut out: Vec<IpNet> = by_label
        .into_values()
        .flat_map(|networks| sliplib_net::subtract(&networks, &cuts))
        .collect();
    out.sort();
    out.dedup();
    if out.is_empty() {
        bail!("the list resolved to no network");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::EgressSpec;
    use hickory_resolver::config::{ConnectionConfig, NameServerConfig};
    use hickory_resolver::proto::op::{Message, OpCode, ResponseCode};
    use hickory_resolver::proto::rr::rdata::{A, AAAA, CNAME};
    use hickory_resolver::proto::rr::{Name, RData, Record, RecordType};
    use serde_json::json;
    use std::net::IpAddr;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UdpSocket};

    const DAY: Duration = Duration::from_secs(86_400);

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|n| n.parse().unwrap()).collect()
    }

    fn label(name: &str) -> Option<String> {
        Some(name.to_owned())
    }

    fn secs(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    #[test]
    fn each_label_merges_on_its_own_and_every_exclusion_cuts_both_families() {
        assert_eq!(
            list(
                &[
                    (
                        label("a"),
                        nets(&["192.0.2.0/26", "192.0.2.64/26", "2001:db8::/33"])
                    ),
                    (label("b"), nets(&["192.0.2.128/26", "2001:db8:8000::/33"])),
                    // Listed under `b` first, so it stays there.
                    (label("a"), nets(&["2001:db8:8000::/33"])),
                    (None, nets(&["192.0.2.192/26", "3fff::/24"])),
                ],
                &nets(&["192.0.2.192/27"]),
                &nets(&["3fff::/25"]),
            )
            .unwrap(),
            nets(&[
                "192.0.2.0/25",
                "192.0.2.128/26",
                "192.0.2.224/27",
                "2001:db8::/33",
                "2001:db8:8000::/33",
                "3fff:80::/25",
            ])
        );
    }

    #[test]
    fn host_bits_are_cleared_and_a_network_two_labels_make_is_listed_once() {
        assert_eq!(
            list(
                &[
                    (label("a"), nets(&["192.0.2.1/25", "192.0.2.128/25"])),
                    (label("b"), nets(&["192.0.2.0/24"])),
                ],
                &[],
                &[],
            )
            .unwrap(),
            nets(&["192.0.2.0/24"])
        );
    }

    #[test]
    fn an_empty_label_is_no_label() {
        assert_eq!(
            list(
                &[
                    (None, nets(&["192.0.2.0/25"])),
                    (label(""), nets(&["192.0.2.128/25"])),
                ],
                &[],
                &[],
            )
            .unwrap(),
            nets(&["192.0.2.0/24"])
        );
    }

    #[test]
    fn a_list_with_nothing_left_is_refused() {
        assert!(
            list(
                &[(None, nets(&["192.0.2.0/24"]))],
                &nets(&["192.0.2.0/24"]),
                &[]
            )
            .is_err()
        );
        assert!(list(&[(None, Vec::new())], &[], &[]).is_err());
    }

    #[test]
    fn a_hosts_records_hold_at_least_the_floor_and_at_most_the_list_unless_its_source_says() {
        let list = Lifetime::List(secs(3600));
        assert_eq!(hold(list, Some(secs(300))), secs(300));
        assert_eq!(hold(list, Some(secs(7200))), secs(3600));
        assert_eq!(hold(list, Some(secs(5))), MIN_HOST_TTL);
        assert_eq!(hold(Lifetime::List(secs(10)), Some(secs(5))), MIN_HOST_TTL);
        assert_eq!(hold(Lifetime::List(secs(10)), None), secs(10));
        assert_eq!(hold(Lifetime::Source(secs(5)), Some(secs(300))), secs(5));
    }

    #[test]
    fn an_asn_is_as_and_digits_and_a_country_two_letters_held_in_upper() {
        for (asn, valid) in [
            ("AS64496", true),
            ("64496", false),
            ("AS", false),
            ("AS64496&x=1", false),
        ] {
            assert_eq!(validate_asn(asn).is_ok(), valid, "{asn}");
        }
        assert_eq!(validate_country("xx").unwrap(), "XX");
        for country in ["X", "XXX", "X1"] {
            assert!(validate_country(country).is_err(), "{country}");
        }
    }

    #[test]
    fn ripestat_answers_give_both_families_and_leave_out_what_is_no_network() {
        assert_eq!(
            announced(&json!({"data": {"prefixes": [
                {"prefix": "192.0.2.0/24"},
                {"prefix": "2001:db8::/32"},
                {"prefix": "not a network"},
            ]}}))
            .unwrap(),
            nets(&["192.0.2.0/24", "2001:db8::/32"])
        );
        assert_eq!(
            country_resources(&json!({"data": {"resources": {
                "ipv4": ["198.51.100.0/24", "198.51.100.0-198.51.100.9"],
                "ipv6": ["3fff::/20"],
            }}}))
            .unwrap(),
            nets(&["198.51.100.0/24", "3fff::/20"])
        );
    }

    #[test]
    fn a_ripestat_answer_without_its_lists_is_an_error() {
        for wrong in [
            json!({"data": {}}),
            json!({"data": {"prefixes": [{"origin": 64496}]}}),
        ] {
            assert!(announced(&wrong).is_err(), "{wrong}");
        }
        for wrong in [
            json!({"data": {"resources": {"ipv4": []}}}),
            json!({"data": {"resources": {"ipv6": []}}}),
            json!({"data": {"resources": {"ipv4": [4], "ipv6": []}}}),
        ] {
            assert!(country_resources(&wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn a_source_asks_one_query_per_asn_host_or_country_and_one_for_its_prefixes() {
        let source: Source = yaml_serde::from_str("{asns: [AS64496, AS64497]}").unwrap();
        assert_eq!(
            queries(&source),
            [Query::Asn("AS64496".into()), Query::Asn("AS64497".into())]
        );
        let source: Source = yaml_serde::from_str("{hostnames: [a.example, b.example]}").unwrap();
        assert_eq!(
            queries(&source),
            [
                Query::Host("a.example".into()),
                Query::Host("b.example".into())
            ]
        );
        let source: Source = yaml_serde::from_str("{country: XX}").unwrap();
        assert_eq!(queries(&source), [Query::Country("XX".into())]);
        let source: Source = yaml_serde::from_str("{prefixes: [192.0.2.0/24, 3fff::/20]}").unwrap();
        assert_eq!(
            queries(&source),
            [Query::Prefixes(vec![
                "192.0.2.0/24".into(),
                "3fff::/20".into()
            ])]
        );
    }

    #[test]
    fn the_budget_is_a_ripestat_timeout_per_round_of_requests_and_one_dns_timeout() {
        let asn = || Query::Asn("AS64496".into());
        let host = || Query::Host("a.example".into());
        assert_eq!(budget(&[host()]), DNS_TIMEOUT);
        assert_eq!(budget(&[asn(), host()]), RIPESTAT_TIMEOUT + DNS_TIMEOUT);
        let queries = vec![Query::Country("XX".into()); RIPESTAT_CONCURRENCY + 1];
        assert_eq!(budget(&queries), RIPESTAT_TIMEOUT * 2 + DNS_TIMEOUT);
    }

    /// The request lines and user agents a stand-in has seen.
    type Requests = Arc<Mutex<Vec<(String, String)>>>;

    /// A RIPEstat stand-in answering every request with `status` and `body`.
    async fn ripestat(status: u16, body: impl ToString) -> (String, Requests) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Requests::default();
        let requests = seen.clone();
        let body = body.to_string();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 4096];
                let read = conn.read(&mut buf).await.unwrap();
                let head = String::from_utf8_lossy(&buf[..read]).into_owned();
                let mut lines = head.lines();
                let line = lines.next().unwrap_or_default().to_owned();
                let agent = lines
                    .find_map(|l| l.strip_prefix("user-agent: "))
                    .unwrap_or_default()
                    .to_owned();
                requests.lock().unwrap().push((line, agent));
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(response.as_bytes()).await;
            }
        });
        (url, seen)
    }

    /// How a stand-in name server answers one record type.
    #[derive(Clone)]
    enum Records {
        Found(Vec<IpAddr>, u32),
        Code(ResponseCode),
        /// `ALIAS` is a CNAME to `TARGET`, which answers with `target`; `together` sends the
        /// target's records in the CNAME's answer too.
        Alias {
            ttl: u32,
            target: Box<Records>,
            together: bool,
        },
    }

    const ALIAS: &str = "a.example.";
    const TARGET: &str = "b.example.";

    /// A name server answering every name by its record type alone but `ALIAS`, and a resolver of
    /// no other.
    async fn name_server(a: Records, aaaa: Records) -> (ResolverConfig, ResolverOpts) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0; 512];
            loop {
                let (len, from) = socket.recv_from(&mut buf).await.unwrap();
                let request = Message::from_vec(&buf[..len]).unwrap();
                let query = request.queries[0].clone();
                let records = match query.query_type {
                    RecordType::A => &a,
                    RecordType::AAAA => &aaaa,
                    _ => &Records::Code(ResponseCode::NotImp),
                };
                let mut response = Message::response(request.metadata.id, OpCode::Query);
                response.metadata.recursion_available = true;
                response.add_query(query.clone());
                let target = Name::from_ascii(TARGET).unwrap();
                let (records, name) = match records {
                    Records::Alias {
                        ttl,
                        target: records,
                        together,
                    } if query.name.to_ascii().eq_ignore_ascii_case(ALIAS) => {
                        let alias = RData::CNAME(CNAME(target.clone()));
                        response.add_answer(Record::from_rdata(query.name.clone(), *ttl, alias));
                        (together.then_some(&**records), target)
                    }
                    Records::Alias { target, .. } => (Some(&**target), query.name.clone()),
                    records => (Some(records), query.name.clone()),
                };
                match records {
                    Some(Records::Found(addresses, ttl)) => {
                        for address in addresses {
                            let data = match address {
                                IpAddr::V4(v4) => RData::A(A(*v4)),
                                IpAddr::V6(v6) => RData::AAAA(AAAA(*v6)),
                            };
                            response.add_answer(Record::from_rdata(name.clone(), *ttl, data));
                        }
                    }
                    Some(Records::Code(code)) => response.metadata.response_code = *code,
                    Some(Records::Alias { .. }) | None => {}
                }
                socket
                    .send_to(&response.to_vec().unwrap(), from)
                    .await
                    .unwrap();
            }
        });
        let mut udp = ConnectionConfig::udp();
        udp.port = address.port();
        let server = NameServerConfig::new(address.ip(), true, vec![udp]);
        (
            ResolverConfig::from_name_servers(vec![server]),
            ResolverOpts::default(),
        )
    }

    async fn asking(ripestat: &str, a: Records, aaaa: Records) -> Resolver {
        let (config, options) = name_server(a, aaaa).await;
        Resolver::with_dns(ripestat, config, options).unwrap()
    }

    fn found(addresses: &[&str], ttl: u32) -> Records {
        Records::Found(addresses.iter().map(|a| a.parse().unwrap()).collect(), ttl)
    }

    const NODATA: Records = Records::Code(ResponseCode::NoError);

    async fn host(resolver: &Resolver, lifetime: Lifetime) -> Result<Answer> {
        resolver
            .answer(&Query::Host(ALIAS.trim_end_matches('.').into()), lifetime)
            .await
    }

    #[tokio::test]
    async fn a_host_answers_with_both_families_held_for_the_shorter_records() {
        let resolver = asking(
            RIPESTAT,
            found(&["192.0.2.7"], 3000),
            found(&["2001:db8::7"], 600),
        )
        .await;
        let answer = host(&resolver, Lifetime::List(DAY)).await.unwrap();
        assert_eq!(answer.networks, nets(&["192.0.2.7/32", "2001:db8::7/128"]));
        assert!(
            (secs(590)..=secs(600)).contains(&answer.ttl),
            "{:?}",
            answer.ttl
        );
    }

    /// The host behind an alias in each family in turn, the other without records.
    async fn behind_an_alias(together: bool) -> Vec<(Vec<IpNet>, Duration)> {
        let mut answers = Vec::new();
        for (a, aaaa) in [
            (Some(found(&["192.0.2.7"], 3000)), None),
            (None, Some(found(&["2001:db8::7"], 3000))),
        ] {
            let alias = |target: Option<Records>| {
                target.map_or(NODATA, |target| Records::Alias {
                    ttl: 600,
                    target: Box::new(target),
                    together,
                })
            };
            let resolver = asking(RIPESTAT, alias(a), alias(aaaa)).await;
            let answer = host(&resolver, Lifetime::List(DAY)).await.unwrap();
            answers.push((answer.networks, answer.ttl));
        }
        answers
    }

    #[tokio::test]
    async fn an_alias_answered_with_its_target_gives_the_targets_addresses_held_for_the_alias() {
        let answers = behind_an_alias(true).await;
        assert_eq!(
            answers
                .iter()
                .map(|(networks, _)| networks.clone())
                .collect::<Vec<_>>(),
            [nets(&["192.0.2.7/32"]), nets(&["2001:db8::7/128"])]
        );
        for (_, ttl) in answers {
            assert!((secs(590)..=secs(600)).contains(&ttl), "{ttl:?}");
        }
    }

    #[tokio::test]
    async fn an_alias_answered_alone_is_followed_to_its_target() {
        let answers = behind_an_alias(false).await;
        assert_eq!(
            answers
                .iter()
                .map(|(networks, _)| networks.clone())
                .collect::<Vec<_>>(),
            [nets(&["192.0.2.7/32"]), nets(&["2001:db8::7/128"])]
        );
        for (_, ttl) in answers {
            assert!((secs(590)..=secs(600)).contains(&ttl), "{ttl:?}");
        }
    }

    #[tokio::test]
    async fn a_family_without_records_is_empty_and_a_host_with_neither_is_an_error() {
        let resolver = asking(RIPESTAT, NODATA, found(&["2001:db8::7"], 600)).await;
        let answer = host(&resolver, Lifetime::List(DAY)).await.unwrap();
        assert_eq!(answer.networks, nets(&["2001:db8::7/128"]));
        let nxdomain = Records::Code(ResponseCode::NXDomain);
        let resolver = asking(RIPESTAT, nxdomain.clone(), nxdomain).await;
        assert!(host(&resolver, Lifetime::List(DAY)).await.is_err());
    }

    #[tokio::test]
    async fn a_family_whose_lookup_fails_fails_the_host() {
        let servfail = Records::Code(ResponseCode::ServFail);
        for (a, aaaa) in [
            (found(&["192.0.2.7"], 600), servfail.clone()),
            (servfail.clone(), found(&["2001:db8::7"], 600)),
        ] {
            let resolver = asking(RIPESTAT, a, aaaa).await;
            assert!(host(&resolver, Lifetime::List(DAY)).await.is_err());
        }
    }

    #[tokio::test]
    async fn an_address_for_a_host_is_its_own_answer_holding_for_the_list() {
        let resolver = asking(RIPESTAT, NODATA, NODATA).await;
        for (address, network) in [
            ("192.0.2.7", "192.0.2.7/32"),
            ("2001:db8::7", "2001:db8::7/128"),
        ] {
            let answer = resolver
                .answer(&Query::Host(address.into()), Lifetime::List(DAY))
                .await
                .unwrap();
            assert_eq!((answer.networks, answer.ttl), (nets(&[network]), DAY));
        }
    }

    const OK: &str = r#"{"status": "ok", "data": {
        "prefixes": [{"prefix": "192.0.2.0/24"}, {"prefix": "2001:db8::/32"}],
        "resources": {"ipv4": ["198.51.100.0/24"], "ipv6": ["3fff::/20"]}
    }}"#;

    #[tokio::test]
    async fn ripestat_is_asked_for_the_resource_by_this_app_and_answers_hold_for_the_list() {
        let (url, requests) = ripestat(200, OK).await;
        let resolver = asking(&url, NODATA, NODATA).await;
        for (query, networks) in [
            (
                Query::Asn("AS64496".into()),
                nets(&["192.0.2.0/24", "2001:db8::/32"]),
            ),
            (
                Query::Country("xx".into()),
                nets(&["198.51.100.0/24", "3fff::/20"]),
            ),
            (
                Query::Prefixes(vec!["203.0.113.0/24".into()]),
                nets(&["203.0.113.0/24"]),
            ),
        ] {
            let answer = resolver.answer(&query, Lifetime::List(DAY)).await.unwrap();
            assert_eq!((answer.networks, answer.ttl), (networks, DAY), "{query:?}");
        }
        let agent = || RIPESTAT_SOURCEAPP.to_owned();
        assert_eq!(
            *requests.lock().unwrap(),
            [
                (
                    "GET /data/announced-prefixes/data.json?resource=AS64496&sourceapp=slipmesh-egress HTTP/1.1".to_owned(),
                    agent()
                ),
                (
                    "GET /data/country-resource-list/data.json?resource=XX&v4_format=prefix&sourceapp=slipmesh-egress HTTP/1.1".to_owned(),
                    agent()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_malformed_query_is_refused_before_it_is_asked() {
        let (url, requests) = ripestat(200, OK).await;
        let resolver = asking(&url, NODATA, NODATA).await;
        for query in [
            Query::Asn("AS64496&x=1".into()),
            Query::Country("X&".into()),
            Query::Prefixes(vec!["192.0.2.0/33".into()]),
        ] {
            assert!(
                resolver.answer(&query, Lifetime::List(DAY)).await.is_err(),
                "{query:?}"
            );
        }
        assert!(requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_ripestat_error_status_or_wrong_answer_is_an_error_not_an_empty_answer() {
        for (status, body) in [
            (502, OK.to_owned()),
            (200, OK.replace(r#""ok""#, r#""error""#)),
            (200, json!({"data": {}}).to_string()),
            (200, "[".to_owned()),
        ] {
            let (url, _) = ripestat(status, &body).await;
            let resolver = asking(&url, NODATA, NODATA).await;
            for query in [Query::Asn("AS64496".into()), Query::Country("XX".into())] {
                assert!(
                    resolver.answer(&query, Lifetime::List(DAY)).await.is_err(),
                    "{status} {body} {query:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn an_answer_longer_than_the_limit_is_an_error() {
        let (url, _) = ripestat(200, OK).await;
        let response = || async { reqwest::get(&url).await.unwrap() };
        assert_eq!(
            capped(response().await, OK.len()).await.unwrap(),
            OK.as_bytes()
        );
        assert!(capped(response().await, OK.len() - 1).await.is_err());
    }

    fn spec(yaml: &str) -> EgressSpec {
        yaml_serde::from_str(yaml).unwrap()
    }

    #[tokio::test]
    async fn a_list_is_every_answer_merged_less_its_exclusions_and_the_excluded() {
        let (url, _) = ripestat(200, OK).await;
        let resolver = asking(&url, NODATA, NODATA).await;
        let resolved = resolver
            .resolve(
                &spec(
                    "{nodes: [node-a], ttl: 3600, include: [
                        {asns: [AS64496]},
                        {label: own, prefixes: [203.0.113.0/24], ttl: 60}
                    ], exclude: [{prefixes: [192.0.2.0/25]}]}",
                ),
                &nets(&["2001:db8::/33"]),
            )
            .await
            .unwrap();
        assert_eq!(
            resolved.networks,
            nets(&["192.0.2.128/25", "203.0.113.0/24", "2001:db8:8000::/33"])
        );
        let asked: Vec<_> = resolved
            .answers
            .iter()
            .map(|(query, answer)| (query.clone(), answer.ttl))
            .collect();
        assert_eq!(
            asked,
            [
                (Query::Asn("AS64496".into()), secs(3600)),
                (Query::Prefixes(vec!["203.0.113.0/24".into()]), secs(60)),
                (Query::Prefixes(vec!["192.0.2.0/25".into()]), secs(3600)),
            ]
        );
    }

    #[tokio::test]
    async fn one_failed_query_fails_the_list() {
        let (url, _) = ripestat(502, OK).await;
        let resolver = asking(&url, NODATA, NODATA).await;
        let failed = resolver
            .resolve(
                &spec(
                    "{nodes: [node-a], include: [{prefixes: [203.0.113.0/24]}, {asns: [AS64496]}]}",
                ),
                &[],
            )
            .await;
        assert!(failed.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_list_that_outlasts_its_budget_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        // Accepts and never answers.
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                held.push(listener.accept().await.unwrap());
            }
        });
        let resolver = asking(&url, NODATA, NODATA).await;
        let queries = [(Query::Asn("AS64496".into()), Lifetime::List(DAY))];
        let started = tokio::time::Instant::now();
        assert!(resolver.answer_all(&queries, secs(10)).await.is_err());
        assert_eq!(started.elapsed(), secs(10));
    }
}
