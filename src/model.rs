use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, Result};

pub const PROTOCOL_VERSION: u32 = 2;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub fn new_id() -> String {
    Uuid::now_v7().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandSpec {
    Argv {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Shell {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<String>,
    },
}

impl CommandSpec {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Argv { program, .. } if program.is_empty() => {
                Err(Error::InvalidRequest("program must not be empty".into()))
            }
            Self::Shell { command, .. } if command.is_empty() => Err(Error::InvalidRequest(
                "shell command must not be empty".into(),
            )),
            _ => Ok(()),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Argv { program, args } => std::iter::once(program.as_str())
                .chain(args.iter().map(String::as_str))
                .map(shell_escape_for_display)
                .collect::<Vec<_>>()
                .join(" "),
            Self::Shell { command, .. } => command.clone(),
        }
    }
}

fn shell_escape_for_display(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-._/:=@+".contains(c))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Initiator {
    #[serde(default = "default_client_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

fn default_client_kind() -> String {
    "unknown".into()
}

impl Default for Initiator {
    fn default() -> Self {
        Self {
            kind: default_client_kind(),
            name: None,
            session_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionRequest {
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub command: CommandSpec,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_base64: Option<String>,
    #[serde(default)]
    pub initiator: Initiator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_limit_bytes: Option<u64>,
}

impl ExecutionRequest {
    pub fn validate(&self) -> Result<()> {
        if self.workspace_id.is_empty() {
            return Err(Error::InvalidRequest(
                "workspace_id must not be empty".into(),
            ));
        }
        self.command.validate()?;
        if let Some(stdin) = &self.stdin_base64 {
            base64::engine::general_purpose::STANDARD
                .decode(stdin)
                .map_err(|e| Error::InvalidRequest(format!("invalid stdin_base64: {e}")))?;
        }
        if self
            .env
            .keys()
            .any(|key| key.is_empty() || key.contains('='))
        {
            return Err(Error::InvalidRequest(
                "environment keys must be non-empty and must not contain '='".into(),
            ));
        }
        Ok(())
    }

    pub fn stdin_bytes(&self) -> Result<Option<Vec<u8>>> {
        self.stdin_base64
            .as_ref()
            .map(|value| {
                base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .map_err(|e| Error::InvalidRequest(format!("invalid stdin_base64: {e}")))
            })
            .transpose()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Queued,
    Running,
    Finished,
    Cancelled,
    Interrupted,
}

impl ExecutionState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished | Self::Cancelled | Self::Interrupted)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Exited { code: i32 },
    Signaled { signal: i32 },
    SpawnError { message: String },
    Cancelled { signal: Option<i32> },
    Interrupted { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Execution {
    pub id: String,
    pub workspace_id: String,
    pub state: ExecutionState,
    pub command: CommandSpec,
    pub command_display: String,
    pub cwd: String,
    pub env_keys: Vec<String>,
    pub initiator: Initiator,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub duration_ms: Option<u64>,
    pub pid: Option<u32>,
    pub pgid: Option<i32>,
    pub outcome: Option<ExecutionOutcome>,
    pub captured_bytes: u64,
    pub output_truncated: bool,
    pub last_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionState {
    Recording,
    Finished,
    Interrupted,
}

impl AgentSessionState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Recording)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Finished => "finished",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentSession {
    pub id: String,
    pub workspace_id: String,
    pub state: AgentSessionState,
    pub agent_kind: String,
    pub name: Option<String>,
    pub command: CommandSpec,
    pub command_display: String,
    pub cwd: String,
    pub created_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub duration_ms: Option<u64>,
    pub recorder_pid: u32,
    pub outcome: Option<ExecutionOutcome>,
    pub captured_bytes: u64,
    pub output_truncated: bool,
    pub initial_cols: u16,
    pub initial_rows: u16,
    pub cast_path: String,
    pub html_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentSessionRequest {
    pub id: String,
    pub workspace_id: String,
    pub agent_kind: String,
    pub name: Option<String>,
    pub command: CommandSpec,
    pub cwd: String,
    pub recorder_pid: u32,
    pub initial_cols: u16,
    pub initial_rows: u16,
    pub cast_path: String,
    pub html_path: String,
}

impl AgentSessionRequest {
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty() || self.workspace_id.is_empty() || self.agent_kind.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "id, workspace_id, and agent_kind must not be empty".into(),
            ));
        }
        if self.initial_cols == 0 || self.initial_rows == 0 {
            return Err(Error::InvalidRequest(
                "terminal dimensions must be greater than zero".into(),
            ));
        }
        self.command.validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentSessionFinish {
    pub state: AgentSessionState,
    pub outcome: ExecutionOutcome,
    pub captured_bytes: u64,
    pub output_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentSessionDetail {
    pub session: AgentSession,
    pub executions: Vec<Execution>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ExecutionStatusCounts {
    pub queued: u64,
    pub running: u64,
    pub exited_zero: u64,
    pub exited_nonzero: u64,
    pub signaled: u64,
    pub spawn_error: u64,
    pub cancelled: u64,
    pub interrupted: u64,
    pub unknown_terminal: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct InitiatorStats {
    pub kind: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ExecutionStats {
    pub workspace: Workspace,
    pub since_ms: i64,
    pub until_ms: i64,
    pub total: u64,
    pub status: ExecutionStatusCounts,
    pub by_initiator: Vec<InitiatorStats>,
    pub captured_bytes: u64,
    pub truncated_executions: u64,
    pub duration_samples: u64,
    pub duration_p50_ms: Option<u64>,
    pub duration_p95_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub root: String,
    pub created_at_ms: i64,
}

impl Workspace {
    pub fn root_path(&self) -> PathBuf {
        PathBuf::from(&self.root)
    }

    pub fn resolve_cwd(&self, requested: Option<&str>) -> Result<PathBuf> {
        let root = self.root_path();
        let candidate = match requested {
            Some(path) if Path::new(path).is_absolute() => PathBuf::from(path),
            Some(path) => root.join(path),
            None => root.clone(),
        };
        let canonical = candidate.canonicalize()?;
        if canonical == root || canonical.starts_with(&root) {
            Ok(canonical)
        } else {
            Err(Error::OutsideWorkspace {
                path: canonical,
                workspace: root,
            })
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionEventPayload {
    Started {
        pid: u32,
        pgid: i32,
    },
    Output {
        stream: OutputStream,
        data_base64: String,
    },
    CaptureTruncated {
        limit_bytes: u64,
    },
    Finished {
        outcome: ExecutionOutcome,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionEvent {
    pub execution_id: String,
    pub seq: u64,
    pub timestamp_ms: i64,
    #[serde(flatten)]
    pub payload: ExecutionEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadOutputResponse {
    pub execution: Execution,
    pub events: Vec<ExecutionEvent>,
    pub next_seq: u64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WaitResponse {
    pub execution: Execution,
    pub events: Vec<ExecutionEvent>,
    pub next_seq: u64,
    pub has_more: bool,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Health {
    pub protocol_version: u32,
    pub daemon_pid: u32,
    pub database_path: String,
    pub socket_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_executions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_sessions: Option<u64>,
}
