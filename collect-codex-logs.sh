#!/usr/bin/env bash

# Copy every local Codex conversation into one timestamped folder.
# Reads from ~/.codex/sessions. Originals are never modified.

set -Eeuo pipefail
umask 077

sessions_dir="${CODEX_HOME:-$HOME/.codex}/sessions"

# Where to save. First argument, or the current directory.
out_root=${1:-$PWD}

if [[ ! -d $sessions_dir ]]; then
  printf 'error: no Codex conversations found at %s\n' "$sessions_dir" >&2
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  printf 'error: python3 is required\n' >&2
  exit 1
fi

mkdir -p -- "$out_root"
out_root=$(cd -- "$out_root" && pwd)
out_dir="$out_root/codex-logs-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p -- "$out_dir"

count=0
while IFS= read -r -d '' source_file; do
  relative=${source_file#"$sessions_dir"/}
  target="$out_dir/$relative"
  mkdir -p -- "${target%/*}"

  python3 - "$source_file" "$target" <<'PY'
import json
import sys
from pathlib import Path

source_path = Path(sys.argv[1])
target_path = Path(sys.argv[2])
lines = source_path.read_bytes().splitlines(keepends=True)

with target_path.open("wb") as output:
    for index, raw_line in enumerate(lines):
        # Strip NUL padding that shows up in some snapshotted log files.
        line = raw_line.lstrip(b"\x00")
        if not line.strip():
            continue
        payload = line.rstrip(b"\r\n")
        try:
            json.loads(payload)
        except (UnicodeDecodeError, json.JSONDecodeError):
            # A session running right now may have a half-written last line.
            is_unfinished_tail = (
                index == len(lines) - 1 and not raw_line.endswith((b"\n", b"\r"))
            )
            if is_unfinished_tail:
                continue
            raise RuntimeError(f"invalid JSONL record in {source_path} at line {index + 1}")
        output.write(payload + b"\n")
PY

  count=$((count + 1))
done < <(find "$sessions_dir" -type f -name '*.jsonl' -print0)

if [[ $count -eq 0 ]]; then
  rm -rf -- "$out_dir"
  printf 'error: no conversation files (*.jsonl) under %s\n' "$sessions_dir" >&2
  exit 1
fi

printf '\nCopied %s conversations (%s)\n' "$count" "$(du -sh -- "$out_dir" | cut -f1)"
printf 'Saved to: %s\n' "$out_dir"
