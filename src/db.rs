//! The shared Postgres store: schema, incremental uploads, and the queries the
//! picker and the CLI read sessions with.
//!
//! Transcripts are kept as gzip-compressed append-only chunks rather than one
//! row per log line. Agent logs are mostly tool traffic, so per-line rows plus
//! their indexes cost several times the raw size, while compressed chunks cost
//! about a quarter of it. Only real conversation text is indexed for search.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use postgres::{Client, NoTls, Row, types::ToSql};
use rustls::{ClientConfig, RootCertStore};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::{
    model::{Agent, Origin, Session},
    transcript::{Message, Role},
};

pub const SCHEMA_VERSION: i32 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_logs_meta (
    key   text PRIMARY KEY,
    value text NOT NULL
);

CREATE TABLE IF NOT EXISTS agent_sessions (
    id                     bigserial PRIMARY KEY,
    host                   text NOT NULL,
    agent                  text NOT NULL,
    session_id             text NOT NULL,
    cwd                    text NOT NULL DEFAULT '',
    title                  text NOT NULL DEFAULT '',
    title_is_authoritative boolean NOT NULL DEFAULT false,
    source_path            text NOT NULL DEFAULT '',
    signature              text NOT NULL DEFAULT '',
    created_at             timestamptz,
    updated_at             timestamptz,
    synced_bytes           bigint NOT NULL DEFAULT 0,
    line_count             bigint NOT NULL DEFAULT 0,
    chunk_count            integer NOT NULL DEFAULT 0,
    message_count          integer NOT NULL DEFAULT 0,
    transcript_pruned      boolean NOT NULL DEFAULT false,
    first_seen_at          timestamptz NOT NULL DEFAULT now(),
    last_sync_at           timestamptz NOT NULL DEFAULT now(),
    UNIQUE (host, agent, session_id)
);

CREATE INDEX IF NOT EXISTS agent_sessions_recent
    ON agent_sessions (updated_at DESC NULLS LAST);

CREATE TABLE IF NOT EXISTS agent_session_chunks (
    session_key bigint NOT NULL REFERENCES agent_sessions (id) ON DELETE CASCADE,
    seq         integer NOT NULL,
    byte_start  bigint NOT NULL,
    byte_end    bigint NOT NULL,
    line_start  bigint NOT NULL,
    line_count  bigint NOT NULL,
    codec       text NOT NULL DEFAULT 'gzip',
    body        bytea NOT NULL,
    stored_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (session_key, seq)
);

CREATE TABLE IF NOT EXISTS agent_messages (
    session_key bigint NOT NULL REFERENCES agent_sessions (id) ON DELETE CASCADE,
    seq         bigint NOT NULL,
    part        integer NOT NULL DEFAULT 0,
    written_at  timestamptz,
    role        text NOT NULL,
    body        text NOT NULL,
    PRIMARY KEY (session_key, seq, part)
);

CREATE INDEX IF NOT EXISTS agent_messages_search
    ON agent_messages USING gin (to_tsvector('simple', body));
"#;

/// Chunk bodies are already gzipped, so stop Postgres from compressing them a
/// second time on the way into TOAST storage.
const SCHEMA_STORAGE: &str =
    "ALTER TABLE agent_session_chunks ALTER COLUMN body SET STORAGE EXTERNAL";

pub struct Store {
    client: Client,
}

/// What the database already holds for one session, and where the next upload
/// has to continue from.
#[derive(Clone, Debug)]
pub struct SessionState {
    pub key: i64,
    pub synced_bytes: i64,
    pub line_count: i64,
    pub chunk_count: i32,
    pub message_count: i32,
    pub signature: String,
    pub title: Option<String>,
    pub title_is_authoritative: bool,
    pub cwd: Option<String>,
    pub created: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
}

