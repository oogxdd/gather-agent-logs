# gather-agent-logs

Coding agents keep their history on the machine that ran them. Work done in a
sandbox stays in that sandbox, and a session started on a laptop is invisible
from anywhere else. This repository collects every machine's Codex and Claude
Code sessions into one Postgres database, and gives you one terminal picker
over all of them.

```text
laptop ─┐
sprite ─┼─▶ agent-logs ship ──▶ Postgres ──▶ agent-resume  (picker + reader)
sprite ─┘   (agent hooks)                    agent-logs list/search/show
```

Two binaries:

- **`agent-logs`** runs on every machine. It uploads new log bytes, and reads
  back what all the machines collected.
- **`agent-resume`** is the picker. Local sessions can be resumed in place;
  sessions from another machine are read in a built-in transcript viewer.

Each session is stored with the machine it ran on, the agent CLI that produced
it, its session ID, working directory, and full transcript.

## Quick start

### 1. Get a Postgres

Any Postgres 12 or newer works. Managed free tiers are enough: see
[Storage](#storage-and-cost) for how much they hold. Keep the connection
string handy; it looks like:

```text
postgresql://user:password@host/database?sslmode=require
```

The database is the only shared component — there is no server to run. TLS is
used automatically unless the URL says `sslmode=disable`.

### 2. Set up each machine

From a checkout, or straight from the repository:

```bash
cargo install --locked --git https://github.com/oogxdd/gather-agent-logs
agent-logs setup --database-url 'postgresql://…' --machine my-laptop
agent-logs install-hooks
agent-logs ship
```

`--machine` names this machine's sessions; it defaults to the hostname, which
on a Sprite is already the sprite's name. `setup` writes
`~/.config/agent-logs/config.json` with owner-only permissions and creates the
schema if it is missing.

The whole sequence, for a new sandbox:

```bash
scripts/bootstrap-machine.sh --database-url 'postgresql://…'
```

### 3. Read everything from your main machine

```bash
agent-resume          # picker over local + collected sessions
agent-logs list       # plain listing
agent-logs search 'auth race'
agent-logs show 019ff21e
```

## Keeping the database current

`install-hooks` wires the collector into the agents themselves:

| Agent | Trigger | What runs |
| --- | --- | --- |
| Claude Code | `Stop`, `SessionEnd` hooks in `settings.json` | uploads that one transcript |
| Codex | `notify` in `config.toml` | rescans Codex logs after each turn |

Existing hooks and settings are preserved, and the original file is copied to
`*.agent-logs-backup` the first time. Use `--dry-run` to see the changes first.
Hooks never fail the agent: if the database is unreachable, the upload is
skipped and the agent carries on.

Codex only reports finished turns, so for continuous uploads run the watcher
instead — on a Sprite, as a service that survives the shell:

```bash
agent-logs watch --interval 60
sprite-env services create agent-logs --cmd "$HOME/.cargo/bin/agent-logs" --args "watch,--interval,60"
```

## Storage and cost

Transcripts are stored as gzip-compressed append-only chunks, and only real
conversation text is indexed for search. That matters: agent logs are mostly
tool output, so a row per log line plus its indexes would cost several times
the raw size.

Measured on one machine's real logs (12 sessions, one week of heavy use):

| | Size |
| --- | --- |
| Raw JSONL on disk | 29.7 MB |
| Compressed chunks | 7.9 MB |
| Database total, indexes included | ~11 MB |

That is about 2.7x smaller than the logs themselves, so a 0.5 GB free tier
holds roughly **1.3 GB of raw agent history** — months of work from several
machines. Watch it with `agent-logs stats`, and trim with:

```bash
agent-logs prune --older-than 90d --yes                    # delete old sessions
agent-logs prune --older-than 90d --transcripts-only --yes # keep them findable
```

`prune` prints what it would delete unless `--yes` is passed. Dropping only
transcripts keeps titles, timestamps, and searchable messages at a fraction of
the storage.

## Commands

```text
agent-logs setup --database-url URL [--machine NAME]  store the URL, create the schema
agent-logs ship [--file PATH]                         upload everything new
agent-logs watch [--interval SECONDS]                 upload on a loop
agent-logs install-hooks [--agent claude|codex] [--dry-run]
agent-logs hook <claude|codex>                        upload from inside a hook
agent-logs list [--from NAME] [--agent AGENT] [--limit N] [--json]
agent-logs show SESSION_ID [--from NAME] [--raw]      print a transcript
agent-logs search QUERY [--from NAME] [--agent AGENT]
agent-logs hosts                                      machines that reported
agent-logs stats                                      storage per machine and agent
agent-logs prune --older-than AGE [--transcripts-only] [--yes]
```

`SESSION_ID` accepts a unique prefix. Anything that reads the database also
accepts `--database-url` and `--host`, and the environment variables
`AGENT_LOGS_DATABASE_URL`, `AGENT_LOGS_HOST`, and `AGENT_LOGS_CONFIG`.

The picker takes the same filters plus:

```text
agent-resume [--source local|remote|all] [--from MACHINE] [--agent AGENT]
             [--sort updated|created] [--limit N] [--list]
```

Without a configured database, `agent-resume` behaves exactly as it did before:
a local-only picker. With one, it shows both and marks where each session came
from.

## Keys

| Key | Action |
| --- | --- |
| `↑` / `k`, `↓` / `j` | Move or scroll the focused pane |
| `gg`, `G` | Jump to the start or end |
| `Ctrl+U`, `Ctrl+D` | Move half a page |
| `Ctrl+B`, `Ctrl+F`, `PgUp`, `PgDn` | Move a full page |
| `Ctrl+W`, `Tab`, `h` / `l` | Switch between sessions and details |
| `s` | Toggle sorting by last update or creation time |
| `/` | Fuzzy-filter the list; `Tab` keeps the filter for navigation |
| `f` | Find sessions by what was said in them |
| `c` | Clear the filter and the content search |
| `v` | Read the transcript of any session |
| `Enter` | Resume a local session, or read a collected one |
| `Esc` | Clear an active search; otherwise quit |
| `q`, `Ctrl+C` | Quit, or leave the transcript |

`/` filters what is on screen — machine, title, project, path, session ID. `f`
is the one for "I know what I said, not where I said it": it searches the
collected conversations themselves, keeps the sessions that matched, and shows
the matching line in the details pane. Matches in sessions outside the current
list are counted rather than hidden.

The fuzzy matcher is Unicode-aware, so mixed Russian/English prompts, paths,
and machine names all work. Content search falls back to substring matching,
which matters for inflected languages: `логов` still finds `логи`.

## How it works

- **Incremental uploads.** Each session row remembers how many bytes were
  stored. An upload reads only what came after that and stops at the last
  complete line, so a session that is still being written costs its new bytes
  and nothing more. A re-run with nothing new takes well under a second.
- **Rewrites.** A fingerprint of the first record detects a log that was
  replaced rather than appended to; the stored copy is then dropped and rebuilt
  so a transcript can never end up spliced from two different sessions.
- **Concurrency.** Uploads are transactional and check the stored byte offset,
  so a hook and a watcher running at once cannot interleave chunks.
- **Titles.** An agent-provided title wins; otherwise the first real prompt is
  used. Injected context — `AGENTS.md`, environment blocks, system reminders,
  slash-command output — is never treated as a prompt, and is not stored as a
  message.
- **Deduplication.** The picker prefers this machine's own copy of a session,
  because that is the one that can be resumed.

### Adding another agent CLI

Sessions are keyed by machine, agent, and session ID, and an agent this build
does not recognise is kept under its own name rather than dropped. To add one:
list its log directory in `discovery::sources`, teach `transcript::extract` the
shape of its records, and give it a resume command in `model::Agent`.

## Privacy

Transcripts contain prompts, source code, command output, and local paths.
Collecting them puts all of that in one database, so treat it as sensitive:
use a private database, keep TLS on, and remember that the config file holds a
password (it is written `0600`). Nothing is ever written back to a log file —
every agent-owned file is opened read-only.

## Development

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Against a local Postgres:

```bash
agent-logs --database-url 'postgres://user@127.0.0.1/agent_logs?sslmode=disable' \
  --host test-machine ship
```

## Snapshot collector

The repository also contains the original portable collector. It copies local
Claude Code, Codex, and Crush logs into a timestamped private snapshot without
credentials or agent configuration, and needs no database at all:

```bash
./collect-agent-conversations.sh DESTINATION_ROOT
./collect-agent-conversations.sh DESTINATION_ROOT --home /home/username
```

Snapshots contain source code, commands, prompts, and local paths. Keep them
private. See the script's `--help` output for its exact layout and behavior.
