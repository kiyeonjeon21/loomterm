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

The session started from zero execution records and covered discovery,
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

No output loss, duplication, orphan process, truncation, or real interactive
blocker was observed. The decision gates therefore select
**packaging, installation, and onboarding** as the next product slice. PTY and
sandbox work remain deferred until real usage supplies a blocker.
