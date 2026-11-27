//! `egress crd` as a caller runs it.

fn printed(kind: &str) -> String {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_egress"))
        .args(["crd", kind])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn each_crd_in_deploy_is_what_egress_crd_prints() {
    for (kind, committed) in [
        ("egress", include_str!("../deploy/egress-crd.yaml")),
        ("mesh", include_str!("../deploy/mesh-crd.yaml")),
    ] {
        assert!(
            printed(kind) == committed,
            "deploy/{kind}-crd.yaml differs from `egress crd {kind}`: write it again as the README says"
        );
    }
}
