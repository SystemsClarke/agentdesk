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
import importlib
import json
import subprocess
import sys
from pathlib import Path
from typing import Any, Optional

from . import aumid, db, paths

# Keeps the PowerShell child window from flashing when the parent is a console
# process. Defined here rather than imported from subprocess because that
# constant only exists on Windows, and this file should still import anywhere.
_CREATE_NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0

# Where notify_open_questions remembers the ids it last announced. Under
# DATA_DIR with the rest of the runtime state, not in the repo.
_STATE_PATH = paths.DATA_DIR / "notify_state.json"

# The AppUserModelID a toast is shown under. agentdesk/aumid.py registers one
# for this app, and when that registration is present the toast is shown as
# AgentDesk; see _powershell_attempts for the order and the fallback.
#
# This one is PowerShell's own, and it is the fallback rather than the design.
# It is what an unregistered app has to borrow to get a toast at all, and the
# cost is honest and accepted: a toast shown under it is attributed to
# "Windows PowerShell", not to AgentDesk. A machine where the registration
# never happened -- a fresh clone, a different user -- still gets its
# notifications, which is the point of keeping it.
_POWERSHELL_AUMID = (
    r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}"
    r"\WindowsPowerShell\v1.0\powershell.exe"
)

# How much text the TRAY route can carry, in UTF-16 code units.
#
# pystray's Windows backend maps notify(message=..., title=...) straight onto
# NOTIFYICONDATAW.szInfo (WCHAR[256]) and .szInfoTitle (WCHAR[64]), and ctypes
# RAISES ValueError rather than truncating when a string does not fit. Measured
# against a real icon on this machine: a 64-unit title is accepted, a 65-unit
# one raises "string too long (65, maximum length 64)".
#
# The unit is a UTF-16 code unit and not a character, which is the part that
# makes a naive [:64] slice wrong: one emoji counts as two, measured -- a title
# of 64 characters containing a single emoji raises. So the clip below counts
# code units, or a subject with one emoji in it slips through the fix and fails
# exactly as before.
#
# These are the tray's limits only. The PowerShell route has no such ceiling and
# is deliberately left unclipped: see toast().
_TRAY_TITLE_UNITS = 64
_TRAY_MESSAGE_UNITS = 256


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


def utf16_units(text: str) -> int:
    """How many UTF-16 code units `text` occupies. The unit the tray counts in."""
    return len(text.encode("utf-16-le")) // 2


def clip_utf16(text: str, limit: int) -> str:
    """Trim `text` to `limit` UTF-16 code units, marking the cut with an ellipsis.

    Counted in code units rather than characters because that is what the
    Windows field counts and what ctypes refuses on -- see _TRAY_TITLE_UNITS.
    The last unit is spent on the ellipsis so a shortened string reads as
    shortened; a subject silently cut in half reads as the whole subject, which
    is the failure mode being fixed here rather than a lesser version of it.
    """
    if utf16_units(text) <= limit:
        return text
    keep, used, out = limit - 1, 0, []
    for ch in text:
        width = utf16_units(ch)
        if used + width > keep:
            break
        out.append(ch)
        used += width
    return "".join(out) + "…"


# --- sinks: the one real plugin point -----------------------------------------
#
# notify.py already has the shape of a plugin host -- a platform impl, a
# fallback chain, a contract that it never raises -- so it is the one module
# worth giving a real extension point (see README.md's "Architecture" section
# and thread 82 for why nothing else at this size needs one).
#
# Declared, not scanned. A directory scan turns a file's mere presence into
# running code, and this repo lives under NoOneDrive, a synced folder -- a
# half-written file from a sync client must never become a sink.
#
# "agentdesk.teams_sink" (work item #129) is the first real sink this seam has
# grown -- exactly the module thread 82's design named as "the piece most
# likely to be replaced by something else (Teams, a webhook...)". If its
# webhook file is missing, `_load_sinks` disables it and logs why, same as
# any other sink that cannot start; nothing here needs it to be present.
_DECLARED_SINKS: tuple = ("agentdesk.teams_sink",)

_sinks_loaded = False
_live_sinks: list = []
_disabled_sinks: dict = {}  # dotted module name -> why it is not live


