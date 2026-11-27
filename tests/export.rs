//! `egress-export` as a caller runs it.

const V1: &str = r#"slipmesh:
  kind: network
cluster:
  bgp_as: 64496
  loopback_networks: {ipv4: "198.51.100.0/25", ipv6: "3fff::/32"}
nodes:
  - {name: node-a, node_id: "198.51.100.1"}
bypass:
  - node: node-a
    include:
      - {kind: asn, label: example, asns: [AS64496]}
"#;

#[test]
fn the_egress_of_the_file_is_printed_and_the_file_left_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("slipmesh.yaml");
    std::fs::write(&path, V1).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_egress-export"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), V1);
    let printed: serde_json::Value =
        sliplib_yaml::from_str(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    assert_eq!(printed["kind"], "Egress");
    assert_eq!(printed["metadata"]["name"], "node-a");
}
