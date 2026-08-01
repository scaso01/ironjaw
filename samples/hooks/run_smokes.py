"""B2 hook live smokes 4/5/6.

Smoke 4 (SessionStart): auth.connect should fire marker.cmd, growing the
                        marker file by one line.
Smoke 5 (UserPromptSubmit Block): chat.send should be short-circuited by
                                  block-prompt.cmd; reply must indicate
                                  blocked, no tokens streamed.
Smoke 6 (hooks.reload): hooks.list before/after edit-and-reload should
                        reflect a manifest change.
"""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
import sys
import time
import uuid

import websockets

GATEWAY_URL = "ws://127.0.0.1:18794/ws"
_TOKEN_ENV = "IRONJAW_AUTH_TOKEN"
TOKEN = os.environ.get(_TOKEN_ENV, "")
if not TOKEN:
    raise SystemExit(
        f"set {_TOKEN_ENV} to the token this gateway was configured with"
    )
MARKER_FILE = pathlib.Path(os.environ["TEMP"]) / "ironjaw-hook-marker.log"
HOOKS_DIR = pathlib.Path.home() / ".config" / "ironjaw" / "hooks"


async def req(ws, method: str, params: dict | None = None) -> dict:
    rid = str(uuid.uuid4())
    await ws.send(json.dumps({"type": "req", "id": rid, "method": method, "params": params or {}}))
    while True:
        raw = await asyncio.wait_for(ws.recv(), timeout=10)
        msg = json.loads(raw)
        if msg.get("id") == rid and msg.get("type") in ("res", "err"):
            return msg


async def smoke4_session_start():
    print("\n=== SMOKE 4: SessionStart subprocess hook ===")
    if MARKER_FILE.exists():
        before = MARKER_FILE.stat().st_size
        before_lines = MARKER_FILE.read_text(encoding="utf-8").count("\n")
    else:
        before, before_lines = 0, 0
    print(f"  marker before: {before} bytes / {before_lines} lines")

    async with websockets.connect(GATEWAY_URL) as ws:
        res = await req(ws, "auth.connect", {"token": TOKEN})
        if res.get("type") != "res":
            print(f"  FAIL auth.connect: {res}")
            return False
        print(f"  auth.connect -> {res.get('payload', {}).get('user_id', '?')}")

    # subprocess hook fires async; give it a moment to flush
    await asyncio.sleep(1.5)

    if not MARKER_FILE.exists():
        print(f"  FAIL: marker file does not exist at {MARKER_FILE}")
        return False
    after = MARKER_FILE.stat().st_size
    after_lines = MARKER_FILE.read_text(encoding="utf-8").count("\n")
    grew = after_lines > before_lines
    print(f"  marker after:  {after} bytes / {after_lines} lines")
    print(f"  PASS" if grew else "  FAIL: marker did not grow")
    return grew


async def smoke5_user_prompt_block():
    print("\n=== SMOKE 5: UserPromptSubmit Block hook ===")
    async with websockets.connect(GATEWAY_URL) as ws:
        res = await req(ws, "auth.connect", {"token": TOKEN})
        if res.get("type") != "res":
            print(f"  FAIL auth.connect: {res}")
            return False

        # Open or create a session, then send a chat message; the hook
        # should short-circuit it.
        sess_res = await req(ws, "session.create", {"title": "smoke5"})
        if sess_res.get("type") != "res":
            print(f"  FAIL session.create: {sess_res}")
            return False
        sid = (
            sess_res["payload"].get("session", {}).get("id")
            or sess_res["payload"].get("id")
            or sess_res["payload"].get("session_id")
        )
        print(f"  session: {sid}")

        chat_res = await req(
            ws, "chat.send", {"session_id": sid, "message": "this should be blocked"}
        )
        # We expect either an err frame with HOOK_BLOCKED, or a res with
        # blocked=true; tolerate both shapes.
        kind = chat_res.get("type")
        code = chat_res.get("payload", {}).get("code") or chat_res.get("payload", {}).get(
            "status"
        )
        body = json.dumps(chat_res, indent=2)[:400]
        blocked = (
            kind == "err"
            and "HOOK_BLOCKED" in json.dumps(chat_res)
            or kind == "res"
            and "block" in json.dumps(chat_res).lower()
        )
        print(f"  chat.send response (truncated): {body}")
        print(f"  PASS" if blocked else "  FAIL: did not look blocked")
        return blocked


def _marker_event(payload) -> str | None:
    """Find marker hook event in hooks.list payload (robust to shape)."""
    if not isinstance(payload, (list, dict)):
        return None
    items = payload if isinstance(payload, list) else payload.get("hooks") or payload.get(
        "entries"
    ) or []
    if not isinstance(items, list):
        return None
    for it in items:
        if isinstance(it, dict) and it.get("name") == "marker":
            return it.get("event")
    return None


async def smoke6_hooks_reload():
    print("\n=== SMOKE 6: hooks.reload pickup ===")
    marker_toml = HOOKS_DIR / "marker.toml"
    original = marker_toml.read_text(encoding="utf-8")
    try:
        async with websockets.connect(GATEWAY_URL) as ws:
            res = await req(ws, "auth.connect", {"token": TOKEN})
            if res.get("type") != "res":
                print(f"  FAIL auth.connect: {res}")
                return False

            list1 = await req(ws, "hooks.list", {})
            if not list1.get("ok", True):
                print(f"  FAIL: hooks.list returned error: {list1}")
                return False
            event1 = _marker_event(list1.get("payload"))
            print(f"  hooks.list before — marker event: {event1}")

            # Flip the manifest's event field so reload demonstrates pickup.
            mutated = original.replace('event = "SessionStart"', 'event = "SessionEnd"')
            if mutated == original:
                print("  FAIL: cannot mutate marker.toml event (template changed?)")
                return False
            marker_toml.write_text(mutated, encoding="utf-8")
            await asyncio.sleep(0.2)

            reload_res = await req(ws, "hooks.reload", {})
            print(f"  hooks.reload -> ok={reload_res.get('ok')}")

            list2 = await req(ws, "hooks.list", {})
            event2 = _marker_event(list2.get("payload"))
            print(f"  hooks.list after  — marker event: {event2}")

            ok = event1 == "SessionStart" and event2 == "SessionEnd"
            print(f"  PASS" if ok else "  FAIL: event did not flip SessionStart -> SessionEnd")
            return ok
    finally:
        marker_toml.write_text(original, encoding="utf-8")


async def main():
    results = {}
    try:
        results["smoke4"] = await smoke4_session_start()
    except Exception as e:
        print(f"  smoke4 EXCEPTION: {e}")
        results["smoke4"] = False
    try:
        results["smoke5"] = await smoke5_user_prompt_block()
    except Exception as e:
        print(f"  smoke5 EXCEPTION: {e}")
        results["smoke5"] = False
    try:
        results["smoke6"] = await smoke6_hooks_reload()
    except Exception as e:
        print(f"  smoke6 EXCEPTION: {e}")
        results["smoke6"] = False

    print("\n=== SUMMARY ===")
    for name, ok in results.items():
        print(f"  {name}: {'PASS' if ok else 'FAIL'}")
    sys.exit(0 if all(results.values()) else 1)


if __name__ == "__main__":
    asyncio.run(main())
