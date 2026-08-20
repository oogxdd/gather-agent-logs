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
