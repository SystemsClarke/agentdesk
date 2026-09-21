"""The one-way mirror between the wiki channel and the memory vault.

THE DECISION, AND WHY. The board's wiki channel and the vault are both called
"durable knowledge", and they were drifting: a wiki entry was invisible from the
vault and a vault note was invisible from the board. Exactly one direction is
mirrored -- **wiki post into the vault** -- and it is gated:

    a wiki thread's opening post is a note CANDIDATE; it lands in notes/ only
    if it satisfies _meta/Memory Protocol.md, and is otherwise PARKED under
    vault/agentdesk/parked/ with the reasons it failed. Nothing is ever written
    from the vault back onto the board.

Why that direction: the board is where agents actually write prose, so it is the
side that produces candidates; the vault is the durable store with a search
index and MOCs over it, so it is the side worth feeding. Why gated: a wiki post
is NOT automatically a note. The protocol demands frontmatter in a fixed key
order, a controlled tag, a <=140-char summary, <=200-word body, >=2 wikilinks
and a MOC registration. Writing posts in unfiltered would not "integrate" the
two -- it would degrade the vault until the linter failed on it, which is the
failure this module exists to prevent.

WHAT WAS REJECTED (work item #26 asked for this to be written down):

  - **Vault -> wiki tab** (surface notes in the GUI). Rejected: it is a read-only
    surface in the direction that does not produce candidates, it means loading
    ~1,100 notes to render a tab that Obsidian already renders better, and it
    fixes nothing about the actual drift, which is that agents write knowledge
    onto the board and it never becomes a memory.
  - **Ungated wiki -> notes/.** Rejected outright: it is the corruption above.
  - **Two-way sync.** Rejected: the vault is hand-edited in Obsidian live, so a
    last-writer-wins merge would silently eat a human edit. A one-way mirror has
    one writer on each side, which is why it cannot conflict.
  - **A git commit on each mirror.** Rejected: the vault's commits carry an
    authorship convention that is John's to make, and `backup.py` already
    refuses for the same reason. This module contains no git call at all.

A mirror run is idempotent: re-mirroring a thread rewrites the note it owns and
replaces its MOC line. Ownership is a THREAD ID, not a yes/no "has our marker" --
the marker names the thread it was written for, and the note is rewritten only
when that thread is the one being mirrored. Every other case parks, and there are
three of them: a hand-written note (no marker), and a note this mirror wrote for
a DIFFERENT thread. That third case is not hypothetical here -- three threads on
this board share the subject "Poll redraw gate" and two share "Tk layout traps:
Text width, sash" -- and it is why the check reads the id out of the marker
rather than testing for the marker's presence: with a presence test, the last
thread mirrored would silently eat the note belonging to the first, and the
marker would then name the survivor as if it had always owned it.

Only the thread's OPENING message becomes the note. Replies are conversation,
not memory -- edit the opening post and re-mirror if the note needs to change.
"""

from __future__ import annotations

import argparse
import json
import re
import sqlite3
from datetime import date, datetime
from pathlib import Path

from . import db, paths

# The provenance marker, written into the body (not the frontmatter -- the
# protocol fixes the frontmatter keys to exactly type/tags/summary/updated, so a
# `source:` key there would itself be a protocol violation). It is an HTML
# comment so it is invisible in a rendered Obsidian note while remaining an
# exact, greppable marker of which notes a machine wrote.
MARKER_OPEN = "<!-- mirrored from AgentDesk wiki thread"
MARKER = ("{open} {thread_id} ({author}, {date}); the board thread is the "
          "source - this file is rewritten from it -->")

# Which thread a note belongs to, read back out of its marker. This is the whole
# ownership check, and it is a regex over the id rather than a test for the
# marker's presence -- see the module docstring for why the difference matters.
_MARKER_OWNER = re.compile(re.escape(MARKER_OPEN) + r"\s+(\d+)\b")

# The heading the mirror registers its entries under in a MOC. A named section
# rather than an append at the end of the file, so a mirrored entry cannot land
# silently inside whichever hand-curated section happened to be last.
MOC_SECTION = "## Mirrored from AgentDesk"

