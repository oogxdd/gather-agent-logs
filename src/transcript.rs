//! Turning agent JSONL records into session metadata, searchable messages, and
//! a readable transcript. Every agent writes a different shape of record, so
//! this is the one place that knows about those shapes.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::model::{Agent, clean_title, is_injected_context};

/// Longest message text kept for search. Full text always stays in the stored
/// transcript; this only bounds the searchable copy.
pub const MAX_STORED_TEXT: usize = 4000;

const MAX_DETAIL_TEXT: usize = 2000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    User,
    Assistant,
    Reasoning,
    Tool,
}

impl Role {
    pub fn key(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Reasoning => "reasoning",
            Self::Tool => "tool",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "user" => Self::User,
            "reasoning" => Self::Reasoning,
            "tool" => Self::Tool,
            _ => Self::Assistant,
        }
    }

    /// Whether this role is worth keeping in the searchable message table.
    /// Reasoning and tool traffic is the bulk of a log and the least useful to
    /// search, so it lives only in the compressed transcript.
    pub fn is_conversation(self) -> bool {
        matches!(self, Self::User | Self::Assistant)
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub seq: i64,
    pub timestamp: Option<DateTime<Utc>>,
    pub role: Role,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct SessionMeta {
    pub id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    pub created: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
    /// Set by an agent-provided title, which outranks the first user prompt.
    pub title_is_authoritative: bool,
}

impl SessionMeta {
    fn set_title(&mut self, title: Option<String>, authoritative: bool) {
        if title.is_none() {
            return;
        }
        if self.title.is_none() || (authoritative && !self.title_is_authoritative) {
            self.title = title;
            self.title_is_authoritative = authoritative;
        }
    }
}

/// Reads a session's records in order, accumulating metadata and emitting the
/// messages worth indexing. It can start from metadata recovered from the
/// database, which is what makes incremental uploads possible.
pub struct SessionScanner {
    agent: Agent,
    meta: SessionMeta,
}

impl SessionScanner {
    pub fn new(agent: &Agent) -> Self {
        Self::with_meta(agent, SessionMeta::default())
    }

    pub fn with_meta(agent: &Agent, meta: SessionMeta) -> Self {
        Self {
            agent: agent.clone(),
            meta,
        }
    }

    pub fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    pub fn into_meta(self) -> SessionMeta {
        self.meta
    }

    /// Feeds one JSONL line. Returns the messages worth storing for search.
    pub fn push(&mut self, seq: i64, line: &str) -> Vec<Message> {
        let Some(record) = parse_line(line) else {
            return Vec::new();
        };

        let timestamp = timestamp_at(&record, "timestamp");
        if self.meta.created.is_none() {
            self.meta.created = timestamp;
        }
        self.absorb_meta(&record, timestamp);

        let mut messages = extract(&self.agent, &record, false);
        for message in &mut messages {
            message.seq = seq;
            message.timestamp = message.timestamp.or(timestamp);
            truncate_in_place(&mut message.text, MAX_STORED_TEXT);
            if message.role.is_conversation() {
                self.meta.updated = message.timestamp.or(self.meta.updated);
            }
        }

        if let Some(first) = messages.iter().find(|message| message.role == Role::User) {
            self.meta.set_title(clean_title(&first.text), false);
        }

        messages.retain(|message| message.role.is_conversation() && !message.text.is_empty());
        messages
    }

    fn absorb_meta(&mut self, record: &Value, timestamp: Option<DateTime<Utc>>) {
        match self.agent {
            Agent::Codex => {
                if record.get("type").and_then(Value::as_str) == Some("session_meta") {
                    let payload = &record["payload"];
                    self.meta.id = self
                        .meta
                        .id
                        .take()
                        .or_else(|| string_at(payload, "id"))
                        .or_else(|| string_at(payload, "session_id"));
                    self.meta.cwd = self
                        .meta
                        .cwd
                        .take()
                        .or_else(|| string_at(payload, "cwd").map(PathBuf::from));
                    self.meta.created = timestamp_at(payload, "timestamp").or(self.meta.created);
                }
                // Older Codex builds only recorded activity as UI events.
                if record.get("type").and_then(Value::as_str) == Some("event_msg") {
                    let payload = &record["payload"];
                    let kind = payload.get("type").and_then(Value::as_str);
                    if matches!(kind, Some("user_message" | "agent_message")) {
                        self.meta.updated = timestamp.or(self.meta.updated);
                    }
                }
            }
            Agent::Claude => {
                if self.meta.id.is_none() {
                    self.meta.id = string_at(record, "sessionId");
                }
                if self.meta.cwd.is_none() {
                    self.meta.cwd = string_at(record, "cwd").map(PathBuf::from);
                }
                match record.get("type").and_then(Value::as_str) {
                    Some("custom-title") => {
                        let title = string_at(record, "customTitle")
                            .as_deref()
                            .and_then(clean_title);
                        self.meta.set_title(title, true);
                    }
                    Some("ai-title") => {
                        let title = string_at(record, "aiTitle")
                            .as_deref()
                            .and_then(clean_title);
                        self.meta.set_title(title, true);
                    }
                    _ => {}
                }
            }
            Agent::Other(_) => {
                if self.meta.id.is_none() {
                    self.meta.id = string_at(record, "sessionId")
                        .or_else(|| string_at(record, "session_id"))
                        .or_else(|| string_at(record, "id"));
                }
                if self.meta.cwd.is_none() {
                    self.meta.cwd = string_at(record, "cwd").map(PathBuf::from);
                }
            }
        }
    }
}

/// Renders a stored transcript for reading: conversation plus a summary of the
/// reasoning and tool traffic between the messages.
pub fn read_transcript(agent: &Agent, jsonl: &[u8]) -> Vec<Message> {
    let mut entries = Vec::new();
    for (index, line) in jsonl.split(|byte| *byte == b'\n').enumerate() {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let Some(record) = parse_line(line) else {
            continue;
        };
        let timestamp = timestamp_at(&record, "timestamp");
        for mut message in extract(agent, &record, true) {
            message.seq = index as i64;
            message.timestamp = message.timestamp.or(timestamp);
            if message.text.is_empty() {
                continue;
            }
            entries.push(message);
        }
    }
    entries
}

fn extract(agent: &Agent, record: &Value, detail: bool) -> Vec<Message> {
    match agent {
        Agent::Codex => extract_codex(record, detail),
        Agent::Claude => extract_claude(record, detail),
        Agent::Other(_) => extract_generic(record),
    }
}

fn extract_codex(record: &Value, detail: bool) -> Vec<Message> {
    // Codex writes each turn twice: as `response_item` conversation records and
    // as `event_msg` UI events. Only the former is read, so nothing is stored
    // or displayed twice.
    if record.get("type").and_then(Value::as_str) != Some("response_item") {
        return Vec::new();
    }

    let payload = &record["payload"];
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            let role = match payload.get("role").and_then(Value::as_str) {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                _ => return Vec::new(),
            };
            let text = content_text(&payload["content"]);
            if role == Role::User && is_injected_context(text.trim()) {
                return Vec::new();
            }
            message(role, text)
        }
        Some("reasoning") if detail => {
            let mut text = content_text(&payload["summary"]);
            if text.trim().is_empty() {
                text = content_text(&payload["content"]);
            }
            message(Role::Reasoning, truncated(&text, MAX_DETAIL_TEXT))
        }
        Some("function_call") if detail => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let arguments = payload
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            message(
                Role::Tool,
                format!("{name} {}", truncated(arguments, MAX_DETAIL_TEXT)),
            )
        }
        Some("custom_tool_call") if detail => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let input = payload.get("input").and_then(Value::as_str).unwrap_or("");
            message(
                Role::Tool,
                format!("{name} {}", truncated(input, MAX_DETAIL_TEXT)),
            )
        }
        Some("function_call_output" | "custom_tool_call_output") if detail => {
            let output = payload
                .get("output")
                .map(value_text)
                .unwrap_or_else(|| value_text(&payload["result"]));
            message(Role::Tool, format!("→ {}", truncated(&output, 600)))
        }
        _ => Vec::new(),
    }
}

