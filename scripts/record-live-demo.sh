#!/usr/bin/env bash
set -euo pipefail

timeout_seconds=${LOOMTERM_DEMO_TIMEOUT_SECONDS:-300}
codex_model=${LOOMTERM_DEMO_CODEX_MODEL:-gpt-5.6-sol}
if ! [[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]]; then
  echo "LOOMTERM_DEMO_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
fi

for command in codex claude tmux vhs ffmpeg ffprobe cwebp python3; do
  command -v "$command" >/dev/null || {
    echo "$command is required" >&2
    exit 2
  }
done

repo=$(cd "$(dirname "$0")/.." && pwd)
root=$(mktemp -d "${TMPDIR:-/tmp}/loomterm-handoff-demo.XXXXXX")
fixture="$root/fixture"
bin_dir="$root/bin"
tmux_session=loomterm-live-demo
export LOOMTERM_STATE_DIR="$root/state"
export LOOMTERM_RUNTIME_DIR="$root/run"
export LOOMTERM_CONFIG="$root/config.toml"
controller_pid=
scroll_pid=
left=
right=

cleanup() {
  [[ -z "$controller_pid" ]] || kill "$controller_pid" 2>/dev/null || true
  [[ -z "$scroll_pid" ]] || kill "$scroll_pid" 2>/dev/null || true
  tmux kill-session -t "$tmux_session" 2>/dev/null || true
  "$repo/target/release/loom" daemon stop --force >/dev/null 2>&1 || true
  rm -rf "$root"
}
trap cleanup EXIT

diagnose() {
  echo "handoff demo recording failed; current panes:" >&2
  [[ -z "$left" ]] || tmux capture-pane -p -S -160 -t "$left" 2>/dev/null >&2 || true
  [[ -z "$right" ]] || tmux capture-pane -p -S -160 -t "$right" 2>/dev/null >&2 || true
}

wait_for_agent_tui() {
  local agent=$1
  local title_pattern
  local ready_pattern
  local deadline=$((SECONDS + timeout_seconds))
  local screen

  if [[ "$agent" == codex ]]; then
    title_pattern='OpenAI Codex'
    ready_pattern='OpenAI Codex'
  else
    title_pattern='Claude Code v'
    ready_pattern='Claude Code v'
  fi
  while ((SECONDS < deadline)); do
    screen=$(tmux capture-pane -p -S -160 -t "$left" 2>/dev/null)
    if grep -Eq "$title_pattern" <<<"$screen" \
      && grep -Fq "$ready_pattern" <<<"$screen"; then
      sleep 0.5
      return 0
    fi
    if grep -Eqi 'Do you trust (the contents|the files)|Is this a project you created or one you trust' <<<"$screen"; then
      tmux send-keys -t "$left" Enter
      sleep 0.5
      continue
    fi
    sleep 0.25
  done
  return 1
}

recording_session_id() {
  local agent=$1
  "$bin_dir/loom" --json session list --workspace live-demo | python3 -c '
import json, sys
agent = sys.argv[1]
for session in json.load(sys.stdin):
    if session.get("state") == "recording" and session.get("agent_kind") == agent:
        print(session["id"])
        break
' "$agent"
}

turn_state() {
  "$bin_dir/loom" --json session get "$1" | python3 -c '
import json, sys
turns = json.load(sys.stdin).get("turns", [])
print(turns[-1].get("state", "") if turns else "")
'
}

session_state() {
  "$bin_dir/loom" --json session get "$1" | python3 -c '
import json, sys
print(json.load(sys.stdin)["session"]["state"])
'
}

wait_for_session_id() {
  local agent=$1
  local id=
  local deadline=$((SECONDS + timeout_seconds))
  while [[ -z "$id" ]] && ((SECONDS < deadline)); do
    id=$(recording_session_id "$agent")
    [[ -n "$id" ]] || sleep 0.25
  done
  [[ -n "$id" ]] || return 1
  printf '%s\n' "$id"
}

