"""Acceptance checks for the wiki <-> memory-vault integration (work item 26).

The brief is thread 26 message 43. Its acceptance is three lines, and each is a
section below:

  1. Post a wiki entry, show the resulting vault state.              (1, 2, 3)
  2. A bad entry is refused or parked rather than corrupting
     the vault.                                              (4, 5, 6, 8, 9)
  3. The direction and the rejected options are written down.      (see the
     module docstring of agentdesk/vault.py, which is where that lives)

WHY THE SCRATCH VAULT IS `git init`-ED. One of the item's constraints is that
the mirror must never commit or push the vault -- John commits it. "The code
does not call git" is a claim about the source; this makes it a claim about
behaviour: the vault is a real repository, a mirror run really does write notes
into it, and section 10 checks that no commit exists afterwards. Without the
repo the check could not tell "did not commit" from "could not have committed".

The scratch vault is seeded with the one MOC the test's tag needs (`gocd`), so
the run is self-contained and John's real vault at
`C:\\Users\\palencharj\\NoOneDrive\\MainClaudeMemory\\MainClaude` is never opened
for writing -- which matters, because the mirror's whole job is to write files
into a vault, and pointing this at the real one would file a test note for real.

    .venv\\Scripts\\python.exe scripts\\check_vault.py
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import tempfile
from datetime import datetime
from pathlib import Path

# --- isolation, BEFORE agentdesk is imported -----------------------------------
#
# LOCALAPPDATA decides paths.DB_PATH, and the VAULT_* constants are rewritten
# below. Both, not one: the mirror writes a note into the vault and a row into
# the board, and a check that isolated only the board would still file a note in
# John's real vault on every run.
_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-vault-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

from agentdesk import db, mcp_server, paths, vault  # noqa: E402

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
TODAY = datetime.now().astimezone().date()
SECRET = "AKIAIOSFODNN7EXAMPLE"


def hr(title: str = "") -> None:
    if title:
        print(f"\n--- {title} " + "-" * max(0, WIDTH - len(title) - 5))
    else:
        print("-" * WIDTH)


def show(label: str, value) -> None:
    print(f"  {label:<46} {value}")


def check(label: str, got, want) -> None:
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<44} {got!r}")


def note(text: str) -> None:
    for line in text.strip().splitlines():
        print(f"  {line}")


# --- the scratch vault ----------------------------------------------------------

def build_vault() -> None:
    """A vault with the shape the mirror checks for, and nothing else in it."""
    for d in (paths.VAULT_NOTES, paths.VAULT_MAPS, paths.VAULT_LOG,
              paths.VAULT_PARKED, paths.VAULT_QUESTIONS):
        d.mkdir(parents=True, exist_ok=True)
    (paths.VAULT_MAPS / "GoCD MOC.md").write_text(
        "---\ntype: moc\ntags: [moc]\nsummary: The test MOC.\n"
        "updated: 2026-01-01\n---\n\n# GoCD MOC\n\n## The server\n\n"
        "- [[Some Other Note]] — already here, and not the mirror's to touch\n",
        encoding="utf-8", newline="\n")
    subprocess.run(["git", "init", "-q", str(_VAULT)], check=True)


def post(subject: str, body: str, channel: str = "wiki") -> int:
    """A thread on the scratch board, via db directly -- the mirror's input."""
    conn = db.connect(paths.DB_PATH)
    try:
        return db.start_thread(conn, channel, subject, "builder", "agent", body)
    finally:
        conn.close()


def mirror(thread_id: int, dry_run: bool = False) -> dict:
    """One mirror run on its own connection, exactly as the CLI makes it."""
    conn = db.connect(paths.DB_PATH)
    try:
        return vault.mirror_thread(conn, thread_id, dry_run=dry_run)
    finally:
        conn.close()


def edit_opening(thread_id: int, body: str) -> None:
    """Correct a thread's opening post, the way a human would on the board.

    This is what "fix the post and re-mirror" actually means. Posting a reply
    would change nothing -- the mirror reads the FIRST message -- and posting a
    second thread with the same subject is a different thing entirely, which is
    what section 7 is about.
    """
    conn = db.connect(paths.DB_PATH)
    try:
        conn.execute(
            "UPDATE messages SET body=? WHERE id=(SELECT MIN(id) FROM messages"
            " WHERE thread_id=?)", (body, thread_id))
        conn.commit()
    finally:
        conn.close()


