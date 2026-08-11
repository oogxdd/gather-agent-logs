mod discovery;
mod model;
mod ui;

use std::{
    env,
    io::{self, IsTerminal},
    path::PathBuf,
    process::{Command, ExitCode},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use discovery::{DiscoveryOptions, discover};
use model::{Agent, Session};

#[derive(Debug, Parser)]
#[command(
    name = "agent-resume",
    version,
    about = "Find and resume local Codex and Claude Code sessions"
)]
struct Cli {
    /// Only show sessions from one agent.
    #[arg(long, value_enum, default_value_t = AgentFilter::All)]
    agent: AgentFilter,

    /// Use HOME_DIR instead of the current user's home directory.
    #[arg(long, value_name = "HOME_DIR")]
    home: Option<PathBuf>,

    /// Read Codex JSONL files directly from this directory.
    #[arg(long, value_name = "SESSIONS_DIR")]
    codex_dir: Option<PathBuf>,

    /// Read Claude Code JSONL files directly from this directory.
    #[arg(long, value_name = "PROJECTS_DIR")]
    claude_dir: Option<PathBuf>,

    /// Print the discovered sessions instead of opening the terminal UI.
    #[arg(long)]
    list: bool,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum AgentFilter {
    #[default]
    All,
    Codex,
    Claude,
}

impl AgentFilter {
    fn includes(self, agent: Agent) -> bool {
        match self {
            Self::All => true,
            Self::Codex => agent == Agent::Codex,
            Self::Claude => agent == Agent::Claude,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8> {
    let cli = Cli::parse();
    let home_was_explicit = cli.home.is_some();
    let home = cli.home.or_else(|| env::var_os("HOME").map(PathBuf::from));

    let codex_dir = match cli.codex_dir {
        Some(directory) => directory,
        None if !home_was_explicit => env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .map(|directory| directory.join("sessions"))
            .unwrap_or_else(|| default_session_dir(&home, ".codex/sessions")),
        None => default_session_dir(&home, ".codex/sessions"),
    };
    let claude_dir = match cli.claude_dir {
        Some(directory) => directory,
        None if !home_was_explicit => env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .map(|directory| directory.join("projects"))
            .unwrap_or_else(|| default_session_dir(&home, ".claude/projects")),
        None => default_session_dir(&home, ".claude/projects"),
    };

    let discovered = discover(&DiscoveryOptions {
        codex_dir,
        claude_dir,
        include_codex: cli.agent.includes(Agent::Codex),
        include_claude: cli.agent.includes(Agent::Claude),
    });

    if cli.list {
        print_sessions(&discovered.sessions);
        if discovered.skipped > 0 {
            eprintln!(
                "warning: skipped {} unreadable or unrecognized session file(s)",
                discovered.skipped
            );
        }
        return Ok(0);
    }

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("the picker needs an interactive terminal; use --list for plain output");
    }
    if discovered.sessions.is_empty() {
        bail!("no Codex or Claude Code sessions found");
    }

    let selection = ui::pick(discovered.sessions, discovered.skipped)?;
    let Some(session) = selection else {
        return Ok(0);
    };

    resume(&session)
}

fn default_session_dir(home: &Option<PathBuf>, relative: &str) -> PathBuf {
    home.as_ref()
        .map(|directory| directory.join(relative))
        .unwrap_or_default()
}

fn print_sessions(sessions: &[Session]) {
    for session in sessions {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            session.agent.as_str(),
            session.updated.to_rfc3339(),
            session.id,
            session.cwd.display(),
            session.title
        );
    }
}

fn resume(session: &Session) -> Result<u8> {
    let mut command = resume_command(session)?;
    let executable = session.agent.executable();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let error = command.exec();
        Err(error).with_context(|| {
            format!("could not launch {executable}; make sure it is installed and on PATH")
        })
    }

    #[cfg(not(unix))]
    {
        let status = command.status().with_context(|| {
            format!("could not launch {executable}; make sure it is installed and on PATH")
        })?;

        Ok(status.code().unwrap_or(1).clamp(0, u8::MAX as i32) as u8)
    }
}

fn resume_command(session: &Session) -> Result<Command> {
    let mut command = match session.agent {
        Agent::Codex => {
            let mut command = Command::new("codex");
            command.arg("resume").arg(&session.id);
            command
        }
        Agent::Claude => {
            let mut command = Command::new("claude");
            command.arg("--resume").arg(&session.id);
            command
        }
    };

    if !session.cwd.as_os_str().is_empty() && !session.cwd.is_dir() {
        bail!(
            "saved working directory no longer exists: {}",
            session.cwd.display()
        );
    }
    if !session.cwd.as_os_str().is_empty() {
        command.current_dir(&session.cwd);
    }

    Ok(command)
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, path::PathBuf};

    use chrono::Utc;

    use super::resume_command;
    use crate::model::{Agent, Session};

    fn session(agent: Agent) -> Session {
        Session {
            agent,
            id: "019ff21e-4824-70d2-8cb6-57e5b1aebefb".to_owned(),
            title: "Test session".to_owned(),
            cwd: std::env::current_dir().unwrap(),
            path: PathBuf::from("session.jsonl"),
            updated: Utc::now(),
        }
    }

    #[test]
    fn codex_resume_uses_native_command_and_saved_directory() {
        let session = session(Agent::Codex);
        let command = resume_command(&session).unwrap();

        assert_eq!(command.get_program(), OsStr::new("codex"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("resume"),
                OsStr::new("019ff21e-4824-70d2-8cb6-57e5b1aebefb")
            ]
        );
        assert_eq!(command.get_current_dir(), Some(session.cwd.as_path()));
    }

    #[test]
    fn claude_resume_uses_native_command_and_saved_directory() {
        let session = session(Agent::Claude);
        let command = resume_command(&session).unwrap();

        assert_eq!(command.get_program(), OsStr::new("claude"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("--resume"),
                OsStr::new("019ff21e-4824-70d2-8cb6-57e5b1aebefb")
            ]
        );
        assert_eq!(command.get_current_dir(), Some(session.cwd.as_path()));
    }
}
