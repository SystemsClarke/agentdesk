//! The session host: runs `claude` sessions in pseudo-terminals the supervisor owns (ConPTY on Windows, a PTY elsewhere), keeps each
//! one's output in `sessions/<name>.log` in the data folder, and can type into one.
//!
//! A session is a child of the supervisor. When the supervisor exits its sessions go with it (the PTY closes), the way the .NET core's
//! kill-on-close job object ended them; the next supervisor resumes whatever the board says was running.

use crate::paths;
use anyhow::{anyhow, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;

/// A session's log stops growing here: the newest half is kept.
const LOG_LIMIT: u64 = 4 * 1024 * 1024;

struct Live {
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    #[cfg_attr(not(test), allow(dead_code))]
    command: Vec<String>,
}

#[derive(Default)]
pub struct Host {
    live: HashMap<String, Live>,
}

pub fn log_path(name: &str) -> PathBuf {
    let safe: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    paths::data_dir().join("sessions").join(format!("{safe}.log"))
}

impl Host {
    pub fn new() -> Host {
        Host::default()
    }

    /// Starts `program args` in `cwd` on a new PTY with `env` added. Returns the child's pid.
    pub fn launch(&mut self, name: &str, cwd: &str, program: &str, args: &[String], env: &[(String, String)]) -> Result<u32> {
        if self.alive(name) {
            return Err(anyhow!("{name} is already running"));
        }
        let pair = native_pty_system().openpty(PtySize { rows: 40, cols: 160, pixel_width: 0, pixel_height: 0 }).map_err(|e| anyhow!("no pty: {e}"))?;
        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(cwd);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).map_err(|e| anyhow!("could not start {program}: {e}"))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| anyhow!("pty reader: {e}"))?;
        let writer = pair.master.take_writer().map_err(|e| anyhow!("pty writer: {e}"))?;
        let pid = child.process_id().unwrap_or(0);
        let log = log_path(name);
        let _ = std::fs::create_dir_all(log.parent().unwrap());
        std::thread::spawn(move || {
            let Ok(mut f) = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&log) else { return };
            let (mut buf, mut written) = ([0u8; 8192], 0u64);
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let _ = f.write_all(&buf[..n]);
                written += n as u64;
                if written > LOG_LIMIT {
                    // keep the newest half: rewrite the file from its second half
                    if let Ok(all) = std::fs::read(&log) {
                        let keep = &all[all.len() / 2..];
                        if let Ok(mut g) = std::fs::File::create(&log) {
                            let _ = g.write_all(keep);
                        }
                        if let Ok(h) = std::fs::OpenOptions::new().append(true).open(&log) {
                            f = h;
                        }
                        written = keep.len() as u64;
                    }
                }
            }
        });
        let mut command = vec![program.to_string()];
        command.extend(args.iter().cloned());
        self.live.insert(name.to_string(), Live { child, _master: pair.master, writer, command });
        Ok(pid)
    }

    /// Whether the named session's process is still running.
    pub fn alive(&mut self, name: &str) -> bool {
        match self.live.get_mut(name) {
            Some(l) => match l.child.try_wait() {
                Ok(None) => true,
                _ => {
                    self.live.remove(name);
                    false
                }
            },
            None => false,
        }
    }

    /// The names of the sessions this host started and still holds (running or not yet noticed dead).
    pub fn names(&self) -> Vec<String> {
        self.live.keys().cloned().collect()
    }

    /// What was run for this session: the program, then its arguments.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn command(&self, name: &str) -> Option<&[String]> {
        self.live.get(name).map(|l| l.command.as_slice())
    }

    /// Ends the session now. Its conversation stays on disk.
    pub fn stop(&mut self, name: &str) {
        if let Some(mut l) = self.live.remove(name) {
            let _ = l.child.kill();
            let _ = l.child.wait();
        }
    }

    pub fn stop_all(&mut self) {
        for n in self.names() {
            self.stop(&n);
        }
    }

    /// Types `text` and presses Enter, as if at the keyboard: how a reply from John reaches a session that is waiting.
    pub fn type_text(&mut self, name: &str, text: &str) -> Result<()> {
        let l = self.live.get_mut(name).ok_or_else(|| anyhow!("{name} is not running"))?;
        l.writer.write_all(text.as_bytes())?;
        l.writer.write_all(b"\r")?;
        l.writer.flush()?;
        Ok(())
    }
}
