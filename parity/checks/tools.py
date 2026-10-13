"""The MCP contract: the 24 tools (name, description, input schema exactly as the original), the server's instructions,
the JSON-RPC behaviour, and the recorded golden scenarios replayed through the Rust tools."""
import json
import os
import subprocess

from lib import ORIGINAL, ROOT, Board, Missing, feature

ORIG = json.load(open(os.path.join(ORIGINAL, "tools.json"), encoding="utf-8"))


def _make(tool):
    name = tool["name"]

    @feature("tool." + name, "tools", "MCP tool %s: listed with the original's description and input schema" % name, "Tools.g.cs")
    def check(b: Board):
        m = b.mcp()
        try:
            m.initialize()
            listed = {t["name"]: t for t in m.tools()}
        finally:
            m.close()
        if name not in listed:
            raise Missing("tool %s is not offered" % name)
        got = listed[name]
        assert got["description"].strip() == tool["description"].strip(), "description differs from the original"
        assert got["inputSchema"] == tool["inputSchema"], "input schema differs: %s vs %s" % (got["inputSchema"], tool["inputSchema"])


for _t in ORIG["tools"]:
    _make(_t)


@feature("tool.instructions", "tools", "The server's instructions text is the original's, word for word", "Tools.g.cs Instructions")
def _instructions(b):
    m = b.mcp()
    try:
        got = m.initialize()["instructions"]
    finally:
        m.close()
    assert got == ORIG["instructions"], "instructions differ"


@feature("mcp.handshake", "tools", "initialize negotiates the protocol version, advertises tools, names the server", "Mcp.cs")
def _handshake(b):
    m = b.mcp()
    try:
        r = m.rpc("initialize", {"protocolVersion": "2025-03-26"})["result"]
        assert r["protocolVersion"] == "2025-03-26"
        assert r["serverInfo"]["name"] == "agentdesk" and "tools" in r["capabilities"]
        r2 = m.rpc("initialize", {"protocolVersion": "1999-01-01"})["result"]
        assert r2["protocolVersion"] == "2025-06-18", "an unknown version is answered with the newest known"
    finally:
        m.close()


@feature("mcp.errors", "tools", "ping, unknown method (-32601), unknown tool (-32602), parse error (-32700), notifications get no reply", "Mcp.cs")
def _errors(b):
    m = b.mcp()
    try:
        m.initialize()
        assert m.rpc("ping")["result"] == {}
        assert m.rpc("no/such/method")["error"]["code"] == -32601
        assert m.rpc("tools/call", {"name": "no_such_tool", "arguments": {}})["error"]["code"] == -32602
        assert m.raw("{not json")["error"]["code"] == -32700
        m.rpc("notifications/cancelled", notify=True)
        assert m.rpc("ping")["result"] == {}, "a notification must not produce a reply that shifts the next one"
    finally:
        m.close()


@feature("tools.golden", "tools", "All seven recorded scenarios (seed.db + golden JSON from the original server) replay exactly", "tests/AgentDesk.Tests/Parity")
def _golden(b):
    r = subprocess.run(["cargo", "test", "--quiet", "parity::"], cwd=ROOT, capture_output=True, text=True)
    assert r.returncode == 0, "golden replay failed:\n" + (r.stdout + r.stderr)[-1500:]


@feature("tools.author-resolution", "tools", "An anonymous author is posted as harness:project#tag from the session; AGENTDESK_AUTHOR wins", "identity.py / Identity.cs")
def _author(b):
    r = b.tool("post_message", {"channel": "discussion", "subject": "s", "body": "b", "author": "claude"}, session="abcd1234")
    t = b.sql("SELECT opened_by FROM threads WHERE id=?", (r["thread_id"],))[0]["opened_by"]
    assert t.startswith("claude-code:") and t.endswith("#abcd"), t
    r = b.tool("post_message", {"channel": "discussion", "subject": "s", "body": "b"}, session="zzzz", author="builder")
    assert b.sql("SELECT opened_by FROM threads WHERE id=?", (r["thread_id"],))[0]["opened_by"] == "builder"
