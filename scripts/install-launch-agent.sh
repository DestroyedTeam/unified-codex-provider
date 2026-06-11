#!/bin/sh
set -eu

LABEL="com.codex.unified-provider-sync"
UCP_BIN="${UCP_BIN:-$(command -v ucp || true)}"

if [ -z "$UCP_BIN" ]; then
    echo "ucp was not found in PATH. Install it before enabling auto-sync." >&2
    exit 1
fi

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
TEMPLATE="$SCRIPT_DIR/../resources/$LABEL.plist.template"
DEST_DIR="$HOME/Library/LaunchAgents"
DEST="$DEST_DIR/$LABEL.plist"

escape_sed() {
    printf '%s' "$1" | sed 's/[&|\\]/\\&/g'
}

mkdir -p "$DEST_DIR" "$HOME/.codex"
BIN_ESCAPED=$(escape_sed "$UCP_BIN")
HOME_ESCAPED=$(escape_sed "$HOME")
sed \
    -e "s|__UCP_BIN__|$BIN_ESCAPED|g" \
    -e "s|__HOME__|$HOME_ESCAPED|g" \
    "$TEMPLATE" > "$DEST"

chmod 600 "$DEST"
DOMAIN="gui/$(id -u)"
launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
launchctl bootstrap "$DOMAIN" "$DEST"
launchctl enable "$DOMAIN/$LABEL"

echo "Installed and loaded: $DEST"
