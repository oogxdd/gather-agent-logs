// Menu bar front end for the ChatGPT to Postgres sync daemon.
//
// It owns no sync logic: Postgres is read directly for what to display, and
// actions shell out to chatgpt_sync.py, which already serialises concurrent
// runs with an advisory lock. The tray icon reflects the one thing the daemon
// cannot fix on its own — whether the ChatGPT app is reachable at all.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::net::TcpStream;
use tokio::process::Command;

const ICON_GREY: &[u8] = include_bytes!("../../icons/tray-grey@2x.png");
const ICON_GREEN: &[u8] = include_bytes!("../../icons/tray-green@2x.png");
const ICON_RED: &[u8] = include_bytes!("../../icons/tray-red@2x.png");

/// What the tray colour means.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum AppRun {
    /// ChatGPT is not running — grey.
    Off,
    /// Running with the debug port; the daemon can sync — green.
    Ready,
    /// Running without the port; nothing can be synced — red.
    NoPort,
}

impl AppRun {
    fn icon(self) -> &'static [u8] {
        match self {
            AppRun::Off => ICON_GREY,
            AppRun::Ready => ICON_GREEN,
            AppRun::NoPort => ICON_RED,
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            AppRun::Off => "ChatGPT is not running",
            AppRun::Ready => "ChatGPT is running — sync available",
            AppRun::NoPort => "ChatGPT is running without the debug port — sync stalled",
        }
    }
}

struct Settings {
    dsn: String,
    port: u16,
    python: PathBuf,
    script: PathBuf,
}

impl Settings {
    fn load() -> Self {
        // Defaults point at the checkout this binary was built from, which is
        // what a personal tool wants; every field has an env override.
        let root = std::env::var("CHATGPT_SYNC_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from("."))
            });
        Settings {
            dsn: std::env::var("CHATGPT_SYNC_DSN")
                .unwrap_or_else(|_| "postgresql:///chatgpt_logs".into()),
            port: std::env::var("CHATGPT_SYNC_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(9222),
            python: root.join(".venv/bin/python"),
            script: root.join("chatgpt_sync.py"),
        }
    }
}

async fn port_open(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_millis(700),
            TcpStream::connect(("127.0.0.1", port))
        )
        .await,
        Ok(Ok(_))
    )
}

async fn app_running() -> bool {
    Command::new("pgrep")
        .args(["-f", "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn detect(port: u16) -> AppRun {
    if port_open(port).await {
        AppRun::Ready
    } else if app_running().await {
        AppRun::NoPort
    } else {
        AppRun::Off
    }
}

/// Connect for one query and drop the connection with it. The tray polls at
/// human speed, so a pool would be more machinery than this needs.
async fn connect(dsn: &str) -> Result<tokio_postgres::Client, String> {
    let (client, connection) = tokio_postgres::connect(dsn, tokio_postgres::NoTls)
        .await
        .map_err(|e| format!("postgres: {e}"))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

fn iso(value: Option<DateTime<Utc>>) -> Value {
    value.map(|t| json!(t.to_rfc3339())).unwrap_or(Value::Null)
}

async fn run_cli(settings: &Settings, args: &[&str]) -> Result<String, String> {
    let output = Command::new(&settings.python)
        .arg(&settings.script)
        .args(args)
        .arg("--dsn")
        .arg(&settings.dsn)
        .output()
        .await
        .map_err(|e| format!("cannot run chatgpt_sync.py: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(text)
    } else {
        Err(text.trim().to_string())
    }
}

#[tauri::command]
async fn get_status(settings: State<'_, Settings>) -> Result<Value, String> {
    let app_state = detect(settings.port).await;
    let client = connect(&settings.dsn).await?;

    let totals = client
        .query_one(
            "SELECT count(*)::bigint,
                    count(*) FILTER (WHERE status = 'synced')::bigint,
                    count(*) FILTER (WHERE status = 'stale')::bigint,
                    count(*) FILTER (WHERE status = 'never')::bigint,
                    count(*) FILTER (WHERE status = 'syncing')::bigint,
                    count(*) FILTER (WHERE scope = 'managed')::bigint
             FROM hist.overview
             WHERE platform = 'chatgpt'",
            &[],
        )
        .await
        .map_err(|e| format!("overview: {e}"))?;

    let last_run = client
        .query_opt(
            "SELECT started_at, finished_at, status, imported, error
             FROM hist.import_runs WHERE source_id LIKE 'chatgpt:%' ORDER BY id DESC LIMIT 1",
            &[],
        )
        .await
        .map_err(|e| format!("sync_runs: {e}"))?;

    let last_ok = client
        .query_opt(
            "SELECT finished_at FROM hist.import_runs
             WHERE source_id LIKE 'chatgpt:%' AND status = 'ok' AND finished_at IS NOT NULL
             ORDER BY id DESC LIMIT 1",
            &[],
        )
        .await
        .map_err(|e| format!("sync_runs: {e}"))?;

    let runtime = client
        .query_opt(
            "SELECT value FROM hist.sync_state WHERE key = 'runtime'",
            &[],
        )
        .await
        .map_err(|e| format!("sync_state: {e}"))?
        .map(|r| r.get::<_, Value>(0))
        .unwrap_or(Value::Null);

    let messages: i64 = client
        .query_one("SELECT count(*)::bigint FROM hist.messages m JOIN hist.conversations c ON c.id = m.conversation_id WHERE c.platform = 'chatgpt'", &[])
        .await
        .map_err(|e| format!("messages: {e}"))?
        .get(0);

    Ok(json!({
        "app": app_state,
        "appLabel": app_state.tooltip(),
        "runtime": runtime,
        "lastSyncAt": iso(last_ok.and_then(|r| r.get::<_, Option<DateTime<Utc>>>(0))),
        "lastRun": last_run.map(|r| json!({
            "startedAt": iso(r.get::<_, Option<DateTime<Utc>>>(0)),
            "finishedAt": iso(r.get::<_, Option<DateTime<Utc>>>(1)),
            "status": r.get::<_, Option<String>>(2),
            "bodies": r.get::<_, i32>(3),
            "error": r.get::<_, Option<String>>(4),
        })),
        "counts": {
            "total": totals.get::<_, i64>(0),
            "synced": totals.get::<_, i64>(1),
            "stale": totals.get::<_, i64>(2),
            "never": totals.get::<_, i64>(3),
            "syncing": totals.get::<_, i64>(4),
            "managed": totals.get::<_, i64>(5),
            "messages": messages,
        }
    }))
}

#[tauri::command]
async fn list_conversations(
    scope: String,
    limit: i64,
    settings: State<'_, Settings>,
) -> Result<Vec<Value>, String> {
    let client = connect(&settings.dsn).await?;
    // 'managed' hides the pre-baseline history, which is skipped on purpose and
    // would otherwise read as hundreds of failures.
    let sql = "SELECT id::text, title, update_time, body_synced_at, status, scope, message_count
               FROM hist.overview
             WHERE platform = 'chatgpt'
               WHERE ($1 = 'all' OR scope = $1)
               ORDER BY updated_at DESC
               LIMIT $2";
    let rows = client
        .query(sql, &[&scope, &limit])
        .await
        .map_err(|e| format!("overview: {e}"))?;
    Ok(rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<_, String>(0),
                "title": r.get::<_, Option<String>>(1),
                "updateTime": iso(r.get::<_, Option<DateTime<Utc>>>(2)),
                "lastSyncAt": iso(r.get::<_, Option<DateTime<Utc>>>(3)),
                "status": r.get::<_, Option<String>>(4),
                "scope": r.get::<_, Option<String>>(5),
                "messages": r.get::<_, Option<i64>>(6),
            })
        })
        .collect())
}

