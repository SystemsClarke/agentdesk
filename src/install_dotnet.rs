//! `agentdesk install --from-dotnet` and `--rollback`: the move from the .NET AgentDesk on a Windows machine, in the binary
//! (what scripts/cutover.ps1 used to do itself). Every step goes through `install::Host`, so the order is tested with a fake.
//!
//! The order is the point: the old window is quit BEFORE the old core (the window relaunches the core if it is left running), both
//! are checked again after a pause in case something relaunches them, and nothing else is touched if either is still there.
//! Nothing of the .NET install is deleted, and a record of how it was is saved first so `--rollback` can put it back.

use crate::install::{self, schtasks_change_args, Ctx, Host, Os, TaskAction};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

const OLD_APP: &str = "AgentDesk.App.exe";
const OLD_CORE: &str = "AgentDesk.Core.exe";
const OLD_RUN: &str = "AgentDesk";
/// The Run value the first cutover script used for the daemon; a rollback removes it too.
const SCRIPT_RUN: &str = "AgentDeskRust";
const TASK: &str = "AgentDesk-Backup";
const PYTHON_ENV: &str = "AGENTDESK_PYTHON_BIN";
/// How long the new core gets to bring back the identities that were running (it starts them as fast as the session limit allows).
const RESUME_WAIT: Duration = Duration::from_secs(120);
/// What the move sets aside instead of deleting (under the data folder), so `--rollback` can put it back.
const ASIDE: &str = "dotnet-old";
/// The .NET install's parts, relative to the AgentDeskApp folder.
const OLD_PARTS: [&str; 4] = ["current", "packages", "AgentDesk.exe", "Update.exe"];
const OLD_TASK: &str = "AgentDesk-Daemon";
/// How long to wait for something that relaunches the old app (a watchdog, a scheduled task) before looking again.
const SETTLE: Duration = Duration::from_secs(20);

fn record_path(ctx: &Ctx) -> PathBuf {
    ctx.data.join("cutover-rollback.json")
}

/// How the machine was before, saved so `--rollback` can put it back. The field names are the ones the PowerShell script wrote.
struct Record {
    run: Option<String>,
    path: Option<String>,
    task: Option<TaskAction>,
    python: Option<String>,
}

impl Record {
    fn to_json(&self) -> Value {
        let t = self.task.as_ref();
        json!({
            "run": self.run, "path": self.path, "python": self.python,
            "backupExecute": t.map(|t| t.execute.as_str()), "backupArguments": t.map(|t| t.arguments.as_str()), "backupDir": t.map(|t| t.dir.as_str()),
        })
    }
    fn from_json(v: &Value) -> Record {
        let s = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(String::from);
        let task = s("backupExecute").map(|execute| TaskAction { execute, arguments: s("backupArguments").unwrap_or_default(), dir: s("backupDir").unwrap_or_default() });
        Record { run: s("run"), path: s("path"), task, python: s("python") }
    }
}

/// The user PATH with `dir` first, once, and nothing from the old `AgentDeskApp` folder.
pub fn fix_path(path: &str, dir: &str, old_app: &str) -> String {
    let same = |a: &str, b: &str| a.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(b.trim_end_matches(['\\', '/']));
    let under_old = |p: &str| !old_app.is_empty() && p.to_lowercase().starts_with(&old_app.to_lowercase());
    std::iter::once(dir).chain(path.split(';').filter(|p| !p.is_empty() && !same(p, dir) && !under_old(p))).collect::<Vec<_>>().join(";")
}

/// The python an old backup task ran, when it was a virtual environment's (its own folder has slack_bolt and numpy already).
fn old_python(h: &dyn Host, t: &TaskAction) -> Option<PathBuf> {
    // split by hand: the task's path is a Windows path whichever OS this is compiled for (and the tests run on all of them)
    let raw = t.execute.trim_matches('"');
    let (dir, file) = raw.rsplit_once(['\\', '/'])?;
    let stem = file.to_lowercase();
    if !(stem.starts_with("python") && stem.ends_with(".exe")) {
        return None;
    }
    let sep = if raw.contains('\\') { '\\' } else { '/' };
    Some(PathBuf::from(format!("{dir}{sep}python.exe"))).filter(|p| h.exists(p))
}

