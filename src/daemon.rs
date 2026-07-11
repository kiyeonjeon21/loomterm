use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use nix::unistd::Pid;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::config::{AppPaths, Settings};
use crate::executor::ExecutionEngine;
use crate::model::{Health, PROTOCOL_VERSION};
use crate::protocol::{
    MAX_FRAME_BYTES, Operation, ProtocolRequest, ProtocolResponse, ProtocolResult,
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
    let interrupted = store.reconcile_incomplete()?;
    if interrupted > 0 {
        tracing::warn!(interrupted, "reconciled incomplete executions");
    }
    let engine = ExecutionEngine::new(store, settings.clone());
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
        }
    }

    tracing::info!("loomd shutting down");
    engine.cancel_all().await;
    tokio::time::sleep(Duration::from_millis(settings.cancel_grace_ms + 100)).await;
    let _ = std::fs::remove_file(&paths.socket);
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
        let request: ProtocolRequest = match serde_json::from_slice(&frame) {
            Ok(request) => request,
            Err(error) => {
                let response = ProtocolResponse::error(
                    "invalid".into(),
                    &Error::InvalidRequest(format!("invalid protocol request: {error}")),
                );
                framed
                    .send(Bytes::from(serde_json::to_vec(&response)?))
                    .await?;
                continue;
            }
        };
        if request.version != PROTOCOL_VERSION {
            let response = ProtocolResponse::error(
                request.request_id,
                &Error::Protocol(format!("unsupported protocol version {}", request.version)),
            );
            framed
                .send(Bytes::from(serde_json::to_vec(&response)?))
                .await?;
            continue;
        }
        let request_id = request.request_id;
        let result = dispatch(request.operation, &engine, &paths, &shutdown).await;
        let response = match result {
            Ok(result) => ProtocolResponse::ok(request_id, result),
            Err(error) => ProtocolResponse::error(request_id, &error),
        };
        framed
            .send(Bytes::from(serde_json::to_vec(&response)?))
            .await?;
    }
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
        Operation::Health => Ok(ProtocolResult::Health(Health {
            protocol_version: PROTOCOL_VERSION,
            daemon_pid: std::process::id(),
            database_path: paths.database.to_string_lossy().into_owned(),
            socket_path: paths.socket.to_string_lossy().into_owned(),
        })),
        Operation::WorkspaceAdd { name, root } => Ok(ProtocolResult::Workspace(
            store.add_workspace(&name, Path::new(&root))?,
        )),
        Operation::WorkspaceRemove { workspace } => {
            store.remove_workspace(&workspace)?;
            Ok(ProtocolResult::Empty)
        }
        Operation::WorkspaceList => Ok(ProtocolResult::Workspaces(store.list_workspaces()?)),
        Operation::Execute { request } => {
            Ok(ProtocolResult::Execution(engine.execute(request).await?))
        }
        Operation::Get { execution_id } => Ok(ProtocolResult::Execution(
            store.get_execution(&execution_id)?,
        )),
        Operation::List { workspace, limit } => Ok(ProtocolResult::Executions(
            store.list_executions(workspace.as_deref(), limit.clamp(1, 1000))?,
        )),
        Operation::ReadOutput {
            execution_id,
            after_seq,
            max_bytes,
        } => Ok(ProtocolResult::Output(store.read_output(
            &execution_id,
            after_seq,
            max_bytes.clamp(1, 8 * 1024 * 1024),
        )?)),
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
        Operation::Cancel { execution_id } => Ok(ProtocolResult::Execution(
            engine.cancel(&execution_id).await?,
        )),
        Operation::Shutdown => {
            let _ = shutdown.send(true);
            Ok(ProtocolResult::Empty)
        }
    }
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
