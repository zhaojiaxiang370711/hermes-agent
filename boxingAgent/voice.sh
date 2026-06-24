#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

APP_DIR_DEFAULT="/home/x/code/qxzn02/app"
if [[ ! -d "$APP_DIR_DEFAULT" && -d /home/xwsl/code/qxzn02/app ]]; then
  APP_DIR_DEFAULT="/home/xwsl/code/qxzn02/app"
fi

APP_DIR="${APP_DIR:-$APP_DIR_DEFAULT}"
PORT="${QXZN_VOICE_RUNTIME_PORT:-50058}"
HOST="${QXZN_VOICE_RUNTIME_HOST:-0.0.0.0}"
KEYWORD="${QXZN_VOICE_RUNTIME_KEYWORD:-小星}"
ASR_MODEL_DIR="${QXZN_VOICE_RUNTIME_ASR_MODEL_DIR:-$APP_DIR/services/ai_agent/models/asr/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17}"
ASR_LANGUAGE="${QXZN_VOICE_RUNTIME_ASR_LANGUAGE:-zh}"
RECORD_SECONDS=12
AUTO_TTS=0
TTS_PROVIDER=""
PLAY=0
ASR_ONLY=0
SERVE_ONLY=0
ONCE=0
RELEASE=0
SKIP_BUILD=0
DEVICE=""
BACKEND="auto"
SOURCE=""
LIST_DEVICES=0

usage() {
  cat <<'EOF'
Usage:
  ./voice.sh                 Start voice runtime and continuously listen for wake word.
  ./voice.sh --once          Start voice runtime, run one mic test, then stop.
  ./voice.sh --serve         Start voice runtime and keep it running.

Options:
  --record N                 One-shot seconds, or command seconds after wake (default: 12).
  --once                     Run one-shot mic test instead of continuous wake loop.
  --device ID                sounddevice input device id.
  --backend NAME             Recording backend: auto, parecord, parec, or sounddevice (default: auto).
  --source NAME              Pulse/PipeWire source for parecord/parec.
  --list-devices             List input devices and exit.
  --asr-only                 Skip real wake-word detection and test ASR + dialogue.
  --auto-tts                 Enable automatic TTS for voice replies (defaults to edge).
  --tts PROVIDER             Enable TTS and select provider: edge or volcano.
  --play                     Try to play returned TTS audio in the test script.
  --port PORT                HTTP port (default: 50058).
  --host HOST                Bind host (default: 0.0.0.0).
  --keyword WORD             Wake keyword (default: 小星).
  --asr-model-dir DIR        SenseVoice model directory.
  --asr-language LANG        zh, en, yue, ja, ko, or auto (default: zh).
  --release                  Build/run release binary.
  --skip-build               Use existing binary.
  -h, --help                 Show this help.

Environment:
  HERMES_HOME                Defaults to ~/.hermes.
  APP_DIR                    Defaults to /home/x/code/qxzn02/app when present.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --record)
      RECORD_SECONDS="${2:-}"
      shift
      ;;
    --once)
      ONCE=1
      ;;
    --device)
      DEVICE="${2:-}"
      shift
      ;;
    --backend)
      BACKEND="${2:-}"
      shift
      ;;
    --source)
      SOURCE="${2:-}"
      shift
      ;;
    --list-devices)
      LIST_DEVICES=1
      ;;
    --asr-only)
      ASR_ONLY=1
      ;;
    --auto-tts)
      AUTO_TTS=1
      ;;
    --tts)
      AUTO_TTS=1
      PLAY=1
      TTS_PROVIDER="${2:-}"
      shift
      ;;
    --play)
      PLAY=1
      ;;
    --serve)
      SERVE_ONLY=1
      ;;
    --port)
      PORT="${2:-}"
      shift
      ;;
    --host)
      HOST="${2:-}"
      shift
      ;;
    --keyword)
      KEYWORD="${2:-}"
      shift
      ;;
    --asr-model-dir)
      ASR_MODEL_DIR="${2:-}"
      shift
      ;;
    --asr-language)
      ASR_LANGUAGE="${2:-}"
      shift
      ;;
    --release)
      RELEASE=1
      ;;
    --skip-build)
      SKIP_BUILD=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

if [[ "$LIST_DEVICES" == "1" ]]; then
  python3 scripts/voice_test.py --list-devices
  exit 0
fi

if [[ ! -d "$APP_DIR" ]]; then
  echo "Missing APP_DIR: $APP_DIR" >&2
  exit 1
fi
if [[ ! -d "$ASR_MODEL_DIR" ]]; then
  echo "Missing ASR model dir: $ASR_MODEL_DIR" >&2
  exit 1
fi

