//! Wiring the collector into the agents themselves, so sessions upload as they
//! happen instead of only when a sync is run by hand.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

/// Claude Code events worth uploading on: the end of every assistant turn, and
/// the end of the session.
pub const CLAUDE_EVENTS: [&str; 2] = ["Stop", "SessionEnd"];

#[derive(Clone, Debug, Default)]
pub struct HookPayload {
    pub session_id: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub event: Option<String>,
}

/// Reads the JSON an agent hands to a hook. Claude Code passes it on stdin;
/// Codex passes it as a single argument.
pub fn parse_payload(raw: &str) -> HookPayload {
    let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(raw.trim()) else {
        return HookPayload::default();
    };
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    HookPayload {
        session_id: text("session_id").or_else(|| text("session-id")),
        transcript_path: text("transcript_path").map(PathBuf::from),
        cwd: text("cwd").map(PathBuf::from),
        event: text("hook_event_name").or_else(|| text("type")),
    }
}

#[derive(Debug)]
pub struct InstallReport {
    pub path: PathBuf,
    pub changed: bool,
    pub notes: Vec<String>,
}

/// The command an agent should run. The absolute path is preferred so hooks
/// keep working in environments with a trimmed PATH.
pub fn collector_command() -> String {
    env::current_exe()
        .ok()
        .filter(|path| path.file_name().is_some_and(|name| name == "agent-logs"))
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "agent-logs".to_owned())
}

/// Adds the upload hooks to Claude Code's settings without disturbing hooks
/// that are already configured.
pub fn install_claude(settings_path: &Path, command: &str, dry_run: bool) -> Result<InstallReport> {
    let mut report = InstallReport {
        path: settings_path.to_owned(),
        changed: false,
        notes: Vec::new(),
    };
    let mut settings = read_json_object(settings_path)?;
    let hook_command = format!("{command} hook claude");

    let hooks = settings
        .entry("hooks".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        report
            .notes
            .push("\"hooks\" in the settings file is not an object; left untouched".to_owned());
        return Ok(report);
    };

    for event in CLAUDE_EVENTS {
        let entry = hooks
            .entry(event.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(groups) = entry.as_array_mut() else {
            report
                .notes
                .push(format!("\"hooks.{event}\" is not an array; left untouched"));
            continue;
        };
        if groups.iter().any(|group| mentions(group, &hook_command)) {
            report.notes.push(format!("{event}: already installed"));
            continue;
        }
        groups.push(serde_json::json!({
            "hooks": [{ "type": "command", "command": hook_command }]
        }));
        report.changed = true;
        report.notes.push(format!("{event}: added"));
    }

    if report.changed && !dry_run {
        write_json_object(settings_path, &settings)?;
    }
    Ok(report)
}

/// Points Codex's `notify` program at the collector. Codex only reports turn
/// completions, so the collector rescans that agent's logs when it fires.
pub fn install_codex(config_path: &Path, command: &str, dry_run: bool) -> Result<InstallReport> {
    let mut report = InstallReport {
        path: config_path.to_owned(),
        changed: false,
        notes: Vec::new(),
    };
    let existing = match fs::read_to_string(config_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", config_path.display()));
        }
    };

    let line = format!("notify = [{}, \"hook\", \"codex\"]", toml_string(command));
    if existing.lines().any(|text| text.trim() == line) {
        report.notes.push("notify: already installed".to_owned());
        return Ok(report);
    }
    if existing
        .lines()
        .any(|text| text.trim_start().starts_with("notify"))
    {
        report.notes.push(format!(
            "notify is already set to something else; add manually:\n    {line}"
        ));
        return Ok(report);
    }

    // Top-level keys have to sit above the first table header.
    let mut updated = String::new();
    let mut inserted = false;
    for text in existing.lines() {
        if !inserted && text.trim_start().starts_with('[') {
            updated.push_str(&line);
            updated.push_str("\n\n");
            inserted = true;
        }
        updated.push_str(text);
        updated.push('\n');
    }
    if !inserted {
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&line);
        updated.push('\n');
    }

    report.changed = true;
    report.notes.push("notify: added".to_owned());
    if !dry_run {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        backup(config_path)?;
        fs::write(config_path, updated)
            .with_context(|| format!("could not write {}", config_path.display()))?;
    }
    Ok(report)
}

