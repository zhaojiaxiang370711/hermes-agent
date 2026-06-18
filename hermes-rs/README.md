# hermes-rs

Faithful Rust port of the Hermes agent **core**, staged per
`docs/superpowers/specs/2026-06-18-hermes-rs-core-rewrite-design.md`.

**Phase 1a (this code):** workspace scaffold + config interop.

## Layout
- `crates/hermes-config` — `~/.hermes/config.yaml` read/write (order- and unknown-key-preserving)
- `crates/hermes-cli` — `hermes-rs` binary
- `crates/hermes-{state,providers,tools,core}` — stubs (Phase 1b)

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

## Status
- `config get/set/list`: implemented, round-trips the real config.
- `chat`, `model`: stubs — implemented in Phase 1b (agent loop + providers).
