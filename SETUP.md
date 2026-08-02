# Ironjaw Setup Guide

## Quick Start

```bash
cargo build
cargo run -- config/ironjaw.toml
# In another terminal:
curl http://127.0.0.1:18794/health
```

## Prerequisites

- Rust 1.70+ (install via rustup)
- llama-server (optional, for LLM/voice/brain features) — runs on port 8080
- SQLite is bundled via sqlx (no external dependency)

## Build

```bash
cargo build              # debug
cargo build --release    # optimized
```

Binary output: `target/release/ironjaw` (or `target/debug/ironjaw`)

## Configuration

Primary config: `config/ironjaw.toml`

### Required sections

- `[server]` — host (default 127.0.0.1), port (default 18794)
- `[auth]` — token (MUST change from default before production use)
- `[audit]` — database_path (default data/ironjaw.db)
- `[logging]` — level (info/debug/trace), format (pretty/json)

### Optional subsystem toggles

Each subsystem has an `enabled` field (default false):

- `[brain]` — RAG memory (requires llama-server for embeddings)
- `[integrations]` — external service tools (search, CC, infra, HA, secrets)
- `[proactive]` — cron scheduler + workflow engine
- `[voice]` — STT/TTS (requires llama-server with Whisper + Orpheus)
- `[vision]` — image processing (max_image_size_bytes, default 10MB)

### Integration endpoints

When `[integrations] enabled = true`, configure:

- searxng_url — SearXNG instance (default http://192.0.2.20:8080)
- perplexica_url — Perplexica instance (default http://192.0.2.20:3001)
- cc_bridge_url — CC Bridge (default http://127.0.0.1:18790)
- cc_chronicle_url — CC Chronicle (default http://127.0.0.1:18793)
- remote_host/remote_user — SSH for infra tools
- ha_url/ha_token — Home Assistant (optional)

### LLM providers

```toml
[llm]
default_model = "qwen3-coder"

[[llm.providers]]
name = "llama-server"
type = "llama-server"
base_url = "http://localhost:8080"
timeout_secs = 300
```

## Running

### Development

```bash
cargo run -- config/ironjaw.toml
```

### Verify health

```bash
curl http://127.0.0.1:18794/health
```

### WebSocket test

```bash
# Using websocat or similar:
websocat ws://127.0.0.1:18794/ws
# Send: {"type":"req","id":"...","method":"auth.connect","params":{"token":"your-token"}}
```

## Testing

```bash
cargo test                          # the whole suite
cargo test -p ironjaw-core          # single crate
cargo test -p ironjaw-ws            # websocket + integration tests
cargo test -p ironjaw-security      # security tests
cargo test -p ironjaw-tools         # tool tests
cargo test -p ironjaw-brain         # brain/RAG tests
cargo test -p ironjaw-proactive     # scheduler + workflow tests
cargo test -p ironjaw-voice         # voice pipeline tests
cargo test -p ironjaw-vision        # image processing tests
cargo test -p ironjaw-integrations  # integration tool tests
```

## Development Tools

```bash
cargo fmt --all                                          # format
cargo fmt --all --check                                  # check only
cargo clippy --workspace --all-targets -- -D warnings    # lint
./hooks/install.sh                                       # install pre-commit hook
```

## Deployment

### Task Scheduler (Windows)

Wrapper scripts in `scripts/`:

- `ironjaw-wrapper.cmd` — sets env, launches binary
- `ironjaw-wrapper-hidden.vbs` — runs wrapper without console window

Register:

```
schtasks /Create /TN "Ironjaw" /SC ONLOGON /TR "wscript.exe scripts\ironjaw-wrapper-hidden.vbs"
```

### Caddy reverse proxy

Point the upstream at `:18794`, or whatever `[server] port` is set to. The gateway
speaks plain HTTP and WebSocket, so the proxy handles TLS and nothing else changes.

## Troubleshooting

### Config parse error

- Check TOML syntax (missing quotes, bracket mismatch)
- All sections except `[server]`, `[auth]`, `[audit]`, `[logging]` are optional with defaults

### Database creation fails

- Ensure `data/` directory exists: `mkdir -p data`
- Check file permissions

### llama-server connection refused

- Verify llama-server is running: `curl http://localhost:8080/health`
- Check base_url in `[llm]` config
- Default timeout is 300s

### Brain embeddings not working

- Set `[brain] enabled = true`
- Ensure llama-server is running with an embedding-capable model
- Default embedding model: `nomic-embed-text`

### Voice not working

- Set `[voice] enabled = true`
- Verify llama-server supports /v1/audio/transcriptions (Whisper)
- Verify llama-server supports /v1/audio/speech (Orpheus/TTS model)
