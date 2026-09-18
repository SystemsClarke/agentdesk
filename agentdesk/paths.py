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
LOG_PATH = DATA_DIR / "agentdesk.log"

# The dispatcher's two handles, and they are deliberately different kinds of
# file. WORKER_STATE is a heartbeat the worker writes, so the window can tell
# whether a dispatcher is running even one it did not start itself. WORKER_STOP
# is how the window asks it to finish: the worker checks for it between items,
# so stopping leaves the item it is holding alone rather than killing it
# mid-edit. A flag file and not a signal, because a signal would arrive while
# the worker was inside a subprocess it does not control.
WORKER_STATE = DATA_DIR / "worker.state"
WORKER_STOP = DATA_DIR / "worker.stop"

# --- the memory vault ---------------------------------------------------------
# Raw transcripts go in their own folder rather than into notes/, because the
# vault's notes are hand-written atomic memories with a search index over them.
# A daily traffic file is not one of those, and dropping hundreds of them into
# notes/ would swamp both the index and the reader.
VAULT_DIR = Path(r"C:\Users\palencharj\NoOneDrive\MainClaudeMemory\MainClaude")
VAULT_AGENTDESK = VAULT_DIR / "agentdesk"    # agentdesk/YYYY-MM-DD.md
VAULT_LOG = VAULT_DIR / "log"                # log/YYYY-MM-DD.md gets a pointer line

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


def ensure_dirs() -> None:
    for d in (DATA_DIR, ARCHIVE_DIR, VAULT_AGENTDESK, VAULT_LOG):
        d.mkdir(parents=True, exist_ok=True)
