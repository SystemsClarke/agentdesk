"""Talk to the AgentDesk MCP server over stdio.

This session has no mcp__agentdesk__* tools exposed, so this drives the real
server instead of reaching into the SQLite file directly.
"""
import json
import os
import subprocess
import sys

REPO = r"C:\Users\palencharj\NoOneDrive\AgentDesk"
PY = REPO + r"\.venv\Scripts\python.exe"


def call(tool, args=None, timeout=60):
    p = subprocess.Popen(
        [PY, "-m", "agentdesk.mcp_server"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        env=dict(os.environ, PYTHONPATH=REPO),
        text=True, encoding="utf-8", bufsize=1,
    )

    def send(obj):
        p.stdin.write(json.dumps(obj) + "\n")
        p.stdin.flush()

    def read_until(mid):
        while True:
            line = p.stdout.readline()
            if not line:
                err = p.stderr.read()
                raise RuntimeError("server closed; stderr=%s" % err[:800])
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == mid:
                return msg

    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
              "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                         "clientInfo": {"name": "claude-code", "version": "1.0"}}})
        read_until(1)
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
              "params": {"name": tool, "arguments": args or {}}})
        msg = read_until(2)
    finally:
        try:
            p.stdin.close()
        except Exception:
            pass
        p.terminate()

    if "error" in msg:
        return {"transport_error": msg["error"]}
    res = msg.get("result", {})
    out = []
    for c in res.get("content", []):
        if c.get("type") == "text":
            out.append(c["text"])
    text = "\n".join(out)
    try:
        return json.loads(text)
    except (json.JSONDecodeError, TypeError):
        return {"raw": text, "isError": res.get("isError")}


if __name__ == "__main__":
    tool = sys.argv[1]
    args = json.loads(sys.argv[2]) if len(sys.argv) > 2 else {}
    print(json.dumps(call(tool, args), indent=2, ensure_ascii=False)[:6000])
