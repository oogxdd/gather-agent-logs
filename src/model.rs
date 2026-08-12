use std::{fmt, path::PathBuf};

use chrono::{DateTime, Utc};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortMode {
    Created,
    #[default]
    Updated,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
        }
    }

    pub fn timestamp(self, session: &Session) -> DateTime<Utc> {
        match self {
            Self::Created => session.created,
            Self::Updated => session.updated,
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::Created => Self::Updated,
            Self::Updated => Self::Created,
        }
    }
}

/// A coding agent CLI. Unknown agents are kept as-is so that a machine running
/// a newer collector can store sessions this build has never heard of.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Agent {
    Codex,
    Claude,
    Other(String),
}

impl Agent {
    pub fn from_key(key: &str) -> Self {
        match key.trim().to_ascii_lowercase().as_str() {
            "codex" => Self::Codex,
            "claude" | "claude-code" => Self::Claude,
            _ => Self::Other(key.trim().to_owned()),
        }
    }

    pub fn key(&self) -> &str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Other(key) => key,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Other(key) => key,
        }
    }

    /// The command that reopens a session of this agent, when one is known.
    pub fn resume_command(&self, session_id: &str) -> Option<(&'static str, Vec<String>)> {
        match self {
            Self::Codex => Some(("codex", vec!["resume".to_owned(), session_id.to_owned()])),
            Self::Claude => Some(("claude", vec!["--resume".to_owned(), session_id.to_owned()])),
            Self::Other(_) => None,
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Where a session record came from: this machine's log files, or the database.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
    Local,
    Remote,
}

impl Origin {
    pub fn is_local(self) -> bool {
        self == Self::Local
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub agent: Agent,
    /// Machine the session was recorded on.
    pub host: String,
    pub origin: Origin,
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    /// Log file path on the machine that produced the session.
    pub path: PathBuf,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    /// Database primary key, used to stream a stored transcript.
    pub remote_key: Option<i64>,
    pub bytes: u64,
    pub messages: u32,
}

impl Session {
    pub fn project(&self) -> String {
        self.cwd
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| self.cwd.display().to_string())
    }

    pub fn searchable_text(&self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.agent.key(),
            self.host,
            self.id,
            self.project(),
            self.cwd.display(),
            self.title
        )
    }

    /// Identity of a session across machines: the same session is never
    /// recorded twice under one host.
    pub fn dedup_key(&self) -> (String, String, String) {
        (
            self.host.clone(),
            self.agent.key().to_owned(),
            self.id.clone(),
        )
    }
}

pub fn sort_sessions(sessions: &mut [Session], mode: SortMode) {
    sessions.sort_by(|left, right| {
        mode.timestamp(right)
            .cmp(&mode.timestamp(left))
            .then_with(|| right.updated.cmp(&left.updated))
            .then_with(|| left.agent.key().cmp(right.agent.key()))
            .then_with(|| left.host.cmp(&right.host))
            .then_with(|| left.id.cmp(&right.id))
    });
}

pub fn clean_title(text: &str) -> Option<String> {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() || is_injected_context(&cleaned) {
        return None;
    }

    let mut chars = cleaned.chars();
    let title: String = chars.by_ref().take(180).collect();
    if chars.next().is_some() {
        Some(format!("{title}…"))
    } else {
        Some(title)
    }
}

/// Context that an agent injects into the conversation on the user's behalf.
/// It is never a real prompt, so it must not become a title or a stored message.
pub fn is_injected_context(text: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "# AGENTS.md instructions",
        "<environment_context>",
        "<permissions instructions>",
        "<system-reminder>",
        "<local-command-caveat>",
        "<local-command-stdout>",
        "<local-command-stderr>",
        "<command-name>",
        "<recommended_plugins>",
        "<skills_instructions>",
        "<apps_instructions>",
        "<plugins_instructions>",
        "<collaboration_mode>",
        "<multi_agent_mode>",
        "<user-prompt-submit-hook>",
        "Caveat: The messages below were generated",
    ];

    PREFIXES.iter().any(|prefix| text.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{TimeZone, Utc};

    use super::{Agent, Origin, Session, SortMode, clean_title, sort_sessions};

    pub fn test_session(agent: Agent, id: &str) -> Session {
        Session {
            agent,
            host: "laptop".to_owned(),
            origin: Origin::Local,
            id: id.to_owned(),
            title: id.to_owned(),
            cwd: PathBuf::from("/work"),
            path: PathBuf::from("session.jsonl"),
            created: Utc.timestamp_opt(1_600_000_000, 0).unwrap(),
            updated: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            remote_key: None,
            bytes: 0,
            messages: 0,
        }
    }

    #[test]
    fn title_collapses_whitespace() {
        assert_eq!(
            clean_title("  fix\n\n the   parser  ").as_deref(),
            Some("fix the parser")
        );
    }

    #[test]
    fn title_rejects_injected_context() {
        assert!(clean_title("<environment_context>cwd=/tmp").is_none());
        assert!(clean_title("# AGENTS.md instructions do things").is_none());
    }

    #[test]
    fn unknown_agents_keep_their_key_and_have_no_resume_command() {
        let agent = Agent::from_key("crush");

        assert_eq!(agent.key(), "crush");
        assert_eq!(agent.label(), "crush");
        assert!(agent.resume_command("id").is_none());
        assert_eq!(Agent::from_key("Codex"), Agent::Codex);
    }

    #[test]
    fn sessions_sort_by_created_or_updated_descending() {
        let make_session = |id: &str, created: i64, updated: i64| {
            let mut session = test_session(Agent::Codex, id);
            session.created = Utc.timestamp_opt(created, 0).unwrap();
            session.updated = Utc.timestamp_opt(updated, 0).unwrap();
            session
        };
        let original = vec![
            make_session("old-created-new-updated", 100, 400),
            make_session("new-created-old-updated", 300, 350),
        ];

        let mut by_created = original.clone();
        sort_sessions(&mut by_created, SortMode::Created);
        assert_eq!(by_created[0].id, "new-created-old-updated");

        let mut by_updated = original;
        sort_sessions(&mut by_updated, SortMode::Updated);
        assert_eq!(by_updated[0].id, "old-created-new-updated");
    }
}
