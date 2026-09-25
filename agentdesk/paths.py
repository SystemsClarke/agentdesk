"""Where AgentDesk keeps its things.

Code lives in the repo (NoOneDrive). Runtime state lives in LOCALAPPDATA, on
purpose: the database is a WAL-mode SQLite file, and those do not belong in a
synced folder - a sync client copying a WAL out from under a writer is how you
get a corrupt database.
"""

import os
from pathlib import Path

APP_NAME = "AgentDesk"

# --- code and runtime state, deliberately in different places -----------------
DATA_DIR = Path(os.environ.get("LOCALAPPDATA", Path.home())) / APP_NAME
DB_PATH = DATA_DIR / "agentdesk.db"
ARCHIVE_DIR = DATA_DIR / "archive"          # hourly raw snapshots

# --- the memory vault ---------------------------------------------------------
# Raw transcripts go in their own folder rather than into notes/, because the
# vault's notes are hand-written atomic memories with a search index over them.
# A daily traffic file is not one of those, and dropping hundreds of them into
# notes/ would swamp both the index and the reader.
#
# THIS PATH DOES NOT FOLLOW LOCALAPPDATA. DATA_DIR/DB_PATH/ARCHIVE_DIR above are
# all derived from the LOCALAPPDATA env var, so pointing that at a scratch
# directory isolates the database and the backup archive -- it does NOT isolate
# the vault, because this constant is a hardcoded absolute path. A script that
# calls backup.write_vault() / backup.run_once() (directly, or via the CLI:
# `python -m agentdesk.backup`) needs to either pass skip_vault=True /
# --skip-vault, or reassign VAULT_DIR and everything derived from it (see
# scripts/check_vault.py or check_archive.py for the pattern) BEFORE calling
# in. Skipping this step writes a real file into John's real vault repo --
# confirmed the hard way on 2026-09-19 (item #179): a same-second "quick sanity
# check" of run_once() with only LOCALAPPDATA overridden clobbered a day's
# entire real transcript with an empty one.
# Derived from Path.home() rather than a hardcoded account name, because the
# same "John, on this machine" turns out to mean a different Windows account
# on different boxes (palencharj vs. jpale) -- a literal path silently pointed
# at an account that does not exist on the second one. Path.home() is exactly
# the "this machine's own account" the comment above already wants; nothing
# about the isolation story changes; a test script still reassigns VAULT_DIR
# (and everything derived from it) before calling in, same as before.
VAULT_DIR = Path.home() / "NoOneDrive" / "MainClaudeMemory" / "MainClaude"
VAULT_AGENTDESK = VAULT_DIR / "agentdesk"    # agentdesk/YYYY-MM-DD.md
VAULT_LOG = VAULT_DIR / "log"                # log/YYYY-MM-DD.md gets a pointer line

# The mirror's two ends. notes/ is the vault's atomic-memory folder and maps/
# holds the MOCs a note must be registered in; both belong to the protocol in
# _meta/Memory Protocol.md, so nothing here may write to either without passing
# that protocol's checks first (see agentdesk/vault.py). PARKED is under
# agentdesk/ on purpose: it is this app's own folder, so a wiki post that cannot
# become a note is visible without putting a non-note into notes/.
VAULT_NOTES = VAULT_DIR / "notes"
VAULT_MAPS = VAULT_DIR / "maps"
VAULT_PARKED = VAULT_AGENTDESK / "parked"

# The search index's DATA half. This is derived from every note's text (an
# embedding is a lossy but real encoding of what it was built from), so it is
# as proprietary as the notes themselves and lives inside the vault repo for
# the same reason VAULT_PARKED does -- never in agentdesk's own repo, which is
# a different trust boundary. `_meta/` already holds non-note vault state
# (Memory Protocol.md, Librarian state.md), so this is one more thing there,
# not a new top-level folder to explain.
#
# The search SOFTWARE that reads and writes this folder is agentdesk/
# vault_search.py -- code, not data, so it lives in this repo like every other
# module. This split (code here, data there) is deliberate: see that module's
# docstring.
VAULT_SEARCH_DIR = VAULT_DIR / "_meta" / "search-index"

# A settled question's full transcript. A subfolder of the app's own folder,
# for the reason VAULT_AGENTDESK's comment already gives -- raw text does not go
# in notes/ -- plus one of its own: the daily traffic file is one file per day
# rewritten whole, and a question transcript is one file per question that is
# written once and never touched again. Same principle, opposite shapes, so they
# do not belong in one folder.
VAULT_QUESTIONS = VAULT_AGENTDESK / "questions"

# --- who is who ---------------------------------------------------------------
HUMAN = "john"
HUMAN_KIND = "human"
AGENT_KIND = "agent"

# Channels are the two pages the user asked for, plus a wiki, plus the work
# queue. 'work' is where a task is posted and an agent comes to find it: the
# thread IS the job, so claiming and finishing are thread-status moves rather
# than a second table.
CHANNELS = ("question", "discussion", "wiki", "work")

# A question is 'open' until the human answers it; that state is what drives the
# toast, so it is a column and not something inferred from the thread shape.
STATUS_OPEN = "open"
STATUS_ANSWERED = "answered"
STATUS_CLOSED = "closed"
STATUS_FYI = "fyi"

