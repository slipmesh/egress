//! The `Egress` custom resource: one list of networks, the exit nodes that route it out, and each
//! node's account of it. Every rule is CEL, so the API server and the tests check the same text.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResource, KubeSchema};
use serde::{Deserialize, Serialize};

/// What the schema writes where a CR leaves it out, so the stored object always has it.
const DEFAULT_TTL_SECONDS: u32 = 86_400;

// Every list and string is bounded: the API server sizes a CEL rule's cost from the bounds, and
// refuses the CRD without them.
const MAX_NODES: u32 = 100;
const MAX_SOURCES: u32 = 100;
const MAX_VALUES: u32 = 1000;

fn default_ttl() -> u32 {
    DEFAULT_TTL_SECONDS
}

/// Read strictly: a new field reaches the agents before any CR carries it.
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kube(
    doc = "One list of networks, resolved and routed out through the exit nodes it names",
    group = "slipmesh.net",
    version = "v1alpha1",
    kind = "Egress",
    plural = "egresses",
    status = "EgressStatus",
    printcolumn = r#"{"name": "Nodes", "type": "string", "jsonPath": ".spec.nodes"}"#,
    printcolumn = r#"{"name": "Install", "type": "boolean", "jsonPath": ".spec.install"}"#,
    printcolumn = r#"{"name": "TTL", "type": "integer", "jsonPath": ".spec.ttl"}"#,
    printcolumn = r#"{"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"}"#
)]
#[x_kube(validation = "size(self.nodes) > 0")]
#[x_kube(validation = "size(self.include) > 0")]
#[x_kube(validation = "self.ttl >= 1")]
#[cfg_attr(test, x_kube(cel))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EgressSpec {
    /// The exit nodes, by their Kubernetes `Node` names.
    #[schemars(length(max = MAX_NODES), inner(length(max = 253)))]
    #[schemars(extend("x-kubernetes-list-type" = "set"))]
    pub nodes: Vec<String>,
    /// Seconds an answer holds before it is resolved again, unless its source says otherwise or it
    /// is a host's, whose records' own TTL counts within it.
    #[serde(default = "default_ttl")]
    pub ttl: u32,
    /// Off, the list is counted and the kernel left alone.
    #[serde(default)]
    pub install: bool,
    #[schemars(length(max = MAX_SOURCES))]
    pub include: Vec<Source>,
    #[schemars(length(max = MAX_SOURCES))]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<Source>,
}

/// One kind of source alone; a `label` keeps its networks from merging with another label's.
#[derive(KubeSchema, Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[x_kube(
    validation = "[has(self.asns), has(self.prefixes), has(self.country), has(self.hostnames)].filter(x, x).size() == 1"
)]
#[x_kube(
    validation = "!has(self.asns) || (size(self.asns) > 0 && self.asns.all(a, a.matches('^AS[0-9]+$')))"
)]
#[x_kube(
    validation = "!has(self.prefixes) || (size(self.prefixes) > 0 && self.prefixes.all(p, isCIDR(p)))"
)]
#[x_kube(validation = "!has(self.country) || self.country.matches('^[A-Za-z]{2}$')")]
#[x_kube(validation = "!has(self.hostnames) || size(self.hostnames) > 0")]
#[x_kube(validation = "!has(self.ttl) || self.ttl >= 1")]
#[cfg_attr(test, x_kube(cel))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    #[schemars(with = "String", length(max = 63))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[schemars(with = "Vec<String>", length(max = MAX_VALUES), inner(length(max = 12)))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asns: Option<Vec<String>>,
    #[schemars(with = "Vec<String>", length(max = MAX_VALUES), inner(length(max = 43)))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefixes: Option<Vec<String>>,
    /// ISO 3166-1 alpha-2.
    #[schemars(with = "String", length(max = 2))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[schemars(with = "Vec<String>", length(max = MAX_VALUES), inner(length(max = 253)))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostnames: Option<Vec<String>>,
    /// Seconds every answer of this source holds, over the list's `ttl` and a host record's own.
    #[schemars(with = "u32")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u32>,
}

/// Each node's account, written by that node alone through server-side apply.
#[derive(KubeSchema, Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct EgressStatus {
    #[schemars(extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["node"]))]
    #[schemars(length(max = MAX_NODES))]
    #[serde(default)]
    pub nodes: Vec<NodeStatus>,
}

