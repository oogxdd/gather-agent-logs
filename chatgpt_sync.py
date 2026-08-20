#!/usr/bin/env python3
"""Sync ChatGPT conversations from the desktop app into local Postgres.

The ChatGPT desktop app keeps no conversation bodies on disk: the sidebar list
is cached in localStorage, but message content is fetched per-open and held in
memory. So there is nothing to copy off the filesystem. What the app does have
is an authenticated ``https://chatgpt.com`` page context. This tool drives that
context over the DevTools protocol and asks it to fetch the conversations, so
the session token never leaves the app.

Only personal ChatGPT conversations are synced. ChatGPT Work and Codex threads
are written to ``~/.codex/sessions`` as JSONL and are collected separately.

    ./chatgpt_sync.py once
    ./chatgpt_sync.py daemon --interval 120
    ./chatgpt_sync.py status
    ./chatgpt_sync.py backfill --limit 50
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import psycopg
from psycopg.types.json import Json

from cdp import CDPError, WebSocket, evaluate, http_json

DEFAULT_DSN = os.environ.get("CHATGPT_SYNC_DSN", "postgresql:///chatgpt_logs")
DEFAULT_PORT = int(os.environ.get("CHATGPT_SYNC_PORT", "9222"))
SCHEMA_PATH = Path(__file__).resolve().parent / "schema.sql"

# The app's own listing parameters. They matter: with the defaults the API
# reports a fraction of the conversations (21 of 722 on the machine this was
# written against), and `total` in the response is unreliable regardless, so
# pagination runs until a page comes back empty.
LIST_PARAMS = (
    "exclude_conversation_origin=tpp"
    "&expand=false"
    "&is_archived=false"
    "&is_starred=false"
)
PAGE_SIZE = 100

_shutdown = False


def _log(message: str) -> None:
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    print(f"{stamp} {message}", flush=True)


def to_timestamp(value) -> datetime | None:
    """The listing returns ISO strings; conversation bodies return epoch floats."""
    if value is None or value == "":
        return None
    if isinstance(value, (int, float)):
        return datetime.fromtimestamp(value, tz=timezone.utc)
    if isinstance(value, str):
        try:
            return datetime.fromisoformat(value.replace("Z", "+00:00"))
        except ValueError:
            return None
    return None


# --------------------------------------------------------------------------
# App context
# --------------------------------------------------------------------------

PRIME_JS = """
(async () => {
  const r = await fetch('/api/auth/session', {credentials: 'include'});
  if (!r.ok) return JSON.stringify({ok: false, error: 'session HTTP ' + r.status});
  const s = await r.json();
  if (!s || !s.accessToken) return JSON.stringify({ok: false, error: 'not signed in'});
  globalThis.__gal = {
    token: s.accessToken,
    account: (s.account && s.account.id) || null,
    async get(path) {
      const headers = {'Authorization': 'Bearer ' + this.token};
      if (this.account) headers['ChatGPT-Account-Id'] = this.account;
      const resp = await fetch(path, {credentials: 'include', headers});
      const text = await resp.text();
      let body = null;
      try { body = JSON.parse(text); } catch (e) { body = text.slice(0, 400); }
      return {status: resp.status, body};
    }
  };
  return JSON.stringify({ok: true, account: globalThis.__gal.account,
                         user: s.user && s.user.id, expires: s.expires});
})()
"""

GET_JS = """
(async () => {
  const g = globalThis.__gal;
  if (!g) return JSON.stringify({ok: false, error: 'context lost'});
  const r = await g.get(%s);
  return JSON.stringify({ok: r.status === 200, status: r.status, body: r.body});
})()
"""


class AppContext:
    """An authenticated chatgpt.com page inside the desktop app."""

    def __init__(self, port: int = DEFAULT_PORT, timeout: float = 120.0):
        self.port = port
        self.timeout = timeout
        self.ws: WebSocket | None = None
        self.created_target: str | None = None
        self.account: str | None = None

    def _find_target(self) -> dict | None:
        for target in http_json("/json/list", self.port):
            if "chatgpt.com" in target.get("url", "") and target.get("webSocketDebuggerUrl"):
                return target
        return None

    def _create_target(self) -> dict:
        version = http_json("/json/version", self.port)
        with WebSocket(version["webSocketDebuggerUrl"], timeout=30) as browser:
            result = browser.call(
                "Target.createTarget", {"url": "https://chatgpt.com/", "background": True}
            )
        self.created_target = result["targetId"]
        for _ in range(40):
            time.sleep(0.5)
            target = self._find_target()
            if target:
                return target
        raise CDPError("created a chatgpt.com target but it never became attachable")

    def open(self) -> None:
        target = self._find_target() or self._create_target()
        self.ws = WebSocket(target["webSocketDebuggerUrl"], timeout=self.timeout)
        info = evaluate(self.ws, PRIME_JS, timeout=self.timeout)
        if not info.get("ok"):
            raise CDPError(f"cannot use the app session: {info.get('error')}")
        self.account = info.get("account")
        _log(f"attached to {target['url'][:60]} (account {self.account})")

    def close(self) -> None:
        if self.ws:
            self.ws.close()
            self.ws = None
        if self.created_target:
            try:
                version = http_json("/json/version", self.port)
                with WebSocket(version["webSocketDebuggerUrl"], timeout=15) as browser:
                    browser.call("Target.closeTarget", {"targetId": self.created_target})
            except CDPError:
                pass
            self.created_target = None

    def __enter__(self) -> "AppContext":
        self.open()
        return self

    def __exit__(self, *exc_info) -> None:
        self.close()

    def get(self, path: str, retries: int = 3):
        """GET a backend-api path from inside the app, with backoff on 429/5xx."""
        assert self.ws is not None, "context not open"
        delay = 2.0
        for attempt in range(retries):
            result = evaluate(self.ws, GET_JS % json.dumps(path), timeout=self.timeout)
            if result.get("ok"):
                return result["body"]
            status = result.get("status")
            if result.get("error") == "context lost":
                self.close()
                self.open()
                continue
            if status in (429, 500, 502, 503, 504) and attempt < retries - 1:
                _log(f"  HTTP {status} on {path[:70]} — retrying in {delay:.0f}s")
                time.sleep(delay)
                delay *= 2
                continue
            raise CDPError(f"GET {path[:80]} failed: HTTP {status} {result.get('body')}")
        raise CDPError(f"GET {path[:80]} failed after {retries} attempts")

    def list_page(self, offset: int) -> list[dict]:
        path = (
            f"/backend-api/conversations?{LIST_PARAMS}"
            f"&limit={PAGE_SIZE}&order=updated&offset={offset}"
        )
        return self.get(path).get("items") or []

    def conversation(self, conversation_id: str) -> dict:
        return self.get(f"/backend-api/conversation/{conversation_id}")


# --------------------------------------------------------------------------
# Storage
# --------------------------------------------------------------------------

CONVERSATION_UPSERT = """
INSERT INTO chatgpt.conversations
    (id, title, create_time, update_time, is_archived, is_starred,
     is_temporary_chat, workspace_id, conversation_origin, gizmo_id,
     async_status, current_node, index_raw, index_synced_at)
