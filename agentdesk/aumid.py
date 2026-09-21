"""AgentDesk's identity to the Windows shell, so a toast can be its own.

A WinRT toast is shown *as* an AppUserModelID: Windows resolves that id to a
name and an icon and attributes the notification to whatever it finds. A
desktop app that was never installed has no such registration, so there are
only two honest choices -- borrow somebody else's id, and let the toast read
"Windows PowerShell", or register one. This module registers one.

The registration is two halves that must agree, and both live here so they
cannot drift:

  * `AUMID`, the string, and
  * one `.lnk` in the user's Start Menu carrying that string as its
    `System.AppUserModel.ID` property.

The shortcut is not decoration. Without it the id is a string the shell has
never heard of, `CreateToastNotifier` still succeeds, and the toast is shown
under the borrowed id again -- silently. That is why `registered()` reads the
property back off the file instead of trusting that the write happened, and
why `ensure_shortcut()` returns a sentence rather than a bool: "wrote the file
but could not put the id on it" is the failure that otherwise looks like
success.

Like notify.py, nothing here raises. This is linked into the MCP server, which
speaks a protocol over stdout -- so no prints either, and every failure comes
back as a string the caller can log.
"""

from __future__ import annotations

import gc
import os
import sys
from pathlib import Path

from . import paths

# The id. Windows keys the notification settings, the shell's attribution and
# the toast's display name off this exact string, so it is the one thing that
# must not change casually: changing it orphans the old registration and the
# old registry entry.
AUMID = "Palencharj.AgentDesk"

# Where the shell looks for a desktop app's registration. Per-user, which is
# the only scope available without an installer and the right one anyway --
# the AUMID is a property of this user's session.
START_MENU_DIR = (
    Path(os.environ.get("APPDATA", Path.home()))
    / "Microsoft" / "Windows" / "Start Menu" / "Programs"
)
SHORTCUT_PATH = START_MENU_DIR / f"{paths.APP_NAME}.lnk"

# The repo root, so the shortcut can point at the app it is a shortcut to.
REPO = Path(__file__).resolve().parent.parent

# Under here Windows writes one subkey per AUMID the first time that id shows a
# toast. Our subkey appearing is proof Windows accepted the id -- and it is
# only half the question, because nothing in this key says what the toast
# looked like on screen.
NOTIFIER_SUBKEY = (
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Notifications\Settings"
)

# GPS_READWRITE, from the property-system flags. Named rather than passed as a
# bare 2 because the value is the whole difference between being able to set
# the id and silently not being able to.
_GPS_READWRITE = 2


def registry_key() -> str:
    """The registry path a toast under this AUMID should create, for showing."""
    return rf"HKCU\{NOTIFIER_SUBKEY}\{AUMID}"


def registry_seen() -> bool:
    """Has Windows recorded this AUMID as a notifier? Never raises.

    Windows writes the key when the id first shows a toast, so False means
    either "no toast yet" or "Windows refused the id", and True means the id
    was accepted. It says nothing at all about attribution on screen.
    """
    try:
        import winreg
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                            f"{NOTIFIER_SUBKEY}\\{AUMID}"):
            return True
    except OSError:
        return False
    except Exception:
        return False


def _default_python() -> str:
    """The interpreter the shortcut should launch: this app's own.

    Prefers the venv's pythonw.exe, because this is the one artifact here a
    person might actually double-click and a console window flashing up on
    every launch is a reason not to. Falls back to whatever is running.
    """
    for cand in (REPO / ".venv" / "Scripts" / "pythonw.exe",
                 REPO / ".venv" / "Scripts" / "python.exe"):
        if cand.exists():
            return str(cand)
    exe = Path(sys.executable)
    if exe.name.lower() == "python.exe":
        windowless = exe.with_name("pythonw.exe")
        if windowless.exists():
            return str(windowless)
    return str(exe)


