use serde_json::Value;

use crate::model::{AgentEvent, AgentEventRequest, AgentSession, AgentSessionState};
use crate::{Error, Result};

pub const MAX_HOOK_INPUT_BYTES: u64 = 1024 * 1024;

pub fn hook_cwd(input: &Value) -> Option<&str> {
    input.get("cwd").and_then(Value::as_str)
}

pub fn select_active_session_id(
    sessions: &[AgentSession],
    provider: &str,
    cwd: &std::path::Path,
) -> Option<String> {
    sessions
        .iter()
        .filter(|session| session.state == AgentSessionState::Recording)
        .filter(|session| {
            session.agent_kind.eq_ignore_ascii_case(provider)
                || session.agent_kind.eq_ignore_ascii_case("generic")
        })
        .find(|session| {
            let session_cwd = std::path::Path::new(&session.cwd);
            cwd == session_cwd || cwd.starts_with(session_cwd)
        })
        .map(|session| session.id.clone())
}

pub fn normalize_hook_event(
    provider: &str,
    loom_session_id: &str,
    input: Value,
) -> Result<Option<AgentEventRequest>> {
    let object = input
        .as_object()
        .ok_or_else(|| Error::InvalidRequest("hook input must be a JSON object".into()))?;
    let provider_session_id = required_string(object.get("session_id"), "session_id")?;
    let hook_event = required_string(object.get("hook_event_name"), "hook_event_name")?;
    let provider_turn_id = optional_string(object.get("turn_id"));

    let event = match hook_event {
        "UserPromptSubmit" => AgentEvent::PromptSubmitted {
            prompt: required_string(object.get("prompt"), "prompt")?.to_owned(),
        },
        "PreToolUse" => AgentEvent::ToolStarted {
            action_id: required_string(object.get("tool_use_id"), "tool_use_id")?.to_owned(),
            tool_name: required_string(object.get("tool_name"), "tool_name")?.to_owned(),
        },
        "PostToolUse" | "PostToolUseFailure" => AgentEvent::ToolFinished {
            action_id: required_string(object.get("tool_use_id"), "tool_use_id")?.to_owned(),
            tool_name: required_string(object.get("tool_name"), "tool_name")?.to_owned(),
            failed: hook_event == "PostToolUseFailure",
            execution_id: object.get("tool_response").and_then(find_execution_id),
        },
        "Stop" | "StopFailure" => AgentEvent::TurnFinished {
            failed: hook_event == "StopFailure",
            last_assistant_message: optional_string(object.get("last_assistant_message"))
                .map(str::to_owned),
        },
        _ => return Ok(None),
    };
    let request = AgentEventRequest {
        session_id: loom_session_id.to_owned(),
        provider: provider.to_owned(),
        provider_session_id: provider_session_id.to_owned(),
        provider_turn_id: provider_turn_id.map(str::to_owned),
        event,
    };
    request.validate()?;
    Ok(Some(request))
}

fn required_string<'a>(value: Option<&'a Value>, name: &str) -> Result<&'a str> {
    optional_string(value)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::InvalidRequest(format!("hook field {name} must be a string")))
}

fn optional_string(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn find_execution_id(value: &Value) -> Option<String> {
    find_execution_id_at_depth(value, 0)
}

fn find_execution_id_at_depth(value: &Value, depth: usize) -> Option<String> {
    if depth > 8 {
        return None;
    }
    match value {
        Value::Object(object) => {
            if let Some(id) = object.get("execution_id").and_then(Value::as_str) {
                return Some(id.to_owned());
            }
            if let Some(id) = object
                .get("execution")
                .and_then(Value::as_object)
                .and_then(|execution| execution.get("id"))
                .and_then(Value::as_str)
            {
                return Some(id.to_owned());
            }
            object
                .values()
                .find_map(|value| find_execution_id_at_depth(value, depth + 1))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_execution_id_at_depth(value, depth + 1)),
        Value::String(text) if text.len() <= MAX_HOOK_INPUT_BYTES as usize => {
            serde_json::from_str::<Value>(text)
                .ok()
                .and_then(|value| find_execution_id_at_depth(&value, depth + 1))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommandSpec, ExecutionOutcome};

    fn recording(id: &str, provider: &str, cwd: &str) -> AgentSession {
        AgentSession {
            id: id.into(),
            workspace_id: "workspace".into(),
            state: AgentSessionState::Recording,
            agent_kind: provider.into(),
            name: None,
            command: CommandSpec::Argv {
                program: provider.into(),
                args: Vec::new(),
            },
            command_display: provider.into(),
            cwd: cwd.into(),
            created_at_ms: 1,
            ended_at_ms: None,
            duration_ms: None,
            recorder_pid: 1,
            outcome: None::<ExecutionOutcome>,
            captured_bytes: 0,
            output_truncated: false,
            initial_cols: 80,
            initial_rows: 24,
            cast_path: "recording.cast".into(),
            html_path: "replay.html".into(),
        }
    }

    #[test]
    fn normalizes_codex_prompt_with_turn_id() {
        let request = normalize_hook_event(
            "codex",
            "loom-session",
            serde_json::json!({
                "session_id": "codex-session",
                "turn_id": "codex-turn",
                "hook_event_name": "UserPromptSubmit",
                "prompt": "Run the tests"
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(request.provider, "codex");
        assert_eq!(request.provider_turn_id.as_deref(), Some("codex-turn"));
        assert!(matches!(
            request.event,
            AgentEvent::PromptSubmitted { ref prompt } if prompt == "Run the tests"
        ));
    }

    #[test]
    fn normalizes_claude_tool_result_and_extracts_execution() {
        let request = normalize_hook_event(
            "claude",
            "loom-session",
            serde_json::json!({
                "session_id": "claude-session",
                "hook_event_name": "PostToolUse",
                "tool_name": "mcp__loomterm__loom_run",
                "tool_use_id": "tool-1",
                "tool_response": {
                    "content": [{"type": "text", "text": "{\"execution\":{\"id\":\"exec-1\"}}"}]
                }
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(request.provider, "claude");
        assert!(matches!(
            request.event,
            AgentEvent::ToolFinished { ref execution_id, .. }
                if execution_id.as_deref() == Some("exec-1")
        ));
    }

    #[test]
    fn ignores_untracked_hook_events() {
        assert!(
            normalize_hook_event(
                "claude",
                "loom-session",
                serde_json::json!({
                    "session_id": "claude-session",
                    "hook_event_name": "SessionStart"
                })
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn selects_matching_active_provider_session_by_cwd() {
        let sessions = vec![
            recording("claude", "claude", "/tmp/project"),
            recording("codex", "codex", "/tmp/project"),
        ];
        assert_eq!(
            select_active_session_id(
                &sessions,
                "codex",
                std::path::Path::new("/tmp/project/subdir")
            )
            .as_deref(),
            Some("codex")
        );
        assert!(
            select_active_session_id(&sessions, "codex", std::path::Path::new("/tmp/other"))
                .is_none()
        );
    }
}