VALUES (%(id)s, %(title)s, %(create_time)s, %(update_time)s, %(is_archived)s,
        %(is_starred)s, %(is_temporary_chat)s, %(workspace_id)s,
        %(conversation_origin)s, %(gizmo_id)s, %(async_status)s,
        %(current_node)s, %(index_raw)s, now())
ON CONFLICT (id) DO UPDATE SET
    title               = EXCLUDED.title,
    create_time         = EXCLUDED.create_time,
    update_time         = EXCLUDED.update_time,
    is_archived         = EXCLUDED.is_archived,
    is_starred          = EXCLUDED.is_starred,
    is_temporary_chat   = EXCLUDED.is_temporary_chat,
    workspace_id        = EXCLUDED.workspace_id,
    conversation_origin = EXCLUDED.conversation_origin,
    gizmo_id            = EXCLUDED.gizmo_id,
    async_status        = EXCLUDED.async_status,
    current_node        = EXCLUDED.current_node,
    index_raw           = EXCLUDED.index_raw,
    index_synced_at     = now()
-- Skip the write when the listing row is byte-identical. A conflicting row
-- filtered out here returns nothing, which is how the caller tells "already
-- current" from "new or changed" and stops paginating.
WHERE conversations.index_raw IS DISTINCT FROM EXCLUDED.index_raw
RETURNING id
"""

MESSAGE_INSERT = """
INSERT INTO chatgpt.messages
    (conversation_id, id, parent_id, children, role, author_name, recipient,
     content_type, text, model_slug, status, end_turn, weight, create_time, raw)
