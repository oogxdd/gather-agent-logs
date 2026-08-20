#!/usr/bin/env python3
"""Import local Codex and ChatGPT Work sessions into the unified history store.

Both live in the same place. The desktop app runs ChatGPT Work on the local
Codex runtime, so a Work thread is written to ~/.codex/sessions as an ordinary
rollout; the only thing that separates them is `originator` on the first line:

    codex_work_desktop  -> ChatGPT Work
    Codex Desktop       -> Codex, desktop app
    codex_cli_rs        -> Codex CLI
    codex-tui           -> Codex TUI

Rollout files are read, never written. A file whose contents have not changed
since the last import is skipped; one that cannot be parsed is recorded with
import_status='failed' so it shows up in the inventory instead of vanishing.

    ./codex_import.py import
    ./codex_import.py import --machine work-laptop --full
    ./codex_import.py status
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import socket
import sqlite3
import sys
from datetime import datetime, timezone
from pathlib import Path

from histstore import DEFAULT_DSN, Store, to_timestamp

CODEX_HOME = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex"))

# rollout-<ISO timestamp>-<uuid>.jsonl — the uuid identifies the file, which is
# the unit of import. It is not always the session id recorded inside.
ROLLOUT_ID = re.compile(
    r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})", re.IGNORECASE
)

# `originator` on the session_meta line decides which platform a rollout is.
WORK_ORIGINATORS = {"codex_work_desktop"}

# App-injected prompts, identical across hundreds of sessions. Kept verbatim but
# left out of the search index so they cannot drown real hits. The runtime sends
# several of these with role 'user', so the role alone is not enough to tell them
# from something the person actually typed.
BOILERPLATE_ROLES = {"system", "developer"}
INJECTED_PREFIXES = (
    "<permissions instructions>",
    "<collaboration_mode>",
    "<app-context>",
    "<environment_context>",
    "<turn_aborted>",
    "<recommended_plugins>",
    "<user_instructions>",
    "# AGENTS.md instructions for",
)


def is_searchable(role: str | None, text: str | None) -> bool:
    if role in BOILERPLATE_ROLES:
        return False
    return not (text or "").lstrip().startswith(INJECTED_PREFIXES)


def _log(message: str) -> None:
    print(f"{datetime.now(timezone.utc):%Y-%m-%dT%H:%M:%SZ} {message}", flush=True)


def parts_text(content) -> str | None:
    """Flatten the several shapes content takes across record types."""
    if isinstance(content, str):
        return content or None
    if isinstance(content, list):
        chunks = []
        for part in content:
            if isinstance(part, str):
                chunks.append(part)
            elif isinstance(part, dict):
                text = part.get("text")
                if isinstance(text, str):
                    chunks.append(text)
        return "\n".join(chunks) or None
    return None


def local_path(value: str | None) -> str | None:
    if not value:
        return None
    return value[len("file://") :] if value.startswith("file://") else value


def load_titles(codex_home: Path = CODEX_HOME) -> dict[str, str]:
    """Titles live in the app's own index, not in the rollout files."""
    database = codex_home / "state_5.sqlite"
    if not database.exists():
        return {}
    try:
        connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True)
        rows = connection.execute(
            "SELECT id, nullif(trim(title), '') FROM threads"
        ).fetchall()
        connection.close()
    except sqlite3.Error:
        return {}
    # Titles fall back to the whole first user message, newlines and all.
    return {
        thread_id: " ".join(title.split())[:160] for thread_id, title in rows if title
    }


def rollout_files(codex_home: Path) -> list[Path]:
    """Oldest first. Sorted by mtime, not by path: archived sessions live in a
    separate directory, so a path sort would not be chronological across both."""
    files = list((codex_home / "sessions").rglob("*.jsonl"))
    files += list((codex_home / "archived_sessions").glob("*.jsonl"))
    return sorted(files, key=lambda p: p.stat().st_mtime)


