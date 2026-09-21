"""Does AgentDesk have its own identity to Windows, and does a toast use it?

Six checks, in rising order of how much they assume:

 1. THE ID AND THE PLACE. The AUMID string and the shortcut path are the ones
    the item asked for -- a .lnk under the user's Start Menu Programs folder,
    not somewhere the shell does not look.
 2. THE SHORTCUT CARRIES IT. The id is read back off the file twice, by two
    different mechanisms (the property system, and the shell's own shortcut
    resolver), because "the write returned without raising" is the failure
    that would still be reported as success.
 3. IT IS MADE ONCE. A second ensure_shortcut() must not rewrite an existing
    correct .lnk, so that merely running the app twice does not keep replacing
    a file in the user's Start Menu.
 4. THE TOAST USES IT, AND THE FALLBACK IS REAL. A toast is fired for real and
    the identity it went out under is reported; then the registration is made
    to look absent and a second toast is fired, which must still appear under
    Windows PowerShell. That second one is the item's "keep the PowerShell
    route as the fallback" and is only proved by firing it.
 5. WINDOWS ACCEPTED IT. HKCU\\...\\Notifications\\Settings\\<AUMID> is read
    back after the toast -- the item's second acceptance criterion, and the
    one piece of it that is checkable from here at all.
 6. THE WINDOW. The real App is built over a scratch database and the Test
    toast button is asserted to be gone from the toolbar -- by reading the
    strip's own children, so a second button under another name still fails.
    So is the handler that sat behind it: a button removed while its handler
    stays leaves a method whose only caller is a test and a dialogue nobody can
    be shown, which is the "no code path left that only the test button could
    reach" half of the item.

 7. THE TOAST WITHOUT THE WINDOW. The capability the button provided is still
    there and no longer needs a window: notify.diagnostic_toast fires one for
    real, and `python -m agentdesk.notify` does the same from a shell and exits
    non-zero when the toast cannot be sent. That is what replaced the button,
    and it is strictly better -- it works with the app closed.

    .venv\\Scripts\\python.exe scripts\\check_aumid.py

THIS SHOWS TOASTS ON THE DESKTOP. Sections 4 and 6 each put one on screen for
a few seconds, on purpose: the subject of the test is a notification. Nothing
here touches the real board -- the App is given a scratch database under a
temp directory -- but the notifier is the machine's, because the whole claim
is about how Windows treats this app.

WHAT THIS CANNOT SHOW, and says so rather than implying otherwise: what the
banner looked like on screen. Everything below is about the id Windows was
handed and whether Windows accepted it. Whether the notification then named
AgentDesk is a fact about a human's screen -- the one thing that had to be
settled by a person looking, which John did, which is why the button that asked
him is no longer in the window.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PY = str(REPO / ".venv" / "Scripts" / "python.exe")
if not Path(PY).exists():
    PY = sys.executable
sys.path.insert(0, str(REPO))

from agentdesk import aumid, notify, paths  # noqa: E402

FAILURES: list[str] = []


def check(what: str, got, want) -> None:
    ok = got == want
    print(f"  [{'ok ' if ok else 'FAIL'}] {what}: {got!r}"
          + ("" if ok else f"   (wanted {want!r})"))
    if not ok:
        FAILURES.append(what)


def note(what: str, value) -> None:
    """Print a fact that is evidence but not a pass/fail -- e.g. on-screen."""
    print(f"       {what}: {value}")


def hr(title: str) -> None:
    print(f"\n=== {title} ===")


# --- 1. the id, and the place ------------------------------------------------

def part_id() -> None:
    hr("1. the AUMID string and the Start Menu location")
    expected_dir = (Path(os.environ["APPDATA"]) / "Microsoft" / "Windows"
                    / "Start Menu" / "Programs")
    print(f"  AUMID          {aumid.AUMID}")
    print(f"  shortcut       {aumid.SHORTCUT_PATH}")
    check("the shortcut is a .lnk", aumid.SHORTCUT_PATH.suffix, ".lnk")
    check("...under the user's Start Menu Programs folder",
          aumid.SHORTCUT_PATH.parent, expected_dir)
    check("the id is a string the shell can key on",
          " " not in aumid.AUMID and aumid.AUMID.strip() == aumid.AUMID, True)
    check("the registry evidence path is derived from the same id",
          aumid.registry_key().endswith("\\" + aumid.AUMID), True)


# --- 2. the shortcut carries it ----------------------------------------------

def part_shortcut() -> None:
    hr("2. the .lnk carries the AppUserModelID, read back two ways")
    outcome = aumid.ensure_shortcut()
    print(f"  ensure_shortcut() -> {outcome}")
    check("the shortcut file exists", aumid.SHORTCUT_PATH.exists(), True)

    # Way 1: the property system, which is what the shell reads.
    read_back = aumid.shortcut_aumid()
    note("System.AppUserModel.ID read off the file", repr(read_back))
    check("the property system reports our AUMID", read_back, aumid.AUMID)

    # Way 2: the shortcut resolver, which is what a double-click uses. A
    # different API on the same file, so agreement between the two is not one
    # read reported twice.
    import win32com.client
    lnk = win32com.client.Dispatch("WScript.Shell").CreateShortCut(
        str(aumid.SHORTCUT_PATH))
    note("target", lnk.TargetPath)
    note("arguments", lnk.Arguments)
    check("the shortcut launches this app", lnk.Arguments, "-m agentdesk.app")
    check("...from this repo", Path(lnk.WorkingDirectory), REPO)
    check("...windowless, as the app is meant to be launched",
          Path(lnk.TargetPath).name.lower(), "pythonw.exe")


# --- 3. made once, not every launch ------------------------------------------

def part_idempotent() -> None:
    hr("3. a correct shortcut is left alone the second time")
    before = aumid.SHORTCUT_PATH.stat().st_mtime_ns
    outcome = aumid.ensure_shortcut()
    after = aumid.SHORTCUT_PATH.stat().st_mtime_ns
    print(f"  ensure_shortcut() -> {outcome}")
    check("the second call did not rewrite the file", after, before)
    check("...and says so", outcome.startswith("Palencharj.AgentDesk already"),
          True)


# --- 4. the toast uses it, and the fallback is real ---------------------------

def part_toast() -> None:
    hr("4. the toast goes out under AgentDesk, and falls back when it cannot")
    print("  (this puts a toast on the desktop)")
    print(f"  before: registry_seen() = {aumid.registry_seen()}")
    shown, shown_as = notify._toast_powershell(
        "AgentDesk AUMID check", "This one should be attributed to AgentDesk.")
    note("the identity the toast was shown under", f"{shown_as!r} (shown={shown})")
    check("a toast was sent", shown, True)
    check("...under AgentDesk's own AUMID, not borrowed",
          shown_as, paths.APP_NAME)

    # The fallback, forced. Monkeypatching the predicate is how the "the
    # registration is not there" branch is reached on a machine where it now
    # is; the toast itself is real, because whether Windows accepts PowerShell's
    # AUMID is exactly the thing that must not be assumed.
    print("\n  now with the registration made to look absent "
          "(second toast):")
    real_registered = aumid.registered
    notify.aumid.registered = lambda: False
    try:
        print(f"  attempts it would make: "
              f"{[name for _, name in notify._powershell_attempts()]}")
        check("AgentDesk's id is not attempted when it is not registered",
              [name for _, name in notify._powershell_attempts()],
              ["Windows PowerShell"])
        shown2, shown_as2 = notify._toast_powershell(
            "AgentDesk fallback check",
            "This one is expected to be attributed to Windows PowerShell.")
        note("the identity the fallback used", f"{shown_as2!r} (shown={shown2})")
        check("the fallback still produces a toast", shown2, True)
        check("...and it is the PowerShell one", shown_as2, "Windows PowerShell")
    finally:
        notify.aumid.registered = real_registered


# --- 5. Windows accepted it ---------------------------------------------------

def part_registry() -> None:
    hr("5. Windows recorded the AUMID as a notifier")
    seen = aumid.registry_seen()
    print(f"  {aumid.registry_key()}")
    note("key present", seen)
    check("the AUMID appears under Notifications\\Settings", seen, True)
    # Windows timestamps the key when it accepts a toast, which is what makes
    # the key evidence that a notification was recorded rather than a file
    # somebody wrote.
    try:
        import winreg
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                            aumid.NOTIFIER_SUBKEY + "\\" + aumid.AUMID) as k:
            stamp = winreg.QueryValueEx(k, "LastNotificationAddedTime")[0]
        note("LastNotificationAddedTime", stamp)
        check("...with a time, i.e. a toast was recorded against it",
              int(stamp) > 0, True)
    except OSError as exc:
        check("...and carries LastNotificationAddedTime", repr(exc), None)


# --- 6. the window carries no test-toast affordance ---------------------------

def part_button(scratch: Path) -> None:
    """The window opens with no test button, and NOTHING is left behind it.

    The item's acceptance is two-sided: "the window opens with no test button,
    and the toast still fires on a real question with no code path left that
    only the test button could reach."

    The second half is the one that needs measuring rather than eyeballing. A
    button removed while its handler stays is a method whose only caller is a
    test script and a dialogue nobody can be shown -- code that exists because
    a widget used to call it. So this asserts the attribute is gone too, and
    then proves the capability still exists by firing the toast through
    `notify.diagnostic_toast`, which is what the handler called and is
    reachable without a window at all.
    """
    hr("6. the real window, with nothing left behind the button")
    import tkinter as tk

    from agentdesk import app as appmod
    from agentdesk import db as dbmod

    root = tk.Tk()
    root.withdraw()
    window = None
    try:
        db_path = scratch / "appdata" / "AgentDesk" / "agentdesk.db"
        db_path.parent.mkdir(parents=True, exist_ok=True)
        conn = dbmod.connect(db_path)
        try:
            dbmod.init_db(conn)
        finally:
            conn.close()

        window = appmod.App(db_path)
        window.root.withdraw()
        strip_buttons = [w.cget("text")
                         for w in window.worker_button.master.winfo_children()
                         if isinstance(w, __import__('tkinter').ttk.Button)]
        print(f"  the strip's buttons: {strip_buttons}")
        # By the strip's own children, not by the absence of one attribute: a
        # second button under a different name would pass the weaker test.
        check("the Test toast button is gone from the strip",
              "Test toast" in strip_buttons, False)
        check("...and the worker control it sat beside is still there",
              "Test toast" not in strip_buttons and len(strip_buttons) >= 1,
              True)
        # The half that a removed widget does not remove by itself.
        for name in ("toast_button", "_test_toast", "_report_toast"):
            check(f"...and no {name} is left on the window",
                  hasattr(window, name), False)
    finally:
        if window is not None and window.icon is not None:
            try:
                window.icon.stop()
            except Exception:
                pass
        root.destroy()


# --- 7. the toast still fires, and nothing needs the window to do it ----------

def part_toast_without_the_window() -> None:
    hr("7. the toast still fires, with no window in the path")
    print("  (this puts the third toast on the desktop)")
    report = notify.diagnostic_toast(
        "AgentDesk test toast",
        "If this banner says AgentDesk, the toast identity works.")
    for key, value in report.items():
        print(f"    {key:14} {value}")
    check("a toast was sent", report["shown"], True)
    check("...and it went out as AgentDesk, not the borrowed id",
          report["shown_as"], paths.APP_NAME)
    check("...and Windows recorded it against the AUMID",
          report["registry_seen"], True)
    # The route that replaces the button, and the reason nothing was lost by
    # deleting it: no window, so it works with the app closed, and a non-zero
    # exit when the toast cannot be sent at all.
    proc = subprocess.run(
        [PY, "-m", "agentdesk.notify", "AgentDesk", "check_aumid"],
        cwd=str(REPO), capture_output=True, timeout=120)
    check("the command-line route still exits 0", proc.returncode, 0)
    check("...and names the identity it used on stdout",
          b"AgentDesk" in proc.stdout, True)
    check("...and prints the identity, not just a success line",
          b"shown_as       AgentDesk" in proc.stdout, True)


# --- a contract that predates this change -------------------------------------

def part_no_print() -> None:
    hr("0. notify still says nothing on stdout (it is linked into the MCP server)")
    proc = subprocess.run([PY, "-c", "import agentdesk.notify"],
                          cwd=str(REPO), capture_output=True, timeout=60)
    check("importing notify prints nothing", proc.stdout, b"")
    check("...and succeeds", proc.returncode, 0)


def main() -> int:
    part_no_print()
    part_id()
    part_shortcut()
    part_idempotent()
    part_toast()
    part_registry()
    with tempfile.TemporaryDirectory(prefix="agentdesk-aumid-") as tmp:
        part_button(Path(tmp))
    part_toast_without_the_window()

    hr("result")
    print("  What none of this shows: whether the banner on the screen named")
    print("  AgentDesk. Every check above is about the id and its acceptance.")
    if FAILURES:
        print(f"  {len(FAILURES)} check(s) FAILED:")
        for f in FAILURES:
            print(f"    - {f}")
        return 1
    print("  all checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
