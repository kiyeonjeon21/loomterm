use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use fs4::FileExt;
use loomterm::client::DaemonClient;
use loomterm::config::{AppPaths, Settings};
use loomterm::executor::ExecutionEngine;
use loomterm::model::{
    AgentSessionFinish, AgentSessionRequest, AgentSessionState, CommandSpec, ExecutionEventPayload,
    ExecutionOutcome, ExecutionRequest, ExecutionState, Initiator, OutputStream, new_id, now_ms,
};
use loomterm::store::Store;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use tempfile::TempDir;

fn settings() -> Settings {
    Settings {
        max_concurrent_executions: 2,
        capture_limit_bytes: 1024 * 1024,
        retention_days: 7,
        retention_bytes: 1024 * 1024,
        cancel_grace_ms: 25,
        shell: "/bin/sh".into(),
        supervisor_path: Some(std::path::PathBuf::from(env!(
            "CARGO_BIN_EXE_loom-supervisor"
        ))),
    }
}

fn request(workspace_id: String, command: CommandSpec) -> ExecutionRequest {
    ExecutionRequest {
        workspace_id,
        cwd: None,
        command,
        env: BTreeMap::new(),
        stdin_base64: None,
        initiator: Initiator {
            kind: "test".into(),
            name: Some("runtime-test".into()),
            session_id: None,
        },
        capture_limit_bytes: None,
    }
}

async fn wait_terminal(engine: &ExecutionEngine, id: &str) -> loomterm::model::Execution {
    let mut cursor = 0;
    loop {
        let response = engine
            .wait(id, cursor, Duration::from_secs(5), Some(1024 * 1024))
            .await
            .unwrap();
        cursor = response.next_seq;
        if response.execution.state.is_terminal() {
            return response.execution;
        }
    }
}

#[tokio::test]
async fn captures_streams_and_nonzero_exit_without_screen_scraping() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("loom.db")).unwrap();
    let workspace = store.add_workspace("test", temp.path()).await.unwrap();
    let engine = ExecutionEngine::new(store.clone(), settings());
    let execution = engine
        .execute(request(
            workspace.id,
            CommandSpec::Shell {
                command: "printf 'hello'; printf 'problem' >&2; exit 7".into(),
                shell: None,
            },
        ))
        .await
        .unwrap();

    let final_execution = wait_terminal(&engine, &execution.id).await;
    assert_eq!(
        final_execution.outcome,
        Some(ExecutionOutcome::Exited { code: 7 })
    );
    let output = store.read_output(&execution.id, 0, 1024).await.unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    for event in output.events {
        if let ExecutionEventPayload::Output {
            stream,
            data_base64,
        } = event.payload
        {
            let text = String::from_utf8(
                base64::engine::general_purpose::STANDARD
                    .decode(data_base64)
                    .unwrap(),
            )
            .unwrap();
            match stream {
                OutputStream::Stdout => stdout.push_str(&text),
                OutputStream::Stderr => stderr.push_str(&text),
            }
        }
    }
    assert_eq!(stdout, "hello");
    assert_eq!(stderr, "problem");
}

#[tokio::test]
async fn enforces_workspace_boundary_and_capture_limit() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("workspace");
    let outside = parent.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let store = Store::open(&parent.path().join("loom.db")).unwrap();
    let workspace = store.add_workspace("test", &root).await.unwrap();
    let engine = ExecutionEngine::new(store.clone(), settings());

    let mut escaped = request(
        workspace.id.clone(),
        CommandSpec::Argv {
            program: "/usr/bin/true".into(),
            args: vec![],
        },
    );
    escaped.cwd = Some(outside.to_string_lossy().into_owned());
    assert!(matches!(
        engine.execute(escaped).await,
        Err(loomterm::Error::OutsideWorkspace { .. })
    ));

    let mut limited = request(
        workspace.id,
        CommandSpec::Shell {
            command: "printf 1234567890".into(),
            shell: None,
        },
    );
    limited.capture_limit_bytes = Some(5);
    let execution = engine.execute(limited).await.unwrap();
    let final_execution = wait_terminal(&engine, &execution.id).await;
    assert_eq!(final_execution.captured_bytes, 5);
    assert!(final_execution.output_truncated);
    assert!(
        store
            .read_output(&execution.id, 0, 1024)
            .await
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(
                event.payload,
                ExecutionEventPayload::CaptureTruncated { .. }
            ))
    );
}

