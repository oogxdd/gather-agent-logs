use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::model::{Agent, Session, clean_title};

#[derive(Debug)]
pub struct DiscoveryOptions {
    pub codex_dir: PathBuf,
    pub claude_dir: PathBuf,
    pub crush_dir: PathBuf,
    pub include_codex: bool,
    pub include_claude: bool,
    pub include_crush: bool,
}

#[derive(Debug)]
pub struct DiscoveryResult {
    pub sessions: Vec<Session>,
    pub skipped: usize,
}

pub fn discover(options: &DiscoveryOptions) -> DiscoveryResult {
    let mut sessions = Vec::new();
    let mut skipped = 0;

    if options.include_codex {
        let mut files = jsonl_files(&options.codex_dir, false, &mut skipped);
        files.sort_unstable();
        for path in files {
            match parse_session_file(&path, Agent::Codex) {
                Some(session) => sessions.push(session),
                None => skipped += 1,
            }
        }
    }

    if options.include_claude {
        let mut files = jsonl_files(&options.claude_dir, true, &mut skipped);
        files.sort_unstable();
        for path in files {
            match parse_session_file(&path, Agent::Claude) {
                Some(session) => sessions.push(session),
                None => skipped += 1,
            }
        }
    }

    if options.include_crush {
        for project in crush_projects(&options.crush_dir) {
            match read_crush_database(&project.database, &project.cwd) {
                Some(found) => sessions.extend(found),
                None => skipped += 1,
            }
        }
    }

    DiscoveryResult { sessions, skipped }
}

fn jsonl_files(root: &Path, skip_subagents: bool, skipped: &mut usize) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }

    let mut files = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                *skipped += 1;
                continue;
            }
        };

        for entry in entries {
            let Ok(entry) = entry else {
                *skipped += 1;
                continue;
            };
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                *skipped += 1;
                continue;
            };

            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if skip_subagents && entry.file_name() == "subagents" {
                    continue;
                }
                directories.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
            {
                if skip_subagents
                    && path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .is_some_and(|stem| stem.starts_with("agent-"))
                {
                    continue;
                }
                files.push(path);
            }
        }
    }
    files
}

fn parse_session_file(path: &Path, agent: Agent) -> Option<Session> {
    let file = File::open(path).ok()?;
    parse_reader(BufReader::new(file), path, file_timestamp(path), agent)
}

fn file_timestamp(path: &Path) -> DateTime<Utc> {
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH);
    DateTime::<Utc>::from(modified)
}

fn parse_reader(
    reader: impl BufRead,
    path: &Path,
    fallback_timestamp: DateTime<Utc>,
    agent: Agent,
) -> Option<Session> {
    match agent {
        Agent::Codex => parse_codex(reader, path, fallback_timestamp),
        Agent::Claude => parse_claude(reader, path, fallback_timestamp),
        // Crush keeps sessions in SQLite databases, not JSONL transcripts.
        Agent::Crush => None,
    }
}

fn parse_codex(
    reader: impl BufRead,
    path: &Path,
    fallback_timestamp: DateTime<Utc>,
) -> Option<Session> {
    let mut id = None;
    let mut cwd = None;
    let mut title = None;
    let mut created = None;
    let mut updated = None;

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let Ok(record) = serde_json::from_str::<Value>(line.trim_start_matches('\0')) else {
            continue;
        };
        let timestamp = timestamp_at(&record, "timestamp");
        created = created.or(timestamp);

        match record.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let payload = &record["payload"];
                id = string_at(payload, "id").or_else(|| string_at(payload, "session_id"));
                cwd = string_at(payload, "cwd").map(PathBuf::from);
                created = timestamp_at(payload, "timestamp").or(created);
            }
            Some("response_item") => {
                let payload = &record["payload"];
                if payload.get("type").and_then(Value::as_str) == Some("message") {
                    let role = payload.get("role").and_then(Value::as_str);
                    if matches!(role, Some("user" | "assistant")) {
                        updated = timestamp.or(updated);
                    }
                    match role {
                        Some("user") if title.is_none() => {
                            title = content_title(&payload["content"]);
                        }
                        _ => {}
                    }
                }
            }
            Some("event_msg") => {
                let payload = &record["payload"];
                let message_type = payload.get("type").and_then(Value::as_str);
                if matches!(message_type, Some("user_message" | "agent_message")) {
                    updated = timestamp.or(updated);
                }
                if title.is_none() && message_type == Some("user_message") {
                    title = payload
                        .get("message")
                        .and_then(Value::as_str)
                        .and_then(clean_title);
                }
            }
            _ => {}
        }
    }

    let id = id.or_else(|| id_from_filename(path))?;
    Some(Session {
        agent: Agent::Codex,
        id,
        title: title.unwrap_or_else(|| "Untitled Codex session".to_owned()),
        cwd: cwd.unwrap_or_default(),
        path: path.to_path_buf(),
        created: created.unwrap_or(fallback_timestamp),
        updated: updated.or(created).unwrap_or(fallback_timestamp),
    })
}

