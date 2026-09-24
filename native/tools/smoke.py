"""Smoke test: drive a published agentdesk.exe over MCP stdio against a scratch copy of the board.

    ./build.ps1 ; publish to %TEMP%\ad-e2e\bin, copy agentdesk.db to %TEMP%\ad-e2e\data, then:
    python native/tools/smoke.py
"""
import json, os, subprocess, sys, time

root = os.path.expandvars(r"%TEMP%\ad-e2e")
env = dict(os.environ, AGENTDESK_PIPE="agentdesk-e2e", AGENTDESK_DATA=root + r"\data",
           CLAUDE_CODE_SESSION_ID="e2e0-session", CLAUDECODE="1")
p = subprocess.Popen([root + r"\bin\agentdesk.exe"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.PIPE, env=env, text=True, encoding="utf-8", cwd=os.path.dirname(os.path.abspath(__file__)))
n = 0


def rpc(method, params=None, notify=False):
    global n
    msg = {"jsonrpc": "2.0", "method": method, **({"params": params} if params is not None else {})}
    if not notify:
        n += 1
        msg["id"] = n
    p.stdin.write(json.dumps(msg) + "\n"); p.stdin.flush()
    if notify:
        return None
    t = time.perf_counter()
    while True:
        line = json.loads(p.stdout.readline())
        if line.get("id") == n:
            return line, (time.perf_counter() - t) * 1000


def call(name, **args):
    r, ms = rpc("tools/call", {"name": name, "arguments": args})
    text = r["result"]["content"][0]["text"]
    return json.loads(text), ms


t0 = time.perf_counter()
init, _ = rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}})
print("initialize:", init["result"]["serverInfo"], f"({(time.perf_counter()-t0)*1000:.0f} ms incl. core start)")
rpc("notifications/initialized", notify=True)
tools, ms = rpc("tools/list")
print("tools:", len(tools["result"]["tools"]), f"{ms:.1f} ms")
for name, args in [("list_threads", {"channel": "question", "limit": 3}), ("open_questions", {}),
                   ("read_thread", {"thread_id": 335}), ("search_messages", {"query": "slack", "limit": 2}),
                   ("post_message", {"channel": "discussion", "subject": "e2e smoke", "body": "hello from the C# core"}),
                   ("recent_messages", {"limit": 2}), ("search_vault", {"query": "agentdesk slack bridge", "k": 1}),
                   ("post_message", {"channel": "nope", "subject": "x", "body": "y"})]:
    doc, ms = call(name, **args)
    summary = {k: (v if not isinstance(v, (list, dict)) else f"<{type(v).__name__} {len(v)}>") for k, v in list(doc.items())[:4]} if isinstance(doc, dict) else doc
    print(f"{name:16} {ms:7.1f} ms  {json.dumps(summary)[:150]}")
p.stdin.close()
p.wait(5)
