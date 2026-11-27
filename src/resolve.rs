//! One `Egress` list resolved: each source asked as one query per ASN, country, host or literal
//! list, each answer with its own resolve time and lifetime, and the answers merged within their
//! label, less every exclusion.

use crate::crd::Source;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sliplib_net::IpNet;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

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
    /// TTL, at least `MIN_HOST_TTL` and at most this.
    List(Duration),
}

/// RIPEstat's documented limit is 8 concurrent requests per address; this stays under it.
const RIPESTAT_CONCURRENCY: usize = 5;
/// From some networks RIPEstat takes half a minute to answer and close to one to fail.
const RIPESTAT_TIMEOUT: Duration = Duration::from_secs(90);
/// Its self-identification parameter: letters, digits, `-` and `_`.
const RIPESTAT_SOURCEAPP: &str = "slipmesh-egress";
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
        let (networks, server_ttl) = match query {
            Query::Prefixes(prefixes) => (literal(prefixes)?, None),
            Query::Asn(asn) => {
                validate_asn(asn)?;
                let response = self
                    .ripestat(&format!("announced-prefixes/data.json?resource={asn}"))
                    .await
                    .with_context(|| format!("the prefixes {asn} announces"))?;
                (announced(&response), None)
            }
            Query::Country(country) => {
                let country = validate_country(country)?;
                let response = self
                    .ripestat(&format!(
                        "country-resource-list/data.json?resource={country}"
                    ))
                    .await
                    .with_context(|| format!("the networks of country {country}"))?;
                (country_resources(&response), None)
            }
            Query::Host(host) => {
                let found = self
                    .dns
                    .lookup_ip(host.as_str())
                    .await
                    .with_context(|| format!("failed to look up {host}"))?;
                let networks = found.iter().map(IpNet::from).collect();
                let ttl = found
                    .valid_until()
                    .saturating_duration_since(std::time::Instant::now());
                (networks, Some(ttl))
            }
        };
        let ttl = match (lifetime, server_ttl) {
            (Lifetime::Source(ttl), _) | (Lifetime::List(ttl), None) => ttl,
            (Lifetime::List(list), Some(ttl)) => ttl.clamp(MIN_HOST_TTL, list.max(MIN_HOST_TTL)),
        };
        Ok(Answer {
            networks,
            resolved_at,
            ttl,
        })
    }

    async fn ripestat(&self, path: &str) -> Result<serde_json::Value> {
        let _permit = self
            .ripestat_permits
            .acquire()
            .await
            .context("the RIPEstat request limit is closed")?;
        let url = format!(
            "{}/data/{path}&sourceapp={RIPESTAT_SOURCEAPP}",
            self.ripestat
        );
        self.http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("RIPEstat request to {url} failed"))?
            .error_for_status()
            .context("RIPEstat answered with an error")?
            .json()
            .await
            .context("RIPEstat's answer is not JSON")
    }
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

fn announced(response: &serde_json::Value) -> Vec<IpNet> {
    networks(
        response
            .pointer("/data/prefixes")
            .and_then(|prefixes| prefixes.as_array())
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("prefix")?.as_str()),
    )
}

fn country_resources(response: &serde_json::Value) -> Vec<IpNet> {
    networks(
        ["ipv4", "ipv6"]
            .into_iter()
            .filter_map(|family| response.pointer(&format!("/data/resources/{family}")))
            .filter_map(|resources| resources.as_array())
            .flatten()
            .filter_map(|resource| resource.as_str()),
    )
}

/// RIPEstat's answers hold address ranges too, which are no network: those are left out.
fn networks<'a>(texts: impl Iterator<Item = &'a str>) -> Vec<IpNet> {
    texts
        .filter_map(|text| match sliplib_net::parse(text) {
            Ok(net) => Some(net),
            Err(e) => {
                tracing::debug!(entry = text, error = %e, "left out of a RIPEstat answer");
                None
            }
        })
        .collect()
}

