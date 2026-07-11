use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore, broadcast, mpsc, oneshot};

use crate::config::Settings;
use crate::model::{
    CommandSpec, Execution, ExecutionEvent, ExecutionOutcome, ExecutionRequest, ExecutionState,
    OutputStream, WaitResponse,
};
use crate::store::{CaptureRecord, Store};
use crate::{Error, Result};

const OUTPUT_CHUNK_BYTES: usize = 16 * 1024;
const DEFAULT_READ_BYTES: usize = 1024 * 1024;
const CAPTURE_QUEUE_DEPTH: usize = 256;
const CAPTURE_BATCH_SIZE: usize = 64;

enum CaptureMessage {
    Record(CaptureRecord),
    Barrier(oneshot::Sender<Result<()>>),
}

#[derive(Debug, Clone)]
struct ProcessControl {
    pgid: i32,
    cancelled: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct ExecutionEngine {
    store: Store,
    settings: Arc<Settings>,
    slots: Arc<Semaphore>,
    processes: Arc<Mutex<HashMap<String, ProcessControl>>>,
    transitions: Arc<Mutex<()>>,
    events: broadcast::Sender<ExecutionEvent>,
    capture_tx: mpsc::Sender<CaptureMessage>,
}

impl ExecutionEngine {
    pub fn new(store: Store, settings: Settings) -> Self {
        let max_concurrent = settings.max_concurrent_executions;
        let (events, _) = broadcast::channel(2048);
        let (capture_tx, capture_rx) = mpsc::channel(CAPTURE_QUEUE_DEPTH);
        tokio::spawn(capture_writer(store.clone(), events.clone(), capture_rx));
        Self {
            store,
            settings: Arc::new(settings),
            slots: Arc::new(Semaphore::new(max_concurrent)),
            processes: Arc::new(Mutex::new(HashMap::new())),
            transitions: Arc::new(Mutex::new(())),
            events,
            capture_tx,
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub async fn execute(&self, mut request: ExecutionRequest) -> Result<Execution> {
        request.validate()?;
        let workspace = self.store.get_workspace(&request.workspace_id)?;
        request.workspace_id = workspace.id.clone();
        let cwd = workspace.resolve_cwd(request.cwd.as_deref())?;
        let execution = self.store.create_execution(&request, &cwd)?;
        let engine = self.clone();
        let id = execution.id.clone();
        tokio::spawn(async move {
            if let Err(error) = engine.run_execution(id.clone(), request, cwd).await {
                tracing::error!(execution_id = %id, %error, "execution task failed");
                if let Ok(current) = engine.store.get_execution(&id)
                    && !current.state.is_terminal()
                    && let Ok(event) = engine.store.finish(
                        &id,
                        ExecutionState::Interrupted,
                        ExecutionOutcome::Interrupted {
                            reason: error.to_string(),
                        },
                    )
                {
                    engine.notify(event);
                }
            }
        });
        Ok(execution)
    }

    async fn run_execution(
        &self,
        id: String,
        request: ExecutionRequest,
        cwd: PathBuf,
    ) -> Result<()> {
        let _permit = self
            .slots
            .acquire()
            .await
            .map_err(|_| Error::Config("execution semaphore was closed".into()))?;

        if self.store.get_execution(&id)?.state.is_terminal() {
            return Ok(());
        }

        let stdin = request.stdin_bytes()?;
        let mut command = self.build_command(&request.command);
        command
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        for (key, value) in &request.env {
            command.env(key, value);
        }
        command.as_std_mut().process_group(0);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let event = self.store.finish(
                    &id,
                    ExecutionState::Finished,
                    ExecutionOutcome::SpawnError {
                        message: error.to_string(),
                    },
                )?;
                self.notify(event);
                return Ok(());
            }
        };
        let pid = child
            .id()
            .ok_or_else(|| Error::Protocol("spawned child has no pid".into()))?;
        let pgid = pid as i32;
        let cancelled = Arc::new(AtomicBool::new(false));

        {
            let _transition = self.transitions.lock().await;
            if self.store.get_execution(&id)?.state.is_terminal() {
                let _ = send_signal(pgid, Signal::SIGKILL);
                let _ = child.wait().await;
                return Ok(());
            }
            self.processes.lock().await.insert(
                id.clone(),
                ProcessControl {
                    pgid,
                    cancelled: cancelled.clone(),
                },
            );
            let event = self.store.mark_running(&id, pid, pgid)?;
            self.notify(event);
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("child stdout was not piped".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Protocol("child stderr was not piped".into()))?;
        let capture_limit = request
            .capture_limit_bytes
            .unwrap_or(self.settings.capture_limit_bytes);
        let captured = Arc::new(AtomicU64::new(0));
        let truncated = Arc::new(AtomicBool::new(false));

        let stdout_task = tokio::spawn(capture_stream(
            self.clone(),
            id.clone(),
            stdout,
            OutputStream::Stdout,
            capture_limit,
            captured.clone(),
            truncated.clone(),
        ));
        let stderr_task = tokio::spawn(capture_stream(
            self.clone(),
            id.clone(),
            stderr,
            OutputStream::Stderr,
            capture_limit,
            captured,
            truncated,
        ));
        let stdin_task = child.stdin.take().map(|mut writer| {
            tokio::spawn(async move {
                if let Some(data) = stdin {
                    writer.write_all(&data).await?;
                }
                writer.shutdown().await
            })
        });

        let status = child.wait().await?;
        if let Some(task) = stdin_task {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
                Ok(Err(error)) => tracing::warn!(execution_id = %id, %error, "stdin writer failed"),
                Err(error) => tracing::warn!(execution_id = %id, %error, "stdin task failed"),
            }
        }
        join_capture(&id, stdout_task).await?;
        join_capture(&id, stderr_task).await?;
        self.processes.lock().await.remove(&id);

        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;

        let was_cancelled = cancelled.load(Ordering::SeqCst);
        let (state, outcome) = if was_cancelled {
            (
                ExecutionState::Cancelled,
                ExecutionOutcome::Cancelled {
                    signal: status.signal(),
                },
            )
        } else if let Some(code) = status.code() {
            (ExecutionState::Finished, ExecutionOutcome::Exited { code })
        } else {
            (
                ExecutionState::Finished,
                ExecutionOutcome::Signaled {
                    signal: status.signal().unwrap_or_default(),
                },
            )
        };
        let event = self.store.finish(&id, state, outcome)?;
        self.notify(event);
        if let Err(error) = self
            .store
            .prune(self.settings.retention_days, self.settings.retention_bytes)
        {
            tracing::warn!(%error, "retention cleanup failed");
        }
        Ok(())
    }

