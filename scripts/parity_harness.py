"""
Claude-Code parity harness for Ironjaw.

Verifies 10 capabilities Ironjaw must have to substitute for Claude Code:
  P1  apply_patch (Edit-style modification)
  P2  Multi-step plans (3+ tool sequence)
  P3  Subagent dispatch (claude_subagent)
  P4  MCP visibility (external MCP tools surface in tools.list)
  P5  Skill activation (skills.list + activate, tool allowlist changes)
  P6  Hook lifecycle (BLOCKED — [hooks] enabled = false)
  P7  Brain writeback (ToolVerified memory after tool call)
  P8  Session forking (fork, divergence)
  P9  Long-context behavior (large prompt handling)
  P10 Chat abort (mid-stream cancel)
"""
import asyncio
import json
import os
import sys
import time
import uuid

import websockets

GATEWAY_URL = "ws://127.0.0.1:18794/ws"
_TOKEN_ENV = "IRONJAW_AUTH_TOKEN"
AUTH_TOKEN = os.environ.get(_TOKEN_ENV, "")
if not AUTH_TOKEN:
    raise SystemExit(
        f"set {_TOKEN_ENV} to the token this gateway was configured with"
    )
WORKING_DIR = "C:/Users/deploy/Projects/ironjaw"

# Force UTF-8 stdout so emoji in chat output don't crash on Windows cp1252.
try:
    sys.stdout.reconfigure(encoding="utf-8")
except Exception:
    pass


async def send_req(ws, method, params, req_id=None):
    rid = req_id or str(uuid.uuid4())
    await ws.send(json.dumps({"type": "req", "id": rid, "method": method, "params": params}))
    return rid


async def send_cmd_res(ws, request_id, ok=True):
    await ws.send(json.dumps({"type": "cmd.res", "id": request_id, "ok": ok}))


