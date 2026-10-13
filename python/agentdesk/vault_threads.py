"""The board as vault notes, and the vault's git backup.

backup.write_vault gives each day a transcript; this gives each THREAD a note of
its own (the whole conversation, however many days it ran), linked to the days it
touched, the authors' bio threads and the threads it mentions, so Obsidian's
backlinks and graph work across the board. Then vault_sync commits what the
backup wrote and pushes it, so the history is on GitHub and not only on this
laptop.

Everything here derives its paths from `paths` at CALL time, never at import,
so a scratch run that reassigns paths.VAULT_* (see check_vault_skip.py) cannot
reach John's real vault by way of a constant captured earlier.
"""

import re
import sqlite3
import subprocess
from datetime import datetime, timezone
from pathlib import Path

from . import paths

# Claude is the author of what this module commits, per the vault's convention;
# the committer stays whoever's credential pushes it.
_AUTHOR_ENV = {
    "GIT_AUTHOR_NAME": "claude[bot]",
    "GIT_AUTHOR_EMAIL": "209825114+claude[bot]@users.noreply.github.com",
}
_SIGN = ["-c", "gpg.format=openpgp", "-c",
         "user.signingkey=C079BAAABAD6BBEDBDE3FFD9C5D98EBD6765C188",
         "-c", "commit.gpgsign=true"]
_TRAILER = "Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"

_THREAD_REF = re.compile(r"\b([Tt]hread) #?(\d+)\b")


def thread_dir() -> Path:
    return paths.VAULT_AGENTDESK / "threads"


def thread_name(tid: int) -> str:
    return f"thread-{tid}"


def link(tid: int, label: str | None = None) -> str:
    """A wikilink to a thread's note. The path is spelled out so it resolves
    whatever else in the vault is called thread-N."""
    target = f"agentdesk/threads/{thread_name(tid)}"
    return f"[[{target}|{label}]]" if label else f"[[{target}]]"