VALUES (%(conversation_id)s, %(id)s, %(parent_id)s, %(children)s, %(role)s,
        %(author_name)s, %(recipient)s, %(content_type)s, %(text)s,
        %(model_slug)s, %(status)s, %(end_turn)s, %(weight)s, %(create_time)s,
        %(raw)s)
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

    def apply_schema(self) -> None:
        with self.conn.cursor() as cur:
            cur.execute(SCHEMA_PATH.read_text())
        self.conn.commit()

    def get_state(self, key: str):
        with self.conn.cursor() as cur:
            cur.execute("SELECT value FROM chatgpt.sync_state WHERE key = %s", (key,))
            row = cur.fetchone()
        return row[0] if row else None

    def set_state(self, key: str, value) -> None:
        with self.conn.cursor() as cur:
            cur.execute(
                "INSERT INTO chatgpt.sync_state (key, value, updated_at) "
                "VALUES (%s, %s, now()) "
                "ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
                (key, Json(value)),
            )
        self.conn.commit()

    def ensure_baseline(self) -> datetime:
        """Timestamp separating skipped history from conversations we capture.

        Set once, on the first run. Bodies are only fetched for conversations
        updated at or after it, so a first run does not pull years of history.
        """
        existing = self.get_state("baseline_at")
        if existing:
            return datetime.fromisoformat(existing["value"])
        now = datetime.now(timezone.utc)
        self.set_state("baseline_at", {"value": now.isoformat()})
        _log(f"baseline set to {now:%Y-%m-%d %H:%M:%SZ} — earlier history is indexed, not fetched")
        return now

    def upsert_index(self, item: dict) -> bool:
        """Store one listing row. Returns True when it was new or had changed.

        Which conversations need bodies is a separate question, answered by
        `pending()` against the whole table rather than page by page.
        """
        params = {
            "id": item["id"],
            "title": item.get("title"),
            "create_time": to_timestamp(item.get("create_time")),
            "update_time": to_timestamp(item.get("update_time")),
            "is_archived": item.get("is_archived"),
            "is_starred": item.get("is_starred"),
            "is_temporary_chat": item.get("is_temporary_chat"),
            "workspace_id": item.get("workspace_id"),
            "conversation_origin": item.get("conversation_origin"),
            "gizmo_id": item.get("gizmo_id"),
            "async_status": None if item.get("async_status") is None else str(item["async_status"]),
            "current_node": item.get("current_node"),
            "index_raw": Json(item),
        }
        with self.conn.cursor() as cur:
            cur.execute(CONVERSATION_UPSERT, params)
            return cur.fetchone() is not None

    def save_body(self, conversation_id: str, body: dict) -> int:
        """Replace the stored body and messages for one conversation."""
        update_time = to_timestamp(body.get("update_time"))
        mapping = body.get("mapping") or {}
        rows = []
        for node_id, node in mapping.items():
            message = node.get("message") or {}
            author = message.get("author") or {}
            content = message.get("content") or {}
            metadata = message.get("metadata") or {}

            parts = content.get("parts")
            if isinstance(parts, list):
                text = "\n".join(p for p in parts if isinstance(p, str)) or None
            else:
                text = content.get("text")

            rows.append(
                {
                    "conversation_id": conversation_id,
                    "id": node_id,
                    "parent_id": node.get("parent"),
                    "children": node.get("children") or [],
                    "role": author.get("role"),
                    "author_name": author.get("name"),
                    "recipient": message.get("recipient"),
                    "content_type": content.get("content_type"),
                    "text": text,
                    "model_slug": metadata.get("model_slug"),
                    "status": message.get("status"),
                    "end_turn": message.get("end_turn"),
                    "weight": message.get("weight"),
                    "create_time": to_timestamp(message.get("create_time")),
                    "raw": Json(node),
                }
            )

        with self.conn.cursor() as cur:
            cur.execute(
                "UPDATE chatgpt.conversations SET "
                "  body_raw = %s, body_update_time = %s, body_synced_at = now(), "
                "  title = coalesce(%s, title), current_node = coalesce(%s, current_node) "
                "WHERE id = %s",
                (Json(body), update_time, body.get("title"), body.get("current_node"), conversation_id),
            )
            cur.execute("DELETE FROM chatgpt.messages WHERE conversation_id = %s", (conversation_id,))
            if rows:
                cur.executemany(MESSAGE_INSERT, rows)
        self.conn.commit()
        return len(rows)

    def pending(self, baseline: datetime | None, limit: int | None) -> list[tuple[str, str]]:
        sql = [
            "SELECT id::text, coalesce(title, '(untitled)') FROM chatgpt.conversations",
            "WHERE (body_update_time IS NULL OR body_update_time < update_time)",
        ]
        params: list = []
        if baseline is not None:
            sql.append("AND update_time >= %s")
            params.append(baseline)
        sql.append("ORDER BY update_time DESC")
        if limit is not None:
            sql.append("LIMIT %s")
            params.append(limit)
        with self.conn.cursor() as cur:
            cur.execute(" ".join(sql), params)
            return cur.fetchall()

    def start_run(self) -> int:
        with self.conn.cursor() as cur:
            cur.execute(
                "INSERT INTO chatgpt.sync_runs (status) VALUES ('running') RETURNING id"
            )
            run_id = cur.fetchone()[0]
        self.conn.commit()
        return run_id

    def finish_run(self, run_id: int, status: str, counts: dict, error: str | None = None) -> None:
        with self.conn.cursor() as cur:
            cur.execute(
                "UPDATE chatgpt.sync_runs SET finished_at = now(), status = %s, "
                "index_seen = %s, index_changed = %s, bodies_synced = %s, "
                "messages_saved = %s, error = %s WHERE id = %s",
                (
                    status,
                    counts.get("index_seen", 0),
                    counts.get("index_changed", 0),
                    counts.get("bodies_synced", 0),
                    counts.get("messages_saved", 0),
                    error,
                    run_id,
                ),
            )
        self.conn.commit()