fn extract_claude(record: &Value, detail: bool) -> Vec<Message> {
    let kind = record.get("type").and_then(Value::as_str);
    if !matches!(kind, Some("user" | "assistant")) {
        return Vec::new();
    }

    let content = &record["message"]["content"];
    if let Some(text) = content.as_str() {
        if is_injected_context(text.trim()) {
            return Vec::new();
        }
        return message(Role::User, text.to_owned());
    }

    let Some(parts) = content.as_array() else {
        return Vec::new();
    };

    let mut messages = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                if kind == Some("user") && is_injected_context(text.trim()) {
                    continue;
                }
                let role = if kind == Some("user") {
                    Role::User
                } else {
                    Role::Assistant
                };
                messages.extend(message(role, text.to_owned()));
            }
            Some("thinking") if detail => {
                let text = part
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                messages.extend(message(Role::Reasoning, truncated(text, MAX_DETAIL_TEXT)));
            }
            Some("tool_use") if detail => {
                let name = part.get("name").and_then(Value::as_str).unwrap_or("tool");
                let input = part.get("input").map(value_text).unwrap_or_default();
                messages.extend(message(
                    Role::Tool,
                    format!("{name} {}", truncated(&input, MAX_DETAIL_TEXT)),
                ));
            }
            Some("tool_result") if detail => {
                let output = part.get("content").map(value_text).unwrap_or_default();
                messages.extend(message(
                    Role::Tool,
                    format!("→ {}", truncated(&output, 600)),
                ));
            }
            _ => {}
        }
    }
    messages
}

