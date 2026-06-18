# hermes-rs

Faithful Rust port of the Hermes agent **core**, staged per
`docs/superpowers/specs/2026-06-18-hermes-rs-core-rewrite-design.md`.

**Phase 1a + 1b (this code):** workspace scaffold, config interop, shared
state read, and Phase-1 LLM providers.

## Layout
- `crates/hermes-config` — `~/.hermes/config.yaml` read/write (order- and unknown-key-preserving) + `.env` key reader + `state_db_path()`
- `crates/hermes-state` — reads the shared `~/.hermes/state.db` (sessions, read-only)
- `crates/hermes-providers` — `Provider` trait + OpenAI-compatible + Anthropic clients (1-shot + SSE), config/.env resolver
- `crates/hermes-cli` — `hermes-rs` binary
- `crates/hermes-{tools,core}` — stubs (Phase 2)

## Build & run
```
cargo build
cargo run -- --version
cargo run -- config list                 # top-level keys of ~/.hermes/config.yaml
cargo run -- config get agent.max_turns
cargo run -- config set agent.max_turns 30
```

Config path mirrors the Python original: `$HERMES_HOME` if set, else `$HOME/.hermes`.

## Tests
```
cargo test
```
Provider tests use `wiremock` for deterministic request-construction + SSE-parse
checks — **no real API calls**. Live reads of `state.db` and live provider
round-trips are manual smoke only.

## Status
- `config get/set/list`: implemented, round-trips the real config.
- `state`: read-only over the shared `~/.hermes/state.db` (session count + summaries).
- `providers`: OpenAI-compatible (`{base_url}/chat/completions`) + Anthropic
  (`{base_url}/v1/messages`) — 1-shot `complete` and SSE `stream`. API keys
  resolved from `providers.<k>.key_env` → `~/.hermes/.env`.
- `chat`, `model`: stubs — Phase 2 (agent loop + default tools + state writes).
