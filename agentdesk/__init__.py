"""AgentDesk's Python half -- what the C# core and the crew still run through.

    agentdesk.plugin       JSON-RPC over stdio for the core: vault mirror and vault search
    agentdesk.crew         the coordinator and role dispatcher (sessions, roles, providers)
    agentdesk.backup       the hourly snapshot and vault transcript

paths.py and db.py are the contract the rest are written against. Read them
first; do not put storage logic anywhere else.
"""

__all__ = ["paths", "db"]