#[tokio::test]
async fn inactive_workspace_blocks_new_execution_and_can_be_reactivated() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("loom.db")).unwrap();
    let workspace = store.add_workspace("test", temp.path()).await.unwrap();
    let engine = ExecutionEngine::new(store.clone(), settings());
    let first = engine
        .execute(request(
            workspace.id.clone(),
            CommandSpec::Argv {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
            },
        ))
        .await
        .unwrap();
    let first = wait_terminal(&engine, &first.id).await;

    store.remove_workspace("test").await.unwrap();
    assert!(store.list_workspaces().await.unwrap().is_empty());
    assert!(matches!(
        engine
            .execute(request(
                workspace.id.clone(),
                CommandSpec::Argv {
                    program: "/usr/bin/true".into(),
                    args: Vec::new(),
                },
            ))
            .await,
        Err(loomterm::Error::WorkspaceNotFound(_))
    ));
    assert_eq!(store.get_execution(&first.id).await.unwrap().id, first.id);

    let reactivated = store.add_workspace("test", temp.path()).await.unwrap();
    assert_eq!(reactivated.id, workspace.id);
    let second = engine
        .execute(request(
            reactivated.id,
            CommandSpec::Argv {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        wait_terminal(&engine, &second.id).await.outcome,
        Some(ExecutionOutcome::Exited { code: 0 })
    );
}

#[tokio::test]
async fn cancels_the_running_process_group() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("loom.db")).unwrap();
    let workspace = store.add_workspace("test", temp.path()).await.unwrap();
    let engine = ExecutionEngine::new(store, settings());
    let execution = engine
        .execute(request(
            workspace.id,
            CommandSpec::Shell {
                command: "sleep 30 & wait".into(),
                shell: None,
            },
        ))
        .await
        .unwrap();

    loop {
        let current = engine.store().get_execution(&execution.id).await.unwrap();
        if current.state == ExecutionState::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let final_execution = engine.cancel(&execution.id).await.unwrap();
    assert_eq!(final_execution.state, ExecutionState::Cancelled);
    assert!(matches!(
        final_execution.outcome,
        Some(ExecutionOutcome::Cancelled { .. })
    ));
}

#[tokio::test]
async fn cancel_waits_for_sigkill_escalation() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("loom.db")).unwrap();
    let workspace = store.add_workspace("test", temp.path()).await.unwrap();
    let mut settings = settings();
    settings.cancel_grace_ms = 50;
    let engine = ExecutionEngine::new(store, settings);
    let execution = engine
        .execute(request(
            workspace.id,
            CommandSpec::Shell {
                command: "trap '' TERM; while :; do sleep 1; done".into(),
                shell: None,
            },
        ))
        .await
        .unwrap();
    loop {
        if engine
            .store()
            .get_execution(&execution.id)
            .await
            .unwrap()
            .state
            == ExecutionState::Running
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;

    let cancelled = engine.cancel(&execution.id).await.unwrap();
    assert_eq!(cancelled.state, ExecutionState::Cancelled);
    assert_eq!(
        cancelled.outcome,
        Some(ExecutionOutcome::Cancelled { signal: Some(9) })
    );
}

