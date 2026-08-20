-- Unified local store for ChatGPT, ChatGPT Work, and Codex history.
--
-- One shape for every platform, so retrieval does not care where a message came
-- from. Imported content is verbatim: `raw` holds the original record for every
-- row, the normalised columns are only a projection of it, and nothing here
-- writes back to the sources on disk. Derived material (summaries, extracted
-- tasks, detected decisions) belongs in a separate `derived` schema and must
-- never be mixed into these tables.
--
-- Applied idempotently on every run by the importers.

CREATE SCHEMA IF NOT EXISTS hist;

-- ---------------------------------------------------------------------------
-- Sources
--
-- A source is one place data comes from: an account on a platform, on a given
-- machine. Import state lives here so a source that failed or is offline is
-- visible as such instead of silently missing from the inventory.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS hist.sources (
    id              text PRIMARY KEY,           -- 'chatgpt:99be8d70…', 'codex:hostname'
    platform        text NOT NULL,              -- chatgpt | chatgpt_work | codex
    account         text,                       -- account or workspace id
    machine         text,                       -- hostname; null for cloud sources
    label           text,
    status          text NOT NULL DEFAULT 'never',  -- ok | partial | failed | offline | never
    detail          text,
    last_attempt_at timestamptz,
    last_success_at timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Conversations and sessions
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS hist.conversations (
    id            bigserial PRIMARY KEY,
    source_id     text NOT NULL REFERENCES hist.sources(id) ON DELETE CASCADE,
    platform      text NOT NULL,
    external_id   text NOT NULL,       -- stable identifier at the source
    -- A resumed Codex session produces a second rollout file that carries the
    -- original session id. Each file stays its own row so nothing is
    -- overwritten; thread_id is what groups them back together.
    thread_id     text,
    title         text,
    created_at    timestamptz,
    updated_at    timestamptz,

    -- Where the work happened. Populated for Codex and ChatGPT Work sessions;
    -- null for cloud ChatGPT conversations, which have no local context.
    machine       text,
    project       text,                -- repository name, else the cwd basename
    repo_url      text,
    branch        text,
    git_commit    text,
    cwd           text,
    originator    text,                -- codex_work_desktop, Codex Desktop, codex_cli_rs…
    workspace_id  text,
    model         text,

    -- Import bookkeeping. `source_ref` points back at the original: a file path
    -- for Codex, a conversation URL for ChatGPT.
    source_ref    text,
    import_status text NOT NULL DEFAULT 'pending',  -- pending | ok | partial | failed
    import_error  text,
    imported_at   timestamptz,
    content_hash  text,                -- skips re-parsing unchanged rollout files
    message_count integer NOT NULL DEFAULT 0,
    raw           jsonb,               -- listing row / session_meta, verbatim

    UNIQUE (source_id, external_id)
);

-- Columns added after the table first shipped. CREATE TABLE IF NOT EXISTS does
-- nothing on an existing table, so every later column is repeated here.
ALTER TABLE hist.conversations
    ADD COLUMN IF NOT EXISTS thread_id        text,
    -- Cloud conversations are listed and fetched separately: the listing gives
    -- an updated_at for every one without opening it, so a body that was never
    -- fetched, or is older than the listing, is detectable without guessing.
    -- Local Codex sessions arrive whole and never need these.
    ADD COLUMN IF NOT EXISTS body_updated_at  timestamptz,
    ADD COLUMN IF NOT EXISTS body_imported_at timestamptz,
    ADD COLUMN IF NOT EXISTS body_started_at  timestamptz;

CREATE INDEX IF NOT EXISTS conversations_updated_idx  ON hist.conversations (updated_at DESC);
CREATE INDEX IF NOT EXISTS conversations_thread_idx   ON hist.conversations (thread_id);
CREATE INDEX IF NOT EXISTS conversations_platform_idx ON hist.conversations (platform);
CREATE INDEX IF NOT EXISTS conversations_project_idx  ON hist.conversations (project);
CREATE INDEX IF NOT EXISTS conversations_machine_idx  ON hist.conversations (machine);
CREATE INDEX IF NOT EXISTS conversations_status_idx   ON hist.conversations (import_status);
CREATE INDEX IF NOT EXISTS conversations_title_fts_idx
    ON hist.conversations USING gin (to_tsvector('simple', coalesce(title, '')));

-- ---------------------------------------------------------------------------
-- Messages
--
-- `seq` is the order within a conversation and is what "surrounding context"
-- walks. Tool calls and their output are messages too, with their own roles, so
-- a search hit inside a command's output can still be placed in the transcript.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS hist.messages (
    id              bigserial PRIMARY KEY,
    conversation_id bigint NOT NULL REFERENCES hist.conversations(id) ON DELETE CASCADE,
    seq             integer NOT NULL,
    external_id     text,
    parent_id       text,
    role            text,        -- user | assistant | reasoning | tool_call | tool_output | system | developer
    author          text,
    content_type    text,
    text            text,
    created_at      timestamptz,

    -- App-injected prompts repeat verbatim across hundreds of sessions. They are
    -- kept for fidelity but excluded from the search index below.
    searchable      boolean NOT NULL DEFAULT true,
    raw             jsonb NOT NULL,

    UNIQUE (conversation_id, seq)
);

