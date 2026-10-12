"""Harness for the parity suite: an isolated board, a driver for the agentdesk binary, and the check registry.

Standard library only. Every check gets a fresh `Board` (its own AGENTDESK_DATA folder, so nothing touches real data).
A check passes by returning, fails by raising AssertionError, and says "this feature does not exist yet" by raising
`Missing`. The registry is the manifest: a feature is whatever is registered here, and `coverage.py` cross-checks
the registry against the original's own documents so a feature cannot be left out by omission.
"""
import json
import os
import shutil
import site
import sqlite3
import stat
import subprocess
import threading
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ORIGINAL = os.path.join(os.path.dirname(os.path.abspath(__file__)), "original")
EXE = "agentdesk.exe" if os.name == "nt" else "agentdesk"


class Missing(Exception):
    """The feature is not implemented in this build (or only partly: the reason says what is absent)."""


class Waived(Exception):
    """The feature is deliberately not reproduced (the reason is the message)."""


FEATURES = {}


def feature(fid, area, title, source):
    """Register a check. `source` points at where the original defines the behaviour."""

    def deco(fn):
        if fid in FEATURES:
            raise RuntimeError("duplicate feature id " + fid)
        FEATURES[fid] = {"id": fid, "area": area, "title": title, "source": source, "fn": fn}
        return fn

    return deco


def binary():
    p = os.environ.get("AGENTDESK_BIN")
    if p:
        return p
    for profile in ("release", "debug"):
        c = os.path.join(ROOT, "target", profile, EXE)
        if os.path.exists(c):
            return c
    raise SystemExit("no agentdesk binary: run `cargo build` or set AGENTDESK_BIN")


_STUBHOST_LOCK = threading.Lock()


def stubhost():
    """The prebuilt Windows stand-in launcher (parity/stubhost), built on first use."""
    manifest = os.path.join(os.path.dirname(os.path.abspath(__file__)), "stubhost", "Cargo.toml")
    exe = os.path.join(os.path.dirname(manifest), "target", "release", "stubhost.exe")
    with _STUBHOST_LOCK:
        if not os.path.exists(exe):
            subprocess.run(["cargo", "build", "--release", "--quiet", "--manifest-path", manifest], check=True, env=dict(os.environ, CARGO_HTTP_CHECK_REVOKE="false"))
    return exe


def make_stub(directory, name, python_body):
    """A stand-in executable (a `claude` or `gh`) that runs `python_body` with sys.argv; works on every OS."""
    os.makedirs(directory, exist_ok=True)
    script = os.path.join(directory, name + "_stub.py")
    with open(script, "w", encoding="utf-8", newline="\n") as f:
        f.write(python_body)
    if os.name == "nt":
        # a real .exe: CreateProcess cannot run a .cmd, and cmd /c cuts an argument at its first newline
        path = os.path.join(directory, name + ".exe")
        shutil.copyfile(stubhost(), path)
        with open(os.path.join(directory, name + "_python.txt"), "w", encoding="utf-8") as f:
            f.write(sys.executable)
    else:
        path = os.path.join(directory, name)
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write('#!/bin/sh\nexec "%s" "%s" "$@"\n' % (sys.executable, script))
        os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)
    return path


