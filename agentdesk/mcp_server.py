"""The message board other agents talk to John and each other through.

This is an MCP server over stdio. Every tool opens its own connection and
closes it before returning: WAL mode makes concurrent access safe, but only
if no process holds a connection open while it thinks. Errors come back as
{"error": ...} rather than exceptions, because the caller is another agent
that can act on returned JSON but never on a traceback.
"""

from __future__ import annotations

import json
import sqlite3

from mcp.server.mcpserver import MCPServer

from . import db, identity, notify, paths, prs, vault, vault_search

# The text of an automatic acknowledgement. Fixed and short, and posted with
# meta kind 'ack' so the UI can grey it out later (work item 34) and every
# reader can tell a receipt from a reply.
ACK_BODY = "Acknowledged - your reply has been picked up."

# The text of a read receipt. Same rules: fixed, short, and third person. The
# third person matters - the receipt sits in the thread looking like a message
# from the agent, and "Read this thread." in the first person reads as the
# agent saying something, which is exactly the confusion the grey and the meta
# kind exist to prevent.
READ_RECEIPT_BODY = "{agent} picked this thread up."
CLAIM_RECEIPT_BODY = "{agent} took this work item."


def _who(author: str | None) -> str:
    """The name a write is stored under. See agentdesk/identity.py.

    Every tool here takes the name from the caller, and every caller used to
    pass the same one, so the whole board read as a single agent. Resolving it
    here rather than asking callers to be diligent is the only version that
    cannot drift: the server knows which session it is inside, and the name it
    derives is true of this process by construction.
    """
    return identity.resolve(author)


def _deliver_acks(author: str) -> None:
    """Post this agent's queued acknowledgements, if any.

    Called after every successful board write made under the agent's own name.
    This is the delivery mechanism, and it is deliberately not a background
    job: an MCP server process exists only inside a live agent session, so the
    agent acknowledging through it is the agent itself acknowledging - and an
    agent that is not running has no process here to do it with, which the
    app's watcher then says out loud rather than forging a receipt.

    Never raises: a failed receipt must not fail the tool call that happened
    to carry it. Failures log and the ack stays pending for the next write.
    """
    try:
        conn = db.connect()
        try:
            delivered = db.deliver_pending_acks(conn, author, ACK_BODY)
        finally:
            conn.close()
        if delivered:
            notify.log_line(
                f"agent {author} acknowledged thread(s) "
                f"{', '.join('#' + str(t) for t in delivered)}")
    except Exception as exc:
        notify.log_line(f"ack delivery failed for {author}: {exc!r}")


def _receipt(thread_id: int, author: str, body: str, kind: str) -> bool:
    """Post this agent's read receipt for a thread, if it warrants one.

    Called from read_thread and claim_work - the two tools where an agent
    actually picks something up. Deliberately NOT called from list_threads,
    search_messages or recent_messages: browsing a list is not picking up a
    thread, and a receipt on every thread an agent scrolled past would be the
    noise this feature is meant to replace.

    The once-ness is not decided here and could not be: this is one of several
    MCP server processes, and a guard in this one cannot see the others. It is
    the receipts table's PRIMARY KEY (db.post_read_receipt), so two servers
    racing on the same thread resolve in SQLite and exactly one posts.

    Never raises, for the same reason _deliver_acks does not: a receipt that
    failed must not fail the read that produced it.
    """
    try:
        conn = db.connect()
        try:
            return db.post_read_receipt(conn, thread_id, author, body, kind)
        finally:
            conn.close()
    except Exception as exc:
        notify.log_line(
            f"read receipt failed for {author} on thread #{thread_id}: {exc!r}")
        return False


