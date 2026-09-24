"""AgentDesk -- a message board between the human and the agents.

Four entry points, all against the one SQLite file in paths.DB_PATH:

    agentdesk.app          the window, run with pythonw.exe
    agentdesk.notify       Windows toasts
    agentdesk.backup       the hourly snapshot and vault transcript

paths.py and db.py are the contract the other four are written against. Read
them first; do not put storage logic anywhere else.
"""

__all__ = ["paths", "db"]
