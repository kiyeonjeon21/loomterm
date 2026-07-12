#!/usr/bin/env bash
set -euo pipefail

command -v tmux >/dev/null || {
  echo "tmux is required" >&2
  exit 2
}

repo=$(cd "$(dirname "$0")/.." && pwd)
binary="$repo/target/debug/loom"
[[ -x "$binary" ]] || cargo build --manifest-path "$repo/Cargo.toml" --bins

root=$(mktemp -d "${TMPDIR:-/tmp}/loom-ui-smoke.XXXXXX")
project="$root/project"
bin_dir="$root/bin"
session="loom-ui-smoke-$$"
export LOOMTERM_STATE_DIR="$root/state"
export LOOMTERM_RUNTIME_DIR="$root/run"
export LOOMTERM_CONFIG="$root/config.toml"

cleanup() {
  tmux kill-session -t "$session" 2>/dev/null || true
  "$binary" daemon stop --force >/dev/null 2>&1 || true
  rm -rf "$root"
}
trap cleanup EXIT

mkdir -p "$project" "$bin_dir"
git -C "$project" init -q
cat >"$bin_dir/codex" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'fake Codex session started\n'
"$LOOM_SMOKE_BINARY" run --detach -- sleep 29 >/dev/null
sleep 0.2
EOF
chmod +x "$bin_dir/codex"
cat >"$bin_dir/claude" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'fake Claude handoff started\n'
sleep 0.2
EOF
chmod +x "$bin_dir/claude"

"$binary" init "$project" --name ui-smoke --agent both >/dev/null
cd "$project"
"$binary" run -- printf smoke-pass >/dev/null
"$binary" run --shell 'printf smoke-fail >&2; exit 3' >/dev/null 2>&1 || true
"$binary" run --detach -- sleep 30 >/dev/null

tmux new-session -d -x 160 -y 40 -s "$session" -n smoke
tmux set-option -t "$session" remain-on-exit on
pane=$(tmux display-message -p -t "$session":smoke '#{pane_id}')
setup="cd '$project' && export PATH='$bin_dir':\"\$PATH\" LOOM_SMOKE_BINARY='$binary' LOOMTERM_STATE_DIR='$LOOMTERM_STATE_DIR' LOOMTERM_RUNTIME_DIR='$LOOMTERM_RUNTIME_DIR' LOOMTERM_CONFIG='$LOOMTERM_CONFIG'"
tmux send-keys -t "$pane" "$setup" Enter
tmux send-keys -t "$pane" "$binary; printf '\nUI_RESTORED\n'; sleep 5" Enter
sleep 1

screen=$(tmux capture-pane -p -t "$pane")
grep -q "Operator UI | daemon connected" <<<"$screen"
grep -q "ALL WORK" <<<"$screen"
grep -q "smoke-pass" <<<"$screen"
grep -q "sleep 30" <<<"$screen"

tmux send-keys -t "$pane" C-p
sleep 0.2
grep -q "Command palette" <<<"$(tmux capture-pane -p -t "$pane")"
tmux send-keys -t "$pane" Escape
sleep 0.1
tmux send-keys -t "$pane" '/'
tmux send-keys -l -t "$pane" smoke-pass
tmux send-keys -t "$pane" Enter
sleep 0.2
grep -q "filter: smoke-pass" <<<"$(tmux capture-pane -p -t "$pane")"

tmux send-keys -t "$pane" Escape
tmux resize-window -t "$session" -x 80 -y 24
sleep 0.2
grep -q "Sessions" <<<"$(tmux capture-pane -p -t "$pane")"
tmux send-keys -t "$pane" Tab
sleep 0.2
grep -q "Executions | all work" <<<"$(tmux capture-pane -p -t "$pane")"

tmux send-keys -t "$pane" n
sleep 0.1
grep -q "New agent" <<<"$(tmux capture-pane -p -t "$pane")"
tmux send-keys -t "$pane" Enter
sleep 1
agent_screen=$(tmux capture-pane -p -t "$pane")
grep -q "CODEX" <<<"$agent_screen"
grep -q "DONE" <<<"$agent_screen"

sleep 0.5
tmux send-keys -t "$pane" h
sleep 0.2
handoff_screen=$(tmux capture-pane -p -t "$pane")
grep -q "Handoff" <<<"$handoff_screen"
grep -q "Active    1 execution" <<<"$handoff_screen"
tmux send-keys -t "$pane" Enter
sleep 1.5
target_screen=$(tmux capture-pane -p -t "$pane")
grep -q "CLAUDE DONE" <<<"$target_screen"

sleep 0.5
tmux send-keys -t "$pane" q
sleep 1
restored=$(tmux capture-pane -p -S - -t "$pane")
if ! grep -q "UI_RESTORED" <<<"$restored"; then
  printf '%s\n' "$restored" >&2
  exit 1
fi
echo "operator UI tmux smoke: ok"