if [[ "$AUTO_TTS" == "1" ]]; then
  TTS_PROVIDER="${TTS_PROVIDER:-edge}"
  case "$TTS_PROVIDER" in
    edge|volcano) ;;
    *)
      echo "Unsupported TTS provider: $TTS_PROVIDER (expected edge or volcano)" >&2
      exit 2
      ;;
  esac
  ./run.sh config set voice.auto_tts true >/dev/null
  ./run.sh config set tts.provider "$TTS_PROVIDER" >/dev/null
  if [[ "$TTS_PROVIDER" == "volcano" ]]; then
    if ! grep -Eq '^VOLC_TTS_API_KEY=.+' "${HERMES_HOME:-$HOME/.hermes}/.env" 2>/dev/null; then
      echo "Missing VOLC_TTS_API_KEY in ${HERMES_HOME:-$HOME/.hermes}/.env" >&2
      exit 1
    fi
    if ! ./run.sh config get tts.volcano.voice >/dev/null 2>&1; then
      echo "Missing tts.volcano.voice in config.yaml" >&2
      exit 1
    fi
  fi
else
  ./run.sh config set voice.auto_tts false >/dev/null
fi

build_flags=(--voice-full)
run_flags=(--voice-full)
if [[ "$RELEASE" == "1" ]]; then
  build_flags+=(--release)
  run_flags+=(--release)
fi
if [[ "$SKIP_BUILD" != "1" ]]; then
  ./build.sh "${build_flags[@]}"
else
  run_flags=()
  if [[ "$RELEASE" == "1" ]]; then
    run_flags+=(--release)
  fi
fi

if command -v lsof >/dev/null 2>&1; then
  pids="$(lsof -ti ":$PORT" 2>/dev/null || true)"
  if [[ -n "$pids" ]]; then
    echo "==> stopping existing process on :$PORT"
    kill $pids 2>/dev/null || true
    sleep 0.5
  fi
fi

export APP_DIR
export QXZN_VOICE_RUNTIME_PORT="$PORT"
export QXZN_VOICE_RUNTIME_HOST="$HOST"
export QXZN_VOICE_RUNTIME_KEYWORD="$KEYWORD"
export QXZN_VOICE_RUNTIME_ENGINE=sherpa-onnx
export QXZN_VOICE_RUNTIME_ASR_MODEL_DIR="$ASR_MODEL_DIR"
export QXZN_VOICE_RUNTIME_ASR_LANGUAGE="$ASR_LANGUAGE"
export VOICE_URL="http://127.0.0.1:$PORT"

if [[ "$SERVE_ONLY" == "1" ]]; then
  echo "==> voice runtime serving at $VOICE_URL"
  echo "==> wake word: $KEYWORD"
  exec ./run.sh "${run_flags[@]}" --max-tokens 256 --max-turns 6 voice
fi

cleanup() {
  if [[ -n "${VOICE_PID:-}" ]] && kill -0 "$VOICE_PID" 2>/dev/null; then
    kill "$VOICE_PID" 2>/dev/null || true
    wait "$VOICE_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

echo "==> starting voice runtime at $VOICE_URL"
./run.sh "${run_flags[@]}" --max-tokens 256 --max-turns 6 voice &
VOICE_PID=$!

ready=0
for _ in {1..60}; do
  if ! kill -0 "$VOICE_PID" 2>/dev/null; then
    echo "voice runtime exited early" >&2
    exit 1
  fi
  if curl -fsS "$VOICE_URL/health" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.5
done
if [[ "$ready" != "1" ]]; then
  echo "voice runtime startup timeout" >&2
  exit 1
fi

echo "==> runtime status"
if [[ "$AUTO_TTS" == "1" ]]; then
  echo "==> auto TTS: $TTS_PROVIDER"
fi
if command -v jq >/dev/null 2>&1; then
  curl -sS "$VOICE_URL/api/v1/voice-runtime/status" |
    jq '{healthy, engine, keyword, sample_rate, kws_model_exists, asr_model_configured, asr_language, state: .runtime.state}'
else
  curl -sS "$VOICE_URL/api/v1/voice-runtime/status"
  echo
fi

test_args=(--record "$RECORD_SECONDS")
test_args+=(--backend "$BACKEND")
if [[ -n "$DEVICE" ]]; then
  test_args+=(--device "$DEVICE")
fi
if [[ -n "$SOURCE" ]]; then
  test_args+=(--source "$SOURCE")
fi
if [[ "$ASR_ONLY" == "1" ]]; then
  test_args+=(--asr-only)
fi
if [[ "$ONCE" != "1" && "$ASR_ONLY" != "1" ]]; then
  test_args+=(--continuous --command-seconds "$RECORD_SECONDS")
fi
if [[ "$PLAY" == "1" ]]; then
  test_args+=(--play)
fi

echo "==> starting mic test"
python3 -u scripts/voice_test.py "${test_args[@]}"
