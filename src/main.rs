//! `agent-resume`: a picker over this machine's agent sessions and the ones
//! collected from every other machine.

use std::{
    collections::HashSet,
    io::{self, IsTerminal},
    path::PathBuf,
    process::{Command, ExitCode},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};

use agent_logs::{
    config::Config,
    db::{SessionFilter, Store},
    discovery::{self, SourceOptions},
    model::{Agent, Origin, Session, SortMode, sort_sessions},
    ui::{self, Backend, SearchMatch},
};

#[derive(Debug, Parser)]
#[command(
    name = "agent-resume",
    version,
    about = "Find and resume Codex and Claude Code sessions, local or collected"
)]
struct Cli {
    /// Which machines' sessions to show.
    #[arg(long, value_enum, value_name = "SOURCE")]
    source: Option<SourceArg>,

    /// Only show sessions from one agent, for example codex or claude.
    #[arg(long, value_name = "AGENT", default_value = "all")]
    agent: String,

    /// Only show collected sessions from this machine.
    #[arg(long, value_name = "NAME")]
    from: Option<String>,

    /// Initial sort order; press s in the picker to toggle it.
    #[arg(long, value_enum, default_value_t = SortArg::Updated)]
    sort: SortArg,

    /// Use HOME_DIR instead of the current user's home directory.
    #[arg(long, value_name = "HOME_DIR")]
    home: Option<PathBuf>,

    /// Read Codex JSONL files directly from this directory.
    #[arg(long, value_name = "SESSIONS_DIR")]
    codex_dir: Option<PathBuf>,

    /// Read Claude Code JSONL files directly from this directory.
    #[arg(long, value_name = "PROJECTS_DIR")]
    claude_dir: Option<PathBuf>,

    /// Postgres connection string for collected sessions.
    #[arg(long, value_name = "URL")]
    database_url: Option<String>,

    /// Name this machine's own sessions are shown under.
    #[arg(long, value_name = "NAME")]
    host: Option<String>,

    /// Most collected sessions to load.
    #[arg(long, default_value_t = 2000)]
    limit: i64,

