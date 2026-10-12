//! agentdesk: a message board between you and the AI agents running on this machine.
//!
//!   agentdesk                      a terminal: the window (TUI); piped (no tty): the MCP server on stdio
//!   agentdesk mcp                  the MCP server on stdio (how every Claude Code session reaches the board)
//!   agentdesk hook <event>         Claude Code hook: stdin JSON in, hook JSON out (never fails the session)
//!   agentdesk wait <thread-id>     block until John replies on the thread, then print his reply
//!   agentdesk tui | daemon         the window | the headless background work (toasts, jobs, merge list)
//!   agentdesk job ...              recurring jobs: a prompt run on a schedule
//!   agentdesk backup | setup | usage-feed | check-prs

mod backup;
mod db;
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
mod prs;
mod service;
mod sessions;
mod supervisor;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_identities;
mod tools;
mod tools_gen;
mod tui;
mod update;
mod usage;
mod vault;
mod vault_threads;

use anyhow::Result;
use clap::{Parser, Subcommand};
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
    /// Recurring jobs.
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Snapshot the board and write a vault note per settled thread.
    Backup,
    /// Check the merge list against GitHub now.
    CheckPrs,
    /// Claude Code status-line feed: writes claude_usage.json from the status-line JSON on stdin.
    UsageFeed,
    /// Print where things live and the Claude Code configuration to add.
    Setup,
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
        Some(Cmd::Backup) => {
            println!("{}", backup::run(&open()?)?);
            Ok(())
        }
        Some(Cmd::CheckPrs) => {
            println!("{} settled", prs::check(&open()?));
            Ok(())
        }
        Some(Cmd::UsageFeed) => {
            let mut input = String::new();
            let _ = std::io::stdin().read_to_string(&mut input);
            println!("{}", usage::feed(&input));
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
            let home = dirs::home_dir().unwrap_or_default();
            println!("{}", migrate::run(apply, &home.join(".claude.json"), &paths::claude_settings())?);
            Ok(())
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
