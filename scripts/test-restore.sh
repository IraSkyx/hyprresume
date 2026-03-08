#!/usr/bin/env bash
set -euo pipefail

SESSION="${1:-last}"

echo "=== Saving session '$SESSION' ==="
hyprresume -vv save "$SESSION"

echo ""
echo "=== Saved session content ==="
cat ~/.local/share/hyprresume/sessions/"$SESSION".toml

echo ""
echo "=== Closing all windows and restoring in 2s (detached) ==="

nohup bash -c "
    sleep 1
    hyprctl clients -j | jq -r '.[].address' | xargs -I{} hyprctl dispatch closewindow address:{}
    sleep 2
    hyprresume -vv restore \"$SESSION\"
" > /tmp/hyprresume-test.log 2>&1 &

echo "Detached. Output in /tmp/hyprresume-test.log"
