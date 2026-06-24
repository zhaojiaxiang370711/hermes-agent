#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

usage() {
  cat <<'EOF'
Usage:
  ./build.sh [--release] [--test] [--check] [--voice-full]

Options:
  --release   Build optimized release binaries.
  --test      Run cargo test after building.
  --check     Run cargo check instead of cargo build.
  --voice-full
              Enable sherpa wake-word + ASR support for boxing-agent voice.
  -h, --help  Show this help.
EOF
}

profile="debug"
run_tests=0
check_only=0
features=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --release)
      profile="release"
      ;;
    --test)
      run_tests=1
      ;;
    --check)
      check_only=1
      ;;
    --voice-full)
      features+=(voice-full)
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

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found. Install Rust first: https://rustup.rs/" >&2
  exit 1
fi

echo "==> boxingAgent build"
echo "    root: $ROOT_DIR"
echo "    rust: $(rustc --version)"
echo "    cargo: $(cargo --version)"

feature_args=()
if [[ "${#features[@]}" -gt 0 ]]; then
  joined="$(IFS=,; echo "${features[*]}")"
  feature_args=(--features "$joined")
  echo "    features: $joined"
fi

if [[ "$check_only" -eq 1 ]]; then
  echo "==> cargo check --workspace ${feature_args[*]}"
  cargo check --workspace "${feature_args[@]}"
else
  if [[ "$profile" == "release" ]]; then
    echo "==> cargo build --workspace --release ${feature_args[*]}"
    cargo build --workspace --release "${feature_args[@]}"
    bin="$ROOT_DIR/target/release/boxing-agent"
  else
    echo "==> cargo build --workspace ${feature_args[*]}"
    cargo build --workspace "${feature_args[@]}"
    bin="$ROOT_DIR/target/debug/boxing-agent"
  fi

  if [[ -x "$bin" ]]; then
    echo "==> binary: $bin"
  fi
fi

if [[ "$run_tests" -eq 1 ]]; then
  echo "==> cargo test --workspace ${feature_args[*]}"
  cargo test --workspace "${feature_args[@]}"
fi

echo "==> done"