# --------------------------------------------------------------------------
# Sync
# --------------------------------------------------------------------------


def sync_index(app: AppContext, store: Store, full: bool) -> dict:
    """Walk the listing newest-first, upserting rows.

    Stops at the first page where nothing changed, unless ``full`` is set:
    the listing is ordered by update time, so an unchanged page means
    everything past it is unchanged too.
    """
    seen = changed = offset = 0
    while not _shutdown:
        items = app.list_page(offset)
        if not items:
            break
        page_changed = 0
        for item in items:
            seen += 1
            if store.upsert_index(item):
                page_changed += 1
        store.conn.commit()
        changed += page_changed
        _log(f"  index offset {offset}: {len(items)} rows, {page_changed} new/changed")
        if page_changed == 0 and not full:
            break
        offset += PAGE_SIZE
    return {"index_seen": seen, "index_changed": changed}


def sync_bodies(app: AppContext, store: Store, targets: list[tuple[str, str]], pause: float) -> dict:
    bodies = messages = 0
    for conversation_id, title in targets:
        if _shutdown:
            break
        body = app.conversation(conversation_id)
        count = store.save_body(conversation_id, body)
        bodies += 1
        messages += count
        _log(f"  saved {count:>4} messages  {title[:58]}")
        if pause:
            time.sleep(pause)
    return {"bodies_synced": bodies, "messages_saved": messages}