/// A run of new log lines to append to a stored session.
pub struct Delta<'a> {
    pub host: &'a str,
    pub agent: &'a Agent,
    pub session_id: &'a str,
    pub source_path: &'a str,
    pub signature: &'a str,
    pub cwd: Option<&'a str>,
    pub title: Option<&'a str>,
    pub title_is_authoritative: bool,
    pub created: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
    pub byte_start: i64,
    pub byte_end: i64,
    pub line_start: i64,
    pub line_count: i64,
    pub body: &'a [u8],
    pub messages: &'a [Message],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeltaOutcome {
    Stored,
    /// Another machine or process already uploaded this range.
    Stale,
}

#[derive(Clone, Debug, Default)]
pub struct SessionFilter {
    pub host: Option<String>,
    pub agent: Option<Agent>,
    pub limit: i64,
    pub since: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct HostSummary {
    pub host: String,
    pub sessions: i64,
    pub last_activity: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct UsageRow {
    pub host: String,
    pub agent: String,
    pub sessions: i64,
    pub raw_bytes: i64,
    pub stored_bytes: i64,
    pub messages: i64,
    pub last_activity: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub session: Session,
    pub role: Role,
    pub written_at: Option<DateTime<Utc>>,
    pub snippet: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PruneReport {
    pub sessions: u64,
    pub chunks: u64,
}

impl Store {
    /// Connects with TLS when the server offers it. Managed Postgres providers
    /// require it; a local development server usually has it switched off.
    pub fn connect(url: &str) -> Result<Self> {
        let client = if wants_tls(url) {
            Client::connect(url, tls_connector()?)
        } else {
            Client::connect(url, NoTls)
        }
        .context("could not connect to the agent-logs database")?;
        Ok(Self { client })
    }

    pub fn init_schema(&mut self) -> Result<()> {
        self.client
            .batch_execute(SCHEMA)
            .context("could not create the agent-logs schema")?;
        // Ignored when the table was created by an older build that lacks the
        // privilege to change storage settings.
        let _ = self.client.batch_execute(SCHEMA_STORAGE);
        self.client.execute(
            "INSERT INTO agent_logs_meta (key, value) VALUES ('schema_version', $1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            &[&SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    pub fn schema_version(&mut self) -> Result<Option<i32>> {
        let row = self
            .client
            .query_opt(
                "SELECT value FROM agent_logs_meta WHERE key = 'schema_version'",
                &[],
            )
            .context("could not read the schema version; run `agent-logs setup` first")?;
        Ok(row.and_then(|row| row.get::<_, String>(0).parse().ok()))
    }

    /// Creates the session row if it is new and returns where to resume from.
    pub fn session_state(
        &mut self,
        host: &str,
        agent: &Agent,
        session_id: &str,
    ) -> Result<SessionState> {
        self.client.execute(
            "INSERT INTO agent_sessions (host, agent, session_id)
             VALUES ($1, $2, $3)
             ON CONFLICT (host, agent, session_id) DO NOTHING",
            &[&host, &agent.key(), &session_id],
        )?;
        let row = self.client.query_one(
            "SELECT id, synced_bytes, line_count, chunk_count, message_count, signature,
                    title, title_is_authoritative, cwd, created_at, updated_at
             FROM agent_sessions
             WHERE host = $1 AND agent = $2 AND session_id = $3",
            &[&host, &agent.key(), &session_id],
        )?;
        Ok(session_state_from_row(&row))
    }

    /// Forgets everything stored for a session. Used when a log file was
    /// truncated or replaced, which would otherwise leave a corrupt transcript.
    pub fn reset_session(&mut self, key: i64) -> Result<()> {
        let mut transaction = self.client.transaction()?;
        transaction.execute(
            "DELETE FROM agent_session_chunks WHERE session_key = $1",
            &[&key],
        )?;
        transaction.execute("DELETE FROM agent_messages WHERE session_key = $1", &[&key])?;
        transaction.execute(
            "UPDATE agent_sessions
             SET synced_bytes = 0, line_count = 0, chunk_count = 0, message_count = 0,
                 transcript_pruned = false
             WHERE id = $1",
            &[&key],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn store_delta(&mut self, delta: &Delta<'_>) -> Result<DeltaOutcome> {
        let mut transaction = self.client.transaction()?;
        let row = transaction.query_one(
            "SELECT id, synced_bytes, line_count, chunk_count, message_count, title,
                    title_is_authoritative
             FROM agent_sessions
             WHERE host = $1 AND agent = $2 AND session_id = $3
             FOR UPDATE",
            &[&delta.host, &delta.agent.key(), &delta.session_id],
        )?;
        let key: i64 = row.get(0);
        let synced_bytes: i64 = row.get(1);
        let line_count: i64 = row.get(2);
        let chunk_count: i32 = row.get(3);
        let message_count: i32 = row.get(4);
        let stored_title: String = row.get(5);
        let title_is_authoritative: bool = row.get(6);

        // Another process may have shipped this range while the file was read.
        if synced_bytes != delta.byte_start {
            transaction.rollback()?;
            return Ok(DeltaOutcome::Stale);
        }

        transaction.execute(
            "INSERT INTO agent_session_chunks
                 (session_key, seq, byte_start, byte_end, line_start, line_count, codec, body)
             VALUES ($1, $2, $3, $4, $5, $6, 'gzip', $7)",
            &[
                &key,
                &chunk_count,
                &delta.byte_start,
                &delta.byte_end,
                &delta.line_start,
                &delta.line_count,
                &delta.body,
            ],
        )?;

        let statement = transaction.prepare(
            "INSERT INTO agent_messages (session_key, seq, part, written_at, role, body)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (session_key, seq, part) DO NOTHING",
        )?;
        let mut part_of_line = (-1_i64, -1_i32);
        for message in delta.messages {
            part_of_line = if part_of_line.0 == message.seq {
                (message.seq, part_of_line.1 + 1)
            } else {
                (message.seq, 0)
            };
            transaction.execute(
                &statement,
                &[
                    &key,
                    &message.seq,
                    &part_of_line.1,
                    &message.timestamp,
                    &message.role.key(),
                    &message.text,
                ],
            )?;
        }

        let keep_stored_title = title_is_authoritative && !delta.title_is_authoritative
            || (!stored_title.is_empty() && delta.title.is_none());
        let title = if keep_stored_title { None } else { delta.title };

        transaction.execute(
            "UPDATE agent_sessions
             SET synced_bytes = $2,
                 line_count = $3,
                 chunk_count = $4,
                 message_count = $5,
                 source_path = $6,
                 signature = $7,
                 cwd = CASE WHEN $8::text IS NULL OR $8 = '' THEN cwd ELSE $8 END,
                 title = coalesce($9, title),
                 title_is_authoritative = title_is_authoritative OR $10,
                 created_at = coalesce(created_at, $11),
                 updated_at = greatest(updated_at, $12),
                 last_sync_at = now()
             WHERE id = $1",
            &[
                &key,
                &delta.byte_end,
                &(line_count + delta.line_count),
                &(chunk_count + 1),
                &(message_count + delta.messages.len() as i32),
                &delta.source_path,
                &delta.signature,
                &delta.cwd,
                &title,
                &delta.title_is_authoritative,
                &delta.created,
                &delta.updated,
            ],
        )?;

        transaction.commit()?;
        Ok(DeltaOutcome::Stored)
    }

    pub fn sessions(&mut self, filter: &SessionFilter) -> Result<Vec<Session>> {
        let agent = filter.agent.as_ref().map(|agent| agent.key().to_owned());
        let limit = if filter.limit > 0 {
            filter.limit
        } else {
            10_000
        };
        let rows = self.client.query(
            "SELECT id, host, agent, session_id, title, cwd, source_path, created_at, updated_at,
                    synced_bytes, message_count
             FROM agent_sessions
             WHERE ($1::text IS NULL OR host = $1)
               AND ($2::text IS NULL OR agent = $2)
               AND ($3::timestamptz IS NULL OR updated_at >= $3)
             ORDER BY updated_at DESC NULLS LAST
             LIMIT $4",
            &[&filter.host, &agent, &filter.since, &limit],
        )?;
        Ok(rows.iter().map(session_from_row).collect())
    }

    pub fn session_by_id(&mut self, session_id: &str, host: Option<&str>) -> Result<Vec<Session>> {
        let pattern = format!("{session_id}%");
        let rows = self.client.query(
            "SELECT id, host, agent, session_id, title, cwd, source_path, created_at, updated_at,
                    synced_bytes, message_count
             FROM agent_sessions
             WHERE (session_id = $1 OR session_id LIKE $2)
               AND ($3::text IS NULL OR host = $3)
             ORDER BY updated_at DESC NULLS LAST",
            &[&session_id, &pattern, &host],
        )?;
        Ok(rows.iter().map(session_from_row).collect())
    }

    /// Rebuilds a stored transcript by decompressing its chunks in order.
    pub fn transcript(&mut self, key: i64) -> Result<Vec<u8>> {
        use std::io::Read;

        let rows = self.client.query(
            "SELECT codec, body FROM agent_session_chunks WHERE session_key = $1 ORDER BY seq",
            &[&key],
        )?;

        let mut jsonl = Vec::new();
        for row in &rows {
            let codec: String = row.get(0);
            let body: Vec<u8> = row.get(1);
            match codec.as_str() {
                "gzip" => {
                    let mut decoder = flate2::read::GzDecoder::new(body.as_slice());
                    decoder
                        .read_to_end(&mut jsonl)
                        .context("stored transcript chunk could not be decompressed")?;
                }
                "none" => jsonl.extend_from_slice(&body),
                other => bail!("unknown transcript codec: {other}"),
            }
        }
        Ok(jsonl)
    }

    pub fn hosts(&mut self) -> Result<Vec<HostSummary>> {
        let rows = self.client.query(
            "SELECT host, count(*)::bigint, max(updated_at)
             FROM agent_sessions
             GROUP BY host
             ORDER BY max(updated_at) DESC NULLS LAST",
            &[],
        )?;
        Ok(rows
            .iter()
            .map(|row| HostSummary {
                host: row.get(0),
                sessions: row.get(1),
                last_activity: row.get(2),
            })
            .collect())
    }

    pub fn usage(&mut self) -> Result<Vec<UsageRow>> {
        let rows = self.client.query(
            "SELECT s.host,
                    s.agent,
                    count(*)::bigint,
                    coalesce(sum(s.synced_bytes), 0)::bigint,
                    coalesce(sum(c.stored), 0)::bigint,
                    coalesce(sum(s.message_count), 0)::bigint,
                    max(s.updated_at)
             FROM agent_sessions s
             LEFT JOIN (
                 SELECT session_key, sum(octet_length(body))::bigint AS stored
                 FROM agent_session_chunks
                 GROUP BY session_key
             ) c ON c.session_key = s.id
             GROUP BY s.host, s.agent
             ORDER BY s.host, s.agent",
            &[],
        )?;
        Ok(rows
            .iter()
            .map(|row| UsageRow {
                host: row.get(0),
                agent: row.get(1),
                sessions: row.get(2),
                raw_bytes: row.get(3),
                stored_bytes: row.get(4),
                messages: row.get(5),
                last_activity: row.get(6),
            })
            .collect())
    }

    /// Full-text search over the conversation, newest match first.
    pub fn search(&mut self, query: &str, filter: &SessionFilter) -> Result<Vec<SearchHit>> {
        let agent = filter.agent.as_ref().map(|agent| agent.key().to_owned());
        let limit = if filter.limit > 0 { filter.limit } else { 50 };
        let rows = self.client.query(
            "SELECT s.id, s.host, s.agent, s.session_id, s.title, s.cwd, s.source_path,
                    s.created_at, s.updated_at, s.synced_bytes, s.message_count,
                    m.role, m.written_at,
                    ts_headline('simple', m.body, websearch_to_tsquery('simple', $1),
                                'MaxFragments=1,MaxWords=24,MinWords=8,StartSel=«,StopSel=»')
             FROM agent_messages m
             JOIN agent_sessions s ON s.id = m.session_key
             WHERE to_tsvector('simple', m.body) @@ websearch_to_tsquery('simple', $1)
               AND ($2::text IS NULL OR s.host = $2)
               AND ($3::text IS NULL OR s.agent = $3)
             ORDER BY m.written_at DESC NULLS LAST
             LIMIT $4",
            &[&query, &filter.host, &agent, &limit],
        )?;
        Ok(rows
            .iter()
            .map(|row| SearchHit {
                session: session_from_row(row),
                role: Role::from_key(row.get::<_, String>(11).as_str()),
                written_at: row.get(12),
                snippet: row
                    .get::<_, String>(13)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            })
            .collect())
    }

    /// Drops old data. Transcripts alone can go, which keeps sessions findable
    /// at a fraction of the storage.
    pub fn prune(
        &mut self,
        before: DateTime<Utc>,
        host: Option<&str>,
        transcripts_only: bool,
    ) -> Result<PruneReport> {
        let mut report = PruneReport::default();
        if transcripts_only {
            report.chunks = self.client.execute(
                "DELETE FROM agent_session_chunks c
                 USING agent_sessions s
                 WHERE c.session_key = s.id
                   AND s.updated_at < $1
                   AND ($2::text IS NULL OR s.host = $2)",
                &[&before, &host],
            )?;
            self.client.execute(
                "UPDATE agent_sessions
                 SET transcript_pruned = true, chunk_count = 0
                 WHERE updated_at < $1 AND ($2::text IS NULL OR host = $2) AND chunk_count > 0",
                &[&before, &host],
            )?;
        } else {
            report.sessions = self.client.execute(
                "DELETE FROM agent_sessions
                 WHERE updated_at < $1 AND ($2::text IS NULL OR host = $2)",
                &[&before, &host],
            )?;
        }
        Ok(report)
    }

    /// Escape hatch for ad-hoc maintenance from the CLI.
    pub fn execute(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
        Ok(self.client.execute(sql, params)?)
    }
}

fn session_state_from_row(row: &Row) -> SessionState {
    let title: String = row.get(6);
    let cwd: String = row.get(8);
    SessionState {
        key: row.get(0),
        synced_bytes: row.get(1),
        line_count: row.get(2),
        chunk_count: row.get(3),
        message_count: row.get(4),
        signature: row.get(5),
        title: (!title.is_empty()).then_some(title),
        title_is_authoritative: row.get(7),
        cwd: (!cwd.is_empty()).then_some(cwd),
        created: row.get(9),
        updated: row.get(10),
    }
}

fn session_from_row(row: &Row) -> Session {
    let created: Option<DateTime<Utc>> = row.get(7);
    let updated: Option<DateTime<Utc>> = row.get(8);
    let fallback = created.or(updated).unwrap_or_else(Utc::now);
    let agent = Agent::from_key(row.get::<_, String>(2).as_str());
    let title: String = row.get(4);
    Session {
        remote_key: Some(row.get(0)),
        host: row.get(1),
        origin: Origin::Remote,
        id: row.get(3),
        title: if title.is_empty() {
            format!("Untitled {} session", agent.label())
        } else {
            title
        },
        cwd: row.get::<_, String>(5).into(),
        path: row.get::<_, String>(6).into(),
        created: created.unwrap_or(fallback),
        updated: updated.unwrap_or(fallback),
        bytes: row.get::<_, i64>(9).max(0) as u64,
        messages: row.get::<_, i32>(10).max(0) as u32,
        agent,
    }
}

/// Managed providers need TLS; `sslmode=disable` opts out for local servers.
fn wants_tls(url: &str) -> bool {
    let lowercase = url.to_ascii_lowercase();
    !lowercase.contains("sslmode=disable")
}

fn tls_connector() -> Result<MakeRustlsConnect> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .context("could not set up TLS")?
            .with_root_certificates(roots)
            .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

#[cfg(test)]
mod tests {
    use super::wants_tls;

    #[test]
    fn tls_is_used_unless_explicitly_disabled() {
        assert!(wants_tls(
            "postgresql://user:pw@db.neon.tech/agent?sslmode=require"
        ));
        assert!(wants_tls("postgres://user@example.com/agent"));
        assert!(!wants_tls(
            "postgres://sprite@localhost/agent_logs?sslmode=disable"
        ));
    }
}
