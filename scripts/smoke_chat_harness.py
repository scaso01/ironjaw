"""
Smoke harness for Ironjaw chat.send agentic loop.

Verifies tool selection, agentic chains, HITL approval, and error recovery
by driving the same chat.send method that both the dashboard and Dr. CLAW use.

Usage:
    python smoke_chat_harness.py [--prompt N]   # run only test index N
    python smoke_chat_harness.py                # run all tests
    python smoke_chat_harness.py --suite slice2 # run a named suite
"""
import argparse
import asyncio
import json
import os
import sys
import time
import uuid
from typing import Any

import websockets

GATEWAY_URL = "ws://127.0.0.1:18794/ws"
_TOKEN_ENV = "IRONJAW_AUTH_TOKEN"
AUTH_TOKEN = os.environ.get(_TOKEN_ENV, "")
if not AUTH_TOKEN:
    raise SystemExit(
        f"set {_TOKEN_ENV} to the token this gateway was configured with"
    )
# Tests --------------------------------------------------------------------
# permission_policy: maps method/tool to "approve" | "deny". Default "approve" for everything.
TESTS = [
    # ---- Tier 2/3/4 baseline (already passing) ----
    {
        "name": "T1_local_count",
        "suite": "baseline",
        "prompt": "How many folders are in C:/Users/deploy/Projects ?",
        "expect_tools": ["list_files"],
        "forbid_tools": ["monster_search", "web_search", "searxng"],
        "expect_in_output": ["folders"],
    },
    {
        "name": "T2_specific_project_listing",
        "suite": "baseline",
        "prompt": "List the files in the JobHunter project.",
        "expect_tools": ["list_files"],
        "forbid_tools": ["monster_search", "web_search", "searxng"],
        "expect_in_output": [],
    },
    {
        "name": "T3_named_subdir_no_websearch",
        "suite": "baseline",
        "prompt": "What is in the ironjaw folder?",
        "expect_tools": ["list_files"],
        "forbid_tools": ["monster_search", "web_search"],
        "expect_in_output": [],
    },
    {
        "name": "T4_genuine_web_query",
        "suite": "baseline",
        "prompt": "What is the latest stable version of Rust?",
        "expect_tools": [],
        "forbid_tools": ["list_files"],
        "expect_in_output": [],
    },
    # ---- SLICE 1: multi-tool chaining ----
    {
        "name": "S1_chain_list_then_read_readme",
        "suite": "slice1",
        "prompt": "What does the README in the ironjaw project say in its first paragraph?",
        "expect_tools": ["read_file"],  # may also call list_files first
        "expect_chain_contains_in_order": ["read_file"],
        "forbid_tools": ["monster_search", "web_search"],
        "expect_in_output": [],
    },
    {
        "name": "S1_chain_describe_jobhunter",
        "suite": "slice1",
        "prompt": "Look at the Cargo.toml of ironjaw and tell me the package name and version.",
        "expect_tools": ["read_file"],
        "forbid_tools": ["monster_search", "web_search"],
        "expect_in_output": ["ironjaw"],
    },
    # ---- SLICE 2: Confirm-tier HITL approval flow ----
    {
        "name": "S2_hitl_write_file_approve",
        "suite": "slice2",
        "prompt": "Create a file at ironjaw/data/smoke_test.txt with the content 'hello from harness 2026-05-07'.",
        "expect_tools": ["write_file"],
        "expect_permission_requests": ["write_file"],
        "permission_policy": {"write_file": "approve"},
        "forbid_tools": [],
        "expect_in_output": [],
    },
    {
        "name": "S2_hitl_write_file_deny",
        "suite": "slice2",
        "prompt": "Create a file at ironjaw/data/should_not_exist.txt with the content 'denied test'.",
        "expect_tools": ["write_file"],
        "expect_permission_requests": ["write_file"],
        "permission_policy": {"write_file": "deny"},
        "forbid_tools": [],
        "expect_in_output": [],
    },
    # ---- SLICE 3: tool diversity ----
    {
        "name": "S3_git_status",
        "suite": "slice3",
        "prompt": "What is the git status of the ironjaw project?",
        "expect_tools_any": ["git_status", "shell"],
        "forbid_tools": ["monster_search", "web_search"],
        "expect_in_output": [],
    },
    {
        "name": "S3_process_list",
        "suite": "slice3",
        "prompt": "List the top 5 processes running on this machine by memory usage.",
        "expect_tools_any": ["process", "shell"],
        "permission_policy": {"shell": "approve", "process": "approve"},
        "forbid_tools": ["monster_search", "web_search"],
        "expect_in_output": [],
    },
    {
        "name": "S3_schedule_list",
        "suite": "slice3",
        "prompt": "What proactive schedules are configured?",
        "expect_tools_any": ["schedule_list"],
        "forbid_tools": ["monster_search", "web_search"],
        "expect_in_output": [],
    },
    {
        "name": "S3_remote_host_status",
        "suite": "slice3",
        "prompt": "What's the status of the docker-host server?",
        "expect_tools_any": ["remote_host_status", "shell"],
        "forbid_tools": [],
        "expect_in_output": [],
    },
    # ---- SLICE 4: error recovery ----
    {
        "name": "S4_path_outside_workdir",
        "suite": "slice4",
        "prompt": "List the contents of /etc/passwd",
        "expect_tools_any": ["list_files", "read_file"],
        "expect_in_output": [],
        "expect_tool_error": True,  # security should deny; LLM should not loop
    },
    {
        "name": "S4_ambiguous_request",
        "suite": "slice4",
        "prompt": "Do the thing with the stuff in the project.",
        "expect_tools_any": [],  # ideally model asks for clarification
        "expect_in_output": [],
    },
    {
        "name": "S4_nonexistent_file",
        "suite": "slice4",
        "prompt": "Read the file at ironjaw/totally_does_not_exist_zzz.txt and tell me what it says.",
        "expect_tools": ["read_file"],
        "expect_tool_error": True,
        "expect_in_output": [],
    },
]

