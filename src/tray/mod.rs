//! `agentdesk tray`: the system tray (Windows), menu-bar (macOS) or StatusNotifier (Linux) icon. It is a client of the board like the
//! terminal window: it makes sure a core runs, shows whether it does and how many questions wait, and opens the window. Quitting the
//! tray never stops the core. It raises no toasts of its own: the core already toasts every new question, and two would be noise.
//!
//! `model` is all the decisions with no window in them; this file is the event loop and the board polling around it.

#![cfg_attr(not(feature = "tray"), allow(dead_code))]

pub mod model;

use anyhow::Result;
use model::{Action, Snapshot};

/// `agentdesk tray [--self-test]`.
pub fn run(self_test: bool) -> Result<()> {
    if self_test {
        println!("{}", serde_json::to_string_pretty(&model::self_test())?);
        return Ok(());
    }
    run_tray()
}

#[cfg(not(feature = "tray"))]
fn run_tray() -> Result<()> {
    eprintln!("agentdesk: this build has no tray (it was built with --no-default-features); use `agentdesk tui`");
    std::process::exit(1);
}

#[cfg(feature = "tray")]
fn run_tray() -> Result<()> {
    gui::run()
}

/// Opens the terminal window running `agentdesk tui`. Fire and forget: the terminal outlives the tray.
fn open_window() -> Result<()> {
    open_terminal(&["tui".to_string()])
}