def _load_sinks() -> None:
    """Import every declared sink once. A sink that cannot be loaded is
    disabled and reported, never raised -- the same contract toast() itself
    keeps, because a broken sink must degrade the board, not crash it."""
    global _sinks_loaded
    if _sinks_loaded:
        return
    _sinks_loaded = True
    for name in _DECLARED_SINKS:
        try:
            module = importlib.import_module(name)
        except Exception as exc:
            _disabled_sinks[name] = f"import failed: {exc!r}"
            log_line(f"sink {name} disabled: import failed: {exc!r}")
            continue
        sink = getattr(module, "SINK", None)
        if sink is None or not callable(getattr(sink, "send", None)):
            # Refuse the undeclared rather than guess: a module that merely
            # sits next to a real sink must never be mistaken for one.
            _disabled_sinks[name] = "does not declare SINK.send"
            log_line(f"sink {name} disabled: does not declare SINK.send")
            continue
        _live_sinks.append((name, sink))


def disabled_sinks() -> dict:
    """name -> reason, for whoever wants the failure visible (the window's
    status strip, a tray tooltip) and not only in the log."""
    _load_sinks()
    return dict(_disabled_sinks)


def _notify_sinks(title: str, message: str) -> None:
    """Best-effort fan-out to every live sink. A sink that raises AFTER it
    loaded, not only at load time, is disabled from here on -- one bad send
    must not repeat forever, and must not take the caller down with it."""
    _load_sinks()
    for entry in list(_live_sinks):
        name, sink = entry
        try:
            sink.send(title, message)
        except Exception as exc:
            log_line(f"sink {name} raised, disabling: {exc!r}")
            _disabled_sinks[name] = f"raised at send: {exc!r}"
            try:
                _live_sinks.remove(entry)
            except ValueError:
                pass


def toast(title: str, message: str, *, launch: Optional[str] = None,
          icon: Any = None) -> bool:
    """Show a Windows toast. Best-effort: returns True if one was shown.

    `icon`, when given, is a pystray Icon. Its own .notify is tried first
    because when the app is running the tray icon IS the notification - no
    subprocess, no AppUserModelID question. The `launch` activation string is
    only honoured by the PowerShell route; pystray's notify has nowhere to
    carry it.

    Text that does not fit the tray's fields is CLIPPED FOR THAT CALL ONLY, to
    the code-unit widths in _TRAY_TITLE_UNITS, and the clip is logged. The
    PowerShell route is given the original text, because it has room for it and
    clipping there would throw away something the reader could have had. Every
    caller that passes a long title is otherwise choosing between a silent
    fallback to a subprocess and an exception; this module's whole contract is
    that it is never the reason something else fails, so it does neither.

    When the tray route is unavailable or refused, the toast goes out through
    the WinRT route as AgentDesk when the AUMID registration is present and as
    Windows PowerShell when it is not -- see _powershell_attempts. Use
    diagnostic_toast instead when you need to know which one happened.

    Every exception is caught and logged. Nothing that calls this may crash
    because of it.
    """
    # Fan out to any declared sink first. _notify_sinks never raises -- see
    # its own docstring -- so this cannot affect the tray/PowerShell delivery
    # below or the bool this function returns.
    _notify_sinks(title, message)
    try:
        notify = getattr(icon, "notify", None)
        if callable(notify):
            try:
                # pystray's signature is notify(message, title=None). Both of
                # those ARE the keyword names, so keyword order does not matter
                # and this line is equivalent to notify(title=..., message=...).
                # It was changed to match the declared order and for no other
                # reason -- an earlier comment here claimed the old keyword
                # order raised TypeError on every call and forced the tray route
                # to fall through to PowerShell. That was wrong: the old call
                # bound fine against pystray 0.19.5 and never raised.
                #
                # So the reason toasts are unreliable is still UNKNOWN. Do not
                # read this call as a fix. True from here means "pystray
                # accepted the call", not "a toast appeared" -- it returns
                # nothing and cannot report whether Windows actually showed it.
                #
                # Clipped here and not at the top of the function, so the
                # PowerShell route below still gets the full text. Logged when
                # it happens: this whole item exists because a degradation
                # nobody could see looked like a working toast.
                tray_title = clip_utf16(title, _TRAY_TITLE_UNITS)
                tray_message = clip_utf16(message, _TRAY_MESSAGE_UNITS)
                if tray_title != title or tray_message != message:
                    log_line(
                        "toast clipped for the tray route: "
                        f"title {utf16_units(title)}->"
                        f"{utf16_units(tray_title)} units, message "
                        f"{utf16_units(message)}->{utf16_units(tray_message)}"
                        " units (the PowerShell route, if it is reached, gets "
                        "the full text)")
                notify(message=tray_message, title=tray_title)
                return True
            except Exception as exc:
                # The tray icon exists but refused; the PowerShell route is
                # still worth a try before giving up entirely.
                log_line(f"tray-icon notify failed, trying PowerShell: {exc!r}")
        return _toast_powershell(title, message, launch=launch)[0]
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


