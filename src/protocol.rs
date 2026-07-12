use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::model::{
    AgentEventRequest, AgentSession, AgentSessionDetail, AgentSessionFinish, AgentSessionRequest,
    AgentTurn, Execution, ExecutionEvent, ExecutionRequest, ExecutionStats, Health,
    PROTOCOL_VERSION, ReadOutputResponse, WaitResponse, Workspace,
};

pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const CAPABILITY_EXECUTION_STATS: &str = "execution_stats_v1";
pub const CAPABILITY_AGENT_SESSIONS: &str = "agent_sessions_v1";
pub const CAPABILITY_AGENT_TURNS: &str = "agent_turns_v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireMessage {
    Request {
        version: u32,
        request_id: String,
        #[serde(flatten)]
        operation: Operation,
    },
    Response {
        version: u32,
        request_id: String,
        #[serde(flatten)]
        body: ResponseBody,
    },
    Event {
        version: u32,
        subscription_id: String,
        event: ExecutionEvent,
    },
}

impl WireMessage {
    pub fn request(operation: Operation) -> Self {
        Self::Request {
            version: PROTOCOL_VERSION,
            request_id: crate::model::new_id(),
            operation,
        }
    }

    pub fn ok(request_id: String, result: ProtocolResult) -> Self {
        Self::Response {
            version: PROTOCOL_VERSION,
            request_id,
            body: ResponseBody::Ok {
                result: Box::new(result),
            },
        }
    }

    pub fn error(request_id: String, error: &Error) -> Self {
        Self::Response {
            version: PROTOCOL_VERSION,
            request_id,
            body: ResponseBody::Error {
                error: ProtocolError::from(error),
            },
        }
    }

    pub fn event(subscription_id: String, event: ExecutionEvent) -> Self {
        Self::Event {
            version: PROTOCOL_VERSION,
            subscription_id,
            event,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Operation {
    Health,
    WorkspaceAdd {
        name: String,
        root: String,
    },
    WorkspaceRemove {
        workspace: String,
    },
    WorkspaceList,
    Execute {
        request: ExecutionRequest,
    },
    Get {
        execution_id: String,
    },
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default = "default_list_limit")]
        limit: u32,
    },
    Stats {
        workspace: String,
        since_ms: i64,
    },
    SessionCreate {
        request: AgentSessionRequest,
    },
    SessionFinish {
        session_id: String,
        finish: AgentSessionFinish,
    },
    SessionGet {
        session_id: String,
    },
    SessionList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default = "default_list_limit")]
        limit: u32,
    },
    SessionDelete {
        session_id: String,
    },
    AgentEvent {
        request: AgentEventRequest,
    },
    ReadOutput {
        execution_id: String,
        #[serde(default)]
        after_seq: u64,
        #[serde(default = "default_read_bytes")]
        max_bytes: usize,
    },
    Wait {
        execution_id: String,
        #[serde(default)]
        after_seq: u64,
        #[serde(default = "default_wait_ms")]
        timeout_ms: u64,
        #[serde(default = "default_read_bytes")]
        max_bytes: usize,
    },
    Subscribe {
        execution_id: String,
        #[serde(default)]
        after_seq: u64,
    },
    Cancel {
        execution_id: String,
    },
    Shutdown {
        #[serde(default)]
        force: bool,
    },
}

pub fn default_list_limit() -> u32 {
    100
}

pub fn default_read_bytes() -> usize {
    1024 * 1024
}

pub fn default_wait_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseBody {
    Ok { result: Box<ProtocolResult> },
    Error { error: ProtocolError },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ProtocolResult {
    Empty,
    Health(Health),
    Workspace(Workspace),
    Workspaces(Vec<Workspace>),
    Execution(Execution),
    Executions(Vec<Execution>),
    Stats(ExecutionStats),
    AgentSession(AgentSession),
    AgentSessions(Vec<AgentSession>),
    AgentSessionDetail(AgentSessionDetail),
    AgentTurn(AgentTurn),
    Output(ReadOutputResponse),
    Wait(WaitResponse),
    Subscription(SubscriptionResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionResponse {
    pub execution: Execution,
    pub next_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
}

impl ProtocolError {
    pub fn into_error(self) -> Error {
        Error::Protocol(format!("{}: {}", self.code, self.message))
    }
}

impl From<&Error> for ProtocolError {
    fn from(error: &Error) -> Self {
        let code = match error {
            Error::WorkspaceNotFound(_) => "workspace_not_found",
            Error::ExecutionNotFound(_) => "execution_not_found",
            Error::AgentSessionNotFound(_) => "agent_session_not_found",
            Error::OutsideWorkspace { .. } => "outside_workspace",
            Error::InvalidRequest(_) => "invalid_request",
            Error::AlreadyTerminal(_) => "already_terminal",
            Error::PermissionDenied(_) => "permission_denied",
            Error::ProtocolVersionMismatch { .. } => "protocol_version_mismatch",
            Error::DaemonUpgradeRequired { .. } => "daemon_upgrade_required",
            Error::DaemonDraining => "daemon_draining",
            Error::Timeout => "timeout",
            Error::Config(_) => "configuration_error",
            Error::StorageUnavailable(_) => "storage_unavailable",
            Error::Io(_)
            | Error::Database(_)
            | Error::Json(_)
            | Error::Protocol(_)
            | Error::DaemonUnavailable(_) => "internal_error",
        };
        Self {
            code: code.into(),
            message: error.to_string(),
        }
    }
}
