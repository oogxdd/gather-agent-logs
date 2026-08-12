use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::model::{Agent, Session, clean_title};

#[derive(Debug)]
pub struct DiscoveryOptions {
    pub codex_dir: PathBuf,
    pub claude_dir: PathBuf,
    pub include_codex: bool,
    pub include_claude: bool,
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
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH);
    let fallback_timestamp = DateTime::<Utc>::from(modified);
    parse_reader(BufReader::new(file), path, fallback_timestamp, agent)
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
    use std::{io::Cursor, path::Path};

    use chrono::{TimeZone, Utc};

    use super::{Agent, parse_reader};

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
}
