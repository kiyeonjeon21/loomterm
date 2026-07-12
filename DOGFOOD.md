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

## Live observer validation: 2026-07-12

The v0.4.0 candidate added a read-only terminal observer over the existing
session and execution APIs. A tmux PTY smoke ran an active recording beside
`loom watch --active`, verified incremental stdout, sent `q`, and confirmed raw
mode and the alternate screen were restored. Wide and narrow layouts also run
against Ratatui's deterministic test backend.

The published 50-second demo records a real Codex run and observes nine
correlated MCP executions. It includes an intentional failed discovery command,
successful inspection and test commands, the final session transition to
`finished`, and selected output from the durable event stream. No protocol or
database migration was required.

This closes the immediate demo visibility gap without claiming a GUI terminal.
The observer cannot mirror the agent screen, supply input, hand control to a
human, subscribe to session events, or operate remotely. Those boundaries remain
evidence-gated follow-up work.

## Agent turn timeline validation: 2026-07-12

The v0.5.0 candidate adds an observation channel for the part the PTY demo did
not explain: which user request caused the visible agent work. `loom init` now
merges provider lifecycle hooks beside the existing MCP configuration. Codex
and Claude Code hook payloads normalize into the same turn/action records, while
the daemon-owned execution remains the process source of truth.

Unit coverage exercises Codex events with explicit turn ids, Claude Code events
without turn ids, out-of-band tool completion, MCP execution-id extraction,
SQLite v1-to-v4 migration, and idempotent preservation of unrelated project
hook settings. The full unit, binary, and daemon integration suite passes. The
live demo workflow now initializes the fixture's Codex hooks before recording,
so the observer and HTML replay show the request, tool actions, and executions
instead of relying on the viewer to infer the request from the agent screen.

The adapter is intentionally best effort. It prefers the recorder environment
and falls back to the provider hook's cwd plus the newest matching active
recording. Unsupported events, malformed input, no matching recording, and
daemon unavailability produce no hook decision and do not interrupt the
provider. Prompt and final assistant text are sensitive local records and must
be included in export review and redaction.

## Cross-agent handoff validation: 2026-07-12

The handoff launcher records two real interactive sessions in one workspace.
The user asks `loom agent codex` to start `python3 -u handoff_worker.py` without
naming Loomterm or an MCP tool. Strict routing keeps native Bash from executing,
and Codex reports durable execution `019f56b9-ccda-7332-b913-c35bf6b3e17c`
before exiting while the worker remains running. `loom handoff claude` then
injects the source request and execution metadata. Claude lists that same
execution, reads its accumulated checkpoints, cancels it, and verifies the
`cancelled` state without starting a replacement process or using native Bash.

The 80-second capture and both HTML replays passed automated assertions for the
exact prompts, completed turns, shared execution ID, source-session ownership,
target-session action link, final state, session ordering, and a clean fixture
worktree. The workflow also asserts that no native Bash action was recorded in
the source session. Store validation rejects an execution link from another
workspace; the runtime integration test exercises takeover and cancellation
against a real daemon. This validates a local launcher-driven continuity flow
between supported agents. It does not yet validate external demand, remote
handoff, autonomous scheduling, or a GUI terminal as the next product
investment.
