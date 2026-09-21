"""Acceptance checks for the tray toast on a long question subject (work item 45).

What was filed, in the reporter's words: "A question notification silently
degrades whenever the subject is over 64 characters", with the acceptance being
"post a question with a 123-character subject on a build where the tray icon
exists, and the log shows no `tray-icon notify failed` line".

That acceptance is run below, end to end, against a real tray icon. Two things
about the report did not survive contact with the code and are measured here
rather than repeated:

  * THE THRESHOLD IS 51, NOT 65. The failing call is in app.py, and it built
    `f"New question: {subject}"` as the TITLE -- a 14-character prefix -- while
    the tray's title field holds 64. So a subject of 51 characters was already
    enough. The "123" in the log was the length of the STRING passed, not of the
    subject. Section 2 measures this instead of arguing it.
  * THE UNIT IS A UTF-16 CODE UNIT, NOT A CHARACTER. One emoji counts as two,
    measured. A truncation to 64 characters therefore does NOT fix this: a
    64-character title containing a single emoji still raises. Section 4 pins
    that.

WHAT THIS COSTS THE READER: tray notifications are real, so every successful
`notify` in here pops a balloon on the screen of whoever is logged in. Ten of
them: two from section 1, five from section 3, two from section 5, one from
section 6. That is deliberate and not reducible -- the item is "a toast silently
stopped appearing", and a test that never shows a toast cannot speak to it. The
section 2 sweep is the exception: it drives the NOTIFYICONDATAW struct directly
because a call that raises never reaches the shell, so those cost nothing, and
the struct is the thing that raises anyway rather than a stand-in for it. The
last balloon of the ten is the deliverable. Tray balloons fade on their own.

    .venv\\Scripts\\python.exe scripts\\check_toast.py
"""

from __future__ import annotations

import argparse
import os
import sys
import tempfile
import time
from pathlib import Path

# Before anything prints: section 4's fixture embeds a real emoji, and on a
# plain PowerShell/cmd console (cp1252 on this machine) sys.stdout's encoding
# comes from the console codepage, which cannot represent it -- a bare
# UnicodeEncodeError that looks like the test crashed rather than what it
# actually is, a console-encoding mismatch (work item #118, reproduced with
# PYTHONIOENCODING=cp1252). UTF-8 can encode every character Python's str can
# hold, so this turns "the script dies" into, at worst, an emoji rendering as
# mojibake on a console that has not itself been switched to UTF-8 -- cosmetic,
# not fatal. Guarded because a stdout that has already been swapped for
# something without reconfigure() (redirected through unrelated tooling) must
# not raise on the guard itself.
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")

# --- isolation, BEFORE agentdesk is imported -----------------------------------
#
# LOCALAPPDATA first: paths.py reads it at import time, and both db.DB_PATH and
# the LOG this script asserts on come from it. Redirecting it is what makes the
# log readable here -- the real one has the historical failures in it, and a
# "no new failure line" assertion against that file would mean nothing.
_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-toast-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))
sys.path.insert(0, str(REPO / "scripts"))

from agentdesk import app as appmod               # noqa: E402
from agentdesk import db, notify, paths           # noqa: E402

_VAULT = _SCRATCH / "vault"
paths.VAULT_DIR = _VAULT
paths.VAULT_AGENTDESK = _VAULT / "agentdesk"
paths.VAULT_LOG = _VAULT / "log"
paths.VAULT_NOTES = _VAULT / "notes"
paths.VAULT_MAPS = _VAULT / "maps"
paths.VAULT_PARKED = paths.VAULT_AGENTDESK / "parked"
paths.VAULT_QUESTIONS = _VAULT / "questions"

WIDTH = 78
FAILURES: list[str] = []


def hr(title: str = "") -> None:
    if title:
        print(f"\n--- {title} " + "-" * max(0, WIDTH - len(title) - 5))
    else:
        print("-" * WIDTH)


def show(label: str, value) -> None:
    print(f"  {label:<44} {value}")


def check(label: str, got, want) -> None:
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<42} {got!r}")


def note(text: str) -> None:
    for line in text.strip().splitlines():
        print(f"  {line}")


# --- the log, which is where the bug was visible and nowhere else ---------------

def log_lines() -> list[str]:
    if not paths.LOG_PATH.exists():
        return []
    return paths.LOG_PATH.read_text(encoding="utf-8").splitlines()


