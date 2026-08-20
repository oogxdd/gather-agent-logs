# gather-agent-logs

A unified, local, queryable inventory of ChatGPT, ChatGPT Work, and Codex
history — one Postgres store, one shape, searchable across every source, with
every result linked back to where it came from.

Built to sit under an MCP server: `hist_query.py` is a set of plain functions
returning bounded, JSON-serialisable results, and the CLI is a thin wrapper over
them. Wiring MCP on top means calling those functions; no retrieval logic has to
move.

## What each source actually allows

The three sources are not equally accessible, and the difference is not a
detail — it decides how each one is collected.

| Source | Where it lives | How it is collected |
|---|---|---|
| **Codex** (CLI, TUI, desktop) | `~/.codex/sessions/**.jsonl` locally, in full | plain file read |
| **ChatGPT Work** | same place — Work runs on the local Codex runtime | plain file read |
| **ChatGPT** (personal) | server-side only | driven through the desktop app |

**Codex and ChatGPT Work are one mechanism.** The ChatGPT desktop app runs Work
threads on the local agent runtime, so a Work session is written to
`~/.codex/sessions` as an ordinary rollout. The only thing separating them is
`originator` on the first line of the file: `codex_work_desktop` is Work,
`Codex Desktop` / `codex_cli_rs` / `codex-tui` are Codex. Both are read straight
off disk and never modified.

**Personal ChatGPT keeps nothing on disk.** Verified on macOS with app
26.814.41957 (Chromium 151): no `IndexedDB`, `Service Worker`, or `Cache
Storage` directory in its profile at `~/Library/Application Support/Codex`;
`Default/History` empty; the HTTP cache holds only static assets; the sidebar
list is cached in localStorage under `codex.chatgpt-conversations` with every
`mapping` null, a sliding window of the last 20. Offline, opening a conversation
fails with `/conversation/{id} → net::ERR_INTERNET_DISCONNECTED`.

So message content exists only on the server and in the app's memory. What the
app does have is an authenticated `https://chatgpt.com` page. `chatgpt_sync.py`
attaches to that page over the DevTools protocol and asks it to fetch — **the
session token never leaves the app**; nothing reads the keychain, the cookie
jar, or `~/.codex/auth.json`.

