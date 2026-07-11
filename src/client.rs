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
    Execution, ExecutionRequest, ExecutionStats, Health, PROTOCOL_VERSION, ReadOutputResponse,
    WaitResponse, Workspace,
};
use crate::protocol::{
    CAPABILITY_EXECUTION_STATS, MAX_FRAME_BYTES, Operation, ProtocolResult, ResponseBody,
    SubscriptionResponse, WireMessage,
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
        match client.health().await {
            Ok(_) => return Ok(client),
            Err(Error::DaemonUnavailable(_)) => {}
            Err(error) => return Err(error),
        }
        start_daemon_process()?;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            match client.health().await {
                Ok(_) => return Ok(client),
                Err(Error::DaemonUnavailable(_)) => {}
                Err(error) => return Err(error),
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

    pub async fn stats(&self, workspace: String, since_ms: i64) -> Result<ExecutionStats> {
        self.require_capability(CAPABILITY_EXECUTION_STATS).await?;
        match self
            .call(Operation::Stats {
                workspace,
                since_ms,
            })
            .await?
        {
            ProtocolResult::Stats(value) => Ok(value),
            value => unexpected("statistics", value),
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

    async fn require_capability(&self, capability: &str) -> Result<()> {
        let health = self.health().await?;
        if health
            .capabilities
            .iter()
            .any(|available| available == capability)
        {
            return Ok(());
        }
        Err(Error::DaemonUpgradeRequired {
            daemon_pid: health.daemon_pid,
            capability: capability.to_owned(),
        })
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
                    return Err(Error::ProtocolVersionMismatch {
                        client: PROTOCOL_VERSION,
                        daemon: version,
                    });
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
        return Err(Error::ProtocolVersionMismatch {
            client: PROTOCOL_VERSION,
            daemon: version,
        });
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use tokio::net::UnixListener;

    use super::*;

    async fn serve_health(socket: &Path, response_version: u32, include_capabilities: bool) {
        let listener = UnixListener::bind(socket).unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(MAX_FRAME_BYTES)
            .new_codec();
        let mut framed = Framed::new(stream, codec);
        let request = framed.next().await.unwrap().unwrap();
        let request: WireMessage = serde_json::from_slice(&request).unwrap();
        let WireMessage::Request { request_id, .. } = request else {
            panic!("expected health request");
        };
        let mut health = serde_json::json!({
            "protocol_version": response_version,
            "daemon_pid": 42,
            "database_path": "/tmp/loom.db",
            "socket_path": socket,
        });
        if include_capabilities {
            health["capabilities"] = serde_json::json!([CAPABILITY_EXECUTION_STATS]);
        }
        let response = serde_json::json!({
            "kind": "response",
            "version": response_version,
            "request_id": request_id,
            "status": "ok",
            "result": {
                "type": "health",
                "value": health,
            },
        });
        framed
            .send(Bytes::from(serde_json::to_vec(&response).unwrap()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stats_requires_a_daemon_capability() {
        let temp = tempdir().unwrap();
        let socket = temp.path().join("loomd.sock");
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            serve_health(&server_socket, PROTOCOL_VERSION, false).await;
        });
        while !socket.exists() {
            tokio::task::yield_now().await;
        }

        let error = DaemonClient::new(&socket)
            .stats("workspace".into(), 0)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::DaemonUpgradeRequired { daemon_pid: 42, .. }
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn connect_or_start_does_not_replace_an_incompatible_daemon() {
        let temp = tempdir().unwrap();
        let paths = AppPaths {
            state_dir: temp.path().join("state"),
            runtime_dir: temp.path().join("run"),
            config_file: temp.path().join("config.toml"),
            database: temp.path().join("state/loom.db"),
            socket: temp.path().join("loomd.sock"),
            lock_file: temp.path().join("loomd.lock"),
        };
        let server_socket = paths.socket.clone();
        let server = tokio::spawn(async move {
            serve_health(&server_socket, PROTOCOL_VERSION + 1, true).await;
        });
        while !paths.socket.exists() {
            tokio::task::yield_now().await;
        }

        let error = DaemonClient::connect_or_start(&paths).await.unwrap_err();
        assert!(matches!(
            error,
            Error::ProtocolVersionMismatch {
                client: PROTOCOL_VERSION,
                daemon,
            } if daemon == PROTOCOL_VERSION + 1
        ));
        server.await.unwrap();
    }
}