def _yaml(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", " ") + '"'


def _day(ts: str) -> str:
    dt = datetime.fromisoformat(ts)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.astimezone().date().isoformat()


def _bio_ids(conn: sqlite3.Connection) -> dict[str, int]:
    """author (lower case) -> the thread that holds their `bio: <name>`."""
    out: dict[str, int] = {}
    for r in conn.execute("SELECT id, subject FROM threads WHERE subject LIKE 'bio:%' ORDER BY id"):
        out[r["subject"][4:].strip().lower()] = r["id"]
    return out


def render_thread(conn: sqlite3.Connection, tid: int, bios: dict[str, int],
                  known: set[int] | None = None) -> str | None:
    """`known` is every thread id that has a note: a mention of any other number
    (an example in someone's text, a typo) stays plain text, not a broken link."""
    t = conn.execute("SELECT * FROM threads WHERE id = ?", (tid,)).fetchone()
    if t is None:
        return None
    msgs = [dict(r) for r in conn.execute(
        "SELECT id, ts, author, author_kind, body, reply_to FROM messages"
        " WHERE thread_id = ? ORDER BY id", (tid,))]
    days = sorted({_day(m["ts"]) for m in msgs})
    updated = days[-1] if days else _day(t["created_ts"])
    tags = ["agentdesk", t["channel"], t["status"]]
    lines = [
        "---",
        "type: agentdesk-thread",
        f"tags: [{', '.join(tags)}]",
        f"summary: {_yaml(t['subject'])}",
        f"aliases: [{_yaml(t['subject'])}]",
        f"updated: {updated}",
        "---",
        f"# {t['subject']}",
        "",
        f"Thread {tid} · channel {t['channel']} · status {t['status']} · "
        f"opened by {t['opened_by']}"
        + (f" ({link(bios[t['opened_by'].lower()], 'bio')})" if t["opened_by"].lower() in bios
           and bios[t["opened_by"].lower()] != tid else "")
        + f" · {len(msgs)} message(s)",
    ]
    if days:
        lines.append("Days: " + " ".join(f"[[agentdesk/{d}]]" for d in days))
    lines += ["", "## Messages", ""]
    for m in msgs:
        who = m["author"]
        bio = bios.get(who.lower())
        who_txt = link(bio, who) if bio is not None and bio != tid else who
        reply = f" (reply to #{m['reply_to']})" if m["reply_to"] else ""
        body = _THREAD_REF.sub(
            lambda x: f"{x.group(1)} {link(int(x.group(2)), x.group(2))}"
            if int(x.group(2)) != tid and (known is None or int(x.group(2)) in known) else x.group(0),
            m["body"].replace("\r\n", "\n"))
        body = body.replace("\n", "\n  ")
        local = datetime.fromisoformat(m["ts"])
        if local.tzinfo is None:
            local = local.replace(tzinfo=timezone.utc)
        stamp = local.astimezone().strftime("%Y-%m-%d %H:%M")
        lines.append(f"- {stamp} **{who_txt}** ({m['author_kind']}){reply} - {body}")
    return "\n".join(lines).rstrip("\n") + "\n"


def _write_if_changed(path: Path, text: str) -> bool:
    if path.exists() and path.read_text(encoding="utf-8") == text:
        return False
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    return True


def write_threads(conn: sqlite3.Connection, touched: set[int]) -> int:
    """Write the note for every thread. `touched` is what had a message today;
    kept for callers, but every thread is checked. Returns how many files changed."""
    d = thread_dir()
    d.mkdir(parents=True, exist_ok=True)
    every = {r[0] for r in conn.execute("SELECT id FROM threads")}
    bios = _bio_ids(conn)
    changed = 0
    # Every thread, not only today's: rendering is cheap, a file is written only when its text changed, and it heals a note a later fix
    # of the renderer (or a thread that appeared after a mention of it) would leave stale.
    for tid in sorted(every):
        text = render_thread(conn, tid, bios, every)
        if text is not None and _write_if_changed(d / f"{thread_name(tid)}.md", text):
            changed += 1
    return changed


def render_moc(conn: sqlite3.Connection) -> str:
    """maps/AgentDesk MOC.md: every thread by channel, newest first."""
    lines = [
        "---", "type: moc", "tags: [moc, agentdesk]",
        "summary: Every AgentDesk board thread, by channel - the board's history in the vault",
        f"updated: {datetime.now().astimezone().date().isoformat()}", "---",
        "Generated by the AgentDesk backup from the board; edit the threads, not this map.",
        "Daily transcripts are in `agentdesk/YYYY-MM-DD`.", "",
    ]
    for ch in [r[0] for r in conn.execute("SELECT DISTINCT channel FROM threads ORDER BY channel")]:
        lines += [f"## {ch}", ""]
        for r in conn.execute(
                "SELECT id, subject, status FROM threads WHERE channel = ? ORDER BY id DESC", (ch,)):
            lines.append(f"- {link(r['id'], r['subject'])} - {r['status']}")
        lines.append("")
    return "\n".join(lines).rstrip("\n") + "\n"


def write_moc(conn: sqlite3.Connection) -> bool:
    return _write_if_changed(paths.VAULT_MAPS / "AgentDesk MOC.md", render_moc(conn))


# --- git ----------------------------------------------------------------------

def _git(args: list[str], cwd: Path, env_extra: dict | None = None,
         timeout: int = 120) -> subprocess.CompletedProcess:
    import os
    env = dict(os.environ)
    env.update(env_extra or {})
    return subprocess.run(["git", *args], cwd=str(cwd), env=env, capture_output=True,
                          text=True, timeout=timeout)


_PATHS = ["agentdesk", "log", "maps/AgentDesk MOC.md"]


def vault_sync(vault: Path | None = None, push: bool = True) -> dict:
    """Commit what the backup wrote (agentdesk/, log/, the map) and push it.

    Only those paths are committed, so a note another session is mid-way through
    writing is left for its author. A failure never raises: a backup that dies
    because GitHub is unreachable would lose the snapshot too, so the problem is
    reported in the result and retried by the next run (the commit stays local).
    """
    vault = Path(vault) if vault is not None else paths.VAULT_DIR
    out = {"committed": False, "pushed": False, "unpushed": None, "error": None}
    try:
        if not (vault / ".git").exists():
            out["error"] = "the vault is not a git repository"
            return out
        _git(["add", "--", *_PATHS], vault)
        if _git(["diff", "--cached", "--quiet", "--", *_PATHS], vault).returncode != 0:
            stamp = datetime.now().astimezone().strftime("%Y-%m-%d %H:%M")
            msg = f"AgentDesk board backup {stamp}\n\n{_TRAILER}"
            r = _git([*_SIGN, "commit", "-q", "-m", msg, "--", *_PATHS], vault, _AUTHOR_ENV)
            if r.returncode != 0:  # the signing key is not usable here: an unsigned backup beats none
                r = _git(["-c", "commit.gpgsign=false", "commit", "-q", "-m", msg, "--", *_PATHS],
                         vault, _AUTHOR_ENV)
            if r.returncode != 0:
                out["error"] = "commit failed: " + (r.stderr or r.stdout).strip()[:300]
                return out
            out["committed"] = True
        branch = _git(["rev-parse", "--abbrev-ref", "HEAD"], vault).stdout.strip()
        upstream = f"origin/{branch}"
        ahead = _git(["rev-list", "--count", f"{upstream}..HEAD"], vault)
        out["unpushed"] = int(ahead.stdout.strip()) if ahead.returncode == 0 else None
        if push and out["unpushed"] != 0:
            r = _git(["push", "origin", "HEAD"], vault)
            if r.returncode != 0:  # someone else pushed first: rebase once, then try again
                if _git(["pull", "--rebase", "--autostash", "origin", branch], vault).returncode != 0:
                    _git(["rebase", "--abort"], vault)
                r = _git(["push", "origin", "HEAD"], vault)
            if r.returncode == 0:
                out["pushed"], out["unpushed"] = True, 0
            else:
                out["error"] = "push failed: " + (r.stderr or r.stdout).strip()[:300]
    except (OSError, subprocess.SubprocessError) as e:
        out["error"] = f"{type(e).__name__}: {e}"
    return out