def parse_rollout(path: Path) -> dict:
    """Read one rollout into conversation fields, messages, and artifacts."""
    meta: dict = {}
    messages: list[dict] = []
    artifacts: list[dict] = []
    first_user_text: str | None = None
    model: str | None = None
    seq = 0

    with path.open("rb") as handle:
        for raw_line in handle:
            # Snapshotted files are sometimes NUL-padded.
            line = raw_line.lstrip(b"\x00").strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except (UnicodeDecodeError, json.JSONDecodeError):
                # A session being written right now has a half-flushed last line.
                continue

            kind = record.get("type")
            payload = record.get("payload") or {}
            stamp = to_timestamp(record.get("timestamp"))

            if kind == "session_meta":
                meta = payload
                continue

            if kind == "turn_context":
                model = payload.get("model") or model
                continue

            if kind == "event_msg":
                item = payload.get("item") or {}
                if item.get("type") == "CommandExecution":
                    command = item.get("command") or []
                    artifacts.append(
                        {
                            "kind": "command",
                            "name": " ".join(command)[:400] if command else None,
                            "path": local_path(item.get("cwd")),
                            "detail": item,
                        }
                    )
                elif payload.get("type") == "patch_apply_end":
                    for entry in (payload.get("stdout") or "").splitlines():
                        parts = entry.split(None, 1)
                        if len(parts) == 2 and parts[0] in {"A", "M", "D"}:
                            artifacts.append(
                                {"kind": "patch", "name": parts[0], "path": parts[1]}
                            )
                elif payload.get("type") == "web_search_end":
                    artifacts.append(
                        {"kind": "search", "name": (payload.get("query") or "")[:400]}
                    )
                continue

            if kind != "response_item":
                continue

            # response_item is the model-facing transcript and the canonical
            # order; event_msg repeats much of it for the UI.
            item_type = payload.get("type")
            role = text = name = None

            if item_type == "message":
                role = payload.get("role")
                text = parts_text(payload.get("content"))
                # The first user record is usually an injected AGENTS.md or
                # environment block, which would make a useless title.
                if role == "user" and first_user_text is None and is_searchable(role, text):
                    first_user_text = text
            elif item_type == "reasoning":
                role = "reasoning"
                text = parts_text(payload.get("summary"))
            elif item_type in ("custom_tool_call", "function_call"):
                role = "tool_call"
                name = payload.get("name")
                text = payload.get("input") or payload.get("arguments")
            elif item_type in ("custom_tool_call_output", "function_call_output"):
                role = "tool_output"
                text = parts_text(payload.get("output"))
            else:
                continue

            seq += 1
            messages.append(
                {
                    "seq": seq,
                    "external_id": payload.get("id") or payload.get("call_id"),
                    "parent_id": None,
                    "role": role,
                    "author": name,
                    "content_type": item_type,
                    "text": text,
                    "created_at": stamp,
                    "searchable": is_searchable(role, text),
                    "raw": record,
                }
            )

    if not meta:
        raise ValueError("no session_meta record")

    git = meta.get("git") or {}
    repo_url = git.get("repository_url")
    cwd = local_path(meta.get("cwd"))
    project = None
    if repo_url:
        project = repo_url.rstrip("/").rsplit("/", 1)[-1].removesuffix(".git")
    elif cwd:
        project = Path(cwd).name

    originator = meta.get("originator")
    platform = "chatgpt_work" if originator in WORK_ORIGINATORS else "codex"

    match = ROLLOUT_ID.search(path.stem)

    return {
        "external_id": match.group(1) if match else path.stem,
        "thread_id": meta.get("session_id") or meta.get("id"),
        "platform": platform,
        "originator": originator,
        "created_at": to_timestamp(meta.get("timestamp")),
        "cwd": cwd,
        "project": project,
        "repo_url": repo_url,
        "branch": git.get("branch"),
        "git_commit": git.get("commit_hash"),
        "model": model,
        "first_user_text": first_user_text,
        # base_instructions is a large constant prompt; keeping it out of `raw`
        # avoids storing the same blob once per session.
        "raw": {k: v for k, v in meta.items() if k != "base_instructions"},
        "messages": messages,
        "artifacts": artifacts,
    }