DEFAULT_POLICY = "approve"


async def send_req(ws, method: str, params: dict[str, Any]) -> str:
    req_id = str(uuid.uuid4())
    await ws.send(
        json.dumps(
            {
                "type": "req",
                "id": req_id,
                "method": method,
                "params": params,
            }
        )
    )
    return req_id


async def send_cmd_res(ws, request_id: str, ok: bool):
    await ws.send(
        json.dumps(
            {
                "type": "cmd.res",
                "id": request_id,
                "ok": ok,
            }
        )
    )


async def auth(ws) -> dict:
    req_id = await send_req(ws, "auth.connect", {"token": AUTH_TOKEN})
    while True:
        raw = await ws.recv()
        msg = json.loads(raw)
        if msg.get("id") == req_id and msg.get("type") in ("res", "err"):
            return msg


async def chat_send(ws, prompt: str) -> str:
    return await send_req(ws, "chat.send", {"message": prompt})


async def session_create_fresh(ws, name: str) -> str:
    req_id = await send_req(ws, "session.create", {"name": name})
    while True:
        raw = await ws.recv()
        msg = json.loads(raw)
        if msg.get("type") in ("res", "err") and msg.get("id") == req_id:
            if not msg.get("ok", True) or msg.get("error"):
                raise RuntimeError(f"session.create failed: {msg}")
            payload = msg.get("payload") or {}
            sid = payload.get("session_id") or payload.get("id") or payload.get("session", {}).get("id")
            if not sid:
                raise RuntimeError(f"session.create returned no id: {msg}")
            sw_id = await send_req(ws, "session.switch", {"session_id": sid})
            while True:
                raw = await ws.recv()
                m2 = json.loads(raw)
                if m2.get("type") in ("res", "err") and m2.get("id") == sw_id:
                    if not m2.get("ok", True) or m2.get("error"):
                        raise RuntimeError(f"session.switch failed: {m2}")
                    return sid


