//! One `Egress` list resolved: each source asked as one query per ASN, country, host or literal
//! list, each answer with its own resolve time and lifetime, and the answers merged within their
//! label, less every exclusion.

use crate::crd::{EgressSpec, Source};
use anyhow::{Context, Result, bail, ensure};
use hickory_resolver::lookup::Lookup;
use hickory_resolver::net::NetError;
use hickory_resolver::proto::rr::{RData, RecordType};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sliplib_net::IpNet;
use std::collections::BTreeMap;
use std::future::Future;
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

/// One query of a list: whether it includes or excludes, its source's label, and how long its
/// answer holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub include: bool,
    pub label: Option<String>,
    pub query: Query,
    pub lifetime: Lifetime,
}

/// Every query of `spec`, its includes first, in the order written.
pub fn asked(spec: &EgressSpec) -> Vec<Asked> {
    let list = Duration::from_secs(spec.ttl.into());
    [(true, &spec.include), (false, &spec.exclude)]
        .into_iter()
        .flat_map(|(include, sources)| {
            sources.iter().flat_map(move |source| {
                let lifetime = source.ttl.map_or(Lifetime::List(list), |ttl| {
                    Lifetime::Source(Duration::from_secs(ttl.into()))
                });
                queries(source).into_iter().map(move |query| Asked {
                    include,
                    label: source.label.clone(),
                    query,
                    lifetime,
                })
            })
        })
        .collect()
}

/// The list `answers`, one for each of `asked`, make, less `excluded`.
pub fn compose(asked: &[Asked], answers: &[&Answer], excluded: &[IpNet]) -> Result<Vec<IpNet>> {
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for (asked, answer) in asked.iter().zip(answers) {
        if asked.include {
            include.push((asked.label.clone(), answer.networks.clone()));
        } else {
            exclude.extend(&answer.networks);
        }
    }
    list(&include, &exclude, excluded)
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
/// A query gives up after this many attempts, waiting twice as long after each failure, up to
/// `MAX_BACKOFF`.
const ATTEMPTS: u32 = 6;
const FIRST_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(180);

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
        let http = reqwest::Client::builder()
            .timeout(RIPESTAT_TIMEOUT)
            .user_agent(RIPESTAT_SOURCEAPP)
            .build()
            .context("failed to build the RIPEstat client")?;
        let dns = hickory_resolver::Resolver::builder_tokio()
            .context("failed to read the system's resolver configuration")?
            .build()
            .context("failed to build the DNS resolver")?;
        Ok(Self {
            ripestat: ripestat.trim_end_matches('/').to_owned(),
            http,
            ripestat_permits: tokio::sync::Semaphore::new(RIPESTAT_CONCURRENCY),
            dns,
        })
    }

    /// `query` resolved now, holding for `lifetime`.
    pub async fn answer(&self, query: &Query, lifetime: Lifetime) -> Result<Answer> {
        let resolved_at = SystemTime::now();
        let (networks, records_ttl) = match query {
            Query::Prefixes(prefixes) => (literal(prefixes)?, None),
            Query::Asn(_) | Query::Country(_) => {
                let url = ripestat_url(&self.ripestat, query)?;
                let networks = retry(|| async { ripestat_networks(query, &self.get(&url).await?) })
                    .await
                    .with_context(|| format!("{query:?} from RIPEstat"))?;
                (networks, None)
            }
            Query::Host(host) => match host.parse::<IpAddr>() {
                Ok(address) => (vec![IpNet::from(address)], None),
                Err(_) => {
                    let (v4, v6) = tokio::join!(
                        retry(|| async { family(self.lookup(host, RecordType::A).await) }),
                        retry(|| async { family(self.lookup(host, RecordType::AAAA).await) }),
                    );
                    let (networks, valid_until) = both_families(host, v4?, v6?)?;
                    let ttl = valid_until.saturating_duration_since(Instant::now());
                    (networks, Some(ttl))
                }
            },
        };
        Ok(Answer {
            networks,
            resolved_at,
            ttl: hold(lifetime, records_ttl),
        })
    }

    async fn lookup(&self, host: &str, kind: RecordType) -> Result<Lookup, NetError> {
        tokio::time::timeout(DNS_TIMEOUT, self.dns.lookup(host, kind))
            .await
            .unwrap_or_else(|_| Err(NetError::from("timed out")))
    }

    async fn get(&self, url: &str) -> Result<String> {
        let _permit = self
            .ripestat_permits
            .acquire()
            .await
            .context("the RIPEstat request limit is closed")?;
        let mut response = self
            .http
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("RIPEstat request to {url} failed"))?;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("failed to read RIPEstat's answer")?
        {
            ensure!(
                body.len() + chunk.len() <= RIPESTAT_BODY_LIMIT,
                "RIPEstat's answer is longer than {RIPESTAT_BODY_LIMIT} bytes"
            );
            body.extend_from_slice(&chunk);
        }
        String::from_utf8(body).context("RIPEstat's answer is not UTF-8")
    }
}

