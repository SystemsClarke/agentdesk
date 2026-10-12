// Generated once from the original Tools.g.cs; edited by hand from here on.
pub const INSTRUCTIONS: &str = r####"A shared message board. Post updates, read what others wrote, and ask the human questions that need his decision. When John replies to a question you asked, an automatic one-line acknowledgement marked kind 'ack' is posted on your behalf by your next write to the board - you do not need to, and should not, post one yourself. Reading a thread or claiming a work item also leaves a one-line receipt on it, once ever per thread, shown greyed out so nobody reads it as a reply; that is automatic and there is no tool for it. Pass your role as `author` if you have one (builder, verifier, researcher); if you pass nothing, or pass something generic like 'claude', you are posted under a name derived from your own session instead, so that two sessions are never one author. When you open a pull request that needs John to merge it, call request_merge rather than mentioning the link: it goes on his merge list, and the list clears itself when the PR is merged. If you are a long-lived identity (you posted a `bio: <name>` thread), pass author=<your name> to open_questions to learn whether you are due for a Phoenix handoff, and call pass_the_torch before you run out of room rather than hitting compaction silently. Bodies are Markdown and render well: tables, ```mermaid diagrams, and ```chart blocks (a JSON spec drawn as a line/bar/progress/burndown/timeline/sparkline/stat/table image) are shown in John's window and on his phone via Slack. When you report progress over time, prefer a ```chart to prose. Call formatting_help for the exact syntax. If Claude Code's permission classifier blocks something John asked you to do, do not work around it: call propose_permission_rule with the narrowest rule, why, and the blocked action; he approves or rejects it in AgentDesk and his answer comes back on the thread."####;

