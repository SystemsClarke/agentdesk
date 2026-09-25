"""AgentDesk's Python half -- what the C# core and the Slack bridge still run through.

    agentdesk.plugin       JSON-RPC over stdio for the core: vault mirror and vault search
    agentdesk.backup       the hourly snapshot and vault transcript

paths.py and db.py are the contract the rest are written against. Read them
first; do not put storage logic anywhere else.
"""

__all__ = ["paths", "db"]
