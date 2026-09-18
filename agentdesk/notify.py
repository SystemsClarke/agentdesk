"""Windows toasts, delivered best-effort.

Nothing in here may ever be the reason something else fails: a notification
that cannot be shown is a disappointment (the app shows the same information
in its window), so every failure path logs and returns rather than raising.

This module is also linked into the MCP server, which speaks over stdio. A
stray print to stdout or stderr would corrupt that protocol, so nothing here
prints - logging goes to paths.LOG_PATH only. (The __main__ block below is a
hand-testing CLI and is the one place a print is allowed.)
"""

import base64
import json
import subprocess
import sys
from pathlib import Path
from typing import Any, Optional

from . import db, paths

# Keeps the PowerShell child window from flashing when the parent is a console
# process. Defined here rather than imported from subprocess because that
# constant only exists on Windows, and this file should still import anywhere.
_CREATE_NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0

# Where notify_open_questions remembers the ids it last announced. Under
# DATA_DIR with the rest of the runtime state, not in the repo.
_STATE_PATH = paths.DATA_DIR / "notify_state.json"

# An unregistered app cannot show a WinRT toast: the notifier needs an
# AppUserModelID, and AgentDesk has none while it is launched without a Start
# Menu shortcut. Borrowing PowerShell's own AUMID is what makes the toast
# appear at all, and the cost is honest and accepted: the toast is attributed
# to "Windows PowerShell", not to AgentDesk. Saying so plainly beats faking it.
_POWERSHELL_AUMID = (
    r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}"
    r"\WindowsPowerShell\v1.0\powershell.exe"
)


def log_line(message: str) -> None:
    """Append a timestamped line to paths.LOG_PATH. Never raises."""
    try:
        paths.DATA_DIR.mkdir(parents=True, exist_ok=True)
        with paths.LOG_PATH.open("a", encoding="utf-8") as f:
            f.write(f"{db.now_iso()} {message}\n")
    except Exception:
        # A logging failure must never take the caller down with it; there is
        # nowhere left to report it, so swallow it.
        pass


def toast(title: str, message: str, *, launch: Optional[str] = None,
          icon: Any = None) -> bool:
    """Show a Windows toast. Best-effort: returns True if one was shown.

    `icon`, when given, is a pystray Icon. Its own .notify is tried first
    because when the app is running the tray icon IS the notification - no
    subprocess, no AppUserModelID question. The `launch` activation string is
    only honoured by the PowerShell route; pystray's notify has nowhere to
    carry it.

    Every exception is caught and logged. Nothing that calls this may crash
    because of it.
    """
    try:
        notify = getattr(icon, "notify", None)
        if callable(notify):
            try:
                notify(title=title, message=message)
                return True
            except Exception as exc:
                # The tray icon exists but refused; the PowerShell route is
                # still worth a try before giving up entirely.
                log_line(f"tray-icon notify failed, trying PowerShell: {exc!r}")
        return _toast_powershell(title, message, launch=launch)
    except Exception as exc:
        log_line(f"toast failed entirely: {exc!r}")
        return False


def notify_open_questions(conn, state_path=None) -> int:
    """Toast a summary of the open questions, but only when the set changed.

    An hourly job that nags about the same unanswered question trains the
    reader to ignore notifications, so the ids announced last time are kept in
    a small JSON file under paths.DATA_DIR and a toast is only sent when the
    set differs (a question answered or added, or an all-clear).

    Returns how many open questions it announced: 0 when it stayed quiet, and
    also 0 when the toast failed - in that case the state file is left alone
    so the next run retries rather than believing the news got through.
    """
    questions = db.open_questions(conn)
    ids = sorted(q["thread_id"] for q in questions)
    path = Path(state_path) if state_path else _STATE_PATH

    try:
        last = set(json.loads(path.read_text(encoding="utf-8"))["open_ids"])
    except Exception:
        # Missing or unreadable state: announce as if this were the first run.
        last = set()

    if set(ids) == last:
        return 0

    if not _announce(questions):
        return 0

    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({"open_ids": ids}), encoding="utf-8")
    except Exception as exc:
        # The toast went out; failing to remember it only risks one repeat.
        log_line(f"could not write toast state to {path}: {exc!r}")
    return len(ids)