def _write_shortcut() -> None:
    """Write the .lnk and stamp the AUMID onto it. Raises on failure.

    Two steps, and the order matters: the property store can only be opened
    over a file that already exists, so Save() first and SetValue() after.

    The store is released before returning. An open read-write IPropertyStore
    keeps the .lnk locked, and the next open of it -- our own read-back, or
    Windows' when it comes to attribute a toast -- fails with "used by another
    process". That looks like the write failing when it is the handle we kept.
    """
    import win32com.client
    from win32com.propsys import propsys, pscon

    shell = win32com.client.Dispatch("WScript.Shell")
    lnk = shell.CreateShortCut(str(SHORTCUT_PATH))
    lnk.TargetPath = _default_python()
    lnk.Arguments = "-m agentdesk.app"
    lnk.WorkingDirectory = str(REPO)
    lnk.Description = "AgentDesk - the message board between John and the agents"
    lnk.IconLocation = f"{lnk.TargetPath},0"
    lnk.Save()
    del lnk, shell
    gc.collect()

    store = propsys.SHGetPropertyStoreFromParsingName(
        str(SHORTCUT_PATH), None, _GPS_READWRITE, propsys.IID_IPropertyStore)
    try:
        store.SetValue(pscon.PKEY_AppUserModel_ID,
                       propsys.PROPVARIANTType(AUMID))
        store.Commit()
    finally:
        del store
        gc.collect()


def shortcut_aumid(path=None) -> str:
    """What AUMID, if any, is on a shortcut file. "" when unreadable. Never raises."""
    target = Path(path) if path else SHORTCUT_PATH
    try:
        from win32com.propsys import propsys, pscon
        store = propsys.SHGetPropertyStoreFromParsingName(str(target))
        try:
            value = store.GetValue(pscon.PKEY_AppUserModel_ID)
            return str(value.GetValue()) if value is not None else ""
        finally:
            del store
            gc.collect()
    except Exception:
        return ""


def registered() -> bool:
    """True when the Start Menu shortcut exists and carries our AUMID.

    This is the check notify.py makes before using the AUMID, so it is
    deliberately a read of the file rather than a belief about it.
    """
    if sys.platform != "win32":
        return False
    if not SHORTCUT_PATH.exists():
        return False
    return shortcut_aumid() == AUMID


def ensure_shortcut(force: bool = False) -> str:
    """Make the registration, once. Returns a sentence describing the outcome.

    Idempotent, and deliberately not destructive: a shortcut that already
    carries the right id is left exactly as it is, so running the app does not
    rewrite a Start Menu entry on every launch. `force` rewrites it, which is
    what to use after changing AUMID or the target interpreter.
    """
    if sys.platform != "win32":
        return f"not Windows: {AUMID} is not registered and cannot be"
    try:
        if not force and registered():
            return f"{AUMID} already registered at {SHORTCUT_PATH}"
        START_MENU_DIR.mkdir(parents=True, exist_ok=True)
        _write_shortcut()
        got = shortcut_aumid()
    except Exception as exc:
        return f"could not register {AUMID}: {exc!r}"
    if got != AUMID:
        # The specific failure worth saying out loud: a .lnk that exists, looks
        # right to the eye, and carries no id.
        return (f"wrote {SHORTCUT_PATH} but its AppUserModelID reads {got!r}, "
                f"not {AUMID!r}")
    return f"registered {AUMID} at {SHORTCUT_PATH}"


def _main() -> int:
    # Hand-testing only, and the one place a print is allowed here for the same
    # reason notify.py's __main__ is: nothing else imports this path.
    if len(sys.argv) > 1 and sys.argv[1] == "--force":
        print(ensure_shortcut(force=True))
    else:
        print(ensure_shortcut())
    print(f"registered()      -> {registered()}")
    print(f"shortcut AUMID    -> {shortcut_aumid()!r}")
    print(f"registry  {registry_key()}  -> "
          f"{'present' if registry_seen() else 'absent'}")
    return 0 if registered() else 1


if __name__ == "__main__":
    raise SystemExit(_main())
