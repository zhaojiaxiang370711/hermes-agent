#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

usage() {
  cat <<'EOF'
Usage:
  ./run.sh [options] "prompt..."
  ./run.sh [options] chat "prompt..."
  ./run.sh [options] config list
  ./run.sh [options] mcp list

Options:
  --release              Use target/release/boxing-agent.
  --build                Build before running.
  --voice-full           Build/run with sherpa wake-word + ASR support.
  --model MODEL          Forward model override to boxing-agent.
  --provider PROVIDER    Forward provider override to boxing-agent.
  --max-turns N          Forward max turn limit.
  --max-tokens N         Forward max token limit.
  --system PROMPT        Forward system prompt override.
  -h, --help             Show this help.

Environment:
  HERMES_HOME            Defaults to ~/.hermes when unset.
EOF
}

profile="debug"
build_first=0
global_args=()
build_args_extra=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --release)
      profile="release"
      ;;
    --build)
      build_first=1
      ;;
    --voice-full)
      build_first=1
      build_args_extra+=(--voice-full)
      ;;
    --model|--provider|--max-turns|--max-tokens|--system)
      if [[ $# -lt 2 ]]; then
        echo "Missing value for $1" >&2
        exit 2
      fi
      global_args+=("$1" "$2")
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    *)
      break
      ;;
  esac
  shift
done

if [[ "$profile" == "release" ]]; then
  bin="$ROOT_DIR/target/release/boxing-agent"
  build_args=(--release "${build_args_extra[@]}")
else
  bin="$ROOT_DIR/target/debug/boxing-agent"
  build_args=("${build_args_extra[@]}")
fi

if [[ "$build_first" -eq 1 || ! -x "$bin" ]]; then
  "$ROOT_DIR/build.sh" "${build_args[@]}"
fi

export HERMES_HOME="${HERMES_HOME:-$HOME/.hermes}"

if [[ ! -f "$HERMES_HOME/config.yaml" ]]; then
  echo "Missing config: $HERMES_HOME/config.yaml" >&2
  echo "Run Hermes setup first, or set HERMES_HOME to an existing Hermes profile." >&2
  exit 1
fi

if [[ ! -f "$HERMES_HOME/.env" ]]; then
  echo "Warning: $HERMES_HOME/.env not found; provider API key lookup may fail." >&2
fi

if [[ $# -eq 0 ]]; then
  usage >&2
  echo >&2
  echo "Example: ./run.sh \"请只回答：boxingAgent OK\"" >&2
  exit 2
fi

case "$1" in
  chat|config|mcp|acp|voice|cron|model|help)
    exec "$bin" "${global_args[@]}" "$@"
    ;;
  *)
    exec "$bin" "${global_args[@]}" chat "$@"
    ;;
esac
