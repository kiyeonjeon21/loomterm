#!/usr/bin/env bash
set -euo pipefail

: "${LOOMTERM_DEMO_BIN_DIR:?LOOMTERM_DEMO_BIN_DIR is required}"

exec codex exec \
  --ephemeral \
  --color always \
  -s workspace-write \
  -c 'approval_policy="never"' \
  -c 'mcp_servers.loomterm.default_tools_approval_mode="approve"' \
  -c 'mcp_servers.loomterm.env_vars=["LOOMTERM_CONFIG","LOOMTERM_STATE_DIR","LOOMTERM_RUNTIME_DIR","LOOMTERM_SESSION_ID","LOOMTERM_AGENT_KIND"]' \
  -c "mcp_servers.loomterm.command=\"$LOOMTERM_DEMO_BIN_DIR/loom-mcp\"" \
  - <<'EOF'
Fix the failing outcome-classification test in this small repository. Keep the
public return shape and test unchanged. Use Loomterm MCP tools for every shell
command: inspect the files, run the focused test, and finish with a diff check.
Do not use the shell tool. Keep the final answer brief.
EOF