# The terminal state, and it means ONE thing: the thread's full text is in the
# vault. It is not "answered" said a second time -- a question that has been
# answered but not yet archived is still on the Questions tab, because the
# reasoning is not recorded anywhere durable yet, and the tab is exactly the
# list of things that are not finished with.
#
# Only a settled question (answered or closed) is archived, and only archiving
# sets this. The open_questions VIEW does not mention it and must not: the view
# asks "is John still being toasted about this", and a status that can only be
# reached after he answers can never change that answer.
STATUS_ARCHIVED = "archived"

# --- the acknowledgement watcher -----------------------------------------------
# When John replies to a question an agent asked, the agent acknowledges it -
# once. The watcher (the app's poll loop) queues the ack and, when the agent
# has gone quiet, posts a note saying nobody has picked the reply up. An agent
# counts as active for this long after its last write to the board; after that
# the watcher says so rather than waiting silently. 10 minutes is several poll
# ticks but short against the hours a session can sit idle at its prompt.
ACK_ACTIVE_SECONDS = 600

# The author the board itself posts under when it has something to say - the
# "nobody has picked this up" note is the board speaking, not an agent, so it
# must not wear an agent's name.
WATCHER = "agentdesk"

# The work queue's two extra states. A work thread is born 'open' -- available
# to be taken -- and moves to 'claimed' when an agent picks it up and 'done'
# when that agent reports back.
#
# 'open' is shared with questions, which is safe because the open_questions VIEW
# filters on channel='question' as well as on the status. It is worth stating
# plainly, because the obvious future change -- dropping the channel test from
# that view -- would turn every unclaimed task into a toast at John.
STATUS_CLAIMED = "claimed"
STATUS_DONE = "done"

# --- pull requests waiting on John's merge -------------------------------------
# A PR is not a thread and is deliberately not modelled as one. It has no
# back-and-forth: it has a URL, a GitHub state, and exactly one thing that
# happens to it (it gets merged or it does not). Modelling it as a thread in a
# fifth channel would have got the list and the detail pane for free, and would
# also have made `post_message(channel="prs")` a way for any agent to inject
# rows into what is supposed to be a merge queue.
#
# 'open' is a PR that GitHub still reports as open, which is also the state a PR
# sits in when the check could not run. That is the point: a failed check must
# leave the row looking exactly like an unchecked one, never like a merged one.
PR_OPEN = "open"
PR_MERGED = "merged"
PR_CLOSED = "closed"

# The two states a PR never leaves. Both mean "this is not waiting on John any
# more", which is what takes it off the tab -- see prs.check_one for why a
# closed-unmerged PR counts as settled rather than as still pending.
PR_TERMINAL = (PR_MERGED, PR_CLOSED)

# How often the background checker asks GitHub. Deliberately its own number and
# not the window's 3-second poll: each check is a `gh` subprocess, which costs
# about a second and a GitHub API call, and the answer changes on the order of
# minutes or hours. 60s is already 20x more often than the state can usefully
# move; a shorter interval buys nothing and spends rate limit.
PR_CHECK_SECONDS = 60

# The first check after startup, which is NOT PR_CHECK_SECONDS. John opens the
# window to find out what is waiting on him, and making him wait a minute to
# find out whether the PR he merged five minutes ago has cleared is the one
# moment the delay is most visible.
PR_FIRST_CHECK_SECONDS = 5

# How long `gh` gets before it is treated as a failed check. Generous, because
# a slow answer is still an answer and only a hung process needs killing -- and
# a false "check failed" leaves the row on the tab, which is a real cost.
PR_GH_TIMEOUT_SECONDS = 30

# The author pull-request notifications are posted under. The board speaking,
# not an agent: the same reason WATCHER exists. A merge notice wearing an
# agent's name would read as that agent having said something.
PR_NOTIFIER = WATCHER

# The two ways a row lands in pull_requests (item #115). An agent that calls
# request_merge is asking for something specific; a row the github-scan pass
# adds is just GitHub saying John is on it. Keeping them distinguishable is
# the point -- see agentdesk/pr_scan.py.
PR_SOURCE_AGENT = "agent-registered"
PR_SOURCE_SCAN = "github-scan"

# How often the notification/assignment scan runs. Shares the watcher thread
# rather than getting its own, but on a longer cadence than the merge check:
# a `gh api notifications` plus a couple of `gh search prs` calls costs more
# than one `gh pr view`, and what it finds changes on the order of hours, not
# minutes.
PR_SCAN_SECONDS = 600

# Ladder triage timeout. Short and separate from PR_GH_TIMEOUT_SECONDS: a
# stalled local model must cost one PR's label, never the gh state check that
# already ran for it, and never the pass.
PR_TRIAGE_TIMEOUT_SECONDS = 25


def ensure_dirs() -> None:
    for d in (DATA_DIR, ARCHIVE_DIR, VAULT_AGENTDESK, VAULT_LOG, VAULT_PARKED,
              VAULT_QUESTIONS):
        d.mkdir(parents=True, exist_ok=True)
