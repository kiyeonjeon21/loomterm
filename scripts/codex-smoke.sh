#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/loomterm-codex.XXXXXX")
export LOOMTERM_STATE_DIR="$TMP/state"
export LOOMTERM_RUNTIME_DIR="$TMP/run"
export LOOMTERM_CONFIG="$TMP/config.toml"

cleanup() {
  "$ROOT/target/release/loom" daemon stop >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

printf 'cancel_grace_ms = 100\n' >"$LOOMTERM_CONFIG"
cd "$ROOT"
cargo build --release --bins
target/release/loom workspace add . --name loomterm-codex-smoke >/dev/null

cat >"$TMP/result-schema.json" <<'EOF'
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "stdout": { "type": "string" },
    "stderr": { "type": "string" },
    "exit_code": { "type": "integer" },
    "execution_state": { "type": "string" },
    "used_loomterm": { "type": "boolean" },
    "long_execution_cancelled": { "type": "boolean" }
  },
  "required": ["stdout", "stderr", "exit_code", "execution_state", "used_loomterm", "long_execution_cancelled"]
}
EOF

codex exec \
  --ephemeral \
  --json \
  -s workspace-write \
  -c 'approval_policy="never"' \
  -c 'mcp_servers.loomterm.default_tools_approval_mode="approve"' \
  --output-schema "$TMP/result-schema.json" \
  -o "$TMP/result.json" \
  - <<'EOF' | tee "$TMP/events.jsonl"
Use the Loomterm MCP tools, not the shell tool, to execute this exact shell command:
printf loom-out; printf loom-err >&2; exit 7

Use text output. Return stdout, stderr, the observed exit code, terminal execution
state, and used_loomterm=true. Then start `sleep 30` with Loomterm using a very
short wait, cancel that execution with loom_cancel, and use loom_wait or loom_get
to verify its terminal state. Return long_execution_cancelled=true only after it
is terminal and cancelled. Do not edit files.
EOF

grep -q 'loom_run' "$TMP/events.jsonl"
grep -q 'loom_cancel' "$TMP/events.jsonl"
grep -Eq '"stdout"[[:space:]]*:[[:space:]]*"loom-out"' "$TMP/result.json"
grep -Eq '"stderr"[[:space:]]*:[[:space:]]*"loom-err"' "$TMP/result.json"
grep -Eq '"exit_code"[[:space:]]*:[[:space:]]*7' "$TMP/result.json"
grep -Eq '"execution_state"[[:space:]]*:[[:space:]]*"finished"' "$TMP/result.json"
grep -Eq '"used_loomterm"[[:space:]]*:[[:space:]]*true' "$TMP/result.json"
grep -Eq '"long_execution_cancelled"[[:space:]]*:[[:space:]]*true' "$TMP/result.json"
printf 'Codex Loomterm smoke test passed.\n'