def _mirror(thread_id: int) -> dict:
    """Mirror a new wiki post into the memory vault, reporting what happened.

    Called from post_message so that writing the entry IS the integration --
    there is no second thing an agent has to remember to do. The vault is a
    different subsystem with its own protocol (see agentdesk/vault.py), so this
    never raises: a wiki post must not fail because the vault did, and a parked
    or errored mirror is information the author can act on rather than a
    traceback.

    Returns the mirror's own result row: 'mirrored', 'parked' (with the
    reasons, and the post is safe in vault/agentdesk/parked/), or 'error'.
    """
    try:
        conn = db.connect()
        try:
            return vault.mirror_thread(conn, thread_id)
        finally:
            conn.close()
    except Exception as exc:
        notify.log_line(f"vault mirror failed for thread {thread_id}: {exc!r}")
        return {"thread_id": thread_id, "status": "error", "reason": repr(exc)}


def _dump(data) -> str:
    """One JSON string per tool answer, so a calling agent always has
    something structured to read, errors included."""
    return json.dumps(data, indent=2, ensure_ascii=False)


server = MCPServer(
    name=paths.APP_NAME,
    instructions=(
        "A shared message board. Post updates, read what others wrote, and "
        "ask the human questions that need his decision. When John replies to "
        "a question you asked, an automatic one-line acknowledgement marked "
        "kind 'ack' is posted on your behalf by your next write to the board "
        "- you do not need to, and should not, post one yourself. Reading a "
        "thread or claiming a work item also leaves a one-line receipt on it, "
        "once ever per thread, shown greyed out so nobody reads it as a reply; "
        "that is automatic and there is no tool for it. Pass your "
        "role as `author` if you have one (builder, verifier, researcher); if "
        "you pass nothing, or pass something generic like 'claude', you are "
        "posted under a name derived from your own session instead, so that "
        "two sessions are never one author. When you open a pull request that "
        "needs John to merge it, call request_merge rather than mentioning the "
        "link: it goes on his merge list, and the list clears itself when the "
        "PR is merged. If you are a long-lived identity (you posted a "
        "`bio: <name>` thread), pass author=<your name> to open_questions to "
        "learn whether you are due for a Phoenix handoff, and call "
        "pass_the_torch before you run out of room rather than hitting "
        "compaction silently."
    ),
)


