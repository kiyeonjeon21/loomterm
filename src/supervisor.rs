use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::model::{CommandSpec, ExecutionOutcome, OutputStream};
use crate::protocol::MAX_FRAME_BYTES;
use crate::{Error, Result};

const OUTPUT_CHUNK_BYTES: usize = 16 * 1024;
const EVENT_QUEUE_DEPTH: usize = 256;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SupervisorCommand {
    Spawn {
        command: CommandSpec,
        cwd: String,
        env: BTreeMap<String, String>,
        stdin_base64: Option<String>,
        shell: String,
        cancel_grace_ms: u64,
    },
    Cancel,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SupervisorEvent {
    Started {
        pid: u32,
        pgid: i32,
    },
    Output {
        stream: OutputStream,
        data_base64: String,
    },
    Finished {
        outcome: ExecutionOutcome,
    },
}

enum InternalEvent {
    Cancel,
    DaemonGone,
    CaptureFailed(String),
}

enum Outgoing {
    Event(SupervisorEvent),
    Flush(oneshot::Sender<()>),
}

pub async fn run() -> Result<()> {
    let codec = LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec();
    let mut input = FramedRead::new(tokio::io::stdin(), codec);
    let first = input
        .next()
        .await
        .ok_or_else(|| Error::Protocol("supervisor received no spawn request".into()))??;
    let request: SupervisorCommand = serde_json::from_slice(&first)?;
    let SupervisorCommand::Spawn {
        command,
        cwd,
        env,
        stdin_base64,
        shell,
        cancel_grace_ms,
    } = request
    else {
        return Err(Error::Protocol(
            "the first supervisor command must be spawn".into(),
        ));
    };

    let (internal_tx, mut internal_rx) = mpsc::channel(16);
    tokio::spawn(read_controls(input, internal_tx.clone()));

    let (outgoing_tx, outgoing_rx) = mpsc::channel(EVENT_QUEUE_DEPTH);
    tokio::spawn(write_events(outgoing_rx, internal_tx.clone()));

    let stdin = stdin_base64
        .map(|value| {
            base64::engine::general_purpose::STANDARD
                .decode(value)
                .map_err(|error| Error::InvalidRequest(format!("invalid stdin_base64: {error}")))
        })
        .transpose()?;
    let mut process = build_command(&command, &shell);
    process
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        process.env(key, value);
    }
    process.as_std_mut().process_group(0);

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            send_and_flush(
                &outgoing_tx,
                SupervisorEvent::Finished {
                    outcome: ExecutionOutcome::SpawnError {
                        message: error.to_string(),
                    },
                },
            )
            .await?;
            return Ok(());
        }
    };
    let pid = child
        .id()
        .ok_or_else(|| Error::Protocol("spawned command has no pid".into()))?;
    let pgid = pid as i32;
    send_event(&outgoing_tx, SupervisorEvent::Started { pid, pgid }).await?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Protocol("command stdout was not piped".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Protocol("command stderr was not piped".into()))?;
    let stdout_task = tokio::spawn(capture_stream(
        stdout,
        OutputStream::Stdout,
        outgoing_tx.clone(),
        internal_tx.clone(),
    ));
    let stderr_task = tokio::spawn(capture_stream(
        stderr,
        OutputStream::Stderr,
        outgoing_tx.clone(),
        internal_tx.clone(),
    ));
    let stdin_task = child.stdin.take().map(|mut writer| {
        tokio::spawn(async move {
            if let Some(data) = stdin {
                writer.write_all(&data).await?;
            }
            writer.shutdown().await
        })
    });

    let mut cancelled = false;
    let mut daemon_gone = false;
    let mut interrupted_reason = None;
    let mut kill_deadline = None;
    let mut exited = None;
    let status = loop {
        tokio::select! {
            status = child.wait(), if exited.is_none() => {
                let status = status?;
                // The leader can die while the rest of its group lives on: a
                // process forked as SIGTERM was delivered never received it, and
                // it still holds the capture pipes, so leaving now would block on
                // stdout that never reaches EOF. Stay until the grace deadline
                // escalates to SIGKILL and the group is actually gone.
                if kill_deadline.is_none() || !process_group_alive(pgid) {
                    break status;
                }
                exited = Some(status);
            }
            message = internal_rx.recv() => {
                match message {
                    Some(InternalEvent::Cancel) if !cancelled => {
                        cancelled = true;
                        terminate(pgid, Signal::SIGTERM)?;
                        kill_deadline = Some(Box::pin(tokio::time::sleep(
                            Duration::from_millis(cancel_grace_ms),
                        )));
                    }
                    Some(InternalEvent::DaemonGone) => {
                        daemon_gone = true;
                        if !cancelled {
                            cancelled = true;
                            let _ = terminate(pgid, Signal::SIGTERM);
                            kill_deadline = Some(Box::pin(tokio::time::sleep(
                                Duration::from_millis(cancel_grace_ms),
                            )));
                        }
                    }
                    Some(InternalEvent::CaptureFailed(reason)) => {
                        interrupted_reason = Some(reason);
                        if !cancelled {
                            cancelled = true;
                            let _ = terminate(pgid, Signal::SIGTERM);
                            kill_deadline = Some(Box::pin(tokio::time::sleep(
                                Duration::from_millis(cancel_grace_ms),
                            )));
                        }
                    }
                    Some(InternalEvent::Cancel) | None => {}
                }
            }
            _ = async {
                if let Some(deadline) = &mut kill_deadline {
                    deadline.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                let _ = terminate(pgid, Signal::SIGKILL);
                kill_deadline = None;
                if let Some(status) = exited.take() {
                    break status;
                }
            }
        }
    };

    join_capture(stdout_task).await?;
    join_capture(stderr_task).await?;
    if let Some(task) = stdin_task {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
            Ok(Err(error)) => return Err(Error::Io(error)),
            Err(error) => {
                return Err(Error::Protocol(format!(
                    "stdin writer task failed: {error}"
                )));
            }
        }
    }
    if daemon_gone {
        return Ok(());
    }

    use std::os::unix::process::ExitStatusExt;
    let outcome = if let Some(reason) = interrupted_reason {
        ExecutionOutcome::Interrupted { reason }
    } else if cancelled {
        ExecutionOutcome::Cancelled {
            signal: status.signal(),
        }
    } else if let Some(code) = status.code() {
        ExecutionOutcome::Exited { code }
    } else {
        ExecutionOutcome::Signaled {
            signal: status.signal().unwrap_or_default(),
        }
    };
    send_and_flush(&outgoing_tx, SupervisorEvent::Finished { outcome }).await
}