/// `attempt` until it succeeds or has failed `ATTEMPTS` times, the last error returned.
async fn retry<T, F: Future<Output = Result<T>>>(mut attempt: impl FnMut() -> F) -> Result<T> {
    let mut backoff = FIRST_BACKOFF;
    for _ in 1..ATTEMPTS {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), ?backoff, "retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
    attempt()
        .await
        .with_context(|| format!("gave up after {ATTEMPTS} attempts"))
}

/// The request for an ASN's or a country's networks. Both go into the URL, so each is checked
/// to be what it says first.
fn ripestat_url(base: &str, query: &Query) -> Result<String> {
    let path = match query {
        Query::Asn(asn) => {
            let digits = asn
                .strip_prefix("AS")
                .with_context(|| format!("ASN {asn:?} does not start with AS"))?;
            ensure!(
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
                "ASN {asn:?} is not AS and digits"
            );
            format!("announced-prefixes/data.json?resource={asn}")
        }
        Query::Country(country) => {
            ensure!(
                country.len() == 2 && country.bytes().all(|b| b.is_ascii_alphabetic()),
                "country {country:?} is not two letters"
            );
            // In upper case, as RIPEstat takes it; IPv4 as prefixes rather than ranges.
            let country = country.to_ascii_uppercase();
            format!("country-resource-list/data.json?resource={country}&v4_format=prefix")
        }
        Query::Host(_) | Query::Prefixes(_) => bail!("{query:?} is not asked of RIPEstat"),
    };
    Ok(format!("{base}/data/{path}&sourceapp={RIPESTAT_SOURCEAPP}"))
}

/// The networks of RIPEstat's answer `body` to `query`. A status other than `ok` or a missing
/// list is an error; an entry that is no network is left out, and said.
fn ripestat_networks(query: &Query, body: &str) -> Result<Vec<IpNet>> {
    let answer: Value = serde_json::from_str(body).context("RIPEstat's answer is not JSON")?;
    let status = answer.get("status").and_then(Value::as_str);
    ensure!(
        status == Some("ok"),
        "RIPEstat answered status {status:?}: {}",
        answer.get("message").and_then(Value::as_str).unwrap_or("")
    );
    let list = |pointer: &str| {
        answer
            .pointer(pointer)
            .and_then(Value::as_array)
            .with_context(|| format!("RIPEstat's answer has no list at {pointer}"))
    };
    let mut texts = Vec::new();
    if let Query::Asn(_) = query {
        for entry in list("/data/prefixes")? {
            texts.push(
                entry
                    .get("prefix")
                    .and_then(Value::as_str)
                    .context("an announced prefix without `prefix`")?,
            );
        }
    } else {
        for family in ["ipv4", "ipv6"] {
            for resource in list(&format!("/data/resources/{family}"))? {
                texts.push(
                    resource
                        .as_str()
                        .context("a country resource that is no text")?,
                );
            }
        }
    }
    let networks: Vec<IpNet> = texts
        .into_iter()
        .filter_map(|text| match sliplib_net::parse(text) {
            Ok(net) => Some(net),
            Err(e) => {
                tracing::warn!(entry = text, error = %e, "left out of a RIPEstat answer");
                None
            }
        })
        .collect();
    if networks.is_empty() {
        tracing::warn!(?query, "RIPEstat gives no network");
    }
    Ok(networks)
}