wait_for_turn() {
  local session_id=$1
  local state=
  local deadline=$((SECONDS + timeout_seconds))
  while ((SECONDS < deadline)); do
    state=$(turn_state "$session_id")
    if [[ "$state" == completed || "$state" == failed ]]; then
      printf '%s\n' "$state"
      return 0
    fi
    sleep 0.5
  done
  return 1
}

wait_for_session_end() {
  local session_id=$1
  local deadline=$((SECONDS + timeout_seconds))
  while ((SECONDS < deadline)); do
    [[ $(session_state "$session_id") != recording ]] && return 0
    sleep 0.25
  done
  return 1
}

stop_interactive_agent() {
  tmux send-keys -l -t "$left" "/exit"
  tmux send-keys -t "$left" Enter
  sleep 0.25
  tmux send-keys -t "$left" Enter
}

agent_arguments() {
  local agent=$1
  local -a argv
  if [[ "$agent" == codex ]]; then
    argv=(
      --yolo
      --dangerously-bypass-hook-trust
      --model "$codex_model"
      -c 'model_reasoning_effort="xhigh"'
    )
  else
    argv=(--dangerously-skip-permissions)
  fi
  printf '%q ' "${argv[@]}"
}

start_agent() {
  local agent=$1
  local name=$2
  local delay=$3
  local command
  command=$(agent_arguments "$agent")
  tmux send-keys -l -t "$left" \
    "sleep $delay; loom agent --name $name $agent -- $command"
  tmux send-keys -t "$left" Enter
}

start_handoff() {
  local source_session=$1
  local delay=$2
  local command
  command=$(agent_arguments claude)
  tmux send-keys -l -t "$left" \
    "sleep $delay; loom handoff --from $source_session --name claude-handoff claude -- $command"
  tmux send-keys -t "$left" Enter
}

switch_observer() {
  tmux send-keys -t "$right" q
  sleep 1
  tmux send-keys -t "$right" clear Enter
  tmux send-keys -l -t "$right" "printf 'PHASE 2  CLAUDE TAKES OVER\\n'; sleep 2; loom watch --active"
  tmux send-keys -t "$right" Enter
}

