#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "Usage: $0 <32-char-extension-id> [binary-path] [chrome|chromium|brave]" >&2
  exit 1
fi

EXT_ID="$1"
if [[ ! "$EXT_ID" =~ ^[a-p]{32}$ ]]; then
  echo "Error: Extension ID must be 32 lowercase characters in [a-p]: $EXT_ID" >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY_PATH="${2:-$SCRIPT_DIR/../target/release/ferrite}"
BROWSER="${3:-chrome}"

if [[ ! -x "$BINARY_PATH" ]]; then
  echo "Error: Binary not found or not executable: $BINARY_PATH" >&2
  exit 1
fi
ABS_BINARY="$(cd "$(dirname "$BINARY_PATH")" && pwd)/$(basename "$BINARY_PATH")"

OS="$(uname -s)"
case "$OS" in
  Linux)
    case "$BROWSER" in
      chrome)   TARGET_DIR="$HOME/.config/google-chrome/NativeMessagingHosts" ;;
      chromium) TARGET_DIR="$HOME/.config/chromium/NativeMessagingHosts" ;;
      brave)    TARGET_DIR="$HOME/.config/BraveSoftware/Brave-Browser/NativeMessagingHosts" ;;
      *) echo "Unsupported browser: $BROWSER" >&2; exit 1 ;;
    esac
    ;;
  Darwin)
    case "$BROWSER" in
      chrome)   TARGET_DIR="$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts" ;;
      chromium) TARGET_DIR="$HOME/Library/Application Support/Chromium/NativeMessagingHosts" ;;
      brave)    TARGET_DIR="$HOME/Library/Application Support/BraveSoftware/Brave-Browser/NativeMessagingHosts" ;;
      *) echo "Unsupported browser: $BROWSER" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $OS (use install-native-host.ps1 on Windows)" >&2
    exit 1
    ;;
esac

mkdir -p "$TARGET_DIR"
MANIFEST_FILE="$TARGET_DIR/com.ferrite.e2e.json"

cat > "$MANIFEST_FILE" <<EOF
{
  "name": "com.ferrite.e2e",
  "description": "Ferrite E2E Native Messaging Host (CDP + Tokio Automation Daemon)",
  "path": "$ABS_BINARY",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://$EXT_ID/"
  ]
}
EOF

echo "Installed Ferrite Native Messaging manifest at: $MANIFEST_FILE"
