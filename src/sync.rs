//! Shipping local log files to the database, one incremental slice at a time.
//!
//! Every upload starts where the last one stopped, so a session that is still
//! being written costs only its new bytes. Partly written trailing lines are
//! left for the next run.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use flate2::{Compression, write::GzEncoder};

use crate::{
    db::{Delta, DeltaOutcome, SessionState, Store},
    discovery::{self, AgentSource, LogFile},
    model::Agent,
    transcript::{Message, SessionMeta, SessionScanner},
};

/// Raw bytes per stored chunk. Small enough to keep statements light, large
/// enough that compression stays effective.
pub const DEFAULT_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Most of a first record; long enough to identify a session, short enough to
/// stay cheap on every scan.
const SIGNATURE_BYTES: usize = 4096;

pub struct SyncOptions {
    pub host: String,
    pub sources: Vec<AgentSource>,
    /// Ship a single log file, as the agent hooks do.
    pub only_path: Option<PathBuf>,
    pub max_chunk_bytes: usize,
}

impl SyncOptions {
    pub fn new(host: String, sources: Vec<AgentSource>) -> Self {
        Self {
            host,
            sources,
            only_path: None,
            max_chunk_bytes: DEFAULT_CHUNK_BYTES,
        }
    }
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub files: usize,
    pub sessions: usize,
    pub raw_bytes: u64,
    pub stored_bytes: u64,
    pub messages: usize,
    pub unchanged: usize,
    pub reset: usize,
    /// Ranges another process had already uploaded.
    pub stale: usize,
    pub errors: Vec<String>,
}

impl SyncReport {
    pub fn summary(&self) -> String {
        format!(
            "{} session(s), {} new, {} stored ({} message(s))",
            self.files,
            human_bytes(self.raw_bytes),
            human_bytes(self.stored_bytes),
            self.messages
        )
    }
}

pub fn sync(store: &mut Store, options: &SyncOptions) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let files = match &options.only_path {
        Some(path) => single_file(path, &options.sources)?.into_iter().collect(),
        None => {
            let scanned = discovery::scan(&options.sources);
            scanned.files
        }
    };

    // One query covers every session this machine has reported. Without it,
    // a machine with hundreds of finished sessions would ask the database
    // about each of them on every run.
    let synced = store.synced_sizes(&options.host)?;

    for file in files {
        report.files += 1;
        match sync_file(store, options, &file, &synced, &mut report) {
            Ok(()) => {}
            Err(error) => report
                .errors
                .push(format!("{}: {error:#}", file.path.display())),
        }
    }
    Ok(report)
}

fn single_file(path: &Path, sources: &[AgentSource]) -> Result<Option<LogFile>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    let agent = agent_for_path(path, sources).unwrap_or(Agent::Claude);
    Ok(Some(LogFile {
        agent,
        id_hint: discovery::session_id_from_path(path),
        path: path.to_path_buf(),
        size: metadata.len(),
        modified: metadata
            .modified()
            .map(chrono::DateTime::from)
            .unwrap_or_else(|_| chrono::Utc::now()),
    }))
}

fn agent_for_path(path: &Path, sources: &[AgentSource]) -> Option<Agent> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    sources
        .iter()
        .find(|source| {
            source
                .root
                .canonicalize()
                .map(|root| path.starts_with(root))
                .unwrap_or(false)
        })
        .map(|source| source.agent.clone())
}

fn sync_file(
    store: &mut Store,
    options: &SyncOptions,
    file: &LogFile,
    synced: &HashMap<(String, String), (i64, String)>,
    report: &mut SyncReport,
) -> Result<()> {
    let signature = signature(&file.path)?;
    let Some(session_id) = session_id(file)? else {
        return Ok(());
    };

    // A log of the size the database already holds, with the same first
    // record, has nothing new in it.
    if let Some((synced_bytes, stored_signature)) =
        synced.get(&(file.agent.key().to_owned(), session_id.clone()))
        && *synced_bytes == file.size as i64
        && *stored_signature == signature
    {
        report.unchanged += 1;
        return Ok(());
    }

    let mut state = store.session_state(&options.host, &file.agent, &session_id)?;

    // A shorter file, or a different first block, means the log was rewritten
    // rather than appended to; anything stored for it is no longer valid.
    let rewritten = (!state.signature.is_empty() && state.signature != signature)
        || file.size < state.synced_bytes.max(0) as u64;
    if rewritten {
        store.reset_session(state.key)?;
        state = reset_state(state);
        report.reset += 1;
    }

    if file.size == state.synced_bytes.max(0) as u64 {
        report.unchanged += 1;
        return Ok(());
    }

    let delta = read_delta(&file.path, state.synced_bytes.max(0) as u64)?;
    if delta.is_empty() {
        report.unchanged += 1;
        return Ok(());
    }

    let mut scanner = SessionScanner::with_meta(&file.agent, meta_from_state(&state));
    let mut byte_cursor = state.synced_bytes.max(0);
    let mut line_cursor = state.line_count.max(0);
    let source_path = file.path.display().to_string();
    let mut uploaded_any = false;

    for slice in chunks(&delta, options.max_chunk_bytes.max(64 * 1024)) {
        let mut messages: Vec<Message> = Vec::new();
        let mut lines = 0_i64;
        for line in slice.split_inclusive(|byte| *byte == b'\n') {
            if let Ok(text) = std::str::from_utf8(line) {
                messages.extend(scanner.push(line_cursor + lines, text.trim_end()));
            }
            lines += 1;
        }

        let body = compress(slice)?;
        let meta = scanner.meta();
        let cwd = meta
            .cwd
            .as_ref()
            .map(|cwd| cwd.display().to_string())
            .unwrap_or_default();
        let outcome = store.store_delta(&Delta {
            host: &options.host,
            agent: &file.agent,
            session_id: &session_id,
            source_path: &source_path,
            signature: &signature,
            cwd: (!cwd.is_empty()).then_some(cwd.as_str()),
            title: meta.title.as_deref(),
            title_is_authoritative: meta.title_is_authoritative,
            created: meta.created,
            updated: meta.updated.or(meta.created),
            byte_start: byte_cursor,
            byte_end: byte_cursor + slice.len() as i64,
            line_start: line_cursor,
            line_count: lines,
            body: &body,
            messages: &messages,
        })?;

        if outcome == DeltaOutcome::Stale {
            report.stale += 1;
            return Ok(());
        }

        byte_cursor += slice.len() as i64;
        line_cursor += lines;
        report.raw_bytes += slice.len() as u64;
        report.stored_bytes += body.len() as u64;
        report.messages += messages.len();
        uploaded_any = true;
    }

    if uploaded_any {
        report.sessions += 1;
    }
    Ok(())
}

