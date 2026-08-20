#!/usr/bin/env python3
"""Sync personal ChatGPT conversations into the unified history store.

The ChatGPT desktop app keeps no conversation bodies on disk: the sidebar list
is cached in localStorage with every mapping null, and opening a conversation
offline fails outright. Message content only exists server-side and in the app's
memory, so there is nothing to copy off the filesystem.

What the app does have is an authenticated https://chatgpt.com page context.
This tool drives that context over the DevTools protocol and asks it to fetch,
so the session token never leaves the app — nothing here reads the keychain,
the cookie jar, or ~/.codex/auth.json.

ChatGPT Work and Codex threads run on the local agent runtime and are written
to ~/.codex/sessions as JSONL; codex_import.py handles those.

    ./chatgpt_sync.py doctor
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
import subprocess
import sys
import time
from datetime import datetime, timezone

from cdp import CDPError, WebSocket, evaluate, http_json
from histstore import DEFAULT_DSN, Store, scrub, to_timestamp

DEFAULT_PORT = int(os.environ.get("CHATGPT_SYNC_PORT", "9222"))

# The app's own listing parameters. They matter: with the defaults the API
# returns a fraction of the conversations (21 of 722 on the machine this was
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
    print(f"{datetime.now(timezone.utc):%Y-%m-%dT%H:%M:%SZ} {message}", flush=True)


# --------------------------------------------------------------------------
# App lifecycle
#
# Chromium only reads --remote-debugging-port at process start, so an app the
# user launched from the Dock can never be attached to. The daemon therefore
# owns launching it. States are written to hist.sync_state so a wrapper (the
# tray app) can render them without reimplementing any of this.
# --------------------------------------------------------------------------

APP_NAME = os.environ.get("CHATGPT_APP", "ChatGPT")
APP_BINARY = f"/Applications/{APP_NAME}.app/Contents/MacOS/{APP_NAME}"

READY = "ready"
STALLED = "stalled"
UNAVAILABLE = "unavailable"


def port_open(port: int) -> bool:
    try:
        http_json("/json/version", port, timeout=2.0)
        return True
    except CDPError:
        return False


def app_running() -> bool:
    return subprocess.run(["pgrep", "-f", APP_BINARY], capture_output=True).returncode == 0


def notify(title: str, message: str) -> None:
    """Best-effort macOS notification. Never fatal: this is a status hint."""
    script = f"display notification {json.dumps(message)} with title {json.dumps(title)}"
    subprocess.run(["osascript", "-e", script], capture_output=True)


def quit_app(wait: float = 20.0) -> bool:
    subprocess.run(["osascript", "-e", f'quit app "{APP_NAME}"'], capture_output=True)
    deadline = time.time() + wait
    while time.time() < deadline:
        if not app_running():
            return True
        time.sleep(0.5)
    return False


def launch_app(port: int, wait: float = 40.0) -> bool:
    subprocess.run(
        ["open", "-a", APP_NAME, "--args", f"--remote-debugging-port={port}"],
        capture_output=True,
    )
    deadline = time.time() + wait
    while time.time() < deadline:
        if port_open(port):
            return True
        time.sleep(0.5)
    return False


def ensure_app(port: int, policy: str) -> tuple[str, str]:
    """Bring the app into a state the sync can attach to.

    policy: 'missing' starts it when it is not running, 'always' additionally
    restarts one that is running without the port, 'never' only ever attaches.
    """
    if port_open(port):
        return READY, "debug port reachable"
    if policy == "never":
        return UNAVAILABLE, "port closed and --launch never"

    running = app_running()
    if running and policy != "always":
        return STALLED, (
            f"{APP_NAME} is running without --remote-debugging-port. "
            "Quit it and let the daemon start it, or use --launch always."
        )
    if running:
        _log(f"restarting {APP_NAME}: running without the debug port")
        if not quit_app():
            return STALLED, f"{APP_NAME} would not quit"

    _log(f"starting {APP_NAME} with --remote-debugging-port={port}")
    if launch_app(port):
        return READY, "started by the daemon"
    return UNAVAILABLE, f"started {APP_NAME} but 127.0.0.1:{port} never opened"


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

# A freshly launched app has targets whose reported URL is already chatgpt.com
# while the document is still about:blank, and its session is restored a moment
# after that. Both have to be waited out before priming.
READY_JS = """
(async () => JSON.stringify({origin: location.origin, readyState: document.readyState}))()
"""


class AppContext:
    """An authenticated chatgpt.com page inside the desktop app."""

    def __init__(self, port: int = DEFAULT_PORT, timeout: float = 120.0):
        self.port = port
        self.timeout = timeout
        self.ws: WebSocket | None = None
        self.created_target: str | None = None
        self.account: str | None = None

    def _candidates(self) -> list[dict]:
        return [
            t
            for t in http_json("/json/list", self.port)
            if "chatgpt.com" in t.get("url", "") and t.get("webSocketDebuggerUrl")
        ]

    def _create_target(self) -> None:
        version = http_json("/json/version", self.port)
        with WebSocket(version["webSocketDebuggerUrl"], timeout=30) as browser:
            result = browser.call(
                "Target.createTarget", {"url": "https://chatgpt.com/", "background": True}
            )
        self.created_target = result["targetId"]
        _log("opened a chatgpt.com target for the sync")

    def _try(self, target: dict) -> str | None:
        """Attach and prime one target. Returns None on success, else why not."""
        try:
            ws = WebSocket(target["webSocketDebuggerUrl"], timeout=self.timeout)
        except CDPError as exc:
            return str(exc)
        try:
            probe = evaluate(ws, READY_JS, timeout=30)
            if probe.get("origin") != "https://chatgpt.com":
                ws.close()
                return f"origin is {probe.get('origin')}"
            if probe.get("readyState") == "loading":
                ws.close()
                return "page still loading"
            info = evaluate(ws, PRIME_JS, timeout=self.timeout)
            if not info.get("ok"):
                ws.close()
                return str(info.get("error"))
        except CDPError as exc:
            ws.close()
            return str(exc)
        self.ws = ws
        self.account = info.get("account")
        _log(f"attached to {target['url'][:60]} (account {self.account})")
        return None

    def open(self, wait: float = 90.0) -> None:
        deadline = time.time() + wait
        reason = "no chatgpt.com target"
        while time.time() < deadline and not _shutdown:
            for target in self._candidates():
                reason = self._try(target)
                if reason is None:
                    return
            # Half the budget spent on the app's own targets; then open our own,
            # in case the only candidate is a page that will never be usable.
            if self.created_target is None and time.time() > deadline - wait / 2:
                self._create_target()
            time.sleep(2)
        raise CDPError(f"no usable chatgpt.com context after {wait:.0f}s ({reason})")

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
# Storage helpers on top of the shared store
# --------------------------------------------------------------------------


def source_id_for(account: str | None) -> str:
    return f"chatgpt:{account or 'default'}"


def index_fields(source_id: str, item: dict) -> dict:
    return {
        "source_id": source_id,
        "platform": "chatgpt",
        "external_id": item["id"],
        "title": item.get("title"),
        "created_at": to_timestamp(item.get("create_time")),
        "updated_at": to_timestamp(item.get("update_time")),
        "workspace_id": item.get("workspace_id"),
        "source_ref": f"https://chatgpt.com/c/{item['id']}",
        "import_status": "pending",
        "raw": item,
    }


def walk_mapping(mapping: dict) -> list[str]:
    """Linearise the message tree depth-first from its roots.

    A ChatGPT conversation is a tree — regenerations and edits create branches —
    so there is no single correct flat order. Pre-order keeps each branch
    contiguous, which is what reading surrounding context wants.
    """
    order: list[str] = []
    seen: set[str] = set()
    roots = [node_id for node_id, node in mapping.items() if not node.get("parent")]
    stack = list(reversed(roots or list(mapping)))
    while stack:
        node_id = stack.pop()
        if node_id in seen or node_id not in mapping:
            continue
        seen.add(node_id)
        order.append(node_id)
        for child in reversed(mapping[node_id].get("children") or []):
            stack.append(child)
    order.extend(node_id for node_id in mapping if node_id not in seen)
    return order


def body_messages(body: dict) -> list[dict]:
    mapping = body.get("mapping") or {}
    rows = []
    for seq, node_id in enumerate(walk_mapping(mapping), start=1):
        node = mapping[node_id]
        message = node.get("message") or {}
        author = message.get("author") or {}
        content = message.get("content") or {}
        metadata = message.get("metadata") or {}

        parts = content.get("parts")
        if isinstance(parts, list):
            text = "\n".join(part for part in parts if isinstance(part, str)) or None
        else:
            text = content.get("text")

        role = author.get("role")
        rows.append(
            {
                "seq": seq,
                "external_id": node_id,
                "parent_id": node.get("parent"),
                "role": role,
                "author": author.get("name") or metadata.get("model_slug"),
                "content_type": content.get("content_type"),
                "text": text,
                "created_at": to_timestamp(message.get("create_time")),
                "searchable": role not in ("system",),
                "raw": node,
            }
        )
    return rows


def pending_conversations(
    store: Store, source_id: str, baseline: datetime | None, limit: int | None
) -> list[tuple[int, str, str]]:
    sql = [
        "SELECT id, external_id, coalesce(title, '(untitled)') FROM hist.conversations",
        "WHERE platform = 'chatgpt' AND source_id = %s",
        "  AND (body_updated_at IS NULL OR body_updated_at < updated_at)",
    ]
    params: list = [source_id]
    if baseline is not None:
        sql.append("AND updated_at >= %s")
        params.append(baseline)
    sql.append("ORDER BY updated_at DESC")
    if limit is not None:
        sql.append("LIMIT %s")
        params.append(limit)
    with store.conn.cursor() as cur:
        cur.execute(" ".join(sql), params)
        return cur.fetchall()


def ensure_baseline(store: Store) -> datetime:
    """Timestamp separating skipped history from conversations we capture.

    Set once, on the first run. Bodies are only fetched for conversations
    updated at or after it, so a first run does not pull years of history.
    """
    existing = store.get_state("baseline_at")
    if existing:
        return datetime.fromisoformat(existing["value"])
    stamp = datetime.now(timezone.utc)
    store.set_state("baseline_at", {"value": stamp.isoformat()})
    _log(f"baseline set to {stamp:%Y-%m-%d %H:%M:%SZ} — earlier history is indexed, not fetched")
    return stamp


def record_runtime(store: Store, state: str, detail: str) -> bool:
    """Persist the app/sync state and announce transitions. Returns True on change."""
    previous = store.get_state("runtime") or {}
    changed = previous.get("state") != state
    stamp = datetime.now(timezone.utc).isoformat()
    store.set_state(
        "runtime",
        {
            "state": state,
            "detail": detail,
            "since": stamp if changed else previous.get("since", stamp),
            "observed_at": stamp,
        },
    )
    if changed:
        if state != READY:
            notify("ChatGPT sync stalled", detail)
        elif previous:
            notify("ChatGPT sync resumed", detail)
    return changed


# --------------------------------------------------------------------------
# Sync
# --------------------------------------------------------------------------


def sync_index(app: AppContext, store: Store, source_id: str, full: bool) -> dict:
    """Walk the listing newest-first, upserting rows.

    Stops at the first page where nothing changed, unless ``full`` is set: the
    listing is ordered by update time, so an unchanged page means everything
    past it is unchanged too.
    """
    seen = changed = offset = 0
    while not _shutdown:
        items = app.list_page(offset)
        if not items:
            break
        page_changed = 0
        for item in items:
            seen += 1
            fields = index_fields(source_id, item)
            with store.conn.cursor() as cur:
                cur.execute(
                    "SELECT raw FROM hist.conversations WHERE source_id = %s AND external_id = %s",
                    (source_id, item["id"]),
                )
                row = cur.fetchone()
            if row is not None and row[0] == scrub(item):
                continue
            store.upsert_conversation(fields)
            page_changed += 1
        store.conn.commit()
        changed += page_changed
        _log(f"  index offset {offset}: {len(items)} rows, {page_changed} new/changed")
        if page_changed == 0 and not full:
            break
        offset += PAGE_SIZE
    return {"seen": seen, "index_changed": changed}


def sync_bodies(
    app: AppContext, store: Store, targets: list[tuple[int, str, str]], pause: float
) -> dict:
    bodies = messages = failed = 0
    for conversation_id, external_id, title in targets:
        if _shutdown:
            break
        with store.conn.cursor() as cur:
            cur.execute(
                "UPDATE hist.conversations SET body_started_at = now() WHERE id = %s",
                (conversation_id,),
            )
        store.conn.commit()  # visible to the tray app while the fetch runs

        try:
            body = app.conversation(external_id)
            rows = body_messages(body)
            store.replace_messages(conversation_id, rows)
            with store.conn.cursor() as cur:
                cur.execute(
                    "UPDATE hist.conversations SET "
                    "  raw = coalesce(raw, '{}'::jsonb), title = coalesce(%s, title), "
                    "  body_updated_at = %s, body_imported_at = now(), "
                    "  import_status = 'ok', import_error = NULL, imported_at = now() "
                    "WHERE id = %s",
                    (body.get("title"), to_timestamp(body.get("update_time")), conversation_id),
                )
            store.conn.commit()
            bodies += 1
            messages += len(rows)
            _log(f"  saved {len(rows):>4} messages  {title[:56]}")
        except Exception as exc:  # noqa: BLE001 - one bad conversation must stay visible
            store.conn.rollback()
            with store.conn.cursor() as cur:
                cur.execute(
                    "UPDATE hist.conversations SET import_status = 'failed', import_error = %s "
                    "WHERE id = %s",
                    (f"{type(exc).__name__}: {exc}"[:2000], conversation_id),
                )
            store.conn.commit()
            failed += 1
            _log(f"  FAILED {title[:56]}: {type(exc).__name__}: {exc}")
        if pause:
            time.sleep(pause)
    return {"imported": bodies, "messages": messages, "failed": failed}


def run_cycle(args, store: Store, app: AppContext) -> dict:
    source_id = source_id_for(app.account)
    store.upsert_source(source_id, "chatgpt", account=app.account, label="ChatGPT (personal)")
    if not store.try_lock(source_id):
        _log("another sync holds the lock — skipping this cycle")
        return {}

    baseline = ensure_baseline(store)
    run_id = store.start_run(source_id)
    counts: dict = {}
    try:
        counts.update(sync_index(app, store, source_id, full=args.full_index))
        limit = None if args.limit in (None, 0) else args.limit
        window = None if args.backfill else baseline
        targets = pending_conversations(store, source_id, window, limit)
        if targets:
            _log(f"  {len(targets)} conversation(s) need bodies")
            counts.update(sync_bodies(app, store, targets, args.pause))
        store.finish_run(run_id, "ok", counts)
        store.set_source_status(source_id, "ok", f"{counts.get('imported', 0)} bodies this run")
    except Exception as exc:  # noqa: BLE001 - recorded and re-raised
        store.conn.rollback()
        store.finish_run(run_id, "error", counts, f"{type(exc).__name__}: {exc}")
        store.set_source_status(source_id, "failed", f"{type(exc).__name__}: {exc}"[:400])
        raise
    finally:
        store.unlock(source_id)
    return counts


# --------------------------------------------------------------------------
# Commands
# --------------------------------------------------------------------------


def cmd_once(args) -> int:
    with Store(args.dsn) as store:
        store.apply_schema()
        state, detail = ensure_app(args.port, args.launch)
        record_runtime(store, state, detail)
        if state != READY:
            _log(f"{state}: {detail}")
            return 1
        with AppContext(args.port) as app:
            counts = run_cycle(args, store, app)
        record_runtime(store, READY, "last cycle completed")
    blank = {"seen": 0, "index_changed": 0, "imported": 0, "messages": 0, "failed": 0}
    _log(
        "done: indexed {seen}, changed {index_changed}, bodies {imported}, "
        "messages {messages}, failed {failed}".format(**{**blank, **counts})
    )
    return 0


def cmd_daemon(args) -> int:
    with Store(args.dsn) as store:
        store.apply_schema()
        while not _shutdown:
            try:
                state, detail = ensure_app(args.port, args.launch)
                if record_runtime(store, state, detail) and state != READY:
                    _log(f"{state}: {detail}")
                if state == READY:
                    with AppContext(args.port) as app:
                        run_cycle(args, store, app)
                    record_runtime(store, READY, "last cycle completed")
            except CDPError as exc:
                _log(f"app unreachable: {exc}")
                record_runtime(store, UNAVAILABLE, str(exc))
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


def cmd_start(args) -> int:
    """Bring the app up with the debug port. Also the tray-app entry point."""
    state, detail = ensure_app(args.port, "always" if args.force else "missing")
    print(f"{state}: {detail}")
    with Store(args.dsn) as store:
        store.apply_schema()
        record_runtime(store, state, detail)
    return 0 if state == READY else 1


def cmd_stop(args) -> int:
    if not app_running():
        print("not running")
        return 0
    ok = quit_app()
    print("stopped" if ok else f"{APP_NAME} would not quit")
    return 0 if ok else 1


def cmd_status(args) -> int:
    with Store(args.dsn) as store:
        store.apply_schema()
        runtime = store.get_state("runtime")
        baseline = store.get_state("baseline_at")
        cutoff = datetime.fromisoformat(baseline["value"]) if baseline else None
        with store.conn.cursor() as cur:
            cur.execute(
                "SELECT count(*), count(*) FILTER (WHERE body_imported_at IS NOT NULL), "
                "       min(updated_at), max(updated_at) "
                "FROM hist.conversations WHERE platform = 'chatgpt'"
            )
            total, with_body, oldest, newest = cur.fetchone()
            cur.execute(
                "SELECT count(*) FILTER (WHERE %s IS NULL OR updated_at >= %s), count(*) "
                "FROM hist.pending",
                (cutoff, cutoff),
            )
            due, pending_total = cur.fetchone()
            cur.execute(
                "SELECT title, updated_at, reason FROM hist.pending "
                "WHERE %s IS NULL OR updated_at >= %s LIMIT 10",
                (cutoff, cutoff),
            )
            rows = cur.fetchall()
            cur.execute(
                "SELECT started_at, status, seen, imported, error FROM hist.import_runs "
                "WHERE source_id LIKE 'chatgpt:%%' ORDER BY id DESC LIMIT 5"
            )
            runs = cur.fetchall()

    if runtime:
        marker = "ok" if runtime["state"] == READY else "!!"
        print(f"{marker} {runtime['state'].upper()}  {runtime['detail']}")
        print(f"   since {runtime['since'][:19]}, last checked {runtime['observed_at'][:19]}")
    else:
        print("?? never run")
    print()
    print(f"conversations   {total} ({with_body} with bodies)")
    print(f"updated         {oldest} .. {newest}")
    print(f"baseline        {baseline['value'] if baseline else '(not set)'}")
    print(f"due now         {due}  (the daemon fetches these)")
    print(f"pre-baseline    {pending_total - due}  (skipped history; `backfill` to pull it)")
    if rows:
        print("\ndue now:")
        for title, updated_at, reason in rows:
            print(f"  {updated_at:%Y-%m-%d %H:%M}  {reason:<14} {(title or '')[:52]}")
    if runs:
        print("\nrecent runs:")
        for started, status, seen, imported, error in runs:
            line = f"  {started:%Y-%m-%d %H:%M}  {status:<7} indexed={seen:<5} bodies={imported}"
            print(line + (f"  {error[:60]}" if error else ""))
    return 0


def cmd_doctor(args) -> int:
    ok = True
    if port_open(args.port):
        version = http_json("/json/version", args.port)
        print(f"[ok]   debug port {args.port}: {version['Browser']}")
    elif app_running():
        print(f"[FAIL] {APP_NAME} is running WITHOUT --remote-debugging-port")
        print("       nothing can be synced until it restarts:")
        print("       ./chatgpt_sync.py start --force")
        return 1
    else:
        print(f"[warn] {APP_NAME} is not running; the daemon would start it")
        print("       ./chatgpt_sync.py start")
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
    common.add_argument(
        "--launch",
        choices=("missing", "always", "never"),
        default=os.environ.get("CHATGPT_SYNC_LAUNCH", "missing"),
        help="missing: start the app when it is not running (default); "
        "always: also restart one running without the debug port; "
        "never: only attach to an app that already has the port",
    )
    common.add_argument(
        "--force", action="store_true", help="`start`: restart the app even if it is running"
    )

    parser = argparse.ArgumentParser(
        description="Sync personal ChatGPT conversations from the desktop app into Postgres.",
    )
    sub = parser.add_subparsers(dest="command", required=True)
    for name, help_text in (
        ("once", "run a single sync cycle"),
        ("daemon", "sync on an interval, starting the app as needed"),
        ("backfill", "fetch bodies for older conversations too"),
        ("status", "show sync state and what is stored"),
        ("start", "start the app with the debug port"),
        ("stop", "quit the app"),
        ("doctor", "check the app, the debug port, and the database"),
    ):
        sub.add_parser(name, parents=[common], help=help_text)

    args = parser.parse_args(argv)
    handlers = {
        "once": cmd_once,
        "daemon": cmd_daemon,
        "backfill": cmd_backfill,
        "status": cmd_status,
        "start": cmd_start,
        "stop": cmd_stop,
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
