"""AgentDesk's single entry point.

Everything that used to be launched as `python -m agentdesk.<module>` is a
subcommand here instead, so the packaged build has exactly one exe to sign,
allow-list, and register in Scheduled Tasks:

    AgentDesk.exe                 launch the GUI (equivalent to old `agentdesk.app`)
    AgentDesk.exe crew             coordinator + role dispatcher (`agentdesk.crew`)
    AgentDesk.exe backup           one-shot snapshot/vault mirror (`agentdesk.backup`)
    AgentDesk.exe notify TITLE BODY   Windows toast, diagnostic CLI
    AgentDesk.exe prs [...]        PR-tracking CLI, args forwarded
    AgentDesk.exe vault [...]      vault archive CLI, args forwarded
    AgentDesk.exe vault-search Q   search the memory vault (`agentdesk.vault_search`)

This module is a thin dispatcher: it does not reimplement any behavior, it
only imports the existing module's `main()`/`_main()` and calls it. Keeping
the modules themselves untouched means the old `python -m agentdesk.X`
invocations still work unchanged during the transition.
"""

from __future__ import annotations

import sys
from pathlib import Path

# Make `agentdesk` importable by absolute name regardless of how this file is
# invoked. Under `python -m agentdesk.cli`, `agentdesk` is already importable
# and this is a no-op. Compiled standalone (Nuitka) or run as a bare script
# (`python cli.py`), only this file's own directory lands on sys.path, and an
# absolute `from agentdesk import X` would otherwise fail with
# ModuleNotFoundError. Same pattern already used by crew.py and worker.py.
try:
    import agentdesk  # noqa: F401
except ImportError:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv

    if not argv:
        from agentdesk import app
        return app.main([])

    cmd, rest = argv[0], argv[1:]

    if cmd == "crew":
        from agentdesk import crew
        return crew.main(rest)

    if cmd == "backup":
        from agentdesk import backup
        sys.argv = ["agentdesk-backup", *rest]
        backup.main()
        return 0

    if cmd == "notify":
        from agentdesk import notify
        return notify._main(["agentdesk-notify", *rest])

    if cmd == "prs":
        from agentdesk import prs
        return prs.main(rest)

    if cmd == "vault":
        from agentdesk import vault
        # vault.main() reads sys.argv itself; splice our rest in.
        sys.argv = ["agentdesk-vault", *rest]
        vault.main()
        return 0

    if cmd == "vault-search":
        from agentdesk import vault_search
        return vault_search.main(rest)

    if cmd in ("-h", "--help", "help"):
        print(__doc__)
        return 0

    if cmd in ("app", "gui"):
        from agentdesk import app
        return app.main(rest)

    print(f"agentdesk: unknown subcommand {cmd!r}\n\n{__doc__}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
