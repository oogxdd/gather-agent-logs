#!/usr/bin/env bash

# Collect local conversation histories from Claude Code, Codex, and Crush.
# The result contains exactly one JSONL file per conversation. Credentials,
# config, prompt history, and the Crush SQLite database are not included.

set -Eeuo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage: collect-agent-conversations.sh DESTINATION_ROOT [--home HOME_DIR]

Creates a timestamped snapshot inside DESTINATION_ROOT:
  agent-conversations-YYYYMMDDTHHMMSSZ/

By default, histories are read from the current user's HOME.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

note() {
  printf '%s\n' "$*" >&2
}

copy_jsonl_conversation() {
  local source_file=$1
  local target_file=$2

  python3 - "$source_file" "$target_file" <<'PY'
import json
import sys
from pathlib import Path

source_path = Path(sys.argv[1])
target_path = Path(sys.argv[2])
data = source_path.read_bytes()
lines = data.splitlines(keepends=True)

with target_path.open("wb") as output:
    for index, raw_line in enumerate(lines):
        # Some Sprite snapshots contain sparse NUL padding before an otherwise
        # intact JSON record. It is not conversation data, so remove it.
        line = raw_line.lstrip(b"\x00")
        if not line.strip():
            continue
        payload = line.rstrip(b"\r\n")
        try:
            json.loads(payload)
        except (UnicodeDecodeError, json.JSONDecodeError):
            # An actively-written final record may have been read mid-append.
            is_unfinished_tail = index == len(lines) - 1 and not raw_line.endswith((b"\n", b"\r"))
            if is_unfinished_tail:
                continue
            raise RuntimeError(f"invalid JSONL record in {source_path} at line {index + 1}")
        output.write(payload + b"\n")
PY
}

