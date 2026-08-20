"""Shared Postgres access for the unified history store.

Both importers (ChatGPT over the desktop app, Codex from local rollout files)
write through this module so the normalised shape stays in one place.
"""

from __future__ import annotations

import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Sequence

import psycopg
from psycopg.types.json import Json

DEFAULT_DSN = os.environ.get("CHATGPT_SYNC_DSN", "postgresql:///chatgpt_logs")
SCHEMA_PATH = Path(__file__).resolve().parent / "schema.sql"

# One advisory lock per source, so the ChatGPT daemon and a Codex import can run
# at the same time while two runs against the same source cannot.
_LOCK_NAMESPACE = 0x6368_6774  # 'chgt'


def now() -> datetime:
    return datetime.now(timezone.utc)


def scrub(value: Any) -> Any:
    """Strip NUL bytes, which Postgres rejects in both text and jsonb.

    Command output captured by Codex occasionally contains them. This is the one
    thing stored content is not byte-identical about; the files on disk are of
    course untouched, and `source_ref` still points at the original.
    """
    if isinstance(value, str):
        return value.replace("\x00", "") if "\x00" in value else value
    if isinstance(value, list):
        return [scrub(item) for item in value]
    if isinstance(value, dict):
        return {scrub(key): scrub(item) for key, item in value.items()}
    return value


def to_timestamp(value: Any) -> datetime | None:
    """Sources mix epoch floats and ISO strings; normalise both."""
    if value is None or value == "":
        return None
    if isinstance(value, (int, float)):
        try:
            return datetime.fromtimestamp(value, tz=timezone.utc)
        except (OverflowError, OSError, ValueError):
            return None
    if isinstance(value, datetime):
        return value if value.tzinfo else value.replace(tzinfo=timezone.utc)
    if isinstance(value, str):
        try:
            parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        except ValueError:
            return None
        return parsed if parsed.tzinfo else parsed.replace(tzinfo=timezone.utc)
    return None


CONVERSATION_UPSERT = """
INSERT INTO hist.conversations
    (source_id, platform, external_id, thread_id, title, created_at, updated_at, machine,
     project, repo_url, branch, git_commit, cwd, originator, workspace_id,
     model, source_ref, import_status, import_error, imported_at, content_hash,
     message_count, raw)
VALUES (%(source_id)s, %(platform)s, %(external_id)s, %(thread_id)s, %(title)s, %(created_at)s,
        %(updated_at)s, %(machine)s, %(project)s, %(repo_url)s, %(branch)s,
        %(git_commit)s, %(cwd)s, %(originator)s, %(workspace_id)s, %(model)s,
        %(source_ref)s, %(import_status)s, %(import_error)s, %(imported_at)s,
        %(content_hash)s, %(message_count)s, %(raw)s)
ON CONFLICT (source_id, external_id) DO UPDATE SET
    thread_id     = coalesce(EXCLUDED.thread_id, hist.conversations.thread_id),
    title         = coalesce(EXCLUDED.title, hist.conversations.title),
    created_at    = coalesce(EXCLUDED.created_at, hist.conversations.created_at),
    updated_at    = coalesce(EXCLUDED.updated_at, hist.conversations.updated_at),
    machine       = coalesce(EXCLUDED.machine, hist.conversations.machine),
    project       = coalesce(EXCLUDED.project, hist.conversations.project),
    repo_url      = coalesce(EXCLUDED.repo_url, hist.conversations.repo_url),
    branch        = coalesce(EXCLUDED.branch, hist.conversations.branch),
    git_commit    = coalesce(EXCLUDED.git_commit, hist.conversations.git_commit),
    cwd           = coalesce(EXCLUDED.cwd, hist.conversations.cwd),
    originator    = coalesce(EXCLUDED.originator, hist.conversations.originator),
    workspace_id  = coalesce(EXCLUDED.workspace_id, hist.conversations.workspace_id),
    model         = coalesce(EXCLUDED.model, hist.conversations.model),
    source_ref    = coalesce(EXCLUDED.source_ref, hist.conversations.source_ref),
    import_status = EXCLUDED.import_status,
    import_error  = EXCLUDED.import_error,
    imported_at   = coalesce(EXCLUDED.imported_at, hist.conversations.imported_at),
    content_hash  = coalesce(EXCLUDED.content_hash, hist.conversations.content_hash),
    message_count = greatest(EXCLUDED.message_count, hist.conversations.message_count),
    raw           = coalesce(EXCLUDED.raw, hist.conversations.raw)
RETURNING id
"""

