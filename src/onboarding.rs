use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub struct AgentSelection {
    pub codex: bool,
    pub claude: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigAction {
    Create,
    Replace,
    Unchanged,
    Skip,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigPlan {
    pub path: PathBuf,
    pub action: ConfigAction,
    #[serde(skip)]
    content: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InitPlan {
    pub root: PathBuf,
    pub name: String,
    pub mcp_command: PathBuf,
    pub loom_command: PathBuf,
    pub codex: ConfigPlan,
    pub codex_hooks: ConfigPlan,
    pub claude: ConfigPlan,
    pub claude_hooks: ConfigPlan,
}

pub fn plan(
    root: &Path,
    name: Option<&str>,
    agents: AgentSelection,
    force: bool,
) -> Result<InitPlan> {
    plan_with_command(root, name, agents, force, find_mcp_command()?)
}

fn plan_with_command(
    root: &Path,
    name: Option<&str>,
    agents: AgentSelection,
    force: bool,
    mcp_command: PathBuf,
) -> Result<InitPlan> {
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err(Error::InvalidRequest(format!(
            "workspace root is not a directory: {}",
            root.display()
        )));
    }
    let name = name.map(str::to_owned).unwrap_or_else(|| {
        root.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("workspace")
            .to_owned()
    });
    if name.trim().is_empty() {
        return Err(Error::InvalidRequest(
            "workspace name must not be empty".into(),
        ));
    }
    let loom_command = mcp_command.with_file_name("loom");
    let codex_path = root.join(".codex/config.toml");
    let codex_hooks_path = root.join(".codex/hooks.json");
    let claude_path = root.join(".mcp.json");
    let claude_hooks_path = root.join(".claude/settings.json");
    let codex = if agents.codex {
        plan_codex(&codex_path, &mcp_command, force)?
    } else {
        skipped(codex_path)
    };
    let codex_hooks = if agents.codex {
        plan_hooks(&codex_hooks_path, &loom_command, "codex", force)?
    } else {
        skipped(codex_hooks_path)
    };
    let claude = if agents.claude {
        plan_claude(&claude_path, &mcp_command, force)?
    } else {
        skipped(claude_path)
    };
    let claude_hooks = if agents.claude {
        plan_hooks(&claude_hooks_path, &loom_command, "claude", force)?
    } else {
        skipped(claude_hooks_path)
    };
    Ok(InitPlan {
        root,
        name,
        mcp_command,
        loom_command,
        codex,
        codex_hooks,
        claude,
        claude_hooks,
    })
}

pub fn apply(plan: &InitPlan) -> Result<()> {
    for config in [
        &plan.codex,
        &plan.codex_hooks,
        &plan.claude,
        &plan.claude_hooks,
    ] {
        if let Some(content) = &config.content {
            write_atomic(&config.path, content.as_bytes())?;
        }
    }
    Ok(())
}

fn plan_hooks(path: &Path, command: &Path, provider: &str, force: bool) -> Result<ConfigPlan> {
    let source = read_optional(path)?;
    let mut root: Value = match &source {
        Some(source) => serde_json::from_str(source)
            .map_err(|error| Error::Config(format!("{}: {error}", path.display())))?,
        None => Value::Object(Map::new()),
    };
    let root_object = root
        .as_object_mut()
        .ok_or_else(|| Error::Config(format!("{}: root must be a JSON object", path.display())))?;
    let hooks = root_object
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| Error::Config(format!("{}: hooks must be a JSON object", path.display())))?;
    let events: &[&str] = if provider == "claude" {
        &[
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "Stop",
            "StopFailure",
        ]
    } else {
        &["UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop"]
    };
    let desired = hook_handler(command, provider);
    let mut exact_events = 0usize;
    let mut stale = false;
    for event in events {
        let groups = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| {
                Error::Config(format!(
                    "{}: hooks.{event} must be a JSON array",
                    path.display()
                ))
            })?;
        let mut exact = false;
        for group in groups.iter() {
            let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for handler in handlers {
                if is_loomterm_hook(handler, provider) {
                    if handler == &desired {
                        exact = true;
                    } else {
                        stale = true;
                    }
                }
            }
        }
        exact_events += usize::from(exact);
    }
    if stale && !force {
        return collision(path);
    }
    if !stale && exact_events == events.len() {
        return Ok(ConfigPlan {
            path: path.into(),
            action: ConfigAction::Unchanged,
            content: None,
        });
    }
    for event in events {
        let groups = hooks.get_mut(*event).and_then(Value::as_array_mut).unwrap();
        for group in groups.iter_mut() {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                handlers.retain(|handler| !is_loomterm_hook(handler, provider));
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|handlers| !handlers.is_empty())
        });
        groups.push(serde_json::json!({ "hooks": [desired.clone()] }));
    }
    let action = if stale {
        ConfigAction::Replace
    } else {
        ConfigAction::Create
    };
    let mut content = serde_json::to_string_pretty(&root)?;
    content.push('\n');
    Ok(ConfigPlan {
        path: path.into(),
        action,
        content: Some(content),
    })
}