# The protocol's controlled tag vocabulary (_meta/Memory Protocol.md). A tag
# outside this is a park, not a guess: the protocol says an out-of-vocabulary
# tag is how a new area announces itself, and adding one is a human decision.
CONTROLLED_TAGS = {
    "gocd", "signing", "infra", "repos", "ado", "perforce",
    "localization", "tooling", "people", "meta", "moc",
}

# tag -> the MOC a note carrying it must be registered in. Explicit rather than
# derived by scanning maps/ for the tag, because several MOCs legitimately carry
# the same one (six carry `gocd`) and a scan would have to guess between them
# every time. Each target is checked to exist before the entry is written.
TAG_MOC = {
    "gocd": "GoCD MOC",
    "signing": "Signing MOC",
    "infra": "Infra MOC",
    "repos": "Repos MOC",
    "ado": "ADO MOC",
    "perforce": "Perforce MOC",
    "localization": "Localization MOC",
    "people": "Working with John MOC",
    "tooling": "Workstation MOC",
}

# Note types this mirror may write into notes/. The protocol's other types have
# their own homes -- `moc` lives in maps/, `log` in log/, `meta`/`template` in
# _meta/ -- so a wiki post claiming one of those is parked rather than filed in
# the wrong folder.
NOTE_TYPES = {"note", "pref", "project", "person"}

# Body length caps, straight from the protocol's own lint rules: 200 words for a
# note, 420 for the accretive pref/project shape. The floor is not in the
# protocol; it exists because a 12-word post is a remark, not a memory.
WORDS_MAX = {"note": 200, "person": 200, "pref": 420, "project": 420}
WORDS_MIN = 20

