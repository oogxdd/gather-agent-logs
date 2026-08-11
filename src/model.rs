use std::{fmt, path::PathBuf};

use chrono::{DateTime, Utc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Agent {
    Codex,
    Claude,
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }

    pub fn executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
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
    use super::clean_title;

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
}
