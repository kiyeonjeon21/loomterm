#!/usr/bin/env bash
set -euo pipefail

agent=${LOOMTERM_DEMO_AGENT:-codex}
timeout_seconds=${LOOMTERM_DEMO_TIMEOUT_SECONDS:-180}
codex_model=${LOOMTERM_DEMO_CODEX_MODEL:-gpt-5.6-sol}

case "$agent" in
  codex | claude) ;;
  *)
    echo "LOOMTERM_DEMO_AGENT must be codex or claude" >&2
    exit 2
    ;;
esac
if ! [[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]]; then
  echo "LOOMTERM_DEMO_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
fi

for command in "$agent" tmux vhs ffmpeg ffprobe cwebp python3; do
  command -v "$command" >/dev/null || {
    echo "$command is required" >&2
    exit 2
  }
done

repo=$(cd "$(dirname "$0")/.." && pwd)
root=$(mktemp -d "${TMPDIR:-/tmp}/loomterm-live-demo.XXXXXX")
fixture="$root/fixture"
bin_dir="$root/bin"
session=loomterm-live-demo
export LOOMTERM_STATE_DIR="$root/state"
export LOOMTERM_RUNTIME_DIR="$root/run"
export LOOMTERM_CONFIG="$root/config.toml"
controller_pid=
scroll_pid=

cleanup() {
  [[ -z "$controller_pid" ]] || kill "$controller_pid" 2>/dev/null || true
  [[ -z "$scroll_pid" ]] || kill "$scroll_pid" 2>/dev/null || true
  tmux kill-session -t "$session" 2>/dev/null || true
  "$repo/target/release/loom" daemon stop --force >/dev/null 2>&1 || true
  rm -rf "$root"
}
trap cleanup EXIT

diagnose() {
  echo "demo recording failed; current panes:" >&2
  tmux capture-pane -p -S -120 -t "$left" 2>/dev/null >&2 || true
  tmux capture-pane -p -S -120 -t "$right" 2>/dev/null >&2 || true
}

wait_for_agent_tui() {
  local title_pattern
  local deadline=$((SECONDS + timeout_seconds))
  local trust_confirmed=false

  if [[ "$agent" == codex ]]; then
    title_pattern='OpenAI Codex'
  else
    title_pattern='Claude Code'
  fi
  while ((SECONDS < deadline)); do
    screen=$(tmux capture-pane -p -S -120 -t "$left" 2>/dev/null)
    if [[ "$trust_confirmed" == false ]] \
      && grep -Eqi 'Do you trust (the contents|the files)' <<<"$screen"; then
      tmux send-keys -t "$left" Enter
      trust_confirmed=true
      sleep 0.25
      continue
    fi
    if grep -Eq "$title_pattern" <<<"$screen"; then
      return 0
    fi
    sleep 0.25
  done
  return 1
}