class Board:
    """One isolated board and the ways to drive it."""

    def __init__(self):
        self.dir = tempfile.mkdtemp(prefix="agentdesk-parity-")
        self.data = os.path.join(self.dir, "data")
        self.home = os.path.join(self.dir, "home")
        self.work = os.path.join(self.dir, "work")
        self.tmp = os.path.join(self.dir, "tmp")  # scratch: the build ignores folders under it when it ranks where John works
        for d in (self.data, self.home, self.work, self.tmp):
            os.makedirs(d)
        self.bin = binary()
        self.env = dict(os.environ)
        self.env.update({
            "AGENTDESK_DATA": self.data,
            "AGENTDESK_NO_TOAST": "1",
            "AGENTDESK_NO_UPDATE_CHECK": "1",
            "AGENTDESK_NO_MODEL_DOWNLOAD": "1",
            "AGENTDESK_VAULT": os.path.join(self.dir, "vault"),
            "HOME": self.home,
            "USERPROFILE": self.home,
            "PYTHONUSERBASE": site.getuserbase(),  # packages pip installed for the user (numpy) must survive the moved HOME
            "TMPDIR": self.tmp,
            "TEMP": self.tmp,
            "TMP": self.tmp,
        })
        for k in list(self.env):
            if k.startswith("CLAUDE") or k in ("AGENTDESK_AUTHOR", "AGENTDESK_IDENTITY", "AGENTDESK_SESSION"):
                del self.env[k]
        self.stubs = os.path.join(self.dir, "stubs")
        self._procs = []

    def close(self):
        for p in self._procs:
            try:
                p.kill()
            except Exception:
                pass
        shutil.rmtree(self.dir, ignore_errors=True)

    # ---- running the binary

    def run(self, *args, input=None, env=None, timeout=60):
        e = dict(self.env)
        e.update(env or {})
        r = subprocess.run([self.bin, *args], input=input, capture_output=True, text=True, encoding="utf-8", errors="replace", env=e, timeout=timeout, cwd=self.work)
        return r

    def spawn(self, *args, env=None):
        e = dict(self.env)
        e.update(env or {})
        p = subprocess.Popen([self.bin, *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace", env=e, cwd=self.work)
        self._procs.append(p)
        return p

    def ui(self, request, args=None, env=None):
        """`agentdesk ui` -> (json document, exit code)."""
        a = [request] + ([json.dumps(args)] if args is not None else [])
        r = self.run("ui", *a, env=env)
        try:
            return json.loads(r.stdout), r.returncode
        except ValueError:
            raise AssertionError("ui %s printed no JSON: stdout=%r stderr=%r" % (request, r.stdout[:300], r.stderr[:300]))

    def need(self, request, args=None, env=None):
        """Like ui(), but `not implemented` raises Missing and any other error fails the check."""
        doc, rc = self.ui(request, args, env)
        if rc == 2:
            raise Missing(doc.get("error", "not implemented"))
        assert rc == 0, "%s failed: %s" % (request, doc)
        return doc

    def snap(self, keys="", size="110x36", env=None):
        r = self.run("ui-snapshot", "--json", "--size", size, "--keys", keys, env=env)
        assert r.returncode == 0, "ui-snapshot failed: " + r.stderr[:300]
        d = json.loads(r.stdout)
        d["text"] = "\n".join(d["lines"])
        return d

    # ---- MCP

    def mcp(self, env=None):
        return Mcp(self.spawn("mcp", env=env))

    def tool(self, name, args=None, session="s-1", author=None, cwd=None):
        """One tool call through a fresh MCP server, as a Claude Code session with this session id."""
        env = {"CLAUDE_CODE_SESSION_ID": session, "CLAUDECODE": "1"}
        if author:
            env["AGENTDESK_AUTHOR"] = author
        m = self.mcp(env=env)
        try:
            m.initialize()
            return m.call(name, args or {})
        finally:
            m.close()

    # ---- the board file

    def sql(self, query, params=()):
        c = sqlite3.connect(os.path.join(self.data, "agentdesk.db"))
        c.row_factory = sqlite3.Row
        try:
            rows = [dict(r) for r in c.execute(query, params)]
            c.commit()
            return rows
        finally:
            c.close()

    def stub(self, name, body):
        path = make_stub(self.stubs, name, body)
        self.env["PATH"] = self.stubs + os.pathsep + self.env.get("PATH", "")
        return path


class Mcp:
    def __init__(self, proc):
        self.p = proc
        self.n = 0

    def rpc(self, method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if not notify:
            self.n += 1
            msg["id"] = self.n
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        if notify:
            return None
        line = self.p.stdout.readline()
        assert line, "the MCP server closed: " + self.p.stderr.read()[:300]
        return json.loads(line)

    def raw(self, line):
        self.p.stdin.write(line + "\n")
        self.p.stdin.flush()
        return json.loads(self.p.stdout.readline())

    def initialize(self, version="2025-06-18"):
        r = self.rpc("initialize", {"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "parity", "version": "1"}})
        self.rpc("notifications/initialized", notify=True)
        return r["result"]

    def tools(self):
        return self.rpc("tools/list")["result"]["tools"]

    def call(self, name, args=None):
        r = self.rpc("tools/call", {"name": name, "arguments": args or {}})
        if "error" in r:
            raise AssertionError("tools/call %s: %s" % (name, r["error"]))
        text = r["result"]["content"][0]["text"]
        try:
            return json.loads(text)
        except ValueError:
            return text

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=5)
        except Exception:
            self.p.kill()


def hook(board, event, payload, session=None):
    """`agentdesk hook <event>` with a JSON payload on stdin -> parsed output or None."""
    r = board.run("hook", event, input=json.dumps(payload))
    assert r.returncode == 0, "a hook must never fail: rc=%s %s" % (r.returncode, r.stderr[:200])
    out = r.stdout.strip()
    return json.loads(out) if out else None


def wait_for(fn, timeout=10.0, step=0.1, what="condition"):
    end = time.time() + timeout
    while time.time() < end:
        v = fn()
        if v:
            return v
        time.sleep(step)
    raise AssertionError("timed out waiting for " + what)