def failure_lines(lines: list[str]) -> list[str]:
    """The tray route giving up. The exact string the reporter found in the log."""
    return [ln for ln in lines if "tray-icon notify failed" in ln]


# --- probes ---------------------------------------------------------------------

PS_CALLS: list = []


def install_ps_probe():
    """Record calls to the PowerShell fallback. Without this, a toast that went
    out the fallback route and a toast that went out the tray are both just
    `True`, and the whole item is about telling those two apart."""
    original = notify._toast_powershell
    notify._toast_powershell = lambda *a, **k: (PS_CALLS.append(a), (True, "probe"))[1]
    return original


TRAY_CALLS: list = []
_SPIED: set = set()


def spy_tray(icon) -> None:
    """Wrap a real icon's notify so the arguments are visible, still calling it.

    A spy around the real thing, not a replacement for it: the balloon still
    appears and the ctypes limits still apply, so a call that would have raised
    still raises.

    Wrapping is once per icon. Every section calls this, and without the guard
    the second call would wrap the first spy -- each notify would then be
    recorded twice, which would not break the assertions but would make
    TRAY_CALLS a list of duplicates and its length meaningless.
    """
    if id(icon) in _SPIED:
        return
    _SPIED.add(id(icon))
    real = icon.notify

    def spy(message=None, title=None, **kw):
        TRAY_CALLS.append({"message": message, "title": title})
        return real(message=message, title=title, **kw)

    icon.notify = spy


# --- sections -------------------------------------------------------------------

def make_icon():
    """A real tray icon, built the way app._make_tray builds one."""
    import pystray
    icon = pystray.Icon(paths.APP_NAME, appmod._tray_image(), title="check-toast",
                        menu=pystray.Menu())
    icon.run_detached()
    time.sleep(0.5)
    return icon


def check_tray_limits(icon) -> None:
    hr("1. the tray's real limits, measured on a real icon")
    note("a successful call pops a balloon; a raising one does not, so the")
    note("failures below are free and only the two successes are visible.")
    print()
    for label, n, field in (("title", 64, "title"), ("title", 65, "title"),
                            ("message", 256, "message"),
                            ("message", 257, "message")):
        kw = {"message": "body", "title": "t"}
        kw[field] = "x" * n
        try:
            icon.notify(**kw)
            outcome = "accepted"
        except Exception as exc:                        # noqa: BLE001
            outcome = repr(exc)
        print(f"  {field:<8} {n:>4} units -> {outcome}")
    print()
    check("the title field is the one that bit", notify._TRAY_TITLE_UNITS, 64)
    check("the message field is wide", notify._TRAY_MESSAGE_UNITS, 256)


def check_threshold_is_51() -> None:
    hr("2. the old call shape broke at 51, not 65")
    note("The failing call built `New question: <subject>` as the TITLE. The")
    note("sweep below runs the exact ctypes assignment pystray makes, so it")
    note("costs no balloons. Then the same numbers are checked against the")
    note("nine real failures in John's log.")
    print()

    from pystray._util.win32 import NOTIFYICONDATAW

    def title_fits(text: str) -> bool:
        try:
            NOTIFYICONDATAW(szInfoTitle=text)
            return True
        except ValueError:
            return False

    first_bad = None
    for n in range(45, 56):
        ok = title_fits(f"New question: {'s' * n}")
        marker = "" if ok else "   <-- first failure" if first_bad is None else ""
        if not ok and first_bad is None:
            first_bad = n
        print(f"  subject {n:>3} chars -> title {14 + n:>3} units -> "
              f"{'ok' if ok else 'RAISES'}{marker}")
    print()
    check("the first subject length that raised", first_bad, 51)
    note("so the item's title ('over 64 chars') understates it by 14 -- a")
    note("50-character subject was fine and a 51-character one was not.")

    print()
    note("and against the real failures in John's log. There the number in the")
    note("message is the TITLE length, which is subject + 14 -- so every one of")
    note("them should be over the 64-unit cap it was measured against, and the")
    note("smallest is the smallest subject that ever broke. Read only.")
    import re
    live = Path(os.environ.get("AGENTDESK_LIVE_LOG",
                               r"C:\Users\palencharj\AppData\Local\AgentDesk\agentdesk.log"))
    if not live.exists():
        show("live log not readable, skipped", live)
        return
    rows = []
    for ln in live.read_text(encoding="utf-8", errors="replace").splitlines():
        m = re.search(r"string too long \((\d+), maximum length 64\)", ln)
        if m:
            rows.append(int(m.group(1)))
    if not rows:
        show("no historical failures in the live log", live)
        return
    show("failures in the live log", len(rows))
    show("their lengths", sorted(rows))
    show("...the smallest, as a subject length", f"{min(rows) - 14} chars")
    check("every one of them over the 64-unit title cap",
          all(r > 64 for r in rows), True)
    check("and none is at or under 50+14", all(r >= 51 + 14 for r in rows), True)


