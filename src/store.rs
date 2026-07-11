use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::{
    CommandSpec, Execution, ExecutionEvent, ExecutionEventPayload, ExecutionOutcome,
    ExecutionRequest, ExecutionState, OutputStream, ReadOutputResponse, Workspace, new_id, now_ms,
};
use crate::{Error, Result};

const SCHEMA_VERSION: i64 = 1;

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

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        store.initialize()?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let store = Self {
            connection: Arc::new(Mutex::new(Connection::open_in_memory()?)),
        };
        store.initialize()?;
        Ok(store)
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
                 PRAGMA user_version = 1;
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
        let workspace = Workspace {
            id: new_id(),
            name: name.to_owned(),
            root: root.to_string_lossy().into_owned(),
            created_at_ms: now_ms(),
        };
        self.connection()?.execute(
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
        let changed = self.connection()?.execute(
            "DELETE FROM workspaces WHERE id = ?1 OR name = ?1",
            [identifier],
        )?;
        if changed == 0 {
            return Err(Error::WorkspaceNotFound(identifier.into()));
        }
        Ok(())
    }

    pub fn get_workspace(&self, identifier: &str) -> Result<Workspace> {
        self.connection()?
            .query_row(
                "SELECT id, name, root, created_at_ms
                 FROM workspaces WHERE id = ?1 OR name = ?1",
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
                "SELECT id, name, root, created_at_ms FROM workspaces WHERE root = ?1",
                [canonical.to_string_lossy().as_ref()],
                workspace_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, name, root, created_at_ms FROM workspaces ORDER BY name COLLATE NOCASE",
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

    pub fn append_output(
        &self,
        id: &str,
        stream: OutputStream,
        data: &[u8],
    ) -> Result<ExecutionEvent> {
        self.append_capture_batch(&[CaptureRecord::Output {
            execution_id: id.into(),
            timestamp_ms: now_ms(),
            stream,
            data: data.to_vec(),
        }])?
        .pop()
        .ok_or_else(|| Error::Protocol("capture batch produced no event".into()))
    }

    pub fn mark_truncated(&self, id: &str, limit: u64) -> Result<ExecutionEvent> {
        self.append_capture_batch(&[CaptureRecord::Truncated {
            execution_id: id.into(),
            timestamp_ms: now_ms(),
            limit_bytes: limit,
        }])?
        .pop()
        .ok_or_else(|| Error::Protocol("capture batch produced no event".into()))
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
            text: String::from_utf8_lossy(data).into_owned(),
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

    #[test]
    fn stores_execution_and_lossless_output() {
        let store = Store::in_memory().unwrap();
        let root = tempdir().unwrap();
        let workspace = store.add_workspace("test", root.path()).unwrap();
        let execution = store
            .create_execution(&request(workspace.id), root.path())
            .unwrap();
        store.mark_running(&execution.id, 42, 42).unwrap();
        store
            .append_output(&execution.id, OutputStream::Stdout, b"hello\xff")
            .unwrap();
        store
            .finish(
                &execution.id,
                ExecutionState::Finished,
                ExecutionOutcome::Exited { code: 0 },
            )
            .unwrap();

        let output = store.read_output(&execution.id, 0, 1024).unwrap();
        assert_eq!(output.events.len(), 3);
        assert!(output.execution.state.is_terminal());
        assert_eq!(output.execution.captured_bytes, 6);
    }

    #[test]
    fn reconciles_incomplete_records_and_prunes_only_terminal_history() {
        let store = Store::in_memory().unwrap();
        let root = tempdir().unwrap();
        let workspace = store.add_workspace("test", root.path()).unwrap();
        let running = store
            .create_execution(&request(workspace.id.clone()), root.path())
            .unwrap();
        store.mark_running(&running.id, 42, 42).unwrap();
        let queued = store
            .create_execution(&request(workspace.id), root.path())
            .unwrap();

        assert_eq!(store.reconcile_incomplete().unwrap(), 2);
        assert_eq!(
            store.get_execution(&running.id).unwrap().state,
            ExecutionState::Interrupted
        );
        assert_eq!(
            store.get_execution(&queued.id).unwrap().state,
            ExecutionState::Interrupted
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(store.prune(0, u64::MAX).unwrap(), 2);
        assert!(matches!(
            store.get_execution(&running.id),
            Err(Error::ExecutionNotFound(_))
        ));
    }
}