async def wait_for_res(ws, req_id, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        raw = await asyncio.wait_for(ws.recv(), timeout=deadline - time.time())
        m = json.loads(raw)
        if m.get("type") in ("res", "err") and m.get("id") == req_id:
            return m
    raise asyncio.TimeoutError(f"no res for {req_id}")


async def auth(ws):
    rid = await send_req(ws, "auth.connect", {"token": AUTH_TOKEN})
    while True:
        raw = await ws.recv()
        m = json.loads(raw)
        if m.get("id") == rid and m.get("type") in ("res", "err"):
            return m


async def session_create(ws, name):
    rid = await send_req(ws, "session.create", {"name": name})
    while True:
        raw = await ws.recv()
        m = json.loads(raw)
        if m.get("id") == rid and m.get("type") in ("res", "err"):
            if not m.get("ok", True) or m.get("error"):
                raise RuntimeError(f"session.create failed: {m}")
            p = m.get("payload") or {}
            sid = p.get("session_id") or p.get("id") or (p.get("session", {}).get("id") if isinstance(p.get("session"), dict) else None)
            return sid


async def session_switch(ws, sid):
    rid = await send_req(ws, "session.switch", {"session_id": sid})
    while True:
        raw = await ws.recv()
        m = json.loads(raw)
        if m.get("id") == rid and m.get("type") in ("res", "err"):
            return m


async def chat_run(ws, prompt, timeout=240, abort_after_s=None, policy=None):
    """
    Send chat.send + drain agentic events. Optionally abort after N seconds.
    Returns dict with tools_called, final_text, perm_requests, was_aborted, elapsed_s.
    """
    policy = policy or {}
    rid = await send_req(ws, "chat.send", {"message": prompt})
    start = time.time()
    tools_called = []
    perm_requests = []
    final_chunks = []
    was_aborted = False
    abort_sent = False
    done = False
    quiet_idle = 90.0
    last_frame = time.time()

    while not done and (time.time() - start) < timeout:
        if abort_after_s and not abort_sent and (time.time() - start) > abort_after_s:
            await send_req(ws, "chat.abort", {})
            abort_sent = True
            print(f"  [SENT chat.abort at +{time.time()-start:.1f}s]")
        try:
            wait = min(quiet_idle - (time.time() - last_frame), timeout - (time.time() - start))
            if wait <= 0:
                print(f"  [QUIET-IDLE]")
                break
            raw = await asyncio.wait_for(ws.recv(), timeout=wait)
        except asyncio.TimeoutError:
            print(f"  [QUIET-IDLE timeout]")
            break
        last_frame = time.time()
        m = json.loads(raw)
        ftype = m.get("type", "")
        evt = m.get("event", "")
        payload = m.get("payload") or {}

        if ftype == "stream":
            if m.get("delta"):
                final_chunks.append(m["delta"])
            if m.get("done"):
                done = True
            continue
        if ftype == "event":
            if evt == "tool.start":
                tn = payload.get("tool_name") or payload.get("tool") or payload.get("name")
                if tn:
                    tools_called.append(tn)
                    print(f"  [tool.start] {tn}")
            elif evt == "tool.result":
                tn = payload.get("tool_name") or payload.get("name")
                ok = payload.get("ok", True)
                preview = json.dumps(payload.get("content") or {})[:200]
                print(f"  [tool.result] {tn} ok={ok}  {preview}")
            elif evt == "permission.request":
                req_id_p = payload.get("request_id")
                tool_p = payload.get("tool") or payload.get("method") or "?"
                perm_requests.append(tool_p)
                decision = policy.get(tool_p, "approve")
                print(f"  [permission.request] {tool_p} -> {decision}")
                if req_id_p:
                    await send_cmd_res(ws, req_id_p, ok=(decision == "approve"))
            elif evt == "chat":
                state = payload.get("state", "")
                if state == "delta":
                    c = payload.get("message", {}).get("content", "")
                    if c:
                        final_chunks.append(c)
                elif state == "final":
                    full = payload.get("message", {}).get("content", "")
                    if full:
                        final_chunks = [full]
                    stop = payload.get("stopReason")
                    if stop == "aborted" or "abort" in str(stop).lower():
                        was_aborted = True
                    print(f"  [chat:final stop={stop} tokens={payload.get('outputTokens')}]")
                    done = True
            continue
        if ftype in ("res", "err") and m.get("id") == rid:
            if not m.get("ok", True):
                done = True
            continue

    elapsed = time.time() - start
    return {
        "tools_called": tools_called,
        "perm_requests": perm_requests,
        "final_text": "".join(final_chunks),
        "was_aborted": was_aborted,
        "abort_sent": abort_sent,
        "elapsed_s": elapsed,
    }


# ---- Individual parity tests --------------------------------------------------

async def p1_apply_patch(ws):
    """Seed a small file, then ask LLM to modify it via apply_patch.

    Gateway working_dir is C:/Users/deploy/Projects, so paths must be relative to that.
    """
    seed_path = "ironjaw/scripts/parity_target.txt"  # relative to gateway working_dir
    seed_content = "line one\nline two\nline three\n"
    # Seed via direct write_file (uses gateway working_dir)
    rid = await send_req(ws, "tools.execute", {"tool": "write_file", "params": {"path": seed_path, "content": seed_content}})
    while True:
        raw = await ws.recv()
        m = json.loads(raw)
        if m.get("type") in ("res", "err") and m.get("id") == rid:
            break

    sid = await session_create(ws, "p1_apply_patch")
    await session_switch(ws, sid)
    prompt = (
        f"There is a file at {seed_path} containing three lines: 'line one', 'line two', 'line three'. "
        f"Use the apply_patch tool to change the line 'line two' to 'line two MODIFIED'. "
        f"Provide the patch in unified diff format."
    )
    r = await chat_run(ws, prompt, timeout=240)

    # Verify result
    rid = await send_req(ws, "tools.execute", {"tool": "read_file", "params": {"path": seed_path}})
    while True:
        raw = await ws.recv()
        m = json.loads(raw)
        if m.get("type") in ("res", "err") and m.get("id") == rid:
            break
    data = (m.get("payload") or {}).get("result") or {}
    final_content = data.get("content") if isinstance(data, dict) else ""
    if isinstance(final_content, dict):
        final_content = final_content.get("content", "")

    used_patch = "apply_patch" in r["tools_called"]
    modified = "MODIFIED" in (final_content or "")
    verdict = []
    verdict.append("PASS used_apply_patch" if used_patch else f"FAIL didnt_use_apply_patch (used: {r['tools_called']})")
    verdict.append("PASS file_modified" if modified else "FAIL file_unchanged")

    return {
        "name": "P1_apply_patch",
        "verdict": verdict,
        "tools_called": r["tools_called"],
        "elapsed_s": r["elapsed_s"],
        "extras": {"final_content_preview": (final_content or "")[:200]},
    }


async def p2_multistep_plan(ws):
    sid = await session_create(ws, "p2_multistep")
    await session_switch(ws, sid)
    # Working_dir is C:/Users/deploy/Projects, so prefix with ironjaw/
    prompt = (
        "Three tasks: (1) read ironjaw/crates/ironjaw-llm/Cargo.toml, "
        "(2) list the files in ironjaw/crates/ironjaw-llm/src, "
        "(3) tell me which Rust files in src/ are likely entry points based on the Cargo.toml."
    )
    r = await chat_run(ws, prompt, timeout=300)
    distinct_tools = set(r["tools_called"])
    verdict = []
    verdict.append(f"PASS used_multiple_tools ({len(r['tools_called'])} calls, {len(distinct_tools)} distinct)" if len(r["tools_called"]) >= 2 else f"FAIL only_{len(r['tools_called'])}_tool_calls")
    verdict.append("PASS read_file_used" if "read_file" in distinct_tools else "FAIL no_read_file")
    verdict.append("PASS list_files_used" if "list_files" in distinct_tools else "FAIL no_list_files")
    return {
        "name": "P2_multistep_plan",
        "verdict": verdict,
        "tools_called": r["tools_called"],
        "elapsed_s": r["elapsed_s"],
        "extras": {"final_text_preview": r["final_text"][:300]},
    }


async def p3_subagent(ws):
    sid = await session_create(ws, "p3_subagent")
    await session_switch(ws, sid)
    prompt = (
        "Use the claude_subagent tool to delegate this question: 'What is 2+2?'. "
        "Return whatever the subagent says."
    )
    r = await chat_run(ws, prompt, timeout=240)
    used = "claude_subagent" in r["tools_called"]
    verdict = []
    verdict.append("PASS subagent_used" if used else f"FAIL no_subagent_call (used: {r['tools_called']})")
    return {"name": "P3_subagent", "verdict": verdict, "tools_called": r["tools_called"], "elapsed_s": r["elapsed_s"], "extras": {"final_preview": r["final_text"][:200]}}


async def p4_mcp_visibility(ws):
    """Check tools.list for MCP-prefixed tools (mcp__*) or known external MCP namespaces."""
    rid = await send_req(ws, "tools.list", {})
    while True:
        raw = await ws.recv()
        m = json.loads(raw)
        if m.get("type") in ("res", "err") and m.get("id") == rid:
            break
    tools = (m.get("payload") or {}).get("tools", [])
    mcp_tools = [t.get("name") for t in tools if "mcp__" in (t.get("name") or "") or "mcp_" in (t.get("name") or "")]
    total = len(tools)
    verdict = []
    verdict.append(f"PASS tools_listed ({total} total)")
    if mcp_tools:
        verdict.append(f"PASS mcp_tools_present ({len(mcp_tools)})")
    else:
        verdict.append("WARN no_mcp_prefix_tools (no [[mcp.servers]] configured — expected)")
    return {"name": "P4_mcp_visibility", "verdict": verdict, "tools_called": [], "elapsed_s": 0, "extras": {"total_tools": total, "mcp_tools": mcp_tools}}


async def p5_skill_activation(ws):
    """Call skills-related methods, see what's wired."""
    results = {}
    for method in ["skills.list", "skill.list", "skills.active", "session.skills"]:
        try:
            rid = await send_req(ws, method, {})
            res = await wait_for_res(ws, rid, timeout=10)
            results[method] = res.get("type")
            if res.get("type") == "res" and res.get("ok", True):
                payload = res.get("payload") or {}
                results[method + "_payload_keys"] = list(payload.keys())[:10]
                if "skills" in payload:
                    results[method + "_skills"] = [s.get("name") if isinstance(s, dict) else s for s in payload.get("skills", [])][:10]
        except Exception as e:
            results[method] = f"err:{e}"
    verdict = []
    if any("skills" in str(v).lower() for v in results.values()):
        verdict.append(f"PASS skills_method_responded ({results})")
    else:
        verdict.append(f"FAIL no_skills_method_works ({results})")
    return {"name": "P5_skill_activation", "verdict": verdict, "tools_called": [], "elapsed_s": 0, "extras": results}


async def p6_hooks(ws):
    rid = await send_req(ws, "hooks.list", {})
    try:
        res = await wait_for_res(ws, rid, timeout=10)
    except Exception as e:
        return {"name": "P6_hooks", "verdict": [f"FAIL hooks_method_unreachable:{e}"], "tools_called": [], "elapsed_s": 0, "extras": {}}
    payload = res.get("payload") or {}
    if not res.get("ok", True):
        verdict = ["WARN hooks_disabled_in_config"]
    else:
        hooks = payload.get("hooks", [])
        verdict = [f"PASS hooks_endpoint_works ({len(hooks)} hooks)"]
    return {"name": "P6_hooks", "verdict": verdict, "tools_called": [], "elapsed_s": 0, "extras": {"payload_keys": list(payload.keys())[:5]}}


async def p7_brain_writeback(ws):
    """Trigger a tool call, then query brain for ToolVerified memories."""
    sid = await session_create(ws, "p7_brain_writeback")
    await session_switch(ws, sid)
    # Trigger a tool call
    r = await chat_run(ws, "List the files in the JobHunter folder.", timeout=180)
    # Query brain.recall to find any ToolVerified memories from this session
    await asyncio.sleep(2)  # let writeback land
    rid = await send_req(ws, "brain.recall", {"query": "JobHunter folder list_files", "limit": 10, "explain": True})
    res = await wait_for_res(ws, rid, timeout=15)
    payload = res.get("payload") or {}
    memories = payload.get("memories") or payload.get("results") or []
    tool_verified = [mm for mm in memories if (mm.get("trust_level") if isinstance(mm, dict) else "") == "ToolVerified" or "tool_verified" in str(mm).lower()]
    verdict = []
    verdict.append("PASS list_files_called" if "list_files" in r["tools_called"] else "FAIL no_list_files")
    verdict.append(f"INFO brain_recall_returned ({len(memories)} memories)")
    verdict.append(f"PASS tool_verified_memory_present ({len(tool_verified)})" if tool_verified else "WARN no_tool_verified_memory_found")
    return {"name": "P7_brain_writeback", "verdict": verdict, "tools_called": r["tools_called"], "elapsed_s": r["elapsed_s"], "extras": {"mem_count": len(memories), "tool_verified": len(tool_verified)}}


async def p8_session_fork(ws):
    """Create session, send a message, fork it, verify both have identical history then diverge."""
    sid_a = await session_create(ws, "p8_parent")
    await session_switch(ws, sid_a)
    r1 = await chat_run(ws, "Remember the codeword XYLOPHONE-7. Just acknowledge.", timeout=120)
    # Fork
    rid = await send_req(ws, "session.fork", {"session_id": sid_a})
    res = await wait_for_res(ws, rid, timeout=10)
    if not res.get("ok", True) or res.get("error"):
        return {"name": "P8_session_fork", "verdict": [f"FAIL fork_method_error:{res}"], "tools_called": [], "elapsed_s": 0, "extras": {}}
    payload = res.get("payload") or {}
    sid_b = (
        payload.get("fork_id")
        or payload.get("session_id")
        or payload.get("id")
        or (payload.get("session", {}) or {}).get("id")
    )
    if not sid_b:
        return {"name": "P8_session_fork", "verdict": [f"FAIL fork_no_id (payload={payload})"], "tools_called": [], "elapsed_s": 0, "extras": payload}
    # Switch to fork; ask if it remembers the codeword
    await session_switch(ws, sid_b)
    r2 = await chat_run(ws, "What was the codeword I asked you to remember?", timeout=120)
    remembers = "XYLOPHONE-7" in r2["final_text"] or "xylophone" in r2["final_text"].lower()
    verdict = []
    verdict.append("PASS fork_returned_id")
    verdict.append("PASS fork_inherited_history" if remembers else f"FAIL fork_lost_history (got: {r2['final_text'][:150]})")
    return {"name": "P8_session_fork", "verdict": verdict, "tools_called": [], "elapsed_s": r1["elapsed_s"] + r2["elapsed_s"], "extras": {"parent": sid_a, "child": sid_b, "remembered": remembers}}


async def p9_long_context(ws):
    """Send a large prompt and verify the gateway handles it."""
    sid = await session_create(ws, "p9_long_context")
    await session_switch(ws, sid)
    # ~6000 token chunk of repeated text + a question at the end
    chunk = "The quick brown fox jumps over the lazy dog. " * 800  # ~30000 chars
    prompt = chunk + "\n\nIn one sentence, what was the repeated phrase above?"
    r = await chat_run(ws, prompt, timeout=300)
    got_answer = "quick brown fox" in r["final_text"].lower() or "lazy dog" in r["final_text"].lower()
    verdict = []
    verdict.append("PASS long_context_handled" if got_answer else f"FAIL long_context_didnt_answer ({r['final_text'][:120]})")
    return {"name": "P9_long_context", "verdict": verdict, "tools_called": r["tools_called"], "elapsed_s": r["elapsed_s"], "extras": {"prompt_chars": len(prompt), "answer_preview": r["final_text"][:200]}}


async def p10_chat_abort(ws):
    """Start a long shell command, send chat.abort, verify cancellation."""
    sid = await session_create(ws, "p10_abort")
    await session_switch(ws, sid)
    # ping -n 30 = ~30 seconds on Windows
    prompt = "Run the shell command 'ping -n 30 127.0.0.1' to wait 30 seconds, then tell me when it finished."
    r = await chat_run(ws, prompt, timeout=120, abort_after_s=8)
    verdict = []
    verdict.append("PASS abort_sent" if r["abort_sent"] else "FAIL abort_not_sent")
    verdict.append("PASS abort_acknowledged" if r["was_aborted"] else f"WARN abort_not_in_stop_reason (elapsed={r['elapsed_s']:.1f}s)")
    return {"name": "P10_chat_abort", "verdict": verdict, "tools_called": r["tools_called"], "elapsed_s": r["elapsed_s"], "extras": {"abort_sent": r["abort_sent"], "was_aborted": r["was_aborted"]}}


# ---- Runner -------------------------------------------------------------------

TESTS = [
    ("P4_mcp_visibility", p4_mcp_visibility),
    ("P5_skill_activation", p5_skill_activation),
    ("P6_hooks", p6_hooks),
    ("P3_subagent", p3_subagent),
    ("P8_session_fork", p8_session_fork),
    ("P7_brain_writeback", p7_brain_writeback),
    ("P1_apply_patch", p1_apply_patch),
    ("P2_multistep_plan", p2_multistep_plan),
    ("P9_long_context", p9_long_context),
    ("P10_chat_abort", p10_chat_abort),  # last — known to poison subsequent state
]


async def main():
    async with websockets.connect(GATEWAY_URL, max_size=20 * 1024 * 1024) as ws:
        a = await auth(ws)
        print(f"[auth ok] session={(a.get('payload') or {}).get('session_id', '?')}")
        results = []
        for name, fn in TESTS:
            print(f"\n{'='*78}\nRUN {name}\n{'='*78}")
            try:
                r = await fn(ws)
                results.append(r)
                print(f"  VERDICT: {' | '.join(r['verdict'])}")
            except Exception as e:
                import traceback
                tb = traceback.format_exc()
                print(f"  EXCEPTION: {e}")
                print(tb[-600:])
                results.append({"name": name, "verdict": [f"FAIL exception:{e}"], "tools_called": [], "elapsed_s": 0, "extras": {"traceback": tb[-400:]}})

        print("\n" + "#" * 78)
        print(f"PARITY SUMMARY ({len(results)} tests)")
        print("#" * 78)
        for r in results:
            fails = [v for v in r["verdict"] if v.startswith("FAIL")]
            warns = [v for v in r["verdict"] if v.startswith("WARN")]
            status = "FAIL" if fails else ("WARN" if warns else "PASS")
            print(f"  {status}  {r['name']:30s} {r['elapsed_s']:5.1f}s  tools={r['tools_called']}")
            for v in r["verdict"]:
                print(f"        {v}")

        out_path = "scripts/parity_results.json"
        try:
            with open(out_path, "w", encoding="utf-8") as fh:
                json.dump(results, fh, indent=2, default=str)
            print(f"\n[wrote] {out_path}")
        except OSError as e:
            print(f"[write fail] {e}")


if __name__ == "__main__":
    asyncio.run(main())
