#!/usr/bin/env python3
"""Repeatable tests for import and retrieval.

Runs against a throwaway database (created if missing) so it never touches the
real store. No test framework: plain asserts and a tiny runner, so the only
dependency is the one the tools already need.

    ./test_hist.py
    CHATGPT_TEST_DSN=postgresql:///something_else ./test_hist.py
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import traceback
from datetime import datetime, timedelta, timezone
from pathlib import Path

import psycopg

import codex_import
import hist_query
from histstore import Store

TEST_DB = os.environ.get("CHATGPT_TEST_DB", "chatgpt_logs_test")
TEST_DSN = os.environ.get("CHATGPT_TEST_DSN", f"postgresql:///{TEST_DB}")
MACHINE = "test-box"

BASE = datetime(2026, 5, 1, 12, 0, tzinfo=timezone.utc)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def ensure_database() -> None:
    try:
        psycopg.connect(TEST_DSN).close()
        return
    except psycopg.OperationalError:
        pass
    with psycopg.connect("postgresql:///postgres", autocommit=True) as conn:
        conn.execute(f'CREATE DATABASE "{TEST_DB}"')


def reset(store: Store) -> None:
    store.apply_schema()
    with store.conn.cursor() as cur:
        cur.execute("TRUNCATE hist.sources, hist.conversations, hist.messages, "
                    "hist.artifacts, hist.import_runs, hist.sync_state CASCADE")
    store.conn.commit()


def line(kind: str, payload: dict, offset: int = 0) -> str:
    stamp = (BASE + timedelta(seconds=offset)).isoformat().replace("+00:00", "Z")
    return json.dumps({"timestamp": stamp, "type": kind, "payload": payload})


def rollout(
    directory: Path,
    uuid: str,
    originator: str = "Codex Desktop",
    cwd: str = "/work/demo-project",
    extra: list[str] | None = None,
) -> Path:
    lines = [
        line(
            "session_meta",
            {
                "session_id": uuid,
                "timestamp": BASE.isoformat().replace("+00:00", "Z"),
                "cwd": cwd,
                "originator": originator,
                "cli_version": "0.0.0-test",
                "git": {
                    "branch": "main",
                    "commit_hash": "deadbeef",
                    "repository_url": "git@github.com:acme/demo-project.git",
                },
                "base_instructions": {"text": "x" * 5000},
            },
        ),
        line("turn_context", {"model": "gpt-5-test"}, 1),
        line(
            "response_item",
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "# AGENTS.md instructions for /work"}]},
            2,
        ),
        line(
            "response_item",
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "please add a marmalade cache layer"}]},
            3,
        ),
        line(
            "response_item",
            {"type": "custom_tool_call", "name": "exec", "call_id": "call-1",
             "input": "ls -la /work/demo-project"},
            4,
        ),
        line(
            "response_item",
            {"type": "custom_tool_call_output", "call_id": "call-1",
             "output": [{"type": "input_text", "text": "total 0"}]},
            5,
        ),
        line(
            "response_item",
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "Added the marmalade cache."}]},
            6,
        ),
        line(
            "event_msg",
            {"type": "patch_apply_end", "stdout": "Success:\nM /work/demo-project/cache.py"},
            7,
        ),
    ]
    lines.extend(extra or [])
    path = directory / f"rollout-2026-05-01T12-00-00-{uuid}.jsonl"
    path.write_text("\n".join(lines) + "\n")
    return path


def make_codex_home(root: Path) -> Path:
    home = root / "codex"
    (home / "sessions" / "2026" / "05" / "01").mkdir(parents=True)
    (home / "archived_sessions").mkdir(parents=True)
    return home


def sessions_dir(home: Path) -> Path:
    return home / "sessions" / "2026" / "05" / "01"


def add_chatgpt_conversation(store: Store, title: str, text: str, when: datetime) -> int:
    """The ChatGPT side, without needing the desktop app running."""
    store.upsert_source("chatgpt:test", "chatgpt", account="test", label="ChatGPT (test)")
    conversation_id = store.upsert_conversation(
        {
            "source_id": "chatgpt:test",
            "platform": "chatgpt",
            "external_id": f"cloud-{title}",
            "title": title,
            "created_at": when,
            "updated_at": when,
            "source_ref": "https://chatgpt.com/c/cloud",
            "import_status": "ok",
            "raw": {"id": "cloud"},
        }
    )
    store.replace_messages(
        conversation_id,
        [
            {"seq": 1, "role": "user", "text": text, "created_at": when, "raw": {}},
            {"seq": 2, "role": "assistant", "text": "Sure.", "created_at": when, "raw": {}},
        ],
    )
    store.conn.commit()
    return conversation_id


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_codex_import_roundtrip(store: Store, root: Path) -> None:
    home = make_codex_home(root)
    rollout(sessions_dir(home), "11111111-1111-4111-8111-111111111111")
    counts = codex_import.import_all(store, home, MACHINE, full=False, limit=None)
    assert counts["imported"] == 1, counts
    assert counts["failed"] == 0, counts

    result = hist_query.list_conversations(store, platform="codex")
    assert result["total"] == 1, result
    row = result["items"][0]
    assert row["project"] == "demo-project", row
    assert row["branch"] == "main", row
    assert row["machine"] == MACHINE, row
    assert row["model"] == "gpt-5-test", row
    assert row["source_ref"].endswith(".jsonl"), row
    # Title falls back to the first real user message, not the AGENTS.md block.
    assert "marmalade" in row["title"], row

    full = hist_query.get_conversation(store, row["conversation_id"])
    roles = [m["role"] for m in full["messages"]["items"]]
    assert roles == ["user", "user", "tool_call", "tool_output", "assistant"], roles
    kinds = {a["kind"] for a in full["artifacts"]}
    assert kinds == {"patch"}, full["artifacts"]


def test_work_platform_is_split_out(store: Store, root: Path) -> None:
    home = make_codex_home(root)
    rollout(sessions_dir(home), "22222222-2222-4222-8222-222222222222")
    rollout(
        sessions_dir(home),
        "33333333-3333-4333-8333-333333333333",
        originator="codex_work_desktop",
    )
    codex_import.import_all(store, home, MACHINE, full=False, limit=None)

    codex = hist_query.list_conversations(store, platform="codex")["total"]
    work = hist_query.list_conversations(store, platform="chatgpt_work")["total"]
    assert (codex, work) == (1, 1), (codex, work)

    sources = {s["id"] for s in hist_query.list_sources(store)}
    assert sources == {f"codex:{MACHINE}", f"chatgpt_work:{MACHINE}"}, sources


def test_unreadable_file_is_visible_not_dropped(store: Store, root: Path) -> None:
    home = make_codex_home(root)
    rollout(sessions_dir(home), "44444444-4444-4444-8444-444444444444")
    broken = sessions_dir(home) / "rollout-2026-05-01T12-00-00-55555555-5555-4555-8555-555555555555.jsonl"
    broken.write_text('{"type": "event_msg", "payload": {}}\n')  # no session_meta

    counts = codex_import.import_all(store, home, MACHINE, full=False, limit=None)
    assert counts["failed"] == 1, counts

    status = hist_query.sync_status(store)
    assert len(status["failures"]) == 1, status["failures"]
    assert "session_meta" in status["failures"][0]["import_error"], status["failures"]
    # It is still counted in the inventory rather than silently missing.
    assert hist_query.list_conversations(store)["total"] == 2


def test_unchanged_files_are_skipped(store: Store, root: Path) -> None:
    home = make_codex_home(root)
    rollout(sessions_dir(home), "66666666-6666-4666-8666-666666666666")
    first = codex_import.import_all(store, home, MACHINE, full=False, limit=None)
    second = codex_import.import_all(store, home, MACHINE, full=False, limit=None)
    assert first["imported"] == 1 and first["skipped"] == 0, first
    assert second["imported"] == 0 and second["skipped"] == 1, second


def test_resumed_session_keeps_both_files(store: Store, root: Path) -> None:
    """Two rollouts can share a session id; neither may overwrite the other."""
    home = make_codex_home(root)
    directory = sessions_dir(home)
    shared = "77777777-7777-4777-8777-777777777777"
    rollout(directory, "88888888-8888-4888-8888-888888888888").write_text(
        (directory / "rollout-2026-05-01T12-00-00-88888888-8888-4888-8888-888888888888.jsonl")
        .read_text()
        .replace("88888888-8888-4888-8888-888888888888", shared)
    )
    rollout(directory, "99999999-9999-4999-8999-999999999999").write_text(
        (directory / "rollout-2026-05-01T12-00-00-99999999-9999-4999-8999-999999999999.jsonl")
        .read_text()
        .replace("99999999-9999-4999-8999-999999999999", shared)
    )
    codex_import.import_all(store, home, MACHINE, full=False, limit=None)

    result = hist_query.list_conversations(store)
    assert result["total"] == 2, result
    with store.conn.cursor() as cur:
        cur.execute("SELECT count(DISTINCT thread_id) FROM hist.conversations")
        (threads,) = cur.fetchone()
    assert threads == 1, threads


def test_search_across_sources_and_filters(store: Store, root: Path) -> None:
    home = make_codex_home(root)
    rollout(sessions_dir(home), "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
    codex_import.import_all(store, home, MACHINE, full=False, limit=None)
    add_chatgpt_conversation(store, "Cloud chat", "marmalade on toast", BASE)

    everything = hist_query.search(store, "marmalade")
    platforms = {hit["source"]["platform"] for hit in everything["hits"]}
    assert platforms == {"codex", "chatgpt"}, platforms
    assert all(hit["source"]["source_ref"] for hit in everything["hits"])

    only_codex = hist_query.search(store, "marmalade", platform="codex")
    assert {h["source"]["platform"] for h in only_codex["hits"]} == {"codex"}

    # Filtering by project drops the cloud conversation, which has none.
    by_project = hist_query.search(store, "marmalade", project="demo")
    assert {h["source"]["platform"] for h in by_project["hits"]} == {"codex"}, by_project
    assert all(h["source"]["project"] == "demo-project" for h in by_project["hits"])

    by_machine = hist_query.search(store, "marmalade", machine="nowhere")
    assert by_machine["total"] == 0, by_machine

    later = (BASE + timedelta(days=1)).isoformat()
    assert hist_query.search(store, "marmalade", since=later)["total"] == 0

    by_role = hist_query.search(store, "marmalade", role="assistant")
    assert {h["role"] for h in by_role["hits"]} == {"assistant"}, by_role
    assert hist_query.search(store, "marmalade", role="tool_output")["total"] == 0


def test_injected_prompts_are_not_searchable(store: Store, root: Path) -> None:
    home = make_codex_home(root)
    rollout(sessions_dir(home), "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
    codex_import.import_all(store, home, MACHINE, full=False, limit=None)
    # The text is stored verbatim...
    with store.conn.cursor() as cur:
        cur.execute("SELECT count(*) FROM hist.messages WHERE text LIKE '# AGENTS.md%'")
        (stored,) = cur.fetchone()
    assert stored == 1, stored
    # ...but does not come back from search.
    assert hist_query.search(store, "AGENTS.md")["total"] == 0


def test_context_surrounds_the_hit(store: Store, root: Path) -> None:
    home = make_codex_home(root)
    rollout(sessions_dir(home), "cccccccc-cccc-4ccc-8ccc-cccccccccccc")
    codex_import.import_all(store, home, MACHINE, full=False, limit=None)

    hit = hist_query.search(store, "marmalade", role="user")["hits"][0]
    context = hist_query.get_context(store, hit["message_id"], before=1, after=2)
    seqs = [m["seq"] for m in context["messages"]]
    assert seqs == [1, 2, 3, 4], seqs
    assert [m["is_match"] for m in context["messages"]].count(True) == 1
    assert context["source"]["source_ref"].endswith(".jsonl")


def test_results_are_bounded(store: Store, root: Path) -> None:
    home = make_codex_home(root)
    rollout(sessions_dir(home), "dddddddd-dddd-4ddd-8ddd-dddddddddddd")
    codex_import.import_all(store, home, MACHINE, full=False, limit=None)

    result = hist_query.search(store, "marmalade", limit=10_000)
    assert result["limit"] == hist_query.MAX_LIMIT, result["limit"]

    conversation_id = hist_query.list_conversations(store)["items"][0]["conversation_id"]
    long_text = "x" * (hist_query.MAX_TEXT + 500)
    with store.conn.cursor() as cur:
        cur.execute(
            "UPDATE hist.messages SET text = %s WHERE conversation_id = %s AND seq = 1",
            (long_text, conversation_id),
        )
    store.conn.commit()
    message = hist_query.get_conversation(store, conversation_id)["messages"]["items"][0]
    assert message["truncated"] is True
    assert len(message["text"]) == hist_query.MAX_TEXT


def test_offline_source_is_recorded(store: Store, root: Path) -> None:
    """A machine that is simply not present must not look like success."""
    store.upsert_source(f"codex:{MACHINE}", "codex", machine=MACHINE)
    store.set_source_status(f"codex:{MACHINE}", "offline", "/nowhere not present")
    source = hist_query.list_sources(store)[0]
    assert source["status"] == "offline", source
    assert source["last_success_at"] is None, source


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

TESTS = [value for name, value in sorted(globals().items()) if name.startswith("test_")]


def main() -> int:
    ensure_database()
    passed = failed = 0
    with Store(TEST_DSN) as store:
        for test in TESTS:
            reset(store)
            with tempfile.TemporaryDirectory() as directory:
                try:
                    test(store, Path(directory))
                except Exception:  # noqa: BLE001 - report and keep going
                    failed += 1
                    print(f"FAIL  {test.__name__}")
                    print("      " + traceback.format_exc().replace("\n", "\n      ").strip())
                    store.conn.rollback()
                else:
                    passed += 1
                    print(f"ok    {test.__name__}")
    print(f"\n{passed} passed, {failed} failed  ({TEST_DSN})")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