    fn build_command(&self, spec: &CommandSpec) -> Command {
        match spec {
            CommandSpec::Argv { program, args } => {
                let mut command = Command::new(program);
                command.args(args);
                command
            }
            CommandSpec::Shell { command, shell } => {
                let mut process = Command::new(shell.as_deref().unwrap_or(&self.settings.shell));
                process.arg("-c").arg(command);
                process
            }
        }
    }

    pub async fn cancel(&self, id: &str) -> Result<Execution> {
        let _transition = self.transitions.lock().await;
        let execution = self.store.get_execution(id)?;
        match execution.state {
            ExecutionState::Queued => {
                let event = self.store.finish(
                    id,
                    ExecutionState::Cancelled,
                    ExecutionOutcome::Cancelled { signal: None },
                )?;
                self.notify(event);
            }
            ExecutionState::Running => {
                let control = self.processes.lock().await.get(id).cloned();
                if let Some(control) = control {
                    control.cancelled.store(true, Ordering::SeqCst);
                    send_signal(control.pgid, Signal::SIGTERM)?;
                    let processes = self.processes.clone();
                    let id = id.to_owned();
                    let grace = self.settings.cancel_grace_ms;
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(grace)).await;
                        let current = processes.lock().await.get(&id).cloned();
                        if let Some(current) = current {
                            let _ = send_signal(current.pgid, Signal::SIGKILL);
                        }
                    });
                } else {
                    return Err(Error::Protocol(format!(
                        "execution {id} is running but has no process handle"
                    )));
                }
            }
            _ => return Err(Error::AlreadyTerminal(id.into())),
        }
        self.store.get_execution(id)
    }

    pub async fn wait(
        &self,
        id: &str,
        after_seq: u64,
        timeout: Duration,
        max_bytes: Option<usize>,
    ) -> Result<WaitResponse> {
        let max_bytes = max_bytes.unwrap_or(DEFAULT_READ_BYTES);
        let mut receiver = self.events.subscribe();
        let first = self.store.read_output(id, after_seq, max_bytes)?;
        if first.execution.state.is_terminal() || !first.events.is_empty() {
            return Ok(WaitResponse {
                execution: first.execution,
                events: first.events,
                next_seq: first.next_seq,
                timed_out: false,
            });
        }

        let notification = tokio::time::timeout(timeout, async {
            loop {
                match receiver.recv().await {
                    Ok(event) if event.execution_id == id && event.seq > after_seq => break,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;
        let output = self.store.read_output(id, after_seq, max_bytes)?;
        Ok(WaitResponse {
            execution: output.execution,
            events: output.events,
            next_seq: output.next_seq,
            timed_out: notification.is_err(),
        })
    }

    pub async fn cancel_all(&self) {
        let ids: Vec<String> = self.processes.lock().await.keys().cloned().collect();
        for id in ids {
            let _ = self.cancel(&id).await;
        }
    }

    fn notify(&self, event: ExecutionEvent) {
        let _ = self.events.send(event);
    }
}

fn send_signal(pgid: i32, signal: Signal) -> Result<()> {
    match killpg(Pid::from_raw(pgid), signal) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(Error::Io(std::io::Error::from_raw_os_error(error as i32))),
    }
}

async fn capture_stream<R>(
    engine: ExecutionEngine,
    id: String,
    mut reader: R,
    stream: OutputStream,
    capture_limit: u64,
    captured: Arc<AtomicU64>,
    truncated: Arc<AtomicBool>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0u8; OUTPUT_CHUNK_BYTES];
    let read_result = loop {
        let read = match reader.read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => break Err(Error::Io(error)),
        };
        if read == 0 {
            break Ok(());
        }
        let previous = captured.fetch_add(read as u64, Ordering::SeqCst);
        let remaining = capture_limit.saturating_sub(previous) as usize;
        let persist = remaining.min(read);
        if persist > 0 {
            engine
                .capture_tx
                .send(CaptureMessage::Record(CaptureRecord::Output {
                    execution_id: id.clone(),
                    timestamp_ms: crate::model::now_ms(),
                    stream,
                    data: buffer[..persist].to_vec(),
                }))
                .await
                .map_err(|_| Error::Protocol("capture writer stopped".into()))?;
        }
        if persist < read && !truncated.swap(true, Ordering::SeqCst) {
            engine
                .capture_tx
                .send(CaptureMessage::Record(CaptureRecord::Truncated {
                    execution_id: id.clone(),
                    timestamp_ms: crate::model::now_ms(),
                    limit_bytes: capture_limit,
                }))
                .await
                .map_err(|_| Error::Protocol("capture writer stopped".into()))?;
        }
    };
    let (done_tx, done_rx) = oneshot::channel();
    engine
        .capture_tx
        .send(CaptureMessage::Barrier(done_tx))
        .await
        .map_err(|_| Error::Protocol("capture writer stopped".into()))?;
    done_rx
        .await
        .map_err(|_| Error::Protocol("capture writer dropped a flush barrier".into()))??;
    read_result
}