#[tokio::test]
async fn daemon_keeps_execution_across_client_connections() {
    let temp = TempDir::new().unwrap();
    let paths = AppPaths {
        state_dir: temp.path().join("state"),
        runtime_dir: temp.path().join("run"),
        config_file: temp.path().join("config.toml"),
        database: temp.path().join("state/loom.db"),
        socket: temp.path().join("run/loomd.sock"),
        lock_file: temp.path().join("run/loomd.lock"),
        sessions_dir: temp.path().join("state/sessions"),
    };
    let daemon_paths = paths.clone();
    let daemon = tokio::spawn(async move { loomterm::daemon::run(daemon_paths, settings()).await });
    for _ in 0..100 {
        if paths.socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let first_client = DaemonClient::new(&paths.socket);
    let workspace = first_client
        .add_workspace("test".into(), temp.path().to_string_lossy().into_owned())
        .await
        .unwrap();
    let execution = first_client
        .execute(request(
            workspace.id,
            CommandSpec::Shell {
                command: "sleep 0.1; printf reconnected".into(),
                shell: None,
            },
        ))
        .await
        .unwrap();
    drop(first_client);

    let second_client = DaemonClient::new(&paths.socket);
    let mut cursor = 0;
    let final_execution = loop {
        let response = second_client
            .wait(execution.id.clone(), cursor, 2_000, 1024)
            .await
            .unwrap();
        cursor = response.next_seq;
        if response.execution.state.is_terminal() {
            break response.execution;
        }
    };
    assert_eq!(
        final_execution.outcome,
        Some(ExecutionOutcome::Exited { code: 0 })
    );
    let output = second_client
        .read_output(execution.id, 0, 1024)
        .await
        .unwrap();
    assert!(output.events.iter().any(|event| {
        let ExecutionEventPayload::Output { data_base64, .. } = &event.payload else {
            return false;
        };
        base64::engine::general_purpose::STANDARD
            .decode(data_base64)
            .is_ok_and(|data| data == b"reconnected")
    }));

    second_client.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn daemon_status_and_stop_do_not_autostart() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);

    for action in ["status", "stop"] {
        let output = loom_command(&paths)
            .args(["daemon", action])
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("daemon is unavailable"),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!paths.socket.exists());
    }
}