# High-signal secret markers. The protocol forbids storing secret values, and
# this board has already leaked one into a transcript -- so the mirror checks
# rather than trusting the author. Deliberately a short, named list of shapes
# that are almost never prose: a false park costs one readable line in the
# parked file, and a false write costs the vault a credential.
SECRET_PATTERNS = (
    (re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"), "a private key block"),
    (re.compile(r"\bAKIA[0-9A-Z]{16}\b"), "an AWS access key id"),
    (re.compile(r"\bghp_[A-Za-z0-9]{20,}"), "a GitHub personal access token"),
    (re.compile(r"\bgithub_pat_[A-Za-z0-9_]{20,}"), "a GitHub PAT"),
    (re.compile(r"\bsk-[A-Za-z0-9]{20,}"), "an API key"),
    (re.compile(r"(?im)^\s*(?:password|passwd|pwd|secret|api[_-]?key)\s*[:=]\s*\S+"),
     "a password/secret assignment"),
)

_FRONTMATTER = re.compile(r"\A---[ \t]*\r?\n(?P<fm>.*?)\r?\n---[ \t]*\r?\n?", re.S)
_COMMENT = re.compile(r"<!--.*?-->", re.S)
_WIKILINK = re.compile(r"\[\[([^\[\]\n]+)\]\]")
_FORBIDDEN_FILENAME = re.compile(r"[#|^:%\[\]]")
_DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")


# --- the protocol's checks ----------------------------------------------------

def _words(text: str) -> int:
    """Words in the note body, ignoring HTML comments so the provenance marker
    this module adds does not change a note's own length."""
    return len(_COMMENT.sub(" ", text).split())


def split_frontmatter(text: str) -> tuple[dict, list[str], str] | tuple[None, list, str]:
    """(key order, parsed keys, body). An empty key order means no frontmatter."""
    m = _FRONTMATTER.match(text)
    if not m:
        return None, [], text
    order, keys = [], {}
    for line in m.group("fm").splitlines():
        if not line.strip():
            continue
        if ":" not in line:
            return order, keys, text[m.end():]
        k, _, v = line.partition(":")
        order.append(k.strip())
        keys[k.strip()] = v.strip()
    return order, keys, text[m.end():]


def note_name(subject: str) -> str:
    """The note's filename, derived from the thread subject.

    The subject is the only title a thread has, so it IS the filename -- which
    is why a wiki subject that is not a valid note name parks the post rather
    than being mangled into one. Sanitising is limited to what cannot be part of
    a filename at all; everything else is judged, not silently rewritten.
    """
    return _FORBIDDEN_FILENAME.sub("", subject).strip().rstrip(".").strip()


def note_owner(note_path: Path) -> int | None:
    """The thread id the note at this path belongs to, or None if it is not one
    of this mirror's notes.

    None covers both a hand-written note and one whose marker cannot be read,
    and the caller treats those the same: neither may be overwritten. Returning
    the id rather than a boolean is what lets the caller tell "my note" from
    "another thread's note", which a marker-presence test cannot.
    """
    if not note_path.exists():
        return None
    m = _MARKER_OWNER.search(note_path.read_text(encoding="utf-8"))
    return int(m.group(1)) if m else None


def check_note(text: str, subject: str, today: date,
               existing_owner: int | None, thread_id: int) -> list[str]:
    """Every reason this text cannot be a note. Empty means it can.

    `existing_owner` is the thread id the target file already belongs to, or
    None if the file is free or was not written by this mirror. The file is
    rewritten only when it is this thread's own note; anything else parks.
    """
    problems: list[str] = []
    order, keys, body = split_frontmatter(text)

    if order is None:
        problems.append("no YAML frontmatter block at the top of the post")
    else:
        want = ["type", "tags", "summary", "updated"]
        if order != want:
            problems.append(
                f"frontmatter keys must be exactly {want} in that order, "
                f"got {order}")
        else:
            if keys["type"] not in NOTE_TYPES:
                problems.append(
                    f"type {keys['type']!r} is not one this mirror files in "
                    f"notes/ (allowed: {sorted(NOTE_TYPES)})")
            tags = [t.strip() for t in keys["tags"].strip("[]").split(",")
                    if t.strip()]
            if not tags:
                problems.append("tags is empty; the protocol wants at least one")
            else:
                bad = [t for t in tags if t not in CONTROLLED_TAGS]
                if bad:
                    problems.append(
                        f"tag(s) {bad} are not in the protocol's controlled "
                        f"vocabulary {sorted(CONTROLLED_TAGS)}")
                elif tags[0] not in TAG_MOC:
                    problems.append(
                        f"no MOC is defined for tag {tags[0]!r}, so the note "
                        f"would be an orphan")
                elif not (paths.VAULT_MAPS / f"{TAG_MOC[tags[0]]}.md").exists():
                    problems.append(
                        f"the MOC for tag {tags[0]!r} "
                        f"({TAG_MOC[tags[0]]}.md) does not exist in maps/")
            if len(keys["summary"]) > 140:
                problems.append(
                    f"summary is {len(keys['summary'])} chars; the cap is 140")
            if not _DATE.match(keys["updated"]):
                problems.append(
                    f"updated must be YYYY-MM-DD, got {keys['updated']!r}")
            elif keys["updated"] != today.isoformat():
                problems.append(
                    f"updated is {keys['updated']} but today is "
                    f"{today.isoformat()}; a note is a fact with a date")

        n = _words(body)
        cap = WORDS_MAX.get(keys.get("type") or "note", 200)
        if n > cap:
            problems.append(f"body is {n} words; type {keys.get('type')!r} "
                            f"caps at {cap}")
        if n < WORDS_MIN:
            problems.append(f"body is {n} words; a memory needs at least "
                            f"{WORDS_MIN}")
        if re.search(r"(?m)^#\s", body):
            problems.append("body contains an H1; the filename is the title")
        if len(_WIKILINK.findall(body)) < 2:
            problems.append("body has fewer than 2 [[wikilinks]]")
        if not re.search(r"(?m)^Related:", body):
            problems.append("body has no `Related:` line")

    for pattern, what in SECRET_PATTERNS:
        if pattern.search(text):
            problems.append(f"looks like it contains {what}; the protocol "
                            f"forbids storing secret values")

    name = note_name(subject)
    words = len(name.split())
    if not 2 <= words <= 6:
        problems.append(
            f"subject gives a {words}-word filename ({name!r}); the protocol "
            f"wants 2-6")
    note_path = paths.VAULT_NOTES / f"{name}.md"
    if note_path.exists():
        if existing_owner is None:
            problems.append(
                f"notes/{name}.md already exists and was not written by this "
                f"mirror; refusing to overwrite a hand-written note")
        elif existing_owner != thread_id:
            problems.append(
                f"notes/{name}.md is this mirror's note for thread "
                f"{existing_owner}, and this post is thread {thread_id}; two "
                f"threads with the same subject cannot share one note -- "
                f"reword whichever subject is wrong")
    return problems


# --- the mirror ---------------------------------------------------------------

def _park(thread_id: int, subject: str, author: str, body: str,
          reasons: list[str], today: date, redact: bool = False) -> Path:
    """Write the refusal where a human will see it, with the reasons.

    Under vault/agentdesk/parked/, not notes/: the file is deliberately not a
    note, so putting it in notes/ would be the corruption this gate exists to
    prevent.

    `redact` withholds the post itself. It is set when a credential shape is one
    of the reasons, and it is not optional politeness: the park file lives in
    the vault too, which is a git repo John pushes, so quoting the post "to
    explain why we refused it" would put the credential in the vault by the back
    door -- the check would have refused the note and leaked the secret in the
    same run. `_park_transcript` below makes exactly this decision for the same
    reason; this is that reasoning applied to the wiki side, where the body was
    previously reproduced unconditionally.
    """
    paths.ensure_dirs()
    name = note_name(subject) or f"thread-{thread_id}"
    park_file = paths.VAULT_PARKED / f"{thread_id}-{name}.md"
    lines = [
        f"# Parked wiki post - thread {thread_id}",
        "",
        f"Not mirrored into `notes/` on {today.isoformat()}. The thread is on",
        f"the board as **{subject}**, posted by {author}.",
        "",
        "## Why it is not a note",
        "",
    ]
    lines += [f"- {r}" for r in reasons]
    lines += [
        "",
        "## What to do",
        "",
        "Fix the opening post on the board and re-run the mirror "
        "(`python -m agentdesk.vault --thread "
        f"{thread_id}`). This file is rewritten or removed by that run; the "
        "board thread is the source.",
        "",
    ]
    if redact:
        lines += [
            "## The post",
            "",
            "Not reproduced here on purpose: one of the reasons above says it",
            "may contain a credential, and this file is inside the vault, which",
            "is a git repository John pushes. Copying the post in to complain",
            "about it would put the credential in the vault anyway. Read the",
            "thread on the board instead, and rotate or remove whatever the",
            "check found.",
            "",
        ]
    else:
        lines += ["## The post as written", "", body, ""]
    park_file.write_text("\n".join(lines), encoding="utf-8", newline="\n")
    return park_file


def _unpark(park_file: Path) -> None:
    """A post that used to be parked and now passes leaves no stale refusal."""
    if park_file.exists():
        park_file.unlink()


def _register(moc_path: Path, name: str, summary: str, today: date) -> None:
    """Put the note in its MOC, replacing any earlier line for it.

    The MOC is a hand-curated file, so the edit is as small as it can be: one
    line, under a section the mirror owns, plus the `updated` bump the protocol
    expects of any file that changed.
    """
    # Em dash, not a hyphen: the protocol writes the MOC entry as
    # `- [[Note]] — summary`, and the vault's MOCs are overwhelmingly in that
    # form (86 entries in GoCD MOC alone against 27 hyphenated ones, which are
    # the older hand-written minority). The line is matched for replacement
    # below by `- [[name]]` alone, so the separator can change without
    # stranding a line this mirror wrote earlier.
    entry = f"- [[{name}]] — {summary}"
    text = moc_path.read_text(encoding="utf-8")
    lines = text.splitlines()

    replaced = False
    for i, line in enumerate(lines):
        if line.startswith(f"- [[{name}]]"):
            lines[i] = entry
            replaced = True
            break

    if not replaced:
        if MOC_SECTION not in lines:
            if lines and lines[-1].strip():
                lines.append("")
            lines += [MOC_SECTION, ""]
        lines.append(entry)

    for i, line in enumerate(lines):
        if line.startswith("updated:"):
            lines[i] = f"updated: {today.isoformat()}"
            break

    moc_path.write_text("\n".join(lines).rstrip("\n") + "\n",
                        encoding="utf-8", newline="\n")


def mirror_thread(conn: sqlite3.Connection, thread_id: int,
                  dry_run: bool = False) -> dict:
    """Mirror one thread's opening post into the vault, or park it.

    Returns a result row shaped for the board to quote back to the author:
    status is 'mirrored', 'parked' or 'skipped'.
    """
    thread = conn.execute(
        "SELECT id, channel, subject FROM threads WHERE id=?",
        (thread_id,)).fetchone()
    if thread is None:
        return {"thread_id": thread_id, "status": "skipped",
                "reason": f"no such thread: {thread_id}", "reasons": []}
    if thread["channel"] != "wiki":
        return {"thread_id": thread_id, "status": "skipped",
                "reason": f"channel is {thread['channel']!r}, not 'wiki'",
                "reasons": []}

    first = conn.execute(
        "SELECT author, body FROM messages WHERE thread_id=? ORDER BY id LIMIT 1",
        (thread_id,)).fetchone()
    if first is None:
        return {"thread_id": thread_id, "status": "skipped",
                "reason": "thread has no messages", "reasons": []}

    today = datetime.now().astimezone().date()
    subject = thread["subject"]
    name = note_name(subject)
    note_path = paths.VAULT_NOTES / f"{name}.md"
    park_file = paths.VAULT_PARKED / f"{thread_id}-{name or 'thread'}.md"
    existing_owner = note_owner(note_path)

    problems = check_note(first["body"], subject, today, existing_owner, thread_id)
    if problems:
        if not dry_run:
            _park(thread_id, subject, first["author"], first["body"],
                  problems, today, redact=bool(_secret_problems(first["body"])))
        return {"thread_id": thread_id, "status": "parked",
                "subject": subject, "parked_at": str(park_file),
                "reasons": problems}

    order, keys, body = split_frontmatter(first["body"])
    frontmatter = first["body"][: len(first["body"]) - len(body)]
    marker = MARKER.format(open=MARKER_OPEN, thread_id=thread_id,
                           author=first["author"], date=today.isoformat())
    note_text = frontmatter + marker + "\n" + body.rstrip("\n") + "\n"

    primary_tag = keys["tags"].strip("[]").split(",")[0].strip()
    moc = TAG_MOC[primary_tag]

    if not dry_run:
        paths.ensure_dirs()
        note_path.write_text(note_text, encoding="utf-8", newline="\n")
        _register(paths.VAULT_MAPS / f"{moc}.md", name, keys["summary"], today)
        _unpark(park_file)
    return {"thread_id": thread_id, "status": "mirrored", "subject": subject,
            "note": str(note_path), "moc": moc, "reasons": []}


def mirror_all(conn: sqlite3.Connection, dry_run: bool = False) -> list[dict]:
    """Mirror every wiki thread. Idempotent, so this is the backfill."""
    ids = [r["id"] for r in conn.execute(
        "SELECT id FROM threads WHERE channel='wiki' ORDER BY id")]
    return [mirror_thread(conn, tid, dry_run=dry_run) for tid in ids]


# --- a settled question into the vault ------------------------------------------
#
# The other direction, and a different KIND of thing from the mirror above. A
# wiki post is a CANDIDATE for a note and is judged by the memory protocol; a
# settled question is a RECORD, and it is not judged: it goes in whole, because
# the point is that the reasoning survives -- why the question was asked and how
# it was settled. So it lands under agentdesk/questions/, which is the app's own
# folder for raw text, and never in notes/, for exactly the reason the daily
# transcript does not.
#
# WHY IT IS A SWEEP AND NOT PART OF REPLYING. Nothing here is triggered by the
# act of answering. The window runs archive_due() on every poll and the CLI can
# run it by hand, so the rule is "a settled question ends up in the vault",
# which holds for a question John answered in another instance, or before this
# window started. An archive wired into the reply path would be the narrower
# rule "a question John answered while this window was open ends up in the
# vault", and the difference only shows up as work that quietly did not happen.
#
# ORDER OF OPERATIONS, which is the load-bearing part: the transcript is written
# FIRST and the status flips to 'archived' only after it has actually landed. A
# vault that is unreachable, unwritable or full therefore leaves the question
# answered and still on the tab. That is the true state -- it is settled but not
# recorded -- and it is recoverable, where a question marked archived with no
# transcript behind it is not: it would have left the tab and be in no vault.
#
# IDEMPOTENT, and the status is what makes it so. A second call on an archived
# thread returns 'already-archived' without writing anything, and the filename
# is keyed to the thread id rather than to the subject, so even a re-run that
# somehow got past that check would rewrite the one file rather than leave a
# second one behind under the question's new wording.

def question_filename(thread_id: int) -> str:
    """The transcript's name, keyed to the thread id alone.

    Not to the subject, which is editable: a subject-keyed name would strand the
    old file as an orphan the first time somebody reworded a question, and
    "archiving twice writes nothing new" would quietly stop being true. The
    subject is the H1 inside the file, where it can change without moving it.
    """
    return f"question-{thread_id}.md"


def _secret_problems(text: str) -> list[str]:
    """Credential shapes found in a transcript about to be written to the vault.

    The same check the wiki mirror makes, and it is not hypothetical here: this
    board has already leaked a credential into a transcript once, and a
    question's transcript is the whole conversation rather than a curated note,
    so it is the more likely of the two to contain one. The vault is a git repo
    that John pushes, which makes a leaked key in it a leaked key on a remote.
    """
    return [f"the transcript looks like it contains {what}"
            for pattern, what in SECRET_PATTERNS if pattern.search(text)]


def _park_transcript(thread_id: int, subject: str, reasons: list[str],
                     today: date) -> tuple[Path, bool]:
    """Leave a readable refusal where a human will find it. Returns (path, was_new).

    Deliberately does NOT reproduce the transcript, which is the one place this
    differs from the wiki mirror's _park(): there the body is a candidate note
    and showing it is the whole point, here the body is what the check just
    flagged as possibly secret, and copying it into the vault to complain about
    it would put the credential in the vault anyway.
    """
    paths.ensure_dirs()
    park_file = paths.VAULT_PARKED / f"{question_filename(thread_id)}"
    was_new = not park_file.exists()
    lines = [
        f"# Parked question transcript - thread {thread_id}",
        "",
        f"Not archived on {today.isoformat()}. The question is on the board as",
        f"**{subject}**, and remains on the Questions tab.",
        "",
        "## Why it was not written to the vault",
        "",
    ]
    lines += [f"- {r}" for r in reasons]
    lines += [
        "",
        "## What to do",
        "",
        "The transcript is not reproduced here on purpose: the check above says",
        "it may contain a credential, and copying it in to complain about it",
        "would put the credential in the vault anyway. Read the thread on the",
        "board instead, remove or rotate whatever the check found, and the next",
        "archive pass will file it.",
        "",
    ]
    park_file.write_text("\n".join(lines), encoding="utf-8", newline="\n")
    return park_file, was_new


def _append_log_pointer(pointer: str, today: date) -> None:
    """Add one line to the vault's log for today, if it is not there already.

    The same shape as backup.py's pointer for the daily transcript, including
    the newline guard: a plain append onto a file whose last line has no
    terminator would weld the new line onto it. Duplicated rather than shared
    because backup.py is a different part of the app with its own file and this
    is eight lines; if a third caller ever appears, it belongs in one place.
    """
    log_file = paths.VAULT_LOG / f"{today.isoformat()}.md"
    if log_file.exists():
        existing = log_file.read_text(encoding="utf-8")
        if pointer not in existing.splitlines():
            prefix = "" if (not existing or existing.endswith("\n")) else "\n"
            with log_file.open("a", encoding="utf-8", newline="\n") as f:
                f.write(prefix + pointer + "\n")
    else:
        with log_file.open("w", encoding="utf-8", newline="\n") as f:
            f.write(pointer + "\n")


def render_question(conn: sqlite3.Connection, thread_id: int,
                    today: date) -> tuple[str, int]:
    """The full transaction: every message, in order, verbatim.

    Not a summary and not the last message. Author, author_kind, timestamp and
    the complete body of each -- the entire value of the archive is that the
    reasoning is recoverable later, and a summary would be somebody's reading of
    the reasoning rather than the reasoning.
    """
    thread = conn.execute(
        "SELECT id, subject, opened_by, created_ts, status FROM threads WHERE id=?",
        (thread_id,)).fetchone()
    msgs = conn.execute(
        "SELECT author, author_kind, ts, body FROM messages"
        " WHERE thread_id=? ORDER BY id", (thread_id,)).fetchall()
    lines = [
        f"# Question #{thread_id} - {thread['subject']}",
        "",
        f"Archived from the AgentDesk board on {today.isoformat()}. Opened by "
        f"{thread['opened_by']} on {thread['created_ts']}. Status at archive: "
        f"{thread['status']}.",
        "",
        f"Every message in the thread, in order, verbatim. {len(msgs)} in total.",
        "",
    ]
    for n, m in enumerate(msgs, 1):
        lines += [
            f"## {n}. {m['author']} ({m['author_kind']}) - {m['ts']}",
            "",
            m["body"].rstrip("\n"),
            "",
        ]
    return "\n".join(lines).rstrip("\n") + "\n", len(msgs)


def archive_question(conn: sqlite3.Connection, thread_id: int,
                     dry_run: bool = False) -> dict:
    """Write a settled question's transcript to the vault, then mark it archived.

    Returns a result row: status is one of

        'archived'         written, and the thread is now STATUS_ARCHIVED
        'already-archived' nothing written; this is the idempotent second call
        'not-ready'        the question is still open, or an agent has the
                           last word in it, so nothing may be filed
        'refused'          a credential shape was found; nothing was written
        'skipped'          not a question thread, or no such thread

    Only 'archived' means the thread left the Questions tab.
    """
    row = conn.execute(
        "SELECT t.id, t.channel, t.status, t.subject,"
        " (CASE WHEN " + db.WAITING_SQL + " THEN 1 ELSE 0 END) AS waiting"
        " FROM threads t WHERE t.id=?", (thread_id,)).fetchone()
    if row is None:
        return {"thread_id": thread_id, "status": "skipped",
                "reason": f"no such thread: {thread_id}", "reasons": []}
    if row["channel"] != "question":
        return {"thread_id": thread_id, "status": "skipped",
                "reason": f"channel is {row['channel']!r}, not 'question'",
                "reasons": []}

    target = paths.VAULT_QUESTIONS / question_filename(thread_id)
    if row["status"] == paths.STATUS_ARCHIVED:
        return {"thread_id": thread_id, "status": "already-archived",
                "subject": row["subject"], "file": str(target), "reasons": []}
    if row["status"] not in (paths.STATUS_ANSWERED, paths.STATUS_CLOSED):
        return {"thread_id": thread_id, "status": "not-ready",
                "subject": row["subject"], "status_now": row["status"],
                "reasons": [f"a question is archived once it is answered or "
                            f"closed; this one is {row['status']!r}"]}
    # Settled by status is not settled. A question John answered once whose
    # newest message is an agent's asking again carries the settled status and
    # is NOT settled, and filing it here would take it off his tab while it is
    # the one thing on the board he still owes an answer to. db.WAITING_SQL's
    # other user, questions_to_archive, filters on the same thing; this is the
    # explicit `--archive-question N` path, and the rule is only a rule if it
    # holds on the paths nobody takes by accident too.
    if row["waiting"]:
        return {"thread_id": thread_id, "status": "not-ready",
                "subject": row["subject"], "status_now": row["status"],
                "reasons": [f"an agent has the last word in this question, so "
                            f"it is waiting on John rather than settled; it is "
                            f"archived once he replies or closes it"]}

    today = datetime.now().astimezone().date()
    text, count = render_question(conn, thread_id, today)

    problems = _secret_problems(text)
    if problems:
        parked_at, was_new = None, False
        if not dry_run:
            park_file, was_new = _park_transcript(thread_id, row["subject"],
                                                  problems, today)
            parked_at = str(park_file)
        return {"thread_id": thread_id, "status": "refused",
                "subject": row["subject"], "parked_at": parked_at,
                "parked_new": was_new, "reasons": problems}

    if dry_run:
        return {"thread_id": thread_id, "status": "archived",
                "subject": row["subject"], "file": str(target),
                "messages": count, "reasons": []}

    paths.ensure_dirs()
    target.write_text(text, encoding="utf-8", newline="\n")
    # Checked rather than assumed. The contract this function exists to keep is
    # "no thread is marked archived unless its transcript landed", and a write
    # that reported success without producing a file would break it silently.
    if not target.exists():
        return {"thread_id": thread_id, "status": "skipped",
                "subject": row["subject"],
                "reason": (f"the transcript did not land at {target}; the "
                           f"thread is left as it was"),
                "reasons": []}

    _append_log_pointer(
        f"- [[agentdesk/questions/question-{thread_id}]] - AgentDesk question "
        f"#{thread_id} archived: {row['subject']}", today)
    # `archived_from` is what makes the archive reversible without guessing:
    # restoring the thread needs the status it was settled with, and 'answered'
    # is the wrong answer for a question John explicitly CLOSED. It is written
    # in the same UPDATE as the status, so the two cannot disagree.
    #
    # Any hold is dropped here rather than being honoured, because reaching this
    # line means the transcript has landed: a hold says "do not sweep this", and
    # this question has just been swept. It is also the path the explicit
    # Close & archive button takes, which has to be able to re-file a question
    # that was brought back.
    db.set_thread_status(conn, thread_id, paths.STATUS_ARCHIVED,
                         meta_updates={"archived_from": row["status"],
                                       "archive_hold": None})
    return {"thread_id": thread_id, "status": "archived",
            "subject": row["subject"], "file": str(target),
            "messages": count, "reasons": []}


def archive_due(conn: sqlite3.Connection, dry_run: bool = False) -> list[dict]:
    """Archive every settled question that is not in the vault yet.

    Idempotent, so this is safe to call every poll tick: the questions it has
    already filed report 'already-archived' and write nothing.
    """
    return [archive_question(conn, r["id"], dry_run=dry_run)
            for r in db.questions_to_archive(conn)]


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Mirror AgentDesk wiki posts into the memory vault.")
    parser.add_argument("--thread", type=int, action="append",
                        help="mirror one thread id (repeatable)")
    parser.add_argument("--all", action="store_true",
                        help="mirror every wiki thread")
    parser.add_argument("--archive-question", type=int, action="append",
                        help="archive one settled question id (repeatable)")
    parser.add_argument("--archive-due", action="store_true",
                        help="archive every settled question not yet in the "
                             "vault; this is the backstop for a question "
                             "answered while no window was running")
    parser.add_argument("--dry-run", action="store_true",
                        help="report what would happen; write nothing")
    parser.add_argument("--json", action="store_true",
                        help="print the results as JSON")
    args = parser.parse_args()

    conn = db.connect()
    try:
        if args.archive_due:
            results = archive_due(conn, dry_run=args.dry_run)
        elif args.archive_question:
            results = [archive_question(conn, t, dry_run=args.dry_run)
                       for t in args.archive_question]
        elif args.all:
            results = mirror_all(conn, dry_run=args.dry_run)
        elif args.thread:
            results = [mirror_thread(conn, t, dry_run=args.dry_run)
                       for t in args.thread]
        else:
            parser.error("give --thread N (repeatable), --all, "
                         "--archive-question N (repeatable) or --archive-due")
    finally:
        conn.close()

    if args.json:
        print(json.dumps(results, indent=2, ensure_ascii=False))
        return
    for r in results:
        # One printer for two result shapes: the mirror's keys are note/parked_at
        # and the archive's are file/parked_at, and every one of them is a path
        # worth showing. 'reason' is the single-line "why not", 'reasons' is the
        # list a refusal or a park carries.
        where = r.get("note") or r.get("file") or r.get("parked_at")
        line = f"thread {r['thread_id']}: {r['status']}"
        if where:
            line += f" -> {where}"
        if r.get("reason"):
            line += f" ({r['reason']})"
        print(line)
        for reason in r.get("reasons", []):
            print(f"    - {reason}")


if __name__ == "__main__":
    main()
