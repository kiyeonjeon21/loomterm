use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify, Semaphore, broadcast, mpsc, oneshot, watch};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::config::Settings;
use crate::model::{
    Execution, ExecutionEvent, ExecutionOutcome, ExecutionRequest, ExecutionState, WaitResponse,
};
use crate::protocol::MAX_FRAME_BYTES;
use crate::store::{CaptureRecord, Store};
use crate::supervisor::{SupervisorCommand, SupervisorEvent};
use crate::{Error, Result};

const DEFAULT_READ_BYTES: usize = 1024 * 1024;
const CAPTURE_QUEUE_DEPTH: usize = 256;
const CAPTURE_BATCH_SIZE: usize = 64;
const SHUTDOWN_SETTLE_MS: u64 = 5_000;

enum CaptureMessage {
    Record(CaptureRecord),
    Barrier(oneshot::Sender<Result<()>>),
}

#[derive(Debug, Clone)]
struct ProcessControl {
    sender: mpsc::Sender<SupervisorCommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Accepting,
    Draining,
    Stopped,
}

#[derive(Clone)]
pub struct ExecutionEngine {
    store: Store,
    settings: Arc<Settings>,
    slots: Arc<Semaphore>,
    processes: Arc<Mutex<HashMap<String, ProcessControl>>>,
    tasks: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>>,
    transitions: Arc<Mutex<()>>,
    lifecycle: Arc<Mutex<Lifecycle>>,
    events: broadcast::Sender<ExecutionEvent>,
    capture_tx: mpsc::Sender<CaptureMessage>,
    active: Arc<AtomicUsize>,
    idle: Arc<Notify>,
    fatal: watch::Sender<Option<String>>,
}

struct ActiveGuard {
    active: Arc<AtomicUsize>,
    idle: Arc<Notify>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.idle.notify_waiters();
    }
}

impl ExecutionEngine {
    pub fn new(store: Store, settings: Settings) -> Self {
        let max_concurrent = settings.max_concurrent_executions;
        let (events, _) = broadcast::channel(2048);
        let (capture_tx, capture_rx) = mpsc::channel(CAPTURE_QUEUE_DEPTH);
        let (fatal, _) = watch::channel(None);
        tokio::spawn(capture_writer(
            store.clone(),
            events.clone(),
            capture_rx,
            fatal.clone(),
        ));
        Self {
            store,
            settings: Arc::new(settings),
            slots: Arc::new(Semaphore::new(max_concurrent)),
            processes: Arc::new(Mutex::new(HashMap::new())),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            transitions: Arc::new(Mutex::new(())),
            lifecycle: Arc::new(Mutex::new(Lifecycle::Accepting)),
            events,
            capture_tx,
            active: Arc::new(AtomicUsize::new(0)),
            idle: Arc::new(Notify::new()),
            fatal,
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<ExecutionEvent> {
        self.events.subscribe()
    }

    pub fn subscribe_fatal(&self) -> watch::Receiver<Option<String>> {
        self.fatal.subscribe()
    }

    pub fn active_executions(&self) -> u64 {
        self.active.load(Ordering::SeqCst) as u64
    }

    pub async fn execute(&self, mut request: ExecutionRequest) -> Result<Execution> {
        request.validate()?;
        let lifecycle = self.lifecycle.lock().await;
        if *lifecycle != Lifecycle::Accepting {
            return Err(Error::DaemonDraining);
        }
        let workspace = self.store.get_workspace(&request.workspace_id).await?;
        request.workspace_id = workspace.id.clone();
        let cwd = workspace.resolve_cwd(request.cwd.as_deref())?;
        let execution = self.store.create_execution(&request, &cwd).await?;
        self.active.fetch_add(1, Ordering::SeqCst);

        let engine = self.clone();
        let id = execution.id.clone();
        let task_id = id.clone();
        let (start, started) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _active = ActiveGuard {
                active: engine.active.clone(),
                idle: engine.idle.clone(),
            };
            let _ = started.await;
            if let Err(error) = engine.run_execution(id.clone(), request, cwd).await {
                tracing::error!(execution_id = %id, %error, "execution task failed");
                if matches!(error, Error::StorageUnavailable(_)) {
                    engine.report_fatal(error.to_string());
                }
                if let Ok(current) = engine.store.get_execution(&id).await
                    && !current.state.is_terminal()
                    && let Ok(event) = engine
                        .store
                        .finish(
                            &id,
                            ExecutionState::Interrupted,
                            ExecutionOutcome::Interrupted {
                                reason: error.to_string(),
                            },
                        )
                        .await
                {
                    engine.notify(event);
                }
            }
            engine.processes.lock().await.remove(&id);
            engine.tasks.lock().await.remove(&id);
        });
        self.tasks.lock().await.insert(task_id, task.abort_handle());
        let _ = start.send(());
        drop(lifecycle);
        Ok(execution)
    }

