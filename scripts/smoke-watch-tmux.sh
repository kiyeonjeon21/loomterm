#!/usr/bin/env bash
set -euo pipefail

command -v tmux >/dev/null || {
  echo "tmux is required" >&2
  exit 2
}

repo=$(cd "$(dirname "$0")/.." && pwd)
binary="$repo/target/debug/loom"
[[ -x "$binary" ]] || cargo build --manifest-path "$repo/Cargo.toml" --bins

root=$(mktemp -d "${TMPDIR:-/tmp}/loom-watch-smoke.XXXXXX")
session="loom-watch-smoke-$$"
export LOOMTERM_STATE_DIR="$root/state"
export LOOMTERM_RUNTIME_DIR="$root/run"
export LOOMTERM_CONFIG="$root/config.toml"

cleanup() {
  tmux kill-session -t "$session" 2>/dev/null || true
  "$binary" daemon stop --force >/dev/null 2>&1 || true
  rm -rf "$root"
}
trap cleanup EXIT

cd "$repo"
"$binary" workspace add . --name watch-smoke >/dev/null
tmux new-session -d -x 160 -y 48 -s "$session" -n smoke
tmux set-option -t "$session" remain-on-exit on
left=$(tmux display-message -p -t "$session":smoke '#{pane_id}')
setup="cd '$repo' && export LOOMTERM_STATE_DIR='$LOOMTERM_STATE_DIR' LOOMTERM_RUNTIME_DIR='$LOOMTERM_RUNTIME_DIR' LOOMTERM_CONFIG='$LOOMTERM_CONFIG'"

tmux send-keys -t "$left" "$setup" Enter
tmux send-keys -t "$left" \
  "$binary session record --name watch-smoke -- sh -c '$binary run --shell \"printf one; sleep 2; printf two >&2; sleep 2; printf three\"; sleep 6'" Enter
sleep 1

right=$(tmux split-window -h -P -F '#{pane_id}' -t "$left")
tmux send-keys -t "$right" "$setup" Enter
tmux send-keys -t "$right" "$binary watch --active; printf '\nWATCH_RESTORED\n'; sleep 5" Enter
sleep 2

screen=$(tmux capture-pane -p -t "$right")
grep -q "Loomterm Live Observer" <<<"$screen"
grep -q "running" <<<"$screen"
grep -q "one" <<<"$screen"

tmux send-keys -t "$right" q
sleep 1
restored=$(tmux capture-pane -p -S - -t "$right")
grep -q "WATCH_RESTORED" <<<"$restored"
echo "watch tmux smoke: ok"
