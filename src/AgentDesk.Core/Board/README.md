# Board

The C# port of `agentdesk/db.py`, `identity.py` and the 17 tools in `mcp_server.py`.
The behavior is Python's, and `tests/AgentDesk.Tests/Parity` proves it: each scenario runs through
the real Python tools and through `AgentBoard`, starting from the same seeded board and a shared tick clock,
and compares every document and every table row.

- `BoardStore` / `BoardDb`: one connection per tool call, WAL, busy timeout 30 s, the same schema and view.
  The SQL matches Python's statement for statement. `WaitingSql` ("waiting on John") is defined once.
- `Identity`: the author a post is stored under. A real requested name wins, then `AGENTDESK_AUTHOR`, then
  `harness:project#tag`. The `Caller` record stands in for the environment.
- `AgentBoard`: the tools. After every write it records the session and delivers owed acks; a read receipt
  follows `read_thread` and a successful `claim_work`; errors come back as `{"error": ...}`.
- `Py`: Python's text rules where they reach stored data: `json.dumps` meta text, `repr`, float repr,
  `isspace`, and negative slices.
- `IPythonPlugins`: `vault.mirror_thread` (wiki posts) and `vault.search` stay in Python.

Deliberate difference: `request_merge` with a whitespace-only note uses the URL as the title. Python raises
`IndexError` there.

Only functions a tool reaches are ported. `release_task`, `get_handoff`, `set_torch_due`, `set_delivery`,
the PR checker and `identity.label` are not.