def run_cycle(args, store: Store, app: AppContext) -> dict:
    baseline = store.ensure_baseline()
    run_id = store.start_run()
    counts: dict = {}
    try:
        counts.update(sync_index(app, store, full=args.full_index))
        limit = None if args.limit in (None, 0) else args.limit
        window = None if args.backfill else baseline
        targets = store.pending(window, limit)
        if targets:
            _log(f"  {len(targets)} conversation(s) need bodies")
            counts.update(sync_bodies(app, store, targets, args.pause))
        store.finish_run(run_id, "ok", counts)
    except Exception as exc:  # noqa: BLE001 - recorded and re-raised
        store.conn.rollback()
        store.finish_run(run_id, "error", counts, f"{type(exc).__name__}: {exc}")
        raise
    return counts


# --------------------------------------------------------------------------
# Commands
# --------------------------------------------------------------------------


def cmd_once(args) -> int:
    with Store(args.dsn) as store:
        store.apply_schema()
        with AppContext(args.port) as app:
            counts = run_cycle(args, store, app)
    _log(
        "done: indexed {index_seen}, changed {index_changed}, "
        "bodies {bodies_synced}, messages {messages_saved}".format(
            **{**{"index_seen": 0, "index_changed": 0, "bodies_synced": 0, "messages_saved": 0}, **counts}
        )
    )
    return 0


def cmd_daemon(args) -> int:
    with Store(args.dsn) as store:
        store.apply_schema()
        while not _shutdown:
            try:
                with AppContext(args.port) as app:
                    run_cycle(args, store, app)
            except CDPError as exc:
                _log(f"app unavailable: {exc}")
            except Exception as exc:  # noqa: BLE001 - daemon must survive a bad cycle
                _log(f"cycle failed: {type(exc).__name__}: {exc}")
            for _ in range(int(args.interval)):
                if _shutdown:
                    break
                time.sleep(1)
    _log("stopped")
    return 0


def cmd_backfill(args) -> int:
    args.backfill = True
    args.full_index = True
    return cmd_once(args)


def cmd_status(args) -> int:
    with Store(args.dsn) as store:
        store.apply_schema()
        with store.conn.cursor() as cur:
            cur.execute(
                "SELECT count(*), count(body_raw), "
                "       min(update_time), max(update_time) FROM chatgpt.conversations"
            )
            total, with_body, oldest, newest = cur.fetchone()
            cur.execute("SELECT count(*) FROM chatgpt.messages")
            (messages,) = cur.fetchone()
            baseline = store.get_state("baseline_at")
            cutoff = datetime.fromisoformat(baseline["value"]) if baseline else None
            cur.execute(
                "SELECT count(*) FILTER (WHERE %s IS NULL OR update_time >= %s), count(*) "
                "FROM chatgpt.pending",
                (cutoff, cutoff),
            )
            due, skipped_total = cur.fetchone()
            cur.execute(
                "SELECT started_at, status, index_seen, bodies_synced, error "
                "FROM chatgpt.sync_runs ORDER BY id DESC LIMIT 5"
            )
            runs = cur.fetchall()
            cur.execute(
                "SELECT title, update_time, reason FROM chatgpt.pending "
                "WHERE %s IS NULL OR update_time >= %s LIMIT 10",
                (cutoff, cutoff),
            )
            pending_rows = cur.fetchall()

    history = skipped_total - due
    print(f"conversations   {total} ({with_body} with bodies)")
    print(f"messages        {messages}")
    print(f"update_time     {oldest} .. {newest}")
    print(f"baseline        {baseline['value'] if baseline else '(not set)'}")
    print(f"due now         {due}  (the daemon fetches these)")
    print(f"pre-baseline    {history}  (skipped history; `backfill` to pull it)")
    if pending_rows:
        print("\ndue now:")
        for title, update_time, reason in pending_rows:
            print(f"  {update_time:%Y-%m-%d %H:%M}  {reason:<14} {(title or '')[:52]}")
    if runs:
        print("\nrecent runs:")
        for started, status, seen, bodies, error in runs:
            line = f"  {started:%Y-%m-%d %H:%M}  {status:<7} indexed={seen:<5} bodies={bodies}"
            print(line + (f"  {error[:60]}" if error else ""))
    return 0