/// Reads everything after `offset` that ends in a complete line.
fn read_delta(path: &Path, offset: u64) -> Result<Vec<u8>> {
    let mut file =
        File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)
        .with_context(|| format!("could not read {}", path.display()))?;

    match buffer.iter().rposition(|byte| *byte == b'\n') {
        Some(index) => {
            buffer.truncate(index + 1);
            Ok(buffer)
        }
        None => Ok(Vec::new()),
    }
}

/// Splits a delta into upload-sized pieces without ever cutting a line.
fn chunks(delta: &[u8], max_bytes: usize) -> Vec<&[u8]> {
    let max_bytes = max_bytes.max(1);
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < delta.len() {
        let limit = (start + max_bytes).min(delta.len());
        let end = if limit == delta.len() {
            delta.len()
        } else {
            match delta[start..limit].iter().rposition(|byte| *byte == b'\n') {
                Some(index) => start + index + 1,
                // A single line longer than the chunk size: keep it whole.
                None => match delta[limit..].iter().position(|byte| *byte == b'\n') {
                    Some(index) => limit + index + 1,
                    None => delta.len(),
                },
            }
        };
        pieces.push(&delta[start..end]);
        start = end;
    }
    pieces
}

fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

fn session_id(file: &LogFile) -> Result<Option<String>> {
    if let Some(id) = &file.id_hint {
        return Ok(Some(id.clone()));
    }

    // No ID in the file name, so the records have to be read for one.
    let handle = File::open(&file.path)
        .with_context(|| format!("could not open {}", file.path.display()))?;
    let mut scanner = SessionScanner::new(&file.agent);
    let mut reader = std::io::BufReader::new(handle);
    let mut index = 0_i64;
    loop {
        let mut bytes = Vec::new();
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            break;
        }
        if let Ok(text) = std::str::from_utf8(&bytes) {
            scanner.push(index, text.trim_end());
        }
        if scanner.meta().id.is_some() {
            break;
        }
        index += 1;
    }
    Ok(scanner.into_meta().id)
}

/// Cheap fingerprint of a log file's first record, used to notice that a file
/// was replaced by a different session rather than appended to. It covers the
/// first line only, so appending to a short file never looks like a rewrite.
fn signature(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut head = Vec::new();
    std::io::BufReader::new(file.take(SIGNATURE_BYTES as u64))
        .read_until(b'\n', &mut head)
        .with_context(|| format!("could not read {}", path.display()))?;

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in &head {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!("{hash:016x}"))
}

fn meta_from_state(state: &SessionState) -> SessionMeta {
    SessionMeta {
        id: None,
        cwd: state.cwd.as_ref().map(PathBuf::from),
        title: state.title.clone(),
        created: state.created,
        updated: state.updated,
        title_is_authoritative: state.title_is_authoritative,
    }
}

fn reset_state(state: SessionState) -> SessionState {
    SessionState {
        synced_bytes: 0,
        line_count: 0,
        chunk_count: 0,
        message_count: 0,
        signature: String::new(),
        title: None,
        title_is_authoritative: false,
        cwd: None,
        created: None,
        updated: None,
        ..state
    }
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use super::{chunks, human_bytes, signature};

    #[test]
    fn appending_does_not_look_like_a_rewrite() {
        let path = std::env::temp_dir().join("agent-logs-signature-test.jsonl");
        let _ = fs::remove_file(&path);
        fs::write(&path, "{\"first\":1}\n").unwrap();
        let before = signature(&path).unwrap();

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all("{\"second\":2}\n".repeat(40).as_bytes())
            .unwrap();
        assert_eq!(
            signature(&path).unwrap(),
            before,
            "a short file that grows is still the same session"
        );

        fs::write(&path, "{\"different\":1}\n").unwrap();
        assert_ne!(
            signature(&path).unwrap(),
            before,
            "a replaced first record is a different session"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn chunks_never_split_a_line() {
        let delta = b"aaaa\nbbbb\ncccc\n";

        let pieces = chunks(delta, 6);

        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces[0], b"aaaa\n");
        assert_eq!(pieces[2], b"cccc\n");
        assert_eq!(
            pieces.concat(),
            delta.to_vec(),
            "chunking must preserve every byte"
        );
    }

    #[test]
    fn a_line_longer_than_the_chunk_size_stays_whole() {
        let delta = b"aaaaaaaaaaaaaaaaaaaa\nbb\n";

        let pieces = chunks(delta, 4);

        assert_eq!(pieces[0], b"aaaaaaaaaaaaaaaaaaaa\n");
        assert_eq!(pieces.concat(), delta.to_vec());
    }

    #[test]
    fn byte_sizes_read_as_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
