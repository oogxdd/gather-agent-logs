#!/usr/bin/env python3
"""Retrieval over the unified history store.

Plain functions returning JSON-serialisable dicts, plus a CLI over the same
functions. An MCP server can wrap these directly: every result is bounded, and
every result carries the reference needed to go back to the original — platform,
source, external id, message position, and the file path or URL it came from.

    ./hist_query.py sources
    ./hist_query.py search "advisory lock" --platform codex --limit 5
    ./hist_query.py conversations --project gather-agent-logs
    ./hist_query.py show 1234 --limit 40
    ./hist_query.py context 987654 --before 3 --after 3
    ./hist_query.py status
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone
from typing import Any

from histstore import DEFAULT_DSN, Store

# Results are bounded so a caller with a context window cannot be flooded.
MAX_LIMIT = 200
MAX_TEXT = 4000
DEFAULT_LIMIT = 20


def _bound(limit: int | None, default: int = DEFAULT_LIMIT) -> int:
    if not limit or limit < 1:
        return default
    return min(limit, MAX_LIMIT)


def _clip(text: str | None, size: int = MAX_TEXT) -> tuple[str | None, bool]:
    if text is None:
        return None, False
    if len(text) <= size:
        return text, False
    return text[:size], True


def _stamp(value: datetime | None) -> str | None:
    return value.astimezone(timezone.utc).isoformat() if value else None


def _rows(cur) -> list[dict]:
    names = [column.name for column in cur.description]
    return [dict(zip(names, row)) for row in cur.fetchall()]


# ---------------------------------------------------------------------------
# Filters shared by search and listing
# ---------------------------------------------------------------------------


def _filters(
    platform: str | None = None,
    source: str | None = None,
    project: str | None = None,
    machine: str | None = None,
    since: str | None = None,
    until: str | None = None,
    prefix: str = "c",
) -> tuple[list[str], list[Any]]:
    clauses: list[str] = []
    params: list[Any] = []
    for column, value in (
        ("platform", platform),
        ("source_id", source),
        ("machine", machine),
    ):
        if value:
            clauses.append(f"{prefix}.{column} = %s")
            params.append(value)
    if project:
        # Substring so 'gather' finds 'gather-agent-logs'; repo_url is checked
        # too because a project can be identified either way.
        clauses.append(f"({prefix}.project ILIKE %s OR {prefix}.repo_url ILIKE %s)")
        params.extend([f"%{project}%", f"%{project}%"])
    if since:
        clauses.append(f"{prefix}.updated_at >= %s")
        params.append(since)
    if until:
        clauses.append(f"{prefix}.updated_at < %s")
        params.append(until)
    return clauses, params


def _reference(row: dict) -> dict:
    """Everything needed to find the original again."""
    return {
        "conversation_id": row.get("conversation_id") or row.get("id"),
        "platform": row.get("platform"),
        "source_id": row.get("source_id"),
        "external_id": row.get("external_id"),
        "source_ref": row.get("source_ref"),
        "machine": row.get("machine"),
        "project": row.get("project"),
        "branch": row.get("branch"),
    }


# ---------------------------------------------------------------------------
# Queries
# ---------------------------------------------------------------------------


def list_sources(store: Store) -> list[dict]:
    """Inventory, including sources that failed or were never imported."""
    with store.conn.cursor() as cur:
        cur.execute(
            "SELECT id, platform, account, machine, label, status, detail, "
            "       last_attempt_at, last_success_at, conversations, imported, "
            "       failed, messages, newest "
            "FROM hist.inventory ORDER BY platform, id"
        )
        rows = _rows(cur)
    for row in rows:
        for key in ("last_attempt_at", "last_success_at", "newest"):
            row[key] = _stamp(row[key])
    return rows


def list_conversations(
    store: Store,
    platform: str | None = None,
    source: str | None = None,
    project: str | None = None,
    machine: str | None = None,
    since: str | None = None,
    until: str | None = None,
    title: str | None = None,
    status: str | None = None,
    limit: int = DEFAULT_LIMIT,
    offset: int = 0,
) -> dict:
    clauses, params = _filters(platform, source, project, machine, since, until)
    if title:
        clauses.append("c.title ILIKE %s")
        params.append(f"%{title}%")
    if status:
        clauses.append("o.status = %s")
        params.append(status)
    where = ("WHERE " + " AND ".join(clauses)) if clauses else ""
    limit = _bound(limit)

    with store.conn.cursor() as cur:
        cur.execute(
            f"SELECT count(*) FROM hist.conversations c "
            f"JOIN hist.overview o ON o.id = c.id {where}",
            params,
        )
        (total,) = cur.fetchone()
        cur.execute(
            f"""
            SELECT c.id AS conversation_id, c.platform, c.source_id, c.external_id,
                   c.thread_id, c.title, c.created_at, c.updated_at, c.machine,
                   c.project, c.repo_url, c.branch, c.cwd, c.model, c.originator,
                   c.message_count, c.source_ref, c.import_status, c.import_error,
                   o.status
            FROM hist.conversations c
            JOIN hist.overview o ON o.id = c.id
            {where}
            ORDER BY c.updated_at DESC NULLS LAST, c.id DESC
            LIMIT %s OFFSET %s
            """,
            [*params, limit, max(0, offset)],
        )
        rows = _rows(cur)

    for row in rows:
        row["created_at"] = _stamp(row["created_at"])
        row["updated_at"] = _stamp(row["updated_at"])
    return {"total": total, "limit": limit, "offset": offset, "items": rows}


def search(
    store: Store,
    query: str,
    platform: str | None = None,
    source: str | None = None,
    project: str | None = None,
    machine: str | None = None,
    role: str | None = None,
    since: str | None = None,
    until: str | None = None,
    mode: str = "fts",
    limit: int = DEFAULT_LIMIT,
    offset: int = 0,
    snippet: int = 240,
) -> dict:
    """Search message text, newest first within relevance.

    mode 'fts' uses the 'simple' text search configuration — no stemming, so it
    behaves the same for every language in the corpus. mode 'substring' is the
    escape hatch for identifiers and paths that the tokeniser splits up.
    """
    clauses, params = _filters(platform, source, project, machine, since, until)
    clauses.append("m.searchable")
    if role:
        clauses.append("m.role = %s")
        params.append(role)

    if mode == "substring":
        match_sql = "m.text ILIKE %s"
        match_params = [f"%{query}%"]
        rank_sql = "0::float4"
        headline = "left(m.text, %s)"
        headline_params = [snippet]
    else:
        match_sql = "to_tsvector('simple', coalesce(m.text, '')) @@ q"
        match_params = []
        rank_sql = "ts_rank_cd(to_tsvector('simple', coalesce(m.text, '')), q)"
        headline = (
            "ts_headline('simple', coalesce(m.text, ''), q, "
            "'MaxFragments=2, MaxWords=28, MinWords=8, ShortWord=2, "
            "StartSel=<<, StopSel=>>')"
        )
        headline_params = []
    clauses.append(match_sql)

    where = " AND ".join(clauses)
    limit = _bound(limit)
    source_cte = "websearch_to_tsquery('simple', %s) AS q" if mode != "substring" else "NULL AS q"
    cte_params = [query] if mode != "substring" else []

    with store.conn.cursor() as cur:
        cur.execute(
            f"""
            WITH tsq AS (SELECT {source_cte})
            SELECT count(*)
            FROM hist.messages m
            JOIN hist.conversations c ON c.id = m.conversation_id
            CROSS JOIN tsq
            WHERE {where}
            """,
            [*cte_params, *params, *match_params],
        )
        (total,) = cur.fetchone()

        cur.execute(
            f"""
            WITH tsq AS (SELECT {source_cte})
            SELECT m.id AS message_id, m.seq, m.role, m.author, m.content_type,
                   m.created_at, {headline} AS snippet, length(m.text) AS text_length,
                   {rank_sql} AS rank,
                   c.id AS conversation_id, c.platform, c.source_id, c.external_id,
                   c.title, c.machine, c.project, c.branch, c.source_ref,
                   c.updated_at AS conversation_updated_at
            FROM hist.messages m
            JOIN hist.conversations c ON c.id = m.conversation_id
            CROSS JOIN tsq
            WHERE {where}
            ORDER BY rank DESC, c.updated_at DESC NULLS LAST, m.id DESC
            LIMIT %s OFFSET %s
            """,
            [*cte_params, *headline_params, *params, *match_params, limit, max(0, offset)],
        )
        rows = _rows(cur)

    hits = []
    for row in rows:
        hits.append(
            {
                "message_id": row["message_id"],
                "seq": row["seq"],
                "role": row["role"],
                "author": row["author"],
                "content_type": row["content_type"],
                "created_at": _stamp(row["created_at"]),
                "snippet": row["snippet"],
                "text_length": row["text_length"],
                "rank": float(row["rank"] or 0),
                "title": row["title"],
                "conversation_updated_at": _stamp(row["conversation_updated_at"]),
                "source": _reference(row),
            }
        )
    return {"query": query, "mode": mode, "total": total, "limit": limit,
            "offset": offset, "hits": hits}


def get_conversation(
    store: Store,
    conversation_id: int,
    limit: int = 50,
    offset: int = 0,
    roles: list[str] | None = None,
    include_text: bool = True,
) -> dict:
    limit = _bound(limit, 50)
    with store.conn.cursor() as cur:
        cur.execute(
            """
            SELECT c.id AS conversation_id, c.platform, c.source_id, c.external_id,
                   c.thread_id, c.title, c.created_at, c.updated_at, c.machine,
                   c.project, c.repo_url, c.branch, c.git_commit, c.cwd, c.model,
                   c.originator, c.workspace_id, c.message_count, c.source_ref,
                   c.import_status, c.import_error
            FROM hist.conversations c WHERE c.id = %s
            """,
            (conversation_id,),
        )
        rows = _rows(cur)
        if not rows:
            return {"error": f"no conversation {conversation_id}"}
        conversation = rows[0]

        clauses = ["conversation_id = %s"]
        params: list[Any] = [conversation_id]
        if roles:
            clauses.append("role = ANY(%s)")
            params.append(roles)
        where = " AND ".join(clauses)
        cur.execute(f"SELECT count(*) FROM hist.messages WHERE {where}", params)
        (total,) = cur.fetchone()
        cur.execute(
            f"""
            SELECT id AS message_id, seq, external_id, parent_id, role, author,
                   content_type, created_at, text, length(text) AS text_length
            FROM hist.messages WHERE {where}
            ORDER BY seq LIMIT %s OFFSET %s
            """,
            [*params, limit, max(0, offset)],
        )
        messages = _rows(cur)

        cur.execute(
            "SELECT kind, name, path FROM hist.artifacts WHERE conversation_id = %s "
            "ORDER BY id LIMIT 100",
            (conversation_id,),
        )
        artifacts = _rows(cur)

    conversation["created_at"] = _stamp(conversation["created_at"])
    conversation["updated_at"] = _stamp(conversation["updated_at"])
    for message in messages:
        message["created_at"] = _stamp(message["created_at"])
        if include_text:
            message["text"], message["truncated"] = _clip(message["text"])
        else:
            message.pop("text", None)
    return {
        "conversation": conversation,
        "source": _reference(conversation),
        "messages": {"total": total, "limit": limit, "offset": offset, "items": messages},
        "artifacts": artifacts,
    }


def get_context(store: Store, message_id: int, before: int = 3, after: int = 3) -> dict:
    """The messages around a hit, so a result is never an isolated fragment."""
    before = max(0, min(before, 25))
    after = max(0, min(after, 25))
    with store.conn.cursor() as cur:
        cur.execute(
            "SELECT conversation_id, seq FROM hist.messages WHERE id = %s", (message_id,)
        )
        row = cur.fetchone()
        if not row:
            return {"error": f"no message {message_id}"}
        conversation_id, seq = row

        cur.execute(
            """
            SELECT c.id AS conversation_id, c.platform, c.source_id, c.external_id,
                   c.title, c.machine, c.project, c.branch, c.source_ref, c.updated_at
            FROM hist.conversations c WHERE c.id = %s
            """,
            (conversation_id,),
        )
        conversation = _rows(cur)[0]

        cur.execute(
            """
            SELECT id AS message_id, seq, role, author, content_type, created_at,
                   text, length(text) AS text_length
            FROM hist.messages
            WHERE conversation_id = %s AND seq BETWEEN %s AND %s
            ORDER BY seq
            """,
            (conversation_id, seq - before, seq + after),
        )
        messages = _rows(cur)

    conversation["updated_at"] = _stamp(conversation["updated_at"])
    for message in messages:
        message["created_at"] = _stamp(message["created_at"])
        message["text"], message["truncated"] = _clip(message["text"])
        message["is_match"] = message["message_id"] == message_id
    return {
        "message_id": message_id,
        "conversation": conversation,
        "source": _reference(conversation),
        "messages": messages,
    }


def get_message(store: Store, message_id: int) -> dict:
    """One message with its untruncated text and the original record."""
    with store.conn.cursor() as cur:
        cur.execute(
            """
            SELECT m.id AS message_id, m.seq, m.external_id, m.parent_id, m.role,
                   m.author, m.content_type, m.created_at, m.text, m.raw,
                   c.id AS conversation_id, c.platform, c.source_id,
                   c.external_id AS conversation_external_id, c.title, c.machine,
                   c.project, c.branch, c.source_ref
            FROM hist.messages m
            JOIN hist.conversations c ON c.id = m.conversation_id
            WHERE m.id = %s
            """,
            (message_id,),
        )
        rows = _rows(cur)
    if not rows:
        return {"error": f"no message {message_id}"}
    row = rows[0]
    row["created_at"] = _stamp(row["created_at"])
    row["source"] = _reference({**row, "external_id": row["conversation_external_id"]})
    return row


def sync_status(store: Store) -> dict:
    """Which sources are current, which failed, and what is not imported yet."""
    with store.conn.cursor() as cur:
        cur.execute(
            "SELECT platform, count(*) AS conversations, "
            "       count(*) FILTER (WHERE import_status = 'ok') AS imported, "
            "       count(*) FILTER (WHERE import_status = 'failed') AS failed, "
            "       coalesce(sum(message_count), 0) AS messages, max(updated_at) AS newest "
            "FROM hist.conversations GROUP BY platform ORDER BY platform"
        )
        platforms = _rows(cur)
        cur.execute(
            "SELECT status, count(*) FROM hist.overview GROUP BY status ORDER BY 2 DESC"
        )
        statuses = dict(cur.fetchall())
        cur.execute(
            "SELECT id AS conversation_id, platform, external_id, title, import_error, source_ref "
            "FROM hist.conversations WHERE import_status = 'failed' "
            "ORDER BY updated_at DESC NULLS LAST LIMIT 25"
        )
        failures = _rows(cur)
        cur.execute(
            "SELECT source_id, started_at, finished_at, status, seen, imported, failed, error "
            "FROM hist.import_runs ORDER BY id DESC LIMIT 10"
        )
        runs = _rows(cur)
        runtime = store.get_state("runtime")

    for row in platforms:
        row["newest"] = _stamp(row["newest"])
    for row in runs:
        row["started_at"] = _stamp(row["started_at"])
        row["finished_at"] = _stamp(row["finished_at"])
    return {
        "sources": list_sources(store),
        "platforms": platforms,
        "conversation_status": statuses,
        "failures": failures,
        "recent_runs": runs,
        "chatgpt_runtime": runtime,
    }


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _one_line(text: str | None, size: int = 78) -> str:
    """Titles are often a whole first message; keep listings to one row each."""
    collapsed = " ".join((text or "(untitled)").split())
    return collapsed[:size] + ("…" if len(collapsed) > size else "")


def _print_json(value: Any) -> None:
    print(json.dumps(value, ensure_ascii=False, indent=2, default=str))


def _print_search(result: dict) -> None:
    print(
        f"{result['total']} hit(s) for {result['query']!r} "
        f"[{result['mode']}], showing {len(result['hits'])}\n"
    )
    for hit in result["hits"]:
        source = hit["source"]
        where = " / ".join(filter(None, [source["platform"], source["project"], source["machine"]]))
        print(f"  #{hit['message_id']}  {where}")
        print(f"  {_one_line(hit['title'], 60)}  ·  {hit['role']}  ·  {hit['created_at'] or '?'}")
        snippet = " ".join((hit["snippet"] or "").split())
        print(f"    {snippet[:300]}")
        print(f"    conversation {source['conversation_id']} · {source['source_ref'] or '-'}")
        print()


def _print_conversations(result: dict) -> None:
    print(f"{result['total']} conversation(s), showing {len(result['items'])}\n")
    for row in result["items"]:
        where = " / ".join(filter(None, [row["platform"], row["project"], row["machine"]]))
        print(f"  #{row['conversation_id']:<6} {where}")
        print(f"    {_one_line(row['title'])}")
        print(
            f"    {row['updated_at'] or '?'} · {row['message_count']} msgs · "
            f"{row['status']}" + (f" · {row['branch']}" if row["branch"] else "")
        )
        print()


def _print_transcript(result: dict) -> None:
    conversation = result["conversation"]
    print(f"#{conversation['conversation_id']}  {_one_line(conversation['title'], 90)}")
    where = " / ".join(
        filter(None, [conversation["platform"], conversation["project"], conversation["machine"]])
    )
    print(f"  {where}")
    print(f"  source: {conversation['source_ref'] or '-'}")
    if conversation.get("branch"):
        print(f"  branch: {conversation['branch']}  cwd: {conversation.get('cwd') or '-'}")
    messages = result["messages"]
    print(f"  {messages['total']} messages, showing {len(messages['items'])}\n")
    for message in messages["items"]:
        head = f"  [{message['seq']}] {message['role'] or '?'}"
        if message.get("author"):
            head += f" ({message['author']})"
        print(f"{head}  {message['created_at'] or ''}")
        text = (message.get("text") or "").strip()
        for line in text.splitlines()[:20]:
            print(f"      {line[:160]}")
        if message.get("truncated"):
            print("      … truncated")
        print()


def _print_status(result: dict) -> None:
    print("sources")
    for row in result["sources"]:
        print(
            f"  {row['id']:<46} {row['status']:<8} "
            f"{row['conversations']:>5} convs  {row['messages']:>8} msgs  "
            f"failed={row['failed']}"
        )
    print("\nconversation status")
    for name, count in result["conversation_status"].items():
        print(f"  {name:<10} {count}")
    if result["failures"]:
        print(f"\nfailures ({len(result['failures'])} shown)")
        for row in result["failures"]:
            print(f"  #{row['conversation_id']} {row['platform']}: {_one_line(row['import_error'], 80)}")
    print("\nrecent runs")
    for row in result["recent_runs"]:
        print(
            f"  {(row['started_at'] or '')[:19]}  {row['source_id']:<44} "
            f"{row['status']:<7} seen={row['seen']:<5} imported={row['imported']}"
        )


def main(argv: list[str] | None = None) -> int:
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--dsn", default=DEFAULT_DSN)
    common.add_argument("--json", action="store_true", help="raw JSON output")

    filters = argparse.ArgumentParser(add_help=False)
    filters.add_argument("--platform", choices=("chatgpt", "chatgpt_work", "codex"))
    filters.add_argument("--source", help="exact source id, e.g. codex:my-laptop")
    filters.add_argument("--project", help="project or repository, substring match")
    filters.add_argument("--machine", help="computer the session ran on")
    filters.add_argument("--since", help="ISO date/time lower bound on last update")
    filters.add_argument("--until", help="ISO date/time upper bound on last update")
    filters.add_argument("--limit", type=int, default=DEFAULT_LIMIT)
    filters.add_argument("--offset", type=int, default=0)

    parser = argparse.ArgumentParser(description="Query the unified history store.")
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("sources", parents=[common], help="inventory of sources")
    sub.add_parser("status", parents=[common], help="import and sync state, including failures")

    p_search = sub.add_parser("search", parents=[common, filters], help="search message text")
    p_search.add_argument("query")
    p_search.add_argument("--role", help="user, assistant, tool_call, tool_output, reasoning")
    p_search.add_argument("--mode", choices=("fts", "substring"), default="fts")

    p_list = sub.add_parser(
        "conversations", parents=[common, filters], help="list conversations and sessions"
    )
    p_list.add_argument("--title", help="title substring")
    p_list.add_argument("--status", choices=("synced", "stale", "never", "syncing", "failed"))

    p_show = sub.add_parser("show", parents=[common], help="one conversation transcript")
    p_show.add_argument("conversation_id", type=int)
    p_show.add_argument("--limit", type=int, default=50)
    p_show.add_argument("--offset", type=int, default=0)
    p_show.add_argument("--roles", nargs="*", help="only these roles")

    p_context = sub.add_parser("context", parents=[common], help="messages around a hit")
    p_context.add_argument("message_id", type=int)
    p_context.add_argument("--before", type=int, default=3)
    p_context.add_argument("--after", type=int, default=3)

    p_message = sub.add_parser("message", parents=[common], help="one message, full text and raw")
    p_message.add_argument("message_id", type=int)

    args = parser.parse_args(argv)

    with Store(args.dsn) as store:
        if args.command == "sources":
            result = list_sources(store)
            if args.json:
                _print_json(result)
            else:
                for row in result:
                    print(
                        f"{row['id']:<46} {row['platform']:<13} {row['status']:<8} "
                        f"{row['conversations']:>5} convs {row['messages']:>8} msgs"
                    )
            return 0

        if args.command == "status":
            result = sync_status(store)
            _print_json(result) if args.json else _print_status(result)
            return 0

        if args.command == "search":
            result = search(
                store, args.query, platform=args.platform, source=args.source,
                project=args.project, machine=args.machine, role=args.role,
                since=args.since, until=args.until, mode=args.mode,
                limit=args.limit, offset=args.offset,
            )
            _print_json(result) if args.json else _print_search(result)
            return 0

        if args.command == "conversations":
            result = list_conversations(
                store, platform=args.platform, source=args.source, project=args.project,
                machine=args.machine, since=args.since, until=args.until,
                title=args.title, status=args.status, limit=args.limit, offset=args.offset,
            )
            _print_json(result) if args.json else _print_conversations(result)
            return 0

        if args.command == "show":
            result = get_conversation(
                store, args.conversation_id, limit=args.limit, offset=args.offset,
                roles=args.roles or None,
            )
            if "error" in result:
                print(result["error"], file=sys.stderr)
                return 1
            _print_json(result) if args.json else _print_transcript(result)
            return 0

        if args.command == "context":
            result = get_context(store, args.message_id, args.before, args.after)
            if "error" in result:
                print(result["error"], file=sys.stderr)
                return 1
            if args.json:
                _print_json(result)
            else:
                _print_transcript(
                    {
                        "conversation": {**result["conversation"], "cwd": None},
                        "messages": {
                            "total": len(result["messages"]),
                            "items": result["messages"],
                        },
                    }
                )
            return 0

        if args.command == "message":
            result = get_message(store, args.message_id)
            if "error" in result:
                print(result["error"], file=sys.stderr)
                return 1
            _print_json(result)
            return 0

    return 1


if __name__ == "__main__":
    sys.exit(main())
