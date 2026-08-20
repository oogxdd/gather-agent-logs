# gather-chatgpt-logs

Keeps your personal ChatGPT conversations in a local Postgres database, updated
as you use the desktop app.

## Why this is not a file copy

The ChatGPT desktop app stores **no conversation bodies on disk**. Verified on
macOS with app 26.814.41957 (Chromium 151):

- there is no `IndexedDB`, `Service Worker`, or `Cache Storage` directory in its
  Chromium profile at `~/Library/Application Support/Codex`
- `Default/History` is empty and the HTTP cache holds only static assets
- the sidebar list is cached in localStorage under `codex.chatgpt-conversations`
  — titles and ids only, every `mapping` is `null`, and it holds a sliding
  window of the last 20
- offline, opening a conversation fails with
  `/conversation/{id} → net::ERR_INTERNET_DISCONNECTED`

So message content only exists on the server and in the app's memory. What the
app does have is an authenticated `https://chatgpt.com` page context. This tool
attaches to that context over the DevTools protocol and asks it to fetch the
conversations, so **the session token never leaves the app** — nothing reads
your keychain, cookie jar, or `~/.codex/auth.json`.

ChatGPT **Work** and **Codex** threads are a different story: those run on the
local agent runtime and are written to `~/.codex/sessions/**.jsonl` in full, so
they are a plain file copy — `collect-agent-conversations.sh`, in this repo,
handles them. The two do not overlap: a Work thread never appears in the
personal conversation listing this tool reads.

## Setup

```bash
./install.sh
```

Creates a virtualenv, the `chatgpt_logs` database, the schema, and optionally a
LaunchAgent that runs the daemon at login.

## Running

The app must be started with its DevTools port open — Chromium only accepts the
flag at process start, so use this instead of the Dock icon:

```bash
./start-chatgpt.sh
```

Then:

```bash
./chatgpt_sync.py doctor      # check port, session, and database
./chatgpt_sync.py once        # one cycle
./chatgpt_sync.py daemon      # keep syncing, --interval 120 by default
./chatgpt_sync.py status      # what is stored, and what is pending
```

An idle cycle is one HTTP request and about two seconds: the listing is ordered
newest-first, so pagination stops at the first page that has not changed.

## History is skipped by default

The first run records a **baseline** timestamp. Every conversation is indexed
(id, title, timestamps — cheap, a few seconds for hundreds of them), but message
bodies are only fetched for conversations updated at or after the baseline. A
fresh install does not pull years of history.

To pull older bodies as well, deliberately:

```bash
./chatgpt_sync.py backfill --limit 50   # 50 most recent pending
./chatgpt_sync.py backfill              # everything
```

## Knowing what is not captured yet

The listing gives `update_time` for every conversation without opening it, so a
conversation you started and walked away from is detectable even if you never
looked at the answer:

```sql
SELECT * FROM chatgpt.pending;
```

A row appears when the stored body is missing (`never captured`) or older than
the server's version (`stale`). The daemon drains this list on each cycle.

## Schema

```text
chatgpt.conversations   one row per conversation; index_raw + body_raw as jsonb
chatgpt.messages        mapping exploded into rows, tree kept via parent_id/children
chatgpt.pending         view: bodies missing or out of date
chatgpt.sync_state      watermarks, notably baseline_at
chatgpt.sync_runs       per-cycle history and errors
```

Messages keep a full-text index, so searching works across languages:

```sql
SELECT c.title, m.role, m.text
FROM chatgpt.messages m JOIN chatgpt.conversations c ON c.id = m.conversation_id
WHERE to_tsvector('simple', coalesce(m.text, '')) @@ plainto_tsquery('simple', 'postgres');
```

The mapping root node is stored as a message row with no role and no text; that
is the tree root, not a bug.

## Notes

While the DevTools port is open, any local process can drive the app. It binds
to `127.0.0.1` only, but do not leave it open on a shared machine.

`/backend-api` is not a public interface. It can change without notice, and it
rate-limits — `--pause` controls the gap between body fetches.

The database holds your conversations in full. Keep it local; `.venv/` and
`logs/` are already git-ignored.

## Requirements

macOS, Python 3.10+, PostgreSQL 14+, and the ChatGPT desktop app signed in.
