#!/usr/bin/env bash
set -euo pipefail

for command in codex tmux vhs ffmpeg cwebp; do
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

cleanup() {
  tmux kill-session -t "$session" 2>/dev/null || true
  "$repo/target/release/loom" daemon stop --force >/dev/null 2>&1 || true
  rm -rf "$root"
}
trap cleanup EXIT

tmux kill-session -t "$session" 2>/dev/null || true
cargo build --manifest-path "$repo/Cargo.toml" --release --bins
"$repo/scripts/create-demo-fixture.sh" "$fixture" >/dev/null
mkdir -p "$bin_dir"
ln -s "$repo/target/release/loom" "$bin_dir/loom"
ln -s "$repo/target/release/loom-mcp" "$bin_dir/loom-mcp"
ln -s "$repo/target/release/loomd" "$bin_dir/loomd"
ln -s "$repo/target/release/loom-supervisor" "$bin_dir/loom-supervisor"
cp "$repo/demo/live-agent.sh" "$fixture/live-agent.sh"
"$bin_dir/loom" workspace add "$fixture" --name live-demo >/dev/null

tmux new-session -d -x 160 -y 48 -s "$session" -n demo
tmux set-option -t "$session" remain-on-exit on
left=$(tmux display-message -p -t "$session":demo '#{pane_id}')
right=$(tmux split-window -h -P -F '#{pane_id}' -t "$left")
setup="cd '$fixture' && export PATH='$bin_dir':\"\$PATH\" LOOMTERM_STATE_DIR='$LOOMTERM_STATE_DIR' LOOMTERM_RUNTIME_DIR='$LOOMTERM_RUNTIME_DIR' LOOMTERM_CONFIG='$LOOMTERM_CONFIG' LOOMTERM_DEMO_BIN_DIR='$bin_dir' && clear"

tmux send-keys -t "$left" "$setup" Enter
tmux send-keys -t "$left" \
  "sleep 3; loom session record --agent codex --name codex-outcome-fix -- ./live-agent.sh" Enter
tmux send-keys -t "$right" "$setup" Enter
tmux send-keys -t "$right" "sleep 3.5; loom watch --active" Enter

(
  sleep 1
  tmux select-layout -t "$session":demo even-horizontal >/dev/null
  sleep 17
  tmux send-keys -t "$right" Down Down Down Down Down Down Down Down Down Down
  sleep 24
  tmux send-keys -t "$right" Down Down Down Down Down Down Down Down Down Down
) &

cd "$repo"
vhs demo/live.tape
for _ in {1..60}; do
  if ! "$bin_dir/loom" session list --workspace live-demo | grep -q recording; then
    break
  fi
  sleep 1
done
cp "$LOOMTERM_STATE_DIR"/sessions/*/replay.html "$repo/docs/replay.html"
poster_png="$root/poster.png"
ffmpeg -y -ss 22 -i docs/demo.mp4 -frames:v 1 -vf 'scale=1600:-2' "$poster_png" \
  >/dev/null 2>&1
cwebp -quiet -q 82 "$poster_png" -o docs/poster.webp
echo "recorded docs/demo.mp4 and docs/poster.webp"
