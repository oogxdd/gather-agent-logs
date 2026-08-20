#!/usr/bin/env bash

# One-time setup: virtualenv, Postgres database, schema, and (optionally) a
# LaunchAgent so the sync daemon starts at login.

set -Eeuo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
db=${CHATGPT_SYNC_DB:-chatgpt_logs}
dsn=${CHATGPT_SYNC_DSN:-postgresql:///$db}
label=com.oogxdd.chatgpt-sync
plist="$HOME/Library/LaunchAgents/$label.plist"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

command -v python3 >/dev/null || die 'python3 is required'
command -v psql >/dev/null || die 'psql is required (brew install postgresql@17)'

echo "==> virtualenv"
[[ -d "$here/.venv" ]] || python3 -m venv "$here/.venv"
"$here/.venv/bin/pip" install --quiet --upgrade pip
"$here/.venv/bin/pip" install --quiet 'psycopg[binary]'

echo "==> database $db"
if psql -lqtA -F'|' | cut -d'|' -f1 | grep -qx "$db"; then
  echo "    already exists"
else
  createdb "$db"
  echo "    created"
fi

echo "==> schema"
"$here/.venv/bin/python" "$here/chatgpt_sync.py" doctor --dsn "$dsn" || true

chmod +x "$here/start-chatgpt.sh" "$here/chatgpt_sync.py"

echo
read -r -p "Install a LaunchAgent to run the daemon at login? [y/N] " reply
if [[ $reply != [yY] ]]; then
  echo "Skipped. Run it manually with:"
  echo "  $here/.venv/bin/python $here/chatgpt_sync.py daemon"
  exit 0
fi

mkdir -p "$HOME/Library/LaunchAgents" "$here/logs"
cat > "$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$label</string>
    <key>ProgramArguments</key>
    <array>
        <string>$here/.venv/bin/python</string>
        <string>$here/chatgpt_sync.py</string>
        <string>daemon</string>
        <string>--dsn</string>
        <string>$dsn</string>
        <string>--interval</string>
        <string>120</string>
    </array>
    <key>WorkingDirectory</key>
    <string>$here</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>$here/logs/chatgpt-sync.log</string>
    <key>StandardErrorPath</key>
    <string>$here/logs/chatgpt-sync.log</string>
</dict>
</plist>
PLIST

launchctl bootout "gui/$UID/$label" 2>/dev/null || true
launchctl bootstrap "gui/$UID" "$plist"
echo "Installed $plist"
echo "Logs: $here/logs/chatgpt-sync.log"
echo "Stop with: launchctl bootout gui/$UID/$label"
