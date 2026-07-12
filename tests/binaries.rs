use assert_cmd::Command;

fn assert_version(mut command: Command, name: &str) {
    command
        .arg("--version")
        .assert()
        .success()
        .stdout(format!("{name} {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn every_distributed_binary_reports_its_version() {
    assert_version(assert_cmd::cargo::cargo_bin_cmd!("loom"), "loom");
    assert_version(assert_cmd::cargo::cargo_bin_cmd!("loomd"), "loomd");
    assert_version(assert_cmd::cargo::cargo_bin_cmd!("loom-mcp"), "loom-mcp");
    assert_version(
        assert_cmd::cargo::cargo_bin_cmd!("loom-supervisor"),
        "loom-supervisor",
    );
}
