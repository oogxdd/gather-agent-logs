-- Postgres schema for locally synced ChatGPT conversations.
-- Applied idempotently by chatgpt_sync.py on every run.

CREATE SCHEMA IF NOT EXISTS chatgpt;

-- One row per conversation. `index_raw` is the listing payload, `body_raw` the
-- full conversation payload. `body_update_time` records which version of the
-- conversation the stored body belongs to, so staleness is a plain comparison
-- against `update_time`.
CREATE TABLE IF NOT EXISTS chatgpt.conversations (
    id                  uuid PRIMARY KEY,
    title               text,
    create_time         timestamptz,
    update_time         timestamptz NOT NULL,
    is_archived         boolean,
    is_starred          boolean,
    is_temporary_chat   boolean,
    workspace_id        text,
    conversation_origin text,
    gizmo_id            text,
    async_status        text,
    current_node        text,
    index_raw           jsonb       NOT NULL,
    first_seen_at       timestamptz NOT NULL DEFAULT now(),
    index_synced_at     timestamptz NOT NULL DEFAULT now(),
    body_update_time    timestamptz,
    body_synced_at      timestamptz,
    body_raw            jsonb
);

CREATE INDEX IF NOT EXISTS conversations_update_time_idx
    ON chatgpt.conversations (update_time DESC);
CREATE INDEX IF NOT EXISTS conversations_pending_idx
    ON chatgpt.conversations (update_time DESC)
    WHERE body_update_time IS NULL OR body_update_time < update_time;

-- Messages exploded out of the conversation `mapping`. The mapping is a tree,
-- so `parent_id`/`children` are kept as-is rather than flattened to a list.
-- Node ids are text: the synthetic root node is not always a uuid.
CREATE TABLE IF NOT EXISTS chatgpt.messages (
    conversation_id uuid  NOT NULL REFERENCES chatgpt.conversations(id) ON DELETE CASCADE,
    id              text  NOT NULL,
    parent_id       text,
    children        text[],
    role            text,
    author_name     text,
    recipient       text,
    content_type    text,
    text            text,
    model_slug      text,
    status          text,
    end_turn        boolean,
    weight          double precision,
    create_time     timestamptz,
    raw             jsonb NOT NULL,
    PRIMARY KEY (conversation_id, id)
);

CREATE INDEX IF NOT EXISTS messages_conversation_time_idx
    ON chatgpt.messages (conversation_id, create_time);
CREATE INDEX IF NOT EXISTS messages_role_idx
    ON chatgpt.messages (role);
-- 'simple' rather than a language config: conversations mix languages.
CREATE INDEX IF NOT EXISTS messages_text_fts_idx
    ON chatgpt.messages USING gin (to_tsvector('simple', coalesce(text, '')));

-- Small key/value table for watermarks (notably the baseline timestamp that
-- separates "history we deliberately skipped" from "captured from now on").
CREATE TABLE IF NOT EXISTS chatgpt.sync_state (
    key        text PRIMARY KEY,
    value      jsonb       NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS chatgpt.sync_runs (
    id             bigserial PRIMARY KEY,
    started_at     timestamptz NOT NULL DEFAULT now(),
    finished_at    timestamptz,
    status         text,
    index_seen     integer NOT NULL DEFAULT 0,
    index_changed  integer NOT NULL DEFAULT 0,
    bodies_synced  integer NOT NULL DEFAULT 0,
    messages_saved integer NOT NULL DEFAULT 0,
    error          text
);

-- Set just before a body fetch and left in place, so a UI can tell "being
-- checked right now" from "queued": in flight means it is newer than the last
-- completed sync for that row.
ALTER TABLE chatgpt.conversations
    ADD COLUMN IF NOT EXISTS body_sync_started_at timestamptz;

-- Per-conversation state for the tray app. `scope` separates the conversations
-- the daemon actually manages from the history deliberately skipped at the
-- baseline, so the UI does not show hundreds of false "not synced" rows.
CREATE OR REPLACE VIEW chatgpt.overview AS
SELECT c.id,
       c.title,
       c.create_time,
       c.update_time,
       c.body_update_time,
       c.body_synced_at,
       CASE
           WHEN c.body_sync_started_at IS NOT NULL
                AND (c.body_synced_at IS NULL OR c.body_sync_started_at > c.body_synced_at)
               THEN 'syncing'
           WHEN c.body_update_time IS NULL          THEN 'never'
           WHEN c.body_update_time < c.update_time  THEN 'stale'
           ELSE 'synced'
       END AS status,
       CASE
           WHEN b.baseline IS NULL OR c.update_time >= b.baseline THEN 'managed'
           ELSE 'history'
       END AS scope,
       (SELECT count(*) FROM chatgpt.messages m WHERE m.conversation_id = c.id) AS message_count
FROM chatgpt.conversations c
LEFT JOIN LATERAL (
    SELECT (value ->> 'value')::timestamptz AS baseline
    FROM chatgpt.sync_state
    WHERE key = 'baseline_at'
) b ON true;

-- Conversations whose stored body is missing or older than the server's
-- version. This is the "I started something, walked away, and never opened the
-- answer" list.
CREATE OR REPLACE VIEW chatgpt.pending AS
SELECT id,
       title,
       update_time,
       body_update_time,
       CASE WHEN body_update_time IS NULL THEN 'never captured'
            ELSE 'stale' END AS reason
FROM chatgpt.conversations
WHERE body_update_time IS NULL OR body_update_time < update_time
ORDER BY update_time DESC;