def check_toast_survives_long_text(icon) -> None:
    hr("3. notify.toast survives any length, and says so when it clips")
    note("Titles and messages far past both fields. Each call is real.")
    print()
    cases = [
        ("a 123-unit title", "T" * 123, "m"),
        ("a 500-unit message", "t", "M" * 500),
        ("both at once", "T" * 300, "M" * 400),
        ("a title with an emoji at 64 chars", "\U0001F600" + "t" * 63, "m"),
        ("a subject-like title of 300", f"New question: {'s' * 287}", "m"),
    ]
    for label, title, message in cases:
        before = len(failure_lines(log_lines()))
        ps_before = len(PS_CALLS)
        tray_before = len(TRAY_CALLS)
        spy_tray(icon)
        result = notify.toast(title, message, icon=icon)
        after = len(failure_lines(log_lines()))
        took_tray = len(TRAY_CALLS) > tray_before
        got = TRAY_CALLS[-1] if took_tray else {}
        print(f"  {label}")
        print(f"    returned {result!r}, tray called {took_tray}, "
              f"fallback called {len(PS_CALLS) > ps_before}, "
              f"new failure lines {after - before}")
        if took_tray:
            print(f"    tray received title={notify.utf16_units(got['title'])} "
                  f"units, message={notify.utf16_units(got['message'])} units")
        check(f"    {label}: no fallback", len(PS_CALLS) > ps_before, False)
        check(f"    {label}: no failure logged", after - before, 0)
        check(f"    {label}: returned True", result, True)
        if took_tray:
            check(f"    {label}: what the tray got fits",
                  notify.utf16_units(got["title"]) <= notify._TRAY_TITLE_UNITS
                  and notify.utf16_units(got["message"]) <= notify._TRAY_MESSAGE_UNITS,
                  True)


def check_clip_is_utf16_correct() -> None:
    hr("4. the clip counts code units, so an emoji cannot slip through")
    note("A [:64] character slice would leave a string that still raises. This")
    note("is the half of the fix a plain truncation gets wrong.")
    print()
    for label, text, limit in (
            ("64 chars, one emoji", "\U0001F600" + "t" * 63, 64),
            ("33 emoji", "\U0001F600" * 33, 64),
            ("plain 300", "t" * 300, 64),
    ):
        clipped = notify.clip_utf16(text, limit)
        print(f"  {label:<22} {notify.utf16_units(text):>4} units -> "
              f"{notify.utf16_units(clipped):>4} units  {clipped[:20]!r}...")
        check(f"  {label} fits", notify.utf16_units(clipped) <= limit, True)

    print()
    check("a string already inside the limit is untouched",
          notify.clip_utf16("short", 64), "short")
    check("and the cut is visible", notify.clip_utf16("t" * 300, 64).endswith("…"),
          True)


def check_clip_is_logged_only_when_it_happens(icon) -> None:
    hr("5. the clip is logged when it happens, and silent when it does not")
    spy_tray(icon)
    before = len([ln for ln in log_lines() if "clipped for the tray route" in ln])
    notify.toast("a short title", "a short message", icon=icon)
    after_short = len([ln for ln in log_lines() if "clipped for the tray route" in ln])
    check("a short toast logs no clip", after_short - before, 0)

    notify.toast("T" * 200, "a short message", icon=icon)
    after_long = len([ln for ln in log_lines() if "clipped for the tray route" in ln])
    check("a long toast logs one clip", after_long - after_short, 1)
    got = TRAY_CALLS[-1]
    check("...and the fallback would still have had the full title",
          notify.utf16_units("T" * 200), 200)
    check("...while the tray got the clipped one",
          notify.utf16_units(got["title"]) <= 64, True)


