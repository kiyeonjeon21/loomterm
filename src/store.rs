use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use base64::Engine;
use rusqlite::{Connection, OptionalExtension, Row, params};
use tokio::sync::{mpsc, oneshot};

use crate::model::{
    CommandSpec, Execution, ExecutionEvent, ExecutionEventPayload, ExecutionOutcome,
    ExecutionRequest, ExecutionState, ExecutionStats, ExecutionStatusCounts, InitiatorStats,
    OutputStream, ReadOutputResponse, Workspace, new_id, now_ms,
};
use crate::{Error, Result};

const SCHEMA_VERSION: i64 = 2;
const STORE_QUEUE_DEPTH: usize = 512;

#[derive(Debug)]
pub(crate) enum CaptureRecord {
    Output {
        execution_id: String,
        timestamp_ms: i64,
        stream: OutputStream,
        data: Vec<u8>,
    },
    Truncated {
        execution_id: String,
        timestamp_ms: i64,
        limit_bytes: u64,
    },
}

#[derive(Clone, Debug)]
pub struct Store {
    sender: mpsc::Sender<StoreCommand>,
}

enum StoreCommand {
    AddWorkspace {
        name: String,
        root: PathBuf,
        reply: oneshot::Sender<Result<Workspace>>,
    },
    RemoveWorkspace {
        identifier: String,
        reply: oneshot::Sender<Result<()>>,
    },
    GetWorkspace {
        identifier: String,
        reply: oneshot::Sender<Result<Workspace>>,
    },
    FindWorkspaceByRoot {
        root: PathBuf,
        reply: oneshot::Sender<Result<Option<Workspace>>>,
    },
    ListWorkspaces {
        reply: oneshot::Sender<Result<Vec<Workspace>>>,
    },
    CreateExecution {
        request: Box<ExecutionRequest>,
        cwd: PathBuf,
        reply: oneshot::Sender<Result<Execution>>,
    },
    MarkRunning {
        id: String,
        pid: u32,
        pgid: i32,
        reply: oneshot::Sender<Result<ExecutionEvent>>,
    },
    AppendCaptureBatch {
        records: Vec<CaptureRecord>,
        reply: oneshot::Sender<Result<Vec<ExecutionEvent>>>,
    },
    Finish {
        id: String,
        state: ExecutionState,
        outcome: ExecutionOutcome,
        reply: oneshot::Sender<Result<ExecutionEvent>>,
    },
    GetExecution {
        id: String,
        reply: oneshot::Sender<Result<Execution>>,
    },
    ListExecutions {
        workspace: Option<String>,
        limit: u32,
        reply: oneshot::Sender<Result<Vec<Execution>>>,
    },
    ExecutionStats {
        workspace: String,
        since_ms: i64,
        until_ms: i64,
        reply: oneshot::Sender<Result<ExecutionStats>>,
    },
    ReadOutput {
        id: String,
        after_seq: u64,
        max_bytes: usize,
        reply: oneshot::Sender<Result<ReadOutputResponse>>,
    },
    ReconcileIncomplete {
        reply: oneshot::Sender<Result<usize>>,
    },
    CancelQueued {
        reply: oneshot::Sender<Result<Vec<ExecutionEvent>>>,
    },
    Prune {
        retention_days: u64,
        retention_bytes: u64,
        reply: oneshot::Sender<Result<usize>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<()>>,
    },
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::spawn(StoreLocation::Path(path.to_path_buf()))
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::spawn(StoreLocation::Memory)
    }

    fn spawn(location: StoreLocation) -> Result<Self> {
        let (sender, receiver) = mpsc::channel(STORE_QUEUE_DEPTH);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("loomterm-sqlite".into())
            .spawn(move || match Database::open(location) {
                Ok(database) => {
                    let _ = ready_tx.send(Ok(()));
                    run_store_actor(database, receiver);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })?;
        ready_rx.recv().map_err(|_| {
            Error::StorageUnavailable("storage actor failed during startup".into())
        })??;
        Ok(Self { sender })
    }

    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T>>) -> StoreCommand,
    ) -> Result<T> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(build(reply))
            .await
            .map_err(|_| Error::StorageUnavailable("storage actor stopped".into()))?;
        response
            .await
            .map_err(|_| Error::StorageUnavailable("storage actor dropped a response".into()))?
    }

    pub async fn add_workspace(&self, name: &str, root: &Path) -> Result<Workspace> {
        self.request(|reply| StoreCommand::AddWorkspace {
            name: name.to_owned(),
            root: root.to_path_buf(),
            reply,
        })
        .await
    }

    pub async fn remove_workspace(&self, identifier: &str) -> Result<()> {
        self.request(|reply| StoreCommand::RemoveWorkspace {
            identifier: identifier.to_owned(),
            reply,
        })
        .await
    }

    pub async fn get_workspace(&self, identifier: &str) -> Result<Workspace> {
        self.request(|reply| StoreCommand::GetWorkspace {
            identifier: identifier.to_owned(),
            reply,
        })
        .await
    }

    pub async fn find_workspace_by_root(&self, root: &Path) -> Result<Option<Workspace>> {
        self.request(|reply| StoreCommand::FindWorkspaceByRoot {
            root: root.to_path_buf(),
            reply,
        })
        .await
    }

    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        self.request(|reply| StoreCommand::ListWorkspaces { reply })
            .await
    }

    pub async fn create_execution(
        &self,
        request: &ExecutionRequest,
        cwd: &Path,
    ) -> Result<Execution> {
        self.request(|reply| StoreCommand::CreateExecution {
            request: Box::new(request.clone()),
            cwd: cwd.to_path_buf(),
            reply,
        })
        .await
    }

    pub async fn mark_running(&self, id: &str, pid: u32, pgid: i32) -> Result<ExecutionEvent> {
        self.request(|reply| StoreCommand::MarkRunning {
            id: id.to_owned(),
            pid,
            pgid,
            reply,
        })
        .await
    }

    pub(crate) async fn append_capture_batch(
        &self,
        records: Vec<CaptureRecord>,
    ) -> Result<Vec<ExecutionEvent>> {
        self.request(|reply| StoreCommand::AppendCaptureBatch { records, reply })
            .await
    }

    pub async fn append_output(
        &self,
        id: &str,
        stream: OutputStream,
        data: &[u8],
    ) -> Result<ExecutionEvent> {
        self.append_capture_batch(vec![CaptureRecord::Output {
            execution_id: id.into(),
            timestamp_ms: now_ms(),
            stream,
            data: data.to_vec(),
        }])
        .await?
        .pop()
        .ok_or_else(|| Error::Protocol("capture batch produced no event".into()))
    }

    pub async fn mark_truncated(&self, id: &str, limit: u64) -> Result<ExecutionEvent> {
        self.append_capture_batch(vec![CaptureRecord::Truncated {
            execution_id: id.into(),
            timestamp_ms: now_ms(),
            limit_bytes: limit,
        }])
        .await?
        .pop()
        .ok_or_else(|| Error::Protocol("capture batch produced no event".into()))
    }

    pub async fn finish(
        &self,
        id: &str,
        state: ExecutionState,
        outcome: ExecutionOutcome,
    ) -> Result<ExecutionEvent> {
        self.request(|reply| StoreCommand::Finish {
            id: id.to_owned(),
            state,
            outcome,
            reply,
        })
        .await
    }

    pub async fn get_execution(&self, id: &str) -> Result<Execution> {
        self.request(|reply| StoreCommand::GetExecution {
            id: id.to_owned(),
            reply,
        })
        .await
    }

    pub async fn list_executions(
        &self,
        workspace: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Execution>> {
        self.request(|reply| StoreCommand::ListExecutions {
            workspace: workspace.map(str::to_owned),
            limit,
            reply,
        })
        .await
    }

    pub async fn execution_stats(
        &self,
        workspace: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<ExecutionStats> {
        if since_ms > until_ms {
            return Err(Error::InvalidRequest(
                "statistics window start must not exceed its end".into(),
            ));
        }
        self.request(|reply| StoreCommand::ExecutionStats {
            workspace: workspace.to_owned(),
            since_ms,
            until_ms,
            reply,
        })
        .await
    }

    pub async fn read_output(
        &self,
        id: &str,
        after_seq: u64,
        max_bytes: usize,
    ) -> Result<ReadOutputResponse> {
        self.request(|reply| StoreCommand::ReadOutput {
            id: id.to_owned(),
            after_seq,
            max_bytes,
            reply,
        })
        .await
    }

    pub async fn reconcile_incomplete(&self) -> Result<usize> {
        self.request(|reply| StoreCommand::ReconcileIncomplete { reply })
            .await
    }

    pub async fn cancel_queued(&self) -> Result<Vec<ExecutionEvent>> {
        self.request(|reply| StoreCommand::CancelQueued { reply })
            .await
    }

    pub async fn prune(&self, retention_days: u64, retention_bytes: u64) -> Result<usize> {
        self.request(|reply| StoreCommand::Prune {
            retention_days,
            retention_bytes,
            reply,
        })
        .await
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.request(|reply| StoreCommand::Shutdown { reply }).await
    }
}

enum StoreLocation {
    Path(PathBuf),
    #[cfg(test)]
    Memory,
}

struct Database {
    connection: Mutex<Connection>,
}

impl Database {
    fn open(location: StoreLocation) -> Result<Self> {
        let connection = match location {
            StoreLocation::Path(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                Connection::open(path)?
            }
            #[cfg(test)]
            StoreLocation::Memory => Connection::open_in_memory()?,
        };
        let database = Self {
            connection: Mutex::new(connection),
        };
        database.initialize()?;
        Ok(database)
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| Error::Config("database mutex was poisoned".into()))
    }

    fn initialize(&self) -> Result<()> {
        let connection = self.connection()?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(Error::Config(format!(
                "database schema {version} is newer than supported schema {SCHEMA_VERSION}"
            )));
        }
        if version == 0 {
            connection.execute_batch(
                "BEGIN;
                 CREATE TABLE workspaces (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL UNIQUE,
                    root TEXT NOT NULL UNIQUE,
                    created_at_ms INTEGER NOT NULL,
                    active INTEGER NOT NULL DEFAULT 1
                 );
                 CREATE TABLE executions (
                    id TEXT PRIMARY KEY,
                    workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                    state TEXT NOT NULL,
                    command_json TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    env_keys_json TEXT NOT NULL,
                    initiator_json TEXT NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    started_at_ms INTEGER,
                    ended_at_ms INTEGER,
                    pid INTEGER,
                    pgid INTEGER,
                    outcome_json TEXT,
                    captured_bytes INTEGER NOT NULL DEFAULT 0,
                    output_truncated INTEGER NOT NULL DEFAULT 0,
                    last_seq INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE INDEX executions_workspace_created
                    ON executions(workspace_id, created_at_ms DESC);
                 CREATE INDEX executions_state_created
                    ON executions(state, created_at_ms);
                 CREATE TABLE events (
                    execution_id TEXT NOT NULL REFERENCES executions(id) ON DELETE CASCADE,
                    seq INTEGER NOT NULL,
                    timestamp_ms INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    stream TEXT,
                    data BLOB,
                    payload_json TEXT,
                    PRIMARY KEY(execution_id, seq)
                 );
                 PRAGMA user_version = 2;
                 COMMIT;",
            )?;
        } else if version == 1 {
            connection.execute_batch(
                "BEGIN;
                 ALTER TABLE workspaces ADD COLUMN active INTEGER NOT NULL DEFAULT 1;
                 PRAGMA user_version = 2;
                 COMMIT;",
            )?;
        }
        Ok(())
    }

    pub fn add_workspace(&self, name: &str, root: &Path) -> Result<Workspace> {
        if name.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "workspace name must not be empty".into(),
            ));
        }
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(Error::InvalidRequest(format!(
                "workspace root is not a directory: {}",
                root.display()
            )));
        }
        let root = root.to_string_lossy().into_owned();
        let connection = self.connection()?;
        let existing_root = connection
            .query_row(
                "SELECT id, name, root, created_at_ms, active FROM workspaces WHERE root = ?1",
                [&root],
                workspace_with_active_from_row,
            )
            .optional()?;
        if let Some((workspace, active)) = existing_root {
            if workspace.name != name {
                return Err(Error::InvalidRequest(format!(
                    "workspace root {} is already registered as {}",
                    workspace.root, workspace.name
                )));
            }
            if !active {
                connection.execute(
                    "UPDATE workspaces SET active = 1 WHERE id = ?1",
                    [&workspace.id],
                )?;
            }
            return Ok(workspace);
        }
        if let Some(workspace) = connection
            .query_row(
                "SELECT id, name, root, created_at_ms FROM workspaces WHERE name = ?1",
                [name],
                workspace_from_row,
            )
            .optional()?
        {
            return Err(Error::InvalidRequest(format!(
                "workspace name {name} is already registered for {}",
                workspace.root
            )));
        }
        let workspace = Workspace {
            id: new_id(),
            name: name.to_owned(),
            root,
            created_at_ms: now_ms(),
        };
        connection.execute(
            "INSERT INTO workspaces(id, name, root, created_at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![
                workspace.id,
                workspace.name,
                workspace.root,
                workspace.created_at_ms
            ],
        )?;
        Ok(workspace)
    }

    pub fn remove_workspace(&self, identifier: &str) -> Result<()> {
        let connection = self.connection()?;
        let workspace = connection
            .query_row(
                "SELECT id FROM workspaces WHERE id = ?1 OR name = ?1",
                [identifier],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(id) = workspace else {
            return Err(Error::WorkspaceNotFound(identifier.into()));
        };
        connection.execute("UPDATE workspaces SET active = 0 WHERE id = ?1", [&id])?;
        Ok(())
    }

    pub fn get_workspace(&self, identifier: &str) -> Result<Workspace> {
        self.connection()?
            .query_row(
                "SELECT id, name, root, created_at_ms
                 FROM workspaces WHERE (id = ?1 OR name = ?1) AND active = 1",
                [identifier],
                workspace_from_row,
            )
            .optional()?
            .ok_or_else(|| Error::WorkspaceNotFound(identifier.into()))
    }

    pub fn find_workspace_by_root(&self, root: &Path) -> Result<Option<Workspace>> {
        let canonical = root.canonicalize()?;
        self.connection()?
            .query_row(
                "SELECT id, name, root, created_at_ms
                 FROM workspaces WHERE root = ?1 AND active = 1",
                [canonical.to_string_lossy().as_ref()],
                workspace_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, name, root, created_at_ms FROM workspaces
             WHERE active = 1 ORDER BY name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], workspace_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn create_execution(&self, request: &ExecutionRequest, cwd: &Path) -> Result<Execution> {
        request.validate()?;
        let id = new_id();
        let created_at_ms = now_ms();
        let env_keys: Vec<String> = request.env.keys().cloned().collect();
        let command_json = serde_json::to_string(&request.command)?;
        let env_keys_json = serde_json::to_string(&env_keys)?;
        let initiator_json = serde_json::to_string(&request.initiator)?;
        self.connection()?.execute(
            "INSERT INTO executions(
                id, workspace_id, state, command_json, cwd, env_keys_json,
                initiator_json, created_at_ms
             ) VALUES (?1, ?2, 'queued', ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                request.workspace_id,
                command_json,
                cwd.to_string_lossy().as_ref(),
                env_keys_json,
                initiator_json,
                created_at_ms
            ],
        )?;
        self.get_execution(&id)
    }

    pub fn mark_running(&self, id: &str, pid: u32, pgid: i32) -> Result<ExecutionEvent> {
        let timestamp = now_ms();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let state: String = transaction
            .query_row("SELECT state FROM executions WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?
            .ok_or_else(|| Error::ExecutionNotFound(id.into()))?;
        if state != "queued" {
            return Err(Error::AlreadyTerminal(id.into()));
        }
        let seq = next_seq(&transaction, id)?;
        let payload = serde_json::to_string(&serde_json::json!({"pid": pid, "pgid": pgid}))?;
        transaction.execute(
            "INSERT INTO events(execution_id, seq, timestamp_ms, kind, payload_json)
             VALUES (?1, ?2, ?3, 'started', ?4)",
            params![id, seq as i64, timestamp, payload],
        )?;
        transaction.execute(
            "UPDATE executions SET state = 'running', started_at_ms = ?2,
                pid = ?3, pgid = ?4, last_seq = ?5 WHERE id = ?1",
            params![id, timestamp, pid, pgid, seq as i64],
        )?;
        transaction.commit()?;
        Ok(ExecutionEvent {
            execution_id: id.into(),
            seq,
            timestamp_ms: timestamp,
            payload: ExecutionEventPayload::Started { pid, pgid },
        })
    }

    pub(crate) fn append_capture_batch(
        &self,
        records: &[CaptureRecord],
    ) -> Result<Vec<ExecutionEvent>> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mut events = Vec::with_capacity(records.len());
        for record in records {
            match record {
                CaptureRecord::Output {
                    execution_id,
                    timestamp_ms,
                    stream,
                    data,
                } => {
                    let seq = next_seq(&transaction, execution_id)?;
                    transaction.execute(
                        "INSERT INTO events(execution_id, seq, timestamp_ms, kind, stream, data)
                         VALUES (?1, ?2, ?3, 'output', ?4, ?5)",
                        params![
                            execution_id,
                            seq as i64,
                            timestamp_ms,
                            stream.as_str(),
                            data
                        ],
                    )?;
                    transaction.execute(
                        "UPDATE executions SET last_seq = ?2,
                            captured_bytes = captured_bytes + ?3 WHERE id = ?1",
                        params![execution_id, seq as i64, data.len() as i64],
                    )?;
                    events.push(output_event(
                        execution_id,
                        seq,
                        *timestamp_ms,
                        *stream,
                        data,
                    ));
                }
                CaptureRecord::Truncated {
                    execution_id,
                    timestamp_ms,
                    limit_bytes,
                } => {
                    let seq = next_seq(&transaction, execution_id)?;
                    let payload =
                        serde_json::to_string(&serde_json::json!({"limit_bytes": limit_bytes}))?;
                    transaction.execute(
                        "INSERT INTO events(execution_id, seq, timestamp_ms, kind, payload_json)
                         VALUES (?1, ?2, ?3, 'capture_truncated', ?4)",
                        params![execution_id, seq as i64, timestamp_ms, payload],
                    )?;
                    transaction.execute(
                        "UPDATE executions SET last_seq = ?2, output_truncated = 1 WHERE id = ?1",
                        params![execution_id, seq as i64],
                    )?;
                    events.push(ExecutionEvent {
                        execution_id: execution_id.clone(),
                        seq,
                        timestamp_ms: *timestamp_ms,
                        payload: ExecutionEventPayload::CaptureTruncated {
                            limit_bytes: *limit_bytes,
                        },
                    });
                }
            }
        }
        transaction.commit()?;
        Ok(events)
    }

    pub fn finish(
        &self,
        id: &str,
        state: ExecutionState,
        outcome: ExecutionOutcome,
    ) -> Result<ExecutionEvent> {
        let timestamp = now_ms();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let current: String = transaction
            .query_row("SELECT state FROM executions WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?
            .ok_or_else(|| Error::ExecutionNotFound(id.into()))?;
        if matches!(current.as_str(), "finished" | "cancelled" | "interrupted") {
            return Err(Error::AlreadyTerminal(id.into()));
        }
        let seq = next_seq(&transaction, id)?;
        let outcome_json = serde_json::to_string(&outcome)?;
        transaction.execute(
            "INSERT INTO events(execution_id, seq, timestamp_ms, kind, payload_json)
             VALUES (?1, ?2, ?3, 'finished', ?4)",
            params![id, seq as i64, timestamp, outcome_json],
        )?;
        transaction.execute(
            "UPDATE executions SET state = ?2, ended_at_ms = ?3, outcome_json = ?4,
                last_seq = ?5 WHERE id = ?1",
            params![id, state.as_str(), timestamp, outcome_json, seq as i64],
        )?;
        transaction.commit()?;
        Ok(ExecutionEvent {
            execution_id: id.into(),
            seq,
            timestamp_ms: timestamp,
            payload: ExecutionEventPayload::Finished { outcome },
        })
    }

    pub fn get_execution(&self, id: &str) -> Result<Execution> {
        self.connection()?
            .query_row(
                "SELECT id, workspace_id, state, command_json, cwd, env_keys_json,
                    initiator_json, created_at_ms, started_at_ms, ended_at_ms, pid, pgid,
                    outcome_json, captured_bytes, output_truncated, last_seq
                 FROM executions WHERE id = ?1",
                [id],
                execution_from_row,
            )
            .optional()?
            .ok_or_else(|| Error::ExecutionNotFound(id.into()))
    }

    pub fn list_executions(&self, workspace: Option<&str>, limit: u32) -> Result<Vec<Execution>> {
        let connection = self.connection()?;
        let sql = if workspace.is_some() {
            "SELECT e.id, e.workspace_id, e.state, e.command_json, e.cwd, e.env_keys_json,
                e.initiator_json, e.created_at_ms, e.started_at_ms, e.ended_at_ms, e.pid,
                e.pgid, e.outcome_json, e.captured_bytes, e.output_truncated, e.last_seq
             FROM executions e JOIN workspaces w ON w.id = e.workspace_id
             WHERE e.workspace_id = ?1 OR w.name = ?1 ORDER BY e.created_at_ms DESC LIMIT ?2"
        } else {
            "SELECT id, workspace_id, state, command_json, cwd, env_keys_json,
                initiator_json, created_at_ms, started_at_ms, ended_at_ms, pid, pgid,
                outcome_json, captured_bytes, output_truncated, last_seq
             FROM executions ORDER BY created_at_ms DESC LIMIT ?2"
        };
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map(params![workspace, limit], execution_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn execution_stats(
        &self,
        workspace: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<ExecutionStats> {
        if since_ms > until_ms {
            return Err(Error::InvalidRequest(
                "statistics window start must not exceed its end".into(),
            ));
        }
        let workspace = self.get_workspace(workspace)?;
        let connection = self.connection()?;
        let filter = params![workspace.id, since_ms, until_ms];
        let (total, status, captured_bytes, truncated_executions) = connection.query_row(
            "SELECT
                COUNT(*),
                COALESCE(SUM(state = 'queued'), 0),
                COALESCE(SUM(state = 'running'), 0),
                COALESCE(SUM(state = 'finished'
                    AND json_extract(outcome_json, '$.kind') = 'exited'
                    AND json_extract(outcome_json, '$.code') = 0), 0),
                COALESCE(SUM(state = 'finished'
                    AND json_extract(outcome_json, '$.kind') = 'exited'
                    AND json_extract(outcome_json, '$.code') != 0), 0),
                COALESCE(SUM(state = 'finished'
                    AND json_extract(outcome_json, '$.kind') = 'signaled'), 0),
                COALESCE(SUM(state = 'finished'
                    AND json_extract(outcome_json, '$.kind') = 'spawn_error'), 0),
                COALESCE(SUM(state = 'cancelled' OR (state = 'finished'
                    AND json_extract(outcome_json, '$.kind') = 'cancelled')), 0),
                COALESCE(SUM(state = 'interrupted' OR (state = 'finished'
                    AND json_extract(outcome_json, '$.kind') = 'interrupted')), 0),
                COALESCE(SUM(state = 'finished' AND outcome_json IS NULL), 0),
                COALESCE(SUM(captured_bytes), 0),
                COALESCE(SUM(output_truncated), 0)
             FROM executions
             WHERE workspace_id = ?1 AND created_at_ms >= ?2 AND created_at_ms <= ?3",
            filter,
            |row| {
                Ok((
                    row_u64(row, 0)?,
                    ExecutionStatusCounts {
                        queued: row_u64(row, 1)?,
                        running: row_u64(row, 2)?,
                        exited_zero: row_u64(row, 3)?,
                        exited_nonzero: row_u64(row, 4)?,
                        signaled: row_u64(row, 5)?,
                        spawn_error: row_u64(row, 6)?,
                        cancelled: row_u64(row, 7)?,
                        interrupted: row_u64(row, 8)?,
                        unknown_terminal: row_u64(row, 9)?,
                    },
                    row_u64(row, 10)?,
                    row_u64(row, 11)?,
                ))
            },
        )?;

        let mut initiator_statement = connection.prepare(
            "SELECT
                COALESCE(NULLIF(json_extract(initiator_json, '$.kind'), ''), 'unknown'),
                COUNT(*)
             FROM executions
             WHERE workspace_id = ?1 AND created_at_ms >= ?2 AND created_at_ms <= ?3
             GROUP BY 1 ORDER BY 1",
        )?;
        let by_initiator = initiator_statement
            .query_map(filter, |row| {
                Ok(InitiatorStats {
                    kind: row.get(0)?,
                    count: row_u64(row, 1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let (duration_samples, duration_p50_ms, duration_p95_ms) = connection.query_row(
            "WITH ranked AS (
                SELECT
                    CASE WHEN ended_at_ms >= started_at_ms
                        THEN ended_at_ms - started_at_ms ELSE 0 END AS duration_ms,
                    ROW_NUMBER() OVER (ORDER BY CASE WHEN ended_at_ms >= started_at_ms
                        THEN ended_at_ms - started_at_ms ELSE 0 END) AS rank_number,
                    COUNT(*) OVER () AS sample_count
                FROM executions
                WHERE workspace_id = ?1 AND created_at_ms >= ?2 AND created_at_ms <= ?3
                    AND state IN ('finished', 'cancelled', 'interrupted')
                    AND started_at_ms IS NOT NULL AND ended_at_ms IS NOT NULL
             )
             SELECT
                COALESCE(MAX(sample_count), 0),
                MAX(CASE WHEN rank_number = ((sample_count * 50 + 99) / 100)
                    THEN duration_ms END),
                MAX(CASE WHEN rank_number = ((sample_count * 95 + 99) / 100)
                    THEN duration_ms END)
             FROM ranked",
            filter,
            |row| {
                Ok((
                    row_u64(row, 0)?,
                    row.get::<_, Option<i64>>(1)?.map(|value| value as u64),
                    row.get::<_, Option<i64>>(2)?.map(|value| value as u64),
                ))
            },
        )?;

        Ok(ExecutionStats {
            workspace,
            since_ms,
            until_ms,
            total,
            status,
            by_initiator,
            captured_bytes,
            truncated_executions,
            duration_samples,
            duration_p50_ms,
            duration_p95_ms,
        })
    }

    pub fn read_output(
        &self,
        id: &str,
        after_seq: u64,
        max_bytes: usize,
    ) -> Result<ReadOutputResponse> {
        let execution = self.get_execution(id)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT seq, timestamp_ms, kind, stream, data, payload_json
             FROM events WHERE execution_id = ?1 AND seq > ?2 ORDER BY seq LIMIT 4096",
        )?;
        let mut rows = statement.query(params![id, after_seq as i64])?;
        let mut events = Vec::new();
        let mut bytes = 0usize;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            let data: Option<Vec<u8>> = row.get(4)?;
            let data_len = data.as_ref().map_or(0, Vec::len);
            if !events.is_empty() && bytes.saturating_add(data_len) > max_bytes {
                has_more = true;
                break;
            }
            bytes = bytes.saturating_add(data_len);
            events.push(event_from_row(id, row, data)?);
        }
        let next_seq = events.last().map_or(after_seq, |event| event.seq);
        has_more |= next_seq < execution.last_seq;
        Ok(ReadOutputResponse {
            execution,
            events,
            next_seq,
            has_more,
        })
    }

    pub fn reconcile_incomplete(&self) -> Result<usize> {
        let ids = {
            let connection = self.connection()?;
            let mut statement = connection
                .prepare("SELECT id FROM executions WHERE state IN ('queued', 'running')")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut count = 0;
        for id in ids {
            self.finish(
                &id,
                ExecutionState::Interrupted,
                ExecutionOutcome::Interrupted {
                    reason: "daemon restarted before the execution completed".into(),
                },
            )?;
            count += 1;
        }
        Ok(count)
    }

    pub fn cancel_queued(&self) -> Result<Vec<ExecutionEvent>> {
        let ids = {
            let connection = self.connection()?;
            let mut statement =
                connection.prepare("SELECT id FROM executions WHERE state = 'queued'")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        ids.into_iter()
            .map(|id| {
                self.finish(
                    &id,
                    ExecutionState::Cancelled,
                    ExecutionOutcome::Cancelled { signal: None },
                )
            })
            .collect()
    }

    pub fn prune(&self, retention_days: u64, retention_bytes: u64) -> Result<usize> {
        let cutoff = now_ms() - (retention_days as i64 * 24 * 60 * 60 * 1000);
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mut removed = transaction.execute(
            "DELETE FROM executions
             WHERE state IN ('finished', 'cancelled', 'interrupted') AND ended_at_ms < ?1",
            [cutoff],
        )?;
        let total = transaction.query_row(
            "SELECT COALESCE(SUM(captured_bytes), 0) FROM executions",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;
        if total > retention_bytes {
            let mut excess = total - retention_bytes;
            let candidates = {
                let mut statement = transaction.prepare(
                    "SELECT id, captured_bytes FROM executions
                     WHERE state IN ('finished', 'cancelled', 'interrupted')
                     ORDER BY ended_at_ms ASC",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };
            for (id, size) in candidates {
                if excess == 0 {
                    break;
                }
                removed += transaction.execute("DELETE FROM executions WHERE id = ?1", [&id])?;
                excess = excess.saturating_sub(size);
            }
        }
        transaction.commit()?;
        Ok(removed)
    }
}

fn run_store_actor(database: Database, mut receiver: mpsc::Receiver<StoreCommand>) {
    while let Some(command) = receiver.blocking_recv() {
        match command {
            StoreCommand::AddWorkspace { name, root, reply } => {
                let _ = reply.send(database.add_workspace(&name, &root));
            }
            StoreCommand::RemoveWorkspace { identifier, reply } => {
                let _ = reply.send(database.remove_workspace(&identifier));
            }
            StoreCommand::GetWorkspace { identifier, reply } => {
                let _ = reply.send(database.get_workspace(&identifier));
            }
            StoreCommand::FindWorkspaceByRoot { root, reply } => {
                let _ = reply.send(database.find_workspace_by_root(&root));
            }
            StoreCommand::ListWorkspaces { reply } => {
                let _ = reply.send(database.list_workspaces());
            }
            StoreCommand::CreateExecution {
                request,
                cwd,
                reply,
            } => {
                let _ = reply.send(database.create_execution(&request, &cwd));
            }
            StoreCommand::MarkRunning {
                id,
                pid,
                pgid,
                reply,
            } => {
                let _ = reply.send(database.mark_running(&id, pid, pgid));
            }
            StoreCommand::AppendCaptureBatch { records, reply } => {
                let _ = reply.send(database.append_capture_batch(&records));
            }
            StoreCommand::Finish {
                id,
                state,
                outcome,
                reply,
            } => {
                let _ = reply.send(database.finish(&id, state, outcome));
            }
            StoreCommand::GetExecution { id, reply } => {
                let _ = reply.send(database.get_execution(&id));
            }
            StoreCommand::ListExecutions {
                workspace,
                limit,
                reply,
            } => {
                let _ = reply.send(database.list_executions(workspace.as_deref(), limit));
            }
            StoreCommand::ExecutionStats {
                workspace,
                since_ms,
                until_ms,
                reply,
            } => {
                let _ = reply.send(database.execution_stats(&workspace, since_ms, until_ms));
            }
            StoreCommand::ReadOutput {
                id,
                after_seq,
                max_bytes,
                reply,
            } => {
                let _ = reply.send(database.read_output(&id, after_seq, max_bytes));
            }
            StoreCommand::ReconcileIncomplete { reply } => {
                let _ = reply.send(database.reconcile_incomplete());
            }
            StoreCommand::CancelQueued { reply } => {
                let _ = reply.send(database.cancel_queued());
            }
            StoreCommand::Prune {
                retention_days,
                retention_bytes,
                reply,
            } => {
                let _ = reply.send(database.prune(retention_days, retention_bytes));
            }
            StoreCommand::Shutdown { reply } => {
                let _ = reply.send(Ok(()));
                break;
            }
        }
    }
}

fn next_seq(transaction: &rusqlite::Transaction<'_>, id: &str) -> Result<u64> {
    let value: i64 = transaction
        .query_row(
            "SELECT last_seq + 1 FROM executions WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| Error::ExecutionNotFound(id.into()))?;
    Ok(value as u64)
}

fn workspace_from_row(row: &Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: row.get(0)?,
        name: row.get(1)?,
        root: row.get(2)?,
        created_at_ms: row.get(3)?,
    })
}

fn workspace_with_active_from_row(row: &Row<'_>) -> rusqlite::Result<(Workspace, bool)> {
    Ok((workspace_from_row(row)?, row.get::<_, i64>(4)? != 0))
}

fn row_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    row.get::<_, i64>(index).map(|value| value.max(0) as u64)
}

fn execution_from_row(row: &Row<'_>) -> rusqlite::Result<Execution> {
    let state: String = row.get(2)?;
    let command_json: String = row.get(3)?;
    let env_keys_json: String = row.get(5)?;
    let initiator_json: String = row.get(6)?;
    let outcome_json: Option<String> = row.get(12)?;
    let command: CommandSpec = json_column(3, &command_json)?;
    let started_at_ms: Option<i64> = row.get(8)?;
    let ended_at_ms: Option<i64> = row.get(9)?;
    let duration_ms = started_at_ms.map(|started| {
        ended_at_ms
            .unwrap_or_else(now_ms)
            .saturating_sub(started)
            .max(0) as u64
    });
    Ok(Execution {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        state: parse_state(2, &state)?,
        command_display: command.display(),
        command,
        cwd: row.get(4)?,
        env_keys: json_column(5, &env_keys_json)?,
        initiator: json_column(6, &initiator_json)?,
        created_at_ms: row.get(7)?,
        started_at_ms,
        ended_at_ms,
        duration_ms,
        pid: row.get(10)?,
        pgid: row.get(11)?,
        outcome: outcome_json
            .map(|value| json_column(12, &value))
            .transpose()?,
        captured_bytes: row.get::<_, i64>(13)? as u64,
        output_truncated: row.get::<_, i64>(14)? != 0,
        last_seq: row.get::<_, i64>(15)? as u64,
    })
}

fn json_column<T: serde::de::DeserializeOwned>(index: usize, value: &str) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn parse_state(index: usize, value: &str) -> rusqlite::Result<ExecutionState> {
    match value {
        "queued" => Ok(ExecutionState::Queued),
        "running" => Ok(ExecutionState::Running),
        "finished" => Ok(ExecutionState::Finished),
        "cancelled" => Ok(ExecutionState::Cancelled),
        "interrupted" => Ok(ExecutionState::Interrupted),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            format!("unknown execution state {value}").into(),
        )),
    }
}

fn output_event(
    id: &str,
    seq: u64,
    timestamp_ms: i64,
    stream: OutputStream,
    data: &[u8],
) -> ExecutionEvent {
    ExecutionEvent {
        execution_id: id.into(),
        seq,
        timestamp_ms,
        payload: ExecutionEventPayload::Output {
            stream,
            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
        },
    }
}

fn event_from_row(
    id: &str,
    row: &Row<'_>,
    data: Option<Vec<u8>>,
) -> rusqlite::Result<ExecutionEvent> {
    let seq = row.get::<_, i64>(0)? as u64;
    let timestamp_ms: i64 = row.get(1)?;
    let kind: String = row.get(2)?;
    let stream: Option<String> = row.get(3)?;
    let payload_json: Option<String> = row.get(5)?;
    let payload = match kind.as_str() {
        "started" => {
            #[derive(serde::Deserialize)]
            struct Started {
                pid: u32,
                pgid: i32,
            }
            let started: Started = json_column(5, payload_json.as_deref().unwrap_or("{}"))?;
            ExecutionEventPayload::Started {
                pid: started.pid,
                pgid: started.pgid,
            }
        }
        "output" => {
            let stream = match stream.as_deref() {
                Some("stdout") => OutputStream::Stdout,
                Some("stderr") => OutputStream::Stderr,
                value => {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        format!("invalid output stream {value:?}").into(),
                    ));
                }
            };
            let data = data.unwrap_or_default();
            return Ok(output_event(id, seq, timestamp_ms, stream, &data));
        }
        "capture_truncated" => {
            #[derive(serde::Deserialize)]
            struct Truncated {
                limit_bytes: u64,
            }
            let value: Truncated = json_column(5, payload_json.as_deref().unwrap_or("{}"))?;
            ExecutionEventPayload::CaptureTruncated {
                limit_bytes: value.limit_bytes,
            }
        }
        "finished" => ExecutionEventPayload::Finished {
            outcome: json_column(5, payload_json.as_deref().unwrap_or("{}"))?,
        },
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                format!("unknown event kind {kind}").into(),
            ));
        }
    };
    Ok(ExecutionEvent {
        execution_id: id.into(),
        seq,
        timestamp_ms,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use super::*;
    use crate::model::Initiator;

    fn request(workspace_id: String) -> ExecutionRequest {
        ExecutionRequest {
            workspace_id,
            cwd: None,
            command: CommandSpec::Argv {
                program: "printf".into(),
                args: vec!["hello".into()],
            },
            env: BTreeMap::new(),
            stdin_base64: None,
            initiator: Initiator::default(),
            capture_limit_bytes: None,
        }
    }

    fn insert_execution(
        database: &Database,
        workspace_id: &str,
        id: &str,
        state: ExecutionState,
        outcome: Option<ExecutionOutcome>,
        initiator: &str,
        duration_ms: Option<u64>,
    ) {
        let (started_at_ms, ended_at_ms) = duration_ms
            .map(|duration| (Some(1_000), Some(1_000 + duration as i64)))
            .unwrap_or((None, None));
        database
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO executions(
                    id, workspace_id, state, command_json, cwd, env_keys_json,
                    initiator_json, created_at_ms, started_at_ms, ended_at_ms,
                    outcome_json, captured_bytes, output_truncated
                 ) VALUES (?1, ?2, ?3, 'not-json', '/tmp/test', '[]', ?4, 100, ?5, ?6, ?7, 10, ?8)",
                params![
                    id,
                    workspace_id,
                    state.as_str(),
                    serde_json::to_string(&Initiator {
                        kind: initiator.into(),
                        name: None,
                        session_id: None,
                    })
                    .unwrap(),
                    started_at_ms,
                    ended_at_ms,
                    outcome.map(|value| serde_json::to_string(&value).unwrap()),
                    i64::from(id == "spawn-error"),
                ],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn stores_execution_and_lossless_output() {
        let store = Store::in_memory().unwrap();
        let root = tempdir().unwrap();
        let workspace = store.add_workspace("test", root.path()).await.unwrap();
        let execution = store
            .create_execution(&request(workspace.id), root.path())
            .await
            .unwrap();
        store.mark_running(&execution.id, 42, 42).await.unwrap();
        store
            .append_output(&execution.id, OutputStream::Stdout, b"hello\xff")
            .await
            .unwrap();
        store
            .finish(
                &execution.id,
                ExecutionState::Finished,
                ExecutionOutcome::Exited { code: 0 },
            )
            .await
            .unwrap();

        let output = store.read_output(&execution.id, 0, 1024).await.unwrap();
        assert_eq!(output.events.len(), 3);
        assert!(output.execution.state.is_terminal());
        assert_eq!(output.execution.captured_bytes, 6);
    }

    #[tokio::test]
    async fn reconciles_incomplete_records_and_prunes_only_terminal_history() {
        let store = Store::in_memory().unwrap();
        let root = tempdir().unwrap();
        let workspace = store.add_workspace("test", root.path()).await.unwrap();
        let running = store
            .create_execution(&request(workspace.id.clone()), root.path())
            .await
            .unwrap();
        store.mark_running(&running.id, 42, 42).await.unwrap();
        let queued = store
            .create_execution(&request(workspace.id), root.path())
            .await
            .unwrap();

        assert_eq!(store.reconcile_incomplete().await.unwrap(), 2);
        assert_eq!(
            store.get_execution(&running.id).await.unwrap().state,
            ExecutionState::Interrupted
        );
        assert_eq!(
            store.get_execution(&queued.id).await.unwrap().state,
            ExecutionState::Interrupted
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(store.prune(0, u64::MAX).await.unwrap(), 2);
        assert!(matches!(
            store.get_execution(&running.id).await,
            Err(Error::ExecutionNotFound(_))
        ));
    }

    #[test]
    fn builds_empty_execution_stats() {
        let database = Database::open(StoreLocation::Memory).unwrap();
        let root = tempdir().unwrap();
        let workspace = database.add_workspace("test", root.path()).unwrap();
        let stats = database.execution_stats(&workspace.id, 100, 200).unwrap();

        assert_eq!(stats.total, 0);
        assert_eq!(stats.status, ExecutionStatusCounts::default());
        assert!(stats.by_initiator.is_empty());
        assert_eq!(stats.duration_p50_ms, None);
        assert_eq!(stats.duration_p95_ms, None);
    }

    #[test]
    fn classifies_every_execution_and_sorts_initiators() {
        let database = Database::open(StoreLocation::Memory).unwrap();
        let root = tempdir().unwrap();
        let workspace = database.add_workspace("test", root.path()).unwrap();
        let records = [
            ("queued", ExecutionState::Queued, None, "mcp", None),
            ("running", ExecutionState::Running, None, "cli", None),
            (
                "success",
                ExecutionState::Finished,
                Some(ExecutionOutcome::Exited { code: 0 }),
                "mcp",
                Some(10),
            ),
            (
                "failure",
                ExecutionState::Finished,
                Some(ExecutionOutcome::Exited { code: 2 }),
                "cli",
                Some(20),
            ),
            (
                "signaled",
                ExecutionState::Finished,
                Some(ExecutionOutcome::Signaled { signal: 9 }),
                "cli",
                Some(30),
            ),
            (
                "spawn-error",
                ExecutionState::Finished,
                Some(ExecutionOutcome::SpawnError {
                    message: "missing".into(),
                }),
                "cli",
                Some(40),
            ),
            (
                "cancelled",
                ExecutionState::Cancelled,
                Some(ExecutionOutcome::Cancelled { signal: Some(15) }),
                "cli",
                Some(50),
            ),
            (
                "interrupted",
                ExecutionState::Interrupted,
                Some(ExecutionOutcome::Interrupted {
                    reason: "restart".into(),
                }),
                "cli",
                Some(60),
            ),
            ("unknown", ExecutionState::Finished, None, "cli", Some(70)),
        ];
        for (id, state, outcome, initiator, duration) in records {
            insert_execution(
                &database,
                &workspace.id,
                id,
                state,
                outcome,
                initiator,
                duration,
            );
        }

        let stats = database.execution_stats(&workspace.id, 0, 200).unwrap();

        assert_eq!(stats.total, 9);
        assert_eq!(stats.status.queued, 1);
        assert_eq!(stats.status.running, 1);
        assert_eq!(stats.status.exited_zero, 1);
        assert_eq!(stats.status.exited_nonzero, 1);
        assert_eq!(stats.status.signaled, 1);
        assert_eq!(stats.status.spawn_error, 1);
        assert_eq!(stats.status.cancelled, 1);
        assert_eq!(stats.status.interrupted, 1);
        assert_eq!(stats.status.unknown_terminal, 1);
        assert_eq!(
            stats.by_initiator,
            vec![
                InitiatorStats {
                    kind: "cli".into(),
                    count: 7,
                },
                InitiatorStats {
                    kind: "mcp".into(),
                    count: 2,
                },
            ]
        );
        assert_eq!(stats.captured_bytes, 90);
        assert_eq!(stats.truncated_executions, 1);
        assert_eq!(stats.duration_samples, 7);
        assert_eq!(stats.duration_p50_ms, Some(40));
        assert_eq!(stats.duration_p95_ms, Some(70));
    }

    #[test]
    fn nearest_rank_handles_boundaries() {
        let database = Database::open(StoreLocation::Memory).unwrap();
        let root = tempdir().unwrap();
        let workspace = database.add_workspace("test", root.path()).unwrap();
        for (id, duration) in [("first", 10), ("second", 20)] {
            insert_execution(
                &database,
                &workspace.id,
                id,
                ExecutionState::Finished,
                Some(ExecutionOutcome::Exited { code: 0 }),
                "cli",
                Some(duration),
            );
        }

        let stats = database.execution_stats(&workspace.id, 0, 200).unwrap();
        assert_eq!(stats.duration_p50_ms, Some(10));
        assert_eq!(stats.duration_p95_ms, Some(20));
    }

    #[test]
    fn workspace_registration_is_idempotent_and_preserves_history() {
        let database = Database::open(StoreLocation::Memory).unwrap();
        let root = tempdir().unwrap();
        let other_root = tempdir().unwrap();
        let workspace = database.add_workspace("test", root.path()).unwrap();
        let duplicate = database.add_workspace("test", root.path()).unwrap();
        assert_eq!(duplicate.id, workspace.id);
        let execution = database
            .create_execution(&request(workspace.id.clone()), root.path())
            .unwrap();

        database.remove_workspace("test").unwrap();
        database.remove_workspace("test").unwrap();
        assert!(database.list_workspaces().unwrap().is_empty());
        assert!(matches!(
            database.get_workspace("test"),
            Err(Error::WorkspaceNotFound(_))
        ));
        assert_eq!(
            database.get_execution(&execution.id).unwrap().id,
            execution.id
        );

        let reactivated = database.add_workspace("test", root.path()).unwrap();
        assert_eq!(reactivated.id, workspace.id);
        assert_eq!(database.list_workspaces().unwrap().len(), 1);
        assert!(matches!(
            database.add_workspace("renamed", root.path()),
            Err(Error::InvalidRequest(_))
        ));
        assert!(matches!(
            database.add_workspace("test", other_root.path()),
            Err(Error::InvalidRequest(_))
        ));
    }

    #[test]
    fn migrates_v1_workspaces_as_active() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("legacy.db");
        let root = temp.path().canonicalize().unwrap();
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE workspaces (
                        id TEXT PRIMARY KEY,
                        name TEXT NOT NULL UNIQUE,
                        root TEXT NOT NULL UNIQUE,
                        created_at_ms INTEGER NOT NULL
                     );
                     CREATE TABLE executions (
                        id TEXT PRIMARY KEY,
                        workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                        state TEXT NOT NULL,
                        command_json TEXT NOT NULL,
                        cwd TEXT NOT NULL,
                        env_keys_json TEXT NOT NULL,
                        initiator_json TEXT NOT NULL,
                        created_at_ms INTEGER NOT NULL,
                        started_at_ms INTEGER,
                        ended_at_ms INTEGER,
                        pid INTEGER,
                        pgid INTEGER,
                        outcome_json TEXT,
                        captured_bytes INTEGER NOT NULL DEFAULT 0,
                        output_truncated INTEGER NOT NULL DEFAULT 0,
                        last_seq INTEGER NOT NULL DEFAULT 0
                     );
                     PRAGMA user_version = 1;",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO workspaces(id, name, root, created_at_ms)
                     VALUES ('legacy-id', 'legacy', ?1, 1)",
                    [root.to_string_lossy().as_ref()],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO executions(
                        id, workspace_id, state, command_json, cwd, env_keys_json,
                        initiator_json, created_at_ms, started_at_ms, ended_at_ms,
                        outcome_json, captured_bytes
                     ) VALUES (
                        'legacy-execution', 'legacy-id', 'finished',
                        '{\"kind\":\"argv\",\"program\":\"true\",\"args\":[]}',
                        ?1, '[]', '{\"kind\":\"cli\"}', 100, 100, 110,
                        '{\"kind\":\"exited\",\"code\":0}', 4
                     )",
                    [root.to_string_lossy().as_ref()],
                )
                .unwrap();
        }

        let database = Database::open(StoreLocation::Path(path)).unwrap();
        assert_eq!(database.list_workspaces().unwrap()[0].id, "legacy-id");
        assert_eq!(
            database.get_execution("legacy-execution").unwrap().id,
            "legacy-execution"
        );
        assert_eq!(database.execution_stats("legacy", 0, 200).unwrap().total, 1);
        assert_eq!(
            database
                .connection()
                .unwrap()
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn database_stats_filter_by_workspace_and_window() {
        let database = Database::open(StoreLocation::Memory).unwrap();
        let first_root = tempdir().unwrap();
        let second_root = tempdir().unwrap();
        let first = database.add_workspace("first", first_root.path()).unwrap();
        let second = database
            .add_workspace("second", second_root.path())
            .unwrap();
        let first_execution = database
            .create_execution(&request(first.id.clone()), first_root.path())
            .unwrap();
        let second_execution = database
            .create_execution(&request(second.id), second_root.path())
            .unwrap();
        database
            .connection()
            .unwrap()
            .execute(
                "UPDATE executions
                 SET created_at_ms = CASE id WHEN ?1 THEN 100 WHEN ?2 THEN 200 ELSE created_at_ms END
                 WHERE id IN (?1, ?2)",
                params![first_execution.id, second_execution.id],
            )
            .unwrap();

        let stats = database.execution_stats(&first.id, 50, 150).unwrap();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.workspace.id, first.id);
        assert_eq!(
            database.execution_stats("first", 101, 300).unwrap().total,
            0
        );
    }

    #[tokio::test]
    async fn rejects_reversed_stats_window() {
        let store = Store::in_memory().unwrap();
        assert!(matches!(
            store.execution_stats("test", 200, 100).await,
            Err(Error::InvalidRequest(_))
        ));
    }
}