fn parse_claude(
    reader: impl BufRead,
    path: &Path,
    fallback_timestamp: DateTime<Utc>,
) -> Option<Session> {
    let mut id = None;
    let mut cwd = None;
    let mut title = None;
    let mut created = None;
    let mut updated = None;

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let Ok(record) = serde_json::from_str::<Value>(line.trim_start_matches('\0')) else {
            continue;
        };
        let timestamp = timestamp_at(&record, "timestamp");
        created = created.or(timestamp);

        id = id.or_else(|| string_at(&record, "sessionId"));
        cwd = cwd.or_else(|| string_at(&record, "cwd").map(PathBuf::from));

        match record.get("type").and_then(Value::as_str) {
            Some("custom-title") if title.is_none() => {
                title = string_at(&record, "customTitle").and_then(|text| clean_title(&text));
            }
            Some("user") if title.is_none() => {
                updated = timestamp.or(updated);
                title = content_title(&record["message"]["content"]);
            }
            Some("user" | "assistant") => updated = timestamp.or(updated),
            _ => {}
        }
    }

    let id = id.or_else(|| id_from_filename(path))?;
    Some(Session {
        agent: Agent::Claude,
        id,
        title: title.unwrap_or_else(|| "Untitled Claude Code session".to_owned()),
        cwd: cwd.unwrap_or_default(),
        path: path.to_path_buf(),
        created: created.unwrap_or(fallback_timestamp),
        updated: updated.or(created).unwrap_or(fallback_timestamp),
    })
}

#[derive(Debug, Eq, PartialEq)]
struct CrushProject {
    database: PathBuf,
    cwd: PathBuf,
}

/// Crush stores one SQLite database per project and lists every project it has
/// opened in `projects.json` inside its data directory.
fn crush_projects(root: &Path) -> Vec<CrushProject> {
    if let Ok(registry) = fs::read_to_string(root.join("projects.json")) {
        let mut projects = parse_crush_registry(&registry);
        projects.retain(|project| project.database.is_file());
        if !projects.is_empty() {
            return projects;
        }
    }

    // Allow the directory to point straight at a single project's data
    // directory, such as ~/.crush or a copy taken from another machine.
    let database = root.join("crush.db");
    if database.is_file() {
        return vec![CrushProject {
            database,
            cwd: crush_project_cwd(root),
        }];
    }

    Vec::new()
}

fn parse_crush_registry(registry: &str) -> Vec<CrushProject> {
    let Ok(registry) = serde_json::from_str::<Value>(registry) else {
        return Vec::new();
    };
    let Some(entries) = registry.get("projects").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut projects: Vec<CrushProject> = Vec::new();
    for entry in entries {
        let Some(cwd) = string_at(entry, "path").map(PathBuf::from) else {
            continue;
        };
        let data_dir = string_at(entry, "data_dir")
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.join(".crush"));
        let database = data_dir.join("crush.db");
        if projects.iter().any(|project| project.database == database) {
            continue;
        }
        projects.push(CrushProject { database, cwd });
    }
    projects
}

fn crush_project_cwd(data_dir: &Path) -> PathBuf {
    if data_dir.file_name().is_some_and(|name| name == ".crush") {
        data_dir.parent().unwrap_or(Path::new("")).to_path_buf()
    } else {
        PathBuf::new()
    }
}

