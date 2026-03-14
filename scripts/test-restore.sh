#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SESSION="${1:-test-restore}"

echo "=== Building latest code ==="
cargo build --release --manifest-path "$PROJECT_DIR/Cargo.toml"
BIN="$PROJECT_DIR/target/release/hyprresume"

echo ""
echo "=== Saving session '$SESSION' ==="
"$BIN" -vv save "$SESSION"

echo ""
echo "=== Saved session content ==="
cat ~/.local/share/hyprresume/sessions/"$SESSION".toml

echo ""
echo "=== Closing all windows and restoring in 3s (detached) ==="

nohup bash -c "
    sleep 1
    hyprctl clients -j | jq -r '.[].address' | xargs -I{} hyprctl dispatch closewindow address:{}
    sleep 3
    '$BIN' -vv restore '$SESSION'
" > /tmp/hyprresume-test.log 2>&1 &

echo "Detached. Output in /tmp/hyprresume-test.log"
