use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::service::NotificationContext;
use rmcp::{Json, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::client::DaemonClient;
use crate::model::{
    CommandSpec, Execution, ExecutionEvent, ExecutionRequest, Initiator, ReadOutputResponse,
    WaitResponse, Workspace,
};

#[derive(Debug, Clone)]
pub struct LoomMcpServer {
    client: DaemonClient,
    roots: Arc<RwLock<Option<Vec<PathBuf>>>>,
    tool_router: ToolRouter<Self>,
}

impl LoomMcpServer {
    pub fn new(client: DaemonClient) -> Self {
        Self {
            client,
            roots: Arc::new(RwLock::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    async fn ensure_workspace_allowed(&self, identifier: &str) -> Result<(), String> {
        let roots = self.roots.read().await.clone();
        let Some(roots) = roots else {
            return Ok(());
        };
        let workspaces = self.client.list_workspaces().await.map_err(tool_error)?;
        let workspace = workspaces
            .into_iter()
            .find(|workspace| workspace.id == identifier || workspace.name == identifier)
            .ok_or_else(|| format!("workspace not found: {identifier}"))?;
        let workspace_root = Path::new(&workspace.root);
        if roots
            .iter()
            .any(|root| workspace_root == root || workspace_root.starts_with(root))
        {
            Ok(())
        } else {
            Err(format!(
                "workspace {} is outside the MCP client's declared roots",
                workspace.name
            ))
        }
    }

    #[allow(deprecated)]
    async fn refresh_roots(&self, context: NotificationContext<RoleServer>) {
        let Ok(result) = context.peer.list_roots().await else {
            return;
        };
        let roots: Vec<PathBuf> = result
            .roots
            .into_iter()
            .filter_map(|root| url::Url::parse(&root.uri).ok())
            .filter_map(|url| url.to_file_path().ok())
            .filter_map(|path| path.canonicalize().ok())
            .collect();
        if !roots.is_empty() {
            *self.roots.write().await = Some(roots);
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunInput {
    #[schemars(description = "Registered workspace id or name")]
    pub workspace: String,
    #[schemars(description = "Working directory relative to the workspace root")]
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
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RunResult {
    pub execution: Execution,
    pub events: Vec<ExecutionEvent>,
    pub next_seq: u64,
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
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitInput {
    pub execution_id: String,
    #[serde(default)]
    pub after_seq: u64,
    pub timeout_ms: Option<u64>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListInput {
    pub workspace: Option<String>,
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

#[tool_router]
impl LoomMcpServer {
    /// Start a durable command execution. The command continues if this MCP call disconnects.
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
    ) -> Result<Json<RunResult>, String> {
        self.ensure_workspace_allowed(&input.workspace).await?;
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
                workspace_id: input.workspace,
                cwd: input.cwd,
                command,
                env: input.env,
                stdin_base64: input.stdin_base64,
                initiator: Initiator {
                    kind: "mcp".into(),
                    name: Some("loom-mcp".into()),
                    session_id: None,
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
        let mut remaining_bytes = 1024 * 1024usize;
        while !current.state.is_terminal() && Instant::now() < deadline && remaining_bytes > 0 {
            let timeout = deadline.saturating_duration_since(Instant::now());
            let response = self
                .client
                .wait(
                    current.id.clone(),
                    cursor,
                    timeout.as_millis().max(1) as u64,
                    remaining_bytes,
                )
                .await
                .map_err(tool_error)?;
            let used = response
                .events
                .iter()
                .map(|event| match &event.payload {
                    crate::model::ExecutionEventPayload::Output { text, .. } => text.len(),
                    _ => 0,
                })
                .sum::<usize>();
            remaining_bytes = remaining_bytes.saturating_sub(used);
            cursor = response.next_seq;
            current = response.execution;
            events.extend(response.events);
            if response.timed_out {
                break;
            }
        }
        Ok(Json(RunResult {
            execution: current,
            events,
            next_seq: cursor,
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
    ) -> Result<Json<Execution>, String> {
        self.client
            .get(input.execution_id)
            .await
            .map(Json)
            .map_err(tool_error)
    }

    /// Read persisted output events after a sequence cursor without waiting.
    #[tool(
        name = "loom_read",
        annotations(title = "Read execution output", read_only_hint = true)
    )]
    async fn read(
        &self,
        Parameters(input): Parameters<ReadInput>,
    ) -> Result<Json<ReadOutputResponse>, String> {
        self.client
            .read_output(
                input.execution_id,
                input.after_seq,
                input.max_bytes.unwrap_or(1024 * 1024).min(8 * 1024 * 1024),
            )
            .await
            .map(Json)
            .map_err(tool_error)
    }

    /// Wait for the next execution event or timeout, then return output after the cursor.
    #[tool(
        name = "loom_wait",
        annotations(title = "Wait for execution", read_only_hint = true)
    )]
    async fn wait(
        &self,
        Parameters(input): Parameters<WaitInput>,
    ) -> Result<Json<WaitResponse>, String> {
        self.client
            .wait(
                input.execution_id,
                input.after_seq,
                input.timeout_ms.unwrap_or(30_000).min(60_000),
                input.max_bytes.unwrap_or(1024 * 1024).min(8 * 1024 * 1024),
            )
            .await
            .map(Json)
            .map_err(tool_error)
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
    ) -> Result<Json<Execution>, String> {
        self.client
            .cancel(input.execution_id)
            .await
            .map(Json)
            .map_err(tool_error)
    }

    /// List recent durable execution records.
    #[tool(
        name = "loom_list",
        annotations(title = "List executions", read_only_hint = true)
    )]
    async fn list(
        &self,
        Parameters(input): Parameters<ListInput>,
    ) -> Result<Json<ExecutionList>, String> {
        if let Some(workspace) = input.workspace.as_deref() {
            self.ensure_workspace_allowed(workspace).await?;
        }
        let executions = self
            .client
            .list(input.workspace, input.limit.unwrap_or(100).min(1000))
            .await
            .map_err(tool_error)?;
        Ok(Json(ExecutionList { executions }))
    }

    /// List the explicitly registered workspaces available to this MCP client.
    #[tool(
        name = "loom_workspaces",
        annotations(title = "List workspaces", read_only_hint = true)
    )]
    async fn workspaces(&self) -> Result<Json<WorkspaceList>, String> {
        let mut workspaces = self.client.list_workspaces().await.map_err(tool_error)?;
        if let Some(roots) = self.roots.read().await.as_ref() {
            workspaces.retain(|workspace| {
                roots.iter().any(|root| {
                    Path::new(&workspace.root) == root
                        || Path::new(&workspace.root).starts_with(root)
                })
            });
        }
        Ok(Json(WorkspaceList { workspaces }))
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "loomterm",
    version = "0.1.0",
    instructions = "Execute commands through Loomterm's durable, workspace-scoped runtime. Use loom_run, then loom_wait/loom_read with the returned sequence cursor."
)]
impl ServerHandler for LoomMcpServer {
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        self.refresh_roots(context).await;
    }

    async fn on_roots_list_changed(&self, context: NotificationContext<RoleServer>) {
        self.refresh_roots(context).await;
    }
}

fn tool_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_the_stable_tool_surface() {
        let server = LoomMcpServer::new(DaemonClient::new("/tmp/loomterm-test-missing.sock"));
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
}
