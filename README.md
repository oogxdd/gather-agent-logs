# gather-agent-logs

Collect local Claude Code, Codex, and Crush conversations into a portable
snapshot. The output contains one JSONL file per conversation and excludes
credentials, agent configuration, prompt-history indexes, and the Crush SQLite
database.

## Usage

```bash
./collect-agent-conversations.sh DESTINATION_ROOT
```

The command creates a timestamped directory:

```text
DESTINATION_ROOT/
└── agent-conversations-YYYYMMDDTHHMMSSZ/
    ├── claude/        # One native JSONL event log per conversation
    ├── codex/         # One native JSONL rollout per conversation
    ├── crush/         # One exported JSONL file per session
    ├── manifest.txt
    └── checksums.sha256
```

To collect histories from a home directory other than the current user's:

```bash
./collect-agent-conversations.sh DESTINATION_ROOT --home /home/username
```

Python 3 and standard GNU/Linux command-line tools are required. Output files
are created with private permissions because conversations can contain source
code, commands, paths, and other sensitive information.

## Collecting from multiple Sprites

Keep the collector itself on `main`. Put each Sprite's exported snapshot on a
separate branch, then push that branch explicitly. A simple flow on another
Sprite is:

```bash
git clone git@github.com:oogxdd/gather-agent-logs.git
cd gather-agent-logs
git switch -c conversations/MY-SPRITE
./collect-agent-conversations.sh snapshots
git add snapshots
git commit -m "Add MY-SPRITE conversations"
git push -u origin conversations/MY-SPRITE
```

Do not merge a conversations branch into `main` unless you intentionally want
the private logs in the default branch.