    /// Print the sessions instead of opening the terminal UI.
    #[arg(long)]
    list: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum SourceArg {
    /// Only this machine's log files.
    Local,
    /// Only what the database has collected.
    Remote,
    /// Both, preferring the local copy of a session.
    All,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum SortArg {
    Created,
    #[default]
    Updated,
}

impl From<SortArg> for SortMode {
    fn from(value: SortArg) -> Self {
        match value {
            SortArg::Created => Self::Created,
            SortArg::Updated => Self::Updated,
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
    let mut config = Config::load()?;
    if let Some(url) = &cli.database_url {
        config.database_url = Some(url.clone());
    }
    if let Some(host) = &cli.host {
        config.host = host.clone();
    }

    let agent_filter = match cli.agent.trim() {
        "all" | "" => None,
        key => Some(Agent::from_key(key)),
    };
    // Without a database there is nothing to collect from, so stay local.
    let source = cli.source.unwrap_or(if config.database_url.is_some() {
        SourceArg::All
    } else {
        SourceArg::Local
    });
    if source != SourceArg::Local && config.database_url.is_none() {
        bail!(
            "no database configured; run `agent-logs setup --database-url postgres://…` or pass --source local"
        );
    }

    let mut sessions = Vec::new();
    let mut skipped = 0;

    if source != SourceArg::Remote {
        let sources = discovery::sources(&SourceOptions {
            home_was_explicit: cli.home.is_some(),
            home: cli.home.clone(),
            codex_dir: cli.codex_dir.clone(),
            claude_dir: cli.claude_dir.clone(),
        });
        let discovered = discovery::discover(&sources, &config.host);
        skipped = discovered.skipped;
        sessions.extend(discovered.sessions);
    }

    // An unreachable database must not cost you the local picker: an offline
    // laptop still has its own sessions, and they are the resumable ones.
    let mut store = match source {
        SourceArg::Local => None,
        SourceArg::Remote => Some(Store::connect(config.database_url()?)?),
        SourceArg::All => match Store::connect(config.database_url()?) {
            Ok(store) => Some(store),
            Err(error) => {
                eprintln!("warning: showing local sessions only: {error:#}");
                None
            }
        },
    };

    if let Some(store) = store.as_mut() {
        let collected = store.sessions(&SessionFilter {
            host: cli.from.clone(),
            agent: agent_filter.clone(),
            limit: cli.limit,
            since: None,
        })?;
        // The local copy of a session is the one that can be resumed.
        let local: HashSet<_> = sessions.iter().map(|session| session.dedup_key()).collect();
        sessions.extend(
            collected
                .into_iter()
                .filter(|session| !local.contains(&session.dedup_key())),
        );
    }

    if let Some(agent) = &agent_filter {
        sessions.retain(|session| session.agent == *agent);
    }
    if let Some(host) = &cli.from {
        sessions.retain(|session| session.host == *host);
    }

    let sort_mode = SortMode::from(cli.sort);
    sort_sessions(&mut sessions, sort_mode);

    if cli.list {
        print_sessions(&sessions);
        if skipped > 0 {
            eprintln!("warning: skipped {skipped} unreadable or unrecognized session file(s)");
        }
        return Ok(0);
    }

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("the picker needs an interactive terminal; use --list for plain output");
    }
    if sessions.is_empty() {
        bail!("no sessions found");
    }

    let mut library = Library {
        store,
        search_limit: cli.limit,
    };
    let selection = ui::pick(sessions, skipped, sort_mode, &mut library)?;
    let Some(session) = selection else {
        return Ok(0);
    };

    resume(&session)
}

/// Serves the picker: transcripts from disk or the database, and full-text
/// search across everything collected.
struct Library {
    store: Option<Store>,
    search_limit: i64,
}

impl Backend for Library {
    fn transcript(&mut self, session: &Session) -> Result<Vec<u8>> {
        if session.origin.is_local() {
            return std::fs::read(&session.path)
                .with_context(|| format!("could not read {}", session.path.display()));
        }
        let key = session
            .remote_key
            .context("collected session is missing its database key")?;
        let store = self
            .store
            .as_mut()
            .context("no database connection for collected sessions")?;
        let jsonl = store.transcript(key)?;
        if jsonl.is_empty() {
            bail!("no transcript stored for this session; it may have been pruned");
        }
        Ok(jsonl)
    }

    fn search_messages(&mut self, query: &str) -> Result<Vec<SearchMatch>> {
        let store = self.store.as_mut().context(
            "searching inside conversations needs the collected database; none is configured",
        )?;
        let hits = store.search(
            query,
            &SessionFilter {
                limit: self.search_limit,
                ..SessionFilter::default()
            },
        )?;
        Ok(hits
            .into_iter()
            .map(|hit| SearchMatch {
                host: hit.session.host,
                agent: hit.session.agent.key().to_owned(),
                session_id: hit.session.id,
                snippet: hit.snippet,
            })
            .collect())
    }
}

fn print_sessions(sessions: &[Session]) {
    for session in sessions {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            session.host,
            session.agent.key(),
            if session.origin.is_local() {
                "local"
            } else {
                "collected"
            },
            session.created.to_rfc3339(),
            session.updated.to_rfc3339(),
            session.id,
            session.cwd.display(),
            session.title
        );
    }
}

fn resume(session: &Session) -> Result<u8> {
    let mut command = resume_command(session)?;
    let executable = command.get_program().to_string_lossy().into_owned();

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
    if session.origin == Origin::Remote {
        bail!(
            "this session was recorded on {}; open it there, or press v to read the transcript",
            session.host
        );
    }
    let Some((executable, arguments)) = session.agent.resume_command(&session.id) else {
        bail!(
            "{} sessions cannot be resumed by this tool; press v to read the transcript",
            session.agent.label()
        );
    };

    let mut command = Command::new(executable);
    command.args(arguments);

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

    use agent_logs::model::{Agent, Origin, Session};
    use chrono::Utc;

    use super::resume_command;

    fn session(agent: Agent) -> Session {
        Session {
            agent,
            host: "laptop".to_owned(),
            origin: Origin::Local,
            id: "019ff21e-4824-70d2-8cb6-57e5b1aebefb".to_owned(),
            title: "Test session".to_owned(),
            cwd: std::env::current_dir().unwrap(),
            path: PathBuf::from("session.jsonl"),
            created: Utc::now(),
            updated: Utc::now(),
            remote_key: None,
            bytes: 0,
            messages: 0,
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
    }

    #[test]
    fn collected_sessions_from_another_machine_are_not_resumed() {
        let mut session = session(Agent::Claude);
        session.origin = Origin::Remote;
        session.host = "sandbox-7".to_owned();

        let error = resume_command(&session).unwrap_err().to_string();

        assert!(error.contains("sandbox-7"), "{error}");
    }
}