recording_session_id() {
  "$bin_dir/loom" --json session list --workspace live-demo | python3 -c '
import json, sys
sessions = json.load(sys.stdin)
for session in sessions:
    if session.get("state") == "recording":
        print(session["id"])
        break
'
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

stop_interactive_agent() {
  tmux send-keys -l -t "$left" "/exit"
  tmux send-keys -t "$left" Enter
  sleep 0.25
  tmux send-keys -t "$left" Enter
}

control_agent() {
  local session_id=
  local state=
  local deadline

  wait_for_agent_tui || {
    echo "$agent TUI did not become ready" >&2
    return 1
  }

  deadline=$((SECONDS + timeout_seconds))
  while [[ -z "$session_id" ]] && ((SECONDS < deadline)); do
    session_id=$(recording_session_id)
    [[ -n "$session_id" ]] || sleep 0.25
  done
  if [[ -z "$session_id" ]]; then
    echo "Loomterm recording session was not created" >&2
    return 1
  fi

  tmux send-keys -l -t "$left" "$prompt"
  tmux send-keys -t "$left" Enter

  deadline=$((SECONDS + timeout_seconds))
  while ((SECONDS < deadline)); do
    state=$(turn_state "$session_id")
    if [[ "$state" == completed || "$state" == failed ]]; then
      break
    fi
    sleep 0.5
  done
  if [[ "$state" != completed && "$state" != failed ]]; then
    echo "agent turn did not reach a terminal state" >&2
    stop_interactive_agent
    return 1
  fi

  sleep 4
  stop_interactive_agent
  deadline=$((SECONDS + timeout_seconds))
  while ((SECONDS < deadline)); do
    [[ $(session_state "$session_id") != recording ]] && break
    sleep 0.25
  done
  if [[ $(session_state "$session_id") == recording ]]; then
    echo "interactive agent did not exit" >&2
    return 1
  fi
  if [[ "$state" == failed ]]; then
    echo "agent turn failed" >&2
    return 1
  fi
}

tmux kill-session -t "$session" 2>/dev/null || true
cargo build --manifest-path "$repo/Cargo.toml" --release --bins
"$repo/scripts/create-demo-fixture.sh" "$fixture" >/dev/null
mkdir -p "$bin_dir"
ln -s "$repo/target/release/loom" "$bin_dir/loom"
ln -s "$repo/target/release/loom-mcp" "$bin_dir/loom-mcp"
ln -s "$repo/target/release/loomd" "$bin_dir/loomd"
ln -s "$repo/target/release/loom-supervisor" "$bin_dir/loom-supervisor"
"$bin_dir/loom" init "$fixture" --name live-demo --agent "$agent" >/dev/null
printf '\n.codex/\n.claude/\n.mcp.json\n' >> "$fixture/.git/info/exclude"

prompt="Fix the failing outcome-classification test in this small repository. Keep the public return shape and test unchanged. Use Loomterm MCP tools for every shell command: inspect the files, run the focused test, and finish with a diff check. Do not use the shell tool. Keep the final answer brief."
if [[ "$agent" == codex ]]; then
  agent_argv=(
    codex
    --yolo
    --dangerously-bypass-hook-trust
    --model "$codex_model"
    -c 'model_reasoning_effort="xhigh"'
  )
else
  agent_argv=(claude --dangerously-skip-permissions)
fi
printf -v agent_command ' %q' "${agent_argv[@]}"

tmux new-session -d -x 160 -y 48 -s "$session" -n demo
tmux set-option -t "$session" remain-on-exit on
left=$(tmux display-message -p -t "$session":demo '#{pane_id}')
right=$(tmux split-window -h -P -F '#{pane_id}' -t "$left")
setup="cd '$fixture' && export PATH='$bin_dir':\"\$PATH\" LOOMTERM_STATE_DIR='$LOOMTERM_STATE_DIR' LOOMTERM_RUNTIME_DIR='$LOOMTERM_RUNTIME_DIR' LOOMTERM_CONFIG='$LOOMTERM_CONFIG' && clear"
record_command="sleep 3; loom session record --agent $agent --name $agent-outcome-fix --$agent_command"

tmux send-keys -t "$left" "$setup" Enter
tmux send-keys -l -t "$left" "$record_command"
tmux send-keys -t "$left" Enter
tmux send-keys -t "$right" "$setup" Enter
tmux send-keys -t "$right" "sleep 3.5; loom watch --active" Enter

tmux select-layout -t "$session":demo even-horizontal >/dev/null
(
  rc=0
  control_agent || rc=$?
  tmux detach-client -s "$session" 2>/dev/null || true
  exit "$rc"
) &
controller_pid=$!
(
  sleep 15
  while tmux has-session -t "$session" 2>/dev/null; do
    tmux send-keys -t "$right" Down Down Down Down Down
    sleep 9
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
for ((attempt = 0; attempt < timeout_seconds; attempt++)); do
  if ! "$bin_dir/loom" session list --workspace live-demo | grep -q recording; then
    break
  fi
  sleep 1
done
if "$bin_dir/loom" session list --workspace live-demo | grep -q recording; then
  diagnose
  echo "recording session did not finish" >&2
  exit 1
fi

session_id=$("$bin_dir/loom" --json session list --workspace live-demo | python3 -c '
import json, sys
sessions = json.load(sys.stdin)
if sessions:
    print(sessions[0]["id"])
')
if [[ -z "$session_id" ]]; then
  echo "recorded session was not found" >&2
  exit 1
fi
detail_json="$root/session.json"
"$bin_dir/loom" --json session get "$session_id" > "$detail_json"
python3 - "$detail_json" "$prompt" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    detail = json.load(stream)
prompt = sys.argv[2]
assert detail["session"]["state"] == "finished", detail["session"]["state"]
assert any(turn["state"] == "completed" and turn["prompt"] == prompt for turn in detail["turns"])
assert detail["actions"], "no structured agent actions were captured"
assert detail["executions"], "no Loomterm executions were correlated"
PY

(cd "$fixture" && python3 -m unittest -q)
status=$(git -C "$fixture" status --short)
if [[ "$status" != " M session_report.py" ]]; then
  echo "unexpected fixture changes:" >&2
  printf '%s\n' "$status" >&2
  exit 1
fi

cp "$LOOMTERM_STATE_DIR/sessions/$session_id/replay.html" "$root/replay.html"
poster_png="$root/poster.png"
duration=$(ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 "$root/demo.mp4")
poster_at=$(python3 - "$duration" <<'PY'
import sys
duration = float(sys.argv[1])
print(max(1.0, min(duration - 1.0, duration * 0.55)))
PY
)
ffmpeg -y -ss "$poster_at" -i "$root/demo.mp4" -frames:v 1 -vf 'scale=1600:-2' "$poster_png" \
  >/dev/null 2>&1
cwebp -quiet -q 82 "$poster_png" -o "$root/poster.webp"
cp "$root/demo.mp4" "$repo/docs/demo.mp4"
cp "$root/poster.webp" "$repo/docs/poster.webp"
cp "$root/replay.html" "$repo/docs/replay.html"
echo "recorded docs/demo.mp4 and docs/poster.webp"
