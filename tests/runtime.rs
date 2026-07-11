use std::collections::BTreeMap;
use std::time::Duration;

use loomterm::client::DaemonClient;
use loomterm::config::{AppPaths, Settings};
use loomterm::executor::ExecutionEngine;
use loomterm::model::{
    CommandSpec, ExecutionEventPayload, ExecutionOutcome, ExecutionRequest, ExecutionState,
    Initiator, OutputStream,
};
use loomterm::store::Store;
use tempfile::TempDir;

fn settings() -> Settings {
    Settings {
        max_concurrent_executions: 2,
        capture_limit_bytes: 1024 * 1024,
        retention_days: 7,
        retention_bytes: 1024 * 1024,
        cancel_grace_ms: 25,
        shell: "/bin/sh".into(),
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
    let workspace = store.add_workspace("test", temp.path()).unwrap();
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
    let output = store.read_output(&execution.id, 0, 1024).unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    for event in output.events {
        if let ExecutionEventPayload::Output { stream, text, .. } = event.payload {
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
    let workspace = store.add_workspace("test", &root).unwrap();
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
async fn cancels_the_running_process_group() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("loom.db")).unwrap();
    let workspace = store.add_workspace("test", temp.path()).unwrap();
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
        let current = engine.store().get_execution(&execution.id).unwrap();
        if current.state == ExecutionState::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    engine.cancel(&execution.id).await.unwrap();
    let final_execution = tokio::time::timeout(
        Duration::from_secs(2),
        wait_terminal(&engine, &execution.id),
    )
    .await
    .unwrap();
    assert_eq!(final_execution.state, ExecutionState::Cancelled);
    assert!(matches!(
        final_execution.outcome,
        Some(ExecutionOutcome::Cancelled { .. })
    ));
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
        matches!(&event.payload, ExecutionEventPayload::Output { text, .. } if text == "reconnected")
    }));

    second_client.shutdown().await.unwrap();
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
    let workspace = store.add_workspace("test", temp.path()).unwrap();
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
    let workspace = store.add_workspace("test", temp.path()).unwrap();
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
        if engine.store().get_execution(&blocker.id).unwrap().state == ExecutionState::Running {
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
