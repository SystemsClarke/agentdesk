"""A stand-in for Slack that the native (Rust) bridge talks to over real sockets, standard library only.

The Web API is an HTTP server on 127.0.0.1 (the bridge is pointed at it with SLACK_API_URL); Socket Mode is a websocket server on another
port, whose address `apps.connections.open` hands out. Every Web API call is appended as a JSON line to `log`, in the shape the slack_bolt
fake (`fakes/slack_bolt`) uses, so the checks read both the same way:

    post    chat.postMessage (its arguments, and the `ts` it was given)
    create  conversations.create (name)        rename / invite / archive: likewise
    ack     an acknowledgement the bridge sent for a Socket Mode envelope (envelope_id)

`conversations.history` answers from `history` (a JSON file: channel -> [messages]), like the bolt fake. `events` is a JSON-lines file the
check appends to: `{"event": {...}}` is delivered to the bridge as an `events_api` envelope, `{"quit": true}` ends the fake.
"""
import base64
import hashlib
import json
import os
import socket
import struct
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


class FakeSlack:
    def __init__(self, log, events, history=None):
        self.log_path, self.events_path, self.history_path = log, events, history
        self.lock = threading.Lock()
        self.n = 0
        self.quit = False
        self.conn = None  # the bridge's websocket, once it has connected
        self.pending = []  # envelopes waiting for it
        outer = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"

            def log_message(self, *a):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("Content-Length") or 0)).decode("utf-8")
                if "json" in (self.headers.get("Content-Type") or ""):
                    args = json.loads(body or "{}")
                else:
                    args = {k: v[0] for k, v in urllib.parse.parse_qs(body, keep_blank_values=True).items()}
                method = self.path.rsplit("/", 1)[-1]
                self._send(outer.api(method, args, self.headers.get("Authorization", "")))

            def do_GET(self):  # a private file's download (a photo in a reply)
                data = b"\x89PNG fake"
                self.send_response(200)
                self.send_header("Content-Type", "image/png")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def _send(self, doc):
                data = json.dumps(doc).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.http.daemon_threads = True
        self.ws = socket.socket()
        self.ws.bind(("127.0.0.1", 0))
        self.ws.listen(4)
        self.api_url = "http://127.0.0.1:%d/api" % self.http.server_address[1]
        self.ws_port = self.ws.getsockname()[1]
        for target in (self.http.serve_forever, self._accept, self._watch):
            threading.Thread(target=target, daemon=True).start()

    # ---- the Web API

    def record(self, kind, **kw):
        with self.lock:
            with open(self.log_path, "a", encoding="utf-8") as f:
                f.write(json.dumps(dict(kind=kind, **kw), default=str) + "\n")

    def ts(self):
        with self.lock:
            self.n += 1
            return "%d.%04d" % (1700000000 + self.n, self.n)

    def api(self, method, a, auth):
        if method == "chat.postMessage":
            ts = self.ts()
            self.record("post", ts=ts, **a)
            return {"ok": True, "ts": ts}
        if method == "conversations.create":
            self.record("create", name=a["name"])
            return {"ok": True, "channel": {"id": "C" + a["name"][-6:].upper().replace("-", "X"), "name": a["name"]}}
        if method == "conversations.rename":
            self.record("rename", channel=a["channel"], name=a["name"])
            return {"ok": True, "channel": {"id": a["channel"], "name": a["name"]}}
        if method in ("conversations.invite", "conversations.archive"):
            self.record(method.split(".")[1], **a)
            return {"ok": True}
        if method == "conversations.history":
            msgs = json.load(open(self.history_path)).get(a["channel"], []) if self.history_path and os.path.exists(self.history_path) else []
            msgs = sorted(msgs, key=lambda m: -float(m["ts"]))
            if a.get("oldest"):
                msgs = [m for m in msgs if float(m["ts"]) > float(a["oldest"])]
            return {"ok": True, "messages": msgs[: int(a.get("limit") or 100)]}
        if method == "apps.connections.open":
            return {"ok": True, "url": "ws://127.0.0.1:%d/link/?ticket=fake" % self.ws_port}
        return {"ok": True}

    # ---- Socket Mode

    def deliver(self, event):
        env = {"envelope_id": "env-%d" % int(time.time() * 1000000), "type": "events_api", "accepts_response_payload": False,
               "payload": {"type": "event_callback", "event": event}}
        with self.lock:
            self.pending.append(env)

    def _accept(self):
        while not self.quit:
            try:
                c, _ = self.ws.accept()
            except OSError:
                return
            threading.Thread(target=self._serve, args=(c,), daemon=True).start()

    def _serve(self, c):
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = c.recv(4096)
            if not chunk:
                return
            head += chunk
        key = [l.split(":", 1)[1].strip() for l in head.decode().split("\r\n") if l.lower().startswith("sec-websocket-key")][0]
        accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
        c.sendall(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: %s\r\n\r\n" % accept).encode())
        self.send(c, json.dumps({"type": "hello", "num_connections": 1}))
        self.conn = c
        threading.Thread(target=self._read, args=(c,), daemon=True).start()
        while not self.quit and self.conn is c:
            with self.lock:
                env = self.pending.pop(0) if self.pending else None
            if env is None:
                time.sleep(0.05)
                continue
            try:
                self.send(c, json.dumps(env))
            except OSError:
                with self.lock:
                    self.pending.insert(0, env)
                return

    @staticmethod
    def send(c, text):
        data = text.encode("utf-8")
        n = len(data)
        head = bytes([0x81]) + (bytes([n]) if n < 126 else bytes([126]) + struct.pack(">H", n) if n < 65536 else bytes([127]) + struct.pack(">Q", n))
        c.sendall(head + data)

    def _read(self, c):
        """Client frames are masked: collect the text ones (the acknowledgements)."""
        buf = b""
        try:
            while True:
                while len(buf) < 2:
                    d = c.recv(4096)
                    if not d:
                        return
                    buf += d
                op, n = buf[0] & 0x0F, buf[1] & 0x7F
                pos = 2
                if n == 126:
                    while len(buf) < 4:
                        buf += c.recv(4096)
                    n, pos = struct.unpack(">H", buf[2:4])[0], 4
                elif n == 127:
                    while len(buf) < 10:
                        buf += c.recv(4096)
                    n, pos = struct.unpack(">Q", buf[2:10])[0], 10
                while len(buf) < pos + 4 + n:
                    d = c.recv(4096)
                    if not d:
                        return
                    buf += d
                mask, payload = buf[pos:pos + 4], bytearray(buf[pos + 4:pos + 4 + n])
                buf = buf[pos + 4 + n:]
                for i in range(n):
                    payload[i] ^= mask[i % 4]
                if op == 1:
                    self.record("ack", **json.loads(payload.decode("utf-8")))
                elif op == 8:
                    return
        except OSError:
            return

    # ---- the check's side: events from a file

    def _watch(self):
        seen = 0
        while not self.quit:
            lines = open(self.events_path, encoding="utf-8").read().splitlines() if os.path.exists(self.events_path) else []
            for line in lines[seen:]:
                msg = json.loads(line)
                if msg.get("quit"):
                    self.quit = True
                    return
                self.deliver(msg["event"])
            seen = len(lines)
            time.sleep(0.1)

    def stop(self):
        self.quit = True
        for s in (self.ws,):
            try:
                s.close()
            except OSError:
                pass
        try:
            self.http.shutdown()
        except Exception:
            pass
