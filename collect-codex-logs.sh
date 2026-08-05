#!/usr/bin/env bash

# Copy local Codex conversations into one timestamped folder.
# Reads from ~/.codex/sessions. Originals are never modified.

set -Eeuo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: ./collect-codex-logs.sh [FOLDER] [-n N]

  FOLDER     where to save (default: current directory)
  -n, --limit N   copy only the N most recent conversations
  -h, --help      show this message

Examples:
  ./collect-codex-logs.sh -n 10        # last 10, into the current directory
  ./collect-codex-logs.sh ~/Desktop    # everything, into ~/Desktop
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

limit=''
out_root=''

while [[ $# -gt 0 ]]; do
  case $1 in
    -n|--limit)
      [[ $# -ge 2 ]] || die '--limit needs a number'
      limit=$2
      shift 2
      ;;
    --limit=*|-n=*)
      limit=${1#*=}
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      die "unknown option: $1"
      ;;
    *)
      [[ -z $out_root ]] || die "too many arguments: $1"
      out_root=$1
      shift
      ;;
  esac
done

[[ -z $limit || $limit =~ ^[1-9][0-9]*$ ]] || die "--limit must be a positive number, got: $limit"

sessions_dir="${CODEX_HOME:-$HOME/.codex}/sessions"
out_root=${out_root:-$PWD}

[[ -d $sessions_dir ]] || die "no Codex conversations found at $sessions_dir"
command -v python3 >/dev/null 2>&1 || die 'python3 is required'

# Codex names its files rollout-<ISO timestamp>-<id>.jsonl inside year/month/day
# folders, so a plain path sort is also a chronological sort.
all_files=$(find "$sessions_dir" -type f -name '*.jsonl' | sort)
[[ -n $all_files ]] || die "no conversation files (*.jsonl) under $sessions_dir"

total=$(printf '%s\n' "$all_files" | wc -l | tr -d ' ')
files=$all_files
if [[ -n $limit ]]; then
  files=$(printf '%s\n' "$all_files" | tail -n "$limit")
fi

mkdir -p -- "$out_root"
out_root=$(cd -- "$out_root" && pwd)
out_dir="$out_root/codex-logs-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p -- "$out_dir"

count=0
while IFS= read -r source_file; do
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
done <<< "$files"

printf '\nCopied %s of %s conversations (%s)\n' "$count" "$total" "$(du -sh -- "$out_dir" | cut -f1)"
printf 'Saved to: %s\n' "$out_dir"
if [[ -n $limit && $count -lt $total ]]; then
  printf 'Run without -n to copy all %s.\n' "$total"
fi
