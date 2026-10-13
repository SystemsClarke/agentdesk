"""The window's speed: the hidden `ui-bench` command, and that a long thread opens and scrolls through the binary.

These have no counterpart in the original (its window was never measured); the features are the Rust port's own, and what
they guard is in docs/ui-bench.md."""
import json

from lib import feature

SRC = "n/a (the Rust port's own: docs/ui-bench.md)"


@feature("perf.bench", "perf", "ui-bench (hidden): refuses to run without a data folder of its own, and prints the window's timings as JSON", SRC)
def _bench(b):
    # no AGENTDESK_DATA: it must stop before opening anything (an empty value is "unset" to the build)
    r = b.run("ui-bench", "--quick", env={"AGENTDESK_DATA": ""})
    assert r.returncode != 0 and "AGENTDESK_DATA" in (r.stderr + r.stdout), "it ran without a data folder of its own:\n%s%s" % (r.stdout, r.stderr)
    # on a seeded board it prints every section
    b.tool("ask_human", {"subject": "Deploy?", "body": "Ship now?", "author": "builder"}, session="s1")
    b.tool("post_message", {"channel": "discussion", "subject": "Notes", "body": "# Notes\n**bold** and `code`", "author": "researcher"}, session="s2")
    r = b.run("ui-bench", "--quick", "--runs", "2", timeout=180)
    assert r.returncode == 0, r.stderr
    d = json.loads(r.stdout)
    for section in ("board", "reload_ms", "draw_ms", "render_ms", "key_to_frame_ms", "terminal_frame", "event_loop"):
        assert section in d, "no %s section in %s" % (section, list(d))
    assert d["board"]["threads"] == 2
    frames = {(f["writer"].split(":")[0], f["frame"]): f for f in d["terminal_frame"]}
    assert frames[("after", "first (full screen)")]["write_calls"] == 1, "a frame must be one write: %s" % frames
    assert frames[("after", "one row changed (down key)")]["bytes"] < frames[("after", "first (full screen)")]["bytes"], "a changed row must cost less than the screen"
    assert d["event_loop"]["idle: redraws requested"] == 0, "an idle window must not ask for frames"


@feature("perf.long-thread", "perf", "A long thread opens at its end, scrolls back to its start, and never draws more than the lines in view", SRC)
def _long_thread(b):
    tid = b.tool("post_message", {"channel": "discussion", "subject": "A long one", "body": "the very first message", "author": "researcher"}, session="s1")["thread_id"]
    for i in range(1, 40):
        b.tool("answer_thread", {"thread_id": tid, "body": "reply number %d\n\nwith **bold** and a list:\n\n- a\n- b" % i, "author": "researcher"}, session="s1")
    end = b.snap("d enter", size="100x20")["text"]
    assert "reply number 39" in end and "the very first message" not in end, end
    start = b.snap("d enter pgup " * 1 + "pgup " * 40, size="100x20")["text"]
    assert "the very first message" in start and "reply number 39" not in start, start
    mid = b.snap("d enter pgup pgup pgup", size="100x20")["text"]
    assert "reply number" in mid and "reply number 39" not in mid, mid
