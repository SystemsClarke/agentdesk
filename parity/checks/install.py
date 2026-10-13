"""`agentdesk install` / `uninstall`: start the core and the tray at login on every OS (new in the Rust build, so `source` is this repo).

Only the read-only verbs run here (`--dry-run`, `--status`): a real install would write the machine's login items. The Windows
Run key is pointed at a scratch key by AGENTDESK_RUN_KEY, and HOME / XDG_CONFIG_HOME are the board's isolated home."""
import json
import os
import sys
import uuid

from lib import Missing, feature

SRC = "src/install.rs"


def env(b):
    return {"AGENTDESK_RUN_KEY": "HKCU\\Software\\AgentDeskParity\\%s\\Run" % uuid.uuid4().hex, "XDG_CONFIG_HOME": os.path.join(b.home, ".config")}


def verb(b, *args):
    r = b.run(*args, env=env(b))
    if r.returncode == 2 and ("unrecognized subcommand" in r.stderr or "unexpected argument" in r.stderr):
        raise Missing("`agentdesk %s` is not a command here" % " ".join(args[:2]))
    return r


def os_name():
    return {"win32": "windows", "darwin": "macos"}.get(sys.platform, "linux")


@feature("install.dry-run", "install", "`agentdesk install --dry-run` says what it would register for this OS and changes nothing", SRC)
def _dry(b):
    r = verb(b, "install", "--dry-run")
    assert r.returncode == 0, r.stderr
    d = json.loads(r.stdout)
    assert d["dry_run"] is True and d["os"] == os_name(), d
    by = {c["name"]: c for c in d["changes"]}
    assert set(by) == {"core", "tray"} and all(c["action"] == "add" for c in by.values()), d
    assert " daemon" in by["core"]["content"] and " tray" in by["tray"]["content"] and "agentdesk" in by["core"]["content"].lower()
    if os_name() == "windows":
        assert by["core"]["target"].endswith("AgentDeskCore") and by["core"]["content"].startswith("conhost.exe --headless ")
    elif os_name() == "macos":
        assert by["core"]["target"].endswith("com.vispero.agentdesk.core.plist") and "RunAtLoad" in by["core"]["content"]
    else:
        assert by["tray"]["target"].endswith("agentdesk-tray.desktop") and "[Desktop Entry]" in by["tray"]["content"]
    only = json.loads(verb(b, "install", "--dry-run", "--no-tray").stdout)
    assert [c["name"] for c in only["changes"]] == ["core"]
    assert not os.path.exists(os.path.join(b.home, "Library")) and not os.path.exists(os.path.join(b.home, ".config", "autostart")), "a dry run writes nothing"


@feature("install.status", "install", "`agentdesk install --status` prints what is registered as JSON; `uninstall --dry-run` on a clean machine has nothing to remove", SRC)
def _status(b):
    r = verb(b, "install", "--status")
    assert r.returncode == 0, r.stderr
    d = json.loads(r.stdout)
    assert d["os"] == os_name() and d["installed"] is False, d
    assert {x["name"] for x in d["registered"]} >= {"core", "tray"} and all(x["state"] == "missing" for x in d["registered"]), d
    u = json.loads(verb(b, "uninstall", "--dry-run").stdout)
    assert u["dry_run"] is True and u["changes"] == [], u


@feature("install.from-dotnet", "install", "`agentdesk install --from-dotnet`: a dry run lists the steps, and off Windows it is a clean error", SRC)
def _dotnet(b):
    r = verb(b, "install", "--from-dotnet", "--dry-run")
    if os_name() == "windows":
        assert r.returncode == 0 and "AgentDesk.App" in r.stdout and "migrate --apply" in r.stdout, r.stdout + r.stderr
        assert r.stdout.index("AgentDesk.App") < r.stdout.index("AgentDesk.Core"), "the window is quit before the core"
    else:
        assert r.returncode != 0 and "only runs on Windows" in r.stderr, r.stderr