#[tokio::test]
async fn daemon_restart_requires_force_for_active_executions() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let mut original_daemon = spawn_daemon_process(&paths);
    let client = wait_for_daemon(&paths).await;
    let original_pid = client.health().await.unwrap().daemon_pid;
    let workspace = client
        .add_workspace("test".into(), temp.path().to_string_lossy().into_owned())
        .await
        .unwrap();
    let execution = client
        .execute(request(
            workspace.id,
            CommandSpec::Argv {
                program: "/bin/sleep".into(),
                args: vec!["30".into()],
            },
        ))
        .await
        .unwrap();
    wait_running(&client, &execution.id).await;

    let refused = loom_command(&paths)
        .args(["daemon", "restart"])
        .output()
        .await
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("active execution"));
    assert_eq!(client.health().await.unwrap().daemon_pid, original_pid);

    let restarted = loom_command(&paths)
        .args(["daemon", "restart", "--force", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        restarted.status.success(),
        "restart failed: {}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    original_daemon.wait().await.unwrap();

    let restarted_client = wait_for_daemon(&paths).await;
    let health = restarted_client.health().await.unwrap();
    assert_ne!(health.daemon_pid, original_pid);
    assert_eq!(health.active_executions, Some(0));
    assert!(
        health
            .capabilities
            .iter()
            .any(|value| value == loomterm::protocol::CAPABILITY_EXECUTION_STATS)
    );
    assert_eq!(
        restarted_client.get(execution.id).await.unwrap().state,
        ExecutionState::Cancelled
    );
    restarted_client.shutdown().await.unwrap();
}

#[tokio::test]
async fn daemon_reports_and_guards_active_agent_sessions() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    paths.ensure().unwrap();
    let daemon_paths = paths.clone();
    let daemon = tokio::spawn(async move { loomterm::daemon::run(daemon_paths, settings()).await });
    let client = wait_for_daemon(&paths).await;
    let workspace = client
        .add_workspace(
            "sessions".into(),
            temp.path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        )
        .await
        .unwrap();
    let session_id = new_id();
    let directory = paths.sessions_dir.join(&session_id);
    std::fs::create_dir(&directory).unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("active.lock"))
        .unwrap();
    FileExt::lock(&lock).unwrap();
    let cast_path = directory.join("recording.cast");
    std::fs::write(
        &cast_path,
        "{\"version\":3,\"term\":{\"cols\":80,\"rows\":24}}\n",
    )
    .unwrap();
    let session = client
        .create_agent_session(AgentSessionRequest {
            id: session_id.clone(),
            workspace_id: workspace.id,
            agent_kind: "generic".into(),
            name: None,
            command: CommandSpec::Argv {
                program: "sh".into(),
                args: Vec::new(),
            },
            cwd: temp.path().to_string_lossy().into_owned(),
            recorder_pid: std::process::id(),
            initial_cols: 80,
            initial_rows: 24,
            cast_path: cast_path.to_string_lossy().into_owned(),
            html_path: directory.join("replay.html").to_string_lossy().into_owned(),
        })
        .await
        .unwrap();
    assert_eq!(client.health().await.unwrap().active_sessions, Some(1));
    assert!(matches!(
        client.shutdown().await,
        Err(loomterm::Error::Protocol(_))
    ));
    drop(lock);
    assert_eq!(client.health().await.unwrap().active_sessions, Some(0));
    assert_eq!(
        client
            .get_agent_session(session.id)
            .await
            .unwrap()
            .session
            .state,
        AgentSessionState::Interrupted
    );
    client.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn daemon_reports_workspace_statistics() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let daemon_paths = paths.clone();
    let daemon = tokio::spawn(async move { loomterm::daemon::run(daemon_paths, settings()).await });
    let client = wait_for_daemon(&paths).await;
    let workspace = client
        .add_workspace("test".into(), temp.path().to_string_lossy().into_owned())
        .await
        .unwrap();
    let execution = client
        .execute(request(
            workspace.id.clone(),
            CommandSpec::Argv {
                program: "/usr/bin/printf".into(),
                args: vec!["stats".into()],
            },
        ))
        .await
        .unwrap();
    let mut cursor = 0;
    loop {
        let response = client
            .wait(execution.id.clone(), cursor, 2_000, 1024)
            .await
            .unwrap();
        cursor = response.next_seq;
        if response.execution.state.is_terminal() {
            break;
        }
    }

    let stats = client
        .stats(workspace.id.clone(), now_ms().saturating_sub(60_000))
        .await
        .unwrap();
    assert_eq!(stats.workspace.id, workspace.id);
    assert_eq!(stats.total, 1);
    assert_eq!(stats.status.exited_zero, 1);
    assert_eq!(stats.by_initiator[0].kind, "test");
    assert_eq!(stats.captured_bytes, 5);
    assert_eq!(stats.duration_samples, 1);

    client.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn live_observer_polling_reads_correlated_execution_without_gaps() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    paths.ensure().unwrap();
    let daemon_paths = paths.clone();
    let daemon = tokio::spawn(async move { loomterm::daemon::run(daemon_paths, settings()).await });
    let client = wait_for_daemon(&paths).await;
    let workspace = client
        .add_workspace(
            "observer".into(),
            temp.path().to_string_lossy().into_owned(),
        )
        .await
        .unwrap();
    let session_id = new_id();
    let directory = paths.sessions_dir.join(&session_id);
    std::fs::create_dir(&directory).unwrap();
    let cast_path = directory.join("recording.cast");
    std::fs::write(
        &cast_path,
        "{\"version\":3,\"term\":{\"cols\":120,\"rows\":30}}\n",
    )
    .unwrap();
    client
        .create_agent_session(AgentSessionRequest {
            id: session_id.clone(),
            workspace_id: workspace.id.clone(),
            agent_kind: "codex".into(),
            name: Some("observer integration".into()),
            command: CommandSpec::Argv {
                program: "codex".into(),
                args: vec!["exec".into()],
            },
            cwd: temp.path().to_string_lossy().into_owned(),
            recorder_pid: std::process::id(),
            initial_cols: 120,
            initial_rows: 30,
            cast_path: cast_path.to_string_lossy().into_owned(),
            html_path: directory.join("replay.html").to_string_lossy().into_owned(),
        })
        .await
        .unwrap();
    let mut execution_request = request(
        workspace.id,
        CommandSpec::Shell {
            command: "printf one; sleep 0.05; printf two >&2; sleep 0.05; printf three".into(),
            shell: None,
        },
    );
    execution_request.initiator.session_id = Some(session_id.clone());
    let execution = client.execute(execution_request).await.unwrap();

    let mut cursor = 0;
    let mut sequences = Vec::new();
    loop {
        let detail = client.get_agent_session(session_id.clone()).await.unwrap();
        assert_eq!(detail.executions.len(), 1);
        assert_eq!(detail.executions[0].id, execution.id);
        let page = client
            .read_output(execution.id.clone(), cursor, 4)
            .await
            .unwrap();
        sequences.extend(page.events.iter().map(|event| event.seq));
        cursor = page.next_seq;
        if page.execution.state.is_terminal() && !page.has_more {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(sequences.first(), Some(&1));
    assert_eq!(sequences.last(), Some(&cursor));
    assert!(sequences.windows(2).all(|pair| pair[1] == pair[0] + 1));

    client
        .finish_agent_session(
            session_id,
            AgentSessionFinish {
                state: AgentSessionState::Finished,
                outcome: ExecutionOutcome::Exited { code: 0 },
                captured_bytes: 0,
                output_truncated: false,
            },
        )
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn batches_concurrent_high_volume_output_without_loss() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("loom.db")).unwrap();
    let workspace = store.add_workspace("test", temp.path()).await.unwrap();
    let engine = ExecutionEngine::new(store, settings());
    let command = "i=0; while [ $i -lt 5000 ]; do printf 0123456789abcdef; i=$((i+1)); done";
    let first = engine
        .execute(request(
            workspace.id.clone(),
            CommandSpec::Shell {
                command: command.into(),
                shell: None,
            },
        ))
        .await
        .unwrap();
    let second = engine
        .execute(request(
            workspace.id,
            CommandSpec::Shell {
                command: command.into(),
                shell: None,
            },
        ))
        .await
        .unwrap();

    let (first, second) = tokio::join!(
        wait_terminal(&engine, &first.id),
        wait_terminal(&engine, &second.id)
    );
    assert_eq!(first.captured_bytes, 80_000);
    assert_eq!(second.captured_bytes, 80_000);
    assert_eq!(first.outcome, Some(ExecutionOutcome::Exited { code: 0 }));
    assert_eq!(second.outcome, Some(ExecutionOutcome::Exited { code: 0 }));
}

