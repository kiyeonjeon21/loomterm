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
    MAX_FRAME_BYTES, Operation, ProtocolResult, ResponseBody, SubscriptionResponse, WireMessage,
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
        let mut framed = self.connect().await?;
        let request = WireMessage::request(operation);
        let request_id = match &request {
            WireMessage::Request { request_id, .. } => request_id.clone(),
            _ => unreachable!(),
        };
        let body = serde_json::to_vec(&request)?;
        framed.send(Bytes::from(body)).await?;
        let frame = framed
            .next()
            .await
            .ok_or_else(|| Error::Protocol("daemon closed the connection".into()))??;
        decode_response(&frame, &request_id)
    }

    async fn connect(&self) -> Result<Framed<UnixStream, LengthDelimitedCodec>> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|_| Error::DaemonUnavailable(self.socket.clone()))?;
        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(MAX_FRAME_BYTES)
            .new_codec();
        Ok(Framed::new(stream, codec))
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

    pub async fn subscribe(
        &self,
        execution_id: String,
        after_seq: u64,
    ) -> Result<ExecutionSubscription> {
        let mut framed = self.connect().await?;
        let request = WireMessage::request(Operation::Subscribe {
            execution_id,
            after_seq,
        });
        let request_id = match &request {
            WireMessage::Request { request_id, .. } => request_id.clone(),
            _ => unreachable!(),
        };
        framed
            .send(Bytes::from(serde_json::to_vec(&request)?))
            .await?;
        let frame = framed
            .next()
            .await
            .ok_or_else(|| Error::Protocol("daemon closed the subscription".into()))??;
        let response = decode_response(&frame, &request_id)?;
        let ProtocolResult::Subscription(SubscriptionResponse {
            execution,
            next_seq,
        }) = response
        else {
            return unexpected("subscription", response);
        };
        Ok(ExecutionSubscription {
            framed,
            subscription_id: request_id,
            execution,
            next_seq,
        })
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

pub struct ExecutionSubscription {
    framed: Framed<UnixStream, LengthDelimitedCodec>,
    subscription_id: String,
    pub execution: Execution,
    pub next_seq: u64,
}

impl ExecutionSubscription {
    pub async fn next_event(&mut self) -> Result<Option<crate::model::ExecutionEvent>> {
        let Some(frame) = self.framed.next().await else {
            return Ok(None);
        };
        let message: WireMessage = serde_json::from_slice(&frame?)?;
        match message {
            WireMessage::Event {
                version,
                subscription_id,
                event,
            } => {
                if version != PROTOCOL_VERSION {
                    return Err(Error::Protocol(format!(
                        "unsupported daemon protocol version {version}"
                    )));
                }
                if subscription_id != self.subscription_id {
                    return Err(Error::Protocol(
                        "subscription event id did not match".into(),
                    ));
                }
                if event.seq != self.next_seq + 1 {
                    return Err(Error::Protocol(format!(
                        "subscription sequence was not contiguous after {}: received {}",
                        self.next_seq, event.seq
                    )));
                }
                self.next_seq = event.seq;
                Ok(Some(event))
            }
            _ => Err(Error::Protocol(
                "expected subscription event from daemon".into(),
            )),
        }
    }
}

fn decode_response(frame: &[u8], expected_request_id: &str) -> Result<ProtocolResult> {
    let response: WireMessage = serde_json::from_slice(frame)?;
    let WireMessage::Response {
        version,
        request_id,
        body,
    } = response
    else {
        return Err(Error::Protocol("expected daemon response".into()));
    };
    if version != PROTOCOL_VERSION {
        return Err(Error::Protocol(format!(
            "unsupported daemon protocol version {version}"
        )));
    }
    if request_id != expected_request_id {
        return Err(Error::Protocol("response request_id did not match".into()));
    }
    match body {
        ResponseBody::Ok { result } => Ok(*result),
        ResponseBody::Error { error } => Err(error.into_error()),
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
