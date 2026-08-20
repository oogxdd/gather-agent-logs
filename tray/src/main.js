const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
let scope = "managed";
let busy = false;

const STATE_TEXT = {
  ready: "ChatGPT running",
  noport: "Running without debug port",
  off: "ChatGPT not running",
};

function ago(iso) {
  if (!iso) return "never";
  const seconds = Math.max(0, (Date.now() - new Date(iso)) / 1000);
  if (seconds < 60) return "just now";
  const steps = [
    [60, "m", 60],
    [3600, "h", 3600],
    [86400, "d", 86400],
  ];
  for (const [limit, unit, divisor] of steps) {
    if (seconds < limit * 60 || unit === "d") {
      return `${Math.floor(seconds / divisor)}${unit} ago`;
    }
  }
  return new Date(iso).toLocaleDateString();
}

function setBusy(on, note = "") {
  busy = on;
  $("sync").disabled = on;
  $("busy").hidden = !on;
  $("action-note").textContent = note;
}

async function refresh() {
  let status;
  try {
    status = await invoke("get_status");
  } catch (error) {
    $("state-label").textContent = String(error);
    $("state-dot").className = "dot noport";
    return;
  }

  const app = status.app;
  $("state-dot").className = `dot ${app}`;
  $("state-label").textContent = STATE_TEXT[app] || app;

  const fix = $("fix");
  fix.hidden = app === "ready" || busy;
  fix.textContent = app === "noport" ? "Restart" : "Start";

  $("last-sync").textContent = `last sync ${ago(status.lastSyncAt)}`;

  const counts = status.counts;
  $("c-synced").textContent = counts.synced;
  $("c-stale").textContent = counts.stale;
  $("c-never").textContent = counts.never;
  $("c-syncing").textContent = counts.syncing;

  const history = counts.total - counts.managed;
  $("scope-note").textContent = history
    ? `${history} older skipped`
    : `${counts.total} total`;

  const run = status.lastRun;
  $("footer-note").textContent =
    run && run.status === "error"
      ? `last run failed: ${(run.error || "").slice(0, 70)}`
      : `${counts.total} conversations · ${counts.messages} messages`;

  await renderList();
}

async function renderList() {
  const rows = await invoke("list_conversations", { scope, limit: 200 });
  const list = $("list");
  list.textContent = "";
  $("empty").hidden = rows.length > 0;

  for (const row of rows) {
    const item = document.createElement("li");

    const dot = document.createElement("span");
    dot.className = `status s-${row.status}`;
    dot.title = row.status;

    const title = document.createElement("span");
    title.className = "title";
    title.textContent = row.title || "(untitled)";

    const meta = document.createElement("span");
    meta.className = "meta";
    meta.textContent =
      row.status === "syncing"
        ? "checking…"
        : row.status === "never"
          ? ago(row.updateTime)
          : `synced ${ago(row.lastSyncAt)}`;
    meta.title = `updated ${row.updateTime || "?"}\nlast sync ${row.lastSyncAt || "never"}`;

    item.append(dot, title, meta);
    list.append(item);
  }
}

async function withBusy(note, fn) {
  setBusy(true, note);
  try {
    await fn();
  } catch (error) {
    $("action-note").textContent = String(error).slice(0, 90);
  } finally {
    setBusy(false, $("action-note").textContent);
    await refresh();
  }
}

$("sync").addEventListener("click", () =>
  withBusy("syncing…", async () => {
    await invoke("sync_now");
    $("action-note").textContent = "";
  }),
);

$("fix").addEventListener("click", () =>
  withBusy("starting ChatGPT…", async () => {
    await invoke("app_start", { force: $("fix").textContent === "Restart" });
    $("action-note").textContent = "";
  }),
);

for (const button of document.querySelectorAll(".scope button")) {
  button.addEventListener("click", () => {
    scope = button.dataset.scope;
    document
      .querySelectorAll(".scope button")
      .forEach((b) => b.classList.toggle("on", b === button));
    renderList();
  });
}

listen("app-state", refresh);
listen("cli-finished", refresh);

refresh();
setInterval(() => {
  if (!busy) refresh();
}, 5000);