#[tokio::test]
async fn queued_execution_can_be_cancelled_before_spawn() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("loom.db")).unwrap();
    let workspace = store.add_workspace("test", temp.path()).await.unwrap();
    let mut single_slot = settings();
    single_slot.max_concurrent_executions = 1;
    let engine = ExecutionEngine::new(store, single_slot);
    let blocker = engine
        .execute(request(
            workspace.id.clone(),
            CommandSpec::Shell {
                command: "sleep 30".into(),
                shell: None,
            },
        ))
        .await
        .unwrap();
    loop {
        if engine
            .store()
            .get_execution(&blocker.id)
            .await
            .unwrap()
            .state
            == ExecutionState::Running
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let queued = engine
        .execute(request(
            workspace.id,
            CommandSpec::Argv {
                program: "/usr/bin/true".into(),
                args: vec![],
            },
        ))
        .await
        .unwrap();
    assert_eq!(queued.state, ExecutionState::Queued);
    let cancelled = engine.cancel(&queued.id).await.unwrap();
    assert_eq!(cancelled.state, ExecutionState::Cancelled);
    engine.cancel(&blocker.id).await.unwrap();
    let _ = wait_terminal(&engine, &blocker.id).await;
}

#[tokio::test]
async fn graceful_shutdown_cancels_queued_work_without_spawning_it() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let mut single_slot = settings();
    single_slot.max_concurrent_executions = 1;
    let daemon_paths = paths.clone();
    let daemon =
        tokio::spawn(async move { loomterm::daemon::run(daemon_paths, single_slot).await });
    let client = wait_for_daemon(&paths).await;
    let workspace = client
        .add_workspace("test".into(), temp.path().to_string_lossy().into_owned())
        .await
        .unwrap();
    let blocker = client
        .execute(request(
            workspace.id.clone(),
            CommandSpec::Argv {
                program: "/bin/sleep".into(),
                args: vec!["30".into()],
            },
        ))
        .await
        .unwrap();
    wait_running(&client, &blocker.id).await;

    let marker = temp.path().join("queued-command-ran");
    let queued = client
        .execute(request(
            workspace.id,
            CommandSpec::Argv {
                program: "/usr/bin/touch".into(),
                args: vec![marker.to_string_lossy().into_owned()],
            },
        ))
        .await
        .unwrap();
    assert_eq!(queued.state, ExecutionState::Queued);

    client.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!marker.exists());

    let store = Store::open(&paths.database).unwrap();
    assert_eq!(
        store.get_execution(&queued.id).await.unwrap().state,
        ExecutionState::Cancelled
    );
    assert_eq!(
        store.get_execution(&blocker.id).await.unwrap().state,
        ExecutionState::Cancelled
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn subscription_reconnect_replays_exactly_after_the_cursor() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let daemon_paths = paths.clone();
    let daemon = tokio::spawn(async move { loomterm::daemon::run(daemon_paths, settings()).await });
    let client = wait_for_daemon(&paths).await;
    let workspace = client
        .add_workspace("test".into(), temp.path().to_string_lossy().into_owned())
        .await
        .unwrap();
    let execution = client
        .execute(request(
            workspace.id,
            CommandSpec::Shell {
                command: "printf one; sleep 0.05; printf two; sleep 0.05; printf three".into(),
                shell: None,
            },
        ))
        .await
        .unwrap();

    let mut first = client.subscribe(execution.id.clone(), 0).await.unwrap();
    let first_event = tokio::time::timeout(Duration::from_secs(2), first.next_event())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut sequences = vec![first_event.seq];
    let cursor = first_event.seq;
    drop(first);

    let mut resumed = client
        .subscribe(execution.id.clone(), cursor)
        .await
        .unwrap();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(2), resumed.next_event())
        .await
        .unwrap()
        .unwrap()
    {
        sequences.push(event.seq);
    }
    let final_execution = client.get(execution.id).await.unwrap();
    assert!(final_execution.state.is_terminal());
    assert_eq!(sequences.first(), Some(&1));
    assert_eq!(sequences.last(), Some(&final_execution.last_seq));
    assert!(sequences.windows(2).all(|pair| pair[1] == pair[0] + 1));

    client.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn daemon_sigkill_causes_supervisor_to_terminate_the_command_group() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    std::fs::write(&paths.config_file, "cancel_grace_ms = 25\n").unwrap();
    let mut daemon = spawn_daemon_process(&paths);
    let client = wait_for_daemon(&paths).await;
    let workspace = client
        .add_workspace("test".into(), temp.path().to_string_lossy().into_owned())
        .await
        .unwrap();
    let execution = client
        .execute(request(
            workspace.id,
            CommandSpec::Shell {
                command: "sleep 30 & wait".into(),
                shell: None,
            },
        ))
        .await
        .unwrap();
    let running = wait_running(&client, &execution.id).await;
    let command_pid = running.pid.unwrap() as i32;

    daemon.start_kill().unwrap();
    daemon.wait().await.unwrap();
    let gone = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match kill(Pid::from_raw(command_pid), None) {
                Err(nix::errno::Errno::ESRCH) => break,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await;
    assert!(gone.is_ok(), "command process survived the daemon crash");

    let mut restarted = spawn_daemon_process(&paths);
    let restarted_client = wait_for_daemon(&paths).await;
    assert_eq!(
        restarted_client.get(execution.id).await.unwrap().state,
        ExecutionState::Interrupted
    );
    restarted_client.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), restarted.wait())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn daemon_queries_remain_responsive_during_high_volume_capture() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let daemon_paths = paths.clone();
    let daemon = tokio::spawn(async move { loomterm::daemon::run(daemon_paths, settings()).await });
    let client = wait_for_daemon(&paths).await;
    let workspace = client
        .add_workspace("test".into(), temp.path().to_string_lossy().into_owned())
        .await
        .unwrap();
    let execution = client
        .execute(request(
            workspace.id.clone(),
            CommandSpec::Shell {
                command:
                    "i=0; while [ $i -lt 100000 ]; do printf 0123456789abcdef; i=$((i+1)); done"
                        .into(),
                shell: None,
            },
        ))
        .await
        .unwrap();

    for _ in 0..20 {
        let listed = tokio::time::timeout(
            Duration::from_secs(1),
            client.list(Some(workspace.id.clone()), 10),
        )
        .await
        .expect("storage query blocked the async daemon")
        .unwrap();
        assert!(listed.iter().any(|item| item.id == execution.id));
    }
    let mut cursor = 0;
    loop {
        let response = client
            .wait(execution.id.clone(), cursor, 5_000, 1024 * 1024)
            .await
            .unwrap();
        cursor = response.next_seq;
        if response.execution.state.is_terminal() {
            break;
        }
    }
    client.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

