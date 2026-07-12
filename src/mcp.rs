use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::client::DaemonClient;
use crate::model::{
    CommandSpec, Execution, ExecutionEvent, ExecutionEventPayload, ExecutionOutcome,
    ExecutionRequest, Initiator, OutputStream, Workspace,
};
use crate::{Error, Result};

const DEFAULT_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct LoomMcpServer {
    client: DaemonClient,
    workspace: Workspace,
    session_id: Option<String>,
    tool_router: ToolRouter<Self>,
}

impl LoomMcpServer {
    pub async fn for_current_project(client: DaemonClient, cwd: &Path) -> Result<Self> {
        let cwd = cwd.canonicalize()?;
        let workspace = select_project_workspace(client.list_workspaces().await?, &cwd)
            .ok_or_else(|| {
                Error::Config(format!(
                    "no registered Loomterm workspace contains {}; run `loom workspace add . --name loomterm` first",
                    cwd.display()
                ))
            })?;
        Ok(Self::with_workspace(client, workspace))
    }

    fn with_workspace(client: DaemonClient, workspace: Workspace) -> Self {
        Self {
            client,
            workspace,
            session_id: std::env::var("LOOMTERM_SESSION_ID")
                .ok()
                .filter(|value| !value.is_empty()),
            tool_router: Self::tool_router(),
        }
    }

