//! One-off: prints the `Egress` resources of the `bypass` lists of a `slipmesh.yaml` of format
//! version 1, one per node, named for it.

use anyhow::{Context, Result, bail};
use clap::Parser;
use egress::crd::{Egress, EgressSpec, Source};
use sliplib_config_v1::router::{BypassSourceEntry, DEFAULT_BYPASS_REFRESH_INTERVAL_SECS};
use sliplib_config_v1::slipmesh_file::SlipmeshFile;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Print the Egress resources of a version 1 slipmesh.yaml's bypass lists"
)]
struct Cli {
    /// Read, never written.
    file: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let raw = std::fs::read_to_string(&cli.file)
        .with_context(|| format!("failed to read {}", cli.file.display()))?;
    print!("{}", manifests(&raw)?);
    Ok(())
}

/// Every list as YAML documents.
fn manifests(raw: &str) -> Result<String> {
    let file = SlipmeshFile::parse(raw).context("failed to read the file as format version 1")?;
    let topology = file.topology();
    let ttl = topology
        .cluster
        .bypass_refresh_interval_secs
        .unwrap_or(DEFAULT_BYPASS_REFRESH_INTERVAL_SECS);
    let ttl = u32::try_from(ttl)
        .with_context(|| format!("cluster.bypass_refresh_interval_secs {ttl} is too long"))?;
    // Version 1 merges a node's entries in file order: one exclude cuts another entry's includes,
    // and a label merges across them.
    let mut by_node: Vec<(&str, Vec<BypassSourceEntry>, Vec<BypassSourceEntry>)> = Vec::new();
    for entry in &topology.bypass {
        let index = match by_node.iter().position(|(node, ..)| *node == entry.node) {
            Some(index) => index,
            None => {
                by_node.push((&entry.node, Vec::new(), Vec::new()));
                by_node.len() - 1
            }
        };
        by_node[index].1.extend(entry.include.iter().cloned());
        by_node[index].2.extend(entry.exclude.iter().cloned());
    }
    let mut documents = Vec::new();
    for (node, include, exclude) in by_node {
        let spec = EgressSpec {
            nodes: vec![node.to_owned()],
            ttl,
            install: false,
            via: None,
            include: sources(&include).with_context(|| format!("bypass of {node}"))?,
            exclude: sources(&exclude).with_context(|| format!("bypass of {node}"))?,
        };
        documents.push(sliplib_yaml::to_string(&Egress::new(node, spec))?);
    }
    Ok(documents.join("---\n"))
}

