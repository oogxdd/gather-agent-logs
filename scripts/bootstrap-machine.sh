#!/usr/bin/env bash

# Prepares one machine to report its agent sessions: installs the binaries,
# stores the database URL, installs the agent hooks, and uploads what is
# already on disk. Safe to re-run.

set -Eeuo pipefail

REPOSITORY_URL=${AGENT_LOGS_REPO:-https://github.com/oogxdd/gather-agent-logs}

usage() {
  cat <<'EOF'
Usage: bootstrap-machine.sh [OPTIONS]

  --database-url URL  Postgres connection string (default: $AGENT_LOGS_DATABASE_URL)
  --machine NAME      Name to store this machine's sessions under (default: hostname)
  --repo URL          Install from this repository instead of the default
  --no-hooks          Skip installing the agent hooks
  --no-ship           Skip the first upload
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

database_url=${AGENT_LOGS_DATABASE_URL:-}
machine=""
install_hooks=1
ship=1

while [[ $# -gt 0 ]]; do
  case $1 in
    --database-url) [[ $# -ge 2 ]] || die '--database-url needs a value'; database_url=$2; shift 2 ;;
    --machine) [[ $# -ge 2 ]] || die '--machine needs a value'; machine=$2; shift 2 ;;
    --repo) [[ $# -ge 2 ]] || die '--repo needs a value'; REPOSITORY_URL=$2; shift 2 ;;
    --no-hooks) install_hooks=0; shift ;;
    --no-ship) ship=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n $database_url ]] || die 'no database URL; pass --database-url or set AGENT_LOGS_DATABASE_URL'
command -v cargo >/dev/null 2>&1 || die 'cargo is required; install Rust from https://rustup.rs'

script_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
checkout=${script_directory%/scripts}

if [[ -f $checkout/Cargo.toml ]] && grep -q '^name = "agent-logs"' "$checkout/Cargo.toml"; then
  printf 'Installing from %s\n' "$checkout"
  cargo install --locked --path "$checkout"
else
  printf 'Installing from %s\n' "$REPOSITORY_URL"
  cargo install --locked --git "$REPOSITORY_URL"
fi

export PATH="$HOME/.cargo/bin:$PATH"
command -v agent-logs >/dev/null 2>&1 || die 'agent-logs is not on PATH; add ~/.cargo/bin to it'

setup_arguments=(setup --database-url "$database_url")
[[ -z $machine ]] || setup_arguments+=(--machine "$machine")
agent-logs "${setup_arguments[@]}"

if [[ $install_hooks -eq 1 ]]; then
  agent-logs install-hooks
fi

if [[ $ship -eq 1 ]]; then
  agent-logs ship
fi

printf '\nDone. This machine reports as: %s\n' "${machine:-$(hostname)}"
printf 'Read everything with: agent-resume\n'
