# Ironjaw Overview

Ironjaw is a Rust WebSocket gateway for the Dr. CLAW AI assistant platform.
It replaces OpenClaw Gateway, CC Bridge, and CC Chronicle with a single binary.

## Key Features

- Protocol v4 with 8 frame types and 38 methods
- 4-tier permission system (Auto, Notify, Confirm, MultiConfirm)
- LLM routing with complexity scoring
- RAG memory with hybrid search
- Proactive scheduling with cron and workflow DAGs
- Voice pipeline (STT/TTS)
- Multi-device support with offline queue

## Architecture

The project is organized as a Cargo workspace with 14 crates:
ironjaw-core, ironjaw-ws, ironjaw-security, ironjaw-audit,
ironjaw-llm, ironjaw-session, ironjaw-tools, ironjaw-brain,
ironjaw-integrations, ironjaw-proactive, ironjaw-voice,
ironjaw-vision, ironjaw-mcp, ironjaw-cli.