/// Quits the old window, then the old core, and refuses to go on if either comes back.
fn quit_old(h: &dyn Host) -> Result<()> {
    if ![OLD_APP, OLD_CORE].into_iter().any(|i| h.process_running(i)) {
        return Ok(()); // already gone (the run that copied the binary here quit them)
    }
    h.kill_process(OLD_APP);
    h.sleep(Duration::from_secs(2));
    h.kill_process(OLD_CORE);
    h.sleep(SETTLE);
    h.kill_process(OLD_APP);
    h.kill_process(OLD_CORE);
    h.sleep(Duration::from_secs(1));
    let still: Vec<&str> = [OLD_APP, OLD_CORE].into_iter().filter(|i| h.process_running(i)).collect();
    if still.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("the .NET app is still running ({}): quit it from its tray icon and run this again. Nothing else was changed.", still.join(", ")))
    }
}

/// `child`: this is the detached run (see below), so it does the work; otherwise it only hands over to one.
pub fn from_dotnet(h: &dyn Host, ctx: &Ctx, tray: bool, dry_run: bool, child: bool) -> Result<Vec<String>> {
    if ctx.os != Os::Windows {
        bail!("--from-dotnet moves a Windows machine off the .NET app; it only runs on Windows");
    }
    // Windows paths, built by hand: `Path` follows the OS this is compiled for, and the tests run on every OS
    let exe = ctx.exe.to_string_lossy().into_owned();
    let old_app = format!("{}\\AgentDeskApp", ctx.local.to_string_lossy().trim_end_matches(['\\', '/']));
    let dir = exe.rsplit_once(['\\', '/']).map(|(d, _)| d.to_string()).unwrap_or_default();
    // the binary lives beside the .NET app (its folder is the one a security product already knows), unless told otherwise
    let home_dir = std::env::var("AGENTDESK_INSTALL_DIR").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| format!("{old_app}\\bin"));
    let target = PathBuf::from(format!("{home_dir}\\agentdesk.exe"));
    let norm = |s: &str| s.replace('/', "\\").to_lowercase();
    let relocate = norm(&exe) != norm(&target.to_string_lossy());
    if dry_run {
        let steps = [
            format!("save how to go back to {}", record_path(ctx).display()),
            if relocate { format!("copy this binary to {} and carry on from there", target.display()) } else { "(this binary is already in its folder)".to_string() },
            "quit AgentDesk.App, then AgentDesk.Core, wait, and quit them again if they came back (stop here if they stay)".to_string(),
            format!("remove the .NET core from logon (HKCU Run value {OLD_RUN})"),
            "agentdesk migrate --apply: back up the board, point Claude Code at this binary".to_string(),
            format!("point {TASK} at {exe} backup, if the task exists"),
            format!("put {dir} first on the user PATH, drop {old_app}"),
            format!("point {PYTHON_ENV} at the old backup task's python, if it ran one"),
            format!("register the core{} to start at logon", if tray { " and the tray" } else { "" }),
            "end any leftover agentdesk daemon or tray, then start the core now".to_string(),
            "check: the core holds the lock, no row of the board is missing, and every identity that was running is running again (else undo it all)".to_string(),
            format!("clean up: set the .NET program files aside in {ASIDE} (nothing is deleted) and drop the first script's logon entries"),
        ];
        return Ok(std::iter::once("dry run: nothing is changed. It would:".to_string()).chain(steps.iter().enumerate().map(|(i, s)| format!("  {}. {s}", i + 1))).collect());
    }
    if !h.exists(&ctx.exe) {
        bail!("{exe} is not there");
    }
    let mut say = Vec::new();

    // 1. how to go back. A record from an earlier run is the original state, so it is kept.
    let rec_file = record_path(ctx);
    if h.file_read(&rec_file).is_none() {
        let rec = Record { run: h.reg_get(OLD_RUN), path: h.user_env_get("Path"), task: h.task_get(TASK), python: h.user_env_get(PYTHON_ENV) };
        h.file_write(&rec_file, &serde_json::to_string_pretty(&rec.to_json())?)?;
        say.push(format!("saved how to go back: {}", rec_file.display()));
    }
    let rec = Record::from_json(&serde_json::from_str(&h.file_read(&rec_file).unwrap_or_default()).unwrap_or(Value::Null));

    // 1b. what the board holds now, and a snapshot of it
    let before = h.board_summary()?;
    if !before.integrity_ok {
        bail!("sqlite does not find the board sound: not moving anything. Run `agentdesk backup` and look at the board first.");
    }
    say.push(format!(
        "the board: {} messages in {} threads, {} identities ({} running), {} goals",
        before.messages,
        before.threads,
        before.identities,
        before.running.len(),
        before.goals
    ));
    say.push(format!("snapshot of the board: {}", h.backup_board()?));

    // 2. hand over to a run that is not a descendant of the .NET core: quitting the core ends everything it hosts (the shell this
    //    was typed in, when that is an agent's), so the quitting is done by a scheduled task, which Windows starts outside that tree.
    if !child {
        if relocate {
            h.copy_file(&ctx.exe, &target)?;
            if !h.exists(&target) {
                bail!("{} is not there after the copy", target.display());
            }
            say.push(format!("copied this binary to {}", target.display()));
        }
        let mut args = vec!["install".to_string(), "--from-dotnet".to_string()];
        if !tray {
            args.push("--no-tray".into());
        }
        say.push(format!("carrying on from {} in a detached run (log: {}); this window may be ended by the move", target.display(), ctx.data.join("cutover.log").display()));
        for l in &say {
            println!("{l}");
        }
        if !h.relaunch(&target, &args)? {
            bail!("the detached run failed; see {}. Nothing is lost: --rollback undoes what was changed.", ctx.data.join("cutover.log").display());
        }
        return Ok(vec![]);
    }

    // 3. quit the old window and core
    quit_old(h)?;
    say.push("quit the .NET window and core".into());
    h.kill_stray();
    h.reg_delete(OLD_RUN)?;
    say.push("took the .NET core off logon".into());

    // 3. the board and Claude Code
    for l in h.migrate()?.lines().filter(|l| !l.trim().is_empty()) {
        say.push(format!("  {l}"));
    }

    // 4. the hourly backup
    if h.task_get(TASK).is_some() {
        let o = h.run("schtasks", &schtasks_change_args(TASK, &exe))?;
        if !o.ok {
            bail!("schtasks could not point {TASK} at {exe} backup");
        }
        say.push(format!("pointed {TASK} at {exe} backup"));
    } else {
        say.push(format!("no {TASK} task: run `agentdesk backup` on a schedule yourself"));
    }

    // 5. PATH, and the python of the Python half
    if let Some(path) = h.user_env_get("Path") {
        let new = fix_path(&path, &dir, &old_app);
        if new != path {
            h.user_env_set("Path", Some(&new))?;
            say.push(format!("put {dir} first on the user PATH"));
        }
    }
    if let Some(py) = rec.task.as_ref().and_then(|t| old_python(h, t)) {
        h.user_env_set(PYTHON_ENV, Some(&py.to_string_lossy()))?;
        say.push(format!("{PYTHON_ENV} = {}", py.display()));
    }

    // 6. logon registration, then the core now
    let r = install::install(h, ctx, tray, false)?;
    say.push(format!("registered at logon: {} change(s)", r["changes"].as_array().map(|a| a.len()).unwrap_or(0)));
    h.kill_stray();
    let started = h.start_core();
    match started.and_then(|_| verify(h, ctx, &before, &mut say)) {
        Ok(()) => {}
        Err(e) => {
            // do not leave a half-moved machine: put the .NET app back
            let undone = rollback(h, ctx, false);
            bail!(
                "{e:#}\nThe move was undone ({}). Nothing on the board was changed.",
                if undone.is_ok() { "the .NET logon entry, PATH and backup task are back" } else { "UNDO ALSO FAILED: run `agentdesk install --rollback`" }
            );
        }
    }

    // 7. clean up: the .NET program files are set aside, not deleted
    h.reg_delete(SCRIPT_RUN)?;
    h.task_delete(OLD_TASK);
    let aside = ctx.data.join(ASIDE);
    let mut moved = Vec::new();
    for part in OLD_PARTS {
        if h.move_aside(&PathBuf::from(format!("{old_app}\\{part}")), &aside.join(part)) {
            moved.push(part);
        }
    }
    if moved.is_empty() {
        say.push("the .NET program files were already gone".into());
    } else {
        say.push(format!("set the .NET program files aside in {} ({}): `--rollback` puts them back, delete the folder when you are happy", aside.display(), moved.join(", ")));
    }

    // 8. the binary must still be there
    if !h.exists(&ctx.exe) {
        bail!("{exe} is gone again at the end of the run");
    }
    say.push("Done. Restart your Claude Code sessions so they reconnect to the new MCP server; `agentdesk tui` is the window.".into());
    Ok(say)
}

