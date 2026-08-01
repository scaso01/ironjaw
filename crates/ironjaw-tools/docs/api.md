# Ironjaw API Reference

## WebSocket Protocol

Connect to `/ws` and send JSON frames.

### Authentication

Send `auth.connect` with a valid token:
```json
{"type": "req", "id": "1", "method": "auth.connect", "params": {"token": "..."}}
```

### Chat

Send messages with `chat.send`:
```json
{"type": "req", "id": "2", "method": "chat.send", "params": {"message": "Hello"}}
```

Responses stream as `stream` frames with `delta` fields.

### Tools

List tools with `tools.list`, execute with `tools.execute`.
Each tool has a permission tier that determines approval flow.

### Brain

Store memories with `brain.remember`, search with `brain.recall`.
Memories are automatically retrieved during chat via hybrid search.

## REST Endpoints

- `GET /health` - Health check
- `POST /v1/chat/completions` - OpenAI-compatible chat
- `GET /v1/models` - List models
- `GET /api/dashboard/*` - Dashboard API
- `GET /api/mobile/*` - Mobile API
