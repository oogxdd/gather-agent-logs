use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::model::{Agent, Session, clean_title};

const METADATA_LINE_LIMIT: usize = 512;

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

    sessions.sort_by(|left, right| {
        right
            .updated
            .cmp(&left.updated)
            .then_with(|| left.agent.as_str().cmp(right.agent.as_str()))
            .then_with(|| left.id.cmp(&right.id))
    });

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
    let updated = DateTime::<Utc>::from(modified);
    parse_reader(BufReader::new(file), path, updated, agent)
}

fn parse_reader(
    reader: impl BufRead,
    path: &Path,
    updated: DateTime<Utc>,
    agent: Agent,
) -> Option<Session> {
    match agent {
        Agent::Codex => parse_codex(reader, path, updated),
        Agent::Claude => parse_claude(reader, path, updated),
    }
}

fn parse_codex(reader: impl BufRead, path: &Path, updated: DateTime<Utc>) -> Option<Session> {
    let mut id = None;
    let mut cwd = None;
    let mut title = None;

    for line in reader.lines().take(METADATA_LINE_LIMIT) {
        let Ok(line) = line else { continue };
        let Ok(record) = serde_json::from_str::<Value>(line.trim_start_matches('\0')) else {
            continue;
        };

        match record.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let payload = &record["payload"];
                id = string_at(payload, "id").or_else(|| string_at(payload, "session_id"));
                cwd = string_at(payload, "cwd").map(PathBuf::from);
            }
            Some("response_item") => {
                let payload = &record["payload"];
                if payload.get("type").and_then(Value::as_str) == Some("message") {
                    match payload.get("role").and_then(Value::as_str) {
                        Some("user") if title.is_none() => {
                            title = content_title(&payload["content"]);
                        }
                        Some("assistant") if title.is_some() && id.is_some() => break,
                        _ => {}
                    }
                }
            }
            Some("event_msg") if title.is_none() => {
                let payload = &record["payload"];
                if payload.get("type").and_then(Value::as_str) == Some("user_message") {
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
        updated,
    })
}

fn parse_claude(reader: impl BufRead, path: &Path, updated: DateTime<Utc>) -> Option<Session> {
    let mut id = None;
    let mut cwd = None;
    let mut title = None;

    for line in reader.lines().take(METADATA_LINE_LIMIT) {
        let Ok(line) = line else { continue };
        let Ok(record) = serde_json::from_str::<Value>(line.trim_start_matches('\0')) else {
            continue;
        };

        id = id.or_else(|| string_at(&record, "sessionId"));
        cwd = cwd.or_else(|| string_at(&record, "cwd").map(PathBuf::from));

        match record.get("type").and_then(Value::as_str) {
            Some("custom-title") if title.is_none() => {
                title = string_at(&record, "customTitle").and_then(|text| clean_title(&text));
            }
            Some("user") if title.is_none() => {
                title = content_title(&record["message"]["content"]);
            }
            Some("assistant") if title.is_some() && id.is_some() => break,
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
        updated,
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

    fn updated() -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn parses_codex_metadata_and_skips_injected_user_context() {
        let input = concat!(
            r#"{"type":"session_meta","payload":{"id":"019ff21e-4824-70d2-8cb6-57e5b1aebefb","cwd":"/work/repo"}}"#,
            "\n",
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\ninternal"}]}}"##,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix the auth race"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}"#,
            "\n"
        );

        let session = parse_reader(
            Cursor::new(input),
            Path::new("rollout.jsonl"),
            updated(),
            Agent::Codex,
        )
        .unwrap();

        assert_eq!(session.id, "019ff21e-4824-70d2-8cb6-57e5b1aebefb");
        assert_eq!(session.cwd, Path::new("/work/repo"));
        assert_eq!(session.title, "Fix the auth race");
    }

    #[test]
    fn parses_claude_string_content() {
        let input = concat!(
            r#"{"type":"queue-operation","sessionId":"86e1a7f3-2e99-483f-b743-64e0003c58c8"}"#,
            "\n",
            r#"{"type":"user","sessionId":"86e1a7f3-2e99-483f-b743-64e0003c58c8","cwd":"/work/repo","message":{"role":"user","content":"Ship the CLI"}}"#,
            "\n"
        );

        let session = parse_reader(
            Cursor::new(input),
            Path::new("86e1a7f3-2e99-483f-b743-64e0003c58c8.jsonl"),
            updated(),
            Agent::Claude,
        )
        .unwrap();

        assert_eq!(session.id, "86e1a7f3-2e99-483f-b743-64e0003c58c8");
        assert_eq!(session.cwd, Path::new("/work/repo"));
        assert_eq!(session.title, "Ship the CLI");
    }
}
