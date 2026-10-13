//! agentdesk: a message board between you and the AI agents running on this machine.
//!
//!   agentdesk                      a terminal: the window (TUI); piped (no tty): the MCP server on stdio
//!   agentdesk mcp                  the MCP server on stdio (how every Claude Code session reaches the board)
//!   agentdesk hook <event>         Claude Code hook: stdin JSON in, hook JSON out (never fails the session)
//!   agentdesk wait <thread-id>     block until John replies on the thread, then print his reply
//!   agentdesk tui | daemon         the window | the headless background work (toasts, jobs, merge list)
//!   agentdesk job ...              recurring jobs: a prompt run on a schedule
//!   agentdesk backup | setup | usage-feed | check-prs

mod adopt;
mod attach;
mod backup;
mod bridge;
mod concierge;
mod db;
mod dispatcher;
mod goals;
mod governor;
mod hooks;
mod identities;
mod identity;
mod jobs;
mod mcp;
mod migrate;
mod notify;
#[cfg(test)]
mod parity;
mod paths;
mod permissions;
mod plain;
mod prs;
mod pyhost;
mod service;
mod sessions;
mod slots;
mod supervisor;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_concierge;
#[cfg(test)]
mod tests_goals;
#[cfg(test)]
mod tests_governor;
#[cfg(test)]
mod tests_identities;
mod tools;
mod tools_gen;
mod tui;
mod ui_api;
mod update;
mod usage;
mod vault;
mod vault_threads;
mod web;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::io::{IsTerminal, Read};

