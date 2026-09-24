"""The Python half of AgentDesk.Core: a JSON-RPC 2.0 loop over stdin/stdout, one request per line.

The C# core starts `python -m agentdesk.plugin`, restarts it if it dies, and calls the methods
below for the jobs Python does best. Logs go to stderr; stdout is the protocol.
"""

from __future__ import annotations

import json
import sys
import traceback

from agentdesk import db, vault, vault_search


def mirror_thread(thread_id: int) -> dict:
    conn = db.connect()
    try:
        return vault.mirror_thread(conn, thread_id)
    finally:
        conn.close()


def search(query: str, k: int = 8, full: int = 0) -> dict:
    try:
        hits = vault_search.search(query, k=k)
    except vault_search.VaultSearchUnavailable as exc:
        return {"error": str(exc)}
    for hit in hits[:full]:
        hit["body"] = vault_search.read_note(hit["path"])
    return {"hits": hits}


METHODS = {"vault.mirror_thread": mirror_thread, "vault.search": search, "ping": lambda: "pong"}


def main() -> None:
    out = sys.stdout
    sys.stdout = sys.stderr  # a stray print must never corrupt the protocol
    for line in sys.stdin:
        if not line.strip():
            continue
        reply = {"jsonrpc": "2.0", "id": None}
        try:  # a bad line gets an error reply; it never ends the loop
            req = json.loads(line.lstrip("﻿"))
            reply["id"] = req.get("id")
            reply["result"] = METHODS[req["method"]](**(req.get("params") or {}))
        except Exception as exc:
            traceback.print_exc()
            reply["error"] = {"code": -32000, "message": f"{type(exc).__name__}: {exc}"}
        out.write(json.dumps(reply, ensure_ascii=False) + "\n")
        out.flush()


if __name__ == "__main__":
    main()
