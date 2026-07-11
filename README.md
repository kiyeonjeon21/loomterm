# Loomterm

Loomterm is a local, durable, structured command runtime for coding agents. It
owns process lifecycles and exposes command input, separate stdout/stderr,
timestamps, cancellation, and terminal outcomes without requiring an agent to
scrape a terminal screen.

This first version is deliberately headless. It is not a terminal emulator, an
agent orchestrator, or an LLM client.

## What it provides

- A persistent `loomd` daemon with bounded concurrent execution.
- A private per-command supervisor that terminates the process group if `loomd` dies.
- Direct argv execution and explicit `/bin/sh -c` execution as distinct modes.
- Lossless stdout/stderr events with a daemon-assigned merged sequence.
- Durable command metadata, output, exit codes, and signals in SQLite WAL.
- Workspace-scoped cwd validation and same-user Unix socket access.
- Process-group cancellation with `SIGTERM` and `SIGKILL` escalation.
- A human CLI and an MCP stdio server over the same versioned core protocol.

Client disconnection does not stop a command. A daemon crash closes the private
supervisor control pipe, which sends `SIGTERM` and then `SIGKILL` to the command
process group. On restart, the durable record becomes `interrupted`; Loomterm
deliberately does not reattach to an unowned process.

## Build

```sh
cargo build --release
```

The build produces four binaries:

- `loomd`: execution daemon
- `loom`: CLI client
- `loom-mcp`: MCP stdio adapter
- `loom-supervisor`: private fail-closed process owner used by `loomd`

The CLI and MCP adapter start a sibling `loomd` automatically when needed.

## Quick start

Register a workspace explicitly:

```sh
target/release/loom workspace add . --name loomterm
```

Registration is idempotent for the same name and canonical root. `workspace
remove` deactivates command execution and project selection without deleting
durable history; adding the same workspace again reactivates its existing id.

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

`loom cancel` returns after the execution reaches a terminal `cancelled` state.
`loom daemon status` and `loom daemon stop` never start a missing daemon. After
upgrading the binaries, use `loom daemon restart`; it refuses to interrupt active
executions unless `--force` is explicit.

Use `--json` for structured output. `loom run --json` emits JSON Lines containing
the initial execution, each event, and the terminal result.

Summarize recent activity for one workspace:

```sh
target/release/loom stats --workspace loomterm --days 7
target/release/loom stats --workspace loomterm --days 7 --json
```

When `--workspace` is omitted, `loom stats` selects the most specific registered
workspace containing the current directory. Statistics are derived from the
existing local SQLite execution records; Loomterm does not send usage telemetry.

## MCP setup

This repository includes a project-scoped Codex configuration in
`.codex/config.toml`. Register the project once, then open Codex from this trusted
repository:

```sh
cargo build --release --bins
target/release/loom workspace add . --name loomterm
codex mcp get loomterm --json
```

The server exposes:

- `loom_run`
- `loom_get`
- `loom_read`
- `loom_wait`
- `loom_cancel`
- `loom_list`
- `loom_workspaces`

`loom-mcp` selects the most specific registered workspace containing its startup
directory. Its tool schemas do not accept a workspace parameter, and every
execution-id operation verifies that the record belongs to that one project.
Startup fails with a registration command when no workspace contains the project.

MCP output defaults to text with a 256 KiB raw-byte budget. Invalid UTF-8 is
reported with `lossy: true`; callers can request `output_format = "base64"` for
the exact bytes. Tool responses expose `next_seq` and `has_more` for paging.

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
# Optional development override; production builds find the sibling binary.
supervisor_path = "/absolute/path/to/loom-supervisor"
```

Environment override values and initial stdin are never persisted. Only
environment key names are recorded for auditability.

## Protocol semantics

Each execution has a UUIDv7 id and progresses through `queued`, `running`, and a
terminal state. A non-zero command exit remains a successfully observed
`exited { code }` outcome rather than a runtime failure.

Output is exact within each stdout/stderr stream. The cross-stream sequence is
the order in which the supervisor observes chunks from the two pipes; operating
systems do not provide a stronger total ordering across separate file descriptors.

The internal Unix socket uses length-prefixed JSON protocol v2 with tagged
request, response, and event envelopes. Daemon health advertises its build
version, active execution count, and additive protocol capabilities so a newer
client can request an explicit restart instead of replacing a running daemon.
`Subscribe { execution_id, after_seq }`
replays durable events after the cursor and then pushes live events on the same
connection. SQLite tables remain authoritative, so a reconnect can resume
without gaps or duplicates.

SQLite runs on a dedicated storage actor thread. Async execution and socket tasks
use a bounded queue and output batches instead of performing synchronous database
work on Tokio workers. Statistics use bounded SQL aggregates rather than loading
command and environment metadata for every matching execution.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
scripts/codex-smoke.sh
```

The integration suite also kills `loomd` with `SIGKILL`, verifies that the
supervisor removes the command group, proves queued work cannot spawn during
graceful shutdown, and checks cursor-exact subscription reconnects.

Use [DOGFOOD.md](DOGFOOD.md) to run a focused local evaluation before choosing
the next product investment.

## Trust boundary

Workspace registration constrains `cwd` and MCP record selection. It is not an
OS filesystem or network sandbox. Commands inherit the permissions and baseline
environment of the local user running `loomd`; only use Loomterm with trusted
agents and review destructive tool calls. Environment override values and stdin
are not persisted, but command processes can access resources available to that
user.

## Current scope

macOS and Linux are the current targets. PTY/TUI control, SSH, remote daemons, GUI,
ACP hosting, and model orchestration are intentionally deferred. A future PTY
mode should extend the same execution/event model with terminal snapshots,
input-required events, and explicit human/agent handoff.
