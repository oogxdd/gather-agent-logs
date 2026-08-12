//! Finding agent log files on this machine and reading their metadata.

use std::{
    env,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use chrono::{DateTime, Utc};

use crate::{
    model::{Agent, Origin, Session},
    transcript::SessionScanner,
};

/// One agent's log directory on this machine. Supporting another CLI is a
/// matter of adding an entry here and teaching `transcript` its record shape.
#[derive(Clone, Debug)]
pub struct AgentSource {
    pub agent: Agent,
    pub root: PathBuf,
    /// Claude Code stores subagent transcripts next to real sessions.
    pub skip_subagents: bool,
}

#[derive(Debug, Default)]
pub struct SourceOptions {
    pub home: Option<PathBuf>,
    /// A caller-provided home disables the agents' own environment overrides.
    pub home_was_explicit: bool,
    pub codex_dir: Option<PathBuf>,
    pub claude_dir: Option<PathBuf>,
}

/// Resolves where each agent keeps its logs, honouring the agents' own
/// environment variables unless an explicit directory was given.
pub fn sources(options: &SourceOptions) -> Vec<AgentSource> {
    let home = options
        .home
        .clone()
        .or_else(|| env::var_os("HOME").map(PathBuf::from));
    let explicit = options.home_was_explicit;

    let codex_root = match options.codex_dir.clone() {
        Some(directory) => directory,
        None if !explicit => env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .map(|directory| directory.join("sessions"))
            .unwrap_or_else(|| under_home(&home, ".codex/sessions")),
        None => under_home(&home, ".codex/sessions"),
    };
    let claude_root = match options.claude_dir.clone() {
        Some(directory) => directory,
        None if !explicit => env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .map(|directory| directory.join("projects"))
            .unwrap_or_else(|| under_home(&home, ".claude/projects")),
        None => under_home(&home, ".claude/projects"),
    };

    vec![
        AgentSource {
            agent: Agent::Codex,
            root: codex_root,
            skip_subagents: false,
        },
        AgentSource {
            agent: Agent::Claude,
            root: claude_root,
            skip_subagents: true,
        },
    ]
}

fn under_home(home: &Option<PathBuf>, relative: &str) -> PathBuf {
    home.as_ref()
        .map(|directory| directory.join(relative))
        .unwrap_or_default()
}

#[derive(Clone, Debug)]
pub struct LogFile {
    pub agent: Agent,
    pub path: PathBuf,
    pub size: u64,
    pub modified: DateTime<Utc>,
    /// Session ID taken from the file name, when it carries one. Lets the
    /// collector skip unchanged files without reading them.
    pub id_hint: Option<String>,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: Vec<LogFile>,
    pub skipped: usize,
}

pub fn scan(sources: &[AgentSource]) -> ScanResult {
    let mut result = ScanResult::default();
    for source in sources {
        let mut paths = jsonl_files(&source.root, source.skip_subagents, &mut result.skipped);
        paths.sort_unstable();
        for path in paths {
            let Ok(metadata) = fs::metadata(&path) else {
                result.skipped += 1;
                continue;
            };
            let modified = metadata
                .modified()
                .map(DateTime::<Utc>::from)
                .unwrap_or_else(|_| DateTime::<Utc>::from(UNIX_EPOCH));
            result.files.push(LogFile {
                agent: source.agent.clone(),
                id_hint: session_id_from_path(&path),
                path,
                size: metadata.len(),
                modified,
            });
        }
    }
    result
}

#[derive(Debug, Default)]
pub struct DiscoveryResult {
    pub sessions: Vec<Session>,
    pub skipped: usize,
}

/// Reads every local session's metadata. Transcripts are never held in memory.
pub fn discover(sources: &[AgentSource], host: &str) -> DiscoveryResult {
    let scanned = scan(sources);
    let mut result = DiscoveryResult {
        sessions: Vec::new(),
        skipped: scanned.skipped,
    };

    for file in scanned.files {
        match read_session(&file, host) {
            Some(session) => result.sessions.push(session),
            None => result.skipped += 1,
        }
    }
    result
}

pub fn read_session(file: &LogFile, host: &str) -> Option<Session> {
    let handle = File::open(&file.path).ok()?;
    read_session_from(BufReader::new(handle), file, host, file.modified)
}

fn read_session_from(
    reader: impl BufRead,
    file: &LogFile,
    host: &str,
    fallback: DateTime<Utc>,
) -> Option<Session> {
    let mut scanner = SessionScanner::new(&file.agent);
    let mut messages = 0_u32;
    for (index, line) in reader.lines().enumerate() {
        let Ok(line) = line else { continue };
        messages += scanner.push(index as i64, &line).len() as u32;
    }

    let meta = scanner.into_meta();
    let id = meta.id.or_else(|| session_id_from_path(&file.path))?;
    Some(Session {
        agent: file.agent.clone(),
        host: host.to_owned(),
        origin: Origin::Local,
        title: meta
            .title
            .unwrap_or_else(|| format!("Untitled {} session", file.agent.label())),
        cwd: meta.cwd.unwrap_or_default(),
        path: file.path.clone(),
        created: meta.created.unwrap_or(fallback),
        updated: meta.updated.or(meta.created).unwrap_or(fallback),
        remote_key: None,
        bytes: file.size,
        messages,
        id,
    })
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

/// Both agents end their file names with the session UUID.
pub fn session_id_from_path(path: &Path) -> Option<String> {
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
    use std::{io::Cursor, path::PathBuf};

    use chrono::{TimeZone, Utc};

    use super::{Agent, LogFile, read_session_from, session_id_from_path};

    fn log_file(agent: Agent, path: &str) -> LogFile {
        LogFile {
            agent,
            path: PathBuf::from(path),
            size: 0,
            modified: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            id_hint: session_id_from_path(&PathBuf::from(path)),
        }
    }

    #[test]
    fn reads_codex_session_metadata() {
        let input = concat!(
            r#"{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{"id":"019ff21e-4824-70d2-8cb6-57e5b1aebefb","cwd":"/work/repo","timestamp":"2026-01-01T10:00:00Z"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T10:03:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix the auth race"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T10:04:00Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}"#,
            "\n"
        );
        let file = log_file(Agent::Codex, "rollout.jsonl");

        let session = read_session_from(
            Cursor::new(input),
            &file,
            "laptop",
            Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        )
        .unwrap();

        assert_eq!(session.id, "019ff21e-4824-70d2-8cb6-57e5b1aebefb");
        assert_eq!(session.host, "laptop");
        assert_eq!(session.title, "Fix the auth race");
        assert_eq!(session.messages, 2);
        assert_eq!(session.cwd, PathBuf::from("/work/repo"));
    }

    #[test]
    fn falls_back_to_the_session_id_in_the_file_name() {
        let file = log_file(
            Agent::Claude,
            "/logs/86e1a7f3-2e99-483f-b743-64e0003c58c8.jsonl",
        );

        let session = read_session_from(
            Cursor::new("{}\n"),
            &file,
            "laptop",
            Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        )
        .unwrap();

        assert_eq!(session.id, "86e1a7f3-2e99-483f-b743-64e0003c58c8");
        assert_eq!(session.title, "Untitled Claude session");
    }

    #[test]
    fn ignores_file_names_without_a_session_id() {
        assert!(session_id_from_path(&PathBuf::from("/logs/history.jsonl")).is_none());
    }
}