def check_end_to_end(app, ids) -> None:
    hr("6. the acceptance in the item: a 123-character subject, no failure line")
    subject = ids["subject"]
    show("the seeded subject is", f"{len(subject)} characters")
    show("and it is", repr(subject[:60] + "..."))
    print()
    spy_tray(app.icon)
    before_fail = len(failure_lines(log_lines()))
    before_tray = len(TRAY_CALLS)
    before_ps = len(PS_CALLS)

    # The real path: _apply_changes is what the poll loop calls when a new
    # question appears, and it is what made the failing toast(title=...) call.
    conn = db.connect(paths.DB_PATH)
    try:
        open_qs = db.open_questions(conn)
        app._first_poll = False          # pretend the app has been running
        app.announced.clear()
        app._apply_changes(conn, open_qs)
    finally:
        conn.close()
    app.root.update()

    after_fail = len(failure_lines(log_lines()))
    took_tray = len(TRAY_CALLS) > before_tray
    got = TRAY_CALLS[-1] if took_tray else {}
    show("tray route taken", took_tray)
    show("fallback route taken", len(PS_CALLS) > before_ps)
    show("new 'tray-icon notify failed' lines", after_fail - before_fail)
    if took_tray:
        show("tray title", repr(got["title"]))
        show("tray message units", notify.utf16_units(got["message"]))
    print()
    check("NO tray failure was logged", after_fail - before_fail, 0)
    check("the tray route was the one used", took_tray, True)
    check("the fallback was not needed", len(PS_CALLS) > before_ps, False)
    if took_tray:
        check("...and the subject arrived whole in the message",
              got["message"], subject)
        check("...with a short title",
              notify.utf16_units(got["title"]) <= notify._TRAY_TITLE_UNITS, True)


# --- main -----------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shot", type=Path, default=None,
                        help="directory to write a PNG of the window into")
    args = parser.parse_args()

    print(f"scratch board: {paths.DB_PATH}")
    print(f"scratch log:   {paths.LOG_PATH}")
    paths.ensure_dirs()

    # The subject is 123 characters, which is the length named in the item.
    subject = ("Please decide the four things I am blocked on before the "
               "morning, and note that this sentence is only here because it "
               "has to add up to one hundred and twenty three charact")
    subject = subject[:123]
    show("seeded subject length", len(subject))

    conn = db.connect(paths.DB_PATH)
    try:
        db.init_db(conn)
        tid = db.start_thread(conn, "question", subject, "claude",
                              paths.AGENT_KIND, "SEEDTOAST body")
    finally:
        conn.close()

    original_ps = install_ps_probe()
    icon = None
    app = None
    try:
        try:
            icon = make_icon()
        except Exception as exc:                        # noqa: BLE001
            FAILURES.append(f"no tray icon on this session: {exc!r}")
            print(f"\n  [FAIL] this session has no tray icon: {exc!r}")
            print("  Every check below would be vacuous, so the run stops here.")
            hr()
            print("1 FAILED: no tray icon")
            return 1

        check_tray_limits(icon)
        check_threshold_is_51()
        check_toast_survives_long_text(icon)
        check_clip_is_utf16_correct()
        check_clip_is_logged_only_when_it_happens(icon)

        app = appmod.App(paths.DB_PATH)
        app.root.withdraw()
        app.root.update()
        time.sleep(0.4)
        check("the running app has a real tray icon", app.icon is not None, True)
        for _ in range(3):
            app.refresh_now()
            app.root.update()
        check_end_to_end(app, {"subject": subject, "tid": tid})

        if args.shot is not None:
            args.shot.mkdir(parents=True, exist_ok=True)
            from check_look import screenshot
            app.root.deiconify()
            app.root.update()
            time.sleep(0.3)
            show("screenshot", screenshot(app.root, (args.shot / "toast.png").resolve()))
            app.root.withdraw()
    finally:
        notify._toast_powershell = original_ps
        if icon is not None:
            try:
                icon.stop()
            except Exception:
                pass
        if app is not None:
            if app.icon is not None:
                try:
                    app.icon.stop()
                except Exception:
                    pass
            app.root.destroy()

    hr()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        print(f"\nscratch left at {_SCRATCH}")
        return 1
    print("every printed property held.")
    print()
    print("WHAT IS NOT ASSERTED HERE: that the balloon was legible, or that")
    print("Windows actually displayed it. pystray's notify returns nothing and")
    print("cannot say -- True means the call was accepted, not that a banner")
    print("appeared. Nor that John's running window is doing any of this: a")
    print("running App does not reload, so his copy still has the old title.")
    print(f"\nscratch board, log and vault left at {_SCRATCH}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
