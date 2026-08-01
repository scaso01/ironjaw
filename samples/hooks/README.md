# Sample hooks (B2)

Reference artifacts for the Wave B2 hook framework. Drop selected files into
`~/.config/ironjaw/hooks/` and flip `[hooks] enabled = true` (plus
`[hooks.subprocess].enabled = true` and a non-empty `enabled_events` list for
subprocess hooks) in `config/ironjaw.toml`. Discovery runs at startup and on
`hooks.reload`.

## Layout convention

Each hook is a *pair* in the hooks directory:

| Pattern | Meaning |
|---------|---------|
| `<name>.wasm` + `<name>.toml` | WASM hook (default sandboxed path) |
| `<name>.{cmd,bat,ps1,sh,py,exe}` + `<name>.toml` | Subprocess hook (escape hatch) |

The manifest's `event` field is parsed against `HookEventKind` via `FromStr`.
Valid event names:

- `PreChat`, `PostChat`
- `PreToolExec`, `PostToolExec`
- `SessionStart`, `SessionEnd`
- `MemoryStore`, `MemoryRecall`
- `PreCompact` (B2 new)
- `UserPromptSubmit` (B2 new)
- `SkillActivated`, `SkillDeactivated` (B2 new, A15)

## Subprocess result protocol

Stdin: HookContext serialized as JSON
`{"event":"...","session_id":"...","data":{...},"timestamp":"..."}`.

Stdout (when exit code = 0):

| Stdout | Resulting `HookAction` |
|--------|------------------------|
| empty | `Continue` |
| `{"action":"continue"}` | `Continue` |
| `{"action":"block","reason":"..."}` | `Block(reason)` |
| `{"action":"modify","value":<any>}` | `Modify(value)` |
| anything else | `Continue` (unknown JSON ignored) |

Non-zero exit → `Block` with stderr (or stdout) snippet.

Stdout is capped at 16 KB. Per-hook timeout defaults to 5 s, clamped to the
ceiling configured in `[hooks.subprocess].timeout_ceiling_secs` (60 s default).
Env is scrubbed via `crates/ironjaw-tools/src/builtin/shell.rs` `sanitize_env`.

## WASM ABI (summary)

Plugin exports `on_event` (override via manifest's `entry` field) — takes no
args, returns one i32:

| Return | Action |
|--------|--------|
| `0` | `Continue` |
| `1` | `Block` |
| `2` | `Modify` |

For Block/Modify, emit one `host_log` line carrying the reason or
modify-payload JSON before returning. Drained log buffer is capped at
16 KB. `host_get_config(key)` returns presence only (0/1) — to inspect
HookContext fields, query each key by name (e.g. `event`, `session_id`,
`force_fresh`).

A working WASM artifact is **not bundled** here — building one needs a
`wasm32-unknown-unknown` Rust crate. This will be added in a follow-up.
For now, `WasmHook::new` and the host-import surface are exercised by the
unit tests in `crates/ironjaw-ws/src/hooks/wasm_bridge.rs`.

## Bundled samples

### `marker.cmd` + `marker.toml`

Subprocess hook bound to `SessionStart`. Appends a timestamped line to
`%TEMP%\ironjaw-hook-marker.log` on every invocation. Exit 0 + empty stdout
→ `Continue`. Used for Smoke Test 5 (subprocess-fires-on-event).

To enable:

```toml
[hooks]
enabled = true
dir = "~/.config/ironjaw/hooks"

[hooks.subprocess]
enabled = true
default_timeout_secs = 5
timeout_ceiling_secs = 60
enabled_events = ["SessionStart"]
```

Drop both files into `~/.config/ironjaw/hooks/`, restart Ironjaw, then send
an `auth.connect` frame from any client. The marker file should grow by one
line. With `enabled_events = []`, the hook is discovered but **not**
registered — the catalog entry will show `enabled = false` with a
`skip_reason`.

### `block-prompt.cmd` + `block-prompt.toml`

Subprocess hook bound to `UserPromptSubmit` that always returns
`{"action":"block","reason":"..."}`. Used for Smoke Test 6
(Block-short-circuits-chat.send). With this hook registered and
`enabled_events = ["UserPromptSubmit"]`, every `chat.send` should reply
with a `HOOK_BLOCKED` error frame; no LLM call should be made and no user
message should land in `chat.history`.

## Negative test artifact

To validate path-traversal rejection at startup (Smoke Test 8), create a
manifest whose stem looks like a traversal attempt:

```bash
# This SHOULD fail at SubprocessHook::new and be logged as a discovery skip,
# not crash startup or load the file.
touch ~/.config/ironjaw/hooks/..evil
```

The hook constructor rejects names containing `..`, `/`, or `\`. Eight
unit tests in `subprocess.rs` and `wasm_bridge.rs` already cover this; the
live test merely confirms the discovery loop logs and continues.