/// Best-effort reading of an agent this build does not know: keep anything that
/// looks like a chat message so unknown logs are still searchable.
fn extract_generic(record: &Value) -> Vec<Message> {
    let role = record
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| record["message"].get("role").and_then(Value::as_str))
        .or_else(|| record.get("type").and_then(Value::as_str));
    let role = match role {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return Vec::new(),
    };

    let content = if record["message"].get("content").is_some() {
        &record["message"]["content"]
    } else {
        &record["content"]
    };
    let text = content_text(content);
    if role == Role::User && is_injected_context(text.trim()) {
        return Vec::new();
    }
    message(role, text)
}

fn message(role: Role, text: String) -> Vec<Message> {
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Vec::new();
    }
    vec![Message {
        seq: 0,
        timestamp: None,
        role,
        text,
    }]
}

fn parse_line(line: &str) -> Option<Value> {
    let line = line.trim_start_matches('\0').trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

/// Flattens the several content shapes the agents use into plain text.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let mut collected: Vec<String> = Vec::new();
            for part in parts {
                match part {
                    Value::String(text) => collected.push(text.clone()),
                    Value::Object(_) => {
                        let text = part
                            .get("text")
                            .and_then(Value::as_str)
                            .or_else(|| part.get("summary_text").and_then(Value::as_str));
                        if let Some(text) = text {
                            collected.push(text.to_owned());
                        }
                    }
                    _ => {}
                }
            }
            collected.join("\n")
        }
        _ => String::new(),
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Array(_) => content_text(value),
        other => other.to_string(),
    }
}

fn truncated(text: &str, limit: usize) -> String {
    let mut text = text.trim().to_owned();
    truncate_in_place(&mut text, limit);
    text
}

fn truncate_in_place(text: &mut String, limit: usize) {
    if text.chars().count() <= limit {
        return;
    }
    let cut = text
        .char_indices()
        .nth(limit)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.truncate(cut);
    text.push('…');
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

pub fn timestamp_at(value: &Value, key: &str) -> Option<DateTime<Utc>> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::{Agent, Role, SessionScanner, read_transcript};

    #[test]
    fn codex_scanner_collects_metadata_and_conversation() {
        let lines = [
            r#"{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{"id":"019ff21e-4824-70d2-8cb6-57e5b1aebefb","cwd":"/work/repo","timestamp":"2026-01-01T10:00:00Z"}}"#,
            r#"{"timestamp":"2026-01-01T10:01:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd"}]}}"#,
            r#"{"timestamp":"2026-01-01T10:02:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix the auth race"}]}}"#,
            r#"{"timestamp":"2026-01-01T10:03:00Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"thinking"}]}}"#,
            r#"{"timestamp":"2026-01-01T10:04:00Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done"}]}}"#,
            r#"{"timestamp":"2026-01-01T10:05:00Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"Done"}]}}}"#,
        ];

        let mut scanner = SessionScanner::new(&Agent::Codex);
        let messages: Vec<_> = lines
            .iter()
            .enumerate()
            .flat_map(|(index, line)| scanner.push(index as i64, line))
            .collect();

        assert_eq!(
            messages.len(),
            2,
            "reasoning and duplicated events are not stored"
        );
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].text, "Fix the auth race");
        assert_eq!(messages[1].role, Role::Assistant);

        let meta = scanner.into_meta();
        assert_eq!(
            meta.id.as_deref(),
            Some("019ff21e-4824-70d2-8cb6-57e5b1aebefb")
        );
        assert_eq!(meta.title.as_deref(), Some("Fix the auth race"));
        assert_eq!(meta.cwd.unwrap().to_str(), Some("/work/repo"));
        assert_eq!(
            meta.updated.unwrap().to_rfc3339(),
            "2026-01-01T10:04:00+00:00"
        );
    }

    #[test]
    fn claude_agent_title_outranks_the_first_prompt() {
        let lines = [
            r#"{"type":"user","timestamp":"2026-02-01T10:01:00Z","sessionId":"abc","cwd":"/work/repo","message":{"role":"user","content":"Ship the CLI"}}"#,
            r#"{"type":"ai-title","timestamp":"2026-02-01T10:02:00Z","aiTitle":"Release the packaging CLI"}"#,
        ];

        let mut scanner = SessionScanner::new(&Agent::Claude);
        for (index, line) in lines.iter().enumerate() {
            scanner.push(index as i64, line);
        }

        let meta = scanner.into_meta();
        assert_eq!(meta.title.as_deref(), Some("Release the packaging CLI"));
        assert_eq!(meta.id.as_deref(), Some("abc"));
    }

    #[test]
    fn detailed_reading_keeps_reasoning_and_tools() {
        let jsonl = concat!(
            r#"{"type":"assistant","timestamp":"2026-02-01T10:01:00Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"weighing options"},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-02-01T10:02:00Z","message":{"role":"user","content":[{"type":"tool_result","content":"src"}]}}"#,
            "\n"
        );

        let entries = read_transcript(&Agent::Claude, jsonl.as_bytes());

        let roles: Vec<_> = entries.iter().map(|entry| entry.role).collect();
        assert_eq!(roles, vec![Role::Reasoning, Role::Tool, Role::Tool]);
        assert!(entries[1].text.starts_with("Bash"));
        assert!(entries[2].text.starts_with("→ src"));
    }
}
