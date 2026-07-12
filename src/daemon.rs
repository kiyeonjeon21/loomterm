use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Write};
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use fs4::{FileExt, TryLockError};
use futures::{SinkExt, StreamExt};
use nix::unistd::Pid;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::config::{AppPaths, Settings};
use crate::executor::ExecutionEngine;
use crate::model::{
    AgentSessionFinish, AgentSessionState, ExecutionEventPayload, ExecutionOutcome, Health,
    PROTOCOL_VERSION, now_ms,
};
use crate::protocol::{
    CAPABILITY_AGENT_SESSIONS, CAPABILITY_EXECUTION_STATS, MAX_FRAME_BYTES, Operation,
    ProtocolResult, SubscriptionResponse, WireMessage,
};
use crate::store::Store;
use crate::{Error, Result};

pub async fn run(paths: AppPaths, settings: Settings) -> Result<()> {
    paths.ensure()?;
    let _lock = DaemonLock::acquire(&paths.lock_file)?;
    prepare_socket(&paths.socket).await?;
    let listener = UnixListener::bind(&paths.socket)?;
    std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))?;

    let store = Store::open(&paths.database)?;
    let interrupted = store.reconcile_incomplete().await?;
    if interrupted > 0 {
        tracing::warn!(interrupted, "reconciled incomplete executions");
    }
    let interrupted_sessions = reconcile_agent_sessions(&store).await?;
    if interrupted_sessions > 0 {
        tracing::warn!(interrupted_sessions, "reconciled incomplete agent sessions");
    }
    let engine = ExecutionEngine::new(store, settings.clone());
    let mut fatal_rx = engine.subscribe_fatal();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    tracing::info!(socket = %paths.socket.display(), "loomd listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if let Err(error) = verify_peer(&stream) {
                    tracing::warn!(%error, "rejected daemon client");
                    continue;
                }
                let engine = engine.clone();
                let paths = paths.clone();
                let shutdown = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, engine, paths, shutdown).await {
                        tracing::warn!(%error, "daemon client connection failed");
                    }
                });
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break;
            }
            changed = fatal_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                if let Some(error) = fatal_rx.borrow().clone() {
                    tracing::error!(%error, "fatal execution storage failure");
                    break;
                }
            }
        }
    }

    tracing::info!("loomd shutting down");
    drop(listener);
    let drain_result = engine.drain().await;
    let storage_result = engine.store().shutdown().await;
    let _ = std::fs::remove_file(&paths.socket);
    drain_result?;
    storage_result?;
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    engine: ExecutionEngine,
    paths: AppPaths,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let codec = LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec();
    let mut framed = Framed::new(stream, codec);
    while let Some(frame) = framed.next().await {
        let frame = frame?;
        let message: WireMessage = match serde_json::from_slice(&frame) {
            Ok(request) => request,
            Err(error) => {
                let response = WireMessage::error(
                    "invalid".into(),
                    &Error::InvalidRequest(format!("invalid protocol request: {error}")),
                );
                framed
                    .send(Bytes::from(serde_json::to_vec(&response)?))
                    .await?;
                continue;
            }
        };
        let WireMessage::Request {
            version,
            request_id,
            operation,
        } = message
        else {
            let response = WireMessage::error(
                "invalid".into(),
                &Error::InvalidRequest("client message must be a request".into()),
            );
            framed
                .send(Bytes::from(serde_json::to_vec(&response)?))
                .await?;
            continue;
        };
        if version != PROTOCOL_VERSION {
            let response = WireMessage::error(
                request_id,
                &Error::Protocol(format!("unsupported protocol version {version}")),
            );
            framed
                .send(Bytes::from(serde_json::to_vec(&response)?))
                .await?;
            continue;
        }
        if let Operation::Subscribe {
            execution_id,
            after_seq,
        } = operation
        {
            return handle_subscription(&mut framed, &engine, request_id, execution_id, after_seq)
                .await;
        }
        let result = dispatch(operation, &engine, &paths, &shutdown).await;
        let response = match result {
            Ok(result) => WireMessage::ok(request_id, result),
            Err(error) => WireMessage::error(request_id, &error),
        };
        framed
            .send(Bytes::from(serde_json::to_vec(&response)?))
            .await?;
    }
    Ok(())
}