Read [Limitations](#limitations) before relying on the ChatGPT side.

## Install

Requires PostgreSQL 14+, Python 3.10+, and (for the ChatGPT side) the ChatGPT
desktop app signed in.

```bash
./install.sh          # virtualenv, database, schema, optional LaunchAgent
```

Everything reads `CHATGPT_SYNC_DSN`, default `postgresql:///chatgpt_logs`.

## Collect

**Codex and ChatGPT Work** — a plain scan, no app involvement:

```bash
./codex_import.py import                       # incremental; unchanged files skipped
./codex_import.py import --machine work-laptop # name this computer explicitly
./codex_import.py status                       # per-source inventory
```

**Personal ChatGPT** — needs the desktop app running with its DevTools port.
Chromium only reads that flag at process start, so the daemon starts the app
itself:

```bash
./chatgpt_sync.py doctor      # check app, port, session, database
./chatgpt_sync.py daemon      # keeps syncing; starts ChatGPT when it is not up
./chatgpt_sync.py status
```

An idle cycle is one HTTP request and about two seconds: the listing is ordered
newest-first, so pagination stops at the first page that has not changed.

### History is skipped by default

The first ChatGPT run records a **baseline**. Every conversation is indexed (id,
title, timestamps — cheap), but message bodies are only fetched for
conversations updated at or after it. A fresh install does not pull years of
history. To pull older bodies deliberately:

```bash
./chatgpt_sync.py backfill --limit 50   # 50 most recent pending
./chatgpt_sync.py backfill              # everything
```

Codex has no equivalent: local files are read in full on the first pass.

## Query

```bash
./hist_query.py sources                                   # inventory + import state
./hist_query.py search "advisory lock" --platform codex   # search message text
./hist_query.py search "postgres" --project casey --role user --since 2026-03-01
./hist_query.py conversations --platform chatgpt_work
./hist_query.py show 1424 --limit 40                      # one transcript
./hist_query.py context 394448 --before 3 --after 3       # messages around a hit
./hist_query.py message 394448                            # full text plus raw record
./hist_query.py status                                    # what failed, what is stale
```

Add `--json` to any of them for machine-readable output.

Filters available on `search` and `conversations`: `--platform`, `--source`,
`--project` (substring, matches repo url too), `--machine`, `--since`,
`--until`, `--limit`, `--offset`; plus `--role` on search and `--title`,
`--status` on conversations.

`search` uses Postgres full-text with the `simple` configuration — no stemming,
so it behaves identically for every language in the corpus. Quote a phrase to
require adjacency. `--mode substring` is the escape hatch for identifiers and
paths the tokeniser splits up.

### As functions

```python
from histstore import Store
import hist_query

with Store() as store:
    hits = hist_query.search(store, "concurrency keys", platform="chatgpt", limit=5)
    for hit in hits["hits"]:
        print(hit["snippet"], hit["source"]["source_ref"])
        around = hist_query.get_context(store, hit["message_id"], before=2, after=2)
```

Every result carries a `source` block — platform, source id, external id,
`source_ref` (the rollout path or the conversation URL), machine, project,
branch — so a caller can always name the original. Results are bounded:
`limit` is clamped to 200 and message text to 4000 characters with an explicit
`truncated` flag, so an agent's context window cannot be flooded.

## Two computers

Each machine runs its own collector and records its own name:

```bash
./codex_import.py import --machine desk-pc
./codex_import.py import --machine work-laptop
```

Source ids become `codex:desk-pc`, `chatgpt_work:work-laptop`, and so on; the
`machine` column is on every conversation and is a filter everywhere. The two
never need to be online at the same time — they only need to reach the same
Postgres, or to run against a local one whose contents are merged later.

The three states the brief asks to distinguish are separate values on
`hist.sources.status`:

| Meaning | status | set when |
|---|---|---|
| No new information | `ok`, `imported = 0` on the run | a scan found only unchanged files |
| Computer offline | `offline` | the collector cannot find `~/.codex` |
| Import failed | `failed` | the run raised; per-file failures are rows |

A file that cannot be parsed is stored as a conversation with
`import_status = 'failed'` and the error text, so it appears in the inventory
instead of vanishing. `./hist_query.py status` lists them.

## Data model

```text
hist.sources        one row per platform+account+machine, with import state
hist.conversations  unified conversations and sessions, `raw` = original record
hist.messages       roles user/assistant/reasoning/tool_call/tool_output/…
hist.artifacts      commands run, files patched, web searches
hist.import_runs    per-run counts and errors
hist.sync_state     watermarks (baseline, daemon runtime state)

hist.overview       per-conversation status and scope  (view)
hist.pending        ChatGPT bodies missing or out of date  (view)
hist.inventory      per-source rollup  (view)
```

Imported content is verbatim. `raw` holds the original record on every
conversation and message row; normalised columns are a projection of it. The
one exception is NUL bytes, which Postgres rejects in both `text` and `jsonb`
and which Codex command output occasionally contains — they are stripped, and
`source_ref` still points at the untouched file.

Derived material — summaries, extracted tasks, detected decisions, suggested
relationships — does not belong in these tables. Put it in a separate `derived`
schema keyed by `conversation_id` / `message_id`.

App-injected prompts (`<permissions instructions>`, `<app-context>`,
`# AGENTS.md instructions for …` and friends) repeat verbatim across hundreds of
sessions. They are stored, but `searchable = false` keeps them out of the index
so they cannot drown real hits.

## Tray app

`tray/` is a macOS menu bar app (Tauri) over the ChatGPT daemon. Three states:
grey when ChatGPT is not running, green when it is running with the debug port,
red when it is running without one and nothing can be synced. The panel shows
the last sync, per-conversation status, and a manual sync button.

```bash
cd tray && pnpm install && pnpm build
```

It owns no sync logic — it reads Postgres and shells out to `chatgpt_sync.py`.

## Tests

```bash
./test_hist.py
```

Runs against a throwaway database (`chatgpt_logs_test`, created if missing), so
it never touches the real store. Covers import round-trip, Work/Codex platform
split, unreadable files staying visible, incremental skip, resumed sessions not
overwriting each other, cross-source search, every filter, injected prompts
being excluded, surrounding context, and result bounding.

## Limitations

- **Personal ChatGPT is not a file copy.** It requires the desktop app to be
  running with `--remote-debugging-port`, and it reads through the app's own
  authenticated page. If the app is launched from the Dock without that flag,
  nothing can be synced; the daemon reports `stalled` and notifies rather than
  failing quietly.
- **`/backend-api` is not a public interface.** It can change without notice and
  it rate-limits (`--pause` controls the gap between body fetches). Automated
  access to it is outside ChatGPT's intended use; the officially supported route
  for personal history is Settings → Data controls → Export data. Decide
  deliberately which you want. Nothing about the mechanism is hidden inside the
  code: it is one file, `chatgpt_sync.py`, and this paragraph.
- **`total` from the ChatGPT listing is unreliable** — it returned 2, 6, and 21
  for the same account depending on parameters, against 722 actual
  conversations. Pagination runs to an empty page instead.
- **ChatGPT attachments and generated images are not fetched.** Only the
  conversation mapping is. Codex artifacts (patches, commands, searches) are
  captured.
- **Reasoning content from Codex is mostly opaque** — `encrypted_content` is
  stored as-is; only the plaintext summary, when present, is searchable.
- **The tray app is macOS-only.** The importers and the query layer are not: the
  Python side is platform-agnostic, and Codex collection on Windows needs only
  `~/.codex` to exist. The ChatGPT daemon's app control (`open -a`, `osascript`,
  `pgrep`) is macOS-specific and would need Windows equivalents.
- **No vector search.** Keyword and full-text only, by design.

## Privacy

Everything is local: Postgres on your machine, no external service, no
telemetry. Credentials never appear in source or config — the ChatGPT side
borrows the desktop app's own session in-place and never extracts a token. The
database holds your conversations in full; `.venv/`, `logs/`, and build output
are git-ignored, and nothing in this repository contains history or secrets.

To remove imported data: rows are traceable by `source_id`, so
`DELETE FROM hist.sources WHERE id = '…'` cascades to everything from it.

## Also here

`collect-agent-conversations.sh` is the older, standalone snapshot script: it
copies Claude Code, Codex, and Crush conversations into a timestamped folder of
JSONL files, with no database. Independent of everything above.