    async fn run_execution(
        &self,
        id: String,
        request: ExecutionRequest,
        cwd: PathBuf,
    ) -> Result<()> {
        let _permit = match self.slots.acquire().await {
            Ok(permit) => permit,
            Err(_) => return Ok(()),
        };
        if self.store.get_execution(&id).await?.state.is_terminal() {
            return Ok(());
        }

        let mut supervisor = Command::new(self.supervisor_executable());
        supervisor
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = match supervisor.spawn() {
            Ok(child) => child,
            Err(error) => {
                let event = self
                    .store
                    .finish(
                        &id,
                        ExecutionState::Finished,
                        ExecutionOutcome::SpawnError {
                            message: format!("could not start loom-supervisor: {error}"),
                        },
                    )
                    .await?;
                self.notify(event);
                return Ok(());
            }
        };
        let supervisor_stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Protocol("supervisor stdin was not piped".into()))?;
        let supervisor_stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("supervisor stdout was not piped".into()))?;
        let (control_tx, control_rx) = mpsc::channel(8);
        let control_task = tokio::spawn(write_supervisor_commands(supervisor_stdin, control_rx));
        let capture_limit = request
            .capture_limit_bytes
            .unwrap_or(self.settings.capture_limit_bytes);
        control_tx
            .send(SupervisorCommand::Spawn {
                command: request.command,
                cwd: cwd.to_string_lossy().into_owned(),
                env: request.env,
                stdin_base64: request.stdin_base64,
                shell: self.settings.shell.clone(),
                cancel_grace_ms: self.settings.cancel_grace_ms,
            })
            .await
            .map_err(|_| Error::Protocol("supervisor control writer stopped".into()))?;

        {
            let _transition = self.transitions.lock().await;
            if self.store.get_execution(&id).await?.state.is_terminal() {
                let _ = control_tx.send(SupervisorCommand::Cancel).await;
            } else {
                self.processes.lock().await.insert(
                    id.clone(),
                    ProcessControl {
                        sender: control_tx.clone(),
                    },
                );
            }
        }

        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(MAX_FRAME_BYTES)
            .new_codec();
        let mut events = FramedRead::new(supervisor_stdout, codec);
        let captured = AtomicU64::new(0);
        let truncated = AtomicBool::new(false);
        let mut received_finished = false;

        while let Some(frame) = events.next().await {
            let event: SupervisorEvent = serde_json::from_slice(&frame?)?;
            match event {
                SupervisorEvent::Started { pid, pgid } => {
                    let _transition = self.transitions.lock().await;
                    let current = self.store.get_execution(&id).await?;
                    if current.state.is_terminal() {
                        let _ = control_tx.send(SupervisorCommand::Cancel).await;
                    } else {
                        let event = self.store.mark_running(&id, pid, pgid).await?;
                        self.notify(event);
                    }
                }
                SupervisorEvent::Output {
                    stream,
                    data_base64,
                } => {
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(data_base64)
                        .map_err(|error| {
                            Error::Protocol(format!("invalid supervisor output: {error}"))
                        })?;
                    self.capture_output(&id, stream, data, capture_limit, &captured, &truncated)
                        .await?;
                }
                SupervisorEvent::Finished { outcome } => {
                    self.flush_capture().await?;
                    let current = self.store.get_execution(&id).await?;
                    if !current.state.is_terminal() {
                        let state = match outcome {
                            ExecutionOutcome::Cancelled { .. } => ExecutionState::Cancelled,
                            ExecutionOutcome::Interrupted { .. } => ExecutionState::Interrupted,
                            _ => ExecutionState::Finished,
                        };
                        let event = self.store.finish(&id, state, outcome).await?;
                        self.notify(event);
                    }
                    received_finished = true;
                    break;
                }
            }
        }

        self.processes.lock().await.remove(&id);
        drop(control_tx);
        match control_task.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) if received_finished => {
                tracing::debug!(execution_id = %id, %error, "supervisor control pipe closed");
            }
            Ok(Err(error)) => return Err(error),
            Err(error) => {
                return Err(Error::Protocol(format!(
                    "supervisor control task failed: {error}"
                )));
            }
        }
        let supervisor_status = child.wait().await?;
        if !received_finished {
            self.flush_capture().await?;
            let current = self.store.get_execution(&id).await?;
            if !current.state.is_terminal() {
                let event = self
                    .store
                    .finish(
                        &id,
                        ExecutionState::Interrupted,
                        ExecutionOutcome::Interrupted {
                            reason: format!(
                                "loom-supervisor exited without a terminal event: {supervisor_status}"
                            ),
                        },
                    )
                    .await?;
                self.notify(event);
            }
        }
        if let Err(error) = self
            .store
            .prune(self.settings.retention_days, self.settings.retention_bytes)
            .await
        {
            tracing::warn!(%error, "retention cleanup failed");
        }
        Ok(())
    }

    fn supervisor_executable(&self) -> PathBuf {
        if let Some(path) = &self.settings.supervisor_path {
            return path.clone();
        }
        let sibling = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("loomd"))
            .with_file_name("loom-supervisor");
        if sibling.exists() {
            sibling
        } else {
            PathBuf::from("loom-supervisor")
        }
    }

    async fn capture_output(
        &self,
        id: &str,
        stream: crate::model::OutputStream,
        data: Vec<u8>,
        capture_limit: u64,
        captured: &AtomicU64,
        truncated: &AtomicBool,
    ) -> Result<()> {
        let previous = captured.fetch_add(data.len() as u64, Ordering::SeqCst);
        let remaining = capture_limit.saturating_sub(previous) as usize;
        let persist = remaining.min(data.len());
        if persist > 0 {
            self.capture_tx
                .send(CaptureMessage::Record(CaptureRecord::Output {
                    execution_id: id.to_owned(),
                    timestamp_ms: crate::model::now_ms(),
                    stream,
                    data: data[..persist].to_vec(),
                }))
                .await
                .map_err(|_| Error::Protocol("capture writer stopped".into()))?;
        }
        if persist < data.len() && !truncated.swap(true, Ordering::SeqCst) {
            self.capture_tx
                .send(CaptureMessage::Record(CaptureRecord::Truncated {
                    execution_id: id.to_owned(),
                    timestamp_ms: crate::model::now_ms(),
                    limit_bytes: capture_limit,
                }))
                .await
                .map_err(|_| Error::Protocol("capture writer stopped".into()))?;
        }
        Ok(())
    }

    async fn flush_capture(&self) -> Result<()> {
        let (done, flushed) = oneshot::channel();
        self.capture_tx
            .send(CaptureMessage::Barrier(done))
            .await
            .map_err(|_| Error::Protocol("capture writer stopped".into()))?;
        flushed
            .await
            .map_err(|_| Error::Protocol("capture writer dropped a flush barrier".into()))?
    }

    pub async fn cancel(&self, id: &str) -> Result<Execution> {
        let mut receiver = self.events.subscribe();
        {
            let _transition = self.transitions.lock().await;
            let execution = self.store.get_execution(id).await?;
            let control = self.processes.lock().await.get(id).cloned();
            match execution.state {
                ExecutionState::Queued => {
                    let event = self
                        .store
                        .finish(
                            id,
                            ExecutionState::Cancelled,
                            ExecutionOutcome::Cancelled { signal: None },
                        )
                        .await?;
                    self.notify(event);
                    if let Some(control) = control {
                        let _ = control.sender.send(SupervisorCommand::Cancel).await;
                    }
                }
                ExecutionState::Running => {
                    let control = control.ok_or_else(|| {
                        Error::Protocol(format!(
                            "execution {id} is running but has no supervisor handle"
                        ))
                    })?;
                    control
                        .sender
                        .send(SupervisorCommand::Cancel)
                        .await
                        .map_err(|_| Error::Protocol("supervisor control writer stopped".into()))?;
                }
                _ => return Err(Error::AlreadyTerminal(id.into())),
            }
        }

        let timeout = Duration::from_millis(
            self.settings
                .cancel_grace_ms
                .saturating_add(SHUTDOWN_SETTLE_MS),
        );
        tokio::time::timeout(timeout, async {
            loop {
                let execution = self.store.get_execution(id).await?;
                if execution.state.is_terminal() {
                    return Ok(execution);
                }
                match receiver.recv().await {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(Error::Protocol("execution event channel closed".into()));
                    }
                }
            }
        })
        .await
        .map_err(|_| Error::Timeout)?
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
        let first = self.store.read_output(id, after_seq, max_bytes).await?;
        if first.execution.state.is_terminal() || !first.events.is_empty() {
            return Ok(WaitResponse {
                execution: first.execution,
                events: first.events,
                next_seq: first.next_seq,
                has_more: first.has_more,
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
        let output = self.store.read_output(id, after_seq, max_bytes).await?;
        Ok(WaitResponse {
            execution: output.execution,
            events: output.events,
            next_seq: output.next_seq,
            has_more: output.has_more,
            timed_out: notification.is_err(),
        })
    }

    pub async fn drain(&self) -> Result<()> {
        {
            let mut lifecycle = self.lifecycle.lock().await;
            if *lifecycle == Lifecycle::Stopped {
                return Ok(());
            }
            *lifecycle = Lifecycle::Draining;
        }
        self.slots.close();
        {
            let _transition = self.transitions.lock().await;
            for event in self.store.cancel_queued().await? {
                self.notify(event);
            }
            let controls: Vec<_> = self.processes.lock().await.values().cloned().collect();
            for control in controls {
                let _ = control.sender.send(SupervisorCommand::Cancel).await;
            }
        }

        let timeout = Duration::from_millis(
            self.settings
                .cancel_grace_ms
                .saturating_add(SHUTDOWN_SETTLE_MS),
        );
        if tokio::time::timeout(timeout, self.wait_for_idle())
            .await
            .is_err()
        {
            self.processes.lock().await.clear();
            let handles: Vec<_> = self.tasks.lock().await.values().cloned().collect();
            for handle in handles {
                handle.abort();
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), self.wait_for_idle()).await;
        }
        *self.lifecycle.lock().await = Lifecycle::Stopped;
        Ok(())
    }

    async fn wait_for_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.active.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }

    fn notify(&self, event: ExecutionEvent) {
        let _ = self.events.send(event);
    }

    fn report_fatal(&self, message: String) {
        let _ = self.fatal.send(Some(message));
    }
}