fn read_crush_database(database: &Path, cwd: &Path) -> Option<Vec<Session>> {
    let connection = open_crush_database(database)?;
    let mut statement = connection
        .prepare(
            "SELECT id, title, created_at, updated_at
             FROM sessions
             WHERE parent_session_id IS NULL OR parent_session_id = ''
             ORDER BY id",
        )
        .ok()?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })
        .ok()?
        .flatten()
        .collect::<Vec<_>>();
    drop(statement);

    let fallback_timestamp = file_timestamp(database);
    let sessions = rows
        .into_iter()
        .filter(|(id, ..)| !id.is_empty())
        .map(|(id, title, created, updated)| {
            let created = created.and_then(epoch_timestamp);
            let updated = updated.and_then(epoch_timestamp);
            let title = clean_title(&title)
                .or_else(|| crush_first_prompt(&connection, &id))
                .unwrap_or_else(|| "Untitled Crush session".to_owned());
            Session {
                agent: Agent::Crush,
                id,
                title,
                cwd: cwd.to_path_buf(),
                path: database.to_path_buf(),
                created: created.or(updated).unwrap_or(fallback_timestamp),
                updated: updated.or(created).unwrap_or(fallback_timestamp),
            }
        })
        .collect();
    Some(sessions)
}

fn open_crush_database(database: &Path) -> Option<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if let Some(connection) = Connection::open_with_flags(database, flags)
        .ok()
        .filter(crush_database_is_readable)
    {
        return Some(connection);
    }

    // A database left in write-ahead mode needs its shared-memory index, which
    // is unavailable on read-only media. Reading the main file alone still
    // shows every session that has been checkpointed.
    let uri = format!("file:{}?immutable=1", uri_path(database)?);
    Connection::open_with_flags(uri, flags | OpenFlags::SQLITE_OPEN_URI)
        .ok()
        .filter(crush_database_is_readable)
}

fn crush_database_is_readable(connection: &Connection) -> bool {
    connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'sessions'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .is_ok_and(|tables| tables > 0)
}

fn uri_path(path: &Path) -> Option<String> {
    let path = path.to_str()?;
    Some(
        path.replace('%', "%25")
            .replace('?', "%3f")
            .replace('#', "%23"),
    )
}

fn crush_first_prompt(connection: &Connection, session_id: &str) -> Option<String> {
    let mut statement = connection
        .prepare(
            "SELECT parts FROM messages
             WHERE session_id = ?1 AND role = 'user'
             ORDER BY created_at, id
             LIMIT 8",
        )
        .ok()?;
    let parts = statement
        .query_map([session_id], |row| row.get::<_, Option<String>>(0))
        .ok()?
        .flatten()
        .flatten()
        .collect::<Vec<_>>();

    parts.iter().find_map(|parts| crush_parts_title(parts))
}

fn crush_parts_title(parts: &str) -> Option<String> {
    let Ok(parts) = serde_json::from_str::<Value>(parts) else {
        return None;
    };

    parts.as_array()?.iter().find_map(|part| {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            return None;
        }
        part.get("data")
            .and_then(|data| data.get("text"))
            .or_else(|| part.get("text"))
            .and_then(Value::as_str)
            .and_then(clean_title)
    })
}

/// Crush has written these columns as seconds and as milliseconds.
fn epoch_timestamp(value: i64) -> Option<DateTime<Utc>> {
    const MILLISECONDS_FROM: i64 = 100_000_000_000;

    if value <= 0 {
        return None;
    }
    let (seconds, nanoseconds) = if value >= MILLISECONDS_FROM {
        (value / 1000, (value % 1000) as u32 * 1_000_000)
    } else {
        (value, 0)
    };
    Utc.timestamp_opt(seconds, nanoseconds).single()
}