CREATE INDEX IF NOT EXISTS messages_conversation_idx ON hist.messages (conversation_id, seq);
CREATE INDEX IF NOT EXISTS messages_created_idx      ON hist.messages (created_at);
CREATE INDEX IF NOT EXISTS messages_role_idx         ON hist.messages (role);
CREATE INDEX IF NOT EXISTS messages_fts_idx
    ON hist.messages USING gin (to_tsvector('simple', coalesce(text, '')))
    WHERE searchable;

-- ---------------------------------------------------------------------------
-- Artifacts: commands run, files patched, images generated.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS hist.artifacts (
    id              bigserial PRIMARY KEY,
    conversation_id bigint NOT NULL REFERENCES hist.conversations(id) ON DELETE CASCADE,
    message_id      bigint REFERENCES hist.messages(id) ON DELETE SET NULL,
    kind            text NOT NULL,   -- command | patch | file | image | attachment
    name            text,
    path            text,
    detail          jsonb
);

CREATE INDEX IF NOT EXISTS artifacts_conversation_idx ON hist.artifacts (conversation_id);
CREATE INDEX IF NOT EXISTS artifacts_kind_idx         ON hist.artifacts (kind);

-- ---------------------------------------------------------------------------
-- Bookkeeping
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS hist.import_runs (
    id             bigserial PRIMARY KEY,
    source_id      text REFERENCES hist.sources(id) ON DELETE CASCADE,
    started_at     timestamptz NOT NULL DEFAULT now(),
    finished_at    timestamptz,
    status         text,            -- running | ok | error
    seen           integer NOT NULL DEFAULT 0,
    imported       integer NOT NULL DEFAULT 0,
    failed         integer NOT NULL DEFAULT 0,
    messages_saved integer NOT NULL DEFAULT 0,
    error          text
);

CREATE INDEX IF NOT EXISTS import_runs_source_idx ON hist.import_runs (source_id, id DESC);

CREATE TABLE IF NOT EXISTS hist.sync_state (
    key        text PRIMARY KEY,
    value      jsonb       NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Views
-- ---------------------------------------------------------------------------
CREATE OR REPLACE VIEW hist.overview AS
SELECT c.id,
       c.source_id,
       c.platform,
       c.external_id,
       c.title,
       c.created_at,
       c.updated_at,
       c.machine,
       c.project,
       c.branch,
       c.message_count,
       c.import_status,
       c.import_error,
       c.body_imported_at AS last_sync_at,
       CASE
           WHEN c.import_status = 'failed' THEN 'failed'
           WHEN c.platform = 'codex' OR c.platform = 'chatgpt_work'
               THEN CASE WHEN c.import_status = 'ok' THEN 'synced' ELSE 'never' END
           WHEN c.body_started_at IS NOT NULL
                AND (c.body_imported_at IS NULL OR c.body_started_at > c.body_imported_at)
               THEN 'syncing'
           WHEN c.body_updated_at IS NULL                 THEN 'never'
           WHEN c.body_updated_at < c.updated_at          THEN 'stale'
           ELSE 'synced'
       END AS status,
       CASE
           WHEN c.platform <> 'chatgpt' THEN 'managed'
           WHEN b.baseline IS NULL OR c.updated_at >= b.baseline THEN 'managed'
           ELSE 'history'
       END AS scope
FROM hist.conversations c
LEFT JOIN LATERAL (
    SELECT (value ->> 'value')::timestamptz AS baseline
    FROM hist.sync_state WHERE key = 'baseline_at'
) b ON true;

-- Cloud conversations whose stored body is missing or older than the listing:
-- the "started something, walked away, never opened the answer" list.
CREATE OR REPLACE VIEW hist.pending AS
SELECT id, external_id, title, updated_at, body_updated_at,
       CASE WHEN body_updated_at IS NULL THEN 'never captured' ELSE 'stale' END AS reason
FROM hist.conversations
WHERE platform = 'chatgpt'
  AND (body_updated_at IS NULL OR body_updated_at < updated_at)
ORDER BY updated_at DESC;

-- Inventory for the "which sources have not been imported" question.
CREATE OR REPLACE VIEW hist.inventory AS
SELECT s.id,
       s.platform,
       s.account,
       s.machine,
       s.label,
       s.status,
       s.detail,
       s.last_attempt_at,
       s.last_success_at,
       count(c.id)                                              AS conversations,
       count(c.id) FILTER (WHERE c.import_status = 'ok')        AS imported,
       count(c.id) FILTER (WHERE c.import_status = 'failed')    AS failed,
       coalesce(sum(c.message_count), 0)                        AS messages,
       max(c.updated_at)                                        AS newest
FROM hist.sources s
LEFT JOIN hist.conversations c ON c.source_id = s.id
GROUP BY s.id;
