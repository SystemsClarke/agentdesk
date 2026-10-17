"""Where AgentDesk keeps its things.

Code lives in the repo (NoOneDrive). Runtime state lives in LOCALAPPDATA, on
purpose: the database is a WAL-mode SQLite file, and those do not belong in a
synced folder - a sync client copying a WAL out from under a writer is how you
get a corrupt database.
"""

import os
import sys
from pathlib import Path

APP_NAME = "AgentDesk"

# --- code and runtime state, deliberately in different places -----------------
def _data_dir() -> Path:
    """The board's folder, as the Rust build computes it: AGENTDESK_DATA, else the per-user local data folder."""
    if os.environ.get("AGENTDESK_DATA"):
        return Path(os.environ["AGENTDESK_DATA"])
    if os.name == "nt":
        return Path(os.environ.get("LOCALAPPDATA", Path.home())) / APP_NAME
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / APP_NAME
    return Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share")) / APP_NAME


DATA_DIR = _data_dir()
DB_PATH = DATA_DIR / "agentdesk.db"

# --- the memory vault ---------------------------------------------------------
# Raw transcripts and parked posts live under the vault's agentdesk/ folder, never in notes/:
# notes/ holds hand-written atomic memories with a search index over them.
#
# A script that writes the vault must point AGENTDESK_VAULT at a scratch folder first, or it
# writes the real one.
VAULT_DIR = Path(os.environ["AGENTDESK_VAULT"]) if os.environ.get("AGENTDESK_VAULT") else DATA_DIR / "vault"
VAULT_AGENTDESK = VAULT_DIR / "agentdesk"
VAULT_NOTES = VAULT_DIR / "notes"      # the protocol's atomic notes (see agentdesk/vault.py)
VAULT_MAPS = VAULT_DIR / "maps"        # the MOCs a note is registered in
VAULT_PARKED = VAULT_AGENTDESK / "parked"  # a wiki post that cannot become a note, with the reasons
VAULT_SEARCH_DIR = VAULT_DIR / "_meta" / "search-index"  # derived from every note: as private as the notes

# --- who is who, and the two thread states the Slack bridge sets ----------------
HUMAN = "john"
HUMAN_KIND = "human"
STATUS_OPEN = "open"
STATUS_ANSWERED = "answered"


def ensure_dirs() -> None:
    for d in (DATA_DIR, VAULT_PARKED):
        d.mkdir(parents=True, exist_ok=True)
