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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Agent {
    Codex,
    Claude,
    Crush,
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Crush => "crush",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Crush => "Crush",
        }
    }

    pub fn executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Crush => "crush",
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub agent: Agent,
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub path: PathBuf,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
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
            "{} {} {} {} {}",
            self.agent.as_str(),
            self.id,
            self.project(),
            self.cwd.display(),
            self.title
        )
    }
}

pub fn sort_sessions(sessions: &mut [Session], mode: SortMode) {
    sessions.sort_by(|left, right| {
        mode.timestamp(right)
            .cmp(&mode.timestamp(left))
            .then_with(|| right.updated.cmp(&left.updated))
            .then_with(|| left.agent.as_str().cmp(right.agent.as_str()))
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

fn is_injected_context(text: &str) -> bool {
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
    ];

    PREFIXES.iter().any(|prefix| text.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{TimeZone, Utc};

    use super::{Agent, Session, SortMode, clean_title, sort_sessions};

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
    fn sessions_sort_by_created_or_updated_descending() {
        let make_session = |id: &str, created: i64, updated: i64| Session {
            agent: Agent::Codex,
            id: id.to_owned(),
            title: id.to_owned(),
            cwd: PathBuf::from("/work"),
            path: PathBuf::from("session.jsonl"),
            created: Utc.timestamp_opt(created, 0).unwrap(),
            updated: Utc.timestamp_opt(updated, 0).unwrap(),
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