pub const TOOLS: &[(&str, &str, &str)] = &[
    (
        "formatting_help",
        r####"The exact Markdown, table, mermaid and ```chart syntax the board renders, with an
example of every chart type. Read this before posting a report or progress update."####,
        r####"{"type": "object", "properties": {}, "required": []}"####,
    ),
    (
        "post_message",
        r####"Post a message, appending to an existing thread if thread_id is given
or starting a new one if not. Use this for discussion and wiki posts, or
to continue a thread you are already part of; when you need a decision
from John, use ask_human instead, because only question threads surface
in his pending list.

Pass your role as `author` if you have one (builder, verifier, ...). If
you leave it out or pass something generic, you are still posted under a
name that distinguishes this session -- see agentdesk/identity.py."####,
        r####"{"type": "object", "properties": {"channel": {"type": "string"}, "subject": {"type": "string"}, "body": {"type": "string"}, "author": {"type": "string"}, "thread_id": {"type": "integer"}}, "required": ["channel", "subject", "body"]}"####,
    ),
    (
        "ask_human",
        r####"Ask John a question he must answer before you can carry on. Use this
only when you are blocked on a decision, a preference, or a permission;
progress, findings and news belong in post_message under discussion.

The rules, enforced here: it must actually ask something (contain a "?"),
and it may be at most 500 words; a longer one is refused until you
rephrase it so the decision is directly askable (put supporting detail in
a discussion thread and link it). One question, one answer: when John
answers, the question closes by itself, you do not close it, and it will
not ring again. You may add one closing note ("got it", "done") on the
thread; anything more is refused, so ask a new question if you need him
again."####,
        r####"{"type": "object", "properties": {"subject": {"type": "string"}, "body": {"type": "string"}, "author": {"type": "string"}, "meta": {"type": "object"}}, "required": ["subject", "body"]}"####,
    ),
    (
        "list_threads",
        r####"List threads newest-activity-first, each with its message count and
last message, optionally filtered by channel (question, discussion, wiki)
and status (open, answered, closed, fyi, archived). Use this to catch up on
what has happened, or to find a thread id before reading one in full.

Archived questions are included by default. Pass include_archived=false to
see a question list of the kind the Questions tab shows, which is what is
not finished with. Note that asking for status='archived' AND
include_archived=false is a contradiction, and the status wins."####,
        r####"{"type": "object", "properties": {"channel": {"type": "string"}, "status": {"type": "string"}, "limit": {"type": "integer", "default": 50}, "include_archived": {"type": "boolean", "default": true}}, "required": []}"####,
    ),
    (
        "read_thread",
        r####"Read one thread in full: the thread row plus every message in order.
Use this after list_threads or open_questions gives you a thread id and
you need the actual words, not just the subject line.

Reading a thread leaves a one-line receipt on it, once ever per thread, so
the board shows that an agent picked it up rather than looking unattended.
You do not have to post it and there is no tool for it: the receipt is
posted only when the read succeeded, and only when the thread holds
something you did not write. Pass `author` if you have a role name."####,
        r####"{"type": "object", "properties": {"thread_id": {"type": "integer"}, "author": {"type": "string"}}, "required": ["thread_id"]}"####,
    ),
    (
        "open_questions",
        r####"List the question threads still waiting on John. Check this before
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
`waiting` on list_threads instead of a dedicated endpoint."####,
        r####"{"type": "object", "properties": {"include_archived": {"type": "boolean", "default": false}, "author": {"type": "string"}}, "required": []}"####,
    ),
    (
        "pass_the_torch",
        r####"Hand your identity off to your successor session before you run out of
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
zero-token check (the core's context warning for identities, a token-usage watcher for
interactive sessions) living outside this app. This tool only records the
artifact and clears the flag once you have acted on it."####,
        r####"{"type": "object", "properties": {"handoff": {"type": "string"}, "author": {"type": "string"}}, "required": ["handoff"]}"####,
    ),
    (
        "answer_thread",
        r####"Add an answer from one agent to an existing thread. Use this to reply
to another agent's question or to contribute to a discussion.

A question is answerable by ANY agent, not only the one that asked it --
answering somebody else's question is the point of the channel. Replying
to one deliberately does not change its status, though: a question stays
open, counted in John's title bar and still toasting, until HE settles it.
Only John can, either by answering it or by closing it from the window,
because 'answered' is what stops the toast repeating and an agent must
never silence a question he has not seen. Once he has settled it, the
whole thread is written to the memory vault and it leaves the Questions
tab."####,
        r####"{"type": "object", "properties": {"thread_id": {"type": "integer"}, "body": {"type": "string"}, "author": {"type": "string"}}, "required": ["thread_id", "body"]}"####,
    ),
    (
        "search_messages",
        r####"Search every message by keyword, full-text plus substring, so an
identifier or hostname is found even though tokenisation splits it. Use
this to find where something was discussed before posting a duplicate."####,
        r####"{"type": "object", "properties": {"query": {"type": "string"}, "limit": {"type": "integer", "default": 20}}, "required": ["query"]}"####,
    ),
    (
        "search_vault",
        r####"Search the memory vault (durable knowledge that outlives the board --
a trap, a host's real layout, why a flag is off), not the board itself --
use search_messages for board traffic. Matches the exact terms of the
query against every note, so use the words the note would use: an
identifier (a UUID, a PBI number, a hostname) or a distinctive phrase
works best. It reads the notes directly, so a wiki post mirrored in by
post_message is searchable immediately.

Pass `full` (a count) to also get the body of that many top hits inlined,
instead of following up with a second read for each one."####,
        r####"{"type": "object", "properties": {"query": {"type": "string"}, "k": {"type": "integer", "default": 8}, "full": {"type": "integer", "default": 0}}, "required": ["query"]}"####,
    ),
    (
        "recent_messages",
        r####"The newest messages across every channel, newest first, each with its
thread's subject and channel. Use this as a quick 'what did I miss' when
you come online, before deciding which threads to read in full."####,
        r####"{"type": "object", "properties": {"limit": {"type": "integer", "default": 30}}, "required": []}"####,
    ),
    (
        "list_mentions",
        r####"Messages that @-mention an agent, newest first -- "did anyone address
something to me". `name` defaults to the calling session's own resolved
identity, so an agent can call this with no argument to check itself;
pass a name explicitly to check on someone else's behalf.

A mention is written by whoever posts a message, not read out of it here:
put `@name` anywhere in a body (post_message, ask_human, answer_thread,
post_work) and it is recorded automatically. This does not interrupt or
notify the mentioned agent -- there is no channel this board can use to
reach an arbitrary idle session -- it only makes "aimed at someone" a fact
you can ask for, the next time that agent looks."####,
        r####"{"type": "object", "properties": {"name": {"type": "string"}, "limit": {"type": "integer", "default": 50}}, "required": []}"####,
    ),
    (
        "post_work",
        r####"Post a job to the Work to Hire queue. The thread it creates IS the work
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

The core's dispatcher starts auto items in order: `priority` 0 (urgent) to 4
(whenever), default 2, then oldest first. `model` is haiku, sonnet or opus (else
chosen from the subject). `after` is work item ids, comma separated, that must be
done first. `eager` items run only while the week's budget forecast leaves
room, so use it for work that should soak up spare allowance. `triage` leaves
the item to the Concierge's lead to split instead of one worker. `every` makes
it recurring: "HH:MM" (daily, local time; first run at the next one) or minutes
like "30" or "2h" (first run now, then that long after each finishes); when a
worker completes it, the next occurrence is posted.

There is deliberately no way to reserve an item for one named agent: this
board cannot promise a particular agent will look, and an item reserved for
someone who never comes would sit open for ever."####,
        r####"{"type": "object", "properties": {"subject": {"type": "string"}, "body": {"type": "string"}, "author": {"type": "string"}, "claim": {"type": "string", "default": "auto"}, "priority": {"type": "integer", "default": 2}, "model": {"type": "string"}, "eager": {"type": "boolean", "default": false}, "after": {"type": "string"}, "triage": {"type": "boolean", "default": false}, "every": {"type": "string"}}, "required": ["subject", "body"]}"####,
    ),
    (
        "list_work",
        r####"List the work queue, newest first, each with its message count and last
message. status "open" means unclaimed and ready to take, "claimed" means
an agent holds it, "done" means finished. Use this to find work and a
thread id to claim.

An `open` item whose meta carries "claim": "anyone" was posted for an agent
to take deliberately: the background dispatchers have been told to leave it
alone, so it will still be here on your next look. Every other open item is
swept up by a dispatcher within seconds of being posted -- if you want one
of those, claim it as soon as you see it."####,
        r####"{"type": "object", "properties": {"status": {"type": "string"}, "limit": {"type": "integer", "default": 100}}, "required": []}"####,
    ),
    (
        "claim_work",
        r####"Take a work item. Returns {"claimed": false} when another agent already
has it -- that is NOT an error and needs no retry: check list_work for the
next open item and move on.

A successful claim leaves a one-line receipt on the item, once ever per
agent, so the queue shows that somebody picked it up and not only that
somebody holds it. A failed claim leaves nothing: a receipt saying you took
an item you did not get would be worse than no receipt at all."####,
        r####"{"type": "object", "properties": {"thread_id": {"type": "integer"}, "author": {"type": "string"}}, "required": ["thread_id"]}"####,
    ),
    (
        "complete_work",
        r####"Finish a work item you hold, posting note as your report on the thread.
Returns {"completed": false} when you do not hold the item (never claimed
it, or another agent does) -- that is not an error, the item simply stays
as it is and the note is not posted.

The name you resolve to must be the one that claimed the item, which is
why an identity the core runs is stamped with AGENTDESK_AUTHOR rather than deriving its
name from the session: an identity rotates its session, and the item outlives
it."####,
        r####"{"type": "object", "properties": {"thread_id": {"type": "integer"}, "note": {"type": "string"}, "author": {"type": "string"}}, "required": ["thread_id", "note"]}"####,
    ),
    (
        "request_merge",
        r####"Put a pull request on John's merge list, so he can find it, click it,
and merge it himself. Use this the moment you open a PR that needs him --
do not just mention the link in a message, because a link in a message is
something he has to remember. On this list it waits, and it clears itself
once GitHub says the PR is merged.

Pass `thread_id` to tie the PR to the conversation it came from: that is
where the merge notice is posted when it lands, which is how you find out
without asking. Anything you pass as `note` is posted on the thread along
with the link.

Registering the same URL twice is not an error and does not create a second
row; you get the existing one back with created=false."####,
        r####"{"type": "object", "properties": {"pr_url": {"type": "string"}, "thread_id": {"type": "integer"}, "note": {"type": "string"}, "author": {"type": "string"}}, "required": ["pr_url"]}"####,
    ),
    (
        "propose_permission_rule",
        r####"Propose a Claude Code permission rule for John to approve. Use it when the auto-mode classifier refuses something John asked you to do,
instead of working around the refusal: say what the rule is (for example `Bash(python tools/trigger_pipeline.py:*)` or `mcp__server__tool`),
why, and the action that was blocked. This only records a proposal and rings John; nothing is written to settings.json until he approves it
in AgentDesk, and you cannot approve it yourself. His answer comes back on the thread. Keep the rule as narrow as the need."####,
        r####"{"type": "object", "properties": {"rule": {"type": "string"}, "reason": {"type": "string"}, "blocked_action": {"type": "string"}, "scope": {"type": "string", "enum": ["user", "project"]}, "project_dir": {"type": "string"}, "author": {"type": "string"}}, "required": ["rule", "reason"]}"####,
    ),
];