#[derive(Parser)]
#[command(name = "agentdesk", version, about = "A message board between you and the AI agents on this machine")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// The MCP server on stdio.
    Mcp,
    /// The terminal window.
    Tui,
    /// Headless background work: toasts, recurring jobs, merge-list checks, usage meter, sweep.
    Daemon,
    /// Claude Code hook: session-start | context | stop (stdin JSON in, hook JSON out).
    Hook { event: String },
    /// Block until John replies on the thread, then print his reply.
    Wait { thread: i64 },
    /// Identities: long-lived named Claude sessions the daemon keeps running (`identity new|list|start|stop|forget`).
    Identity {
        #[command(subcommand)]
        cmd: IdentityCmd,
    },
    /// Goals: a measured hypothesis a lead and its members chase (`goal new|approve|stop|budget|list|status`).
    Goal {
        #[command(subcommand)]
        cmd: GoalCmd,
    },
    /// The usage governor: `governor` shows the plan and the session pool, `governor on|off` turns enforcement on or off.
    Governor {
        /// on, off or show (the default)
        action: Option<String>,
    },
    /// The Concierge: keeps Work to Hire drained with a small swarm. `concierge` shows it, `concierge on|off` turns it on or off.
    Concierge {
        /// on, off or show (the default)
        action: Option<String>,
        /// The folder its lead and workers run in (default: $AGENTDESK_CONCIERGE_FOLDER, else here).
        #[arg(long)]
        folder: Option<String>,
    },
    /// The headless sessions the daemon hosts (`ui:session_list`).
    Sessions,
    /// `start <name>` runs the identity; `start <name> <folder> [command...]` runs a plain headless session (default command: claude).
    Start {
        name: String,
        folder: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Stops the identity or plain session called <name>.
    Stop { name: String },
    /// Stops the identity and removes it from the list; its conversation stays on disk.
    Forget { name: String },
    /// Signs up an identity: `new <name> <folder> [--charter T] [--host H] [--autostart] [--model M]`.
    New {
        name: String,
        folder: String,
        #[arg(long)]
        charter: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        autostart: bool,
    },
    /// Lists the identities.
    List,
    /// Claude Code conversations of the last day that someone typed in.
    Adoptable,
    /// Makes a conversation an identity and starts it: `adopt <session-id> <name>`.
    Adopt { session_id: String, name: String },
    /// Uses a hosted session from this console, like tmux attach; Ctrl+] detaches.
    Attach { name: String },
    /// Recurring jobs.
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Snapshot the board and write a vault note per settled thread (`--prune-dry-run` lists what thinning would delete).
    Backup {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        prune_dry_run: bool,
        /// Only the snapshot: leave the vault alone (for an ad hoc run against a real board).
        #[arg(long)]
        skip_vault: bool,
    },
    /// Restore the board from a snapshot: a dry run unless --yes. `restore <archive> [--yes] [--force-newer] [--json]`.
    Restore {
        archive: std::path::PathBuf,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        force_newer: bool,
        #[arg(long)]
        json: bool,
    },
    /// Check the merge list against GitHub now.
    CheckPrs,
    /// Claude Code status-line feed: writes claude_usage.json from the status-line JSON on stdin.
    UsageFeed,
    /// Refresh the usage feed now with `claude -p /usage` (the original does this every 5 minutes).
    UsageProbe,
    /// Runs a Python script or module of the embedded Python half with its packages importable: `python scripts/slack_bridge.py`, `python -m agentdesk.backup`.
    Python {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print where things live and the Claude Code configuration to add.
    Setup,
    /// Run one of the original's `ui:*` requests (or an agent tool) against the board and print its JSON: `agentdesk ui ui:thread '{"thread_id":1}'`.
    Ui { request: String, args: Option<String> },
    /// Render the window headlessly after some keys and print the screen (for tests): `agentdesk ui-snapshot --keys "q enter"`.
    UiSnapshot {
        #[arg(long, default_value = "110x36")]
        size: String,
        #[arg(long, default_value = "")]
        keys: String,
        #[arg(long)]
        json: bool,
    },
    /// Check GitHub for a newer release; with --apply, download it and swap it in.
    Update {
        #[arg(long)]
        apply: bool,
    },
    /// Move a machine from the .NET AgentDesk: back up the board, point Claude Code at this binary. Dry run unless --apply.
    Migrate {
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum IdentityCmd {
    /// Sign up an identity: `identity new <name> <folder> [--charter TEXT] [--model haiku|sonnet|opus] [--autostart]`.
    New {
        name: String,
        folder: String,
        #[arg(long)]
        charter: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        autostart: bool,
    },
    List,
    /// Ask for it to run; the daemon starts it (resuming its conversation) when it has a slot.
    Start {
        name: String,
    },
    /// Ask for it to stop; its conversation is kept.
    Stop {
        name: String,
    },
    /// Stop it and remove it from the list; its conversation stays on disk.
    Forget {
        name: String,
    },
}

#[derive(Subcommand)]
enum GoalCmd {
    /// Make a goal: `goal new <name> <folder> --objective TEXT`. Its lead (<name>-lead) proposes how to test it; you approve.
    New {
        name: String,
        folder: String,
        /// What it should achieve (or give the words after the folder: `goal new g1 . make it faster`).
        #[arg(long)]
        objective: Option<String>,
        #[arg(trailing_var_arg = true)]
        words: Vec<String>,
    },
    /// Approve the lead's proposal and start the goal; optional budget: members, hours, wake cadence in minutes.
    Approve {
        name: String,
        #[arg(long)]
        members: Option<i64>,
        #[arg(long)]
        hours: Option<f64>,
        #[arg(long)]
        cadence: Option<f64>,
    },
    /// Change a running goal's budget.
    Budget {
        name: String,
        #[arg(long)]
        members: Option<i64>,
        #[arg(long)]
        hours: Option<f64>,
        #[arg(long)]
        cadence: Option<f64>,
    },
    Stop {
        name: String,
    },
    List,
    Status {
        name: String,
    },
}

#[derive(Subcommand)]
enum JobCmd {
    /// Make a job: `job new <name> <folder> --prompt TEXT (--at HH:MM [--days daily|weekdays|mon,thu] | --every MINUTES)`.
    New {
        name: String,
        folder: String,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        days: Option<String>,
        #[arg(long)]
        every: Option<f64>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        catch_up: Option<f64>,
    },
    List,
    Run {
        name: String,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    Delete {
        name: String,
    },
}

fn open() -> Result<db::Db> {
    let d = db::Db::open(&paths::db_path())?;
    d.init()?;
    Ok(d)
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("agentdesk: {}", db::anyhow_msg(&e));
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => {
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                tui::run()
            } else {
                mcp::serve()
            }
        }
        Some(Cmd::Mcp) => mcp::serve(),
        Some(Cmd::Tui) => tui::run(),
        Some(Cmd::Daemon) => {
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            println!("agentdesk daemon running on {}", paths::db_path().display());
            service::run(stop);
            Ok(())
        }
        Some(Cmd::Hook { event }) => {
            // a hook must never break a session
            let mut input = String::new();
            let _ = std::io::stdin().read_to_string(&mut input);
            let v = serde_json::from_str(input.trim_start_matches('\u{feff}')).unwrap_or_else(|_| serde_json::json!({}));
            if let Ok(d) = open() {
                print!("{}", hooks::run(&d, &event, &v));
            }
            Ok(())
        }
        Some(Cmd::Wait { thread }) => {
            let d = open()?;
            println!("{}", hooks::wait(&d, thread)?);
            Ok(())
        }
        Some(Cmd::Job { cmd }) => job(cmd),
        Some(Cmd::Identity { cmd }) => identity(cmd),
        Some(Cmd::Goal { cmd }) => goal(cmd),
        Some(Cmd::Sessions) => verb("ui:session_list", None),
        Some(Cmd::Start { name, folder, command }) => match folder {
            None => verb("ui:identity_start", Some(json!({"name": name}))),
            Some(f) => {
                let f = std::fs::canonicalize(&f).map(|p| p.to_string_lossy().into_owned()).unwrap_or(f);
                verb("ui:session_start", Some(json!({"name": name, "folder": f, "command": if command.is_empty() { Value::Null } else { json!(command.join(" ")) }})))
            }
        },
        Some(Cmd::Stop { name }) => verb("ui:identity_stop", Some(json!({"name": name}))),
        Some(Cmd::Forget { name }) => verb("ui:identity_forget", Some(json!({"name": name}))),
        Some(Cmd::New { name, folder, charter, host, model, autostart }) => {
            let f = std::fs::canonicalize(&folder).map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string()).unwrap_or(folder);
            verb("ui:identity_create", Some(json!({"name": name, "folder": f, "charter": charter, "host": host, "model": model, "autostart": autostart})))
        }
        Some(Cmd::List) => verb("ui:identity_list", None),
        Some(Cmd::Adoptable) => verb("ui:adoptable", None),
        Some(Cmd::Adopt { session_id, name }) => verb("ui:adopt", Some(json!({"session_id": session_id, "name": name}))),
        Some(Cmd::Attach { name }) => attach::client(&name),
        Some(Cmd::Governor { action }) => governor_cmd(action.as_deref()),
        Some(Cmd::Concierge { action, folder }) => concierge_cmd(action.as_deref(), folder),
        Some(Cmd::Backup { json, prune_dry_run, skip_vault }) => {
            if prune_dry_run {
                println!("{}", backup::prune_dry_run());
                return Ok(());
            }
            let out = backup::run_with(&open()?, skip_vault)?;
            println!("{}", if json { serde_json::json!({"result": out}).to_string() } else { out });
            Ok(())
        }
        Some(Cmd::Restore { archive, yes, force_newer, json }) => match backup::restore(&open()?, &archive, yes, force_newer, json) {
            Ok(m) => {
                println!("{m}");
                Ok(())
            }
            Err(e) => {
                println!("{}", db::anyhow_msg(&e));
                std::process::exit(1);
            }
        },
        Some(Cmd::CheckPrs) => {
            println!("{} settled", prs::check(&open()?));
            Ok(())
        }
        Some(Cmd::UsageFeed) => {
            let mut input = String::new();
            let _ = std::io::stdin().read_to_string(&mut input);
            print!("{}", usage::feed(&input));
            Ok(())
        }
        Some(Cmd::Update { apply }) => {
            let (latest, newer) = update::check()?;
            if !newer {
                println!("agentdesk {} is up to date (latest release: {latest})", update::CURRENT);
            } else if apply {
                println!("updating {} -> {latest} ...", update::CURRENT);
                println!("now running {}; restart Claude Code sessions and the window to use it", update::apply()?);
            } else {
                println!("agentdesk {latest} is available (you have {}). Run `agentdesk update --apply`.", update::CURRENT);
            }
            Ok(())
        }
        Some(Cmd::Migrate { apply }) => {
            let home = paths::home_dir();
            println!("{}", migrate::run(apply, &home.join(".claude.json"), &paths::claude_settings())?);
            Ok(())
        }
        Some(Cmd::Ui { request, args }) => std::process::exit(ui_api::run(&request, args.as_deref())),
        Some(Cmd::UiSnapshot { size, keys, json }) => {
            println!("{}", tui::snapshot(&size, &keys, json)?);
            Ok(())
        }
        Some(Cmd::UsageProbe) => {
            if usage::probe() {
                println!("usage feed updated");
                Ok(())
            } else {
                anyhow::bail!("the probe named no usage window (is `claude` on the PATH and signed in?)")
            }
        }
        Some(Cmd::Python { args }) => {
            let status = pyhost::command()?.args(&args).status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Some(Cmd::Setup) => {
            setup();
            Ok(())
        }
    }
}

fn job(cmd: JobCmd) -> Result<()> {
    let d = open()?;
    let pretty = |v: serde_json::Value| println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    match cmd {
        JobCmd::New { name, folder, prompt, at, days, every, model, catch_up } => {
            let folder = std::fs::canonicalize(&folder).map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string()).unwrap_or(folder);
            pretty(jobs::create(
                &d,
                jobs::NewJob {
                    name: &name,
                    folder: &folder,
                    prompt: &prompt,
                    at: at.as_deref(),
                    days: days.as_deref(),
                    every_minutes: every,
                    model: model.as_deref(),
                    catch_up_minutes: catch_up,
                },
            )?)
        }
        JobCmd::List => pretty(serde_json::json!({"jobs": jobs::list(&d)?})),
        JobCmd::Run { name } => {
            let j = jobs::run_now(&d, &name)?;
            pretty(j);
            // the run belongs to this process: wait for it so it is not orphaned half way
            while jobs::is_running(&name) {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        JobCmd::Enable { name } => pretty(jobs::enable(&d, &name, true)?),
        JobCmd::Disable { name } => pretty(jobs::enable(&d, &name, false)?),
        JobCmd::Delete { name } => pretty(jobs::delete(&d, &name)?),
    }
    Ok(())
}

fn setup() {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "agentdesk".into());
    println!("Board:   {}", paths::db_path().display());
    println!("Vault:   {}", paths::vault_dir().display());
    println!("Log:     {}\n", paths::log_file().display());
    println!("1. Register the MCP server with Claude Code:\n\n   claude mcp add --scope user agentdesk -- \"{exe}\" mcp\n");
    println!("2. Add the hooks to ~/.claude/settings.json:\n");
    let hook = |ev: &str| serde_json::json!([{"hooks": [{"type": "command", "command": format!("\"{exe}\" hook {ev}")}]}]);
    let cfg = serde_json::json!({"hooks": {"SessionStart": hook("session-start"), "PostToolUse": hook("context"), "Stop": hook("stop")}});
    println!("{}\n", serde_json::to_string_pretty(&cfg).unwrap());
    println!("3. Optional, the usage meter: set the status line command to `\"{exe}\" usage-feed`.");
    println!("4. Open the window with `agentdesk`; keep `agentdesk daemon` running (or the window open) for toasts and recurring jobs.");
}

fn identity(cmd: IdentityCmd) -> Result<()> {
    let d = open()?;
    let (row, note) = match cmd {
        IdentityCmd::New { name, folder, charter, model, autostart } => (identities::create(&d, &name, &folder, charter.as_deref(), model.as_deref(), autostart)?, false),
        IdentityCmd::List => {
            for r in identities::list(&d)? {
                println!(
                    "{:<24} {:<8} gen {:<3} {:<7} {}",
                    r["name"].as_str().unwrap_or(""),
                    r["state"].as_str().unwrap_or(""),
                    r["generation"].as_i64().unwrap_or(1),
                    r["model"].as_str().unwrap_or(""),
                    r["folder"].as_str().unwrap_or("")
                );
            }
            return Ok(());
        }
        IdentityCmd::Start { name } => (identities::start(&d, &name)?, true),
        IdentityCmd::Stop { name } => (identities::stop(&d, &name)?, true),
        IdentityCmd::Forget { name } => (identities::forget(&d, &name)?, false),
    };
    println!("{}", serde_json::to_string_pretty(&row)?);
    if note && !supervisor::running() {
        eprintln!("note: no supervisor is running, so nothing will act on this yet. Start `agentdesk daemon` (or the window).");
    }
    Ok(())
}

fn concierge_cmd(action: Option<&str>, folder: Option<String>) -> Result<()> {
    let on = match action {
        Some("on") => Some(true),
        Some("off") => Some(false),
        Some("show") | None => None,
        Some(other) => anyhow::bail!("concierge takes on, off or show, not {other}"),
    };
    let folder = folder
        .or_else(|| std::env::var("AGENTDESK_CONCIERGE_FOLDER").ok().filter(|f| !f.is_empty()))
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let d = open()?;
    let out = concierge::toggle(&d, &identity::Caller::from_env(), &folder, on)?;
    println!("{}", serde_json::to_string_pretty(&out)?);
    if on.is_some() && !supervisor::running() {
        eprintln!("note: no supervisor is running, so nothing will act on this yet. Start `agentdesk daemon` (or the window).");
    }
    Ok(())
}

fn governor_cmd(action: Option<&str>) -> Result<()> {
    match action {
        Some("on") => governor::set_enforce(true)?,
        Some("off") => governor::set_enforce(false)?,
        Some("show") | None => {}
        Some(other) => anyhow::bail!("governor takes on, off or show, not {other}"),
    }
    let d = open()?;
    let r = governor::report(&d, chrono::Utc::now(), false, None);
    println!("{}", r["summary"].as_str().unwrap_or(""));
    println!("{}", serde_json::to_string_pretty(&r)?);
    Ok(())
}

/// Runs one of the original's requests through `ui_api` and prints its document (the original CLI's verbs are thin wrappers).
fn verb(request: &str, args: Option<Value>) -> Result<()> {
    let code = ui_api::run(request, args.map(|a| a.to_string()).as_deref());
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn goal(cmd: GoalCmd) -> Result<()> {
    let d = open()?;
    let me = identity::Caller::from_env();
    let out = match cmd {
        GoalCmd::New { name, folder, objective, words } => {
            let objective = objective.unwrap_or_else(|| words.join(" "));
            if objective.trim().is_empty() {
                anyhow::bail!("say what the goal should achieve: goal new <name> <folder> <objective...>");
            }
            let folder = std::fs::canonicalize(&folder).map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string()).unwrap_or(folder);
            goals::create(&d, &goals::slug(&name), &objective, &folder)?
        }
        GoalCmd::Approve { name, members, hours, cadence } => goals::approve(&d, &me, &name, members, hours, cadence)?,
        GoalCmd::Budget { name, members, hours, cadence } => goals::set_budget(&d, &me, &name, members, hours, cadence)?,
        GoalCmd::Stop { name } => goals::stop(&d, &me, &name)?,
        GoalCmd::List => goals::list(&d)?,
        GoalCmd::Status { name } => goals::status(&d, &name)?,
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    if !supervisor::running() {
        eprintln!("note: no supervisor is running, so nothing will act on this yet. Start `agentdesk daemon` (or the window).");
    }
    Ok(())
}
