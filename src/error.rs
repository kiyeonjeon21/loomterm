use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("storage unavailable: {0}")]
    StorageUnavailable(String),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),
    #[error("execution not found: {0}")]
    ExecutionNotFound(String),
    #[error("path {path:?} is outside workspace {workspace:?}")]
    OutsideWorkspace { path: PathBuf, workspace: PathBuf },
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("daemon is unavailable at {0:?}")]
    DaemonUnavailable(PathBuf),
    #[error("daemon is draining and cannot accept new executions")]
    DaemonDraining,
    #[error("operation timed out")]
    Timeout,
    #[error("execution is already terminal: {0}")]
    AlreadyTerminal(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