fn hook_handler(command: &Path, provider: &str) -> Value {
    let environment = [
        "LOOMTERM_CONFIG",
        "LOOMTERM_STATE_DIR",
        "LOOMTERM_RUNTIME_DIR",
    ]
    .into_iter()
    .filter_map(|key| {
        std::env::var_os(key).map(|value| (key, value.to_string_lossy().into_owned()))
    })
    .collect::<Vec<_>>();
    if provider == "claude" {
        let mut args = environment
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>();
        if environment.is_empty() {
            serde_json::json!({
                "type": "command",
                "command": command,
                "args": ["agent-event", "--provider", provider],
                "timeout": 5
            })
        } else {
            args.push(command.to_string_lossy().into_owned());
            args.extend(["agent-event".into(), "--provider".into(), provider.into()]);
            serde_json::json!({
                "type": "command",
                "command": "/usr/bin/env",
                "args": args,
                "timeout": 5
            })
        }
    } else {
        let environment = environment
            .iter()
            .map(|(key, value)| format!("{key}={}", shell_quote(value)))
            .collect::<Vec<_>>()
            .join(" ");
        let environment = if environment.is_empty() {
            String::new()
        } else {
            format!("{environment} ")
        };
        serde_json::json!({
            "type": "command",
            "command": format!(
                "{environment}{} agent-event --provider {provider}",
                shell_quote(command.to_string_lossy().as_ref())
            ),
            "timeout": 5
        })
    }
}

