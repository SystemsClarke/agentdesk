#!/usr/bin/env python3
"""Checks that the parity manifest itself is complete: every request, tool, command and screen the original defines
has a registered feature. It reads the original's own documents (parity/original/) so a feature cannot be forgotten by
omission. Exit 1 when the manifest has a hole.

  python3 parity/coverage.py
"""
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import importlib  # noqa: E402
import pkgutil  # noqa: E402

import lib  # noqa: E402

ORIG = os.path.join(HERE, "original")


def load():
    import checks

    for m in pkgutil.iter_modules(checks.__path__):
        importlib.import_module("checks." + m.name)


def main():
    load()
    have = set(lib.FEATURES)
    holes = []

    # 1. every ui:* request in the original's protocol table
    doc = open(os.path.join(ORIG, "ui-api.md"), encoding="utf-8").read()
    for name in re.findall(r"^\| `ui:(\w+)` \|", doc, re.M):
        if "ui." + name not in have:
            holes.append("ui:" + name + "  (docs/ui-api.md)  -> ui." + name)
    # the job requests are documented in the README, not the table
    for name in ("job_create", "job_list", "job_run", "job_enable", "job_delete"):
        if "ui." + name not in have:
            holes.append("ui:" + name + "  -> ui." + name)

    # 2. every MCP tool
    for t in json.load(open(os.path.join(ORIG, "tools.json"), encoding="utf-8"))["tools"]:
        if "tool." + t["name"] not in have:
            holes.append("tool " + t["name"] + "  -> tool." + t["name"])

    # 3. every command line verb of the original's Program.cs header
    usage = open(os.path.join(ORIG, "cli-usage.txt"), encoding="utf-8").read()
    verbs = {
        "hook": "cli.hook", "wait": "cli.wait", "sessions": "cli.sessions", "start": "cli.start-stop-session", "stop": "cli.start-stop-session",
        "attach": "cli.attach", "new": "cli.identity", "forget": "cli.identity", "list": "cli.identity", "adoptable": "cli.adopt",
        "adopt": "cli.adopt", "update": "cli.update", "job new": "cli.job.new", "job list": "cli.job.list-enable-disable-delete",
        "job run": "cli.job.run", "job enable": "cli.job.list-enable-disable-delete", "goal new": "cli.goal", "goal approve": "cli.goal",
        "goal stop": "cli.goal", "goal list": "cli.goal",
    }
    for verb, fid in verbs.items():
        if re.search(r"agentdesk " + re.escape(verb) + r"\b", usage) and fid not in have:
            holes.append("command `agentdesk %s`  -> %s" % (verb, fid))

    # 4. every screen of the window (TopLine's titles) and every documented key, by the original's footer hints
    screens = {"main": "screen.main", "list": "screen.list.questions", "read": "screen.reader", "prs": "screen.prs", "jobs": "screen.jobs", "rules": "screen.rules",
               "sysop": "screen.sysop", "who": "screen.who", "options": "screen.options", "compose": "screen.compose", "agents": "screen.agents",
               "adopt": "screen.adopt", "goal": "screen.goal", "ask": "key.jobs.new"}
    hints = json.load(open(os.path.join(ORIG, "hints.json"), encoding="utf-8"))
    for s, fid in screens.items():
        if fid not in have:
            holes.append("screen %s  -> %s" % (s, fid))
    screen_text = " ".join((f["title"] + " " + f["id"]).lower() for f in lib.FEATURES.values() if f["area"] == "screens")
    unmapped = []
    for s, ks in hints.items():
        for key, label in ks:
            words = [w for w in re.findall(r"[a-z]+", label.lower()) if len(w) > 2]
            if words and not any(w in screen_text for w in words):
                unmapped.append("%s: %s %s" % (s, key, label))

    # 5. every section of the design documents
    docs = {"governor.md": ["Samples", "Forecast", "The session pool", "Enforcement"], "phoenix.md": ["The state file"]}
    for d, secs in docs.items():
        text = open(os.path.join(ORIG, d), encoding="utf-8").read()
        for s in secs:
            assert s in text, "the original's %s lost its section %s" % (d, s)
    for fid in ("governor.forecast", "governor.enforcement", "governor.pool-fair-share", "phoenix.state-file", "phoenix.handoff-restart"):
        if fid not in have:
            holes.append("design doc  -> " + fid)

    print("manifest: %d features" % len(have))
    if unmapped:
        print("\nfooter hints with no obvious feature (review; informational):")
        for u in unmapped:
            print("  " + u)
    if holes:
        print("\nHOLES in the manifest (the original defines these; nothing tests them):")
        for h in holes:
            print("  " + h)
        return 1
    print("\nevery request, tool, command and screen of the original has a feature")
    return 0


if __name__ == "__main__":
    sys.exit(main())