/// The move worked: the core holds its lock from the binary in its place, the board lost nothing, and every identity that was running
/// has been resumed (the new core queues what the board says was running and starts it as slots allow).
fn verify(h: &dyn Host, ctx: &Ctx, before: &install::Summary, say: &mut Vec<String>) -> Result<()> {
    let (pid, core_exe) = h.core_info().ok_or_else(|| anyhow!("the core is not holding its lock"))?;
    let norm = |s: &str| s.replace('/', "\\").to_lowercase();
    if norm(&core_exe) != norm(&ctx.exe.to_string_lossy()) {
        bail!("the core that is running (pid {pid}) is {core_exe}, not {}", ctx.exe.display());
    }
    say.push(format!("the core is running (pid {pid})"));
    let mut after = h.board_summary()?;
    let mut waited = Duration::ZERO;
    while before.running.iter().any(|n| !after.running.contains(n)) && waited < RESUME_WAIT && h.core_info().is_some() {
        h.sleep(Duration::from_secs(5));
        waited += Duration::from_secs(5);
        after = h.board_summary()?;
    }
    if !after.integrity_ok {
        bail!("sqlite does not find the board sound after the move");
    }
    for (what, b, a) in [
        ("messages", before.messages, after.messages),
        ("threads", before.threads, after.threads),
        ("identities", before.identities, after.identities),
        ("goals", before.goals, after.goals),
    ] {
        if a < b {
            bail!("the board has fewer {what} than before ({a} < {b})");
        }
    }
    if h.core_info().is_none() {
        bail!("the core stopped while the identities were coming back");
    }
    say.push(format!("the board is whole: {} messages, {} threads, {} identities, {} goals", after.messages, after.threads, after.identities, after.goals));
    // Slack: the same token files, now read by the core's bridge (a warning, not a failure: Slack may never have been set up)
    let mut slack = h.slack_state();
    let mut tries = 0;
    while !slack.0 && tries < 6 && slack.1.contains("NOT up") {
        h.sleep(Duration::from_secs(5));
        slack = h.slack_state();
        tries += 1;
    }
    say.push(slack.1);
    let missing: Vec<&String> = before.running.iter().filter(|n| !after.running.contains(*n)).collect();
    say.push(format!("identities running again: {} of {}", before.running.len() - missing.len(), before.running.len()));
    if !missing.is_empty() {
        say.push(format!(
            "  still queued (the session limit or the usage governor holds them; the core starts them as slots free up): {}",
            missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(())
}

pub fn rollback(h: &dyn Host, ctx: &Ctx, dry_run: bool) -> Result<Vec<String>> {
    if ctx.os != Os::Windows {
        bail!("--rollback undoes --from-dotnet, which only runs on Windows");
    }
    let file = record_path(ctx);
    let text = h.file_read(&file).ok_or_else(|| anyhow!("nothing to roll back: {} is missing", file.display()))?;
    let rec = Record::from_json(&serde_json::from_str(&text)?);
    if dry_run {
        return Ok(vec![
            "dry run: nothing is changed. It would stop the core, remove the logon entries this binary added, and restore:".into(),
            format!("  the .NET logon entry: {}", rec.run.as_deref().unwrap_or("(there was none)")),
            format!("  the user PATH ({} characters)", rec.path.as_ref().map(|p| p.len()).unwrap_or(0)),
            format!("  the {TASK} task: {}", rec.task.as_ref().map(|t| format!("{} {}", t.execute, t.arguments)).unwrap_or("(there was none)".into())),
        ]);
    }
    let mut say = Vec::new();
    h.stop_core()?;
    install::uninstall(h, ctx, false)?;
    h.reg_delete(SCRIPT_RUN)?;
    say.push("stopped the core and removed what this binary registered at logon".into());
    if let Some(run) = &rec.run {
        h.reg_set(OLD_RUN, run)?;
        say.push("restored the .NET logon entry".into());
    }
    if let Some(path) = &rec.path {
        h.user_env_set("Path", Some(path))?;
        say.push("restored the user PATH".into());
    }
    if let Some(t) = &rec.task {
        h.task_restore(TASK, t)?;
        say.push(format!("pointed {TASK} back at {}", t.execute));
    }
    h.user_env_set(PYTHON_ENV, rec.python.as_deref())?;
    let old_app = format!("{}\\AgentDeskApp", ctx.local.to_string_lossy().trim_end_matches(['\\', '/']));
    let aside = ctx.data.join(ASIDE);
    let back: Vec<&str> = OLD_PARTS.into_iter().filter(|part| h.move_aside(&aside.join(part), &PathBuf::from(format!("{old_app}\\{part}")))).collect();
    if !back.is_empty() {
        say.push(format!("put the .NET program files back ({})", back.join(", ")));
    }
    h.file_remove(&file)?;
    let old = ctx.local.join("AgentDeskApp").join("current").join("AgentDesk.App.exe");
    say.push(format!(
        "Rolled back. Put Claude Code back on the old binary with the files in {}\\migrate_backups, then start the old app: \"{}\"",
        ctx.data.display(),
        old.display()
    ));
    Ok(say)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::fake::Fake;

    fn ctx() -> Ctx {
        Ctx {
            os: Os::Windows,
            exe: PathBuf::from(r"C:\Users\jo\AppData\Local\AgentDeskApp\bin\agentdesk.exe"),
            home: PathBuf::from(r"C:\Users\jo"),
            data: PathBuf::from(r"C:\Users\jo\AppData\Local\AgentDesk"),
            config: PathBuf::new(),
            local: PathBuf::from(r"C:\Users\jo\AppData\Local"),
            uid: String::new(),
        }
    }

    /// A machine with the .NET app running, its logon entry, a backup task that runs a venv python, and an old PATH.
    fn dotnet_machine(c: &Ctx) -> Fake {
        let h = Fake::default();
        h.running.borrow_mut().extend([OLD_APP.to_string(), OLD_CORE.to_string()]);
        h.reg.borrow_mut().insert("AgentDesk".into(), r"C:\Users\jo\AppData\Local\AgentDeskApp\current\AgentDesk.Core.exe".into());
        h.reg.borrow_mut().insert("OtherApp".into(), "keep".into());
        h.env.borrow_mut().insert("Path".into(), r"C:\Windows;C:\Users\jo\AppData\Local\AgentDeskApp\current;C:\tools".into());
        *h.task.borrow_mut() = Some(TaskAction { execute: r"C:\old\checkout\.venv\Scripts\python.exe".into(), arguments: "backup.py".into(), dir: r"C:\old\checkout".into() });
        h.present.borrow_mut().insert(c.exe.clone());
        *h.summary.borrow_mut() = install::Summary { messages: 10, threads: 3, identities: 4, goals: 1, running: vec!["a".into(), "b".into()], integrity_ok: true };
        *h.core_exe.borrow_mut() = c.exe.to_string_lossy().into_owned();
        for part in OLD_PARTS {
            h.present.borrow_mut().insert(PathBuf::from(format!(r"C:\Users\jo\AppData\Local\AgentDeskApp\{part}")));
        }
        h.present.borrow_mut().insert(PathBuf::from(r"C:\old\checkout\.venv\Scripts\python.exe"));
        h
    }

    #[test]
    fn the_steps_run_in_order_window_before_core() {
        let c = ctx();
        let h = dotnet_machine(&c);
        let say = from_dotnet(&h, &c, true, false, true).unwrap();
        let order = [
            "write C:\\Users\\jo\\AppData\\Local\\AgentDesk\\cutover-rollback.json",
            "kill AgentDesk.App.exe",
            "kill AgentDesk.Core.exe",
            "reg-delete AgentDesk",
            "migrate",
            "run schtasks /Change /TN AgentDesk-Backup /TR",
            "env-set Path",
            "env-set AGENTDESK_PYTHON_BIN",
            "reg-set AgentDeskCore",
            "reg-set AgentDeskTray",
            "start-core",
        ];
        let at: Vec<usize> = order.iter().map(|p| h.pos(p)).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "wrong order: {at:?} in {:#?}", h.log.borrow());
        assert!(h.pos("kill AgentDesk.App.exe") < h.pos("kill AgentDesk.Core.exe"));
        assert!(h.count("kill AgentDesk.App.exe") == 2 && h.count("kill AgentDesk.Core.exe") == 2, "both are killed again after the wait");
        assert!(say.last().unwrap().starts_with("Done"));
        // the result
        assert_eq!(h.env.borrow()["Path"], r"C:\Users\jo\AppData\Local\AgentDeskApp\bin;C:\Windows;C:\tools");
        assert_eq!(h.env.borrow()["AGENTDESK_PYTHON_BIN"], r"C:\old\checkout\.venv\Scripts\python.exe");
        let reg = h.reg.borrow();
        assert!(!reg.contains_key("AgentDesk") && reg.contains_key("OtherApp") && reg["AgentDeskCore"].starts_with("conhost.exe --headless"));
    }

    #[test]
    fn it_aborts_without_touching_anything_when_the_old_app_comes_back() {
        let c = ctx();
        let h = dotnet_machine(&c);
        h.respawn.set(true);
        let e = from_dotnet(&h, &c, true, false, true).unwrap_err().to_string();
        assert!(e.contains("still running") && e.contains(OLD_APP), "{e}");
        for later in ["reg-delete", "migrate", "run schtasks", "env-set", "reg-set", "start-core"] {
            assert_eq!(h.count(later), 0, "{later} must not run");
        }
        assert!(h.count("write ") == 1, "only the rollback record was written");
        assert!(h.reg.borrow().contains_key("AgentDesk"), "the .NET logon entry is still there");
    }

    #[test]
    fn a_second_run_keeps_the_first_runs_record() {
        let c = ctx();
        let h = dotnet_machine(&c);
        from_dotnet(&h, &c, true, false, true).unwrap();
        let first = h.files.borrow()[&record_path(&c)].clone();
        h.running.borrow_mut().insert(OLD_APP.into());
        from_dotnet(&h, &c, true, false, true).unwrap();
        assert_eq!(h.files.borrow()[&record_path(&c)], first, "the record still describes the .NET machine");
        assert_eq!(h.count("reg-set AgentDeskCore"), 1, "registration is idempotent");
    }

    #[test]
    fn rollback_puts_the_machine_back() {
        let c = ctx();
        let h = dotnet_machine(&c);
        let (path, run, task) = (h.env.borrow()["Path"].clone(), h.reg.borrow()["AgentDesk"].clone(), h.task.borrow().clone().unwrap());
        from_dotnet(&h, &c, true, false, true).unwrap();
        rollback(&h, &c, false).unwrap();
        assert_eq!(h.env.borrow()["Path"], path);
        assert!(!h.env.borrow().contains_key("AGENTDESK_PYTHON_BIN"));
        let reg = h.reg.borrow();
        assert_eq!(reg["AgentDesk"], run);
        assert!(!reg.contains_key("AgentDeskCore") && !reg.contains_key("AgentDeskTray") && reg.contains_key("OtherApp"));
        assert_eq!(h.task.borrow().as_ref().unwrap().arguments, task.arguments);
        assert!(h.count("stop-core") == 1 && !h.files.borrow().contains_key(&record_path(&c)));
        assert!(rollback(&h, &c, false).unwrap_err().to_string().contains("nothing to roll back"));
    }

    #[test]
    fn a_binary_run_from_elsewhere_copies_itself_beside_the_old_app_and_carries_on_from_there() {
        let mut c = ctx();
        c.exe = PathBuf::from(r"C:\Users\jo\Downloads\agentdesk.exe");
        let h = dotnet_machine(&c);
        from_dotnet(&h, &c, true, false, false).unwrap();
        let bin = r"C:\Users\jo\AppData\Local\AgentDeskApp\bin\agentdesk.exe";
        let copy = format!(r"copy C:\Users\jo\Downloads\agentdesk.exe -> {bin}");
        let relaunch = format!("relaunch {bin} install --from-dotnet");
        let order = ["summary", "backup-board", copy.as_str(), relaunch.as_str()];
        let at: Vec<usize> = order.iter().map(|p| h.pos(p)).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "wrong order: {at:?} in {:#?}", h.log.borrow());
        for later in ["kill ", "reg-delete", "migrate", "run schtasks", "reg-set", "start-core"] {
            assert_eq!(h.count(later), 0, "{later} is the detached run's job, not the first process's");
        }
    }

    #[test]
    fn leftover_daemons_are_ended_before_the_core_starts() {
        let c = ctx();
        let h = dotnet_machine(&c);
        from_dotnet(&h, &c, true, false, true).unwrap();
        let log = h.log.borrow();
        let last_stray = log.iter().rposition(|l| l == "kill-stray").unwrap();
        assert!(last_stray < log.iter().position(|l| l == "start-core").unwrap());
    }

    #[test]
    fn off_windows_it_is_a_clean_error_and_a_dry_run_changes_nothing() {
        let mut c = ctx();
        c.os = Os::Linux;
        assert!(from_dotnet(&Fake::default(), &c, true, false, true).unwrap_err().to_string().contains("only runs on Windows"));
        let c = ctx();
        let h = dotnet_machine(&c);
        let say = from_dotnet(&h, &c, true, true, true).unwrap();
        assert!(say[0].starts_with("dry run") && say.len() == 13);
        assert!(h.log.borrow().is_empty());
    }

    #[test]
    fn the_path_puts_the_install_folder_first_once_and_drops_the_old_app() {
        let old = r"C:\Users\jo\AppData\Local\AgentDeskApp";
        let p = fix_path(r"C:\Windows;c:\tools\agentdesk\;C:\Users\jo\AppData\Local\AgentDeskApp\current;C:\x;;", r"C:\tools\agentdesk", old);
        assert_eq!(p, r"C:\tools\agentdesk;C:\Windows;C:\x");
        assert_eq!(fix_path(&p, r"C:\tools\agentdesk", old), p, "idempotent");
    }
}
