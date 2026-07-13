use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

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

#[test]
fn watch_cli_enforces_interactive_contract() {
    assert_cmd::cargo::cargo_bin_cmd!("loom")
        .args(["watch", "session", "--active"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));

    let temp = TempDir::new().unwrap();
    for extra in [["--json", "watch", "session"], ["watch", "session", ""]] {
        let mut command = assert_cmd::cargo::cargo_bin_cmd!("loom");
        command
            .env("LOOMTERM_STATE_DIR", temp.path().join("state"))
            .env("LOOMTERM_RUNTIME_DIR", temp.path().join("run"))
            .env("LOOMTERM_CONFIG", temp.path().join("config.toml"))
            .arg("--no-autostart");
        if extra[2].is_empty() {
            command.args(&extra[..2]);
        } else {
            command.args(extra);
        }
        let expected = if extra[0] == "--json" {
            "--json is not supported"
        } else {
            "requires an interactive terminal"
        };
        command
            .assert()
            .failure()
            .stderr(predicate::str::contains(expected));
    }
}

#[test]
fn operator_ui_is_the_default_and_requires_a_terminal() {
    let temp = TempDir::new().unwrap();
    for args in [Vec::<&str>::new(), vec!["ui"]] {
        assert_cmd::cargo::cargo_bin_cmd!("loom")
            .env("LOOMTERM_STATE_DIR", temp.path().join("state"))
            .env("LOOMTERM_RUNTIME_DIR", temp.path().join("run"))
            .env("LOOMTERM_CONFIG", temp.path().join("config.toml"))
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "`loom ui` requires an interactive terminal",
            ));
        assert!(!temp.path().join("state").exists());
        assert!(!temp.path().join("run").exists());
    }
}

#[test]
fn strict_agent_hook_denies_native_bash_without_contacting_the_daemon() {
    let input = serde_json::json!({
        "session_id": "provider-session",
        "hook_event_name": "PreToolUse",
        "tool_use_id": "tool-1",
        "tool_name": "Bash",
        "tool_input": {"command": "cargo test"},
        "cwd": "/tmp"
    });
    assert_cmd::cargo::cargo_bin_cmd!("loom")
        .env("LOOMTERM_SHELL_ROUTING", "strict")
        .args(["agent-event", "--provider", "codex"])
        .write_stdin(serde_json::to_vec(&input).unwrap())
        .assert()
        .success()
        .stdout(predicate::str::contains("\"permissionDecision\":\"deny\""))
        .stdout(predicate::str::contains("loom_run"));
}