def _powershell_attempts() -> list:
    """The AppUserModelIDs to try, in order, as (id, what it is called).

    AgentDesk's own first, and only when aumid.registered() says the Start Menu
    shortcut carrying it is really there -- trying an id the shell does not
    know is not harmless, because CreateToastNotifier accepts it and the toast
    comes out under the borrowed id instead, which is the failure this whole
    change exists to remove and one that leaves no error behind.

    PowerShell's own is last and unconditional, so a machine where the
    registration never happened still gets a notification.
    """
    attempts = []
    if aumid.registered():
        attempts.append((aumid.AUMID, paths.APP_NAME))
    attempts.append((_POWERSHELL_AUMID, "Windows PowerShell"))
    return attempts


def _toast_powershell(title: str, message: str,
                      launch: Optional[str] = None) -> tuple:
    """Route 2: a WinRT toast via PowerShell. Returns (shown, shown_as).

    `shown_as` names the identity the toast actually went out under, so a
    caller can tell "a toast appeared" from "a toast appeared as AgentDesk" --
    which are different answers and only the second one is the goal.
    """
    for notifier_id, shown_as in _powershell_attempts():
        if _run_ps(title, message, launch, notifier_id):
            if shown_as != paths.APP_NAME:
                log_line(
                    f"toast shown as {shown_as}, not {paths.APP_NAME}: the "
                    f"{aumid.AUMID} registration is missing or was refused "
                    f"(see agentdesk.aumid, {aumid.SHORTCUT_PATH})")
            return True, shown_as
    return False, ""


def _run_ps(title: str, message: str, launch: Optional[str],
            notifier_id: str) -> bool:
    """Run the toast script once under one AppUserModelID. True if it ran clean.

    The script is passed with -EncodedCommand (base64 of UTF-16LE) rather than
    -Command so that no shell quoting layer can mangle the text. There is
    deliberately NO -ExecutionPolicy flag: the estate denies that flag, and
    the toast is not worth fighting the estate over - EncodedCommand is not
    blocked by policy anyway.
    """
    script = _ps_script(title, message, launch, notifier_id)
    encoded = base64.b64encode(script.encode("utf-16-le")).decode("ascii")
    si = None
    if sys.platform == "win32":
        si = subprocess.STARTUPINFO()
        si.dwFlags |= subprocess.STARTF_USESHOWWINDOW
        si.wShowWindow = subprocess.SW_HIDE
    result = subprocess.run(
        ["powershell.exe", "-NoProfile", "-NonInteractive",
         "-EncodedCommand", encoded],
        capture_output=True,  # captured, never inherited: no stray prints
        timeout=15,
        creationflags=_CREATE_NO_WINDOW,
        startupinfo=si,
    )
    if result.returncode != 0:
        log_line(
            f"PowerShell toast failed under {notifier_id} "
            f"(rc={result.returncode}): "
            f"{result.stderr.decode('utf-8', 'replace').strip()[:500]}"
        )
        return False
    return True


def diagnostic_toast(title: str, message: str) -> dict:
    """One toast through the non-tray route, reporting which identity it used.

    Deliberately skips the tray icon. When the window is running pystray shows
    the notification itself and it is attributed correctly, so firing that
    would answer none of the question: this is the route where the
    AppUserModelID decides the attribution, and so the only route worth
    testing.

    Returns what happened rather than a bool, because "a toast appeared" and
    "a toast appeared as AgentDesk" are different answers and the caller here
    is a human looking at the screen, not a log.
    """
    registration = aumid.ensure_shortcut()
    shown, shown_as = _toast_powershell(title, message)
    return {
        "shown": shown,
        "shown_as": shown_as,
        "aumid": aumid.AUMID,
        "registration": registration,
        "registered": aumid.registered(),
        "registry_key": aumid.registry_key(),
        "registry_seen": aumid.registry_seen(),
    }


def _ps_script(title: str, message: str, launch: Optional[str],
               notifier_id: str = _POWERSHELL_AUMID) -> str:
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
        "::CreateToastNotifier('" + notifier_id.replace("'", "''")
        + "').Show($toast)",
    ]
    return "\n".join(lines)


def _main(argv: list) -> int:
    title = argv[1] if len(argv) > 1 else paths.APP_NAME
    message = argv[2] if len(argv) > 2 else ""
    # diagnostic_toast, not toast: a hand-test that cannot say which identity
    # the toast used is a hand-test that cannot fail.
    report = diagnostic_toast(title, message)
    for key, value in report.items():
        print(f"{key:14} {value}")
    print("\nAttribution on screen cannot be read from here. What this proves "
          "is that\nWindows accepted the id; whether the banner named "
          "AgentDesk is yours to say.")
    return 0 if report["shown"] else 1


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv))
