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
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// A session's log stops growing here: the newest half is kept.
const LOG_LIMIT: u64 = 4 * 1024 * 1024;
/// How much recent output a newly attached client is sent.
const TAIL_LIMIT: usize = 128 * 1024;

/// What an attached client needs from a session, shared with the thread that reads the PTY.
pub struct Shared {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    /// The newest output (replayed to a client that attaches) and the clients now listening.
    out: Mutex<Out>,
    /// The startup cursor query has been answered.
    answered: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Out {
    tail: Vec<u8>,
    subs: Vec<Sender<Vec<u8>>>,
    closed: bool,
}

/// Sessions by name, reachable from the attach server's threads.
pub type Registry = Arc<Mutex<HashMap<String, Arc<Shared>>>>;

impl Shared {
    fn push(&self, chunk: &[u8]) {
        let mut o = self.out.lock().unwrap_or_else(|e| e.into_inner());
        o.tail.extend_from_slice(chunk);
        if o.tail.len() > TAIL_LIMIT {
            let cut = o.tail.len() - TAIL_LIMIT / 2;
            o.tail.drain(..cut);
        }
        o.subs.retain(|s| s.send(chunk.to_vec()).is_ok());
    }

    /// Windows' ConPTY asks the terminal where the cursor is (ESC[6n) when it starts, and holds the session's output until it is told.
    /// The host always answers that first query itself, and keeps it out of what clients see (a client's terminal would answer a
    /// replayed one too, and the stray reply would land in the session as typing). Later queries are the program's own: left alone.
    fn answer_startup_query(&self, chunk: &[u8]) -> Vec<u8> {
        let at = chunk.windows(4).position(|w| w == b"[6n");
        match at {
            Some(i) if !self.answered.swap(true, std::sync::atomic::Ordering::SeqCst) => {
                let _ = self.write(b"[1;1R");
                [&chunk[..i], &chunk[i + 4..]].concat()
            }
            _ => chunk.to_vec(),
        }
    }

    fn close(&self) {
        let mut o = self.out.lock().unwrap_or_else(|e| e.into_inner());
        o.closed = true;
        o.subs.clear(); // dropping the senders ends every listener
    }

    /// Starts listening: the recent output first, then everything after it. The receiver ends when the session does.
    pub fn subscribe(&self) -> (Vec<u8>, Receiver<Vec<u8>>) {
        let (tx, rx) = channel();
        let mut o = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let tail = o.tail.clone();
        if !o.closed {
            o.subs.push(tx);
        }
        (tail, rx)
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let m = self.master.lock().unwrap_or_else(|e| e.into_inner());
        let _ = m.resize(PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 });
    }
}

struct Live {
    child: Box<dyn Child + Send + Sync>,
    shared: Arc<Shared>,
    #[cfg_attr(not(test), allow(dead_code))]
    command: Vec<String>,
}

#[derive(Default)]
pub struct Host {
    live: HashMap<String, Live>,
    registry: Registry,
}

pub fn log_path(name: &str) -> PathBuf {
    let safe: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    paths::data_dir().join("sessions").join(format!("{safe}.log"))
}

impl Host {
    pub fn new() -> Host {
        Host::default()
    }

    /// The sessions by name, for the attach server.
    pub fn registry(&self) -> Registry {
        self.registry.clone()
    }

    /// Starts `program args` in `cwd` on a new PTY with `env` added. Returns the child's pid.
    pub fn launch(&mut self, name: &str, cwd: &str, program: &str, args: &[String], env: &[(String, String)]) -> Result<u32> {
        self.launch_with(name, cwd, program, args, env, &[])
    }

    /// `launch`, with these variables removed from the environment the child inherits first (a plain session must not see the
    /// `CLAUDE*` / `AGENTDESK_SESSION` / `AGENTDESK_AUTHOR` of whatever session started the daemon).
    pub fn launch_with(&mut self, name: &str, cwd: &str, program: &str, args: &[String], env: &[(String, String)], remove: &[String]) -> Result<u32> {
        if self.alive(name) {
            return Err(anyhow!("{name} is already running"));
        }
        let pair = native_pty_system().openpty(PtySize { rows: 40, cols: 160, pixel_width: 0, pixel_height: 0 }).map_err(|e| anyhow!("no pty: {e}"))?;
        let (exe, pre) = crate::paths::resolve(program);
        let mut cmd = CommandBuilder::new(exe);
        cmd.args(pre);
        cmd.args(args);
        cmd.cwd(cwd);
        for k in remove {
            cmd.env_remove(k);
        }
        // A daemon started from inside a Claude Code session inherits `CLAUDE_CODE_CHILD_SESSION`, and claude then skips saving
        // the transcript of every session it hosts: `--resume` finds nothing and "keeps working when closed" quietly stops.
        cmd.env("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE", "1");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).map_err(|e| anyhow!("could not start {program}: {e}"))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| anyhow!("pty reader: {e}"))?;
        let writer = pair.master.take_writer().map_err(|e| anyhow!("pty writer: {e}"))?;
        let shared = Arc::new(Shared { master: Mutex::new(pair.master), writer: Mutex::new(writer), out: Mutex::new(Out::default()), answered: Default::default() });
        let feed = shared.clone();
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
                feed.push(&feed.answer_startup_query(&buf[..n]));
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
            feed.close();
        });
        let mut command = vec![program.to_string()];
        command.extend(args.iter().cloned());
        self.registry.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string(), shared.clone());
        self.live.insert(name.to_string(), Live { child, shared, command });
        Ok(pid)
    }

    /// Whether the named session's process is still running.
    pub fn alive(&mut self, name: &str) -> bool {
        match self.live.get_mut(name) {
            Some(l) => match l.child.try_wait() {
                Ok(None) => true,
                _ => {
                    self.forget(name);
                    false
                }
            },
            None => false,
        }
    }

    fn forget(&mut self, name: &str) {
        self.live.remove(name);
        self.registry.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
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
        self.registry.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
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

    /// Writes raw bytes to the session's terminal.
    pub fn input(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        self.live.get(name).ok_or_else(|| anyhow!("{name} is not running"))?.shared.write(bytes)
    }

    /// Resizes the session's terminal.
    pub fn resize(&mut self, name: &str, cols: u16, rows: u16) -> Result<()> {
        self.live.get(name).ok_or_else(|| anyhow!("{name} is not running"))?.shared.resize(cols, rows);
        Ok(())
    }

    /// Types `text` and presses Enter, as if at the keyboard: how a reply from John reaches a session that is waiting.
    pub fn type_text(&mut self, name: &str, text: &str) -> Result<()> {
        let l = self.live.get(name).ok_or_else(|| anyhow!("{name} is not running"))?;
        l.shared.write(format!("{text}\r").as_bytes())
    }
}
