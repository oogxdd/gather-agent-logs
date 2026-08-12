# agent-resume

`agent-resume` is a fast terminal picker for returning to local Codex and
Claude Code sessions. It finds both agents' native JSONL logs, shows the most
recent work first, and hands the selected session back to the original CLI.

```text
┌ agent-resume ─────────────────────────────────────── 7 of 7 ┐
│ Press / to search title, project, path, or session ID       │
└──────────────────────────────────────────────────────────────┘
┌ Sessions ─────────────────────────┬ Details ────────────────┐
│ Agent  Updated  Project   Prompt  │ Agent   Codex           │
│›Codex  now      backend   fix...  │ Project /work/backend   │
│ Claude 2h ago   web       add...  │ ID      019f...         │
└───────────────────────────────────┴─────────────────────────┘
```

## Quick start

Requirements:

- Rust 1.88 or newer
- Codex, Claude Code, or both installed and available on `PATH`

From the repository root, launch the picker without installing it:

```bash
cargo run --release
```

To install it as a regular command:

```bash
cargo install --locked --path .
agent-resume
```

Cargo normally installs the binary into `~/.cargo/bin`. Add that directory to
`PATH` if your shell cannot find `agent-resume` after installation.

The default scan locations are:

- Codex: `$CODEX_HOME/sessions`, otherwise `~/.codex/sessions`
- Claude Code: `$CLAUDE_CONFIG_DIR/projects`, otherwise `~/.claude/projects`

Choose a session and press `Enter`, including directly from search. The picker
restores the terminal, changes to the session's saved working directory, and
runs either `codex resume SESSION_ID` or `claude --resume SESSION_ID`. On Unix,
the native agent replaces `agent-resume`, so no wrapper process remains.

Session files are read-only. If a session's saved working directory no longer
exists, the picker stops with an error instead of resuming it in the wrong
project.

## Keys

| Key | Action |
| --- | --- |
| `↑` / `k`, `↓` / `j` | Move or scroll the focused pane |
| `gg`, `G` | Jump to the start or end |
| `Ctrl+U`, `Ctrl+D` | Move half a page |
| `Ctrl+B`, `Ctrl+F`, `PgUp`, `PgDn` | Move a full page |
| `H`, `M`, `L` | Select the top, middle, or bottom visible session |
| `Ctrl+W`, `Tab`, `h` / `l` | Switch between sessions and details |
| `s` | Toggle sorting by last update or creation time |
| `/` | Fuzzy-search; `Tab` keeps the filter for navigation |
| `Enter` | Resume the selected session, including while searching |
| `Esc` | Clear an active search; otherwise quit |
| `c` | Clear a kept search |
| `q`, `Ctrl+C` | Quit |

The fuzzy matcher is Unicode-aware, so mixed Russian/English prompts and paths
work as expected.

## CLI options

```text
agent-resume [OPTIONS]

  --agent <all|codex|claude>  Limit the list to one agent
  --sort <updated|created>     Choose the initial sort order
  --home <HOME_DIR>           Scan another home directory
  --codex-dir <SESSIONS_DIR>  Override the Codex sessions directory
  --claude-dir <PROJECTS_DIR> Override the Claude projects directory
  --list                      Print sessions without opening the TUI
```

For example:

```bash
agent-resume --agent codex
agent-resume --sort created
agent-resume --list
agent-resume --codex-dir /mnt/old-home/.codex/sessions
```

## How scanning stays light

The program stores only small metadata records in memory. It streams each JSONL
file once to find the session creation timestamp and the timestamp of the last
real user or assistant message. The transcript itself is never retained.
Known injected context such as `AGENTS.md`, environment blocks, plugin hints,
and Claude slash-command output is excluded from titles. Claude subagent logs
are excluded from the top-level session list. File modification time is used
only as a fallback for old or incomplete logs without message timestamps.

The TUI keeps the metadata list but only renders the visible viewport. It does
not load full transcripts and never modifies session files.

## Troubleshooting

Check what the scanner sees without opening the TUI:

```bash
agent-resume --list
```

Plain output is tab-separated: agent, creation time, last-message time, session
ID, working directory, and title. `--sort` applies to this output too.

If no sessions are found, confirm the agent-specific directory and override it
when necessary:

```bash
agent-resume --codex-dir ~/.codex/sessions
agent-resume --claude-dir ~/.claude/projects
```

If selecting a session reports that `codex` or `claude` cannot be launched,
run the corresponding command directly to confirm it is installed and on
`PATH`.

## Development

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## Snapshot collector

The repository still contains the original portable conversation collector.
It copies local Claude Code, Codex, and Crush logs into a timestamped private
snapshot without credentials or agent configuration:

```bash
./collect-agent-conversations.sh DESTINATION_ROOT
```

To read from a different home directory:

```bash
./collect-agent-conversations.sh DESTINATION_ROOT --home /home/username
```

Snapshots contain source code, commands, prompts, and local paths. Keep them
private. See the script's `--help` output for its exact layout and behavior.