fn content_title(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => clean_title(text),
        Value::Array(parts) => parts.iter().find_map(|part| {
            let kind = part.get("type").and_then(Value::as_str);
            if !matches!(kind, Some("text" | "input_text")) {
                return None;
            }
            part.get("text")
                .and_then(Value::as_str)
                .and_then(clean_title)
        }),
        _ => None,
    }
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn timestamp_at(value: &Value, key: &str) -> Option<DateTime<Utc>> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let candidate = stem.get(stem.len().saturating_sub(36)..)?;
    let looks_like_uuid = candidate.len() == 36
        && candidate
            .chars()
            .enumerate()
            .all(|(index, character)| match index {
                8 | 13 | 18 | 23 => character == '-',
                _ => character.is_ascii_hexdigit(),
            });
    looks_like_uuid.then(|| candidate.to_owned())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Cursor,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use chrono::{TimeZone, Utc};
    use rusqlite::Connection;

    use super::{
        Agent, CrushProject, DiscoveryOptions, crush_parts_title, discover, epoch_timestamp,
        parse_crush_registry, parse_reader,
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);

            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agent-resume-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::remove_dir_all(&path).ok();
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn fallback_timestamp() -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    fn timestamp(value: &str) -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn parses_codex_metadata_and_skips_injected_user_context() {
        let input = concat!(
            r#"{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{"id":"019ff21e-4824-70d2-8cb6-57e5b1aebefb","cwd":"/work/repo","timestamp":"2026-01-01T10:00:00Z"}}"#,
            "\n",
            r##"{"timestamp":"2026-01-01T10:01:00Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"internal"}]}}"##,
            "\n",
            r##"{"timestamp":"2026-01-01T10:02:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\ninternal"}]}}"##,
            "\n",
            r#"{"timestamp":"2026-01-01T10:03:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix the auth race"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T10:04:00Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T10:05:00Z","type":"response_item","payload":{"type":"custom_tool_call"}}"#,
            "\n"
        );

        let session = parse_reader(
            Cursor::new(input),
            Path::new("rollout.jsonl"),
            fallback_timestamp(),
            Agent::Codex,
        )
        .unwrap();

        assert_eq!(session.id, "019ff21e-4824-70d2-8cb6-57e5b1aebefb");
        assert_eq!(session.cwd, Path::new("/work/repo"));
        assert_eq!(session.title, "Fix the auth race");
        assert_eq!(session.created, timestamp("2026-01-01T10:00:00Z"));
        assert_eq!(session.updated, timestamp("2026-01-01T10:04:00Z"));
    }

    #[test]
    fn parses_claude_string_content() {
        let input = concat!(
            r#"{"type":"queue-operation","timestamp":"2026-02-01T10:00:00Z","sessionId":"86e1a7f3-2e99-483f-b743-64e0003c58c8"}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-02-01T10:01:00Z","sessionId":"86e1a7f3-2e99-483f-b743-64e0003c58c8","cwd":"/work/repo","message":{"role":"user","content":"Ship the CLI"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-02-01T10:03:00Z","sessionId":"86e1a7f3-2e99-483f-b743-64e0003c58c8","cwd":"/work/repo","message":{"role":"assistant","content":"Done"}}"#,
            "\n",
            r#"{"type":"last-prompt","timestamp":"2026-02-01T10:05:00Z","sessionId":"86e1a7f3-2e99-483f-b743-64e0003c58c8"}"#,
            "\n"
        );

        let session = parse_reader(
            Cursor::new(input),
            Path::new("86e1a7f3-2e99-483f-b743-64e0003c58c8.jsonl"),
            fallback_timestamp(),
            Agent::Claude,
        )
        .unwrap();

        assert_eq!(session.id, "86e1a7f3-2e99-483f-b743-64e0003c58c8");
        assert_eq!(session.cwd, Path::new("/work/repo"));
        assert_eq!(session.title, "Ship the CLI");
        assert_eq!(session.created, timestamp("2026-02-01T10:00:00Z"));
        assert_eq!(session.updated, timestamp("2026-02-01T10:03:00Z"));
    }

    #[test]
    fn crush_registry_resolves_databases_and_ignores_repeats() {
        let registry = r#"{"projects":[
            {"path":"/work/backend","data_dir":"/work/backend/.crush"},
            {"path":"/work/frontend"},
            {"path":"/work/backend","data_dir":"/work/backend/.crush"},
            {"data_dir":"/orphan/.crush"}
        ]}"#;

        assert_eq!(
            parse_crush_registry(registry),
            [
                CrushProject {
                    database: PathBuf::from("/work/backend/.crush/crush.db"),
                    cwd: PathBuf::from("/work/backend"),
                },
                CrushProject {
                    database: PathBuf::from("/work/frontend/.crush/crush.db"),
                    cwd: PathBuf::from("/work/frontend"),
                },
            ]
        );
        assert!(parse_crush_registry("not json").is_empty());
    }

    #[test]
    fn crush_message_parts_yield_a_title() {
        assert_eq!(
            crush_parts_title(r#"[{"type":"reasoning","data":{"thinking":"hmm"}},{"type":"text","data":{"text":"Fix the parser"}}]"#).as_deref(),
            Some("Fix the parser")
        );
        assert_eq!(
            crush_parts_title(r#"[{"type":"text","text":"Older shape"}]"#).as_deref(),
            Some("Older shape")
        );
        assert!(crush_parts_title(r#"[{"type":"tool_call"}]"#).is_none());
    }

    #[test]
    fn crush_timestamps_accept_seconds_and_milliseconds() {
        let expected = timestamp("2026-08-13T16:27:03Z");

        assert_eq!(epoch_timestamp(1_786_638_423), Some(expected));
        assert_eq!(epoch_timestamp(1_786_638_423_000), Some(expected));
        assert_eq!(epoch_timestamp(0), None);
    }

    #[test]
    fn crush_databases_provide_sessions_titles_and_project_paths() {
        let root = TempDir::new("crush");
        let project = root.path().join("work/repo");
        let data_dir = project.join(".crush");
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(
            root.path().join("projects.json"),
            format!(
                r#"{{"projects":[{{"path":{:?},"data_dir":{:?}}},{{"path":"/gone","data_dir":"/gone/.crush"}}]}}"#,
                project.display().to_string(),
                data_dir.display().to_string()
            ),
        )
        .unwrap();

        let database = data_dir.join("crush.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    parent_session_id TEXT,
                    title TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE messages (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    parts TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO sessions VALUES
                    ('parent', NULL, 'Improve the picker', 1786638423, 1786641643),
                    ('untitled', '', '', 1786638000, 1786638100),
                    ('child$$call_1', 'parent', 'Subagent work', 1786638433, 1786638610);
                INSERT INTO messages VALUES
                    ('m1', 'untitled', 'assistant', '[{"type":"text","data":{"text":"Working"}}]', 1786638000),
                    ('m2', 'untitled', 'user', '[{"type":"text","data":{"text":"Ship the CLI"}}]', 1786638050);
                "#,
            )
            .unwrap();
        drop(connection);

        let discovered = discover(&DiscoveryOptions {
            codex_dir: PathBuf::new(),
            claude_dir: PathBuf::new(),
            crush_dir: root.path().to_path_buf(),
            include_codex: false,
            include_claude: false,
            include_crush: true,
        });

        let titles = discovered
            .sessions
            .iter()
            .map(|session| (session.id.as_str(), session.title.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            titles,
            [
                ("parent", "Improve the picker"),
                ("untitled", "Ship the CLI")
            ]
        );

        let session = &discovered.sessions[0];
        assert_eq!(session.agent, Agent::Crush);
        assert_eq!(session.cwd, project);
        assert_eq!(session.path, database);
        assert_eq!(session.created, timestamp("2026-08-13T16:27:03Z"));
        assert_eq!(session.updated, timestamp("2026-08-13T17:20:43Z"));
        assert_eq!(discovered.skipped, 0);
    }

    #[test]
    fn crush_directory_can_point_at_a_single_project_database() {
        let root = TempDir::new("crush-single");
        let data_dir = root.path().join("project/.crush");
        fs::create_dir_all(&data_dir).unwrap();

        let database = data_dir.join("crush.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    parent_session_id TEXT,
                    title TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                INSERT INTO sessions VALUES ('solo', NULL, 'Copied database', 1786638423, 1786641643);
                "#,
            )
            .unwrap();
        drop(connection);

        let discovered = discover(&DiscoveryOptions {
            codex_dir: PathBuf::new(),
            claude_dir: PathBuf::new(),
            crush_dir: data_dir,
            include_codex: false,
            include_claude: false,
            include_crush: true,
        });

        assert_eq!(discovered.sessions.len(), 1);
        assert_eq!(discovered.sessions[0].id, "solo");
        assert_eq!(discovered.sessions[0].cwd, root.path().join("project"));
    }
}