[[ $# -ge 1 ]] || { usage >&2; exit 2; }
[[ ${1:-} != -h && ${1:-} != --help ]] || { usage; exit 0; }

destination_root=$1
shift
source_home=${HOME:?HOME is not set}

while [[ $# -gt 0 ]]; do
  case $1 in
    --home)
      [[ $# -ge 2 ]] || die '--home requires a directory'
      source_home=$2
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[[ -d $source_home ]] || die "home directory does not exist: $source_home"
command -v python3 >/dev/null 2>&1 || die 'python3 is required'

source_home=$(realpath -e -- "$source_home")
mkdir -p -- "$destination_root"
destination_root=$(realpath -e -- "$destination_root")

case "$destination_root/" in
  "$source_home/.claude/"*|"$source_home/.codex/"*|"$source_home/.crush/"*)
    die 'destination must not be inside an agent data directory'
    ;;
esac

timestamp=$(date -u +%Y%m%dT%H%M%SZ)
snapshot_name="agent-conversations-$timestamp"
snapshot="$destination_root/$snapshot_name"
if [[ -e $snapshot ]]; then
  snapshot="$destination_root/$snapshot_name-$$"
fi

staging="$destination_root/.${snapshot##*/}.tmp-$$"
[[ ! -e $staging ]] || die "temporary path already exists: $staging"
mkdir -p -- "$staging"
cleanup() {
  rm -rf -- "$staging"
}
trap cleanup EXIT

found=0

claude_root="$source_home/.claude"
if [[ -d $claude_root/projects ]]; then
  note 'Collecting Claude Code conversations...'
  while IFS= read -r -d '' source_file; do
    relative=${source_file#"$claude_root/projects/"}
    target="$staging/claude/$relative"
    mkdir -p -- "${target%/*}"
    copy_jsonl_conversation "$source_file" "$target"
    found=1
  done < <(find "$claude_root/projects" -type f -name '*.jsonl' -print0)
else
  note 'Claude Code: no conversation directory found.'
fi

codex_root="$source_home/.codex"
if [[ -d $codex_root/sessions ]]; then
  note 'Collecting Codex conversations...'
  while IFS= read -r -d '' source_file; do
    relative=${source_file#"$codex_root/sessions/"}
    target="$staging/codex/$relative"
    mkdir -p -- "${target%/*}"
    copy_jsonl_conversation "$source_file" "$target"
    found=1
  done < <(find "$codex_root/sessions" -type f -name '*.jsonl' -print0)
else
  note 'Codex: no conversation directory found.'
fi

crush_db=''
crush_candidates=(
  "$source_home/.crush/crush.db"
  "${XDG_DATA_HOME:-$source_home/.local/share}/crush/crush.db"
  "$source_home/.local/share/crush/crush.db"
)
for candidate in "${crush_candidates[@]}"; do
  if [[ -f $candidate ]]; then
    crush_db=$candidate
    break
  fi
done

if [[ -n $crush_db ]]; then
  note 'Collecting Crush conversations...'
  mkdir -p -- "$staging/crush"
  python3 - "$crush_db" "$staging/crush" <<'PY'
import hashlib
import json
import re
import sqlite3
import sys
from pathlib import Path

source_path = Path(sys.argv[1])
output_dir = Path(sys.argv[2])
db = sqlite3.connect(f"file:{source_path}?mode=ro", uri=True)
db.row_factory = sqlite3.Row
try:
    db.execute("PRAGMA query_only = ON")
    db.execute("BEGIN")
    tables = {
        row[0]
        for row in db.execute("SELECT name FROM sqlite_master WHERE type = 'table'")
    }
    if not {"sessions", "messages"}.issubset(tables):
        raise RuntimeError("Crush database has no sessions/messages tables")

    used_names = set()
    sessions = db.execute("SELECT * FROM sessions ORDER BY created_at, id").fetchall()
    for session_row in sessions:
        session = dict(session_row)
        session_id = str(session["id"])
        safe_id = re.sub(r"[^A-Za-z0-9._-]", "_", session_id).strip(".") or "session"
        if safe_id in used_names:
            digest = hashlib.sha256(session_id.encode()).hexdigest()[:12]
            safe_id = f"{safe_id}-{digest}"
        used_names.add(safe_id)

        output_path = output_dir / f"{safe_id}.jsonl"
        with output_path.open("w", encoding="utf-8") as output:
            output.write(json.dumps(
                {"type": "session", "payload": session},
                ensure_ascii=False,
                separators=(",", ":"),
            ) + "\n")

            query = "SELECT * FROM messages WHERE session_id = ? ORDER BY created_at, id"
            for message_row in db.execute(query, (session_id,)):
                message = dict(message_row)
                parts = message.get("parts")
                if isinstance(parts, str):
                    try:
                        message["parts"] = json.loads(parts)
                    except json.JSONDecodeError:
                        pass
                output.write(json.dumps(
                    {"type": "message", "payload": message},
                    ensure_ascii=False,
                    separators=(",", ":"),
                ) + "\n")
finally:
    db.close()
PY
  if find "$staging/crush" -type f -name '*.jsonl' -print -quit | grep -q .; then
    found=1
  fi
else
  note 'Crush: no conversation database found.'
fi

[[ $found -eq 1 ]] || die "no supported agent conversations found under $source_home"

{
  printf 'created_at_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'source_home=%s\n' "$source_home"
  printf 'format_version=2\n'
  printf 'layout=one_jsonl_file_per_conversation\n'
  for agent in claude codex crush; do
    if [[ -d $staging/$agent ]]; then
      conversations=$(find "$staging/$agent" -type f -name '*.jsonl' | wc -l)
      bytes=$(du -sb "$staging/$agent" | cut -f1)
      printf '%s_conversations=%s\n' "$agent" "$conversations"
      printf '%s_bytes=%s\n' "$agent" "$bytes"
    fi
  done
} > "$staging/manifest.txt"

(
  cd "$staging"
  find . -type f ! -name checksums.sha256 -print0 \
    | sort -z \
    | xargs -0 sha256sum -- \
    > checksums.sha256
)

mv -- "$staging" "$snapshot"
trap - EXIT

printf 'Snapshot created: %s\n' "$snapshot"
