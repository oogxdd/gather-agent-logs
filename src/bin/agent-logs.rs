//! `agent-logs`: collect this machine's coding-agent sessions into the shared
//! database, and read what every machine has collected.

use std::{
    io::{self, Read, Write},
    path::PathBuf,
    process::ExitCode,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};

use agent_logs::{
    config::{self, Config},
    db::{HOOK_CONNECT_TIMEOUT, SessionFilter, Store},
    discovery::{self, SourceOptions},
    hooks,
    model::{Agent, Session},
    sync::{self, SyncOptions, human_bytes},
    transcript::{Role, read_transcript},
};

#[derive(Debug, Parser)]
#[command(
    name = "agent-logs",
    version,
    about = "Collect Codex and Claude Code sessions from every machine into one database"
)]
struct Cli {
    /// Postgres connection string; overrides the stored configuration.
    #[arg(long, global = true, value_name = "URL")]
    database_url: Option<String>,

    /// Name this machine's sessions are stored under.
    #[arg(long, global = true, value_name = "NAME")]
    host: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Store the database URL and create the schema.
    Setup {
        #[arg(long, value_name = "URL")]
        database_url: String,
        /// Name to store this machine's sessions under.
        #[arg(long, value_name = "NAME")]
        machine: Option<String>,
        /// Only write the configuration, without touching the database.
        #[arg(long)]
        skip_schema: bool,
    },
    /// Create or update the database schema.
    Init,
    /// Upload everything new from this machine's agent logs.
    Ship {
        #[command(flatten)]
        sources: SourceArgs,
        /// Upload a single log file instead of scanning.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        #[arg(long)]
        quiet: bool,
    },
    /// Keep uploading on an interval, for agents without a usable hook.
    Watch {
        #[command(flatten)]
        sources: SourceArgs,
        /// Seconds between uploads.
        #[arg(long, default_value_t = 60, value_name = "SECONDS")]
        interval: u64,
    },
    /// Upload from inside an agent hook. Never fails the agent.
    Hook {
        #[arg(value_enum)]
        agent: HookAgent,
        #[command(flatten)]
        sources: SourceArgs,
        /// Hook payload, when the agent passes it as an argument.
        #[arg(value_name = "PAYLOAD")]
        payload: Option<String>,
    },
    /// Add the upload hooks to the agents installed on this machine.
    InstallHooks {
        /// Only install for these agents (default: all that are configured).
        #[arg(long, value_enum, value_name = "AGENT")]
        agent: Vec<HookAgent>,
        /// Show what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// List collected sessions, newest first.
    List {
        #[command(flatten)]
        filter: FilterArgs,
        #[arg(long)]
        json: bool,
    },
    /// Print a collected session's transcript.
    Show {
        /// Session ID, or a unique prefix of one.
        session_id: String,
        /// Disambiguate when several machines have the same session ID.
        #[arg(long, value_name = "NAME")]
        from: Option<String>,
        /// Print the stored JSONL instead of a readable transcript.
        #[arg(long)]
        raw: bool,
    },
    /// Full-text search across every collected conversation.
    Search {
        query: String,
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Show the machines that have reported sessions.
    Hosts,
    /// Show how much storage the collected sessions use.
    Stats,
    /// Delete old sessions, or only their transcripts.
    Prune {
        /// Age threshold, for example 90d, 6w, or 1y.
        #[arg(long, value_name = "AGE")]
        older_than: String,
        /// Limit the deletion to one machine.
        #[arg(long, value_name = "NAME")]
        from: Option<String>,
        /// Keep session metadata and messages; drop only the transcripts.
        #[arg(long)]
        transcripts_only: bool,
        /// Required to actually delete anything.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HookAgent {
    Claude,
    Codex,
}

impl HookAgent {
    fn agent(self) -> Agent {
        match self {
            Self::Claude => Agent::Claude,
            Self::Codex => Agent::Codex,
        }
    }
}

#[derive(Debug, Default, Args)]
struct SourceArgs {
    /// Read logs from this home directory instead of the current user's.
    #[arg(long, value_name = "HOME_DIR")]
    home: Option<PathBuf>,
    /// Codex sessions directory.
    #[arg(long, value_name = "SESSIONS_DIR")]
    codex_dir: Option<PathBuf>,
    /// Claude Code projects directory.
    #[arg(long, value_name = "PROJECTS_DIR")]
    claude_dir: Option<PathBuf>,
}

impl SourceArgs {
    fn options(&self) -> SourceOptions {
        SourceOptions {
            home_was_explicit: self.home.is_some(),
            home: self.home.clone(),
            codex_dir: self.codex_dir.clone(),
            claude_dir: self.claude_dir.clone(),
        }
    }
}

#[derive(Debug, Default, Args)]
struct FilterArgs {
    /// Only sessions from this machine.
    #[arg(long, value_name = "NAME")]
    from: Option<String>,
    /// Only sessions from this agent.
    #[arg(long, value_name = "AGENT")]
    agent: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: i64,
}

impl FilterArgs {
    fn filter(&self) -> SessionFilter {
        SessionFilter {
            host: self.from.clone(),
            agent: self.agent.as_deref().map(Agent::from_key),
            limit: self.limit,
            since: None,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let mut config = Config::load()?;
    if let Some(url) = &cli.database_url {
        config.database_url = Some(url.clone());
    }
    if let Some(host) = &cli.host {
        config.host = host.clone();
    }

    match cli.command {
        Command::Setup {
            database_url,
            machine,
            skip_schema,
        } => setup(&config, &database_url, machine.as_deref(), skip_schema),
        Command::Init => {
            let mut store = connect(&config)?;
            store.init_schema()?;
            println!(
                "schema ready on {}",
                config::redact_url(config.database_url()?)
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Ship {
            sources,
            file,
            quiet,
        } => ship(&config, &sources, file, quiet),
        Command::Watch { sources, interval } => watch(&config, &sources, interval),
        Command::Hook {
            agent,
            sources,
            payload,
        } => Ok(hook(&config, agent, &sources, payload)),
        Command::InstallHooks { agent, dry_run } => install_hooks(&agent, dry_run),
        Command::List { filter, json } => list(&config, &filter, json),
        Command::Show {
            session_id,
            from,
            raw,
        } => show(&config, &session_id, from.as_deref(), raw),
        Command::Search { query, filter } => search(&config, &query, &filter),
        Command::Hosts => hosts(&config),
        Command::Stats => stats(&config),
        Command::Prune {
            older_than,
            from,
            transcripts_only,
            yes,
        } => prune(&config, &older_than, from.as_deref(), transcripts_only, yes),
    }
}

fn connect(config: &Config) -> Result<Store> {
    Store::connect(config.database_url()?)
}

fn setup(
    config: &Config,
    database_url: &str,
    machine: Option<&str>,
    skip_schema: bool,
) -> Result<ExitCode> {
    config.save(Some(database_url), machine)?;
    println!("wrote {}", config.path.display());
    println!("  database {}", config::redact_url(database_url));
    println!("  machine  {}", machine.unwrap_or(&config.host));

    if skip_schema {
        return Ok(ExitCode::SUCCESS);
    }
    let mut store = Store::connect(database_url)?;
    store.init_schema()?;
    println!("schema ready");
    Ok(ExitCode::SUCCESS)
}

fn ship(
    config: &Config,
    sources: &SourceArgs,
    file: Option<PathBuf>,
    quiet: bool,
) -> Result<ExitCode> {
    let mut store = connect(config)?;
    let mut options = SyncOptions::new(config.host.clone(), discovery::sources(&sources.options()));
    options.only_path = file;

    let report = sync::sync(&mut store, &options)?;
    if !quiet {
        println!("{} → {}", config.host, report.summary());
        if report.unchanged > 0 {
            println!("  {} unchanged", report.unchanged);
        }
        if report.reset > 0 {
            println!("  {} re-uploaded after the log was rewritten", report.reset);
        }
        if report.stale > 0 {
            println!("  {} skipped, uploaded by another process", report.stale);
        }
    }
    for error in &report.errors {
        eprintln!("warning: {error}");
    }
    Ok(ExitCode::SUCCESS)
}

fn watch(config: &Config, sources: &SourceArgs, interval: u64) -> Result<ExitCode> {
    let options = SyncOptions::new(config.host.clone(), discovery::sources(&sources.options()));
    let interval = Duration::from_secs(interval.max(5));
    let mut store = connect(config)?;
    println!(
        "watching {} agent log directories every {}s as {}",
        options.sources.len(),
        interval.as_secs(),
        config.host
    );

    loop {
        match sync::sync(&mut store, &options) {
            Ok(report) => {
                if report.sessions > 0 {
                    println!("{} {}", Utc::now().format("%H:%M:%S"), report.summary());
                }
                for error in &report.errors {
                    eprintln!("warning: {error}");
                }
            }
            Err(error) => {
                eprintln!("warning: upload failed: {error:#}");
                // The connection is the usual casualty; rebuild it and retry.
                match connect(config) {
                    Ok(fresh) => store = fresh,
                    Err(error) => eprintln!("warning: reconnect failed: {error:#}"),
                }
            }
        }
        thread::sleep(interval);
    }
}

/// Hook mode never reports failure: a collector problem must not interrupt the
/// agent that called it.
fn hook(
    config: &Config,
    agent: HookAgent,
    sources: &SourceArgs,
    argument: Option<String>,
) -> ExitCode {
    let raw = argument.unwrap_or_else(|| {
        let mut buffer = String::new();
        let _ = io::stdin().read_to_string(&mut buffer);
        buffer
    });
    let payload = hooks::parse_payload(&raw);

    let result = (|| -> Result<()> {
        let mut store = Store::connect_with_timeout(config.database_url()?, HOOK_CONNECT_TIMEOUT)?;
        let mut options =
            SyncOptions::new(config.host.clone(), discovery::sources(&sources.options()));
        // Claude Code hands over the exact transcript; Codex only says that a
        // turn finished, so its logs are rescanned instead.
        options.only_path = payload
            .transcript_path
            .filter(|path| matches!(agent, HookAgent::Claude) && path.exists());
        if options.only_path.is_none() {
            options
                .sources
                .retain(|source| source.agent == agent.agent());
        }
        let report = sync::sync(&mut store, &options)?;
        for error in &report.errors {
            eprintln!("agent-logs: {error}");
        }
        Ok(())
    })();

    if let Err(error) = result {
        eprintln!("agent-logs: upload skipped: {error:#}");
    }
    ExitCode::SUCCESS
}

fn install_hooks(agents: &[HookAgent], dry_run: bool) -> Result<ExitCode> {
    let command = hooks::collector_command();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let wanted = |agent: HookAgent| {
        agents.is_empty()
            || agents
                .iter()
                .any(|selected| selected.agent() == agent.agent())
    };

    if wanted(HookAgent::Claude) {
        let settings = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".claude")))
            .map(|directory| directory.join("settings.json"));
        match settings {
            Some(path) => {
                let report = hooks::install_claude(&path, &command, dry_run)?;
                print_install("Claude Code", &report, dry_run);
            }
            None => eprintln!("warning: no HOME, cannot locate Claude Code settings"),
        }
    }

    if wanted(HookAgent::Codex) {
        let config = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".codex")))
            .map(|directory| directory.join("config.toml"));
        match config {
            Some(path) => {
                let report = hooks::install_codex(&path, &command, dry_run)?;
                print_install("Codex", &report, dry_run);
                println!(
                    "  Codex only notifies on finished turns; run `agent-logs watch` for live uploads"
                );
            }
            None => eprintln!("warning: no HOME, cannot locate the Codex configuration"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_install(agent: &str, report: &hooks::InstallReport, dry_run: bool) {
    let state = match (report.changed, dry_run) {
        (true, true) => "would change",
        (true, false) => "updated",
        (false, _) => "unchanged",
    };
    println!("{agent}: {state} {}", report.path.display());
    for note in &report.notes {
        println!("  {note}");
    }
}

fn list(config: &Config, filter: &FilterArgs, json: bool) -> Result<ExitCode> {
    let mut store = connect(config)?;
    let sessions = store.sessions(&filter.filter())?;

    if json {
        let mut out = io::stdout().lock();
        for session in &sessions {
            writeln!(out, "{}", session_json(session))?;
        }
        return Ok(ExitCode::SUCCESS);
    }

    if sessions.is_empty() {
        println!("no sessions collected yet");
        return Ok(ExitCode::SUCCESS);
    }

    let host_width = sessions
        .iter()
        .map(|session| session.host.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 20);
    println!(
        "{:<host_width$}  {:<6}  {:<16}  {:<22}  TITLE",
        "HOST", "AGENT", "UPDATED", "PROJECT"
    );
    for session in &sessions {
        println!(
            "{:<host_width$}  {:<6}  {:<16}  {:<22}  {}",
            truncate(&session.host, host_width),
            truncate(session.agent.key(), 6),
            session.updated.format("%Y-%m-%d %H:%M"),
            truncate(&session.project(), 22),
            truncate(&session.title, 60)
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn show(config: &Config, session_id: &str, from: Option<&str>, raw: bool) -> Result<ExitCode> {
    let mut store = connect(config)?;
    let matches = store.session_by_id(session_id, from)?;
    let session = match matches.len() {
        0 => bail!("no collected session matches {session_id}"),
        1 => &matches[0],
        _ => {
            eprintln!("{session_id} matches several sessions; pick one with --from:");
            for session in &matches {
                eprintln!("  {} {} {}", session.host, session.agent.key(), session.id);
            }
            return Ok(ExitCode::FAILURE);
        }
    };

    let key = session
        .remote_key
        .context("collected session is missing its database key")?;
    let jsonl = store.transcript(key)?;
    let mut out = io::stdout().lock();

    if raw {
        out.write_all(&jsonl)?;
        return Ok(ExitCode::SUCCESS);
    }

    writeln!(
        out,
        "{} · {} · {} · {}",
        session.host,
        session.agent.label(),
        session.id,
        session.cwd.display()
    )?;
    writeln!(out, "{}\n", session.title)?;
    if jsonl.is_empty() {
        writeln!(out, "(transcript not stored; it may have been pruned)")?;
        return Ok(ExitCode::SUCCESS);
    }

    for entry in read_transcript(&session.agent, &jsonl) {
        let marker = match entry.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Reasoning => "thinking",
            Role::Tool => "tool",
        };
        let time = entry
            .timestamp
            .map(|timestamp| timestamp.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "        ".to_owned());
        writeln!(out, "[{time}] {marker}:")?;
        for line in entry.text.lines() {
            writeln!(out, "    {line}")?;
        }
        writeln!(out)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn search(config: &Config, query: &str, filter: &FilterArgs) -> Result<ExitCode> {
    let mut store = connect(config)?;
    let hits = store.search(query, &filter.filter())?;
    if hits.is_empty() {
        println!("nothing matches {query}");
        return Ok(ExitCode::SUCCESS);
    }
    for hit in &hits {
        println!(
            "{} · {} · {} · {}",
            hit.session.host,
            hit.session.agent.key(),
            hit.written_at
                .unwrap_or(hit.session.updated)
                .format("%Y-%m-%d %H:%M"),
            hit.session.id
        );
        println!("  {}: {}", hit.role.key(), hit.snippet);
    }
    Ok(ExitCode::SUCCESS)
}

fn hosts(config: &Config) -> Result<ExitCode> {
    let mut store = connect(config)?;
    let hosts = store.hosts()?;
    if hosts.is_empty() {
        println!("no machines have reported sessions yet");
        return Ok(ExitCode::SUCCESS);
    }
    println!("{:<24}  {:>8}  LAST ACTIVITY", "MACHINE", "SESSIONS");
    for host in &hosts {
        println!(
            "{:<24}  {:>8}  {}",
            truncate(&host.host, 24),
            host.sessions,
            host.last_activity
                .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "-".to_owned())
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn stats(config: &Config) -> Result<ExitCode> {
    let mut store = connect(config)?;
    let usage = store.usage()?;
    if usage.is_empty() {
        println!("no sessions collected yet");
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "{:<20}  {:<6}  {:>8}  {:>10}  {:>10}  {:>9}",
        "MACHINE", "AGENT", "SESSIONS", "LOGGED", "STORED", "MESSAGES"
    );
    let mut raw_total = 0_i64;
    let mut stored_total = 0_i64;
    for row in &usage {
        raw_total += row.raw_bytes;
        stored_total += row.stored_bytes;
        println!(
            "{:<20}  {:<6}  {:>8}  {:>10}  {:>10}  {:>9}",
            truncate(&row.host, 20),
            truncate(&row.agent, 6),
            row.sessions,
            human_bytes(row.raw_bytes.max(0) as u64),
            human_bytes(row.stored_bytes.max(0) as u64),
            row.messages
        );
    }
    let ratio = if stored_total > 0 {
        raw_total as f64 / stored_total as f64
    } else {
        0.0
    };
    println!(
        "\ntotal: {} of logs stored as {} ({ratio:.1}x smaller)",
        human_bytes(raw_total.max(0) as u64),
        human_bytes(stored_total.max(0) as u64)
    );
    Ok(ExitCode::SUCCESS)
}

fn prune(
    config: &Config,
    older_than: &str,
    from: Option<&str>,
    transcripts_only: bool,
    yes: bool,
) -> Result<ExitCode> {
    let cutoff = Utc::now() - parse_age(older_than)?;
    let target = if transcripts_only {
        "transcripts"
    } else {
        "sessions"
    };
    if !yes {
        println!(
            "would delete {target} last updated before {} ({}); pass --yes to run it",
            cutoff.format("%Y-%m-%d %H:%M UTC"),
            from.unwrap_or("all machines")
        );
        return Ok(ExitCode::SUCCESS);
    }

    let mut store = connect(config)?;
    let report = store.prune(cutoff, from, transcripts_only)?;
    if transcripts_only {
        println!("deleted {} transcript chunk(s)", report.chunks);
    } else {
        println!("deleted {} session(s)", report.sessions);
    }
    Ok(ExitCode::SUCCESS)
}

fn parse_age(value: &str) -> Result<chrono::Duration> {
    let value = value.trim();
    let (number, unit) = value.split_at(
        value
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(value.len()),
    );
    let number: i64 = number
        .parse()
        .with_context(|| format!("{value} is not an age like 90d or 6w"))?;
    let duration = match unit.trim() {
        "h" => chrono::Duration::hours(number),
        "d" | "" => chrono::Duration::days(number),
        "w" => chrono::Duration::weeks(number),
        "m" => chrono::Duration::days(number * 30),
        "y" => chrono::Duration::days(number * 365),
        other => bail!("unknown age unit {other}; use h, d, w, m, or y"),
    };
    if duration <= chrono::Duration::zero() {
        bail!("the age must be positive");
    }
    Ok(duration)
}

fn session_json(session: &Session) -> String {
    serde_json::json!({
        "host": session.host,
        "agent": session.agent.key(),
        "session_id": session.id,
        "title": session.title,
        "cwd": session.cwd.display().to_string(),
        "path": session.path.display().to_string(),
        "created_at": rfc3339(session.created),
        "updated_at": rfc3339(session.updated),
        "bytes": session.bytes,
        "messages": session.messages,
    })
    .to_string()
}

fn rfc3339(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut result: String = text.chars().take(width.saturating_sub(1)).collect();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::{parse_age, truncate};

    #[test]
    fn ages_accept_the_common_units() {
        assert_eq!(parse_age("90d").unwrap(), chrono::Duration::days(90));
        assert_eq!(parse_age("6w").unwrap(), chrono::Duration::weeks(6));
        assert_eq!(parse_age("12h").unwrap(), chrono::Duration::hours(12));
        assert_eq!(parse_age("30").unwrap(), chrono::Duration::days(30));
        assert!(parse_age("soon").is_err());
        assert!(parse_age("0d").is_err());
    }

    #[test]
    fn truncation_is_character_aware() {
        assert_eq!(truncate("привет", 10), "привет");
        assert_eq!(truncate("привет мир", 6), "приве…");
    }
}