def notes_dir() -> list:
    return sorted(p.name for p in paths.VAULT_NOTES.iterdir())


def parked_dir() -> list:
    return sorted(p.name for p in paths.VAULT_PARKED.iterdir())


def moc_text() -> str:
    return (paths.VAULT_MAPS / "GoCD MOC.md").read_text(encoding="utf-8")


def note_text(name: str) -> str:
    return (paths.VAULT_NOTES / f"{name}.md").read_text(encoding="utf-8")


# --- posts ----------------------------------------------------------------------

FILE, VERSION, SUMMARY = "A note the mirror should accept", 2026, "A valid note"


def good_body(summary: str = SUMMARY, extra: str = "") -> str:
    """A post that satisfies every rule in _meta/Memory Protocol.md."""
    return f"""---
type: note
tags: [gocd]
summary: {summary}
updated: {TODAY.isoformat()}
---
The mirror is a one-way gate: a board thread is a candidate and the vault's
protocol decides whether it becomes a memory. This post is written to pass that
gate, so that the checks below have something that lands in notes/ and can be
re-run, rewritten and registered in its MOC.

Related: [[GoCD MOC]] · [[Some Other Note]]
{extra}"""


def main() -> int:
    build_vault()
    conn = db.connect(paths.DB_PATH)
    db.init_db(conn)
    conn.close()

    print(f"scratch board:  {paths.DB_PATH}")
    print(f"scratch vault:  {_VAULT}  (git init'ed; nothing is ever committed)")

    # -- 1. the integration is on the write -----------------------------------
    hr("1. posting a wiki entry IS the integration")
    note("""
    The item's shape is 'a wiki entry should not be invisible from the vault'.
    The strongest form of that is that there is no second step: the mirror runs
    from post_message itself, so an agent that writes a wiki entry has already
    done everything required. This calls the same function object the MCP server
    registered, so what is checked here is the shipping call path.
    """)
    answer = json.loads(mcp_server.post_message(
        channel="wiki", subject="The integration runs on the write",
        body=good_body(summary="Posted through the MCP tool, not the db."),
        author="builder"))
    check("post_message succeeded", answer.get("ok"), True)
    vault_result = answer.get("vault") or {}
    show("the reply carries a vault field", vault_result.get("status"))
    check("no second step was needed", vault_result.get("status"), "mirrored")
    check("and a note is on disk",
          "The integration runs on the write.md" in notes_dir(), True)

    # -- 2. the note is a note ------------------------------------------------
    hr("2. the note it wrote satisfies the protocol it enforces")
    name = FILE
    good_id = post(name, good_body())
    r = mirror(good_id)
    check("the good post mirrored", r["status"], "mirrored")
    text = note_text(name)
    order, keys, body = vault.split_frontmatter(text)
    check("keys are exactly the protocol's, in order",
          order, ["type", "tags", "summary", "updated"])
    check("summary is within the 140-char cap", len(keys["summary"]) <= 140, True)
    check("no H1 in the body", bool(re.search(r"(?m)^#\s", body)), False)
    check("at least 2 wikilinks",
          len(re.findall(r"\[\[[^\[\]\n]+\]\]", body)) >= 2, True)
    check("it has a Related: line", bool(re.search(r"(?m)^Related:", body)), True)
    check("body is within the note cap", vault._words(body) <= 200, True)
    owner = vault.note_owner(paths.VAULT_NOTES / f"{name}.md")
    show("the provenance marker names", f"thread {owner} (this one is {r['thread_id']})")
    check("the marker names the thread it came from", owner, r["thread_id"])
    check("the body survived verbatim",
          "a board thread is a candidate" in text, True)
    check("nothing was parked", parked_dir(), [])

    # -- 3. idempotent, so a corrected post can be re-filed -------------------
    hr("3. re-mirroring rewrites the note it owns instead of parking")
    note("""
    This is the section that found a real defect. The ownership test used to
    compare the note's whole marker LINE against the marker's PREFIX, which are
    never equal -- so a note the mirror had written itself was reported as
    "already exists and was not written by this mirror", and the documented
    rewrite path was dead code. The reason is shown below because a park is
    silent otherwise: it looks the same as a correct refusal.
    """)
    edit_opening(good_id, good_body(summary="The opening post was corrected."))
    edited = mirror(good_id)
    show("second run of the same thread", edited["status"])
    if edited["status"] != "mirrored":
        show("and it gave this reason", edited.get("reasons"))
    check("the second run mirrored, not parked", edited["status"], "mirrored")
    check("the correction reached the note",
          "was corrected" in note_text(name), True)

    lines = [ln for ln in moc_text().splitlines() if ln.startswith(f"- [[{name}]]")]
    show("MOC lines for this note", f"{len(lines)} -> {lines[0] if lines else None!r}")
    check("the MOC holds exactly one line for it", len(lines), 1)
    check("the MOC line uses the protocol's em dash", "—" in lines[0], True)
    check("the MOC line is not the old hyphen one",
          any(ln.startswith(f"- [[{name}]] - ") for ln in moc_text().splitlines()),
          False)

    # -- 4. a bad post is parked, and notes/ is untouched --------------------
    hr("4. a post that cannot be a note is parked, not filed")
    before = notes_dir()
    bad = mirror(post(
        "A post with no frontmatter",
        "There is no frontmatter block at the top of this post, so nothing here "
        "says what type it is, which tags it belongs under, how long its summary "
        "is, or when it was last updated. It is prose, and the mirror should say "
        "so rather than guess at the four keys it needs."))
    check("the bad post parked", bad["status"], "parked")
    check("and the only reason is the missing frontmatter",
          bad["reasons"], ["no YAML frontmatter block at the top of the post"])
    show("parked at", bad.get("parked_at"))
    check("notes/ gained nothing", notes_dir(), before)
    ours = [p for p in paths.VAULT_PARKED.iterdir()
            if p.name.startswith(f"{bad['thread_id']}-")]
    check("one park file, keyed to the thread", len(ours), 1)
    parked_text = ours[0].read_text(encoding="utf-8") if ours else ""
    check("the park file carries the reasons",
          all(r in parked_text for r in bad["reasons"]), True)
    check("and quotes the post, so the author can see what was wrong",
          "There is no frontmatter block" in parked_text, True)
    check("the park file is NOT in notes/",
          any(p.name.startswith("A post with no frontmatter") for p in
              paths.VAULT_NOTES.iterdir()), False)

    # -- 5. the reasons are true reasons --------------------------------------
    hr("5. the refusal names what is actually wrong")
    bad2 = mirror(post(
        "A post with two faults",
        f"""---
type: note
tags: [agentdesk, tkinter]
summary: This post carries two faults that the gate has to name.
updated: {TODAY.isoformat()}
---
Too short to be a memory, and tagged outside the vocabulary.

Related: [[GoCD MOC]] · [[Some Other Note]]"""))
    shown = " | ".join(bad2["reasons"])
    show("reasons", "")
    for reason in bad2["reasons"]:
        print(f"        - {reason}")
    check("the out-of-vocabulary tags are named",
          "agentdesk" in shown and "tkinter" in shown, True)
    check("the short body is named", "at least 20" in shown, True)
    check("the note was not filed despite having valid frontmatter",
          "A post with two faults.md" in notes_dir(), False)
    check("it parked rather than being silently fixed", bad2["status"], "parked")

    # -- 6. a human's note is never eaten ------------------------------------
    hr("6. a note the mirror did not write is never overwritten")
    hand = "A hand written note"
    (paths.VAULT_NOTES / f"{hand}.md").write_text(
        "---\ntype: note\ntags: [gocd]\nsummary: John's own note.\n"
        "updated: 2026-01-01\n---\n\nHis words, not the mirror's.\n",
        encoding="utf-8", newline="\n")
    before_text = note_text(hand)
    r6 = mirror(post(hand, good_body()))
    check("the colliding post parked", r6["status"], "parked")
    show("the reason", r6["reasons"][0] if r6["reasons"] else None)
    check("colliding with a human's note is the ONLY reason", len(r6["reasons"]), 1)
    check("and the reason says whose note it is",
          "hand-written note" in " ".join(r6["reasons"]), True)
    check("the hand-written note is byte-identical", note_text(hand), before_text)

    # -- 7. two threads, one subject -----------------------------------------
    hr("7. two threads with the same subject cannot share one note")
    note("""
    Not hypothetical: this board has three threads called "Poll redraw gate" and
    two called "Tk layout traps: Text width, sash". The note's filename comes
    from the subject, so subject collisions are real input. Without an owner
    check the last thread mirrored wins and the marker then names the survivor,
    which loses the first thread's note silently and with no error anywhere.
    """)
    shared = "Two threads sharing one subject"
    first = mirror(post(shared, good_body(summary="The first thread's version.")))
    second_id = post(shared, good_body(summary="The second thread's version."))
    second = mirror(second_id)
    check("the first thread mirrored", first["status"], "mirrored")
    check("the second parked instead of overwriting", second["status"], "parked")
    show("the reason", second["reasons"][0] if second["reasons"] else None)
    check("the note still belongs to the first thread",
          vault.note_owner(paths.VAULT_NOTES / f"{shared}.md"), first["thread_id"])
    check("and still holds the first thread's words",
          "first thread's version" in note_text(shared), True)

    # -- 8. a credential never reaches the vault -----------------------------
    hr("8. a post carrying a credential is refused, and not copied in either")
    note("""
    The park file lives in the vault too, so refusing the note while quoting the
    post in the refusal would leak the secret by the back door. Both halves are
    checked: the note is not written, AND the string is not in the park file.
    """)
    before = notes_dir()
    r8 = mirror(post(
        "A post carrying a credential",
        good_body(summary="Carries a key id, which the protocol forbids.",
                  extra=f"\nThe post also contains the value {SECRET} in prose.\n")))
    check("it was refused", r8["status"], "parked")
    show("the reason", r8["reasons"][0] if r8["reasons"] else None)
    check("the reason names a credential",
          "AWS access key" in " ".join(r8["reasons"]), True)
    check("notes/ gained nothing", notes_dir(), before)
    leaks = [p.name for p in paths.VAULT_PARKED.iterdir()
             if SECRET in p.read_text(encoding="utf-8")]
    show("park files containing the credential", leaks)
    check("the credential is not anywhere in the vault's agentdesk/", leaks, [])

    # -- 9. --dry-run writes nothing -----------------------------------------
    hr("9. --dry-run reports and writes nothing")
    before_notes, before_parks = notes_dir(), parked_dir()
    moc_before = moc_text()
    dry = mirror(post("A dry run post",
                      "Deliberately not a note at all, so that any write this "
                      "run made would be visible in the three checks below."),
                 dry_run=True)
    check("it still reports the verdict", dry["status"], "parked")
    check("notes/ unchanged", notes_dir(), before_notes)
    check("parked/ unchanged", parked_dir(), before_parks)
    check("the MOC unchanged", moc_text(), moc_before)

    # -- 10. the vault is never committed ------------------------------------
    hr("10. nothing is committed: the vault's history is untouched")
    check("the scratch vault really is a repository",
          (_VAULT / ".git").is_dir(), True)
    log = subprocess.run(["git", "-C", str(_VAULT), "log", "--oneline"],
                         capture_output=True, text=True)
    show("git log", (log.stdout.strip() or log.stderr.strip() or "")[:60] or "(none)")
    check("no commit exists after all those mirror runs", log.returncode != 0, True)
    imports = Path(vault.__file__).read_text(encoding="utf-8")
    check("vault.py imports no process-spawning module",
          "subprocess" in imports, False)

    hr()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        return 1
    print("every printed property held.")
    print("""
WHAT IS NOT ASSERTED HERE. Whether a mirrored note READS well in Obsidian, and
whether the MOC section it registers under is the right one for its topic -- the
mirror picks the MOC from the tag, and for `gocd` there are six MOCs that
legitimately carry that tag, so TAG_MOC is a hand-made choice that a human
should sanity-check rather than something this script can derive. Nor is the
backfill checked: `--all` over this board parks every one of its 15 wiki threads
today (none of them were written as notes), which is the gate working, but it
does mean the mirror currently files nothing without an author writing for it.
""")
    return 0


if __name__ == "__main__":
    sys.exit(main())