fn mentions(group: &Value, command: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|existing| existing.trim() == command)
            })
        })
        .unwrap_or(false)
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>> {
    let body = match fs::read_to_string(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    if body.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(&body)
        .with_context(|| format!("{} is not valid JSON", path.display()))?
    {
        Value::Object(map) => Ok(map),
        _ => anyhow::bail!("{} must contain a JSON object", path.display()),
    }
}

fn write_json_object(path: &Path, value: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    backup(path)?;
    let body = serde_json::to_string_pretty(&Value::Object(value.clone()))?;
    fs::write(path, format!("{body}\n"))
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

/// Keeps one copy of the file as it was before the collector touched it.
fn backup(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let backup = path.with_extension(format!(
        "{}.agent-logs-backup",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("bak")
    ));
    if backup.exists() {
        return Ok(());
    }
    fs::copy(path, &backup).with_context(|| format!("could not back up {}", path.display()))?;
    Ok(())
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{install_claude, install_codex, parse_payload};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("agent-logs-test-{name}"));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn claude_hooks_are_added_next_to_existing_ones() {
        let directory = temp_dir("claude-hooks");
        let settings = directory.join("settings.json");
        fs::write(
            &settings,
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"other"}]}]}}"#,
        )
        .unwrap();

        let report = install_claude(&settings, "/usr/local/bin/agent-logs", false).unwrap();
        assert!(report.changed);

        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(written["model"], "opus");
        let stop = written["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "the existing hook must survive");
        assert_eq!(
            stop[1]["hooks"][0]["command"],
            "/usr/local/bin/agent-logs hook claude"
        );
        assert!(written["hooks"]["SessionEnd"].is_array());
        assert!(settings.with_extension("json.agent-logs-backup").exists());

        // Installing twice must not duplicate anything.
        let second = install_claude(&settings, "/usr/local/bin/agent-logs", false).unwrap();
        assert!(!second.changed);
    }

    #[test]
    fn codex_notify_is_written_above_the_first_table() {
        let directory = temp_dir("codex-notify");
        let config = directory.join("config.toml");
        fs::write(
            &config,
            "model = \"gpt-5\"\n\n[projects.\"/work\"]\ntrust_level = \"trusted\"\n",
        )
        .unwrap();

        let report = install_codex(&config, "/usr/local/bin/agent-logs", false).unwrap();
        assert!(report.changed);

        let body = fs::read_to_string(&config).unwrap();
        let notify_line = body
            .lines()
            .position(|line| line.starts_with("notify = "))
            .unwrap();
        let table_line = body.lines().position(|line| line.starts_with('[')).unwrap();
        assert!(notify_line < table_line);
        assert!(body.contains("trust_level = \"trusted\""));

        let second = install_codex(&config, "/usr/local/bin/agent-logs", false).unwrap();
        assert!(!second.changed);
    }

    #[test]
    fn an_existing_notify_setting_is_never_overwritten() {
        let directory = temp_dir("codex-existing-notify");
        let config = directory.join("config.toml");
        fs::write(&config, "notify = [\"my-script\"]\n").unwrap();

        let report = install_codex(&config, "/usr/local/bin/agent-logs", false).unwrap();

        assert!(!report.changed);
        assert!(report.notes[0].contains("already set to something else"));
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            "notify = [\"my-script\"]\n"
        );
    }

    #[test]
    fn hook_payloads_expose_the_transcript_path() {
        let payload = parse_payload(
            r#"{"session_id":"abc","transcript_path":"/logs/abc.jsonl","hook_event_name":"Stop"}"#,
        );

        assert_eq!(payload.session_id.as_deref(), Some("abc"));
        assert_eq!(
            payload.transcript_path.unwrap().to_str(),
            Some("/logs/abc.jsonl")
        );
        assert_eq!(payload.event.as_deref(), Some("Stop"));
        assert!(parse_payload("not json").transcript_path.is_none());
    }
}