fn is_loomterm_hook(handler: &Value, provider: &str) -> bool {
    let suffix = format!("agent-event --provider {provider}");
    if handler
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.contains(&suffix))
    {
        return true;
    }
    handler
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(|args| {
            args.windows(3).any(|values| {
                values
                    == [
                        Value::String("agent-event".into()),
                        Value::String("--provider".into()),
                        Value::String(provider.into()),
                    ]
            })
        })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn plan_codex(path: &Path, command: &Path, force: bool) -> Result<ConfigPlan> {
    let source = read_optional(path)?;
    let mut document = source
        .as_deref()
        .unwrap_or("")
        .parse::<DocumentMut>()
        .map_err(|error| Error::Config(format!("{}: {error}", path.display())))?;
    let command = toml_edit::value(command.to_string_lossy().as_ref()).to_string();
    let desired = format!(
        concat!(
            "[mcp_servers.loomterm]\n",
            "command = {command}\n",
            "cwd = \".\"\n",
            "env_vars = [\"LOOMTERM_CONFIG\", \"LOOMTERM_STATE_DIR\", ",
            "\"LOOMTERM_RUNTIME_DIR\", \"LOOMTERM_SESSION_ID\", ",
            "\"LOOMTERM_AGENT_KIND\"]\n",
            "startup_timeout_sec = 30\n",
            "tool_timeout_sec = 90\n",
            "required = true\n",
            "default_tools_approval_mode = \"writes\"\n"
        ),
        command = command
    )
    .parse::<DocumentMut>()
    .map_err(|error| Error::Config(format!("could not build Codex configuration: {error}")))?;
    let desired_item = desired["mcp_servers"]["loomterm"].clone();
    let current = document
        .get("mcp_servers")
        .and_then(Item::as_table_like)
        .and_then(|servers| servers.get("loomterm"));
    let action = match current {
        Some(item) if item.to_string() == desired_item.to_string() => ConfigAction::Unchanged,
        Some(_) if !force => return collision(path),
        Some(_) => ConfigAction::Replace,
        None => ConfigAction::Create,
    };
    if action == ConfigAction::Unchanged {
        return Ok(ConfigPlan {
            path: path.into(),
            action,
            content: None,
        });
    }
    if !document.contains_key("mcp_servers") {
        document["mcp_servers"] = Item::Table(toml_edit::Table::new());
    }
    document["mcp_servers"]["loomterm"] = desired_item;
    Ok(ConfigPlan {
        path: path.into(),
        action,
        content: Some(document.to_string()),
    })
}

fn plan_claude(path: &Path, command: &Path, force: bool) -> Result<ConfigPlan> {
    let source = read_optional(path)?;
    let mut root: Value = match &source {
        Some(source) => serde_json::from_str(source)
            .map_err(|error| Error::Config(format!("{}: {error}", path.display())))?,
        None => Value::Object(Map::new()),
    };
    let root_object = root
        .as_object_mut()
        .ok_or_else(|| Error::Config(format!("{}: root must be a JSON object", path.display())))?;
    let servers = root_object
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            Error::Config(format!(
                "{}: mcpServers must be a JSON object",
                path.display()
            ))
        })?;
    let desired = serde_json::json!({
        "type": "stdio",
        "command": command,
        "args": [],
        "env": {}
    });
    let action = match servers.get("loomterm") {
        Some(current) if current == &desired => ConfigAction::Unchanged,
        Some(_) if !force => return collision(path),
        Some(_) => ConfigAction::Replace,
        None => ConfigAction::Create,
    };
    if action == ConfigAction::Unchanged {
        return Ok(ConfigPlan {
            path: path.into(),
            action,
            content: None,
        });
    }
    servers.insert("loomterm".into(), desired);
    let mut content = serde_json::to_string_pretty(&root)?;
    content.push('\n');
    Ok(ConfigPlan {
        path: path.into(),
        action,
        content: Some(content),
    })
}

fn find_mcp_command() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("loom-mcp");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    let sibling = std::env::current_exe()?.with_file_name("loom-mcp");
    if sibling.is_file() {
        return Ok(sibling);
    }
    Err(Error::Config(
        "could not find loom-mcp in PATH or beside the loom binary".into(),
    ))
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    if path.exists() {
        Ok(Some(fs::read_to_string(path)?))
    } else {
        Ok(None)
    }
}

fn skipped(path: PathBuf) -> ConfigPlan {
    ConfigPlan {
        path,
        action: ConfigAction::Skip,
        content: None,
    }
}

