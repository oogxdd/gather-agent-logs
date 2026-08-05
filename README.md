# gather-codex-logs

Copies your local Codex conversations into one folder.

## Start small

Grab the last 10 conversations first, see what you get:

```bash
git clone -b codex https://github.com/oogxdd/gather-agent-logs.git
cd gather-agent-logs
./collect-codex-logs.sh -n 10
```

The script prints the exact path when it's done:

```text
Copied 10 of 137 conversations (48M)
Saved to: /Users/you/gather-agent-logs/codex-logs-20260805T183000Z
```

Open that folder, look at a file, and if it's what you want — run it again
without `-n` to get everything.

## Where the files go

By default: into the folder you ran the command from.

```text
./codex-logs-20260805T183000Z/
└── 2026/08/05/rollout-2026-08-05T18-30-00-<id>.jsonl   <- one file per conversation
```

To save somewhere else, pass a folder:

```bash
./collect-codex-logs.sh ~/Desktop -n 10
# -> ~/Desktop/codex-logs-20260805T183000Z/
```

## All the options

```text
./collect-codex-logs.sh [FOLDER] [-n N]

  FOLDER          where to save (default: current directory)
  -n, --limit N   copy only the N most recent conversations
  -h, --help      show this message
```

## Where it reads from

`~/.codex/sessions` (or `$CODEX_HOME/sessions` if you set that variable).

Nothing else is read — no config, no API keys, no auth files. Your original
logs stay exactly where they are; this only copies them.

## Requirements

`bash` and `python3`. macOS and Linux both ship with these.

## Note

The copied files contain your full conversations: source code, file paths,
shell commands. Keep them private.
