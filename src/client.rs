use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::config::AppPaths;
use crate::model::{
    Execution, ExecutionRequest, Health, PROTOCOL_VERSION, ReadOutputResponse, WaitResponse,
    Workspace,
};
use crate::protocol::{
    MAX_FRAME_BYTES, Operation, ProtocolRequest, ProtocolResponse, ProtocolResult, ResponseBody,
};
use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct DaemonClient {
    socket: std::path::PathBuf,
}

impl DaemonClient {
    pub fn new(socket: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub async fn connect_or_start(paths: &AppPaths) -> Result<Self> {
        let client = Self::new(&paths.socket);
        if client.health().await.is_ok() {
            return Ok(client);
        }
        start_daemon_process()?;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if client.health().await.is_ok() {
                return Ok(client);
            }
        }
        Err(Error::DaemonUnavailable(paths.socket.clone()))
    }

    pub async fn call(&self, operation: Operation) -> Result<ProtocolResult> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|_| Error::DaemonUnavailable(self.socket.clone()))?;
        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(MAX_FRAME_BYTES)
            .new_codec();
        let mut framed = Framed::new(stream, codec);
        let request = ProtocolRequest::new(operation);
        let request_id = request.request_id.clone();
        let body = serde_json::to_vec(&request)?;
        framed.send(Bytes::from(body)).await?;
        let frame = framed
            .next()
            .await
            .ok_or_else(|| Error::Protocol("daemon closed the connection".into()))??;
        let response: ProtocolResponse = serde_json::from_slice(&frame)?;
        if response.version != PROTOCOL_VERSION {
            return Err(Error::Protocol(format!(
                "unsupported daemon protocol version {}",
                response.version
            )));
        }
        if response.request_id != request_id {
            return Err(Error::Protocol("response request_id did not match".into()));
        }
        match response.body {
            ResponseBody::Ok { result } => Ok(*result),
            ResponseBody::Error { error } => Err(error.into_error()),
        }
    }

    pub async fn health(&self) -> Result<Health> {
        expect_health(self.call(Operation::Health).await?)
    }

    pub async fn add_workspace(&self, name: String, root: String) -> Result<Workspace> {
        expect_workspace(self.call(Operation::WorkspaceAdd { name, root }).await?)
    }

    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        match self.call(Operation::WorkspaceList).await? {
            ProtocolResult::Workspaces(value) => Ok(value),
            value => unexpected("workspaces", value),
        }
    }

    pub async fn remove_workspace(&self, workspace: String) -> Result<()> {
        match self.call(Operation::WorkspaceRemove { workspace }).await? {
            ProtocolResult::Empty => Ok(()),
            value => unexpected("empty response", value),
        }
    }

    pub async fn execute(&self, request: ExecutionRequest) -> Result<Execution> {
        expect_execution(self.call(Operation::Execute { request }).await?)
    }

    pub async fn get(&self, execution_id: String) -> Result<Execution> {
        expect_execution(self.call(Operation::Get { execution_id }).await?)
    }

    pub async fn list(&self, workspace: Option<String>, limit: u32) -> Result<Vec<Execution>> {
        match self.call(Operation::List { workspace, limit }).await? {
            ProtocolResult::Executions(value) => Ok(value),
            value => unexpected("executions", value),
        }
    }

    pub async fn read_output(
        &self,
        execution_id: String,
        after_seq: u64,
        max_bytes: usize,
    ) -> Result<ReadOutputResponse> {
        match self
            .call(Operation::ReadOutput {
                execution_id,
                after_seq,
                max_bytes,
            })
            .await?
        {
            ProtocolResult::Output(value) => Ok(value),
            value => unexpected("output", value),
        }
    }

    pub async fn wait(
        &self,
        execution_id: String,
        after_seq: u64,
        timeout_ms: u64,
        max_bytes: usize,
    ) -> Result<WaitResponse> {
        match self
            .call(Operation::Wait {
                execution_id,
                after_seq,
                timeout_ms,
                max_bytes,
            })
            .await?
        {
            ProtocolResult::Wait(value) => Ok(value),
            value => unexpected("wait", value),
        }
    }

    pub async fn cancel(&self, execution_id: String) -> Result<Execution> {
        expect_execution(self.call(Operation::Cancel { execution_id }).await?)
    }

    pub async fn shutdown(&self) -> Result<()> {
        match self.call(Operation::Shutdown).await? {
            ProtocolResult::Empty => Ok(()),
            value => unexpected("empty response", value),
        }
    }
}

fn start_daemon_process() -> Result<()> {
    let current = std::env::current_exe()?;
    let sibling = current.with_file_name("loomd");
    let executable = if sibling.exists() {
        sibling
    } else {
        std::path::PathBuf::from("loomd")
    };
    let mut command = std::process::Command::new(executable);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    command.spawn().map_err(Error::Io)?;
    Ok(())
}

fn expect_health(result: ProtocolResult) -> Result<Health> {
    match result {
        ProtocolResult::Health(value) => Ok(value),
        value => unexpected("health", value),
    }
}

fn expect_workspace(result: ProtocolResult) -> Result<Workspace> {
    match result {
        ProtocolResult::Workspace(value) => Ok(value),
        value => unexpected("workspace", value),
    }
}

fn expect_execution(result: ProtocolResult) -> Result<Execution> {
    match result {
        ProtocolResult::Execution(value) => Ok(value),
        value => unexpected("execution", value),
    }
}

fn unexpected<T>(expected: &str, actual: ProtocolResult) -> Result<T> {
    Err(Error::Protocol(format!(
        "expected {expected} response, received {actual:?}"
    )))
}

pub fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
