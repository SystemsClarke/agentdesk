"""Acceptance checks for @mentions (work item #111).

John's ask: let an agent aim a message at a specific other agent and have
that be a distinct, filterable thing -- a "PMs area" -- rather than prose in
the body nobody but a careful reader notices.

Investigated first, on thread 111: there are two populations of "agent" --
the long-lived crew roles (builder/verifier/researcher), which already poll
the board on a loop, and ad-hoc Claude Code sessions, which do not poll
anything and can only be reached through Claude Code's own undocumented,
key-guarded cross-session daemon (~/.claude/daemon/*). Depending on that from
a standalone SQLite/Tkinter app would tie AgentDesk's behaviour to a private
harness internal it has no business assuming. So this is the version the
investigation's own fallback describes: a mention is recorded and answered on
demand ("messages that mention me"), honest about not being a push, PLUS one
real interrupt-shaped case that costs nothing extra: a crew role already polls,
so @mentioning one by name in an item's text can steer the coordinator's
routing outright instead of guessing.

Three surfaces, three sections:

  1. db.py -- extraction (what counts as a mention, what must NOT), storage
     (meta.mentions appears only when something was actually mentioned), and
     retrieval (db.list_mentions: case-insensitive, newest first, exact name).
  2. mcp_server.py -- the list_mentions tool: defaults to the caller's own
     resolved identity with no argument, takes an explicit name otherwise.
  3. crew.py -- an item whose text @mentions a role name routes there
     directly, with no heuristic guess and no model call; the dry-run preview
     reports the same thing a real run would do.

    python scripts/check_mentions.py
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path

_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-mentions-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

from agentdesk import crew, db, mcp_server, paths  # noqa: E402

FAILURES: list[str] = []


def check(label: str, got, want) -> None:
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<58} {got!r}")


def hr(title: str) -> None:
    print(f"\n--- {title} " + "-" * max(0, 74 - len(title)))


# --- 1. extraction ---------------------------------------------------------

def check_extraction() -> None:
    hr("1. what counts as a mention, and what must not")

    check("a bare mention", db.extract_mentions("ping @builder please"),
          ["builder"])
    check("two distinct mentions, in first-seen order",
          db.extract_mentions("@builder and @verifier, both of you"),
          ["builder", "verifier"])
    check("a repeated mention is listed once",
          db.extract_mentions("@builder @builder @builder"), ["builder"])
    check("a session-shaped name (':' and '#')",
          db.extract_mentions("cc @claude-code:GoCDPipelineTool#59cb"),
          ["claude-code:GoCDPipelineTool#59cb"])
    check("a hyphenated agent name",
          db.extract_mentions("see @gocd-pipeline-ops on this"),
          ["gocd-pipeline-ops"])
    check("case is preserved as written",
          db.extract_mentions("@Builder can you look"), ["Builder"])

    check("an email address is NOT a mention",
          db.extract_mentions("contact user@example.com about this"), [])
    check("mid-word '@' is not a mention (not just email-shaped)",
          db.extract_mentions("weirdcase@builder"), [])
    check("a bare '@' with nothing name-shaped after it",
          db.extract_mentions("look at this @ sign"), [])
    check("'@' followed by a digit is not a name (must start with a letter)",
          db.extract_mentions("meeting @2pm today"), [])
    check("a mention inside a fenced code block is not extracted",
          db.extract_mentions("text\n```\n@echo off\n```\nmore text"), [])
    check("a real mention survives NEXT TO a fenced block",
          db.extract_mentions("```\n@echo off\n```\nreal ping: @builder"),
          ["builder"])
    check("no '@' at all", db.extract_mentions("just a normal sentence"), [])
    check("empty body", db.extract_mentions(""), [])


# --- 2. storage --------------------------------------------------------------

def check_storage(conn) -> dict:
    hr("2. storage: meta.mentions on start_thread and reply")

    tid = db.start_thread(conn, "discussion", "SEEDMENTION opening",
                          "researcher", paths.AGENT_KIND,
                          "posting this for @builder to see")
    row = conn.execute(
        "SELECT meta FROM messages WHERE thread_id=? ORDER BY id LIMIT 1",
        (tid,)).fetchone()
    meta = json.loads(row["meta"])
    check("the opening message records the mention",
          meta.get("mentions"), ["builder"])

    mid_plain = db.reply(conn, tid, "verifier", paths.AGENT_KIND,
                         "no mention in this one")
    row2 = conn.execute(
        "SELECT meta FROM messages WHERE id=?", (mid_plain,)).fetchone()
    check("a reply with no '@' gets no mentions key at all "
          "(not an empty list on every message forever)",
          row2["meta"] is None or "mentions" not in json.loads(row2["meta"]),
          True)

    mid_multi = db.reply(conn, tid, "builder", paths.AGENT_KIND,
                         "looping in @verifier and @researcher on this",
                         meta={"kind": "crew-report"})
    row3 = conn.execute(
        "SELECT meta FROM messages WHERE id=?", (mid_multi,)).fetchone()
    meta3 = json.loads(row3["meta"])
    check("mentions coexist with an existing meta key rather than replacing it",
          meta3.get("kind"), "crew-report")
    check("multiple mentions in a reply are all recorded",
          sorted(meta3.get("mentions") or []), ["researcher", "verifier"])

    return {"t": tid, "mid_multi": mid_multi}


# --- 3. retrieval --------------------------------------------------------------

def check_retrieval(conn, ids: dict) -> None:
    hr("3. db.list_mentions: case-insensitive, newest first, exact name")

    got = db.list_mentions(conn, "builder")
    check("exactly one message mentions 'builder'", len(got), 1)
    check("...and it is the opening message",
          got[0]["body"], "posting this for @builder to see")

    got_ci = db.list_mentions(conn, "BUILDER")
    check("lookup is case-insensitive", [m["id"] for m in got_ci],
          [m["id"] for m in got])

    got_v = db.list_mentions(conn, "verifier")
    check("'verifier' is mentioned once", len(got_v), 1)
    check("...in the multi-mention reply", got_v[0]["id"], ids["mid_multi"])

    check("a name nobody mentioned returns nothing",
          db.list_mentions(conn, "nobody-ever-said-this"), [])

    # Newest-first and limit, driven by a second, later mention of the same name.
    tid2 = db.start_thread(conn, "discussion", "SEEDMENTION second",
                           "builder", paths.AGENT_KIND,
                           "one more thing for @verifier")
    got_v2 = db.list_mentions(conn, "verifier")
    check("a later mention of the same name sorts first (newest-first)",
          got_v2[0]["thread_id"], tid2)
    check("...and the earlier one is still there, second",
          got_v2[1]["id"], ids["mid_multi"])
    got_v_limited = db.list_mentions(conn, "verifier", limit=1)
    check("limit is respected", len(got_v_limited), 1)


# --- 4. the MCP tool -----------------------------------------------------------

def check_mcp_tool() -> None:
    hr("4. mcp_server.list_mentions: explicit name, and default-to-self")

    explicit = json.loads(mcp_server.list_mentions(name="builder"))
    check("explicit name returns the mentions for that name",
          len(explicit.get("mentions") or []) >= 1, True)

    # No name given: identity.resolve falls back to AGENTDESK_AUTHOR (the
    # crew's own stamping mechanism, see identity.py) rather than the
    # anonymous default, so this exercises the same self-lookup a role's own
    # "did anyone address me" call would make with no argument.
    old = os.environ.get("AGENTDESK_AUTHOR")
    os.environ["AGENTDESK_AUTHOR"] = "verifier"
    try:
        default_self = json.loads(mcp_server.list_mentions())
    finally:
        if old is None:
            os.environ.pop("AGENTDESK_AUTHOR", None)
        else:
            os.environ["AGENTDESK_AUTHOR"] = old
    check("omitting name resolves to the caller's own stamped identity",
          len(default_self.get("mentions") or []) >= 1, True)
    check("...and every returned row actually mentions 'verifier'",
          all("verifier" in (json.loads(m["meta"]).get("mentions") or [])
              for m in default_self["mentions"]), True)


# --- 5. crew routing -----------------------------------------------------------

def check_crew_routing() -> None:
    hr("5. an @mention steers the coordinator directly, no model call needed")

    check("a plain item has no mentioned role",
          crew._mentioned_role("fix the thing", "no name in here"), None)
    check("@builder in the subject is found",
          crew._mentioned_role("@builder please look", "body text"), "builder")
    check("@verifier in the body is found",
          crew._mentioned_role("subject", "can @verifier check this claim"),
          "verifier")
    check("case-insensitive, and normalised to lowercase",
          crew._mentioned_role("@ReSeArChEr, what do we know about this", ""),
          "researcher")
    check("a non-role name is not treated as a role mention",
          crew._mentioned_role("cc @john on this", "for context"), None)

    # _route with an explicit mention must return WITHOUT touching the
    # keyword heuristic or spawning the CLI at all -- proven by never reaching
    # the "researcher hint" words that would otherwise win, and by returning
    # instantly rather than hanging on a subprocess call.
    item = {"subject": "@verifier: reproduce this before we trust it",
            "id": 9999, "last_body": "find out where this comes from"}
    role, why = crew._route(item)
    check("an @mention wins over words that would route elsewhere by keyword",
          role, "verifier")
    check("the reason names the mention, not a model or a guess",
          "mention" in why, True)

    # _route() itself is not called here for the no-mention case: with no
    # mention it goes on to spawn the real `claude` CLI to confirm the
    # keyword guess, which is a real subprocess call this check must not
    # make. _heuristic_route is the pure function _route falls back to when
    # no mention and no model answer are available, and it is what the
    # no-mention half of _route's contract rests on.
    check("with no mention, the keyword heuristic still applies",
          crew._heuristic_route("reproduce the failure",
                                "verify that this actually happens"),
          crew.roles.VERIFIER.name)


def main() -> int:
    check_extraction()

    conn = db.connect()
    db.init_db(conn)
    ids = check_storage(conn)
    check_retrieval(conn, ids)
    conn.close()

    check_mcp_tool()
    check_crew_routing()

    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        print(f"\nscratch board left at {_SCRATCH}")
        return 1
    print("all checks passed")
    print(f"\nscratch board left at {_SCRATCH}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