/// Opens a terminal window running `agentdesk <args>` (`open 12` for a clicked toast). Fire and forget.
pub fn open_terminal(args: &[String]) -> Result<()> {
    let exe = std::env::current_exe()?.display().to_string();
    let over = std::env::var("AGENTDESK_TERMINAL").ok();
    let Some(cmd) = model::terminal_command_args(model::Os::current(), &exe, over.as_deref(), &model::which, args) else {
        anyhow::bail!("no terminal found (set AGENTDESK_TERMINAL to a terminal command line)");
    };
    let mut c = crate::paths::new_command(&cmd[0]); // no console flash on Windows
    c.args(&cmd[1..]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    c.spawn().map_err(|e| anyhow::anyhow!("could not start {}: {e}", cmd[0]))?;
    Ok(())
}

/// Starts `cmd` (program first) with no window and no stdio; fire and forget.
fn launch(cmd: &[String]) -> Result<()> {
    let mut c = crate::paths::new_command(&cmd[0]);
    c.args(&cmd[1..]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    c.spawn().map_err(|e| anyhow::anyhow!("could not start {}: {e}", cmd[0]))?;
    Ok(())
}

fn open_console() -> Result<()> {
    let Some(url) = crate::web::url() else { anyhow::bail!("the ops console is not running") };
    launch(&model::open_command(model::Os::current(), &url))
}

fn open_folder() -> Result<()> {
    let dir = crate::paths::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    launch(&model::open_command(model::Os::current(), &dir.display().to_string()))
}

/// The update sequence with its effects passed in, so the order and the failure paths are tested without a network or a core: install
/// the new build first; only when that worked stop the core, start it again from the new build, and start a new tray. A failed install
/// touches nothing (the running core and tray stay as they were, and `update::apply` has put the old binary back). Returns the version
/// installed and whether a new tray took over, in which case this one should quit.
fn install_and_restart(
    apply: impl FnOnce() -> Result<String>,
    stop_core: impl FnOnce(),
    start_core: impl FnOnce() -> Result<()>,
    start_tray: impl FnOnce() -> Result<()>,
) -> Result<(String, bool)> {
    let version = apply()?;
    stop_core();
    start_core().map_err(|e| anyhow::anyhow!("updated to {version} but the core did not restart: {e}"))?;
    let relaunched = match start_tray() {
        Ok(()) => true,
        Err(e) => {
            crate::paths::log("WARN", &format!("tray: updated to {version} but could not start the new tray: {e}"));
            false
        }
    };
    Ok((version, relaunched))
}

/// Progress of "install and restart", shared with the install thread.
const IDLE: u8 = 0;
const INSTALLING: u8 = 1;
const HANDED_OVER: u8 = 2;

/// The board as the menu needs it. `data_version` lets a second with no board writes cost one pragma, not two queries.
struct Poller {
    db: Option<crate::db::Db>,
    last_version: i64,
    last: Snapshot,
    install: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl Poller {
    fn new() -> Poller {
        Poller {
            db: None,
            last_version: -1,
            last: Snapshot { core_up: false, questions: vec![], concierge_on: false, console_up: false, update: None, updating: false },
            install: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(IDLE)),
        }
    }

    fn db(&mut self) -> Option<&crate::db::Db> {
        if self.db.is_none() {
            self.db = crate::db::Db::open(&crate::paths::db_path()).and_then(|d| d.init().map(|_| d)).ok();
        }
        self.db.as_ref()
    }

    /// The current snapshot; the previous one when the board cannot be read this time.
    fn poll(&mut self) -> Snapshot {
        let core_up = crate::daemon::running();
        let version = self.last_version;
        let fresh = self.db().map(|db| (db.data_version(), version));
        if let Some((v, old)) = fresh {
            if v != old {
                self.refresh(v);
            }
        }
        self.last.core_up = core_up;
        // not board data: two cheap local reads (a loopback connect, a small file), so they are taken every tick
        self.last.console_up = crate::web::url().is_some();
        self.last.update = crate::update::pending().map(|v| v.trim_start_matches('v').to_string());
        self.last.updating = self.install.load(std::sync::atomic::Ordering::SeqCst) == INSTALLING;
        self.last.clone()
    }

    /// True once an update was installed and a new tray started in this one's place: the loop should quit.
    fn handed_over(&self) -> bool {
        self.install.load(std::sync::atomic::Ordering::SeqCst) == HANDED_OVER
    }

    /// Install the newest release, then restart the core and the tray from it. Off the event loop thread (a download and a core wind-down).
    /// The path of the running program is taken before the swap: afterwards, on Windows, it may name the parked old build.
    fn install_update(&mut self) -> Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        let exe = std::env::current_exe()?;
        if self.install.compare_exchange(IDLE, INSTALLING, SeqCst, SeqCst).is_err() {
            return Ok(()); // already under way
        }
        let state = self.install.clone();
        std::thread::spawn(move || {
            let tray_exe = exe.clone();
            let core_exe = exe.clone();
            let result = install_and_restart(
                crate::update::apply,
                || {
                    if let Err(e) = crate::daemon::stop(std::time::Duration::from_secs(15)) {
                        crate::paths::log("WARN", &format!("tray: stopping the core for the update failed: {e}"));
                    }
                },
                move || crate::daemon::ensure_at(&core_exe, std::time::Duration::from_secs(15)).map(|_| ()),
                move || launch(&[tray_exe.display().to_string(), "tray".to_string()]),
            );
            match result {
                Ok((v, relaunched)) => {
                    crate::paths::log("INFO", &format!("tray: updated to {v}, core restarted"));
                    state.store(if relaunched { HANDED_OVER } else { IDLE }, SeqCst);
                }
                Err(e) => {
                    crate::paths::log("WARN", &format!("tray: update failed: {e}"));
                    state.store(IDLE, SeqCst);
                }
            }
        });
        Ok(())
    }

    fn refresh(&mut self, version: i64) {
        let Some(db) = self.db.as_ref() else { return };
        let (Ok(rows), Ok(c)) = (db.open_questions(false), crate::concierge::state(db)) else { return };
        self.last.questions = model::questions_from_rows(&rows);
        self.last.concierge_on = c["on"].as_bool().unwrap_or(false);
        self.last_version = version;
    }

    fn toggle_concierge(&mut self) -> Result<()> {
        let on = !self.last.concierge_on;
        let Some(db) = self.db() else { anyhow::bail!("the board is not readable") };
        crate::ui_api::call(db, &crate::identity::Caller::from_env(), "ui:concierge", &serde_json::json!({"on": on}))?;
        self.last_version = -1; // re-read on the next tick
        Ok(())
    }
}

/// What a click does. Failures are logged, never fatal: a tray that dies on a bad click is worse than one that ignores it.
fn perform(a: &Action, poller: &mut Poller) {
    let result = match a {
        Action::Open | Action::OpenQuestion(_) => open_window(), // the window has no thread argument: it opens on the list
        Action::ToggleConcierge => poller.toggle_concierge(),
        Action::OpenConsole => open_console(),
        Action::OpenFolder => open_folder(),
        Action::InstallUpdate => poller.install_update(),
        Action::StopCore => {
            // off the event loop thread: the core can take seconds to wind down
            std::thread::spawn(|| {
                if let Err(e) = crate::daemon::stop(std::time::Duration::from_secs(15)) {
                    crate::paths::log("WARN", &format!("tray: stopping the core failed: {e}"));
                }
            });
            Ok(())
        }
        Action::Quit | Action::None => Ok(()),
    };
    if let Err(e) = result {
        crate::paths::log("WARN", &format!("tray: {a:?} failed: {e}"));
    }
}

#[cfg(feature = "tray")]
mod gui {
    use super::*;
    use std::time::{Duration, Instant};
    use tao::event::{Event, StartCause};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    const TICK: Duration = Duration::from_secs(1);

    enum Wake {
        Menu(MenuEvent),
    }

    fn icon(s: &Snapshot) -> Result<Icon> {
        Icon::from_rgba(model::icon_rgba(model::icon_state(s), model::ICON_SIZE), model::ICON_SIZE, model::ICON_SIZE).map_err(|e| anyhow::anyhow!("icon: {e}"))
    }

    fn build_menu(s: &Snapshot) -> Result<Menu> {
        let menu = Menu::new();
        for e in model::menu(s) {
            if e.separator {
                menu.append(&PredefinedMenuItem::separator())?;
            } else {
                menu.append(&MenuItem::with_id(e.id.as_str(), &e.label, e.enabled, None))?;
            }
        }
        Ok(menu)
    }

    fn make_tray(s: &Snapshot) -> Result<TrayIcon> {
        TrayIconBuilder::new().with_menu(Box::new(build_menu(s)?)).with_tooltip(model::tooltip(s)).with_icon(icon(s)?).build().map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Without a display the toolkit panics or aborts; say so in one line instead.
    fn display_available() -> bool {
        !cfg!(all(unix, not(target_os = "macos")))
            || std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
            || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
    }

    /// Windows hides a new notification icon in the overflow ("^") until it is promoted: set `IsPromoted` to 1 on this program's entry under
    /// `HKCU\Control Panel\NotifyIconSettings\<id>` (which Windows writes, with `ExecutablePath`, once the icon is shown). Done with
    /// `reg.exe`, hidden, so no PowerShell starts; the entry is looked up by `model::promotable_keys`. The entry can take a while to
    /// appear, so it is retried for half a minute. Best effort; the user can still drag the icon out by hand.
    fn promote_icon() {
        #[cfg(windows)]
        std::thread::spawn(|| {
            let Ok(exe) = std::env::current_exe() else { return };
            let exe = exe.display().to_string();
            for _ in 0..15 {
                std::thread::sleep(Duration::from_secs(2));
                let out = crate::paths::new_command("reg").args(["query", r"HKCU\Control Panel\NotifyIconSettings", "/s", "/v", "ExecutablePath"]).output();
                let Ok(out) = out else { return };
                let keys = model::promotable_keys(&String::from_utf8_lossy(&out.stdout), &exe);
                let mut done = false;
                for k in &keys {
                    let set = crate::paths::new_command("reg").args(["add", k, "/v", "IsPromoted", "/t", "REG_DWORD", "/d", "1", "/f"]).output();
                    done |= set.map(|o| o.status.success()).unwrap_or(false);
                }
                if done {
                    return;
                }
            }
        });
    }

    fn fail(why: &str) -> ! {
        eprintln!("agentdesk: no system tray here ({why}); use `agentdesk tui`");
        std::process::exit(1);
    }

    pub fn run() -> Result<()> {
        if !display_available() {
            fail("no display: DISPLAY and WAYLAND_DISPLAY are unset");
        }
        // a tray with no core is a tray that says "down" forever: start it, but a failure to start is shown in the icon, not fatal
        if let Err(e) = crate::daemon::ensure(Duration::from_secs(15)) {
            crate::paths::log("WARN", &format!("tray: {e}"));
        }
        let built = std::panic::catch_unwind(|| EventLoopBuilder::<Wake>::with_user_event().build());
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let Ok(mut event_loop) = built
        else {
            fail("the display toolkit could not start")
        };
        #[cfg(target_os = "macos")]
        {
            use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
            event_loop.set_activation_policy(ActivationPolicy::Accessory); // a menu-bar item, no Dock icon
        }
        let proxy = event_loop.create_proxy();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = proxy.send_event(Wake::Menu(e));
        }));

        let mut poller = Poller::new();
        let mut shown: Option<Snapshot> = None;
        let mut tray: Option<TrayIcon> = None;
        let mut next = Instant::now();
        event_loop.run(move |event, _, flow| {
            *flow = ControlFlow::WaitUntil(next);
            match event {
                Event::UserEvent(Wake::Menu(e)) => {
                    let a = model::action(&e.id.0);
                    if a == Action::Quit {
                        tray.take(); // drop the icon first so it does not linger in the notification area
                        *flow = ControlFlow::Exit;
                    } else {
                        perform(&a, &mut poller);
                        shown = None; // redraw at once: the click probably changed something
                        next = Instant::now();
                    }
                }
                Event::NewEvents(StartCause::Init | StartCause::ResumeTimeReached { .. } | StartCause::WaitCancelled { .. }) if Instant::now() >= next => {
                    if poller.handed_over() {
                        tray.take(); // the updated tray has started: leave it the notification area
                        *flow = ControlFlow::Exit;
                        return;
                    }
                    let snap = poller.poll();
                    next = Instant::now() + TICK;
                    *flow = ControlFlow::WaitUntil(next);
                    if shown.as_ref() == Some(&snap) {
                        return;
                    }
                    let drawn = match tray.as_ref() {
                        None => make_tray(&snap).map(|t| {
                            tray = Some(t);
                            promote_icon();
                        }),
                        Some(t) => build_menu(&snap).and_then(|m| {
                            t.set_menu(Some(Box::new(m)));
                            t.set_icon(Some(icon(&snap)?)).map_err(|e| anyhow::anyhow!("{e}"))?;
                            t.set_tooltip(Some(model::tooltip(&snap))).map_err(|e| anyhow::anyhow!("{e}"))
                        }),
                    };
                    match drawn {
                        Ok(()) => shown = Some(snap),
                        Err(e) if tray.is_none() => fail(&format!("{e}")),
                        Err(e) => crate::paths::log("WARN", &format!("tray: redraw failed: {e}")),
                    }
                }
                _ => {}
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn steps(apply: Result<&str, &str>, core: bool, tray: bool) -> (Result<(String, bool)>, Vec<&'static str>) {
        let log = RefCell::new(Vec::new());
        let r = install_and_restart(
            || {
                log.borrow_mut().push("apply");
                apply.map(str::to_string).map_err(|e| anyhow::anyhow!("{e}"))
            },
            || log.borrow_mut().push("stop"),
            || {
                log.borrow_mut().push("core");
                if core {
                    Ok(())
                } else {
                    anyhow::bail!("no core")
                }
            },
            || {
                log.borrow_mut().push("tray");
                if tray {
                    Ok(())
                } else {
                    anyhow::bail!("no tray")
                }
            },
        );
        (r, log.into_inner())
    }

    #[test]
    fn a_good_update_installs_then_restarts_core_then_tray() {
        let (r, order) = steps(Ok("1.2.3"), true, true);
        assert_eq!(r.unwrap(), ("1.2.3".to_string(), true));
        assert_eq!(order, ["apply", "stop", "core", "tray"]);
    }

    #[test]
    fn a_failed_install_leaves_the_running_core_and_tray_alone() {
        let (r, order) = steps(Err("download failed"), true, true);
        assert!(r.unwrap_err().to_string().contains("download failed"));
        assert_eq!(order, ["apply"], "nothing is stopped when the new build did not install");
    }

    #[test]
    fn a_core_that_will_not_restart_is_reported_and_no_new_tray_is_started() {
        let (r, order) = steps(Ok("1.2.3"), false, true);
        let e = r.unwrap_err().to_string();
        assert!(e.contains("updated to 1.2.3") && e.contains("did not restart"), "{e}");
        assert_eq!(order, ["apply", "stop", "core"]);
    }

    #[test]
    fn a_tray_that_cannot_be_replaced_stays_up() {
        let (r, order) = steps(Ok("1.2.3"), true, false);
        assert_eq!(r.unwrap(), ("1.2.3".to_string(), false), "false: this tray must not quit");
        assert_eq!(order, ["apply", "stop", "core", "tray"]);
    }

    #[test]
    fn a_second_click_while_installing_is_ignored() {
        use std::sync::atomic::Ordering::SeqCst;
        let mut p = Poller::new();
        p.install.store(INSTALLING, SeqCst);
        assert!(p.install_update().is_ok());
        assert_eq!(p.install.load(SeqCst), INSTALLING, "no second install thread, state untouched");
        assert!(!p.handed_over());
        p.install.store(HANDED_OVER, SeqCst);
        assert!(p.handed_over());
    }
}