async fn handle_subscription(
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
    engine: &ExecutionEngine,
    subscription_id: String,
    execution_id: String,
    after_seq: u64,
) -> Result<()> {
    let mut receiver = engine.subscribe_events();
    let execution = match engine.store().get_execution(&execution_id).await {
        Ok(execution) => execution,
        Err(error) => {
            let response = WireMessage::error(subscription_id, &error);
            framed
                .send(Bytes::from(serde_json::to_vec(&response)?))
                .await?;
            return Ok(());
        }
    };
    if after_seq > execution.last_seq {
        let response = WireMessage::error(
            subscription_id,
            &Error::InvalidRequest(format!(
                "after_seq {after_seq} exceeds execution cursor {}",
                execution.last_seq
            )),
        );
        framed
            .send(Bytes::from(serde_json::to_vec(&response)?))
            .await?;
        return Ok(());
    }
    let response = WireMessage::ok(
        subscription_id.clone(),
        ProtocolResult::Subscription(SubscriptionResponse {
            execution,
            next_seq: after_seq,
        }),
    );
    framed
        .send(Bytes::from(serde_json::to_vec(&response)?))
        .await?;

    let mut cursor = after_seq;
    if replay_subscription(framed, engine, &execution_id, &subscription_id, &mut cursor).await? {
        return Ok(());
    }

    loop {
        tokio::select! {
            incoming = framed.next() => {
                match incoming {
                    None | Some(Err(_)) => return Ok(()),
                    Some(Ok(_)) => {
                        return Err(Error::Protocol(
                            "subscription connections do not accept additional requests".into(),
                        ));
                    }
                }
            }
            received = receiver.recv() => {
                match received {
                    Ok(event) if event.execution_id != execution_id || event.seq <= cursor => {}
                    Ok(event) if event.seq == cursor + 1 => {
                        let terminal = matches!(&event.payload, ExecutionEventPayload::Finished { .. });
                        send_subscription_event(framed, &subscription_id, event).await?;
                        cursor += 1;
                        if terminal {
                            return Ok(());
                        }
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if replay_subscription(
                            framed,
                            engine,
                            &execution_id,
                            &subscription_id,
                            &mut cursor,
                        )
                        .await?
                        {
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = replay_subscription(
                            framed,
                            engine,
                            &execution_id,
                            &subscription_id,
                            &mut cursor,
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn replay_subscription(
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
    engine: &ExecutionEngine,
    execution_id: &str,
    subscription_id: &str,
    cursor: &mut u64,
) -> Result<bool> {
    loop {
        let page = engine
            .store()
            .read_output(execution_id, *cursor, 1024 * 1024)
            .await?;
        for event in page.events {
            if event.seq <= *cursor {
                continue;
            }
            let terminal = matches!(&event.payload, ExecutionEventPayload::Finished { .. });
            *cursor = event.seq;
            send_subscription_event(framed, subscription_id, event).await?;
            if terminal {
                return Ok(true);
            }
        }
        if page.has_more {
            continue;
        }
        return Ok(page.execution.state.is_terminal() && *cursor >= page.execution.last_seq);
    }
}

async fn send_subscription_event(
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
    subscription_id: &str,
    event: crate::model::ExecutionEvent,
) -> Result<()> {
    let message = WireMessage::event(subscription_id.to_owned(), event);
    framed
        .send(Bytes::from(serde_json::to_vec(&message)?))
        .await?;
    Ok(())
}

async fn dispatch(
    operation: Operation,
    engine: &ExecutionEngine,
    paths: &AppPaths,
    shutdown: &watch::Sender<bool>,
) -> Result<ProtocolResult> {
    let store = engine.store();
    match operation {
        Operation::Health => {
            reconcile_agent_sessions(store).await?;
            Ok(ProtocolResult::Health(Health {
                protocol_version: PROTOCOL_VERSION,
                daemon_pid: std::process::id(),
                database_path: paths.database.to_string_lossy().into_owned(),
                socket_path: paths.socket.to_string_lossy().into_owned(),
                server_version: Some(env!("CARGO_PKG_VERSION").into()),
                capabilities: vec![
                    CAPABILITY_EXECUTION_STATS.into(),
                    CAPABILITY_AGENT_SESSIONS.into(),
                ],
                active_executions: Some(engine.active_executions()),
                active_sessions: Some(store.active_agent_sessions().await?.len() as u64),
            }))
        }
        Operation::WorkspaceAdd { name, root } => Ok(ProtocolResult::Workspace(
            store.add_workspace(&name, Path::new(&root)).await?,
        )),
        Operation::WorkspaceRemove { workspace } => {
            store.remove_workspace(&workspace).await?;
            Ok(ProtocolResult::Empty)
        }
        Operation::WorkspaceList => Ok(ProtocolResult::Workspaces(store.list_workspaces().await?)),
        Operation::Execute { request } => {
            Ok(ProtocolResult::Execution(engine.execute(request).await?))
        }
        Operation::Get { execution_id } => Ok(ProtocolResult::Execution(
            store.get_execution(&execution_id).await?,
        )),
        Operation::List { workspace, limit } => Ok(ProtocolResult::Executions(
            store
                .list_executions(workspace.as_deref(), limit.clamp(1, 1000))
                .await?,
        )),
        Operation::Stats {
            workspace,
            since_ms,
        } => Ok(ProtocolResult::Stats(
            store
                .execution_stats(&workspace, since_ms, now_ms())
                .await?,
        )),
        Operation::SessionCreate { request } => {
            validate_session_paths(paths, &request.id, &request.cast_path, &request.html_path)?;
            Ok(ProtocolResult::AgentSession(
                store.create_agent_session(&request).await?,
            ))
        }
        Operation::SessionFinish { session_id, finish } => Ok(ProtocolResult::AgentSession(
            store.finish_agent_session(&session_id, finish).await?,
        )),
        Operation::SessionGet { session_id } => {
            reconcile_agent_sessions(store).await?;
            Ok(ProtocolResult::AgentSessionDetail(
                store.get_agent_session(&session_id).await?,
            ))
        }
        Operation::SessionList { workspace, limit } => {
            reconcile_agent_sessions(store).await?;
            Ok(ProtocolResult::AgentSessions(
                store
                    .list_agent_sessions(workspace.as_deref(), limit.clamp(1, 1000))
                    .await?,
            ))
        }
        Operation::SessionDelete { session_id } => {
            let detail = store.get_agent_session(&session_id).await?;
            if !detail.session.state.is_terminal() {
                return Err(Error::InvalidRequest(format!(
                    "agent session {session_id} is still recording"
                )));
            }
            if let Some(directory) = Path::new(&detail.session.cast_path).parent()
                && directory.starts_with(&paths.sessions_dir)
                && directory.exists()
            {
                std::fs::remove_dir_all(directory)?;
            }
            store.delete_agent_session(&session_id).await?;
            Ok(ProtocolResult::Empty)
        }
        Operation::ReadOutput {
            execution_id,
            after_seq,
            max_bytes,
        } => Ok(ProtocolResult::Output(
            store
                .read_output(
                    &execution_id,
                    after_seq,
                    max_bytes.clamp(1, 8 * 1024 * 1024),
                )
                .await?,
        )),
        Operation::Wait {
            execution_id,
            after_seq,
            timeout_ms,
            max_bytes,
        } => Ok(ProtocolResult::Wait(
            engine
                .wait(
                    &execution_id,
                    after_seq,
                    Duration::from_millis(timeout_ms.clamp(1, 60_000)),
                    Some(max_bytes.clamp(1, 8 * 1024 * 1024)),
                )
                .await?,
        )),
        Operation::Subscribe { .. } => Err(Error::Protocol(
            "subscribe must use a dedicated connection".into(),
        )),
        Operation::Cancel { execution_id } => Ok(ProtocolResult::Execution(
            engine.cancel(&execution_id).await?,
        )),
        Operation::Shutdown { force } => {
            reconcile_agent_sessions(store).await?;
            let active_sessions = store.active_agent_sessions().await?.len();
            if !force && active_sessions > 0 {
                return Err(Error::InvalidRequest(format!(
                    "daemon has {active_sessions} active agent session(s); wait for them or use --force"
                )));
            }
            let _ = shutdown.send(true);
            Ok(ProtocolResult::Empty)
        }
    }
}

fn validate_session_paths(
    paths: &AppPaths,
    session_id: &str,
    cast_path: &str,
    html_path: &str,
) -> Result<()> {
    let cast = Path::new(cast_path);
    let html = Path::new(html_path);
    let Some(directory) = cast.parent() else {
        return Err(Error::InvalidRequest("cast_path has no parent".into()));
    };
    if directory == paths.sessions_dir
        || !directory.starts_with(&paths.sessions_dir)
        || html.parent() != Some(directory)
        || directory.file_name().and_then(|value| value.to_str()) != Some(session_id)
    {
        return Err(Error::InvalidRequest(
            "session artifacts must share a private directory under the Loomterm state directory"
                .into(),
        ));
    }
    Ok(())
}

async fn reconcile_agent_sessions(store: &Store) -> Result<usize> {
    let mut interrupted = 0;
    for session in store.active_agent_sessions().await? {
        let Some(directory) = Path::new(&session.cast_path).parent() else {
            continue;
        };
        let lock_path = directory.join("active.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        match FileExt::try_lock(&lock) {
            Ok(()) => {
                let captured_bytes = cast_output_bytes(Path::new(&session.cast_path))
                    .unwrap_or(session.captured_bytes);
                store
                    .finish_agent_session(
                        &session.id,
                        AgentSessionFinish {
                            state: AgentSessionState::Interrupted,
                            outcome: ExecutionOutcome::Interrupted {
                                reason: "session recorder exited before finalization".into(),
                            },
                            captured_bytes,
                            output_truncated: session.output_truncated,
                        },
                    )
                    .await?;
                interrupted += 1;
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(Error::Io(error)),
        }
    }
    Ok(interrupted)
}

fn cast_output_bytes(path: &Path) -> Option<u64> {
    let file = File::open(path).ok()?;
    let mut total = 0_u64;
    for line in std::io::BufReader::new(file).lines().skip(1) {
        let value: serde_json::Value = serde_json::from_str(&line.ok()?).ok()?;
        let event = value.as_array()?;
        if event.get(1).and_then(serde_json::Value::as_str) == Some("o") {
            total = total.saturating_add(
                event
                    .get(2)
                    .and_then(serde_json::Value::as_str)
                    .map_or(0, |text| text.len() as u64),
            );
        }
    }
    Some(total)
}

async fn prepare_socket(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if UnixStream::connect(path).await.is_ok() {
        return Err(Error::Config(format!(
            "another loomd is already listening at {}",
            path.display()
        )));
    }
    std::fs::remove_file(path)?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn verify_peer(stream: &UnixStream) -> Result<()> {
    let (uid, _) = nix::unistd::getpeereid(stream.as_fd())
        .map_err(|error| Error::Io(std::io::Error::from_raw_os_error(error as i32)))?;
    if uid != nix::unistd::Uid::effective() {
        return Err(Error::PermissionDenied(format!(
            "peer uid {} does not match daemon uid {}",
            uid.as_raw(),
            nix::unistd::Uid::effective().as_raw()
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_peer(stream: &UnixStream) -> Result<()> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};

    let credentials = getsockopt(stream, PeerCredentials)
        .map_err(|error| Error::Io(std::io::Error::from_raw_os_error(error as i32)))?;
    if credentials.uid() != nix::unistd::Uid::effective().as_raw() {
        return Err(Error::PermissionDenied(format!(
            "peer uid {} does not match daemon uid {}",
            credentials.uid(),
            nix::unistd::Uid::effective().as_raw()
        )));
    }
    Ok(())
}

struct DaemonLock {
    path: std::path::PathBuf,
    _file: File,
}

impl DaemonLock {
    fn acquire(path: &Path) -> Result<Self> {
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                        _file: file,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_owner_alive(path)? {
                        return Err(Error::Config("another loomd process is running".into()));
                    }
                    std::fs::remove_file(path)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(Error::Config("could not acquire daemon lock".into()))
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock_owner_alive(path: &Path) -> Result<bool> {
    let mut source = String::new();
    File::open(path)?.read_to_string(&mut source)?;
    let Ok(pid) = source.trim().parse::<i32>() else {
        return Ok(false);
    };
    match nix::sys::signal::kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(Error::Io(std::io::Error::from_raw_os_error(error as i32))),
    }
}