/// A literal source's prefixes go one source per label, in the order the labels first appear:
/// the daemon merges networks within a label alone, and a source has one.
fn sources(entries: &[BypassSourceEntry]) -> Result<Vec<Source>> {
    let mut out = Vec::new();
    // The daemon takes a missing label and an empty one as the same group.
    let labelled = |label: Option<String>| label.filter(|label| !label.is_empty());
    for entry in entries {
        let label = labelled(entry.label.clone());
        match entry.kind.as_str() {
            "literal" => {
                let prefixes = entry
                    .prefixes
                    .as_ref()
                    .context("a literal source without `prefixes`")?;
                let mut by_label: Vec<(Option<String>, Vec<String>)> = Vec::new();
                for prefix in prefixes {
                    let label = labelled(prefix.label.clone());
                    // As the daemon read it: the CRD takes no zero before a prefix length.
                    let net = sliplib_net_v1::parse(&prefix.net)
                        .with_context(|| format!("literal prefix {:?}", prefix.net))?
                        .to_string();
                    match by_label.iter_mut().find(|(seen, _)| *seen == label) {
                        Some((_, nets)) => nets.push(net),
                        None => by_label.push((label, vec![net])),
                    }
                }
                out.extend(by_label.into_iter().map(|(label, nets)| Source {
                    label,
                    prefixes: Some(nets),
                    ..Source::default()
                }));
            }
            "asn" => out.push(Source {
                label,
                asns: entry.asns.clone(),
                ..Source::default()
            }),
            "dns" => out.push(Source {
                label,
                hostnames: entry.hostnames.clone(),
                ..Source::default()
            }),
            "geoip" => {
                let country = entry
                    .country
                    .as_deref()
                    .context("a geoip source without `country`")?
                    .to_ascii_uppercase();
                // The daemon fills in a label only where none is written; an empty one stays.
                let label = match &entry.label {
                    None => Some(format!("geoip {country}")),
                    Some(label) => labelled(Some(label.clone())),
                };
                out.push(Source {
                    label,
                    country: Some(country),
                    ..Source::default()
                });
            }
            other => bail!("unknown bypass source kind {other:?}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = r#"slipmesh:
  kind: network
cluster:
  bgp_as: 64496
  loopback_networks: {ipv4: "198.51.100.0/25", ipv6: "3fff::/32"}
  bypass_refresh_interval_secs: 3600
nodes:
  - {name: node-a, node_id: "198.51.100.1"}
  - {name: node-b, node_id: "198.51.100.2"}
bypass:
  - node: node-a
    include:
      - {kind: asn, label: example, asns: [AS64496, AS64497]}
      - kind: literal
        prefixes:
          - {net: 192.0.2.0/25, label: one}
          - {net: 198.51.100.128/25}
          - {net: 192.0.2.128/25, label: one}
      - {kind: geoip, country: xx}
      - {kind: dns, label: names, hostnames: [example.com]}
    exclude:
      - kind: literal
        prefixes: [{net: 203.0.113.0/24}]
  - node: node-b
    include:
      - {kind: geoip, label: country, country: XX}
  - node: node-a
    include:
      - {kind: geoip, label: "", country: XY}
      - kind: literal
        prefixes:
          - {net: 192.0.2.0/024, label: ""}
          - {net: 3fff::/20}
    exclude:
      - {kind: dns, label: names, hostnames: [example.net]}
"#;

    fn yaml(text: &str) -> Vec<serde_json::Value> {
        text.split("\n---\n")
            .map(|document| sliplib_yaml::from_str(document).unwrap())
            .collect()
    }

    #[test]
    fn the_lists_of_each_node_become_one_egress_in_file_order_with_every_source_kept() {
        assert_eq!(
            yaml(&manifests(V1).unwrap()),
            yaml(
                "apiVersion: slipmesh.net/v1alpha1
kind: Egress
metadata: {name: node-a}
spec:
  nodes: [node-a]
  ttl: 3600
  install: false
  include:
    - {label: example, asns: [AS64496, AS64497]}
    - {label: one, prefixes: [192.0.2.0/25, 192.0.2.128/25]}
    - {prefixes: [198.51.100.128/25]}
    - {label: geoip XX, country: XX}
    - {label: names, hostnames: [example.com]}
    - {country: XY}
    - {prefixes: [192.0.2.0/24, 3fff::/20]}
  exclude:
    - {prefixes: [203.0.113.0/24]}
    - {label: names, hostnames: [example.net]}
---
apiVersion: slipmesh.net/v1alpha1
kind: Egress
metadata: {name: node-b}
spec:
  nodes: [node-b]
  ttl: 3600
  install: false
  include:
    - {label: country, country: XX}"
            )
        );
    }

    #[test]
    fn a_literal_source_without_prefixes_is_refused_naming_its_node() {
        let without = V1.replace(
            "      - {kind: dns, label: names, hostnames: [example.com]}\n",
            "      - {kind: literal, label: empty}\n",
        );
        let refused = format!("{:#}", manifests(&without).unwrap_err());
        assert!(refused.contains("node-a"), "{refused}");
    }

    #[test]
    fn a_file_without_a_refresh_interval_gets_the_daemons_default_written_out() {
        let without = V1.replace("  bypass_refresh_interval_secs: 3600\n", "");
        for egress in yaml(&manifests(&without).unwrap()) {
            assert_eq!(egress["spec"]["ttl"], 86_400, "{egress}");
        }
    }
}