/// `include`'s networks, each label merged on its own, less every network of `exclude` and
/// `excluded`. A network listed under two labels keeps the first. Nothing left is an error: an
/// empty list would mean a source that quietly gave nothing.
pub fn list(
    include: &[(Option<String>, Vec<IpNet>)],
    exclude: &[IpNet],
    excluded: &[IpNet],
) -> Result<Vec<IpNet>> {
    let mut first_label: BTreeMap<IpNet, &Option<String>> = BTreeMap::new();
    for (label, networks) in include {
        for net in networks {
            first_label.entry(net.trunc()).or_insert(label);
        }
    }
    let mut by_label: BTreeMap<&Option<String>, Vec<IpNet>> = BTreeMap::new();
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
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const DAY: Duration = Duration::from_secs(86_400);

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|n| n.parse().unwrap()).collect()
    }

    fn label(name: &str) -> Option<String> {
        Some(name.to_owned())
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
                    // Listed under `a` first, so it merges there.
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
    fn ripestat_answers_give_both_families_and_leave_out_ranges() {
        assert_eq!(
            announced(&json!({"data": {"prefixes": [
                {"prefix": "192.0.2.0/24"},
                {"prefix": "2001:db8::/32"},
                {"prefix": "not a network"},
            ]}})),
            nets(&["192.0.2.0/24", "2001:db8::/32"])
        );
        assert_eq!(
            country_resources(&json!({"data": {"resources": {
                "ipv4": ["198.51.100.0/24", "198.51.100.0-198.51.100.9"],
                "ipv6": ["3fff::/20"],
            }}})),
            nets(&["198.51.100.0/24", "3fff::/20"])
        );
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

    /// A RIPEstat stand-in answering every request with `status` and `body`.
    async fn ripestat(status: u16, body: serde_json::Value) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = [0; 4096];
                let _ = conn.read(&mut buf).await;
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(response.as_bytes()).await;
            }
        });
        url
    }

    #[tokio::test]
    async fn ripestat_and_literal_answers_hold_for_the_lists_lifetime() {
        let url = ripestat(
            200,
            json!({"data": {
                "prefixes": [{"prefix": "192.0.2.0/24"}, {"prefix": "2001:db8::/32"}],
                "resources": {"ipv4": ["198.51.100.0/24"], "ipv6": ["3fff::/20"]},
            }}),
        )
        .await;
        let resolver = Resolver::new(&url).unwrap();
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
    }

    #[tokio::test]
    async fn a_host_answers_with_both_families_within_the_floor_and_the_list_unless_its_source_says()
     {
        let resolver = Resolver::new(RIPESTAT).unwrap();
        for (host, address) in [
            ("192.0.2.7", "192.0.2.7/32"),
            ("2001:db8::7", "2001:db8::7/128"),
        ] {
            let answer = resolver
                .answer(&Query::Host(host.into()), Lifetime::List(DAY))
                .await
                .unwrap();
            assert_eq!(answer.networks, nets(&[address]));
            assert!(
                (MIN_HOST_TTL..=DAY).contains(&answer.ttl),
                "{:?}",
                answer.ttl
            );
        }
        let own = Duration::from_secs(5);
        let answer = resolver
            .answer(&Query::Host("192.0.2.7".into()), Lifetime::Source(own))
            .await
            .unwrap();
        assert_eq!(answer.ttl, own);
    }

    #[tokio::test]
    async fn a_ripestat_error_or_a_malformed_query_is_an_error_not_an_empty_answer() {
        let url = ripestat(502, json!({})).await;
        let resolver = Resolver::new(&url).unwrap();
        for query in [
            Query::Asn("AS64496".into()),
            Query::Country("XX".into()),
            Query::Asn("AS64496&x=1".into()),
            Query::Prefixes(vec!["192.0.2.0/33".into()]),
        ] {
            assert!(
                resolver.answer(&query, Lifetime::List(DAY)).await.is_err(),
                "{query:?}"
            );
        }
    }
}