def file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def import_all(store: Store, codex_home: Path, machine: str, full: bool, limit: int | None) -> dict:
    files = rollout_files(codex_home)
    if limit:
        files = files[-limit:]

    titles = load_titles(codex_home)
    counts = {"seen": 0, "imported": 0, "skipped": 0, "failed": 0, "messages": 0}
    known: dict[str, dict[str, str]] = {}
    runs: dict[str, int] = {}

    for path in files:
        counts["seen"] += 1
        digest = file_digest(path)

        try:
            parsed = parse_rollout(path)
            platform = parsed["platform"]
            source_id = f"{platform}:{machine}"
            if source_id not in runs:
                label = ("ChatGPT Work" if platform == "chatgpt_work" else "Codex")
                store.upsert_source(
                    source_id, platform, machine=machine, label=f"{label} on {machine}"
                )
                known[source_id] = store.content_hashes(source_id)
                runs[source_id] = store.start_run(source_id)

            if not full and known[source_id].get(parsed["external_id"]) == digest:
                counts["skipped"] += 1
                continue

            title = titles.get(parsed["external_id"])
            if not title and parsed["first_user_text"]:
                title = " ".join(parsed["first_user_text"].split())[:120]

            last_message = max(
                (m["created_at"] for m in parsed["messages"] if m["created_at"]),
                default=parsed["created_at"],
            )

            conversation_id = store.upsert_conversation(
                {
                    "source_id": source_id,
                    "platform": platform,
                    "external_id": parsed["external_id"],
                    "thread_id": parsed["thread_id"],
                    "title": title,
                    "created_at": parsed["created_at"],
                    "updated_at": last_message or parsed["created_at"],
                    "machine": machine,
                    "project": parsed["project"],
                    "repo_url": parsed["repo_url"],
                    "branch": parsed["branch"],
                    "git_commit": parsed["git_commit"],
                    "cwd": parsed["cwd"],
                    "originator": parsed["originator"],
                    "model": parsed["model"],
                    "source_ref": str(path),
                    "import_status": "ok",
                    "import_error": None,
                    "imported_at": datetime.now(timezone.utc),
                    "content_hash": digest,
                    "message_count": len(parsed["messages"]),
                    "raw": parsed["raw"],
                }
            )
            store.replace_messages(conversation_id, parsed["messages"])
            store.replace_artifacts(conversation_id, parsed["artifacts"])
            store.conn.commit()

            counts["imported"] += 1
            counts["messages"] += len(parsed["messages"])
            if counts["imported"] % 50 == 0:
                _log(f"  {counts['imported']} imported, {counts['skipped']} unchanged")
        except Exception as exc:  # noqa: BLE001 - one bad file must not end the run
            store.conn.rollback()
            source_id = f"codex:{machine}"
            store.upsert_source(source_id, "codex", machine=machine, label=f"Codex on {machine}")
            store.mark_failed(
                source_id,
                path.stem,
                f"{type(exc).__name__}: {exc}",
                platform="codex",
                machine=machine,
                source_ref=str(path),
                content_hash=digest,
            )
            counts["failed"] += 1
            _log(f"  FAILED {path.name}: {type(exc).__name__}: {exc}")

    for source_id, run_id in runs.items():
        store.finish_run(run_id, "ok", counts)
        store.set_source_status(source_id, "ok", f"{counts['imported']} imported this run")
    return counts


def cmd_import(args) -> int:
    codex_home = Path(args.codex_home).expanduser()
    if not (codex_home / "sessions").exists():
        print(f"no Codex sessions at {codex_home / 'sessions'}", file=sys.stderr)
        with Store(args.dsn) as store:
            store.apply_schema()
            source_id = f"codex:{args.machine}"
            store.upsert_source(source_id, "codex", machine=args.machine)
            store.set_source_status(source_id, "offline", f"{codex_home} not present")
        return 1

    with Store(args.dsn) as store:
        store.apply_schema()
        lock_id = f"codex:{args.machine}"
        if not store.try_lock(lock_id):
            _log("another Codex import is running")
            return 1
        try:
            counts = import_all(store, codex_home, args.machine, args.full, args.limit)
        finally:
            store.unlock(lock_id)

    _log(
        "done: {seen} files, {imported} imported, {skipped} unchanged, "
        "{failed} failed, {messages} messages".format(**counts)
    )
    return 0


def cmd_status(args) -> int:
    with Store(args.dsn) as store:
        store.apply_schema()
        with store.conn.cursor() as cur:
            cur.execute(
                "SELECT id, platform, machine, status, conversations, imported, failed, "
                "       messages, newest FROM hist.inventory ORDER BY platform, id"
            )
            rows = cur.fetchall()
    if not rows:
        print("no sources yet")
        return 0
    print(f"{'source':<28} {'status':<9} {'convs':>6} {'ok':>6} {'fail':>5} {'msgs':>8}  newest")
    for source_id, _platform, _machine, status, total, ok, failed, messages, newest in rows:
        stamp = f"{newest:%Y-%m-%d %H:%M}" if newest else "-"
        print(f"{source_id:<28} {status:<9} {total:>6} {ok:>6} {failed:>5} {messages:>8}  {stamp}")
    return 0


def main(argv: list[str] | None = None) -> int:
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--dsn", default=DEFAULT_DSN, help=f"Postgres DSN (default: {DEFAULT_DSN})")
    common.add_argument(
        "--machine",
        default=os.environ.get("CHATGPT_SYNC_MACHINE", socket.gethostname()),
        help="name recorded for this computer (default: hostname)",
    )
    common.add_argument("--codex-home", default=str(CODEX_HOME), help="Codex home directory")
    common.add_argument("--limit", type=int, default=0, help="only the N most recent rollouts")
    common.add_argument("--full", action="store_true", help="reimport even unchanged files")

    parser = argparse.ArgumentParser(
        description="Import local Codex and ChatGPT Work sessions into Postgres."
    )
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("import", parents=[common], help="scan rollout files and import them")
    sub.add_parser("status", parents=[common], help="show the source inventory")

    args = parser.parse_args(argv)
    args.limit = args.limit or None
    return {"import": cmd_import, "status": cmd_status}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
