"""Acceptance checks for the notifier sink registry (thread 82's one real
plugin point, built as work item 105).

The contract under test: a sink is declared, not scanned; a sink that fails to
import, that does not declare `SINK.send`, or that raises when sent to, is
disabled and reported -- never allowed to take the rest of notify.py down with
it. That is the "drop rather than die" rule the design (thread 82, section 4)
asks for, applied to the one module that already has plugin shape.

No real Windows toast is sent here: `toast()` is exercised with a fake `icon`
whose `.notify` succeeds, so the tray branch returns before ever reaching
PowerShell or the real notification surface -- this is a check of the sink
plumbing, not a repeat of check_aumid.py.

    .venv\\Scripts\\python.exe scripts\\check_sinks.py
"""

from __future__ import annotations

import os
import sys
import tempfile
import types
from pathlib import Path

_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-sinks-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

from agentdesk import notify  # noqa: E402

FAILURES: list[str] = []


def check(label: str, ok: bool, detail: str = "") -> None:
    if not ok:
        FAILURES.append(label + (f" ({detail})" if detail else ""))
    print(f"  [{'ok' if ok else 'FAIL'}] {label}" + (f" -- {detail}" if detail else ""))


class _FakeIcon:
    """Stands in for the tray icon so toast() never reaches a real surface."""
    def __init__(self):
        self.calls = []

    def notify(self, message=None, title=None):
        self.calls.append((title, message))


def _reset_registry(declared: tuple) -> None:
    notify._DECLARED_SINKS = declared
    notify._sinks_loaded = False
    notify._live_sinks = []
    notify._disabled_sinks = {}


def _install_fixture(name: str, module: types.ModuleType) -> None:
    sys.modules[name] = module


def main() -> int:
    icon = _FakeIcon()

    print("=== a well-formed sink is loaded and sent to ===")
    good = types.ModuleType("agentdesk_check_sink_good")
    sent = []

    class _GoodSink:
        def send(self, title, message):
            sent.append((title, message))
    good.SINK = _GoodSink()
    _install_fixture("agentdesk_check_sink_good", good)
    _reset_registry(("agentdesk_check_sink_good",))

    ok = notify.toast("hello", "world", icon=icon)
    check("toast() still returns True through the tray route", ok is True)
    check("the good sink received the same title/message",
          sent == [("hello", "world")], str(sent))
    check("no sink is reported disabled", notify.disabled_sinks() == {})

    print("\n=== a module that fails to import is disabled, not raised ===")
    _reset_registry(("agentdesk_check_sink_missing_entirely",))
    ok = notify.toast("t2", "m2", icon=icon)
    check("toast() still succeeds", ok is True)
    disabled = notify.disabled_sinks()
    check("the missing module is disabled",
          "agentdesk_check_sink_missing_entirely" in disabled, str(disabled))

    print("\n=== a module with no SINK.send is refused, not guessed at ===")
    bare = types.ModuleType("agentdesk_check_sink_bare")
    _install_fixture("agentdesk_check_sink_bare", bare)
    _reset_registry(("agentdesk_check_sink_bare",))
    ok = notify.toast("t3", "m3", icon=icon)
    check("toast() still succeeds", ok is True)
    disabled = notify.disabled_sinks()
    check("a sink with no SINK.send is disabled",
          "does not declare SINK.send" in disabled.get("agentdesk_check_sink_bare", ""),
          str(disabled))

    print("\n=== a sink that raises at send-time is disabled from then on ===")
    flaky = types.ModuleType("agentdesk_check_sink_flaky")

    class _FlakySink:
        def __init__(self):
            self.calls = 0
        def send(self, title, message):
            self.calls += 1
            raise RuntimeError("simulated failure")
    flaky_sink = _FlakySink()
    flaky.SINK = flaky_sink
    _install_fixture("agentdesk_check_sink_flaky", flaky)
    _reset_registry(("agentdesk_check_sink_flaky",))

    ok1 = notify.toast("t4a", "m4a", icon=icon)
    ok2 = notify.toast("t4b", "m4b", icon=icon)
    check("toast() succeeds both times despite the sink raising",
          ok1 is True and ok2 is True)
    check("the flaky sink was only actually called once, then dropped",
          flaky_sink.calls == 1, f"calls={flaky_sink.calls}")
    disabled = notify.disabled_sinks()
    check("the flaky sink is now listed as disabled",
          "raised at send" in disabled.get("agentdesk_check_sink_flaky", ""),
          str(disabled))

    print("\n=== declared-but-empty is the default, and it is silent ===")
    _reset_registry(())
    ok = notify.toast("t5", "m5", icon=icon)
    check("toast() succeeds with no sinks declared", ok is True)
    check("nothing is disabled when nothing is declared",
          notify.disabled_sinks() == {})

    for name in ("agentdesk_check_sink_good", "agentdesk_check_sink_bare",
                 "agentdesk_check_sink_flaky"):
        sys.modules.pop(name, None)

    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
