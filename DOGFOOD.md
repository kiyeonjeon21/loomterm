# Loomterm Focused Dogfood Protocol

Use one 30-45 minute session of real development work to choose Loomterm's next
product investment. This is a local evaluation, not a benchmark and not a
telemetry program.

## Setup

Build the current revision and register the repository once:

```sh
cargo build --release --bins
target/release/loom workspace add . --name loomterm
target/release/loom doctor
```

Capture the starting baseline:

```sh
target/release/loom stats --workspace loomterm --days 1 --json > /tmp/loomterm-focused-dogfood-start.json
```

## Focused run

Run at least 12 commands through Loomterm across at least five task types. Use
both the CLI and a real coding agent connected through `loom-mcp`.

- repository discovery and source inspection
- builds and dependency checks
- unit or integration tests
- formatting and linting
- version-control or release checks

Exercise these lifecycle paths during the same session:

- direct argv and explicit shell execution
- an intentional non-zero exit
- a detached command followed by a cursor-based wait or log reconnect
- cancellation followed by terminal-state verification
- one controlled interactive-input probe

The controlled probe documents the current pipe-based boundary. It does not
count as evidence for prioritizing PTY support unless a real task is also
blocked or materially degraded.

For every observed friction, record the task, signal, workaround, and product
implication. Missing, duplicated, reordered, or inaccessible output is a
reliability issue, not normal friction.

## Decision gates

Capture the final summary with `loom stats --workspace loomterm --days 1` and
choose the next investment in this order:

1. Fix runtime reliability if any command is lost, duplicated, orphaned, or
   cannot be resumed correctly.
2. Prioritize PTY, input-required events, and human handoff when at least two
   real tasks are blocked or materially degraded by interactive input.
3. Prioritize a sandbox-policy spike when the trusted workflow exposes a
   concrete permission or isolation blocker.
4. Otherwise prioritize packaging, installation, and onboarding.

The summary contains no command text, output contents, environment values, or
stdin. All source records and derived statistics remain in the local Loomterm
database; nothing is sent to an external service.

## Focused run: 2026-07-12

This automated probe spanned about 2 minutes 31 seconds, so it did not satisfy
the 30-45 minute real-work protocol above. It started from zero execution
records and covered discovery,
dependency metadata, formatting, linting, tests, release builds, version
control, direct argv, explicit shell, non-zero outcomes, cancellation, and
cursor-based reconnects.

| Metric | Result |
| --- | ---: |
| Total executions | 15 |
| Exited zero | 11 |
| Expected non-zero | 2 |
| Cancelled | 2 |
| CLI / MCP initiators | 10 / 5 |
| Captured output | 10,526 bytes |
| Truncated executions | 0 |
| Duration samples | 15 |
| Duration p50 / p95 | 71 ms / 14,156 ms |

The p95 includes a deliberate `sleep 30` that remained active while Codex
planned its next MCP call. Once cancellation was requested, the command reached
the terminal `cancelled` state and was available through `loom_wait` or
`loom get`.

| Task | Signal | Workaround | Product implication |
| --- | --- | --- | --- |
| MCP direct and shell execution | Structured stdout and outcomes matched exactly | None | Core structured execution is usable |
| Cursor reconnect | Delayed `reconnect-ok` output replayed once with contiguous sequence numbers | None | No reconnect defect observed |
| Cancellation | The evaluated build could acknowledge while the returned record still said `running` | Follow with `loom wait` or `loom get` | Resolved after the run: cancel now waits for the terminal record |
| Interactive probe | A shell `read` received EOF and exited 1 without a PTY | Use non-interactive flags or initial stdin | Known capability gap, but no real task was blocked |
| Initial activation | Use still depends on a source build, workspace registration, and project MCP configuration | Follow the repository setup steps | Packaging and onboarding are now the largest observed product gap |

No output loss, duplication, orphan process, truncation, or interactive blocker
appeared in the probe. This makes **packaging, installation, and onboarding** the
leading hypothesis, not a final product decision. PTY and sandbox priority still
requires a real focused session rather than controlled coverage alone.

## Public preview validation: 2026-07-12

The v0.3.0 public preview was installed from `kiyeonjeon21/tap/loomterm` on an
Apple Silicon Mac. The Homebrew test exercised all four packaged binaries. A
clean fixture then used `loom init` to register one workspace and generate both
project MCP configurations with `/opt/homebrew/bin/loom-mcp`.

Codex and Claude Code each fixed the same failing outcome-classification test,
then ran separate syntax, test, and diff validation commands. Every shell
command was issued through `loom_run` inside a recorded agent session.

| Metric | Result |
| --- | ---: |
| Recorded Codex / Claude sessions | 2 / 2 |
| Correlated Codex / Claude executions | 8 / 6 |
| Total executions | 14 |
| Exited zero / intentional non-zero | 11 / 3 |
| Captured output | 6,165 bytes |
| Truncated executions | 0 |
| Duration p50 / p95 | 177 ms / 641 ms |

No execution was lost, duplicated, orphaned, or linked to the wrong session.
The published Codex session lasted 39.1 seconds and correlated five commands;
the primary Claude session lasted 61.9 seconds and correlated three commands.

Observed onboarding and release friction:

- Codex requires the exact project path to be trusted before it loads
  `.codex/config.toml`. Non-interactive `codex exec` also needed an explicit
  approval override for the write-capable MCP tools.
- Claude correctly surfaced `.mcp.json` as pending until the user approved it.
  Its schema loader warns that Rust integer formats such as `uint64` are
  unknown, but it still registers and executes the tools.
- A restricted Claude `--tools` list omitted the dynamic MCP tool in one
  headless run. Loading the approved `.mcp.json` explicitly with
  `--strict-mcp-config` was reliable.
- The first release dry-run exposed missing `--version` handling in the three
  service binaries. The archive smoke test caught this before tagging.
- Current Homebrew requires nested OS and architecture blocks for the binary
  formula. `brew style`, `brew test`, and a clean install passed after the
  generator was corrected.

This was a focused release validation, not the planned 20-30 minute real-work
session per agent. It validates packaging, onboarding, recording, correlation,
redaction, and replay publication, but it does not satisfy the evidence gate for
choosing input-required, human handoff, sandboxing, or a GUI as the next product
investment.