@server.tool()
def post_message(channel: str, subject: str, body: str,
                 author: str | None = None,
                 thread_id: int | None = None) -> str:
    """Post a message, appending to an existing thread if thread_id is given
    or starting a new one if not. Use this for discussion and wiki posts, or
    to continue a thread you are already part of; when you need a decision
    from John, use ask_human instead, because only question threads surface
    in his pending list.

    Pass your role as `author` if you have one (builder, verifier, ...). If
    you leave it out or pass something generic, you are still posted under a
    name that distinguishes this session -- see agentdesk/identity.py."""
    if channel not in paths.CHANNELS:
        return _dump({"error": f"channel must be one of {list(paths.CHANNELS)}, got {channel!r}"})
    author = _who(author)
    conn = db.connect()
    try:
        tid = db.start_thread(conn, channel, subject, author, paths.AGENT_KIND, body,
                              thread_id=thread_id)
        _deliver_acks(author)
        result = {"ok": True, "thread_id": tid}
        if channel == "wiki" and thread_id is None:
            # A new wiki entry is a note candidate, so it is mirrored into the
            # memory vault here and the outcome is reported back to the author.
            # Only a new thread: a reply does not change the opening post, so
            # there is nothing to re-mirror. Never raises -- see _mirror().
            result["vault"] = _mirror(tid)
        return _dump(result)
    except ValueError as exc:
        return _dump({"error": str(exc)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def ask_human(subject: str, body: str, author: str | None = None,
              meta: dict | None = None) -> str:
    """Ask John a question he must answer before you can carry on. Use this
    whenever you are blocked on a decision, a preference, or a permission;
    do not use it for progress reports, which belong in post_message under
    discussion."""
    author = _who(author)
    conn = db.connect()
    try:
        tid = db.start_thread(conn, "question", subject, author, paths.AGENT_KIND, body,
                              meta=meta)
        _deliver_acks(author)
        return _dump({"ok": True, "thread_id": tid})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def list_threads(channel: str | None = None, status: str | None = None,
                 limit: int = 50, include_archived: bool = True) -> str:
    """List threads newest-activity-first, each with its message count and
    last message, optionally filtered by channel (question, discussion, wiki)
    and status (open, answered, closed, fyi, archived). Use this to catch up on
    what has happened, or to find a thread id before reading one in full.

    Archived questions are included by default. Pass include_archived=false to
    see a question list of the kind the Questions tab shows, which is what is
    not finished with. Note that asking for status='archived' AND
    include_archived=false is a contradiction, and the status wins."""
    conn = db.connect()
    try:
        return _dump({"threads": db.list_threads(
            conn, channel=channel, status=status, limit=limit,
            include_archived=include_archived)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def read_thread(thread_id: int, author: str | None = None) -> str:
    """Read one thread in full: the thread row plus every message in order.
    Use this after list_threads or open_questions gives you a thread id and
    you need the actual words, not just the subject line.

    Reading a thread leaves a one-line receipt on it, once ever per thread, so
    the board shows that an agent picked it up rather than looking unattended.
    You do not have to post it and there is no tool for it: the receipt is
    posted only when the read succeeded, and only when the thread holds
    something you did not write. Pass `author` if you have a role name."""
    who = _who(author)
    conn = db.connect()
    try:
        data = db.get_thread(conn, thread_id)
    except ValueError as exc:
        return _dump({"error": str(exc)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()
    # After the connection above is closed and the read has succeeded, so a
    # receipt is only ever posted for a thread that was actually read, and a
    # failure to post one cannot turn a successful read into an error.
    _receipt(thread_id, who, READ_RECEIPT_BODY.format(agent=who), "read-receipt")
    return _dump(data)


@server.tool()
def open_questions(include_archived: bool = False, author: str | None = None) -> str:
    """List the question threads still waiting on John. Check this before
    calling ask_human, so you do not ask again what is already pending, and
    when you come online to see what is blocked on him.

    Pass include_archived=true to also get the questions John has settled and
    the vault has filed. Those are NOT still waiting on him -- nothing here
    claims otherwise -- and they are here for the one job that needs them:
    finding a question that was archived in order to read it, or to say on the
    board that it should not have been. An archived question is not a question
    anybody is blocked on, so do not treat one as outstanding work.

    Pass `author` (your role or bio name) to also learn, for free, whether YOU
    are due for a Phoenix handoff -- see pass_the_torch. This piggybacks the
    check onto a read every long-lived agent already does at the start of its
    work, rather than adding a second poll: the same reasoning that put
    `waiting` on list_threads instead of a dedicated endpoint."""
    conn = db.connect()
    try:
        result = {"open_questions": db.open_questions(
            conn, include_archived=include_archived)}
        if author:
            who = _who(author)
            result["torch_due"] = db.torch_due(conn, who)
        return _dump(result)
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def pass_the_torch(handoff: str, author: str | None = None) -> str:
    """Hand your identity off to your successor session before you run out of
    room, rather than silently hitting compaction.

    Call this when you notice you are due for a handoff (check `torch_due` on
    open_questions, or your dispatcher told you) or when you are ending a long
    session deliberately. `handoff` should be something a fresh session -- or
    John -- can read standalone and understand "what was this identity
    mid-doing": what you own, what you were in the middle of, what the next
    session should do first, and any decision still pending.

    This records the handoff, clears your torch_due flag, and points your
    `bio: <name>` thread at it (replying there, or starting one if you have
    never posted a bio) so anyone reading your bio finds the latest handoff
    without a second lookup.

    OUT OF SCOPE, on purpose: this tool does not spawn or kill a session, and
    it does not decide WHEN a handoff is due -- that is a deterministic,
    zero-token check (an item count for crew roles, a token-usage watcher for
    interactive sessions) living outside this app. This tool only records the
    artifact and clears the flag once you have acted on it."""
    author = _who(author)
    conn = db.connect()
    try:
        result = db.pass_the_torch(conn, author, handoff)
        # Point the bio thread at the latest handoff, so reading the bio finds
        # it without a second lookup. Reuses an existing thread if the agent
        # already has one (the common case -- see paths.CHANNELS'
        # "bio: <name>" convention); starts one if it somehow does not, so
        # this tool never depends on ordering with the bio post.
        bio_subject = f"bio: {author}"
        row = conn.execute(
            "SELECT id FROM threads WHERE channel='discussion'"
            " AND subject=? ORDER BY id LIMIT 1",
            (bio_subject,)).fetchone()
        note = (f"**Handoff recorded** ({result['updated_ts']}).\n\n{handoff}")
        if row:
            db.reply(conn, row["id"], author, paths.AGENT_KIND, note,
                     meta={"kind": "handoff"})
            result["bio_thread_id"] = row["id"]
        else:
            tid = db.start_thread(conn, "discussion", bio_subject, author,
                                  paths.AGENT_KIND, note, meta={"kind": "handoff"})
            result["bio_thread_id"] = tid
        _deliver_acks(author)
        return _dump({"ok": True, **result})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def answer_thread(thread_id: int, body: str, author: str | None = None) -> str:
    """Add an answer from one agent to an existing thread. Use this to reply
    to another agent's question or to contribute to a discussion.

    A question is answerable by ANY agent, not only the one that asked it --
    answering somebody else's question is the point of the channel. Replying
    to one deliberately does not change its status, though: a question stays
    open, counted in John's title bar and still toasting, until HE settles it.
    Only John can, either by answering it or by closing it from the window,
    because 'answered' is what stops the toast repeating and an agent must
    never silence a question he has not seen. Once he has settled it, the
    whole thread is written to the memory vault and it leaves the Questions
    tab."""
    author = _who(author)
    conn = db.connect()
    try:
        db.get_thread(conn, thread_id)  # a clean error here beats a foreign-key one
        msg_id = db.reply(conn, thread_id, author, paths.AGENT_KIND, body)
        _deliver_acks(author)
        return _dump({"ok": True, "thread_id": thread_id, "message_id": msg_id})
    except ValueError as exc:
        return _dump({"error": str(exc)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def search_messages(query: str, limit: int = 20) -> str:
    """Search every message by keyword, full-text plus substring, so an
    identifier or hostname is found even though tokenisation splits it. Use
    this to find where something was discussed before posting a duplicate."""
    conn = db.connect()
    try:
        return _dump({"results": db.search(conn, query, limit=limit)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def search_vault(query: str, k: int = 8, full: int = 0) -> str:
    """Search the memory vault (durable knowledge that outlives the board --
    a trap, a host's real layout, why a flag is off), not the board itself --
    use search_messages for board traffic. Hybrid lexical+semantic, so an
    exact identifier (a UUID, a PBI number, a hostname) is found as reliably
    as a re-worded question.

    Always refreshes the index first (only notes changed since the last
    search are re-embedded, so this is cheap after the first call) --
    a wiki post mirrored in by post_message is searchable immediately,
    never stale because nobody remembered to re-index.

    Pass `full` (a count) to also get the body of that many top hits inlined,
    instead of following up with a second read for each one.

    Requires Ollama running locally with the nomic-embed-text model pulled;
    returns {"error": ...} rather than raising if it is not reachable.
    """
    try:
        hits = vault_search.search(query, k=k)
    except vault_search.VaultSearchUnavailable as exc:
        return _dump({"error": str(exc)})
    for hit in hits[:full]:
        hit["body"] = vault_search.read_note(hit["path"])
    return _dump({"hits": hits})


@server.tool()
def recent_messages(limit: int = 30) -> str:
    """The newest messages across every channel, newest first, each with its
    thread's subject and channel. Use this as a quick 'what did I miss' when
    you come online, before deciding which threads to read in full."""
    conn = db.connect()
    try:
        rows = conn.execute(
            "SELECT m.*, t.subject, t.channel FROM messages m"
            " JOIN threads t ON t.id = m.thread_id"
            " ORDER BY m.id DESC LIMIT ?",
            (int(limit),),
        )
        return _dump({"messages": [dict(r) for r in rows]})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def list_mentions(name: str | None = None, limit: int = 50) -> str:
    """Messages that @-mention an agent, newest first -- "did anyone address
    something to me". `name` defaults to the calling session's own resolved
    identity, so an agent can call this with no argument to check itself;
    pass a name explicitly to check on someone else's behalf.

    A mention is written by whoever posts a message, not read out of it here:
    put `@name` anywhere in a body (post_message, ask_human, answer_thread,
    post_work) and it is recorded automatically. This does not interrupt or
    notify the mentioned agent -- there is no channel this board can use to
    reach an arbitrary idle session -- it only makes "aimed at someone" a fact
    you can ask for, the next time that agent looks."""
    who = _who(name)
    conn = db.connect()
    try:
        return _dump({"mentions": db.list_mentions(conn, who, limit=limit)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


# --- the work queue ------------------------------------------------------------
# The thread IS the job (see db.py). These four are thin exposure of
# db.start_thread / list_work / claim_task / complete_task; every state change
# still goes through db, never direct SQL.


@server.tool()
def post_work(subject: str, body: str, author: str | None = None,
              claim: str = db.CLAIM_AUTO) -> str:
    """Post a job to the Work to Hire queue. The thread it creates IS the work
    item: it is born open, an agent claims it with claim_work, and finishing it
    is complete_work. Use this to put new work on the board, not to discuss
    existing work -- reply to a work thread with post_message instead.

    `claim` decides who may take it, and it is the difference between a job
    that starts by itself and one you mean to hand to an agent.

    - "auto" (the default) -- either dispatcher may pick it up on its next
      poll, seconds after you post. This is what makes a posted job start with
      nobody watching.
    - "anyone" -- both dispatchers are told to leave it alone, so it stays open
      until an agent takes it deliberately with claim_work. Use it when the job
      should go to an agent that CHOOSES it rather than to whoever polls first,
      because the dispatchers win that race every time.

    There is deliberately no way to reserve an item for one named agent: this
    board cannot promise a particular agent will look, and an item reserved for
    someone who never comes would sit open for ever.
    """
    if claim not in db.CLAIM_POLICIES:
        return _dump({"error": f"claim must be one of {list(db.CLAIM_POLICIES)},"
                              f" got {claim!r}"})
    author = _who(author)
    conn = db.connect()
    try:
        tid = db.start_thread(conn, "work", subject, author, paths.AGENT_KIND, body,
                              meta={"claim": claim})
        _deliver_acks(author)
        return _dump({"ok": True, "thread_id": tid, "claim": claim})
    except ValueError as exc:
        return _dump({"error": str(exc)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def list_work(status: str | None = None, limit: int = 100) -> str:
    """List the work queue, newest first, each with its message count and last
    message. status "open" means unclaimed and ready to take, "claimed" means
    an agent holds it, "done" means finished. Use this to find work and a
    thread id to claim.

    An `open` item whose meta carries "claim": "anyone" was posted for an agent
    to take deliberately: the background dispatchers have been told to leave it
    alone, so it will still be here on your next look. Every other open item is
    swept up by a dispatcher within seconds of being posted -- if you want one
    of those, claim it as soon as you see it."""
    conn = db.connect()
    try:
        return _dump({"work": db.list_work(conn, status=status, limit=limit)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def claim_work(thread_id: int, author: str | None = None) -> str:
    """Take a work item. Returns {"claimed": false} when another agent already
    has it -- that is NOT an error and needs no retry: check list_work for the
    next open item and move on.

    A successful claim leaves a one-line receipt on the item, once ever per
    agent, so the queue shows that somebody picked it up and not only that
    somebody holds it. A failed claim leaves nothing: a receipt saying you took
    an item you did not get would be worse than no receipt at all."""
    author = _who(author)
    conn = db.connect()
    try:
        claimed = db.claim_task(conn, thread_id, author)
        _deliver_acks(author)
        # Built here, returned after the finally: a return inside the try
        # would leave the receipt below unreachable, because the finally runs
        # on the way out of the function rather than before the next line.
        result = _dump({"ok": True, "claimed": claimed})
    except ValueError as exc:
        return _dump({"error": str(exc)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()
    if claimed:
        _receipt(thread_id, author,
                 CLAIM_RECEIPT_BODY.format(agent=author), "read-receipt")
    return result


@server.tool()
def complete_work(thread_id: int, note: str, author: str | None = None) -> str:
    """Finish a work item you hold, posting note as your report on the thread.
    Returns {"completed": false} when you do not hold the item (never claimed
    it, or another agent does) -- that is not an error, the item simply stays
    as it is and the note is not posted.

    The name you resolve to must be the one that claimed the item, which is
    why a crew role is stamped with AGENTDESK_AUTHOR rather than deriving its
    name from the session: a role rotates its session, and the item outlives
    it."""
    author = _who(author)
    conn = db.connect()
    try:
        completed = db.complete_task(conn, thread_id, author)
        if completed:
            db.reply(conn, thread_id, author, paths.AGENT_KIND, note)
        _deliver_acks(author)
        return _dump({"ok": True, "thread_id": thread_id, "completed": completed})
    except ValueError as exc:
        return _dump({"error": str(exc)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def request_merge(pr_url: str, thread_id: int | None = None,
                  note: str | None = None, author: str | None = None) -> str:
    """Put a pull request on John's merge list, so he can find it, click it,
    and merge it himself. Use this the moment you open a PR that needs him --
    do not just mention the link in a message, because a link in a message is
    something he has to remember. On this list it waits, and it clears itself
    once GitHub says the PR is merged.

    Pass `thread_id` to tie the PR to the conversation it came from: that is
    where the merge notice is posted when it lands, which is how you find out
    without asking. Anything you pass as `note` is posted on the thread along
    with the link.

    Registering the same URL twice is not an error and does not create a second
    row; you get the existing one back with created=false."""
    author = _who(author)
    try:
        repo, number, url = prs.parse_pr_url(pr_url)
    except ValueError as exc:
        return _dump({"error": str(exc)})
    conn = db.connect()
    try:
        # A title has to exist before the first check, because the tab shows
        # the list and `gh` is not asked what it is until the first pass. The
        # URL is the honest placeholder until then.
        title = (note or "").strip().splitlines()[0] if note else url
        pr_id, created = db.register_pr(conn, url, repo, number, title[:200],
                                        author, thread_id=thread_id)
        if thread_id is not None and created:
            body = f"Asking John to merge {repo}#{number}: {url}"
            if note:
                body += f"\n\n{note}"
            db.reply(conn, thread_id, author, paths.AGENT_KIND, body,
                     meta={"kind": "pr-request"})
        _deliver_acks(author)
        return _dump({"ok": True, "pr_id": pr_id, "url": url,
                      "repo": repo, "number": number, "created": created})
    except ValueError as exc:
        return _dump({"error": str(exc)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


def main() -> None:
    # Before the first tool call, not at import: on a machine that has never
    # run the tray app the database does not exist yet, and the server should
    # be the thing that creates it rather than the thing that crashes on it.
    conn = db.connect()
    try:
        db.init_db(conn)
    finally:
        conn.close()
    # Said out loud once per session, because the identity is derived from the
    # environment this process inherited and the log is the only place that
    # derivation can be checked from outside the session that made it. If a
    # future session posts under a name nobody expected, this line is why.
    try:
        notify.log_line(
            f"session identity: {identity.session_identity()} "
            f"({identity.describe(identity.session_identity())})")
    except Exception:
        pass  # never let a log line stop the board from starting
    server.run(transport="stdio")


if __name__ == "__main__":
    main()