async fn write_supervisor_commands(
    writer: tokio::process::ChildStdin,
    mut receiver: mpsc::Receiver<SupervisorCommand>,
) -> Result<()> {
    let codec = LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec();
    let mut output = FramedWrite::new(writer, codec);
    while let Some(command) = receiver.recv().await {
        output
            .send(Bytes::from(serde_json::to_vec(&command)?))
            .await?;
    }
    output.close().await?;
    Ok(())
}

async fn capture_writer(
    store: Store,
    events: broadcast::Sender<ExecutionEvent>,
    mut receiver: mpsc::Receiver<CaptureMessage>,
    fatal: watch::Sender<Option<String>>,
) {
    let mut records = Vec::with_capacity(CAPTURE_BATCH_SIZE);
    let mut pending_error: Option<String> = None;
    while let Some(message) = receiver.recv().await {
        process_capture_message(
            message,
            &store,
            &events,
            &mut records,
            &mut pending_error,
            &fatal,
        )
        .await;
        while records.len() < CAPTURE_BATCH_SIZE {
            match receiver.try_recv() {
                Ok(message) => {
                    process_capture_message(
                        message,
                        &store,
                        &events,
                        &mut records,
                        &mut pending_error,
                        &fatal,
                    )
                    .await
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        flush_capture(&store, &events, &mut records, &mut pending_error, &fatal).await;
    }
    flush_capture(&store, &events, &mut records, &mut pending_error, &fatal).await;
}

async fn process_capture_message(
    message: CaptureMessage,
    store: &Store,
    events: &broadcast::Sender<ExecutionEvent>,
    records: &mut Vec<CaptureRecord>,
    pending_error: &mut Option<String>,
    fatal: &watch::Sender<Option<String>>,
) {
    match message {
        CaptureMessage::Record(record) => {
            records.push(record);
            if records.len() >= CAPTURE_BATCH_SIZE {
                flush_capture(store, events, records, pending_error, fatal).await;
            }
        }
        CaptureMessage::Barrier(done) => {
            flush_capture(store, events, records, pending_error, fatal).await;
            let result = match pending_error.take() {
                Some(error) => Err(Error::StorageUnavailable(error)),
                None => Ok(()),
            };
            let _ = done.send(result);
        }
    }
}

async fn flush_capture(
    store: &Store,
    events: &broadcast::Sender<ExecutionEvent>,
    records: &mut Vec<CaptureRecord>,
    pending_error: &mut Option<String>,
    fatal: &watch::Sender<Option<String>>,
) {
    if records.is_empty() {
        return;
    }
    let batch = std::mem::take(records);
    match store.append_capture_batch(batch).await {
        Ok(written) => {
            for event in written {
                let _ = events.send(event);
            }
        }
        Err(error) => {
            tracing::error!(%error, "capture batch write failed");
            let message = error.to_string();
            *pending_error = Some(message.clone());
            let _ = fatal.send(Some(message));
        }
    }
}

pub fn workspace_contains(root: &Path, cwd: &Path) -> bool {
    cwd == root || cwd.starts_with(root)
}