def _announce(questions: list) -> bool:
    """Build the summary text and show it. One question is named; several are
    counted with the first few subjects, because a toast is a glance, not a list."""
    if not questions:
        title = f"{paths.APP_NAME}: all clear"
        message = "No open questions."
    else:
        subjects = [str(q["subject"]) for q in questions]
        if len(subjects) == 1:
            message = subjects[0]
        else:
            shown = "; ".join(s[:60] for s in subjects[:3])
            remaining = len(subjects) - 3
            if remaining > 0:
                shown += f" and {remaining} more"
            message = shown
        title = f"{paths.APP_NAME}: {len(subjects)} open question" \
                + ("s" if len(subjects) != 1 else "")
    return toast(title, message)


def _toast_powershell(title: str, message: str,
                      launch: Optional[str] = None) -> bool:
    """Route 2: a WinRT toast via PowerShell. Returns True if it ran clean.

    The script is passed with -EncodedCommand (base64 of UTF-16LE) rather than
    -Command so that no shell quoting layer can mangle the text. There is
    deliberately NO -ExecutionPolicy flag: the estate denies that flag, and
    the toast is not worth fighting the estate over - EncodedCommand is not
    blocked by policy anyway.

    Honest cost of this route: the toast is attributed to Windows PowerShell,
    not to AgentDesk, because a WinRT toast carries an AppUserModelID and a
    self-registered app with no Start Menu shortcut does not have one.
    """
    script = _ps_script(title, message, launch)
    encoded = base64.b64encode(script.encode("utf-16-le")).decode("ascii")
    result = subprocess.run(
        ["powershell.exe", "-NoProfile", "-NonInteractive",
         "-EncodedCommand", encoded],
        capture_output=True,  # captured, never inherited: no stray prints
        timeout=15,
        creationflags=_CREATE_NO_WINDOW,
    )
    if result.returncode != 0:
        log_line(
            f"PowerShell toast failed (rc={result.returncode}): "
            f"{result.stderr.decode('utf-8', 'replace').strip()[:500]}"
        )
        return False
    return True


def _ps_script(title: str, message: str, launch: Optional[str]) -> str:
    """Assemble the PowerShell for one toast.

    The text goes in through the DOM, NOT by substituting placeholders into the
    template XML. GetTemplateContent hands back a document whose text nodes are
    already EMPTY --

        <text id="1"></text><text id="2"></text>

    -- not the literal strings 'Text1'/'Text2' that the old XML-documentation
    examples suggest. A textual .Replace('Text1', ...) against that document
    matches nothing, so it returned success while showing a blank toast, and
    the caller then recorded those questions as announced and never raised them
    again. CreateTextNode also means the serializer does the XML escaping, so
    an ampersand in a build name is safe without hand-escaping it first --
    escaping it here as well would put a literal '&amp;' on screen.

    The single quotes still have to be doubled for PowerShell's own
    single-quoted literals; that is a different layer from the XML.
    """
    ps_title = title.replace("'", "''")
    ps_message = message.replace("'", "''")
    lines = [
        "$ErrorActionPreference = 'Stop'",
        "[void][Windows.UI.Notifications.ToastNotificationManager,"
        " Windows.UI.Notifications, ContentType=WindowsRuntime]",
        "$template = [Windows.UI.Notifications.ToastNotificationManager]"
        "::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]"
        "::ToastText02)",
        "$texts = $template.GetElementsByTagName('text')",
        "$null = $texts.Item(0).AppendChild("
        "$template.CreateTextNode('" + ps_title + "'))",
        "$null = $texts.Item(1).AppendChild("
        "$template.CreateTextNode('" + ps_message + "'))",
    ]
    if launch:
        # PS-literal quoting only; the DOM escapes XML when serializing.
        ps_launch = launch.replace("'", "''")
        lines.append(
            "$template.DocumentElement.SetAttribute('launch', '"
            + ps_launch + "')"
        )
    lines += [
        "$toast = [Windows.UI.Notifications.ToastNotification]::new($template)",
        "[Windows.UI.Notifications.ToastNotificationManager]"
        "::CreateToastNotifier('" + _POWERSHELL_AUMID + "').Show($toast)",
    ]
    return "\n".join(lines)


def _main(argv: list) -> int:
    title = argv[1] if len(argv) > 1 else paths.APP_NAME
    message = argv[2] if len(argv) > 2 else ""
    shown = toast(title, message)
    print(f"toast shown: {shown}")
    return 0 if shown else 1


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv))