#[derive(KubeSchema, Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    #[schemars(length(max = 253))]
    pub node: String,
    pub observed_generation: i64,
    /// When the list was last resolved.
    #[schemars(with = "Time")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<Time>,
    pub prefixes: FamilyCounts,
    pub routes: FamilyCounts,
    /// `sha256:` and the hex digest of the sorted list, one network per line.
    #[schemars(with = "String", length(max = 71))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[schemars(extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = ["type"]))]
    #[schemars(length(max = 8))]
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[derive(KubeSchema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FamilyCounts {
    pub ipv4: u32,
    pub ipv6: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;
    use serde_json::{Value, json};

    fn accepted(validate: impl Fn(&Value) -> bool, cases: &[(Value, bool)]) {
        for (value, valid) in cases {
            assert_eq!(validate(value), *valid, "{value}");
        }
    }

    #[test]
    fn a_source_is_exactly_one_kind_each_well_formed() {
        accepted(
            |value| Source::validate_cel(value, None).is_ok(),
            &[
                (json!({"asns": ["AS64496"], "label": "a"}), true),
                (json!({"prefixes": ["192.0.2.0/24", "2001:db8::/32"]}), true),
                (json!({"country": "xx"}), true),
                (json!({"hostnames": ["example.com"]}), true),
                (json!({"label": "a"}), false),
                (json!({"asns": ["AS64496"], "country": "XX"}), false),
                (json!({"asns": []}), false),
                (json!({"asns": ["64496"]}), false),
                (json!({"asns": ["AS64496&x=1"]}), false),
                (json!({"prefixes": []}), false),
                (json!({"prefixes": ["192.0.2.0/33"]}), false),
                (json!({"country": "XXX"}), false),
                (json!({"country": "X1"}), false),
                (json!({"hostnames": []}), false),
                (json!({"country": "XX", "ttl": 60}), true),
                (json!({"country": "XX", "ttl": 0}), false),
            ],
        );
    }

    #[test]
    fn a_spec_names_a_node_and_includes_something() {
        let spec = |nodes: Value, include: Value, ttl: u32| json!({"nodes": nodes, "include": include, "ttl": ttl, "install": false});
        let source = json!([{"country": "XX"}]);
        accepted(
            |value| EgressSpec::validate_cel(value, None).is_ok(),
            &[
                (spec(json!(["node-a", "node-b"]), source.clone(), 60), true),
                (spec(json!([]), source.clone(), 60), false),
                (spec(json!(["node-a"]), json!([]), 60), false),
                (spec(json!(["node-a"]), source.clone(), 0), false),
            ],
        );
    }

    #[test]
    fn what_a_cr_leaves_out_reads_as_the_schema_writes_it() {
        let read: EgressSpec =
            serde_json::from_value(json!({"nodes": ["node-a"], "include": [{"country": "XX"}]}))
                .unwrap();
        assert_eq!((read.ttl, read.install), (DEFAULT_TTL_SECONDS, false));

        let crd = serde_json::to_value(Egress::crd()).unwrap();
        let spec = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"];
        assert_eq!(spec["properties"]["ttl"]["default"], DEFAULT_TTL_SECONDS);
        assert_eq!(spec["properties"]["install"]["default"], false);
    }

    #[test]
    fn the_example_reads_whole_with_a_status_and_passes_every_rule() {
        let mut egress: Egress =
            sliplib_yaml::from_str(include_str!("../docs/example/egress.yaml")).unwrap();
        egress.status = Some(
            serde_json::from_value(json!({"nodes": [{
                "node": "node-a",
                "observedGeneration": 1,
                "resolvedAt": "2026-01-01T00:00:00Z",
                "prefixes": {"ipv4": 2, "ipv6": 1},
                "routes": {"ipv4": 0, "ipv6": 0},
                "digest": format!("sha256:{}", "0".repeat(64)),
                "conditions": [{
                    "type": "Resolved",
                    "status": "True",
                    "reason": "Resolved",
                    "message": "",
                    "lastTransitionTime": "2026-01-01T00:00:00Z",
                }],
            }]}))
            .unwrap(),
        );
        let spec = serde_json::to_value(&egress.spec).unwrap();
        EgressSpec::validate_cel(&spec, None).unwrap();
        for source in spec["include"].as_array().unwrap() {
            Source::validate_cel(source, None).unwrap();
        }
        assert_eq!(egress.spec.include[3].ttl, Some(300));
    }

    #[test]
    fn no_field_of_the_spec_is_nullable_in_the_crd() {
        let crd = serde_json::to_value(Egress::crd()).unwrap();
        let spec = crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            .to_string();
        assert!(!spec.contains("nullable"), "{spec}");
    }
}