#[tauri::command]
async fn sync_now(settings: State<'_, Settings>) -> Result<String, String> {
    run_cli(&settings, &["once"]).await
}

#[tauri::command]
async fn sync_conversation(_id: String, settings: State<'_, Settings>) -> Result<String, String> {
    // A single stale row is drained by an ordinary cycle; --limit keeps it short.
    run_cli(&settings, &["once", "--limit", "1"]).await
}

#[tauri::command]
async fn app_start(force: bool, settings: State<'_, Settings>) -> Result<String, String> {
    let mut args = vec!["start"];
    if force {
        args.push("--force");
    }
    run_cli(&settings, &args).await
}

#[tauri::command]
async fn app_stop(settings: State<'_, Settings>) -> Result<String, String> {
    run_cli(&settings, &["stop"]).await
}

fn toggle_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }
}

fn main() {
    tauri::Builder::default()
        .manage(Settings::load())
        .invoke_handler(tauri::generate_handler![
            get_status,
            list_conversations,
            sync_now,
            sync_conversation,
            app_start,
            app_stop
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)?;
            let sync = MenuItem::with_id(app, "sync", "Sync now", true, None::<&str>)?;
            let start = MenuItem::with_id(app, "start", "Start ChatGPT", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &sync, &start, &separator, &quit])?;

            let tray = TrayIconBuilder::with_id("status")
                .icon(Image::from_bytes(AppRun::Off.icon())?)
                .icon_as_template(false)
                .tooltip(AppRun::Off.tooltip())
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    let app = app.clone();
                    match event.id.as_ref() {
                        "open" => toggle_window(&app),
                        "quit" => app.exit(0),
                        id @ ("sync" | "start") => {
                            let id = id.to_string();
                            tauri::async_runtime::spawn(async move {
                                let settings = app.state::<Settings>();
                                let result = if id == "sync" {
                                    run_cli(&settings, &["once"]).await
                                } else {
                                    run_cli(&settings, &["start"]).await
                                };
                                let _ = app.emit("cli-finished", json!({
                                    "action": id,
                                    "ok": result.is_ok(),
                                    "output": result.unwrap_or_else(|e| e),
                                }));
                            });
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // Poll the app state so the colour is live rather than waiting on a
            // daemon cycle; the daemon writes its own state to Postgres.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let port = handle.state::<Settings>().port;
                let mut last: Option<AppRun> = None;
                loop {
                    let state = detect(port).await;
                    if last != Some(state) {
                        if let Ok(icon) = Image::from_bytes(state.icon()) {
                            let _ = tray.set_icon(Some(icon));
                        }
                        let _ = tray.set_tooltip(Some(state.tooltip()));
                        let _ = handle.emit("app-state", json!({ "app": state }));
                        last = Some(state);
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the popover should leave the app in the menu bar.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