fn test_paths(temp: &TempDir) -> AppPaths {
    AppPaths {
        state_dir: temp.path().join("state"),
        runtime_dir: temp.path().join("run"),
        config_file: temp.path().join("config/config.toml"),
        database: temp.path().join("state/loom.db"),
        socket: temp.path().join("run/loomd.sock"),
        lock_file: temp.path().join("run/loomd.lock"),
        sessions_dir: temp.path().join("state/sessions"),
    }
}

async fn wait_for_daemon(paths: &AppPaths) -> DaemonClient {
    let client = DaemonClient::new(&paths.socket);
    for _ in 0..300 {
        if client.health().await.is_ok() {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon did not become ready at {}", paths.socket.display());
}

async fn wait_running(client: &DaemonClient, id: &str) -> loomterm::model::Execution {
    for _ in 0..300 {
        let execution = client.get(id.to_owned()).await.unwrap();
        if execution.state == ExecutionState::Running {
            return execution;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("execution did not enter running state: {id}");
}

fn spawn_daemon_process(paths: &AppPaths) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_loomd"));
    command
        .env("LOOMTERM_STATE_DIR", &paths.state_dir)
        .env("LOOMTERM_RUNTIME_DIR", &paths.runtime_dir)
        .env("LOOMTERM_CONFIG", &paths.config_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command.spawn().unwrap()
}

fn loom_command(paths: &AppPaths) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_loom"));
    command
        .env("LOOMTERM_STATE_DIR", &paths.state_dir)
        .env("LOOMTERM_RUNTIME_DIR", &paths.runtime_dir)
        .env("LOOMTERM_CONFIG", &paths.config_file)
        .stdin(Stdio::null());
    command
}
