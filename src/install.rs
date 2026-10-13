//! `agentdesk install` / `uninstall`: start the core (`agentdesk daemon`) and the tray (`agentdesk tray`) at login, on every OS.
//!
//! Every file and command is built by a pure function (so the escaping of a path with spaces is unit-tested), a `Plan` says what
//! would change against what is registered now, and a `Host` does the touching: the real one writes files, the HKCU Run key and runs
//! `launchctl` / `systemctl`; the tests hand in a fake. Running install twice changes nothing the second time, and uninstall removes
//! exactly the places install writes.
//!
//!   Windows  HKCU Run values AgentDeskCore and AgentDeskTray (hidden through `conhost.exe --headless`)
//!   macOS    ~/Library/LaunchAgents/com.vispero.agentdesk.{core,tray}.plist, loaded with `launchctl bootstrap gui/<uid>`
//!   Linux    ~/.config/systemd/user/agentdesk.service (`systemctl --user enable --now`) and an XDG autostart .desktop for the tray
//!            (and for the core too when `systemd --user` is not there)
//!
//! The move from the .NET app (`--from-dotnet`, `--rollback`) is in `install_dotnet.rs`.

use crate::{daemon, migrate, paths};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const RUN_CORE: &str = "AgentDeskCore";
pub const RUN_TRAY: &str = "AgentDeskTray";
pub const MAC_CORE: &str = "com.vispero.agentdesk.core";
pub const MAC_TRAY: &str = "com.vispero.agentdesk.tray";
pub const UNIT: &str = "agentdesk.service";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Windows,
    Mac,
    Linux,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Linux
        }
    }
    fn name(self) -> &'static str {
        match self {
            Os::Windows => "windows",
            Os::Mac => "macos",
            Os::Linux => "linux",
        }
    }
}

/// Everything the generators need to know about the machine; the tests build one by hand.
pub struct Ctx {
    pub os: Os,
    pub exe: PathBuf,
    pub home: PathBuf,
    /// The data folder (the macOS plists log into it, the rollback record lives in it).
    pub data: PathBuf,
    /// `$XDG_CONFIG_HOME` or `~/.config` (Linux).
    pub config: PathBuf,
    /// `%LOCALAPPDATA%` (Windows): where the .NET app lived.
    pub local: PathBuf,
    /// The numeric user id (macOS: `launchctl bootstrap gui/<uid>`).
    pub uid: String,
}

