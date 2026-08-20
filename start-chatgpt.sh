#!/usr/bin/env bash

# Start the ChatGPT desktop app with its DevTools port open, which is what
# chatgpt_sync.py attaches to. Use this instead of the Dock icon.
#
# Chromium only applies the flag at process start, so if the app is already
# running without it this offers to restart it.

set -Eeuo pipefail

port=${CHATGPT_SYNC_PORT:-9222}
app=${CHATGPT_APP:-ChatGPT}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

port_open() {
  curl -sf --max-time 2 "http://127.0.0.1:$port/json/version" >/dev/null 2>&1
}

app_running() {
  pgrep -f "/Applications/$app.app/Contents/MacOS/$app" >/dev/null 2>&1
}

[[ -d "/Applications/$app.app" ]] || die "/Applications/$app.app not found"
command -v curl >/dev/null || die 'curl is required'

if port_open; then
  echo "Already listening on 127.0.0.1:$port — nothing to do."
  exit 0
fi

if app_running; then
  echo "$app is running without --remote-debugging-port."
  read -r -p "Quit and restart it with the port open? [y/N] " reply
  [[ $reply == [yY] ]] || die 'left running as-is'
  osascript -e "quit app \"$app\""
  for _ in $(seq 1 30); do
    app_running || break
    sleep 0.5
  done
  app_running && die "$app did not quit"
fi

open -a "$app" --args --remote-debugging-port="$port"

for _ in $(seq 1 40); do
  if port_open; then
    echo "$app is up, DevTools port on 127.0.0.1:$port"
    exit 0
  fi
  sleep 0.5
done

die "started $app but 127.0.0.1:$port never opened"
