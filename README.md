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

## Install and run

Rust 1.88 or newer is required.

```bash
cargo install --path .
agent-resume
```

To try it without installing:

```bash
cargo run --release
```

The default scan locations are:

- Codex: `$CODEX_HOME/sessions`, otherwise `~/.codex/sessions`
- Claude Code: `$CLAUDE_CONFIG_DIR/projects`, otherwise `~/.claude/projects`

Choose a session and press `Enter`. The picker restores the terminal, changes
to the session's saved working directory when it still exists, and runs either
`codex resume SESSION_ID` or `claude --resume SESSION_ID`.

## Keys

| Key | Action |
| --- | --- |
| `↑` / `k`, `↓` / `j` | Move through sessions |
| `PgUp`, `PgDn`, `g`, `G` | Jump through the list |
| `/` | Fuzzy-search title, project, path, agent, or session ID |
| `Enter` | Keep a search, or resume the selected session |
| `Esc` | Clear an active search; otherwise quit |
| `c` | Clear a kept search |
| `q`, `Ctrl+C` | Quit |

The fuzzy matcher is Unicode-aware, so mixed Russian/English prompts and paths
work as expected.

## CLI options

```text
agent-resume [OPTIONS]

  --agent <all|codex|claude>  Limit the list to one agent
  --home <HOME_DIR>           Scan another home directory
  --codex-dir <SESSIONS_DIR>  Override the Codex sessions directory
  --claude-dir <PROJECTS_DIR> Override the Claude projects directory
  --list                      Print sessions without opening the TUI
```

For example:

```bash
agent-resume --agent codex
agent-resume --list
agent-resume --codex-dir /mnt/old-home/.codex/sessions
```

## How scanning stays light

The program stores only small metadata records in memory. For each JSONL file,
it reads at most the first 512 records and stops as soon as it has the session
ID, working directory, first real user prompt, and the first assistant reply.
Known injected context such as `AGENTS.md`, environment blocks, plugin hints,
and Claude slash-command output is excluded from titles. Claude subagent logs
are excluded from the top-level session list.

The TUI keeps the metadata list but only renders the visible viewport. It does
not load full transcripts and never modifies session files.

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