impl Ctx {
    pub fn detect() -> Result<Ctx> {
        let home = paths::home_dir();
        let config = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
        let uid = if cfg!(unix) {
            std::process::Command::new("id").arg("-u").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok(Ctx { os: Os::current(), exe: std::env::current_exe()?, data: paths::data_dir(), local: dirs::data_local_dir().unwrap_or_default(), config, home, uid })
    }
}

// ---- escaping, one function per format

/// A Windows command-line argument, quoted the way `CommandLineToArgvW` reads it back: backslashes only count before a quote.
pub fn win_quote(s: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in s.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                slashes = 0;
                out.push(c);
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// A systemd.service word: C-style escapes inside double quotes, and `%` / `$` doubled because systemd expands them.
pub fn systemd_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A .desktop `Exec` argument: quoted, with `"`, backtick, `$` and `\` backslash-escaped, then every backslash doubled (the file's own
/// string escaping), and `%` doubled so it is not a field code.
pub fn desktop_quote(s: &str) -> String {
    let mut arg = String::new();
    for c in s.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                arg.push('\\');
                arg.push(c);
            }
            '%' => arg.push_str("%%"),
            _ => arg.push(c),
        }
    }
    format!("\"{}\"", arg.replace('\\', "\\\\"))
}

// ---- the pure generators

/// The HKCU Run value: `conhost.exe --headless` runs a console program without a window.
pub fn run_value(exe: &str, arg: &str) -> String {
    format!("conhost.exe --headless {} {arg}", win_quote(exe))
}

pub fn plist(label: &str, exe: &str, arg: &str, log: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
\t<key>Label</key>\n\t<string>{}</string>\n\
\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}</string>\n\t\t<string>{}</string>\n\t</array>\n\
\t<key>RunAtLoad</key>\n\t<true/>\n\
\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n\
\t<key>StandardOutPath</key>\n\t<string>{}</string>\n\
\t<key>StandardErrorPath</key>\n\t<string>{}</string>\n\
</dict>\n\
</plist>\n",
        xml_escape(label),
        xml_escape(exe),
        xml_escape(arg),
        xml_escape(log),
        xml_escape(log)
    )
}

pub fn systemd_unit(exe: &str) -> String {
    format!(
        "[Unit]\nDescription=AgentDesk core: the board's one background process\n\n[Service]\nExecStart={} daemon\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        systemd_quote(exe)
    )
}

pub fn desktop_entry(name: &str, exe: &str, arg: &str) -> String {
    format!("[Desktop Entry]\nType=Application\nName={name}\nExec={} {arg}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n", desktop_quote(exe))
}

/// `schtasks` arguments that point an existing scheduled task at `<exe> backup`.
pub fn schtasks_change_args(task: &str, exe: &str) -> Vec<String> {
    ["/Change", "/TN", task, "/TR", &format!("{} backup", win_quote(exe))].iter().map(|s| s.to_string()).collect()
}

/// Reads `name`'s data out of `reg query KEY /v NAME` output.
pub fn parse_reg_query(out: &str, name: &str) -> Option<String> {
    for line in out.lines() {
        let Some(rest) = line.trim_start().strip_prefix(name) else { continue };
        let Some((ty, val)) = rest.trim_start().split_once(char::is_whitespace) else { continue };
        if ty.starts_with("REG_") && rest.starts_with(char::is_whitespace) {
            return Some(val.trim().to_string());
        }
    }
    None
}

// ---- what gets registered

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Place {
    /// A value of the HKCU Run key.
    Run(String),
    File(PathBuf),
}

impl Place {
    fn show(&self) -> String {
        match self {
            Place::Run(n) => format!("HKCU Run\\{n}"),
            Place::File(p) => p.display().to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    /// `core` or `tray`.
    pub name: &'static str,
    pub place: Place,
    pub content: String,
}

/// What install registers: the core, and the tray unless `tray` is false. `systemd` says whether `systemd --user` is there (Linux).
pub fn items(ctx: &Ctx, tray: bool, systemd: bool) -> Vec<Item> {
    let exe = ctx.exe.to_string_lossy().into_owned();
    let mut v = Vec::new();
    match ctx.os {
        Os::Windows => {
            v.push(Item { name: "core", place: Place::Run(RUN_CORE.into()), content: run_value(&exe, "daemon") });
            if tray {
                v.push(Item { name: "tray", place: Place::Run(RUN_TRAY.into()), content: run_value(&exe, "tray") });
            }
        }
        Os::Mac => {
            let dir = ctx.home.join("Library").join("LaunchAgents");
            let log = |n: &str| ctx.data.join(n).to_string_lossy().into_owned();
            v.push(Item { name: "core", place: Place::File(dir.join(format!("{MAC_CORE}.plist"))), content: plist(MAC_CORE, &exe, "daemon", &log("core.out.log")) });
            if tray {
                v.push(Item { name: "tray", place: Place::File(dir.join(format!("{MAC_TRAY}.plist"))), content: plist(MAC_TRAY, &exe, "tray", &log("tray.out.log")) });
            }
        }
        Os::Linux => {
            let autostart = ctx.config.join("autostart");
            if systemd {
                v.push(Item { name: "core", place: Place::File(ctx.config.join("systemd").join("user").join(UNIT)), content: systemd_unit(&exe) });
            } else {
                v.push(Item { name: "core", place: Place::File(autostart.join("agentdesk-core.desktop")), content: desktop_entry("AgentDesk core", &exe, "daemon") });
            }
            if tray {
                v.push(Item { name: "tray", place: Place::File(autostart.join("agentdesk-tray.desktop")), content: desktop_entry("AgentDesk tray", &exe, "tray") });
            }
        }
    }
    v
}

/// Every place install can have written (both Linux variants), for status and uninstall.
pub fn candidates(ctx: &Ctx) -> Vec<Item> {
    let mut all = items(ctx, true, true);
    for i in items(ctx, true, false) {
        if !all.iter().any(|a| a.place == i.place) {
            all.push(i);
        }
    }
    all
}

// ---- the executor

pub struct Out {
    pub ok: bool,
    pub stdout: String,
}

#[derive(Clone, Debug)]
pub struct TaskAction {
    pub execute: String,
    pub arguments: String,
    pub dir: String,
}

/// Everything that touches the machine. `SysHost` is the real one; the tests use `fake::Fake`.
pub trait Host {
    fn file_read(&self, p: &Path) -> Option<String>;
    fn file_write(&self, p: &Path, content: &str) -> Result<()>;
    fn file_remove(&self, p: &Path) -> Result<()>;
    fn exists(&self, p: &Path) -> bool;
    fn reg_get(&self, name: &str) -> Option<String>;
    fn reg_set(&self, name: &str, value: &str) -> Result<()>;
    fn reg_delete(&self, name: &str) -> Result<()>;
    fn run(&self, prog: &str, args: &[String]) -> Result<Out>;
    // the move from the .NET app
    fn process_running(&self, image: &str) -> bool;
    fn kill_process(&self, image: &str);
    fn sleep(&self, d: Duration);
    fn user_env_get(&self, name: &str) -> Option<String>;
    fn user_env_set(&self, name: &str, value: Option<&str>) -> Result<()>;
    fn task_get(&self, name: &str) -> Option<TaskAction>;
    fn task_restore(&self, name: &str, t: &TaskAction) -> Result<()>;
    fn migrate(&self) -> Result<String>;
    fn start_core(&self) -> Result<()>;
    fn stop_core(&self) -> Result<()>;
}

fn read_place(h: &dyn Host, p: &Place) -> Option<String> {
    match p {
        Place::Run(n) => h.reg_get(n),
        Place::File(f) => h.file_read(f),
    }
}

fn write_place(h: &dyn Host, i: &Item) -> Result<()> {
    match &i.place {
        Place::Run(n) => h.reg_set(n, &i.content),
        Place::File(f) => h.file_write(f, &i.content),
    }
}

fn remove_place(h: &dyn Host, p: &Place) -> Result<()> {
    match p {
        Place::Run(n) => h.reg_delete(n),
        Place::File(f) => h.file_remove(f),
    }
}

// ---- the plan

#[derive(Clone, Debug)]
pub struct Cmd {
    pub prog: String,
    pub args: Vec<String>,
    /// A failure is fine (booting out something that was never loaded).
    pub tolerate: bool,
}

impl Cmd {
    fn new(prog: &str, args: &[&str], tolerate: bool) -> Cmd {
        Cmd { prog: prog.into(), args: args.iter().map(|s| s.to_string()).collect(), tolerate }
    }
    fn show(&self) -> String {
        std::iter::once(self.prog.as_str()).chain(self.args.iter().map(|s| s.as_str())).collect::<Vec<_>>().join(" ")
    }
}

#[derive(Default)]
pub struct Plan {
    /// To write: missing or stale. The bool is whether something was there (an update rather than an add).
    pub writes: Vec<(Item, bool)>,
    pub unchanged: Vec<Item>,
    pub removes: Vec<Item>,
    pub before: Vec<Cmd>,
    pub after: Vec<Cmd>,
}

fn systemd_available(h: &dyn Host) -> bool {
    h.run("systemctl", &["--user".to_string(), "show-environment".to_string()]).map(|o| o.ok).unwrap_or(false)
}

/// What to do to go from what is registered to `want`; everything else install could have written is removed.
fn build(h: &dyn Host, ctx: &Ctx, want: Vec<Item>) -> Plan {
    let mut p = Plan::default();
    for w in &want {
        match read_place(h, &w.place) {
            Some(cur) if cur == w.content => p.unchanged.push(w.clone()),
            cur => p.writes.push((w.clone(), cur.is_some())),
        }
    }
    for c in candidates(ctx) {
        if !want.iter().any(|w| w.place == c.place) && read_place(h, &c.place).is_some() {
            p.removes.push(c);
        }
    }
    commands(ctx, &mut p);
    p
}

pub fn plan_install(h: &dyn Host, ctx: &Ctx, tray: bool) -> Plan {
    let systemd = ctx.os == Os::Linux && systemd_available(h);
    build(h, ctx, items(ctx, tray, systemd))
}

pub fn plan_uninstall(h: &dyn Host, ctx: &Ctx) -> Plan {
    build(h, ctx, Vec::new())
}

/// The load / unload commands a change needs: launchd and systemd have to be told, the Run key and autostart folders are read at login.
fn commands(ctx: &Ctx, p: &mut Plan) {
    let file_of = |i: &Item| if let Place::File(f) = &i.place { Some(f.clone()) } else { None };
    match ctx.os {
        Os::Mac => {
            let domain = format!("gui/{}", ctx.uid);
            let label = |f: &Path| f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            for f in p.writes.iter().filter_map(|(i, _)| file_of(i)).chain(p.removes.iter().filter_map(file_of)) {
                p.before.push(Cmd::new("launchctl", &["bootout", &format!("{domain}/{}", label(&f))], true));
            }
            for f in p.writes.iter().filter_map(|(i, _)| file_of(i)) {
                p.after.push(Cmd::new("launchctl", &["bootstrap", &domain, &f.to_string_lossy()], false));
            }
        }
        Os::Linux => {
            let is_unit = |i: &Item| file_of(i).and_then(|f| f.file_name().map(|n| n == UNIT)).unwrap_or(false);
            if p.writes.iter().any(|(i, _)| is_unit(i)) {
                p.after.push(Cmd::new("systemctl", &["--user", "daemon-reload"], false));
                p.after.push(Cmd::new("systemctl", &["--user", "enable", "--now", UNIT], false));
            }
            if p.removes.iter().any(is_unit) {
                p.before.push(Cmd::new("systemctl", &["--user", "disable", "--now", UNIT], true));
                p.after.push(Cmd::new("systemctl", &["--user", "daemon-reload"], true));
            }
        }
        Os::Windows => {}
    }
}

fn exec(h: &dyn Host, c: &Cmd) -> Result<()> {
    match h.run(&c.prog, &c.args) {
        Ok(o) if o.ok => Ok(()),
        _ if c.tolerate => Ok(()),
        Ok(_) => Err(anyhow!("`{}` failed", c.show())),
        Err(e) => Err(anyhow!("`{}` failed: {e}", c.show())),
    }
}

pub fn apply(h: &dyn Host, p: &Plan) -> Result<()> {
    for c in &p.before {
        exec(h, c)?;
    }
    for r in &p.removes {
        remove_place(h, &r.place)?;
    }
    for (i, _) in &p.writes {
        write_place(h, i)?;
    }
    for c in &p.after {
        exec(h, c)?;
    }
    Ok(())
}

pub fn plan_json(ctx: &Ctx, p: &Plan, dry_run: bool) -> Value {
    let mut changes: Vec<Value> =
        p.writes.iter().map(|(i, existed)| json!({"name": i.name, "target": i.place.show(), "action": if *existed {"update"} else {"add"}, "content": i.content})).collect();
    changes.extend(p.removes.iter().map(|i| json!({"name": i.name, "target": i.place.show(), "action": "remove"})));
    let commands: Vec<String> = p.before.iter().chain(p.after.iter()).map(Cmd::show).collect();
    json!({
        "dry_run": dry_run,
        "os": ctx.os.name(),
        "exe": ctx.exe.to_string_lossy(),
        "changes": changes,
        "unchanged": p.unchanged.iter().map(|i| json!({"name": i.name, "target": i.place.show()})).collect::<Vec<_>>(),
        "commands": commands,
    })
}

/// Plans and, unless `dry_run`, applies.
pub fn install(h: &dyn Host, ctx: &Ctx, tray: bool, dry_run: bool) -> Result<Value> {
    let p = plan_install(h, ctx, tray);
    if !dry_run {
        if ctx.exe.to_string_lossy().contains('"') {
            bail!("the program path {} has a quote in it, which no startup format can carry", ctx.exe.display());
        }
        apply(h, &p)?;
    }
    Ok(plan_json(ctx, &p, dry_run))
}

pub fn uninstall(h: &dyn Host, ctx: &Ctx, dry_run: bool) -> Result<Value> {
    let p = plan_uninstall(h, ctx);
    if !dry_run {
        apply(h, &p)?;
    }
    Ok(plan_json(ctx, &p, dry_run))
}

/// `install --status`: what is registered, and whether it still points at this binary.
pub fn status(h: &dyn Host, ctx: &Ctx) -> Value {
    let wanted: Vec<Item> = items(ctx, true, true).into_iter().chain(items(ctx, true, false)).collect();
    let mut registered = Vec::new();
    let mut core = false;
    for c in candidates(ctx) {
        let state = match read_place(h, &c.place) {
            None => "missing",
            Some(cur) if wanted.iter().any(|w| w.place == c.place && w.content == cur) => "current",
            Some(_) => "stale",
        };
        core |= c.name == "core" && state != "missing";
        registered.push(json!({"name": c.name, "target": c.place.show(), "state": state}));
    }
    json!({"os": ctx.os.name(), "exe": ctx.exe.to_string_lossy(), "installed": core, "registered": registered})
}

// ---- the real host

pub struct SysHost;

fn run_key() -> String {
    std::env::var("AGENTDESK_RUN_KEY").ok().filter(|k| !k.is_empty()).unwrap_or_else(|| r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run".into())
}

fn command_out(mut c: std::process::Command) -> Result<Out> {
    let o = c.output().map_err(|e| anyhow!("could not run {:?}: {e}", c.get_program()))?;
    Ok(Out { ok: o.status.success(), stdout: String::from_utf8_lossy(&o.stdout).into_owned() })
}

/// PowerShell with the values passed as environment variables, so no quoting of a path or a PATH can go wrong.
fn powershell(script: &str, env: &[(&str, &str)]) -> Result<Out> {
    let mut c = std::process::Command::new("powershell.exe");
    c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script]);
    for (k, v) in env {
        c.env(k, v);
    }
    command_out(c)
}

impl Host for SysHost {
    fn file_read(&self, p: &Path) -> Option<String> {
        std::fs::read_to_string(p).ok()
    }
    fn file_write(&self, p: &Path, content: &str) -> Result<()> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, content).map_err(|e| anyhow!("could not write {}: {e}", p.display()))
    }
    fn file_remove(&self, p: &Path) -> Result<()> {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(anyhow!("could not remove {}: {e}", p.display())),
            _ => Ok(()),
        }
    }
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }
    fn reg_get(&self, name: &str) -> Option<String> {
        let mut c = std::process::Command::new("reg");
        c.args(["query", &run_key(), "/v", name]);
        command_out(c).ok().filter(|o| o.ok).and_then(|o| parse_reg_query(&o.stdout, name))
    }
    fn reg_set(&self, name: &str, value: &str) -> Result<()> {
        let mut c = std::process::Command::new("reg");
        c.args(["add", &run_key(), "/v", name, "/t", "REG_SZ", "/d", value, "/f"]);
        if command_out(c)?.ok {
            Ok(())
        } else {
            bail!("reg add {name} failed")
        }
    }
    fn reg_delete(&self, name: &str) -> Result<()> {
        if self.reg_get(name).is_none() {
            return Ok(());
        }
        let mut c = std::process::Command::new("reg");
        c.args(["delete", &run_key(), "/v", name, "/f"]);
        if command_out(c)?.ok {
            Ok(())
        } else {
            bail!("reg delete {name} failed")
        }
    }
    fn run(&self, prog: &str, args: &[String]) -> Result<Out> {
        let mut c = std::process::Command::new(prog);
        c.args(args);
        command_out(c)
    }
    fn process_running(&self, image: &str) -> bool {
        let mut c = std::process::Command::new("tasklist");
        c.args(["/FI", &format!("IMAGENAME eq {image}"), "/NH"]);
        command_out(c).map(|o| o.stdout.to_lowercase().contains(&image.to_lowercase())).unwrap_or(false)
    }
    fn kill_process(&self, image: &str) {
        let mut c = std::process::Command::new("taskkill");
        c.args(["/F", "/T", "/IM", image]);
        let _ = command_out(c);
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
    fn user_env_get(&self, name: &str) -> Option<String> {
        let o = powershell("[Environment]::GetEnvironmentVariable($env:AD_NAME, 'User')", &[("AD_NAME", name)]).ok().filter(|o| o.ok)?;
        Some(o.stdout.trim_end_matches(['\r', '\n']).to_string()).filter(|v| !v.is_empty())
    }
    fn user_env_set(&self, name: &str, value: Option<&str>) -> Result<()> {
        let script = "$v = if ($env:AD_UNSET -eq '1') { $null } else { $env:AD_VAL }; [Environment]::SetEnvironmentVariable($env:AD_NAME, $v, 'User')";
        let unset = if value.is_none() { "1" } else { "0" };
        if powershell(script, &[("AD_NAME", name), ("AD_VAL", value.unwrap_or("")), ("AD_UNSET", unset)])?.ok {
            Ok(())
        } else {
            bail!("could not set the user environment variable {name}")
        }
    }
    fn task_get(&self, name: &str) -> Option<TaskAction> {
        let script = "$t = Get-ScheduledTask -TaskName $env:AD_TASK -ErrorAction SilentlyContinue; if ($t) { $a = $t.Actions | Select-Object -First 1; @{execute=[string]$a.Execute; arguments=[string]$a.Arguments; dir=[string]$a.WorkingDirectory} | ConvertTo-Json -Compress }";
        let o = powershell(script, &[("AD_TASK", name)]).ok().filter(|o| o.ok)?;
        let v: Value = serde_json::from_str(o.stdout.trim()).ok()?;
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        Some(TaskAction { execute: s("execute"), arguments: s("arguments"), dir: s("dir") })
    }
    fn task_restore(&self, name: &str, t: &TaskAction) -> Result<()> {
        let script = "$p = @{ Execute = $env:AD_EXEC }; if ($env:AD_ARGS) { $p.Argument = $env:AD_ARGS }; if ($env:AD_DIR) { $p.WorkingDirectory = $env:AD_DIR }; $a = New-ScheduledTaskAction @p; Set-ScheduledTask -TaskName $env:AD_TASK -Action $a | Out-Null";
        if powershell(script, &[("AD_TASK", name), ("AD_EXEC", &t.execute), ("AD_ARGS", &t.arguments), ("AD_DIR", &t.dir)])?.ok {
            Ok(())
        } else {
            bail!("could not restore the scheduled task {name}")
        }
    }
    fn migrate(&self) -> Result<String> {
        migrate::run(true, &paths::home_dir().join(".claude.json"), &paths::claude_settings())
    }
    fn start_core(&self) -> Result<()> {
        daemon::ensure(Duration::from_secs(15)).map(|_| ())
    }
    fn stop_core(&self) -> Result<()> {
        daemon::stop(Duration::from_secs(10)).map(|_| ())
    }
}

/// The `agentdesk install` / `uninstall` entry points.
pub fn cmd_install(no_tray: bool, from_dotnet: bool, dry_run: bool, status_only: bool, rollback: bool) -> Result<()> {
    let (h, ctx) = (SysHost, Ctx::detect()?);
    if status_only {
        println!("{}", serde_json::to_string_pretty(&status(&h, &ctx))?);
    } else if rollback {
        crate::install_dotnet::rollback(&h, &ctx, dry_run)?.iter().for_each(|l| println!("{l}"));
    } else if from_dotnet {
        crate::install_dotnet::from_dotnet(&h, &ctx, !no_tray, dry_run)?.iter().for_each(|l| println!("{l}"));
    } else {
        println!("{}", serde_json::to_string_pretty(&install(&h, &ctx, !no_tray, dry_run)?)?);
    }
    Ok(())
}

pub fn cmd_uninstall(dry_run: bool) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&uninstall(&SysHost, &Ctx::detect()?, dry_run)?)?);
    Ok(())
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, BTreeSet, HashMap};

    /// A machine in memory (or, with `disk`, files in a scratch folder): records every call in `log`.
    #[derive(Default)]
    pub struct Fake {
        pub disk: bool,
        pub files: RefCell<BTreeMap<PathBuf, String>>,
        pub present: RefCell<BTreeSet<PathBuf>>,
        pub reg: RefCell<BTreeMap<String, String>>,
        pub env: RefCell<HashMap<String, String>>,
        pub task: RefCell<Option<TaskAction>>,
        pub running: RefCell<BTreeSet<String>>,
        /// Every sleep brings the old window back, like a watchdog would.
        pub respawn: Cell<bool>,
        pub systemd: Cell<bool>,
        pub log: RefCell<Vec<String>>,
    }

    impl Fake {
        pub fn note(&self, s: String) {
            self.log.borrow_mut().push(s);
        }
        /// A path built with `Path::join` on Linux has `/` where the Windows fixtures write `\`: compare them alike.
        fn norm(s: &str) -> String {
            s.replace('\', "/")
        }
        pub fn pos(&self, prefix: &str) -> usize {
            let want = Self::norm(prefix);
            self.log.borrow().iter().position(|l| Self::norm(l).starts_with(&want)).unwrap_or_else(|| panic!("no `{prefix}` in {:#?}", self.log.borrow()))
        }
        pub fn count(&self, prefix: &str) -> usize {
            let want = Self::norm(prefix);
            self.log.borrow().iter().filter(|l| Self::norm(l).starts_with(&want)).count()
        }
        pub fn mutations(&self) -> usize {
            self.log.borrow().iter().filter(|l| !l.starts_with("read") && !l.starts_with("run systemctl --user show-environment")).count()
        }
    }

    impl Host for Fake {
        fn file_read(&self, p: &Path) -> Option<String> {
            if self.disk {
                return std::fs::read_to_string(p).ok();
            }
            self.files.borrow().get(p).cloned()
        }
        fn file_write(&self, p: &Path, c: &str) -> Result<()> {
            self.note(format!("write {}", p.display()));
            if self.disk {
                std::fs::create_dir_all(p.parent().unwrap())?;
                std::fs::write(p, c)?;
            } else {
                self.files.borrow_mut().insert(p.to_path_buf(), c.to_string());
            }
            Ok(())
        }
        fn file_remove(&self, p: &Path) -> Result<()> {
            self.note(format!("remove {}", p.display()));
            if self.disk {
                let _ = std::fs::remove_file(p);
            } else {
                self.files.borrow_mut().remove(p);
            }
            Ok(())
        }
        fn exists(&self, p: &Path) -> bool {
            self.present.borrow().contains(p) || self.files.borrow().contains_key(p) || (self.disk && p.exists())
        }
        fn reg_get(&self, name: &str) -> Option<String> {
            self.reg.borrow().get(name).cloned()
        }
        fn reg_set(&self, name: &str, v: &str) -> Result<()> {
            self.note(format!("reg-set {name}"));
            self.reg.borrow_mut().insert(name.into(), v.into());
            Ok(())
        }
        fn reg_delete(&self, name: &str) -> Result<()> {
            self.note(format!("reg-delete {name}"));
            self.reg.borrow_mut().remove(name);
            Ok(())
        }
        fn run(&self, prog: &str, args: &[String]) -> Result<Out> {
            let line = format!("run {prog} {}", args.join(" "));
            self.note(line.clone());
            Ok(Out { ok: !(prog == "systemctl" && line.contains("show-environment")) || self.systemd.get(), stdout: String::new() })
        }
        fn process_running(&self, image: &str) -> bool {
            self.running.borrow().contains(image)
        }
        fn kill_process(&self, image: &str) {
            self.note(format!("kill {image}"));
            self.running.borrow_mut().remove(image);
        }
        fn sleep(&self, _: Duration) {
            self.note("sleep".into());
            if self.respawn.get() {
                self.running.borrow_mut().insert("AgentDesk.App.exe".into());
            }
        }
        fn user_env_get(&self, name: &str) -> Option<String> {
            self.env.borrow().get(name).cloned()
        }
        fn user_env_set(&self, name: &str, v: Option<&str>) -> Result<()> {
            self.note(format!("env-set {name}"));
            match v {
                Some(v) => self.env.borrow_mut().insert(name.into(), v.into()),
                None => self.env.borrow_mut().remove(name),
            };
            Ok(())
        }
        fn task_get(&self, _: &str) -> Option<TaskAction> {
            self.task.borrow().clone()
        }
        fn task_restore(&self, _: &str, t: &TaskAction) -> Result<()> {
            self.note("task-restore".into());
            *self.task.borrow_mut() = Some(t.clone());
            Ok(())
        }
        fn migrate(&self) -> Result<String> {
            self.note("migrate".into());
            Ok(String::new())
        }
        fn start_core(&self) -> Result<()> {
            self.note("start-core".into());
            Ok(())
        }
        fn stop_core(&self) -> Result<()> {
            self.note("stop-core".into());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Fake;
    use super::*;

    fn ctx(os: Os, root: &Path) -> Ctx {
        let (exe, home) = match os {
            Os::Windows => (PathBuf::from(r"C:\Users\Jo Doe\AppData\Local\Programs\agent desk\agentdesk.exe"), PathBuf::from(r"C:\Users\Jo Doe")),
            _ => (root.join("My Apps").join("agentdesk"), root.join("home")),
        };
        Ctx { os, exe, data: root.join("data"), config: home.join(".config"), local: root.join("local"), home, uid: "501".into() }
    }

    #[test]
    fn quoting_follows_each_formats_rules() {
        assert_eq!(win_quote(r"C:\Program Files\a b\x.exe"), r#""C:\Program Files\a b\x.exe""#);
        assert_eq!(win_quote(r"C:\dir\"), r#""C:\dir\\""#, "a trailing backslash must not eat the closing quote");
        assert_eq!(win_quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(xml_escape(r#"a&b<c>"d'"#), "a&amp;b&lt;c&gt;&quot;d&apos;");
        assert_eq!(systemd_quote(r#"/opt/my "app"/50%$x"#), r#""/opt/my \"app\"/50%%$$x""#);
        assert_eq!(desktop_quote("/opt/my app/x"), "\"/opt/my app/x\"");
        assert_eq!(desktop_quote(r#"a"b$c`d\e%f"#), r#""a\\"b\\$c\\`d\\\\e%%f""#);
    }

    #[test]
    fn the_windows_run_values_hide_the_console() {
        let c = ctx(Os::Windows, Path::new("/x"));
        let v = items(&c, true, false);
        assert_eq!(v[0].content, r#"conhost.exe --headless "C:\Users\Jo Doe\AppData\Local\Programs\agent desk\agentdesk.exe" daemon"#);
        assert_eq!(v[1].content, r#"conhost.exe --headless "C:\Users\Jo Doe\AppData\Local\Programs\agent desk\agentdesk.exe" tray"#);
        assert_eq!(v[0].place, Place::Run("AgentDeskCore".into()));
        assert_eq!(items(&c, false, false).len(), 1, "--no-tray");
    }

    #[test]
    fn the_plist_is_valid_xml_with_the_escaped_path() {
        let p = plist(MAC_CORE, "/Apps/R&D <x>/agentdesk", "daemon", "/data/core.out.log");
        assert!(p.contains("<string>/Apps/R&amp;D &lt;x&gt;/agentdesk</string>") && p.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(p.contains("<key>SuccessfulExit</key>") && p.contains("<key>StandardOutPath</key>\n\t<string>/data/core.out.log</string>"));
        assert!(p.contains("<string>com.vispero.agentdesk.core</string>"));
    }

    #[test]
    fn the_systemd_unit_and_desktop_file() {
        let u = systemd_unit("/opt/my app/agentdesk");
        assert!(u.contains("ExecStart=\"/opt/my app/agentdesk\" daemon\n") && u.contains("Restart=on-failure") && u.contains("WantedBy=default.target"));
        let d = desktop_entry("AgentDesk tray", "/opt/my app/agentdesk", "tray");
        assert!(d.starts_with("[Desktop Entry]\n") && d.contains("Exec=\"/opt/my app/agentdesk\" tray\n") && d.contains("Terminal=false"));
    }

    #[test]
    fn the_schtasks_command_points_the_task_at_backup() {
        let a = schtasks_change_args("AgentDesk-Backup", r"C:\Users\Jo Doe\agentdesk.exe");
        assert_eq!(a, ["/Change", "/TN", "AgentDesk-Backup", "/TR", r#""C:\Users\Jo Doe\agentdesk.exe" backup"#]);
    }

    #[test]
    fn reg_query_output_is_parsed_exactly() {
        let out = "\r\nHKEY_CURRENT_USER\\Software\\X\\Run\r\n    AgentDeskCore    REG_SZ    conhost.exe --headless \"C:\\a b\\agentdesk.exe\" daemon\r\n    AgentDesk    REG_SZ    old\r\n\r\n";
        assert_eq!(parse_reg_query(out, "AgentDeskCore").unwrap(), "conhost.exe --headless \"C:\\a b\\agentdesk.exe\" daemon");
        assert_eq!(parse_reg_query(out, "AgentDesk").unwrap(), "old", "AgentDesk is not a prefix match of AgentDeskCore");
        assert!(parse_reg_query(out, "Agent").is_none());
    }

    #[test]
    fn windows_install_twice_changes_nothing_and_uninstall_leaves_nothing() {
        let c = ctx(Os::Windows, Path::new("/x"));
        let h = Fake::default();
        h.reg.borrow_mut().insert("OtherApp".into(), "keep me".into());
        let first = install(&h, &c, true, false).unwrap();
        assert_eq!(first["changes"].as_array().unwrap().len(), 2);
        assert_eq!(h.reg.borrow().len(), 3);
        let before = h.log.borrow().len();
        let second = install(&h, &c, true, false).unwrap();
        assert!(second["changes"].as_array().unwrap().is_empty() && h.log.borrow().len() == before, "nothing written the second time");
        assert_eq!(status(&h, &c)["installed"], true);
        uninstall(&h, &c, false).unwrap();
        assert_eq!(h.reg.borrow().keys().collect::<Vec<_>>(), ["OtherApp"], "only what install added is gone");
        assert_eq!(status(&h, &c)["installed"], false);
    }

    #[test]
    fn dry_run_touches_nothing_and_no_tray_removes_a_registered_tray() {
        let c = ctx(Os::Windows, Path::new("/x"));
        let h = Fake::default();
        let d = install(&h, &c, true, true).unwrap();
        assert_eq!(d["dry_run"], true);
        assert!(h.reg.borrow().is_empty() && h.mutations() == 0);
        install(&h, &c, true, false).unwrap();
        install(&h, &c, false, false).unwrap();
        assert_eq!(h.reg.borrow().keys().collect::<Vec<_>>(), ["AgentDeskCore"]);
    }

    #[test]
    fn a_moved_binary_is_an_update_not_a_second_entry() {
        let mut c = ctx(Os::Windows, Path::new("/x"));
        let h = Fake::default();
        install(&h, &c, true, false).unwrap();
        c.exe = PathBuf::from(r"D:\new\agentdesk.exe");
        assert_eq!(status(&h, &c)["registered"][0]["state"], "stale");
        let r = install(&h, &c, true, false).unwrap();
        assert_eq!(r["changes"][0]["action"], "update");
        assert_eq!(h.reg.borrow().len(), 2);
        assert!(h.reg.borrow()[RUN_CORE].contains(r"D:\new\agentdesk.exe"));
    }

    #[test]
    fn linux_with_systemd_writes_a_unit_and_enables_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(Os::Linux, dir.path());
        let h = Fake { disk: true, ..Default::default() };
        h.systemd.set(true);
        let r = install(&h, &c, true, false).unwrap();
        let unit = c.config.join("systemd/user/agentdesk.service");
        assert!(std::fs::read_to_string(&unit).unwrap().contains("agentdesk\" daemon"));
        assert!(c.config.join("autostart/agentdesk-tray.desktop").exists() && !c.config.join("autostart/agentdesk-core.desktop").exists());
        assert_eq!(r["commands"], json!(["systemctl --user daemon-reload", "systemctl --user enable --now agentdesk.service"]));
        let n = h.count("run systemctl --user enable");
        install(&h, &c, true, false).unwrap();
        assert_eq!(h.count("run systemctl --user enable"), n, "the second install does not enable again");
        let u = uninstall(&h, &c, false).unwrap();
        assert_eq!(u["commands"][0], "systemctl --user disable --now agentdesk.service");
        assert!(!unit.exists() && !c.config.join("autostart/agentdesk-tray.desktop").exists());
    }

    #[test]
    fn linux_without_systemd_autostarts_the_core_from_a_desktop_file() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(Os::Linux, dir.path());
        let h = Fake { disk: true, ..Default::default() };
        let r = install(&h, &c, true, false).unwrap();
        assert!(c.config.join("autostart/agentdesk-core.desktop").exists() && !c.config.join("systemd").exists());
        assert!(r["commands"].as_array().unwrap().is_empty());
        let core = std::fs::read_to_string(c.config.join("autostart/agentdesk-core.desktop")).unwrap();
        assert!(core.contains("My Apps") && core.contains("agentdesk\" daemon"));
        uninstall(&h, &c, false).unwrap();
        assert!(!c.config.join("autostart/agentdesk-core.desktop").exists());
    }

    #[test]
    fn macos_writes_two_launch_agents_and_bootstraps_them_once() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(Os::Mac, dir.path());
        let h = Fake { disk: true, ..Default::default() };
        let r = install(&h, &c, true, false).unwrap();
        let agents = c.home.join("Library/LaunchAgents");
        assert!(agents.join("com.vispero.agentdesk.core.plist").exists() && agents.join("com.vispero.agentdesk.tray.plist").exists());
        let cmds = r["commands"].as_array().unwrap();
        assert!(cmds.iter().any(|c| c.as_str().unwrap().starts_with("launchctl bootstrap gui/501 ") && c.as_str().unwrap().ends_with("com.vispero.agentdesk.core.plist")));
        assert!(cmds.iter().any(|c| c == "launchctl bootout gui/501/com.vispero.agentdesk.tray"));
        let n = h.count("run launchctl");
        install(&h, &c, true, false).unwrap();
        assert_eq!(h.count("run launchctl"), n);
        uninstall(&h, &c, false).unwrap();
        assert!(std::fs::read_dir(&agents).unwrap().next().is_none());
        assert!(h.count("run launchctl bootout") >= 4);
    }
}
