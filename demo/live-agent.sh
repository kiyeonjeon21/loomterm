#!/usr/bin/env bash
set -euo pipefail

: "${LOOMTERM_DEMO_BIN_DIR:?LOOMTERM_DEMO_BIN_DIR is required}"
: "${LOOMTERM_CONFIG:?LOOMTERM_CONFIG is required}"
: "${LOOMTERM_STATE_DIR:?LOOMTERM_STATE_DIR is required}"
: "${LOOMTERM_RUNTIME_DIR:?LOOMTERM_RUNTIME_DIR is required}"
: "${LOOMTERM_SESSION_ID:?LOOMTERM_SESSION_ID is required}"

hook_command="LOOMTERM_CONFIG=$LOOMTERM_CONFIG LOOMTERM_STATE_DIR=$LOOMTERM_STATE_DIR LOOMTERM_RUNTIME_DIR=$LOOMTERM_RUNTIME_DIR LOOMTERM_SESSION_ID=$LOOMTERM_SESSION_ID $LOOMTERM_DEMO_BIN_DIR/loom agent-event --provider codex"
prompt="Fix the failing outcome-classification test in this small repository. Keep the
public return shape and test unchanged. Use Loomterm MCP tools for every shell
command: inspect the files, run the focused test, and finish with a diff check.
Do not use the shell tool. Keep the final answer brief."

exec codex exec \
  --ephemeral \
  --dangerously-bypass-hook-trust \
  --color always \
  -s workspace-write \
  -c 'approval_policy="never"' \
  -c "hooks.UserPromptSubmit=[{hooks=[{type=\"command\",command=\"$hook_command\",timeout=5}]}]" \
  -c "hooks.PreToolUse=[{hooks=[{type=\"command\",command=\"$hook_command\",timeout=5}]}]" \
  -c "hooks.PostToolUse=[{hooks=[{type=\"command\",command=\"$hook_command\",timeout=5}]}]" \
  -c "hooks.Stop=[{hooks=[{type=\"command\",command=\"$hook_command\",timeout=5}]}]" \
  -c 'mcp_servers.loomterm.default_tools_approval_mode="approve"' \
  -c 'mcp_servers.loomterm.env_vars=["LOOMTERM_CONFIG","LOOMTERM_STATE_DIR","LOOMTERM_RUNTIME_DIR","LOOMTERM_SESSION_ID","LOOMTERM_AGENT_KIND"]' \
  -c "mcp_servers.loomterm.command=\"$LOOMTERM_DEMO_BIN_DIR/loom-mcp\"" \
  "$prompt"
