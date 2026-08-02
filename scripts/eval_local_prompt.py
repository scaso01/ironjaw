"""Eval harness for the local-model base prompt + curated tools.

Runs a small agentic loop DIRECTLY against llama-server (:8080), non-streaming,
executing read-only tools locally. This isolates the exact variable under test —
the system prompt (config/prompts/local_agentic.md) and the curated tool set —
from the Ironjaw gateway's streaming/model-management layers (which are a
separate, pre-existing concern and affect every prompt equally, so they can't
distinguish prompt A from prompt B).

Scores tool-selection, answer quality, and the known failure modes (empty final,
announce-but-don't-act, wrong tool). Emits an aggregate JSON score.

  py -3 scripts/eval_local_prompt.py [--split train|heldout|all]
     [--prompt-file config/prompts/local_agentic.md | --no-prompt]
     [--model qwen3.6:35b-a3b] [--url http://localhost:8080/v1/chat/completions]
     [--max-iters 6] [--out data-dev/eval_result.json]

Autoresearch: mutate config/prompts/local_agentic.md, re-run `--split train`,
keep changes that raise `score`. Held-out tasks check for overfitting.
Read-only: tool execution never writes or execs anything.
"""
import argparse
import asyncio
import json
import os
import platform
import re
import subprocess
import urllib.request
import uuid

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))       # ...\ironjaw
BASE = os.path.dirname(REPO)                                             # the dir holding this repo (tool working_dir)
FIXTURES = os.path.join(REPO, "data-dev", "eval_fixtures")
FIX_BARE = "ironjaw/data-dev/eval_fixtures"
ANNOUNCE = re.compile(r"\b(let me|i'?ll|i will|let's|i'?m going to|i am going to|going to)\b", re.I)


def ensure_fixtures():
    os.makedirs(FIXTURES, exist_ok=True)
    with open(os.path.join(FIXTURES, "facts.txt"), "w", encoding="utf-8") as f:
        f.write("The project codename is BLUEJAY.\nThe build number is 4471.\n")
    for base, names in (("countdir", ["a", "b", "c", "d", "e"]), ("countdir2", ["x", "y", "z"])):
        for n in names:
            os.makedirs(os.path.join(FIXTURES, base, n), exist_ok=True)


# ---- read-only tool implementations (resolve bare names under BASE) ----
def _resolve(path):
    p = os.path.normpath(os.path.join(BASE, path.replace("\\", "/").strip("/")))
    if os.path.commonpath([os.path.normcase(p), os.path.normcase(BASE)]) != os.path.normcase(BASE):
        raise ValueError(f"path escapes working dir: {path}")
    return p


def t_read_file(args):
    p = _resolve(args["path"])
    with open(p, encoding="utf-8", errors="replace") as f:
        return f.read()[:8000]


def t_list_files(args):
    p = _resolve(args.get("path", "."))
    entries = sorted(os.listdir(p))
    dirs = [e for e in entries if os.path.isdir(os.path.join(p, e))]
    return json.dumps({"path": args.get("path", "."), "count": len(entries),
                       "dir_count": len(dirs), "dirs": dirs, "entries": entries[:200]})


def t_grep(args):
    root = _resolve(args.get("path", "."))
    pat = re.compile(args["pattern"], re.I)
    hits = []
    for dp, _, files in os.walk(root):
        for fn in files:
            fp = os.path.join(dp, fn)
            try:
                for i, line in enumerate(open(fp, encoding="utf-8", errors="replace"), 1):
                    if pat.search(line):
                        hits.append({"file": os.path.relpath(fp, BASE).replace("\\", "/"),
                                     "line": i, "text": line.strip()[:200]})
            except Exception:
                pass
    return json.dumps({"count": len(hits), "matches": hits[:50]})


def t_whoami(args):
    return json.dumps({"user": os.environ.get("USERNAME", "?"),
                       "host": platform.node(), "os": platform.system()})


def t_process_list(args):
    try:
        out = subprocess.run(["tasklist", "/fo", "csv", "/nh"], capture_output=True,
                             text=True, timeout=15).stdout
        names = sorted({line.split(",")[0].strip('"') for line in out.splitlines() if line})
        return json.dumps({"count": len(names), "processes": sorted(names)[:60]})
    except Exception as e:
        return json.dumps({"error": str(e)})


def t_web_search(args):
    return json.dumps({"note": "web_search is disabled during eval; use local tools for local questions"})


def t_todo(args):
    return json.dumps({"ok": True})


IMPLS = {"read_file": t_read_file, "list_files": t_list_files, "grep": t_grep,
         "whoami": t_whoami, "process_list": t_process_list, "web_search": t_web_search, "todo": t_todo}

