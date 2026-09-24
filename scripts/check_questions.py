"""Questions are answerable by every agent, and settle into the vault.

Work item #33. Each of the brief's four acceptance arms prints a line here
rather than being asserted in prose:

  1. A question with a back-and-forth -- several agent replies through the real
     MCP tool, then John's answer. The vault file is PRINTED, so a reader can
     see every message in it and in order, not take "all of them" on trust.
  2. The question leaves the Questions tab only once the transcript has landed,
     and the title-bar count drops when John answers. The vault is deliberately
     broken for this arm so the two moments are separable: answered is on the
     tab, archived is off it, and the tab is what proves the difference.
  3. While agents were replying and John had NOT answered, the question was
     still open, still in the view the toast reads, and still what the hourly
     nag announces -- with a control showing the nag can announce nothing.
  4. A second archive of the same thread writes nothing new: same status, same
     bytes, same mtime.

Plus the edge cases the brief states as rules and does not ask to be shown: an
open question is never archived however many agents replied; a non-question is
skipped; and a transcript carrying a credential shape is refused rather than
filed, with the refusal itself not reproducing the credential.

WHY THIS DRIVES THE REAL CODE. `LOCALAPPDATA` is redirected to a scratch
directory before anything imports `agentdesk.paths`, and paths reads it once at
import, so every `db.connect()` resolves to a scratch board -- which is how
`mcp_server.answer_thread` and a real `App` can be driven here directly. The
vault needs a second redirect: `paths.VAULT_DIR` is a hardcoded real path, not
derived from LOCALAPPDATA, and this script's entire job is writing transcripts.
Without the reassignment below it would file test questions into John's real
vault. Nothing caches a VAULT_* path -- every module looks them up on `paths` at
call time -- so reassigning the attributes is sufficient.

    .venv\\Scripts\\python.exe scripts\\check_questions.py
    .venv\\Scripts\\python.exe scripts\\check_questions.py --shot .\\questions
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))
sys.path.insert(0, str(REPO / "scripts"))

# Before the first agentdesk import, and it must stay before it: paths.py reads
# LOCALAPPDATA at import time and every writer in the package inherits the
# answer for the life of the process.
_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-questions-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

from agentdesk import app as appmod  # noqa: E402
from agentdesk import db, mcp_server, notify, paths, terminal, vault  # noqa: E402

# The vault redirect. See the module docstring -- this is not optional
# housekeeping, it is the difference between a test and writing into John's
# memory vault, which is a git repo he pushes.
_VAULT = _SCRATCH / "vault"
paths.VAULT_DIR = _VAULT
paths.VAULT_AGENTDESK = _VAULT / "agentdesk"
paths.VAULT_LOG = _VAULT / "log"
paths.VAULT_NOTES = _VAULT / "notes"
paths.VAULT_MAPS = _VAULT / "maps"
paths.VAULT_PARKED = paths.VAULT_AGENTDESK / "parked"
paths.VAULT_QUESTIONS = paths.VAULT_AGENTDESK / "questions"

WIDTH = 78
FAILURES: list[str] = []

OPENER_MARK = "SEEDOPEN the retry budget is being eaten by one host"
REPLY_A = "SEEDREPLY-A per-host. two hosts were eating the whole budget"
REPLY_B = "SEEDREPLY-B seconding per-host, with a per-job ceiling as a stop"
JOHN_MARK = "SEEDJOHN per-host it is. i will raise the per-job default separately"


def hr(title: str = "") -> None:
    if title:
        print(f"\n--- {title} " + "-" * max(0, WIDTH - len(title) - 5))
    else:
        print("-" * WIDTH)


def show(label: str, value) -> None:
    print(f"  {label:<44} {value}")


def check(label: str, got, want) -> None:
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<39} {got!r}")


def note(text: str) -> None:
    for line in text.strip().splitlines():
        print(f"  {line}")


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


# Every toast this run would have shown, in order. The probe is installed for
# the whole of main(), not per section, because a real App on a real desktop
# fires real banners -- several sections here create fresh open questions, and
# without the probe John would collect a notification for each one.
TOASTS: list = []


def install_toast_probe():
    """Replace notify.toast with a recorder and return the original.

    The replacement returns True rather than None on purpose:
    notify_open_questions reads a falsy return from _announce as "the toast
    failed" and declines to record what it announced, so a stub returning None
    would make every nag report zero and the control would be measuring the
    stub rather than the nag.
    """
    original = notify.toast
    notify.toast = lambda *a, **k: (TOASTS.append(a), True)[1]
    return original


def transcript_path(thread_id: int) -> Path:
    return paths.VAULT_QUESTIONS / vault.question_filename(thread_id)


def tab_ids(app, channel: str) -> list[int]:
    """What the terminal view's list for `channel` holds right now.

    The view's rows are what its list screen paints, one line per row; there
    is no separate widget tree any more to disagree with them.
    """
    return sorted(int(r["id"]) for r in app.view.rows[channel])


def tab_states(app, channel: str) -> list[tuple]:
    """(thread id, the state code the list prints for it) for every row."""
    return sorted((int(r["id"]), terminal.state_code(channel, r)[0].strip())
                  for r in app.view.rows[channel])


# --- the board under test -------------------------------------------------------

def seed(conn) -> dict:
    """One question with a back-and-forth about to happen, plus two non-questions.

    Written through db.py, the same module the app and the MCP server write
    through, so what is measured is a real board rather than a fixture shaped
    like one. The work and discussion threads exist only to be refused later:
    archiving must be a question thing and nothing else.
    """
    ids = {}
    ids["q"] = db.start_thread(
        conn, "question", "Should the retry budget be per-host or per-job?",
        "researcher", paths.AGENT_KIND, OPENER_MARK)
    ids["work"] = db.start_thread(
        conn, "work", "The window forgets where I put it",
        "claude-code", paths.AGENT_KIND, "Acceptance: the geometry survives.")
    ids["disc"] = db.start_thread(
        conn, "discussion", "A discussion that is not a question",
        "researcher", paths.AGENT_KIND, "Nothing to see here.")
    conn.commit()
    return ids


def answer_as_agent(thread_id: int, body: str, agent: str) -> dict:
    """Reply through the REAL MCP tool, not db.reply.

    The item's subject is that every agent may answer a question, so the arm
    that proves it has to go through the tool an agent would actually call --
    including its identity resolution and its own status handling.
    """
    return json.loads(mcp_server.answer_thread(thread_id, body, author=agent))


# --- 1. agents replying does not settle anything --------------------------------

def check_agents_cannot_settle(ids: dict) -> None:
    hr("1. agents answer it; the question is still John's to settle")
    conn = db.connect(paths.DB_PATH)
    try:
        before = db.open_questions(conn)
        show("open questions before any reply", len(before))

        for agent, body in (("verifier", REPLY_A), ("builder", REPLY_B)):
            res = answer_as_agent(ids["q"], body, agent)
            show(f"{agent}'s answer_thread result", res.get("ok"))
            st = conn.execute("SELECT status FROM threads WHERE id=?",
                              (ids["q"],)).fetchone()["status"]
            show(f"  question status after {agent} replied", repr(st))

        after = db.open_questions(conn)
        show("open questions after two agent replies", len(after))
        check("agent replies left it open", len(after), 1)
        check("...and it is still the one open question",
              after[0]["thread_id"] if after else None, ids["q"])

        # THE TOAST ARM. notify.notify_open_questions is the hourly nag, and the
        # view it reads is the same one the window toasts from. A fresh state
        # file is used because the nag only speaks when the SET changes -- an
        # existing state file would make it stay quiet for the wrong reason,
        # namely that it had already spoken.
        state = _SCRATCH / "nag-state.json"
        n = notify.notify_open_questions(conn, state_path=state)
        show("the nag's first announcement", f"{n} question(s)")
        check("the nag still announces it", n, 1)
        if TOASTS:
            show("  what it said", repr(TOASTS[-1][1][:44]))

        # The nag then stays quiet on a second run with the same set even though
        # agents have replied in between. So "still open" is not the same claim
        # as "re-toasting every hour", and the printed pair is the honest
        # version of both: the source of the toast still holds the question,
        # and the nag only speaks when the set changes.
        before_second = len(TOASTS)
        n2 = notify.notify_open_questions(conn, state_path=state)
        check("the nag repeats only on a change of set", n2, 0)
        check("...so it said nothing on the second run",
              len(TOASTS) - before_second, 0)

        # CONTROL. A nag that can only ever print numbers proves nothing about
        # its ability to announce anything at all.
        state.unlink()
        n3 = notify.notify_open_questions(conn, state_path=state)
        check("CONTROL: a fresh nag re-announces the same question", n3, 1)
    finally:
        conn.close()


# --- 2. John answers, and it settles into the vault -----------------------------

def check_answer_archives(ids: dict, app) -> None:
    hr("2. John answers it through the window; it is filed and leaves the tab")
    qid = ids["q"]
    before = tab_ids(app, "question")
    show("the Questions tab before his answer", before)
    show("the title bar before his answer", repr(app.root.title()))

    toasts_before = len(TOASTS)
    # The body IS JOHN_MARK, not merely a string containing it. Section 3 greps
    # the transcript for the first word of that constant as proof his answer
    # survived verbatim, and a harness whose marker and whose payload are two
    # different strings proves nothing about the payload it actually sent.
    app.post_reply("question", qid, JOHN_MARK)
    app.root.update()
    # Answering and archiving must not notify anybody. The count of open
    # questions FALLS here, which is a change of set, and the obvious wrong
    # implementation -- announce on any change -- would fire on it.
    check("neither his answer nor the archive toasted anyone",
          len(TOASTS) - toasts_before, 0)

    conn = db.connect(paths.DB_PATH)
    try:
        st = conn.execute("SELECT status FROM threads WHERE id=?",
                          (qid,)).fetchone()["status"]
        check("the thread's status after John answered", st,
              paths.STATUS_ARCHIVED)
        check("open_questions no longer holds it", db.open_questions(conn), [])
    finally:
        conn.close()

    after = tab_ids(app, "question")
    show("the Questions tab after archiving", after)
    check("it left the Questions tab", qid in after, False)
    check("...and nothing else left with it", before, [qid])
    show("the title bar now", repr(app.root.title()))

    target = transcript_path(qid)
    check("the transcript exists", target.exists(), True)
    show("the transcript's path", str(target))


def check_transcript_content(ids: dict) -> None:
    hr("3. the vault file: every message, in order, verbatim")
    qid = ids["q"]
    target = transcript_path(qid)
    text = target.read_text(encoding="utf-8")

    conn = db.connect(paths.DB_PATH)
    try:
        rows = [dict(m) for m in conn.execute(
            "SELECT author, author_kind, ts, body FROM messages"
            " WHERE thread_id=? ORDER BY id", (qid,))]
    finally:
        conn.close()

    print("  the file as written:")
    for line in text.splitlines():
        print(f"      | {line[:64]}")

    print()
    print("  is every message in it, in order?")
    positions = []
    for m in rows:
        marker = m["body"].split()[0]
        positions.append(text.find(marker))
        print(f"      #{m['author']:<12} ({m['author_kind']}) "
              f"{m['ts']}  -> found at {text.find(marker)}")
    check("messages in the database", len(rows), 4)
    check("...all of them present in the file",
          [p >= 0 for p in positions], [True] * len(rows))
    check("...in the database's order, ascending",
          positions == sorted(positions), True)
    check("...and nothing but them (the opening message too)",
          text.count(OPENER_MARK), 1)
    check("John's answer is in it", JOHN_MARK.split()[0] in text, True)
    check("a summary would not have this: the author_kind is on every entry",
          text.count("(agent)") + text.count("(human)"), len(rows))
    check("the vault log carries a pointer line", _pointer_present(qid), True)


def _pointer_present(qid: int) -> bool:
    for f in paths.VAULT_LOG.glob("*.md"):
        if f"agentdesk/questions/question-{qid}" in f.read_text(encoding="utf-8"):
            return True
    return False


def check_second_archive(ids: dict) -> None:
    hr("4. archiving the same thread twice writes nothing new")
    qid = ids["q"]
    target = transcript_path(qid)
    before_stat = target.stat()
    before_sha = sha(target)

    conn = db.connect(paths.DB_PATH)
    try:
        result = vault.archive_question(conn, qid)
        due = [r["id"] for r in db.questions_to_archive(conn)]
        again = vault.archive_due(conn)
    finally:
        conn.close()

    check("a direct second archive", result["status"], "already-archived")
    # Empty and not ["already-archived"], and the difference is the point. The
    # sweep is driven by questions_to_archive, which asks for answered-or-closed
    # rows; an archived row is neither, so the sweep does not reach this thread
    # at all. That is the stronger claim -- the second archive is prevented by
    # the query not selecting it, not by a check inside that has to remember to
    # refuse. 'already-archived' is the fallback for a caller holding a stale id,
    # and is exercised by the direct call above.
    check("...and the sweep does not even consider it (not in the due list)",
          qid in due, False)
    check("...so the sweep files nothing at all", again, [])
    check("the file is byte-identical", sha(target), before_sha)
    check("the file was not rewritten at all",
          target.stat().st_mtime_ns, before_stat.st_mtime_ns)
    check("exactly one transcript exists for it",
          len(list(paths.VAULT_QUESTIONS.glob(f"question-{qid}*.md"))), 1)


# --- 5. the tab is the list of what is not recorded yet -------------------------

def check_leaves_only_after_landing(app) -> None:
    hr("5. it leaves the tab only once the transcript has landed")
    conn = db.connect(paths.DB_PATH)
    try:
        qid = db.start_thread(
            conn, "question", "Is the nightly archive kept for 14 days?",
            "builder", paths.AGENT_KIND, "SEEDQ5 nothing has been pruned yet.")
    finally:
        conn.close()
    app.refresh_now()
    app.root.update()
    target = transcript_path(qid)

    # Break the vault at the ONE point that matters: the transcript cannot be
    # written, because a directory already occupies its filename. Everything
    # else -- the connection, ensure_dirs, the rest of the poll -- still works,
    # so what follows is the real archive failing rather than the app being
    # disabled.
    target.mkdir(parents=True)

    # His answer is a MESSAGE and the status together, because it is now the
    # message that says a question is settled -- the status alone is a thread
    # whose last word is still the agent's, which is an open question wearing
    # an answered label. This section's subject is the vault write failing, so
    # the thread has to be genuinely ready to be written.
    conn = db.connect(paths.DB_PATH)
    try:
        db.reply(conn, qid, paths.HUMAN, paths.HUMAN_KIND,
                 "SEEDQ5 fourteen days, and the prune is nightly.")
        db.set_thread_status(conn, qid, paths.STATUS_ANSWERED)
    finally:
        conn.close()
    app.refresh_now()
    app.root.update()

    print("  John has answered, and the vault cannot take the transcript:")
    show("the title bar", repr(app.root.title()))
    show("the tab", tab_states(app, "question"))
    check("the question is still on the tab", qid in tab_ids(app, "question"), True)
    check("...shown as answered, not archived",
          dict(tab_states(app, "question")).get(qid), "ansd")
    conn = db.connect(paths.DB_PATH)
    try:
        check("...and the DATABASE still says answered, not archived",
              conn.execute("SELECT status FROM threads WHERE id=?",
                           (qid,)).fetchone()["status"], paths.STATUS_ANSWERED)
    finally:
        conn.close()
    check("no transcript was written", target.is_file(), False)
    check("the title bar stopped counting it when he answered",
          str(qid) in app.root.title() or "open question" in app.root.title(),
          False)

    # Repair it and let the same sweep, unmodified, try again.
    target.rmdir()
    app.refresh_now()
    app.root.update()
    print("  ...then the vault is writable again and the next poll runs:")
    show("the tab", tab_states(app, "question"))
    check("it is now archived", conn_status(qid), paths.STATUS_ARCHIVED)
    check("...and it left the tab", qid in tab_ids(app, "question"), False)
    check("...with its transcript on disk", target.is_file(), True)


def conn_status(thread_id: int) -> str:
    conn = db.connect(paths.DB_PATH)
    try:
        return conn.execute("SELECT status FROM threads WHERE id=?",
                            (thread_id,)).fetchone()["status"]
    finally:
        conn.close()


# --- 6. the explicit close, and the things that must not be archived ------------

def check_close_button(app) -> None:
    hr("6. closing a question without answering it, through the real button")
    # The invariant, checked as an absence rather than asserted: only John may
    # settle a question, so there must be no MCP tool an agent could call to
    # close one. @server.tool() returns the plain function, so a tool that
    # existed would be an attribute here.
    check("no MCP tool can close a question",
          hasattr(mcp_server, "close_question"), False)

    conn = db.connect(paths.DB_PATH)
    try:
        # 'closed' is a state the sweep must act on, so it is checked at the
        # query as well as through the button: the button proves the state is
        # reachable, this proves it is the state the archive keys on.
        qid = db.start_thread(
            conn, "question", "Do you still want the old relay kept around?",
            "builder", paths.AGENT_KIND, "SEEDQ6 it has not been used in months.")
        db.set_thread_status(conn, qid, paths.STATUS_CLOSED)
        check("a CLOSED question is due for the archive",
              [r["id"] for r in db.questions_to_archive(conn) if r["id"] == qid],
              [qid])
    finally:
        conn.close()
    app.refresh_now()
    app.root.update()
    check("...and the sweep archived it without a transcript of its own",
          conn_status(qid), paths.STATUS_ARCHIVED)

    conn = db.connect(paths.DB_PATH)
    try:
        qid2 = db.start_thread(
            conn, "question", "Was the relay meant to keep the old certificates?",
            "builder", paths.AGENT_KIND, "SEEDQ6B nothing has been rotated.")
    finally:
        conn.close()
    app.refresh_now()
    app.root.update()

    check("the new question is on the list to close", qid2 in tab_ids(app, "question"), True)

    # The click itself. It runs the real handler, which sets the status and then
    # asks for a refresh -- and that refresh is a poll, so the sweep runs inside
    # this call and the item is archived by the time it returns. There is
    # deliberately no observable 'closed' instant from outside for that reason;
    # the check above is where that state is shown.
    app.close_question(qid2)
    app.root.update()
    check("the button archived it", conn_status(qid2), paths.STATUS_ARCHIVED)
    check("...so it left the tab", qid2 in tab_ids(app, "question"), False)
    check("...with a transcript", transcript_path(qid2).is_file(), True)


def check_refusals(ids: dict, app) -> None:
    hr("7. what must NOT be archived")
    conn = db.connect(paths.DB_PATH)
    try:
        open_q = db.start_thread(
            conn, "question", "An open question nobody has settled yet",
            "builder", paths.AGENT_KIND, "SEEDQ7 still waiting on John.")
        res = vault.archive_question(conn, open_q)
        check("an OPEN question is not archived", res["status"], "not-ready")
        check("...and it is still on the tab exactly as it was",
              conn_status(open_q), paths.STATUS_OPEN)

        check("a work item is skipped",
              vault.archive_question(conn, ids["work"])["status"], "skipped")
        check("a discussion is skipped",
              vault.archive_question(conn, ids["disc"])["status"], "skipped")
        check("a thread that does not exist is skipped",
              vault.archive_question(conn, 99999)["status"], "skipped")

        # A transcript carrying a credential shape. The vault is a git repo John
        # pushes, and this board has already leaked one credential into a
        # transcript once, so the archive checks rather than trusting.
        secret_q = db.start_thread(
            conn, "question",
            "Which token should the release runner use for the mirror push?",
            "builder", paths.AGENT_KIND,
            "SEEDQ8 use this one: ghp_" + "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8")
        # John has to have the last word for this thread to be archivable at
        # all now that 'settled' means settled AND answered. Without this the
        # archive is refused for the standing reason -- he has not replied --
        # and the credential refusal this fixture exists to test is never
        # reached: it would report 'not-ready' and the check would be
        # measuring the wrong refusal. The credential is still the opening
        # message's, which is what the transcript would have carried.
        db.reply(conn, secret_q, paths.HUMAN, paths.HUMAN_KIND,
                 "Not that one - rotate it and use the release token.")
        db.set_thread_status(conn, secret_q, paths.STATUS_ANSWERED)
        res = vault.archive_question(conn, secret_q)
        check("a transcript with a credential in it is refused",
              res["status"], "refused")
        check("...the thread is NOT marked archived",
              conn_status(secret_q), paths.STATUS_ANSWERED)
        check("...nothing was written into the vault's questions folder",
              transcript_path(secret_q).exists(), False)
        park = paths.VAULT_PARKED / vault.question_filename(secret_q)
        check("...a refusal was left where a human will see it",
              park.is_file(), True)
        check("...and the refusal does NOT reproduce the credential",
              "ghp_" in park.read_text(encoding="utf-8"), False)
        print("      the refusal, as written:")
        for line in park.read_text(encoding="utf-8").splitlines():
            print(f"        | {line[:60]}")
    finally:
        conn.close()


# --- 8. a question already answered once can be asked again ---------------------

# The view this replaces, spelled out rather than imported, because the point
# of section 9's second half is to build a database that HAS it -- and the
# only way to prove a migration ran is to start from the thing it migrates.
OLD_VIEW_SQL = (
    "CREATE VIEW open_questions AS"
    " SELECT t.id AS thread_id, t.subject, t.opened_by, t.created_ts, t.updated_ts"
    " FROM threads t WHERE t.channel = 'question' AND t.status = 'open'")


def check_reask_survives_the_one_way_door(app) -> None:
    """Work item #75. A question's status never goes back to `open`, so while
    'waiting on John' WAS that status, the first human reply made a thread a
    one-shot: every later ask in it reached nobody.

    Thread #54 on the real board is the shape this reproduces -- John replies,
    an agent asks again, the status stays `answered` and the ask is invisible.
    The reproduction is here rather than read off that thread because a check
    that depends on a real thread's mileage is a check that stops working the
    day he answers it.
    """
    hr("8. a question John answered once can still be asked again")
    note("""
    The sequence is the live one: John replies (status -> answered), an agent
    asks again, nothing sets the status back. Every surface is read after the
    second ask, and the control for each is the same surface before it.

    The board is not empty by this point -- an earlier section leaves a
    question John has deliberately not answered -- so what is checked is the
    DIFFERENCE this question makes, against a baseline taken here. An absolute
    count would be a check on the other sections, and would quietly stop
    measuring this one the moment they changed.
    """)
    conn = db.connect(paths.DB_PATH)
    try:
        baseline = sorted(q["thread_id"] for q in db.open_questions(conn))
        show("already waiting before this section", baseline)
    finally:
        conn.close()

    conn = db.connect(paths.DB_PATH)
    try:
        qid = db.start_thread(
            conn, "question", "SEEDQ9 may I deploy to the second host now?",
            "builder", paths.AGENT_KIND,
            "SEEDQ9 the last step needs your go-ahead.")
        conn.commit()
    finally:
        conn.close()

    # John's reply, written the way the window writes it but WITHOUT a refresh:
    # the refresh would run the archive sweep, and the sweep filing the thread
    # before the re-ask is the thing under test not happening. This is also
    # what the real board actually looks like -- the sweep only runs while a
    # window is open, and #54 was answered with none running.
    conn = db.connect(paths.DB_PATH)
    try:
        db.reply(conn, qid, "john", paths.HUMAN_KIND, "SEEDQ9 yes, one host.")
        db.set_thread_status(conn, qid, paths.STATUS_ANSWERED)
        conn.commit()
        show("after John's reply, the status is", conn_status(qid))
        check("the reply settled it", conn_status(qid), paths.STATUS_ANSWERED)
        check("and the view dropped it, which is the old behaviour working",
              sorted(q["thread_id"] for q in db.open_questions(conn)), baseline)
    finally:
        conn.close()

    # The re-ask, through the real MCP tool -- agent identity resolution and
    # all, because that is what an agent actually calls.
    answer_as_agent(qid, "SEEDQ9 asking again: the deploy is still unauthorised.",
                    "builder")
    app.refresh_now()

    expected = sorted(baseline + [qid])
    conn = db.connect(paths.DB_PATH)
    try:
        view_ids = sorted(q["thread_id"] for q in db.open_questions(conn))
        summary = db.unread_summary(conn)
        due = [r["id"] for r in db.questions_to_archive(conn)]
        show("open_questions after the re-ask", view_ids)
        check("the ask reaches the MCP tool and the toast", view_ids, expected)
        check("and the tray/title count agrees with it",
              summary["open_questions"], len(expected))
        check("and the sweep will NOT file a question he has not seen",
              qid in due, False)
        check("...so the transcript is not in the vault",
              transcript_path(qid).exists(), False)
        check("...and its status is untouched: no reset was invented",
              conn_status(qid), paths.STATUS_ANSWERED)
    finally:
        conn.close()

    # The window. The row has to be on the tab AND read as waiting -- a toast
    # that fires for a row the tab draws as settled is the disagreement this
    # whole change is about.
    show("the Questions tab", tab_ids(app, "question"))
    check("the row is on the tab", qid in tab_ids(app, "question"), True)
    states = dict(tab_states(app, "question"))
    show("the word the tab prints for it", states.get(qid))
    check("it reads as waiting, not as the status it is stored with",
          states.get(qid), "WAIT")
    row = next((r for r in app.view.rows["question"] if r["id"] == qid), None)
    check("and the view counts it as waiting (the red row)",
          bool(row) and terminal.waiting("question", row), True)
    # The title is checked against the VIEW rather than against a number, so
    # this says the two surfaces agree and not merely that a number appeared.
    title = app.root.title()
    show("the title bar", repr(title))
    found = re.search(r"(\d+) open question", title)
    check("the title counts exactly what the view holds",
          int(found.group(1)) if found else None, len(expected))
    # The hourly nag, on a state file of its own: it only speaks when the set
    # changes, so a shared one would make it stay quiet for the wrong reason.
    state = _SCRATCH / "nag-state-q9.json"
    conn = db.connect(paths.DB_PATH)
    try:
        n = notify.notify_open_questions(conn, state_path=state)
        show("the hourly nag announces", f"{n} question(s)")
        check("the nag announces it too, and counts what the view holds",
              n, len(expected))
    finally:
        conn.close()

    # TRAP 2, in its larger form. A receipt is an agent message. If receipts
    # counted, the board's own housekeeping would re-open the thread it is
    # housekeeping about: an agent delivering its ack would undo the ack, the
    # board's "nobody has picked this up" note would itself count as somebody
    # picking it up, and an agent merely READING the thread would put it back
    # in front of John. All three kinds, because the tuple is the point.
    conn = db.connect(paths.DB_PATH)
    try:
        for kind in db.RECEIPT_KINDS:
            db.reply(conn, qid, "builder", paths.AGENT_KIND,
                     f"A {kind}, which is the board keeping its own books.",
                     meta={"kind": kind, "ack_for": 1})
            conn.commit()
            ids_now = sorted(q["thread_id"] for q in db.open_questions(conn))
            show(f"open_questions after a {kind!r} message", ids_now)
            check(f"a {kind!r} did not duplicate the entry",
                  len(ids_now), len(expected))
            check(f"...and did not take the thread off the list",
                  ids_now, expected)
    finally:
        conn.close()

    # John replies again: the ask is dealt with, and only now may it be filed.
    app.post_reply("question", qid, "SEEDQ9 go ahead, one host only.")
    app.refresh_now()
    conn = db.connect(paths.DB_PATH)
    try:
        check("his reply takes it off the list",
              sorted(q["thread_id"] for q in db.open_questions(conn)), baseline)
        check("...and it is filed once he has the last word",
              conn_status(qid), paths.STATUS_ARCHIVED)
        check("...with a transcript",
              transcript_path(qid).is_file(), True)
    finally:
        conn.close()

    # The boundary, stated rather than left to be discovered. Once a question
    # is ARCHIVED it is out of the live set and a later agent message does not
    # bring it back -- 'archived' is a state John's own archive pass put it in,
    # and the rule refuses to overrule a status a person's action produced.
    # "Bring back" is the way out, and it exists for exactly this. On today's
    # board this is reachable only for threads filed BEFORE this change, and
    # there are none; it is checked so the claim is measured, not assumed.
    answer_as_agent(qid, "SEEDQ9 one more: the second host is still waiting.",
                    "builder")
    app.refresh_now()
    conn = db.connect(paths.DB_PATH)
    try:
        ids_now = sorted(q["thread_id"] for q in db.open_questions(conn))
        show("an agent asks again on an ARCHIVED thread", ids_now)
        check("an archived thread does not come back by itself",
              ids_now, baseline)
        check("...and it is still archived", conn_status(qid),
              paths.STATUS_ARCHIVED)
    finally:
        conn.close()


def check_view_migration() -> None:
    """TRAP 1, and the only reason this section is a section.

    `init_db` is `executescript(SCHEMA)`, and the schema is all CREATE ... IF
    NOT EXISTS. A corrected open_questions view therefore applies to a fresh
    database and to NOTHING ELSE: on John's board the old view stays, the
    tests pass against a fresh file, and the fix is a no-op where it matters.
    The only way to catch that is to start from a database that has the OLD
    view in it and run init_db over the top -- which is what this does, in a
    database of its own so the rest of this run is not disturbed.
    """
    hr("9. the fix reaches a database that already had the old view")
    target = _SCRATCH / "pre-existing.db"
    conn = db.connect(target)
    try:
        db.init_db(conn)
        conn.execute("DROP VIEW open_questions")
        conn.execute(OLD_VIEW_SQL)
        conn.commit()
        stored = conn.execute("SELECT sql FROM sqlite_master WHERE type='view'"
                              " AND name='open_questions'").fetchone()[0]
        check("the starting point really is the OLD view",
              "status = 'open'" in stored, True)

        qid = db.start_thread(conn, "question", "Pre-existing question",
                              "builder", paths.AGENT_KIND, "the ask")
        db.reply(conn, qid, "john", paths.HUMAN_KIND, "answered")
        db.set_thread_status(conn, qid, paths.STATUS_ANSWERED)
        db.reply(conn, qid, "builder", paths.AGENT_KIND, "asked again")
        conn.commit()
        show("on the old view, open_questions returns",
             [q["thread_id"] for q in db.open_questions(conn)])
        check("CONTROL: the old view cannot see the re-ask",
              [q["thread_id"] for q in db.open_questions(conn)], [])

        db.init_db(conn)
        ids_now = [q["thread_id"] for q in db.open_questions(conn)]
        show("after init_db over the top of it", ids_now)
        check("init_db replaced the view on an existing database", ids_now, [qid])
        stored = conn.execute("SELECT sql FROM sqlite_master WHERE type='view'"
                              " AND name='open_questions'").fetchone()[0]
        check("...and the stored definition is the new one",
              "json_extract" in stored, True)
        check("...and one speaking of acks, so the ack rule is in it",
              "'ack'" in stored, True)
    finally:
        conn.close()


# --- 10. the window, to look at -------------------------------------------------

def check_app_toast_probe(app) -> None:
    """The control for the window half of the toast probe.

    Every other toast number in this run is a zero, and a probe that cannot see
    a toast would print the same zeros. This makes the window fire one.
    """
    hr("10. CONTROL: the probe can see the window's own toast")
    # Settle the board FIRST, and only then take the baseline. Section 7 creates
    # an open question and never refreshes, so without this the control's first
    # refresh announces that one as well as its own and the count is 2 -- which
    # is _apply_changes behaving correctly on a backlog, not a fault. Draining
    # it here means the only thing that can be announced after the baseline is
    # the one question this control makes.
    for _ in range(3):
        app.refresh_now()
        app.root.update()
    before = len(TOASTS)
    conn = db.connect(paths.DB_PATH)
    try:
        db.start_thread(conn, "question", "CONTROL: does a new question toast?",
                        "researcher", paths.AGENT_KIND, "Deliberately new.")
    finally:
        conn.close()
    for _ in range(3):
        app.refresh_now()
        app.root.update()
    check("the window's new-question toast was seen",
          len(TOASTS) - before, 1)
    if TOASTS:
        show("  what it said", repr(TOASTS[-1][0][:52]))


def check_look(app, shot: Path | None) -> None:
    hr("11. the window, as John would see it")
    conn = db.connect(paths.DB_PATH)
    try:
        qid = db.start_thread(
            conn, "question", "Should the mirror run before or after the backup?",
            "researcher", paths.AGENT_KIND,
            "SEEDQ9 they both rewrite the vault day file.")
        db.reply(conn, qid, "verifier", paths.AGENT_KIND,
                 "SEEDQ9R before, so the backup snapshots what was published.")
    finally:
        conn.close()
    app.refresh_now()
    app.root.update()

    try:
        app.view.open_thread(qid)
        err = None
    except Exception as e:  # noqa: BLE001
        err = f"{type(e).__name__}: {e}"
        app.view.screen = "main"
    check("the thread opens in the reader", err, None)
    if err is None:
        body = app.view.read_view.get("1.0", "end")
        check("...and the reader shows the agent's reply", "SEEDQ9R" in body, True)
    app.root.deiconify()
    app.root.update_idletasks()
    app.root.update()
    time.sleep(0.3)
    app.root.update()

    print("  the question tab, row by row:")
    for tid, word in tab_states(app, "question"):
        print(f"      #{tid}  {word}")
    show("the title bar", repr(app.root.title()))
    show("the top line", repr(app.view.top.get("1.0", "end-1c").strip()))
    if shot is not None:
        shot.mkdir(parents=True, exist_ok=True)
        from check_look import screenshot  # shared with the look checks
        png = shot / "questions-archived.png"
        screenshot(app.root, png)
        show("screenshot", f"saved {png}")


# --- main ------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shot", type=Path, default=None,
                        help="directory to write a PNG of the window into")
    args = parser.parse_args()

    print(f"scratch board: {paths.DB_PATH}")
    print(f"scratch vault: {paths.VAULT_DIR}")
    paths.ensure_dirs()
    conn = db.connect(paths.DB_PATH)
    try:
        db.init_db(conn)
        ids = seed(conn)
    finally:
        conn.close()
    print(f"  seeded{'':<37}threads {sorted(ids.values())}")

    # Installed for the whole run, before the first toast could be fired: this
    # window is real and so are its banners, and several sections below create
    # fresh open questions. Without the probe John would get a notification for
    # each one -- and there would be no way to count what the archive did.
    original_toast = install_toast_probe()
    try:
        check_agents_cannot_settle(ids)

        app = appmod.App(paths.DB_PATH)
        app.root.withdraw()
        app.root.update()
        time.sleep(0.3)
        try:
            for _ in range(3):
                app.refresh_now()
                app.root.update()

            check_answer_archives(ids, app)
            check_transcript_content(ids)
            check_second_archive(ids)
            check_leaves_only_after_landing(app)
            check_close_button(app)
            check_refusals(ids, app)
            check_reask_survives_the_one_way_door(app)
            check_view_migration()
            check_app_toast_probe(app)
            check_look(app, args.shot)
        finally:
            if app.icon is not None:
                try:
                    app.icon.stop()
                except Exception:
                    pass
            app.root.destroy()
    finally:
        notify.toast = original_toast

    hr()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        print(f"\nscratch board left at {_SCRATCH}")
        return 1
    print("every printed property held.")
    print()
    print("WHAT IS NOT ASSERTED HERE: that any of this looks right, and that a")
    print("question settled while no window is running is filed -- that last one")
    print("rests on `python -m agentdesk.vault --archive-due`, which this script")
    print("does not run as a subprocess.")
    print(f"\nscratch board and vault left at {_SCRATCH}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
