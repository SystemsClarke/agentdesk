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
    let mut c = std::process::Command::new(&cmd[0]);
    c.args(&cmd[1..]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no flash of the cmd that runs `start`
    }
    c.spawn().map_err(|e| anyhow::anyhow!("could not start {}: {e}", cmd[0]))?;
    Ok(())
}

/// The board as the menu needs it. `data_version` lets a second with no board writes cost one pragma, not two queries.
struct Poller {
    db: Option<crate::db::Db>,
    last_version: i64,
    last: Snapshot,
}

impl Poller {
    fn new() -> Poller {
        Poller { db: None, last_version: -1, last: Snapshot { core_up: false, questions: vec![], concierge_on: false } }
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
        self.last.clone()
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

    /// Windows hides a new notification icon in the overflow ("^") until it is promoted: set IsPromoted on this program's entry, a moment
    /// after the icon is made (Windows writes the entry then). Best effort; the user can still drag it out by hand.
    fn promote_icon() {
        #[cfg(windows)]
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(3));
            let script = r"$me = $env:AD_EXE; Get-ChildItem 'HKCU:\Control Panel\NotifyIconSettings' -ErrorAction SilentlyContinue | Where-Object { (Get-ItemProperty $_.PSPath).ExecutablePath -eq $me } | ForEach-Object { Set-ItemProperty $_.PSPath -Name IsPromoted -Value 1 -Type DWord }";
            if let Ok(exe) = std::env::current_exe() {
                let _ = crate::paths::new_command("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", script]).env("AD_EXE", exe).output();
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