MESSAGE_INSERT = """
INSERT INTO hist.messages
    (conversation_id, seq, external_id, parent_id, role, author, content_type,
     text, created_at, searchable, raw)
VALUES (%(conversation_id)s, %(seq)s, %(external_id)s, %(parent_id)s, %(role)s,
        %(author)s, %(content_type)s, %(text)s, %(created_at)s, %(searchable)s,
        %(raw)s)
"""

ARTIFACT_INSERT = """
INSERT INTO hist.artifacts (conversation_id, kind, name, path, detail)
VALUES (%(conversation_id)s, %(kind)s, %(name)s, %(path)s, %(detail)s)
"""


class Store:
    def __init__(self, dsn: str = DEFAULT_DSN):
        self.conn = psycopg.connect(dsn, autocommit=False)

    def close(self) -> None:
        self.conn.close()

    def __enter__(self) -> "Store":
        return self

    def __exit__(self, *exc_info) -> None:
        self.close()

    # -- schema and locking -------------------------------------------------

    def apply_schema(self) -> None:
        with self.conn.cursor() as cur:
            cur.execute(SCHEMA_PATH.read_text())
        self.conn.commit()

    def try_lock(self, source_id: str) -> bool:
        with self.conn.cursor() as cur:
            cur.execute(
                "SELECT pg_try_advisory_lock(%s, hashtext(%s))",
                (_LOCK_NAMESPACE, source_id),
            )
            (acquired,) = cur.fetchone()
        self.conn.commit()
        return bool(acquired)

    def unlock(self, source_id: str) -> None:
        with self.conn.cursor() as cur:
            cur.execute(
                "SELECT pg_advisory_unlock(%s, hashtext(%s))",
                (_LOCK_NAMESPACE, source_id),
            )
        self.conn.commit()

    # -- watermarks ---------------------------------------------------------

    def get_state(self, key: str):
        with self.conn.cursor() as cur:
            cur.execute("SELECT value FROM hist.sync_state WHERE key = %s", (key,))
            row = cur.fetchone()
        return row[0] if row else None

    def set_state(self, key: str, value: Any) -> None:
        with self.conn.cursor() as cur:
            cur.execute(
                "INSERT INTO hist.sync_state (key, value, updated_at) VALUES (%s, %s, now()) "
                "ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
                (key, Json(value)),
            )
        self.conn.commit()

    # -- sources ------------------------------------------------------------

    def upsert_source(
        self,
        source_id: str,
        platform: str,
        account: str | None = None,
        machine: str | None = None,
        label: str | None = None,
    ) -> str:
        with self.conn.cursor() as cur:
            cur.execute(
                "INSERT INTO hist.sources (id, platform, account, machine, label) "
                "VALUES (%s, %s, %s, %s, %s) "
                "ON CONFLICT (id) DO UPDATE SET "
                "  platform = EXCLUDED.platform, "
                "  account  = coalesce(EXCLUDED.account, hist.sources.account), "
                "  machine  = coalesce(EXCLUDED.machine, hist.sources.machine), "
                "  label    = coalesce(EXCLUDED.label, hist.sources.label)",
                (source_id, platform, account, machine, label),
            )
        self.conn.commit()
        return source_id

    def set_source_status(self, source_id: str, status: str, detail: str | None = None) -> None:
        with self.conn.cursor() as cur:
            cur.execute(
                "UPDATE hist.sources SET status = %s, detail = %s, last_attempt_at = now(), "
                "  last_success_at = CASE WHEN %s IN ('ok', 'partial') THEN now() "
                "                        ELSE last_success_at END "
                "WHERE id = %s",
                (status, detail, status, source_id),
            )
        self.conn.commit()

    # -- conversations ------------------------------------------------------

    def upsert_conversation(self, fields: dict) -> int:
        params = {
            "source_id": fields["source_id"],
            "platform": fields["platform"],
            "external_id": fields["external_id"],
            "thread_id": fields.get("thread_id"),
            "title": scrub(fields.get("title")),
            "created_at": fields.get("created_at"),
            "updated_at": fields.get("updated_at"),
            "machine": fields.get("machine"),
            "project": fields.get("project"),
            "repo_url": fields.get("repo_url"),
            "branch": fields.get("branch"),
            "git_commit": fields.get("git_commit"),
            "cwd": fields.get("cwd"),
            "originator": fields.get("originator"),
            "workspace_id": fields.get("workspace_id"),
            "model": fields.get("model"),
            "source_ref": fields.get("source_ref"),
            "import_status": fields.get("import_status", "pending"),
            "import_error": fields.get("import_error"),
            "imported_at": fields.get("imported_at"),
            "content_hash": fields.get("content_hash"),
            "message_count": fields.get("message_count", 0),
            "raw": Json(scrub(fields["raw"])) if fields.get("raw") is not None else None,
        }
        with self.conn.cursor() as cur:
            cur.execute(CONVERSATION_UPSERT, params)
            (conversation_id,) = cur.fetchone()
        return conversation_id

    def mark_failed(self, source_id: str, external_id: str, error: str, **extra) -> int:
        """Record a source that could not be imported, rather than dropping it."""
        fields = {
            "source_id": source_id,
            "external_id": external_id,
            "import_status": "failed",
            "import_error": error[:2000],
            **extra,
        }
        fields.setdefault("platform", "codex")
        conversation_id = self.upsert_conversation(fields)
        self.conn.commit()
        return conversation_id

    def content_hashes(self, source_id: str) -> dict[str, str]:
        """external_id -> content_hash for rows already imported cleanly."""
        with self.conn.cursor() as cur:
            cur.execute(
                "SELECT external_id, content_hash FROM hist.conversations "
                "WHERE source_id = %s AND content_hash IS NOT NULL AND import_status = 'ok'",
                (source_id,),
            )
            return dict(cur.fetchall())

    # -- messages and artifacts --------------------------------------------

    def replace_messages(self, conversation_id: int, rows: Sequence[dict]) -> int:
        """Messages are rewritten wholesale: a conversation can gain branches."""
        with self.conn.cursor() as cur:
            cur.execute("DELETE FROM hist.messages WHERE conversation_id = %s", (conversation_id,))
            if rows:
                cur.executemany(
                    MESSAGE_INSERT,
                    [
                        {
                            "conversation_id": conversation_id,
                            "seq": row["seq"],
                            "external_id": row.get("external_id"),
                            "parent_id": row.get("parent_id"),
                            "role": row.get("role"),
                            "author": row.get("author"),
                            "content_type": row.get("content_type"),
                            "text": scrub(row.get("text")),
                            "created_at": row.get("created_at"),
                            "searchable": row.get("searchable", True),
                            "raw": Json(scrub(row.get("raw", {}))),
                        }
                        for row in rows
                    ],
                )
            cur.execute(
                "UPDATE hist.conversations SET message_count = %s WHERE id = %s",
                (len(rows), conversation_id),
            )
        return len(rows)

    def replace_artifacts(self, conversation_id: int, rows: Iterable[dict]) -> int:
        rows = list(rows)
        with self.conn.cursor() as cur:
            cur.execute("DELETE FROM hist.artifacts WHERE conversation_id = %s", (conversation_id,))
            if rows:
                cur.executemany(
                    ARTIFACT_INSERT,
                    [
                        {
                            "conversation_id": conversation_id,
                            "kind": row["kind"],
                            "name": scrub(row.get("name")),
                            "path": scrub(row.get("path")),
                            "detail": Json(scrub(row["detail"])) if row.get("detail") else None,
                        }
                        for row in rows
                    ],
                )
        return len(rows)

    # -- run bookkeeping ----------------------------------------------------

    def start_run(self, source_id: str) -> int:
        with self.conn.cursor() as cur:
            cur.execute(
                "INSERT INTO hist.import_runs (source_id, status) VALUES (%s, 'running') "
                "RETURNING id",
                (source_id,),
            )
            (run_id,) = cur.fetchone()
        self.conn.commit()
        return run_id

    def finish_run(self, run_id: int, status: str, counts: dict, error: str | None = None) -> None:
        with self.conn.cursor() as cur:
            cur.execute(
                "UPDATE hist.import_runs SET finished_at = now(), status = %s, seen = %s, "
                "  imported = %s, failed = %s, messages_saved = %s, error = %s WHERE id = %s",
                (
                    status,
                    counts.get("seen", 0),
                    counts.get("imported", 0),
                    counts.get("failed", 0),
                    counts.get("messages", 0),
                    error,
                    run_id,
                ),
            )
        self.conn.commit()
