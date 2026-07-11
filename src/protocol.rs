use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::model::{
    Execution, ExecutionRequest, Health, PROTOCOL_VERSION, ReadOutputResponse, WaitResponse,
    Workspace,
};

pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolRequest {
    pub version: u32,
    pub request_id: String,
    #[serde(flatten)]
    pub operation: Operation,
}

impl ProtocolRequest {
    pub fn new(operation: Operation) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: crate::model::new_id(),
            operation,
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
    Cancel {
        execution_id: String,
    },
    Shutdown,
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
pub struct ProtocolResponse {
    pub version: u32,
    pub request_id: String,
    #[serde(flatten)]
    pub body: ResponseBody,
}

impl ProtocolResponse {
    pub fn ok(request_id: String, result: ProtocolResult) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            body: ResponseBody::Ok {
                result: Box::new(result),
            },
        }
    }

    pub fn error(request_id: String, error: &Error) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            body: ResponseBody::Error {
                error: ProtocolError::from(error),
            },
        }
    }
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
    Output(ReadOutputResponse),
    Wait(WaitResponse),
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
            Error::OutsideWorkspace { .. } => "outside_workspace",
            Error::InvalidRequest(_) => "invalid_request",
            Error::AlreadyTerminal(_) => "already_terminal",
            Error::PermissionDenied(_) => "permission_denied",
            Error::Timeout => "timeout",
            Error::Config(_) => "configuration_error",
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