control_handoff() {
  local source_session
  local target_session
  local execution_id
  local state

  wait_for_agent_tui codex || {
    echo "Codex TUI did not become ready" >&2
    return 1
  }
  source_session=$(wait_for_session_id codex) || {
    echo "Codex recording session was not created" >&2
    return 1
  }
  tmux send-keys -l -t "$left" "$source_prompt"
  tmux send-keys -t "$left" Enter
  sleep 0.5
  [[ -n $(turn_state "$source_session") ]] || tmux send-keys -t "$left" Enter
  state=$(wait_for_turn "$source_session") || {
    echo "Codex turn did not finish" >&2
    return 1
  }
  [[ "$state" == completed ]] || {
    echo "Codex turn failed" >&2
    return 1
  }
  execution_id=$("$bin_dir/loom" --json session get "$source_session" | python3 -c '
import json, sys
detail = json.load(sys.stdin)
for execution in detail.get("executions", []):
    command = execution.get("command", {})
    if command.get("program") == "python3" and "handoff_worker.py" in command.get("args", []):
        if execution.get("state") == "running":
            print(execution["id"])
            break
')
  [[ -n "$execution_id" ]] || {
    echo "Codex did not leave a running handoff execution" >&2
    return 1
  }

  sleep 3
  stop_interactive_agent
  wait_for_session_end "$source_session" || {
    echo "Codex session did not exit" >&2
    return 1
  }
  [[ $("$bin_dir/loom" --json get "$execution_id" | python3 -c 'import json,sys; print(json.load(sys.stdin)["state"])') == running ]] || {
    echo "handoff execution stopped with the Codex session" >&2
    return 1
  }

  tmux send-keys -t "$left" clear Enter
  tmux send-keys -l -t "$left" "printf 'PHASE 2  CLAUDE TAKES OVER\\n'"
  tmux send-keys -t "$left" Enter
  start_handoff "$source_session" 2
  wait_for_agent_tui claude || {
    echo "Claude TUI did not become ready" >&2
    return 1
  }
  target_session=$(wait_for_session_id claude) || {
    echo "Claude recording session was not created" >&2
    return 1
  }
  switch_observer
  state=$(wait_for_turn "$target_session") || {
    echo "Claude turn did not finish" >&2
    return 1
  }
  [[ "$state" == completed ]] || {
    echo "Claude turn failed" >&2
    return 1
  }
  [[ $("$bin_dir/loom" --json get "$execution_id" | python3 -c 'import json,sys; print(json.load(sys.stdin)["state"])') == cancelled ]] || {
    echo "Claude did not cancel the handoff execution" >&2
    return 1
  }

  sleep 4
  stop_interactive_agent
  wait_for_session_end "$target_session" || {
    echo "Claude session did not exit" >&2
    return 1
  }
}

tmux kill-session -t "$tmux_session" 2>/dev/null || true
cargo build --manifest-path "$repo/Cargo.toml" --release --bins
"$repo/scripts/create-demo-fixture.sh" "$fixture" >/dev/null
mkdir -p "$bin_dir"
ln -s "$repo/target/release/loom" "$bin_dir/loom"
ln -s "$repo/target/release/loom-mcp" "$bin_dir/loom-mcp"
ln -s "$repo/target/release/loomd" "$bin_dir/loomd"
ln -s "$repo/target/release/loom-supervisor" "$bin_dir/loom-supervisor"
PATH="$bin_dir:$PATH" "$bin_dir/loom" init "$fixture" --name live-demo --agent both >/dev/null
printf '\n.codex/\n.claude/\n.mcp.json\n' >> "$fixture/.git/info/exclude"

source_prompt="Start python3 -u handoff_worker.py as a durable long-running task for a handoff demo. Wait only long enough to confirm it is running, report its full execution ID, and stop. Do not wait for completion or cancel it. The next agent should inspect its progress and cancel it safely. Keep the final answer brief."

tmux new-session -d -x 160 -y 48 -s "$tmux_session" -n demo
tmux set-option -t "$tmux_session" remain-on-exit on
left=$(tmux display-message -p -t "$tmux_session":demo '#{pane_id}')
right=$(tmux split-window -h -P -F '#{pane_id}' -t "$left")
setup="cd '$fixture' && unset NO_COLOR && export TERM=xterm-256color COLORTERM=truecolor PATH='$bin_dir':\"\$PATH\" LOOMTERM_STATE_DIR='$LOOMTERM_STATE_DIR' LOOMTERM_RUNTIME_DIR='$LOOMTERM_RUNTIME_DIR' LOOMTERM_CONFIG='$LOOMTERM_CONFIG' && clear"

tmux send-keys -t "$left" "$setup" Enter
start_agent codex codex-starts-worker 3
tmux send-keys -t "$right" "$setup" Enter
tmux send-keys -t "$right" "sleep 3.5; loom watch --active" Enter
tmux select-layout -t "$tmux_session":demo even-horizontal >/dev/null

(
  rc=0
  control_handoff || rc=$?
  tmux detach-client -s "$tmux_session" 2>/dev/null || true
  exit "$rc"
) &
controller_pid=$!
(
  sleep 12
  while tmux has-session -t "$tmux_session" 2>/dev/null; do
    tmux send-keys -t "$right" Down Down Down
    sleep 8
  done
) &
scroll_pid=$!

cd "$repo"
python3 - "$root/demo.mp4" demo/live.tape "$root/live.tape" "$timeout_seconds" <<'PY'
import pathlib
import sys

output, source, destination, timeout = sys.argv[1:]
tape = pathlib.Path(source).read_text(encoding="utf-8")
tape = tape.replace("Set WaitTimeout 180s", f"Set WaitTimeout {timeout}s")
pathlib.Path(destination).write_text(f'Output "{output}"\n{tape}', encoding="utf-8")
PY
if ! vhs "$root/live.tape"; then
  diagnose
  exit 1
fi
if ! wait "$controller_pid"; then
  controller_pid=
  diagnose
  exit 1
fi
controller_pid=

sessions_json="$root/sessions.json"
"$bin_dir/loom" --json session list --workspace live-demo > "$sessions_json"
source_session=$(python3 - "$sessions_json" <<'PY'
import json, sys
for session in json.load(open(sys.argv[1], encoding="utf-8")):
    if session.get("agent_kind") == "codex":
        print(session["id"])
        break
PY
)
target_session=$(python3 - "$sessions_json" <<'PY'
import json, sys
for session in json.load(open(sys.argv[1], encoding="utf-8")):
    if session.get("agent_kind") == "claude":
        print(session["id"])
        break
PY
)
[[ -n "$source_session" && -n "$target_session" ]] || {
  echo "both handoff sessions were not found" >&2
  exit 1
}
source_json="$root/source.json"
target_json="$root/target.json"
"$bin_dir/loom" --json session get "$source_session" > "$source_json"
"$bin_dir/loom" --json session get "$target_session" > "$target_json"
python3 - "$source_json" "$target_json" "$source_prompt" <<'PY'
import json
import sys

source = json.load(open(sys.argv[1], encoding="utf-8"))
target = json.load(open(sys.argv[2], encoding="utf-8"))
source_prompt = sys.argv[3]
assert source["session"]["state"] == "finished"
assert target["session"]["state"] == "finished"
assert any(turn["state"] == "completed" and turn["prompt"] == source_prompt for turn in source["turns"])
target_turns = [turn for turn in target["turns"] if turn["state"] == "completed"]
assert len(target_turns) == 1
target_prompt = target_turns[0]["prompt"]
assert target_prompt.startswith("Take over durable Loomterm work from the previous codex session")
assert source["session"]["id"] in target_prompt
assert len(source["executions"]) == 1
execution = source["executions"][0]
assert execution["id"] in target_prompt
assert execution["state"] == "cancelled"
assert execution["initiator"]["session_id"] == source["session"]["id"]
linked = [item for item in target["executions"] if item["id"] == execution["id"]]
assert len(linked) == 1
assert linked[0]["state"] == "cancelled"
assert any(action.get("execution_id") == execution["id"] for action in target["actions"])
assert all(action["tool_name"].lower() != "bash" for action in source["actions"])
assert source["session"]["ended_at_ms"] <= target["session"]["created_at_ms"]
print(execution["id"])
PY

status=$(git -C "$fixture" status --short)
if [[ -n "$status" ]]; then
  echo "unexpected fixture changes:" >&2
  printf '%s\n' "$status" >&2
  exit 1
fi

cp "$LOOMTERM_STATE_DIR/sessions/$source_session/replay.html" "$root/replay-codex.html"
cp "$LOOMTERM_STATE_DIR/sessions/$target_session/replay.html" "$root/replay.html"
poster_png="$root/poster.png"
duration=$(ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 "$root/demo.mp4")
poster_at=$(python3 - "$duration" <<'PY'
import sys
duration = float(sys.argv[1])
print(max(1.0, min(duration - 1.0, duration * 0.82)))
PY
)
ffmpeg -y -ss "$poster_at" -i "$root/demo.mp4" -frames:v 1 -vf 'scale=1600:-2' "$poster_png" \
  >/dev/null 2>&1
cwebp -quiet -q 82 "$poster_png" -o "$root/poster.webp"
cp "$root/demo.mp4" "$repo/docs/demo.mp4"
cp "$root/poster.webp" "$repo/docs/poster.webp"
cp "$root/replay.html" "$repo/docs/replay.html"
cp "$root/replay-codex.html" "$repo/docs/replay-codex.html"
echo "recorded Codex-to-Claude handoff demo"