    async fn allowed_execution(&self, id: &str) -> std::result::Result<Execution, String> {
        let execution = self.client.get(id.to_owned()).await.map_err(tool_error)?;
        ensure_workspace_match(execution, &self.workspace.id)
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Text,
    Base64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunInput {
    #[schemars(description = "Working directory relative to the project workspace root")]
    pub cwd: Option<String>,
    #[schemars(description = "Executable for direct argv mode")]
    pub program: Option<String>,
    #[serde(default)]
    #[schemars(description = "Arguments for direct argv mode")]
    pub args: Vec<String>,
    #[schemars(description = "Command for explicit shell mode; mutually exclusive with program")]
    pub shell_command: Option<String>,
    #[schemars(description = "Shell executable, default /bin/sh")]
    pub shell_program: Option<String>,
    #[serde(default)]
    #[schemars(description = "Per-process environment overrides")]
    pub env: BTreeMap<String, String>,
    #[schemars(description = "Optional initial stdin encoded as standard base64")]
    pub stdin_base64: Option<String>,
    #[schemars(description = "Wait this long for output or completion before returning a handle")]
    pub wait_ms: Option<u64>,
    pub capture_limit_bytes: Option<u64>,
    #[serde(default)]
    pub output_format: OutputFormat,
    #[schemars(description = "Maximum raw output bytes returned, default 262144, maximum 1048576")]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RunResult {
    pub execution: Execution,
    pub events: Vec<ToolEvent>,
    pub next_seq: u64,
    pub has_more: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecutionIdInput {
    pub execution_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadInput {
    pub execution_id: String,
    #[serde(default)]
    pub after_seq: u64,
    #[serde(default)]
    pub output_format: OutputFormat,
    #[schemars(description = "Maximum raw output bytes returned, default 262144, maximum 1048576")]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitInput {
    pub execution_id: String,
    #[serde(default)]
    pub after_seq: u64,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub output_format: OutputFormat,
    #[schemars(description = "Maximum raw output bytes returned, default 262144, maximum 1048576")]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListInput {
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExecutionList {
    pub executions: Vec<Execution>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkspaceList {
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ToolOutputResponse {
    pub execution: Execution,
    pub events: Vec<ToolEvent>,
    pub next_seq: u64,
    pub has_more: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ToolWaitResponse {
    pub execution: Execution,
    pub events: Vec<ToolEvent>,
    pub next_seq: u64,
    pub has_more: bool,
    pub timed_out: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ToolEvent {
    pub execution_id: String,
    pub seq: u64,
    pub timestamp_ms: i64,
    #[serde(flatten)]
    pub payload: ToolEventPayload,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolEventPayload {
    Started {
        pid: u32,
        pgid: i32,
    },
    Output {
        stream: OutputStream,
        raw_bytes: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        data_base64: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        lossy: Option<bool>,
    },
    CaptureTruncated {
        limit_bytes: u64,
    },
    Finished {
        outcome: ExecutionOutcome,
    },
}

#[tool_router]
impl LoomMcpServer {
    /// Start a durable command in this project. It continues if the MCP call disconnects.
    #[tool(
        name = "loom_run",
        annotations(
            title = "Run command",
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn run(
        &self,
        Parameters(input): Parameters<RunInput>,
    ) -> std::result::Result<Json<RunResult>, String> {
        let command = match (input.program, input.shell_command) {
            (Some(program), None) => CommandSpec::Argv {
                program,
                args: input.args,
            },
            (None, Some(command)) => CommandSpec::Shell {
                command,
                shell: input.shell_program,
            },
            (Some(_), Some(_)) => {
                return Err("program and shell_command are mutually exclusive".into());
            }
            (None, None) => return Err("one of program or shell_command is required".into()),
        };
        let execution = self
            .client
            .execute(ExecutionRequest {
                workspace_id: self.workspace.id.clone(),
                cwd: input.cwd,
                command,
                env: input.env,
                stdin_base64: input.stdin_base64,
                initiator: Initiator {
                    kind: "mcp".into(),
                    name: Some("loom-mcp".into()),
                    session_id: self.session_id.clone(),
                },
                capture_limit_bytes: input.capture_limit_bytes,
            })
            .await
            .map_err(tool_error)?;

        let wait_for = Duration::from_millis(input.wait_ms.unwrap_or(30_000).min(60_000));
        let deadline = Instant::now() + wait_for;
        let mut current = execution;
        let mut events = Vec::new();
        let mut cursor = 0;
        let mut remaining = normalized_max(input.max_bytes);
        let mut has_more = false;
        while !current.state.is_terminal() && Instant::now() < deadline && remaining > 0 {
            let timeout = deadline.saturating_duration_since(Instant::now());
            let response = self
                .client
                .wait(
                    current.id.clone(),
                    cursor,
                    timeout.as_millis().max(1) as u64,
                    remaining,
                )
                .await
                .map_err(tool_error)?;
            let native_next = response.next_seq;
            let (converted, page_more) = convert_events(
                response.events,
                input.output_format,
                &mut remaining,
                &mut cursor,
            )?;
            events.extend(converted);
            current = response.execution;
            has_more = response.has_more || page_more || cursor < native_next;
            if response.timed_out || page_more {
                break;
            }
        }
        has_more |= !current.state.is_terminal() && remaining == 0;
        Ok(Json(RunResult {
            execution: current,
            events,
            next_seq: cursor,
            has_more,
        }))
    }

    /// Return current execution metadata including exit code or terminating signal.
    #[tool(
        name = "loom_get",
        annotations(title = "Get execution", read_only_hint = true)
    )]
    async fn get(
        &self,
        Parameters(input): Parameters<ExecutionIdInput>,
    ) -> std::result::Result<Json<Execution>, String> {
        self.allowed_execution(&input.execution_id).await.map(Json)
    }

    /// Read persisted output after a sequence cursor without waiting.
    #[tool(
        name = "loom_read",
        annotations(title = "Read execution output", read_only_hint = true)
    )]
    async fn read(
        &self,
        Parameters(input): Parameters<ReadInput>,
    ) -> std::result::Result<Json<ToolOutputResponse>, String> {
        self.allowed_execution(&input.execution_id).await?;
        let max_bytes = normalized_max(input.max_bytes);
        let response = self
            .client
            .read_output(input.execution_id, input.after_seq, max_bytes)
            .await
            .map_err(tool_error)?;
        let native_next = response.next_seq;
        let mut remaining = max_bytes;
        let mut cursor = input.after_seq;
        let (events, page_more) = convert_events(
            response.events,
            input.output_format,
            &mut remaining,
            &mut cursor,
        )?;
        Ok(Json(ToolOutputResponse {
            execution: response.execution,
            events,
            next_seq: cursor,
            has_more: response.has_more || page_more || cursor < native_next,
        }))
    }

    /// Wait for output or completion, then return events after the cursor.
    #[tool(
        name = "loom_wait",
        annotations(title = "Wait for execution", read_only_hint = true)
    )]
    async fn wait(
        &self,
        Parameters(input): Parameters<WaitInput>,
    ) -> std::result::Result<Json<ToolWaitResponse>, String> {
        self.allowed_execution(&input.execution_id).await?;
        let max_bytes = normalized_max(input.max_bytes);
        let response = self
            .client
            .wait(
                input.execution_id,
                input.after_seq,
                input.timeout_ms.unwrap_or(30_000).min(60_000),
                max_bytes,
            )
            .await
            .map_err(tool_error)?;
        let native_next = response.next_seq;
        let mut remaining = max_bytes;
        let mut cursor = input.after_seq;
        let (events, page_more) = convert_events(
            response.events,
            input.output_format,
            &mut remaining,
            &mut cursor,
        )?;
        Ok(Json(ToolWaitResponse {
            execution: response.execution,
            events,
            next_seq: cursor,
            has_more: response.has_more || page_more || cursor < native_next,
            timed_out: response.timed_out,
        }))
    }

    /// Cancel a queued or running execution and its process group.
    #[tool(
        name = "loom_cancel",
        annotations(
            title = "Cancel execution",
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn cancel(
        &self,
        Parameters(input): Parameters<ExecutionIdInput>,
    ) -> std::result::Result<Json<Execution>, String> {
        self.allowed_execution(&input.execution_id).await?;
        self.client
            .cancel(input.execution_id)
            .await
            .map(Json)
            .map_err(tool_error)
    }

    /// List recent durable execution records for this project.
    #[tool(
        name = "loom_list",
        annotations(title = "List executions", read_only_hint = true)
    )]
    async fn list(
        &self,
        Parameters(input): Parameters<ListInput>,
    ) -> std::result::Result<Json<ExecutionList>, String> {
        let executions = self
            .client
            .list(
                Some(self.workspace.id.clone()),
                input.limit.unwrap_or(100).min(1000),
            )
            .await
            .map_err(tool_error)?;
        Ok(Json(ExecutionList { executions }))
    }

    /// Return the single project workspace available to this MCP server.
    #[tool(
        name = "loom_workspaces",
        annotations(title = "List workspaces", read_only_hint = true)
    )]
    async fn workspaces(&self) -> std::result::Result<Json<WorkspaceList>, String> {
        Ok(Json(WorkspaceList {
            workspaces: vec![self.workspace.clone()],
        }))
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "loomterm",
    version = "0.2.0",
    instructions = "Execute commands through Loomterm's durable runtime for this project. Use loom_run, then loom_wait or loom_read with next_seq while has_more is true."
)]
impl ServerHandler for LoomMcpServer {}

fn normalized_max(value: Option<usize>) -> usize {
    value
        .unwrap_or(DEFAULT_OUTPUT_BYTES)
        .clamp(1, MAX_OUTPUT_BYTES)
}

fn select_project_workspace(workspaces: Vec<Workspace>, cwd: &Path) -> Option<Workspace> {
    workspaces
        .into_iter()
        .filter(|workspace| {
            let root = Path::new(&workspace.root);
            cwd == root || cwd.starts_with(root)
        })
        .max_by_key(|workspace| Path::new(&workspace.root).components().count())
}

fn ensure_workspace_match(
    execution: Execution,
    workspace_id: &str,
) -> std::result::Result<Execution, String> {
    if execution.workspace_id == workspace_id {
        Ok(execution)
    } else {
        Err("execution is outside the MCP server's project workspace".into())
    }
}

fn convert_events(
    events: Vec<ExecutionEvent>,
    format: OutputFormat,
    remaining: &mut usize,
    cursor: &mut u64,
) -> std::result::Result<(Vec<ToolEvent>, bool), String> {
    let mut converted = Vec::with_capacity(events.len());
    for event in events {
        let raw_bytes = event_raw_bytes(&event)?;
        if raw_bytes > *remaining {
            return Ok((converted, true));
        }
        *remaining = remaining.saturating_sub(raw_bytes);
        *cursor = event.seq;
        converted.push(convert_event(event, format)?);
    }
    Ok((converted, false))
}

fn event_raw_bytes(event: &ExecutionEvent) -> std::result::Result<usize, String> {
    match &event.payload {
        ExecutionEventPayload::Output { data_base64, .. } => {
            base64::engine::general_purpose::STANDARD
                .decode(data_base64)
                .map(|data| data.len())
                .map_err(|error| format!("invalid persisted output: {error}"))
        }
        _ => Ok(0),
    }
}

fn convert_event(
    event: ExecutionEvent,
    format: OutputFormat,
) -> std::result::Result<ToolEvent, String> {
    let payload = match event.payload {
        ExecutionEventPayload::Started { pid, pgid } => ToolEventPayload::Started { pid, pgid },
        ExecutionEventPayload::Output {
            stream,
            data_base64,
        } => {
            let data = base64::engine::general_purpose::STANDARD
                .decode(&data_base64)
                .map_err(|error| format!("invalid persisted output: {error}"))?;
            let raw_bytes = data.len();
            match format {
                OutputFormat::Text => {
                    let (text, lossy) = match String::from_utf8(data) {
                        Ok(text) => (text, false),
                        Err(error) => {
                            (String::from_utf8_lossy(error.as_bytes()).into_owned(), true)
                        }
                    };
                    ToolEventPayload::Output {
                        stream,
                        raw_bytes,
                        text: Some(text),
                        data_base64: None,
                        lossy: lossy.then_some(true),
                    }
                }
                OutputFormat::Base64 => ToolEventPayload::Output {
                    stream,
                    raw_bytes,
                    text: None,
                    data_base64: Some(data_base64),
                    lossy: None,
                },
            }
        }
        ExecutionEventPayload::CaptureTruncated { limit_bytes } => {
            ToolEventPayload::CaptureTruncated { limit_bytes }
        }
        ExecutionEventPayload::Finished { outcome } => ToolEventPayload::Finished { outcome },
    };
    Ok(ToolEvent {
        execution_id: event.execution_id,
        seq: event.seq,
        timestamp_ms: event.timestamp_ms,
        payload,
    })
}

fn tool_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_the_stable_tool_surface() {
        let server = LoomMcpServer::with_workspace(
            DaemonClient::new("/tmp/loomterm-test-missing.sock"),
            Workspace {
                id: "test".into(),
                name: "test".into(),
                root: "/tmp".into(),
                created_at_ms: 0,
            },
        );
        let names: Vec<String> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "loom_cancel",
                "loom_get",
                "loom_list",
                "loom_read",
                "loom_run",
                "loom_wait",
                "loom_workspaces",
            ]
        );
    }

    #[test]
    fn text_output_reports_lossy_utf8_without_duplicate_base64() {
        let event = ExecutionEvent {
            execution_id: "e".into(),
            seq: 1,
            timestamp_ms: 0,
            payload: ExecutionEventPayload::Output {
                stream: OutputStream::Stdout,
                data_base64: base64::engine::general_purpose::STANDARD.encode(b"hello\xff"),
            },
        };
        let converted = convert_event(event, OutputFormat::Text).unwrap();
        let ToolEventPayload::Output {
            text,
            data_base64,
            lossy,
            raw_bytes,
            ..
        } = converted.payload
        else {
            panic!("expected output event");
        };
        assert_eq!(text.as_deref(), Some("hello�"));
        assert!(data_base64.is_none());
        assert_eq!(lossy, Some(true));
        assert_eq!(raw_bytes, 6);
    }

    #[test]
    fn base64_output_omits_text_and_preserves_the_raw_budget() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"binary\0data");
        let event = ExecutionEvent {
            execution_id: "e".into(),
            seq: 1,
            timestamp_ms: 0,
            payload: ExecutionEventPayload::Output {
                stream: OutputStream::Stdout,
                data_base64: encoded.clone(),
            },
        };
        let mut remaining = 11;
        let mut cursor = 0;
        let (events, has_more) = convert_events(
            vec![event],
            OutputFormat::Base64,
            &mut remaining,
            &mut cursor,
        )
        .unwrap();
        let ToolEventPayload::Output {
            text,
            data_base64,
            lossy,
            ..
        } = &events[0].payload
        else {
            panic!("expected output event");
        };
        assert!(text.is_none());
        assert_eq!(data_base64.as_deref(), Some(encoded.as_str()));
        assert!(lossy.is_none());
        assert_eq!(remaining, 0);
        assert_eq!(cursor, 1);
        assert!(!has_more);
    }

    #[test]
    fn selects_the_most_specific_project_workspace() {
        let selected = select_project_workspace(
            vec![
                Workspace {
                    id: "parent".into(),
                    name: "parent".into(),
                    root: "/work".into(),
                    created_at_ms: 0,
                },
                Workspace {
                    id: "project".into(),
                    name: "project".into(),
                    root: "/work/project".into(),
                    created_at_ms: 0,
                },
            ],
            Path::new("/work/project/src"),
        )
        .unwrap();
        assert_eq!(selected.id, "project");
    }

    #[test]
    fn rejects_an_execution_from_another_workspace() {
        let execution = Execution {
            id: "execution".into(),
            workspace_id: "other".into(),
            state: crate::model::ExecutionState::Queued,
            command: CommandSpec::Argv {
                program: "true".into(),
                args: vec![],
            },
            command_display: "true".into(),
            cwd: "/work/other".into(),
            env_keys: vec![],
            initiator: Initiator::default(),
            created_at_ms: 0,
            started_at_ms: None,
            ended_at_ms: None,
            duration_ms: None,
            pid: None,
            pgid: None,
            outcome: None,
            captured_bytes: 0,
            output_truncated: false,
            last_seq: 0,
        };
        assert!(ensure_workspace_match(execution, "project").is_err());
    }
}