TOOLS = [
    {"type": "function", "function": {"name": "read_file", "description": "Read a file's contents by path.",
     "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}},
    {"type": "function", "function": {"name": "list_files", "description": "List directory entries by path (returns count + names).",
     "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}},
    {"type": "function", "function": {"name": "grep", "description": "Search files under a path for a regex pattern.",
     "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "pattern": {"type": "string"}}, "required": ["path", "pattern"]}}},
    {"type": "function", "function": {"name": "web_search", "description": "Search the public web (external info only).",
     "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}},
    {"type": "function", "function": {"name": "process_list", "description": "List running processes on this machine.",
     "parameters": {"type": "object", "properties": {}}}},
    {"type": "function", "function": {"name": "whoami", "description": "Report the current user, machine, and OS.",
     "parameters": {"type": "object", "properties": {}}}},
    {"type": "function", "function": {"name": "todo", "description": "Track a task checklist.",
     "parameters": {"type": "object", "properties": {"items": {"type": "array", "items": {"type": "string"}}}}}},
]


def check(expected_tool=None, must_call=True, include=None, exclude_tools=None):
    include = include or []
    exclude_tools = exclude_tools or []

    def score(tools, answer):
        fails = []
        a = (answer or "").strip()
        if not a:
            fails.append("empty_final")
        elif len(a) < 20:
            fails.append("too_short")
        if must_call and not tools:
            fails.append("no_tool_called")
            if a and ANNOUNCE.search(a):
                fails.append("announce_stop")
        if expected_tool and expected_tool not in tools:
            fails.append(f"missing_tool:{expected_tool}")
        for t in exclude_tools:
            if t in tools:
                fails.append(f"used_forbidden_tool:{t}")
        for sub in include:
            if sub.lower() not in a.lower():
                fails.append(f"missing_text:{sub}")
        return fails

    return score


TASKS = [
    ("read_fact", "train", f"Read the file {FIX_BARE}/facts.txt and tell me the project codename.",
     check(expected_tool="read_file", include=["BLUEJAY"])),
    ("count_dir", "train", f"How many subfolders are directly inside {FIX_BARE}/countdir?",
     check(expected_tool="list_files", include=["5"])),
    ("grep_find", "train", f"Search under {FIX_BARE} for the word BLUEJAY and tell me which file contains it.",
     check(expected_tool="grep", include=["facts.txt"])),
    ("whoami", "train", "What user account and machine am I running as? Check the system, don't guess.",
     check(expected_tool="whoami", include=[])),
    ("local_not_web", "train", "What is the 'ironjaw' project on this machine? Look at the actual files, don't search the web.",
     check(must_call=True, exclude_tools=["web_search"], include=["ironjaw"])),
    ("list_crates", "train", "List the crates in the ironjaw Rust workspace by looking at ironjaw/crates.",
     check(expected_tool="list_files", include=["ironjaw-ws"])),
    ("count_lines", "train", "How many lines are in the file ironjaw/rustfmt.toml? Read it and count.",
     check(must_call=True, include=[])),

    ("read_fact2", "heldout", f"What build number is recorded in {FIX_BARE}/facts.txt?",
     check(expected_tool="read_file", include=["4471"])),
    ("count_dir2", "heldout", f"Count the folders inside {FIX_BARE}/countdir2 and give me the number.",
     check(expected_tool="list_files", include=["3"])),
    ("grep_codename", "heldout", f"Which file under {FIX_BARE} mentions the word 'codename'?",
     check(expected_tool="grep", include=["facts.txt"])),
    ("security_crate", "heldout", "Which crate in the ironjaw workspace handles security? Look at ironjaw/crates.",
     check(must_call=True, include=["ironjaw-security"])),
    ("platform", "heldout", "Am I running on Windows or Linux? Check the system rather than assuming.",
     check(must_call=True, include=["Windows"])),
]


def llm(url, model, messages, max_tokens=512):
    body = json.dumps({"model": model, "messages": messages, "tools": TOOLS,
                       "tool_choice": "auto", "max_tokens": max_tokens, "stream": False}).encode()
    req = urllib.request.Request(url, body, {"Content-Type": "application/json"})
    d = json.load(urllib.request.urlopen(req, timeout=180))
    return d["choices"][0]["message"]


def run_task(url, model, system, prompt, max_iters):
    """Mini agentic loop: call -> execute tool -> feed back -> until plain answer."""
    messages = ([{"role": "system", "content": system}] if system else []) + [{"role": "user", "content": prompt}]
    tools_called = []
    for _ in range(max_iters):
        msg = llm(url, model, messages)
        tcs = msg.get("tool_calls") or []
        if not tcs:
            return tools_called, (msg.get("content") or "")
        messages.append({"role": "assistant", "content": msg.get("content") or "", "tool_calls": tcs})
        for tc in tcs:
            fn = tc["function"]["name"]
            tools_called.append(fn)
            try:
                args = json.loads(tc["function"].get("arguments") or "{}")
            except Exception:
                args = {}
            try:
                result = IMPLS[fn](args) if fn in IMPLS else json.dumps({"error": f"unknown tool {fn}"})
            except Exception as e:
                result = json.dumps({"error": str(e)})
            messages.append({"role": "tool", "tool_call_id": tc.get("id", fn),
                             "content": str(result)[:6000]})
    # ran out of iterations: force one plain-text answer
    messages.append({"role": "user", "content": "Now answer my original question in plain text, no more tools."})
    msg = llm(url, model, messages)
    return tools_called, (msg.get("content") or "")


async def run_task_ws(url, token, model, prompt, timeout=240):
    """Run one task THROUGH the Ironjaw gateway over WebSocket (real system path).
    The gateway owns prompt assembly + tool execution; base prompt comes from
    [chat.local], not from us. Returns (tools_called, final_answer)."""
    import websockets
    async with websockets.connect(url, max_size=None, open_timeout=15) as ws:
        # Fresh session_id per task — WITHOUT this, auth.connect resumes the most
        # recent session and tasks pollute each other's history (a long/malformed
        # history degrades the local model into emitting broken text tool calls).
        await ws.send(json.dumps({"type": "req", "id": str(uuid.uuid4()),
                                  "method": "auth.connect",
                                  "params": {"token": token, "session_id": str(uuid.uuid4())}}))
        while True:
            v = json.loads(await asyncio.wait_for(ws.recv(), timeout=30))
            if v.get("type") == "res":
                if not v.get("ok"):
                    raise RuntimeError(f"auth failed: {v}")
                break
        await ws.send(json.dumps({"type": "req", "id": str(uuid.uuid4()), "method": "chat.send",
            "params": {"message": prompt, "ephemeral_model": model, "force_fresh": True}}))
        tools, answer = [], ""
        while True:
            v = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
            if v.get("type") != "event":
                continue
            ev, pl = v.get("event"), v.get("payload") or {}
            if ev == "tool.start":
                if pl.get("tool_name"):
                    tools.append(pl["tool_name"])
            elif ev == "chat" and pl.get("state") in ("final", "error", "aborted"):
                answer = ((pl.get("message") or {}).get("content")) or f"[{pl.get('state')}]"
                break
        return tools, answer


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--split", choices=["train", "heldout", "all"], default="all")
    ap.add_argument("--transport", choices=["direct", "ws"], default="direct",
                    help="direct=mini-loop vs llama-server; ws=through the Ironjaw gateway")
    ap.add_argument("--ws-url", default="ws://127.0.0.1:18795/ws")
    ap.add_argument("--token", default=os.environ.get("IRONJAW_DEV_TOKEN", "dev-eval-local-only"))
    ap.add_argument("--ws-model", default="llama-server/qwen3.6:35b-a3b")
    ap.add_argument("--prompt-file", default=os.path.join(REPO, "config", "prompts", "local_agentic.md"))
    ap.add_argument("--no-prompt", action="store_true", help="control: no behavioral system prompt")
    ap.add_argument("--url", default="http://localhost:8080/v1/chat/completions")
    ap.add_argument("--model", default="qwen3.6:35b-a3b")
    ap.add_argument("--max-iters", type=int, default=6)
    ap.add_argument("--out", default=os.path.join(REPO, "data-dev", "eval_result.json"))
    args = ap.parse_args()

    ensure_fixtures()
    system = "" if args.no_prompt else open(args.prompt_file, encoding="utf-8").read()
    tasks = [t for t in TASKS if args.split == "all" or t[1] == args.split]

    results, fail_modes = [], {}
    for tid, split, prompt, scorer in tasks:
        try:
            if args.transport == "ws":
                tools, answer = asyncio.run(run_task_ws(args.ws_url, args.token, args.ws_model, prompt))
            else:
                tools, answer = run_task(args.url, args.model, system, prompt, args.max_iters)
            fails = scorer(tools, answer)
        except Exception as e:
            tools, answer, fails = [], f"[error: {e}]", ["exception"]
        passed = not fails
        for f in fails:
            fail_modes[f.split(":")[0]] = fail_modes.get(f.split(":")[0], 0) + 1
        results.append({"id": tid, "split": split, "passed": passed, "tools": tools,
                        "fails": fails, "answer": (answer or "")[:200]})
        print(f"[{'PASS' if passed else 'FAIL'}] {tid:16} tools={tools} "
              f"{'' if passed else '<< ' + ','.join(fails)}", flush=True)

    def agg(sp):
        rs = [r for r in results if r["split"] == sp]
        return sum(r["passed"] for r in rs), len(rs)
    tp, tt = agg("train")
    hp, ht = agg("heldout")
    total_p = sum(r["passed"] for r in results)
    summary = {"score": round(total_p / len(results), 3) if results else 0.0,
               "prompt": "none" if args.no_prompt else args.prompt_file,
               "total_passed": total_p, "total": len(results),
               "train_passed": tp, "train_total": tt, "heldout_passed": hp, "heldout_total": ht,
               "failure_modes": fail_modes, "results": results}
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(summary, f, indent=2, ensure_ascii=False)
    print(f"\nSCORE {total_p}/{len(results)}  train {tp}/{tt}  heldout {hp}/{ht}"
          f"  failure_modes={fail_modes}\nwrote {args.out}")


if __name__ == "__main__":
    main()