fn build_command(spec: &CommandSpec, default_shell: &str) -> Command {
    match spec {
        CommandSpec::Argv { program, args } => {
            let mut command = Command::new(program);
            command.args(args);
            command
        }
        CommandSpec::Shell { command, shell } => {
            let mut process = Command::new(shell.as_deref().unwrap_or(default_shell));
            process.arg("-c").arg(command);
            process
        }
    }
}

async fn read_controls<R>(
    mut input: FramedRead<R, LengthDelimitedCodec>,
    sender: mpsc::Sender<InternalEvent>,
) where
    R: AsyncRead + Unpin,
{
    while let Some(frame) = input.next().await {
        match frame.map_err(Error::Io).and_then(|frame| {
            serde_json::from_slice::<SupervisorCommand>(&frame).map_err(Into::into)
        }) {
            Ok(SupervisorCommand::Cancel) => {
                if sender.send(InternalEvent::Cancel).await.is_err() {
                    return;
                }
            }
            Ok(SupervisorCommand::Spawn { .. }) | Err(_) => {
                let _ = sender
                    .send(InternalEvent::CaptureFailed(
                        "invalid supervisor control message".into(),
                    ))
                    .await;
                return;
            }
        }
    }
    let _ = sender.send(InternalEvent::DaemonGone).await;
}

async fn write_events(
    mut receiver: mpsc::Receiver<Outgoing>,
    internal: mpsc::Sender<InternalEvent>,
) {
    let codec = LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec();
    let mut output = FramedWrite::new(tokio::io::stdout(), codec);
    while let Some(message) = receiver.recv().await {
        match message {
            Outgoing::Event(event) => {
                let result = serde_json::to_vec(&event)
                    .map(Bytes::from)
                    .map_err(Error::Json);
                let result = match result {
                    Ok(frame) => output.send(frame).await.map_err(Error::Io),
                    Err(error) => Err(error),
                };
                if result.is_err() {
                    let _ = internal.send(InternalEvent::DaemonGone).await;
                    return;
                }
            }
            Outgoing::Flush(done) => {
                if output.flush().await.is_err() {
                    let _ = internal.send(InternalEvent::DaemonGone).await;
                    return;
                }
                let _ = done.send(());
            }
        }
    }
}

async fn capture_stream<R>(
    mut reader: R,
    stream: OutputStream,
    outgoing: mpsc::Sender<Outgoing>,
    internal: mpsc::Sender<InternalEvent>,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; OUTPUT_CHUNK_BYTES];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return,
            Ok(read) => {
                let event = SupervisorEvent::Output {
                    stream,
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&buffer[..read]),
                };
                if outgoing.send(Outgoing::Event(event)).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = internal
                    .send(InternalEvent::CaptureFailed(error.to_string()))
                    .await;
                return;
            }
        }
    }
}

async fn send_event(sender: &mpsc::Sender<Outgoing>, event: SupervisorEvent) -> Result<()> {
    sender
        .send(Outgoing::Event(event))
        .await
        .map_err(|_| Error::Protocol("supervisor event writer stopped".into()))
}

async fn send_and_flush(sender: &mpsc::Sender<Outgoing>, event: SupervisorEvent) -> Result<()> {
    send_event(sender, event).await?;
    let (done, flushed) = oneshot::channel();
    sender
        .send(Outgoing::Flush(done))
        .await
        .map_err(|_| Error::Protocol("supervisor event writer stopped".into()))?;
    flushed
        .await
        .map_err(|_| Error::Protocol("supervisor event writer failed to flush".into()))
}

async fn join_capture(task: tokio::task::JoinHandle<()>) -> Result<()> {
    task.await
        .map_err(|error| Error::Protocol(format!("capture task failed: {error}")))
}

fn process_group_alive(pgid: i32) -> bool {
    killpg(Pid::from_raw(pgid), None).is_ok()
}

fn terminate(pgid: i32, signal: Signal) -> Result<()> {
    match killpg(Pid::from_raw(pgid), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(Error::Io(std::io::Error::from_raw_os_error(error as i32))),
    }
}