fn collision<T>(path: &Path) -> Result<T> {
    Err(Error::Config(format!(
        "{} already contains a different Loomterm configuration; use --force to replace only Loomterm-managed entries",
        path.display()
    )))
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::Config(format!(
            "configuration path has no parent: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".loomterm-{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        if let Ok(metadata) = fs::metadata(path) {
            fs::set_permissions(&temporary, metadata.permissions())?;
        }
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn fake_mcp() -> (TempDir, PathBuf) {
        let directory = TempDir::new().unwrap();
        let executable = directory.path().join("loom-mcp");
        fs::write(&executable, "").unwrap();
        (directory, executable)
    }

    #[test]
    fn creates_and_reuses_both_configs() {
        let workspace = TempDir::new().unwrap();
        let (_bin, executable) = fake_mcp();
        {
            let first = plan_with_command(
                workspace.path(),
                None,
                AgentSelection {
                    codex: true,
                    claude: true,
                },
                false,
                executable.clone(),
            )
            .unwrap();
            assert_eq!(first.mcp_command, executable);
            assert_eq!(first.codex.action, ConfigAction::Create);
            assert_eq!(first.codex_hooks.action, ConfigAction::Create);
            assert_eq!(first.claude.action, ConfigAction::Create);
            assert_eq!(first.claude_hooks.action, ConfigAction::Create);
            apply(&first).unwrap();
            let second = plan_with_command(
                workspace.path(),
                None,
                AgentSelection {
                    codex: true,
                    claude: true,
                },
                false,
                executable,
            )
            .unwrap();
            assert_eq!(second.codex.action, ConfigAction::Unchanged);
            assert_eq!(second.codex_hooks.action, ConfigAction::Unchanged);
            assert_eq!(second.claude.action, ConfigAction::Unchanged);
            assert_eq!(second.claude_hooks.action, ConfigAction::Unchanged);

            let codex_hooks: Value = serde_json::from_slice(
                &fs::read(workspace.path().join(".codex/hooks.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                codex_hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]["type"],
                "command"
            );
            let claude_hooks: Value = serde_json::from_slice(
                &fs::read(workspace.path().join(".claude/settings.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                claude_hooks["hooks"]["PostToolUseFailure"][0]["hooks"][0]["args"],
                serde_json::json!(["agent-event", "--provider", "claude"])
            );
        }
    }

    #[test]
    fn preserves_unrelated_settings_and_force_replaces_only_loomterm() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join(".codex")).unwrap();
        fs::write(
            workspace.path().join(".codex/config.toml"),
            "model = \"gpt-test\"\n[mcp_servers.other]\ncommand = \"other\"\n[mcp_servers.loomterm]\ncommand = \"old\"\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join(".mcp.json"),
            r#"{"other":true,"mcpServers":{"other":{"command":"other"},"loomterm":{"command":"old"}}}"#,
        )
        .unwrap();
        let (bin, _) = fake_mcp();
        {
            assert!(
                plan_with_command(
                    workspace.path(),
                    None,
                    AgentSelection {
                        codex: true,
                        claude: true,
                    },
                    false,
                    bin.path().join("loom-mcp"),
                )
                .is_err()
            );
            let replacement = plan_with_command(
                workspace.path(),
                None,
                AgentSelection {
                    codex: true,
                    claude: true,
                },
                true,
                bin.path().join("loom-mcp"),
            )
            .unwrap();
            apply(&replacement).unwrap();
            let codex = fs::read_to_string(workspace.path().join(".codex/config.toml")).unwrap();
            assert!(codex.contains("model = \"gpt-test\""));
            assert!(codex.contains("[mcp_servers.other]"));
            let claude: Value =
                serde_json::from_slice(&fs::read(workspace.path().join(".mcp.json")).unwrap())
                    .unwrap();
            assert_eq!(claude["other"], true);
            assert_eq!(claude["mcpServers"]["other"]["command"], "other");
        }
    }

    #[test]
    fn rejects_invalid_existing_documents_without_writing() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join(".codex")).unwrap();
        let path = workspace.path().join(".codex/config.toml");
        fs::write(&path, "not = [valid").unwrap();
        let (bin, _) = fake_mcp();
        {
            assert!(
                plan_with_command(
                    workspace.path(),
                    None,
                    AgentSelection {
                        codex: true,
                        claude: false,
                    },
                    false,
                    bin.path().join("loom-mcp"),
                )
                .is_err()
            );
        }
        assert_eq!(fs::read_to_string(path).unwrap(), "not = [valid");
    }
}
