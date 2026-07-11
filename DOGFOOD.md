# Loomterm Dogfood Protocol

Use real development work to decide Loomterm's next product investment. This is
a seven-day local evaluation, not a benchmark and not a telemetry program.

## Setup

Build the current revision and register the repository once:

```sh
cargo build --release --bins
target/release/loom workspace add . --name loomterm
target/release/loom doctor
```

Capture the starting baseline:

```sh
target/release/loom stats --workspace loomterm --days 7 --json > /tmp/loomterm-stats-start.json
```

## Seven-day run

Run at least 20 real commands through Loomterm across at least five task types:

- repository discovery and source inspection
- builds and dependency checks
- unit or integration tests
- formatting and linting
- version-control or release checks

Do not add synthetic commands just to reach the count. Include normal failures
and corrections. During the run, exercise these lifecycle paths when real work
makes them relevant:

- a successful direct argv command
- a non-zero exit
- an explicit shell command for pipes, redirects, or shell expansion
- a detached command followed by `list` or `logs --follow`
- cancellation of a long-running command

For every task that is blocked or materially degraded, add one row here:

| Date | Task | Signal | Workaround | Product implication |
| --- | --- | --- | --- | --- |
| | | shell fallback / interactive prompt / cancel-reconnect / ambiguity | | |

Record a shell fallback only when shell syntax was necessary. Record an
interactive prompt when the lack of a PTY or follow-up stdin changed the task,
even if a non-interactive flag provided a workaround. Record any missing,
duplicated, reordered, or inaccessible output as a reliability issue.

## Review

At the end of day seven, capture the same local summary:

```sh
target/release/loom stats --workspace loomterm --days 7
target/release/loom stats --workspace loomterm --days 7 --json > /tmp/loomterm-stats-end.json
```

Choose the next investment using the collected evidence:

- Fix lifecycle or output reliability before adding product surface if any real
  command is lost, duplicated, orphaned, or cannot be resumed correctly.
- Prioritize PTY and explicit human/agent handoff if at least three real tasks are
  blocked or materially degraded by interactive input.
- Prioritize packaging, installation, and sandbox policy if at least 90% of tasks
  complete through the structured runtime and interactive input is not a blocker.
- Continue the evaluation instead of choosing a direction if fewer than 20 real
  commands or five task types were observed.

The summary includes counts, outcomes, initiator kinds, captured byte totals,
truncation counts, and terminal duration percentiles. It contains no command
text, output contents, environment values, or stdin. All source records and
derived statistics remain in the local Loomterm database; nothing is sent to an
external service.