def cmd_doctor(args) -> int:
    ok = True
    try:
        version = http_json("/json/version", args.port)
        print(f"[ok]   debug port {args.port}: {version['Browser']}")
    except CDPError as exc:
        print(f"[FAIL] debug port {args.port}: {exc}")
        return 1
    targets = [t for t in http_json("/json/list", args.port) if "chatgpt.com" in t.get("url", "")]
    print(
        f"[ok]   chatgpt.com context present ({targets[0]['url'][:50]})"
        if targets
        else "[warn] no chatgpt.com context — one will be created on demand"
    )
    try:
        with Store(args.dsn) as store:
            store.apply_schema()
            print(f"[ok]   postgres {args.dsn}: schema applied")
    except Exception as exc:  # noqa: BLE001
        print(f"[FAIL] postgres {args.dsn}: {exc}")
        ok = False
    try:
        with AppContext(args.port) as app:
            print(f"[ok]   app session usable (account {app.account})")
    except CDPError as exc:
        print(f"[FAIL] app session: {exc}")
        ok = False
    return 0 if ok else 1


def main(argv: list[str] | None = None) -> int:
    # Shared options live on a parent parser attached to each subcommand, so
    # they are written after it: `backfill --limit 50`. Putting them on the top
    # parser too would let the subparser's defaults overwrite them.
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--dsn", default=DEFAULT_DSN, help=f"Postgres DSN (default: {DEFAULT_DSN})")
    common.add_argument("--port", type=int, default=DEFAULT_PORT, help="CDP port (default: 9222)")
    common.add_argument("--pause", type=float, default=0.4, help="seconds between body fetches")
    common.add_argument("--limit", type=int, default=0, help="max bodies per cycle (0 = no limit)")
    common.add_argument("--interval", type=float, default=120, help="daemon seconds between cycles")
    common.add_argument(
        "--full-index", action="store_true", help="walk every listing page, not just changed ones"
    )
    common.add_argument(
        "--backfill", action="store_true", help="also fetch bodies for pre-baseline history"
    )

    parser = argparse.ArgumentParser(
        description="Sync ChatGPT conversations from the desktop app into Postgres.",
    )
    sub = parser.add_subparsers(dest="command", required=True)
    for name, help_text in (
        ("once", "run a single sync cycle"),
        ("daemon", "sync on an interval"),
        ("backfill", "fetch bodies for older conversations too"),
        ("status", "show what is stored"),
        ("doctor", "check the app, the debug port, and the database"),
    ):
        sub.add_parser(name, parents=[common], help=help_text)

    args = parser.parse_args(argv)

    handlers = {
        "once": cmd_once,
        "daemon": cmd_daemon,
        "backfill": cmd_backfill,
        "status": cmd_status,
        "doctor": cmd_doctor,
    }
    try:
        return handlers[args.command](args)
    except CDPError as exc:
        _log(f"error: {exc}")
        return 1


def _handle_signal(signum, _frame):
    global _shutdown
    _shutdown = True
    _log(f"signal {signum} — finishing current step")


if __name__ == "__main__":
    signal.signal(signal.SIGINT, _handle_signal)
    signal.signal(signal.SIGTERM, _handle_signal)
    sys.exit(main())