async def run_one_test(ws, test: dict, timeout_s: float = 600.0) -> dict:
    print(f"\n{'='*78}")
    print(f"TEST {test['name']}: {test['prompt']}")
    print('=' * 78)

    policy = test.get("permission_policy", {})
    expect_perms = list(test.get("expect_permission_requests", []))
    permission_requests_seen: list[str] = []

    req_id = await chat_send(ws, test["prompt"])
    start = time.time()

    tools_called: list[str] = []
    tool_results: list[dict] = []
    tool_error_seen = False
    nudge_fired = False
    final_text_chunks: list[str] = []
    deadline = start + timeout_s
    done = False
    got_ack = False
    last_frame_time = time.time()
    quiet_idle_s = 120.0

    while not done and time.time() < deadline:
        try:
            now = time.time()
            if got_ack:
                idle_remaining = quiet_idle_s - (now - last_frame_time)
                if idle_remaining <= 0:
                    print(f"  [QUIET-IDLE {quiet_idle_s}s — assuming done]")
                    break
                wait = min(deadline - now, idle_remaining)
            else:
                wait = deadline - now
            if wait <= 0:
                print("  [TIMEOUT]")
                break
            raw = await asyncio.wait_for(ws.recv(), timeout=wait)
        except asyncio.TimeoutError:
            print("  [QUIET-IDLE — assuming done]")
            break

        last_frame_time = time.time()
        msg = json.loads(raw)
        ftype = msg.get("type", "")
        evt = msg.get("event", "")
        payload = msg.get("payload") or {}

        if ftype == "stream":
            delta = msg.get("delta")
            if delta:
                final_text_chunks.append(delta)
            if msg.get("done"):
                done = True
                print(f"  [stream done]")
            continue

        if ftype == "event":
            if evt == "tool.start":
                tname = payload.get("tool_name") or payload.get("name")
                if tname:
                    tools_called.append(tname)
                    args_blob = payload.get("arguments") or payload.get("params") or {}
                    print(f"  [tool.start] {tname}  args={json.dumps(args_blob)[:200]}")
            elif evt == "tool.result":
                tname = payload.get("tool_name") or payload.get("name")
                tr = {"name": tname, "payload": payload}
                tool_results.append(tr)
                if not payload.get("ok", True) or "error" in (payload.get("content") or {}) or "error_code" in payload:
                    tool_error_seen = True
                preview = json.dumps(payload)[:300]
                print(f"  [tool.result] {tname}  {preview}")
            elif evt == "permission.request":
                req_id_p = payload.get("request_id")
                method_p = payload.get("method") or payload.get("tool") or "?"
                permission_requests_seen.append(method_p)
                # Tool name might be in source field (e.g., "tool:write_file")
                source = payload.get("source", "") or ""
                tool_for_policy = method_p
                if source.startswith("tool:"):
                    tool_for_policy = source[5:]
                decision = policy.get(tool_for_policy, policy.get(method_p, DEFAULT_POLICY))
                print(f"  [permission.request] method={method_p} source={source}  -> {decision}")
                if req_id_p:
                    await send_cmd_res(ws, req_id_p, ok=(decision == "approve"))
            elif evt == "chat":
                state = payload.get("state", "")
                if state == "delta":
                    chunk = payload.get("message", {}).get("content", "")
                    if chunk:
                        final_text_chunks.append(chunk)
                elif state == "final":
                    full = payload.get("message", {}).get("content", "")
                    if full:
                        final_text_chunks = [full]
                    print(f"  [chat:final stop={payload.get('stopReason')} tokens_out={payload.get('outputTokens')}]")
                    done = True
                elif state in ("thinking", "thinking_done"):
                    pass  # quiet — too noisy
            else:
                blob = json.dumps(msg).lower()
                if "wrong_tool" in blob or "appears to be a local folder" in blob or "did you mean to use list_files" in blob:
                    nudge_fired = True
                    print(f"  [NUDGE? in event:{evt}]")
                # Else: silent
            continue

        if ftype in ("res", "err") and msg.get("id") == req_id:
            if not msg.get("ok", True) or msg.get("error"):
                print(f"  [ERR res] {json.dumps(msg)[:300]}")
                done = True
            else:
                streaming = bool(payload.get("streaming"))
                got_ack = True
                if not streaming:
                    done = True
            continue

    elapsed = time.time() - start
    final_text = "".join(final_text_chunks)
    print(f"\n  [final ({elapsed:.1f}s)] {final_text[:500]}")

    # Verdict assembly
    verdict = []
    if test.get("expect_tools"):
        for t in test["expect_tools"]:
            if t in tools_called:
                verdict.append(f"PASS expect_tool:{t}")
            else:
                verdict.append(f"FAIL expect_tool:{t} (got {tools_called})")
    if test.get("expect_tools_any"):
        any_match = any(t in tools_called for t in test["expect_tools_any"])
        if any_match:
            verdict.append(f"PASS expect_any:{test['expect_tools_any']}")
        else:
            verdict.append(f"FAIL expect_any:{test['expect_tools_any']} (got {tools_called})")
    for t in test.get("forbid_tools", []):
        if t in tools_called:
            verdict.append(f"FAIL forbidden_tool:{t}")
        else:
            verdict.append(f"PASS no_forbidden:{t}")
    for kw in test.get("expect_in_output", []):
        if kw.lower() in final_text.lower():
            verdict.append(f"PASS contains:{kw}")
        else:
            verdict.append(f"WARN missing:{kw}")
    chain = test.get("expect_chain_contains_in_order")
    if chain:
        idx = 0
        for tn in tools_called:
            if idx < len(chain) and tn == chain[idx]:
                idx += 1
        if idx == len(chain):
            verdict.append(f"PASS chain:{chain}")
        else:
            verdict.append(f"FAIL chain:{chain} progressed_to={idx}")
    for p in expect_perms:
        if p in permission_requests_seen:
            verdict.append(f"PASS perm_req:{p}")
        else:
            verdict.append(f"FAIL perm_req:{p} not_seen (saw {permission_requests_seen})")
    if test.get("expect_tool_error"):
        if tool_error_seen:
            verdict.append("PASS tool_error_handled")
        else:
            verdict.append("WARN expected_tool_error_not_seen")

    print(f"\n  VERDICT: {' | '.join(verdict) or '(no checks)'}")
    print(f"  tools_called: {tools_called}")
    print(f"  permission_requests: {permission_requests_seen}")
    return {
        "name": test["name"],
        "suite": test.get("suite", ""),
        "tools_called": tools_called,
        "permission_requests": permission_requests_seen,
        "tool_error_seen": tool_error_seen,
        "elapsed_s": elapsed,
        "final_text": final_text,
        "verdict": verdict,
    }


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--prompt", type=int, default=None)
    ap.add_argument("--suite", type=str, default=None)
    ap.add_argument("--timeout", type=float, default=600.0)
    args = ap.parse_args()

    if args.prompt is not None:
        tests = [TESTS[args.prompt]]
    elif args.suite:
        tests = [t for t in TESTS if t.get("suite") == args.suite]
    else:
        tests = TESTS

    async with websockets.connect(GATEWAY_URL, max_size=10 * 1024 * 1024) as ws:
        auth_resp = await auth(ws)
        if auth_resp.get("type") == "err":
            print(f"AUTH FAILED: {auth_resp}")
            sys.exit(1)
        payload = auth_resp.get("payload") or {}
        print(f"[auth ok] session={payload.get('session_id', '?')}")

        results = []
        for t in tests:
            fresh = await session_create_fresh(ws, f"smoke_{t['name']}")
            print(f"\n[fresh session] {fresh}")
            results.append(await run_one_test(ws, t, timeout_s=args.timeout))

        # Summary
        print("\n" + "#" * 78)
        print(f"SUMMARY ({len(results)} tests)")
        print("#" * 78)
        for r in results:
            fails = [v for v in r["verdict"] if v.startswith("FAIL")]
            warns = [v for v in r["verdict"] if v.startswith("WARN")]
            status = "FAIL" if fails else ("WARN" if warns else "PASS")
            print(f"  {status}  [{r['suite']:8s}] {r['name']:38s}  {r['elapsed_s']:5.1f}s  tools={r['tools_called']}")
            for f in fails:
                print(f"        -> {f}")
            for w in warns:
                print(f"        -> {w}")

        # JSON dump for follow-up analysis
        out_path = "scripts/smoke_results.json"
        try:
            with open(out_path, "w", encoding="utf-8") as fh:
                json.dump(results, fh, indent=2)
            print(f"\n[wrote] {out_path}")
        except OSError:
            pass


if __name__ == "__main__":
    asyncio.run(main())