async fn join_capture(id: &str, task: tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    match task.await {
        Ok(result) => result,
        Err(error) => Err(Error::Protocol(format!(
            "output capture task for {id} failed: {error}"
        ))),
    }
}

async fn capture_writer(
    store: Store,
    events: broadcast::Sender<ExecutionEvent>,
    mut receiver: mpsc::Receiver<CaptureMessage>,
) {
    let mut records = Vec::with_capacity(CAPTURE_BATCH_SIZE);
    let mut pending_error: Option<String> = None;
    while let Some(message) = receiver.recv().await {
        process_capture_message(message, &store, &events, &mut records, &mut pending_error);
        while records.len() < CAPTURE_BATCH_SIZE {
            match receiver.try_recv() {
                Ok(message) => process_capture_message(
                    message,
                    &store,
                    &events,
                    &mut records,
                    &mut pending_error,
                ),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        flush_capture(&store, &events, &mut records, &mut pending_error);
    }
    flush_capture(&store, &events, &mut records, &mut pending_error);
}

fn process_capture_message(
    message: CaptureMessage,
    store: &Store,
    events: &broadcast::Sender<ExecutionEvent>,
    records: &mut Vec<CaptureRecord>,
    pending_error: &mut Option<String>,
) {
    match message {
        CaptureMessage::Record(record) => {
            records.push(record);
            if records.len() >= CAPTURE_BATCH_SIZE {
                flush_capture(store, events, records, pending_error);
            }
        }
        CaptureMessage::Barrier(done) => {
            flush_capture(store, events, records, pending_error);
            let result = match pending_error.take() {
                Some(error) => Err(Error::Protocol(error)),
                None => Ok(()),
            };
            let _ = done.send(result);
        }
    }
}

fn flush_capture(
    store: &Store,
    events: &broadcast::Sender<ExecutionEvent>,
    records: &mut Vec<CaptureRecord>,
    pending_error: &mut Option<String>,
) {
    if records.is_empty() {
        return;
    }
    match store.append_capture_batch(records) {
        Ok(written) => {
            for event in written {
                let _ = events.send(event);
            }
        }
        Err(error) => {
            tracing::error!(%error, "capture batch write failed");
            *pending_error = Some(error.to_string());
        }
    }
    records.clear();
}
