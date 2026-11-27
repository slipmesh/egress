//! `egress crd` as a caller runs it.

#[test]
fn the_crd_in_deploy_is_what_egress_crd_prints() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_egress"))
        .arg("crd")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout).unwrap() == include_str!("../deploy/egress-crd.yaml"),
        "deploy/egress-crd.yaml differs from `egress crd`: write it again as the README says"
    );
}