/// One family's addresses and until when they hold; none where the name has no such records or
/// does not exist.
fn family(found: Result<Lookup, NetError>) -> Result<Option<(Vec<IpNet>, Instant)>> {
    let lookup = match found {
        Err(e) if e.is_no_records_found() => return Ok(None),
        found => found.context("lookup failed")?,
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

/// Both families of `host` and until when the shorter holds; neither is an error.
fn both_families(
    host: &str,
    v4: Option<(Vec<IpNet>, Instant)>,
    v6: Option<(Vec<IpNet>, Instant)>,
) -> Result<(Vec<IpNet>, Instant)> {
    let found: Vec<_> = [v4, v6].into_iter().flatten().collect();
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

/// The lifetime of an answer whose records, if any, hold for `records`.
fn hold(lifetime: Lifetime, records: Option<Duration>) -> Duration {
    match (lifetime, records) {
        (Lifetime::Source(ttl), _) | (Lifetime::List(ttl), None) => ttl,
        (Lifetime::List(list), Some(records)) => records.min(list).max(MIN_HOST_TTL),
    }
}

fn literal(prefixes: &[String]) -> Result<Vec<IpNet>> {
    prefixes
        .iter()
        .map(|prefix| sliplib_net::parse(prefix).with_context(|| format!("prefix {prefix:?}")))
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
    use hickory_resolver::net::NoRecords;
    use hickory_resolver::proto::op::{Query as DnsQuery, ResponseCode};
    use hickory_resolver::proto::rr::rdata::{A, AAAA, CNAME};
    use hickory_resolver::proto::rr::{Name, Record};

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
    fn host_bits_are_cleared_a_network_two_labels_make_is_listed_once_and_empty_is_no_label() {
        assert_eq!(
            list(
                &[
                    (label("a"), nets(&["192.0.2.1/25", "192.0.2.128/25"])),
                    (label("b"), nets(&["192.0.2.0/24"])),
                    (None, nets(&["198.51.100.0/25"])),
                    (label(""), nets(&["198.51.100.128/25"])),
                ],
                &[],
                &[],
            )
            .unwrap(),
            nets(&["192.0.2.0/24", "198.51.100.0/24"])
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
    fn a_source_asks_one_query_per_asn_host_or_country_and_one_for_its_prefixes() {
        let source: Source = sliplib_yaml::from_str(
            "{asns: [AS64496], hostnames: [a.example], country: XX, prefixes: [192.0.2.0/24]}",
        )
        .unwrap();
        assert_eq!(
            queries(&source),
            [
                Query::Asn("AS64496".into()),
                Query::Country("XX".into()),
                Query::Host("a.example".into()),
                Query::Prefixes(vec!["192.0.2.0/24".into()]),
            ]
        );
    }

    #[test]
    fn ripestat_is_asked_for_the_checked_resource_by_this_app() {
        let base = "https://ripestat.example";
        assert_eq!(
            ripestat_url(base, &Query::Asn("AS64496".into())).unwrap(),
            "https://ripestat.example/data/announced-prefixes/data.json?resource=AS64496&sourceapp=slipmesh-egress"
        );
        assert_eq!(
            ripestat_url(base, &Query::Country("xx".into())).unwrap(),
            "https://ripestat.example/data/country-resource-list/data.json?resource=XX&v4_format=prefix&sourceapp=slipmesh-egress"
        );
        for refused in [
            Query::Asn("64496".into()),
            Query::Asn("AS".into()),
            Query::Asn("AS64496&x=1".into()),
            Query::Country("X".into()),
            Query::Country("XXX".into()),
            Query::Country("X&".into()),
            Query::Host("a.example".into()),
        ] {
            assert!(ripestat_url(base, &refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn ripestat_answers_give_both_families_and_leave_out_what_is_no_network() {
        let asn = Query::Asn("AS64496".into());
        let country = Query::Country("XX".into());
        assert_eq!(
            ripestat_networks(
                &asn,
                r#"{"status": "ok", "data": {"prefixes": [
                    {"prefix": "192.0.2.0/24"}, {"prefix": "2001:db8::/32"}, {"prefix": "not a network"}
                ]}}"#
            )
            .unwrap(),
            nets(&["192.0.2.0/24", "2001:db8::/32"])
        );
        assert_eq!(
            ripestat_networks(&asn, r#"{"status": "ok", "data": {"prefixes": []}}"#).unwrap(),
            []
        );
        assert_eq!(
            ripestat_networks(
                &country,
                r#"{"status": "ok", "data": {"resources": {"ipv4": ["198.51.100.0/24"], "ipv6": ["3fff::/20"]}}}"#
            )
            .unwrap(),
            nets(&["198.51.100.0/24", "3fff::/20"])
        );
    }

    #[test]
    fn a_ripestat_answer_not_ok_or_without_its_lists_is_an_error() {
        let asn = Query::Asn("AS64496".into());
        let country = Query::Country("XX".into());
        for (query, body) in [
            (&asn, r#"{"status": "error", "data": {"prefixes": []}}"#),
            (&asn, r#"{"data": {"prefixes": []}}"#),
            (&asn, r#"{"status": "ok", "data": {}}"#),
            (
                &asn,
                r#"{"status": "ok", "data": {"prefixes": [{"origin": 64496}]}}"#,
            ),
            (&asn, "["),
            (
                &country,
                r#"{"status": "ok", "data": {"resources": {"ipv4": []}}}"#,
            ),
            (
                &country,
                r#"{"status": "ok", "data": {"resources": {"ipv6": []}}}"#,
            ),
            (
                &country,
                r#"{"status": "ok", "data": {"resources": {"ipv4": [4], "ipv6": []}}}"#,
            ),
        ] {
            assert!(ripestat_networks(query, body).is_err(), "{body}");
        }
    }

    fn no_records(code: ResponseCode) -> Result<Lookup, NetError> {
        Err(NoRecords::new(DnsQuery::new(), code).into())
    }

    #[test]
    fn a_family_is_its_addresses_none_without_records_and_any_other_failure_an_error() {
        let name = Name::from_ascii("a.example.").unwrap();
        let until = Instant::now() + secs(600);
        let records = [
            Record::from_rdata(name.clone(), 600, RData::CNAME(CNAME(name.clone()))),
            Record::from_rdata(name.clone(), 600, RData::A(A::new(192, 0, 2, 7))),
            Record::from_rdata(
                name,
                600,
                RData::AAAA(AAAA::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 7)),
            ),
        ];
        assert_eq!(
            family(Ok(Lookup::new_with_deadline(
                DnsQuery::new(),
                records,
                until
            )))
            .unwrap(),
            Some((nets(&["192.0.2.7/32", "2001:db8::7/128"]), until))
        );
        assert_eq!(family(no_records(ResponseCode::NXDomain)).unwrap(), None);
        assert_eq!(family(no_records(ResponseCode::NoError)).unwrap(), None);
        assert!(family(Err(NetError::from("timed out"))).is_err());
    }

    #[test]
    fn a_host_is_both_families_held_until_the_shorter_and_neither_is_an_error() {
        let soon = Instant::now() + secs(60);
        let later = soon + secs(600);
        assert_eq!(
            both_families(
                "a.example",
                Some((nets(&["192.0.2.7/32"]), later)),
                Some((nets(&["2001:db8::7/128"]), soon)),
            )
            .unwrap(),
            (nets(&["192.0.2.7/32", "2001:db8::7/128"]), soon)
        );
        assert_eq!(
            both_families("a.example", None, Some((nets(&["2001:db8::7/128"]), soon))).unwrap(),
            (nets(&["2001:db8::7/128"]), soon)
        );
        assert!(both_families("a.example", None, None).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_query_is_retried_with_a_growing_backoff_until_it_succeeds_or_gives_up() {
        let started = tokio::time::Instant::now();
        let failures = std::cell::Cell::new(2);
        let succeeded = retry(|| async {
            if failures.get() == 0 {
                return Ok(());
            }
            failures.set(failures.get() - 1);
            bail!("failed")
        })
        .await;
        assert!(succeeded.is_ok());
        assert_eq!(started.elapsed(), FIRST_BACKOFF * 3);

        let started = tokio::time::Instant::now();
        let attempts = std::cell::Cell::new(0);
        let gave_up: Result<()> = retry(|| async {
            attempts.set(attempts.get() + 1);
            bail!("failed")
        })
        .await;
        assert!(gave_up.is_err());
        assert_eq!(attempts.get(), ATTEMPTS);
        // 5 + 10 + 20 + 40 + 80 s.
        assert_eq!(started.elapsed(), secs(155));
    }

    #[test]
    fn a_list_asks_each_source_for_its_own_lifetime_and_is_composed_less_its_exclusions() {
        let spec: EgressSpec = sliplib_yaml::from_str(
            "{nodes: [node-a], ttl: 3600, include: [
                {label: a, prefixes: [192.0.2.0/24]},
                {prefixes: [203.0.113.0/24], ttl: 60}
            ], exclude: [{prefixes: [192.0.2.0/25]}]}",
        )
        .unwrap();
        let asked = asked(&spec);
        let shape: Vec<_> = asked
            .iter()
            .map(|asked| (asked.include, asked.label.as_deref(), asked.lifetime))
            .collect();
        assert_eq!(
            shape,
            [
                (true, Some("a"), Lifetime::List(secs(3600))),
                (true, None, Lifetime::Source(secs(60))),
                (false, None, Lifetime::List(secs(3600))),
            ]
        );
        let answer = |net: &str| Answer {
            networks: nets(&[net]),
            resolved_at: SystemTime::UNIX_EPOCH,
            ttl: secs(60),
        };
        let answers = [
            answer("192.0.2.0/24"),
            answer("203.0.113.0/24"),
            answer("192.0.2.0/25"),
        ];
        assert_eq!(
            compose(
                &asked,
                &answers.iter().collect::<Vec<_>>(),
                &nets(&["203.0.113.0/25"])
            )
            .unwrap(),
            nets(&["192.0.2.128/25", "203.0.113.128/25"])
        );
    }
}
