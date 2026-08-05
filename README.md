# gather-codex-logs

Copies all your local Codex conversations into one folder.

## Run it

```bash
git clone -b codex https://github.com/oogxdd/gather-agent-logs.git
cd gather-agent-logs
./collect-codex-logs.sh
```

That's it. The script prints the exact path when it's done.

## Where the files go

By default: into the folder you ran the command from.

```text
./codex-logs-20260805T183000Z/
└── 2026/08/05/rollout-2026-08-05T18-30-00-<id>.jsonl   <- one file per conversation
```

To save somewhere else, pass a folder:

```bash
./collect-codex-logs.sh ~/Desktop
# -> ~/Desktop/codex-logs-20260805T183000Z/
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
