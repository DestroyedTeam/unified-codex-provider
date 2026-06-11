#!/bin/sh
set -eu

LABEL="com.codex.unified-provider-sync"
DEST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"

launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
rm -f "$DEST"

echo "Removed: $DEST"
