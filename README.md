# Loomterm

Loomterm is a local, durable, structured command runtime for coding agents. It
owns process lifecycles and exposes command input, separate stdout/stderr,
timestamps, cancellation, and terminal outcomes without requiring an agent to
scrape a terminal screen.

This first version is deliberately headless. It is not a terminal emulator, an
agent orchestrator, or an LLM client.

## What it provides

- A persistent `loomd` daemon with bounded concurrent execution.
- Direct argv execution and explicit `/bin/sh -c` execution as distinct modes.
- Lossless stdout/stderr events with a daemon-assigned merged sequence.
- Durable command metadata, output, exit codes, and signals in SQLite WAL.
- Workspace-scoped cwd validation and same-user Unix socket access.
- Process-group cancellation with `SIGTERM` and `SIGKILL` escalation.
- A human CLI and an MCP stdio server over the same versioned core protocol.

Client disconnection does not stop a command. A daemon crash currently marks
incomplete records as `interrupted`; v0 does not reattach to surviving processes.

## Build

```sh
cargo build --release
```

The build produces three binaries:

- `loomd`: execution daemon
- `loom`: CLI client
- `loom-mcp`: MCP stdio adapter

The CLI and MCP adapter start a sibling `loomd` automatically when needed.

## Quick start

Register a workspace explicitly:

```sh
target/release/loom workspace add . --name loomterm
```

Run a direct command. The CLI streams the original stdout/stderr and exits with
the child command's exit code:

```sh
target/release/loom run --workspace loomterm -- printf 'hello\n'
```

Use shell mode only when shell syntax is required:

```sh
target/release/loom run --workspace loomterm --shell 'printf out; printf err >&2; exit 7'
```

Start a command without waiting, then reconnect using its execution id:

```sh
target/release/loom run --workspace loomterm --detach -- sleep 30
target/release/loom list --workspace loomterm
target/release/loom logs --follow EXECUTION_ID
target/release/loom cancel EXECUTION_ID
```

Use `--json` for structured output. `loom run --json` emits JSON Lines containing
the initial execution, each event, and the terminal result.

## MCP setup

Point an MCP client at the absolute `loom-mcp` binary path:

```json
{
  "mcpServers": {
    "loomterm": {
      "command": "/absolute/path/to/loomterm/target/release/loom-mcp"
    }
  }
}
```

The server exposes:

- `loom_run`
- `loom_get`
- `loom_read`
- `loom_wait`
- `loom_cancel`
- `loom_list`
- `loom_workspaces`

When a client advertises MCP roots, `loom-mcp` intersects them with Loomterm's
explicit workspace registry. Clients without roots can still use registered
workspaces; the daemon remains restricted to those workspace roots.

## Configuration

Loomterm uses platform-native config, state, and runtime directories. Override
them for tests or isolated installations with:

- `LOOMTERM_CONFIG`
- `LOOMTERM_STATE_DIR`
- `LOOMTERM_RUNTIME_DIR`

Example `config.toml`:

```toml
max_concurrent_executions = 8
capture_limit_bytes = 268435456
retention_days = 7
retention_bytes = 1073741824
cancel_grace_ms = 2000
shell = "/bin/sh"
```

Environment override values and initial stdin are never persisted. Only
environment key names are recorded for auditability.

## Protocol semantics

Each execution has a UUIDv7 id and progresses through `queued`, `running`, and a
terminal state. A non-zero command exit remains a successfully observed
`exited { code }` outcome rather than a runtime failure.

Output is exact within each stdout/stderr stream. The cross-stream sequence is
the order in which `loomd` receives chunks from the two pipes; operating systems
do not provide a stronger total ordering across separate file descriptors.

The internal Unix socket uses length-prefixed JSON protocol v1. SQLite tables
for workspaces, executions, and events are the authoritative state.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

The integration suite covers separate output streams, non-zero exits, workspace
escape rejection, capture truncation, process-group cancellation, and reconnecting
through a fresh client connection.

## Current scope

macOS and Linux are the v0 targets. PTY/TUI control, SSH, remote daemons, GUI,
ACP hosting, and model orchestration are intentionally deferred. A future PTY
mode should extend the same execution/event model with terminal snapshots,
input-required events, and explicit human/agent handoff.

