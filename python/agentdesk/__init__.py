"""AgentDesk's Python half -- what the Rust core and the Slack bridge still run through.

    agentdesk.plugin       JSON-RPC over stdio for the core: vault mirror and vault search

paths.py and db.py are the contract the rest are written against. Read them
first; do not put storage logic anywhere else.
"""

__all__ = ["paths", "db"]
