//! The script runner: a script an agent put in a question (a fenced ```powershell, ```bash, ```sh or ```cmd block) is run from the
//! reader, in a pane under the thread, instead of being copied into another terminal. It never runs by itself: the first key opens the
//! pane and shows the script, the second runs it. Output streams into the pane; it can be killed and its tail put into the reply.
//!
//! The script goes to the shell on stdin (not a file: a security product on this machine quarantines scripts a program writes), with no
//! console window of its own. It runs in the user's home folder.

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

/// Output kept in the pane.
const KEEP_LINES: usize = 2000;

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub lang: String,
    pub script: String,
    pub from: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shell {
    Pwsh,
    Bash,
    Sh,
    Cmd,
}

pub fn shell_for(lang: &str) -> Option<Shell> {
    match lang.trim().to_lowercase().as_str() {
        "powershell" | "pwsh" | "ps1" | "ps" => Some(Shell::Pwsh),
        "bash" | "zsh" => Some(Shell::Bash),
        "sh" | "shell" | "console" => Some(Shell::Sh),
        "cmd" | "bat" | "batch" => Some(Shell::Cmd),
        _ => None,
    }
}

/// Every runnable fenced block in the messages (`(author, body)`), the newest message's first.
pub fn extract_blocks(msgs: &[(String, String)]) -> Vec<Block> {
    let mut out = Vec::new();
    for (author, body) in msgs.iter().rev() {
        let (mut lang, mut buf): (Option<String>, Vec<&str>) = (None, vec![]);
        let mut found: Vec<Block> = Vec::new();
        for line in body.lines() {
            let t = line.trim_start();
            match &lang {
                None => {
                    if let Some(rest) = t.strip_prefix("```") {
                        lang = Some(rest.trim().to_string());
                        buf.clear();
                    }
                }
                Some(l) => {
                    if t.starts_with("```") {
                        if shell_for(l).is_some() && !buf.join("").trim().is_empty() {
                            found.push(Block { lang: l.clone(), script: buf.join("\n"), from: author.clone() });
                        }
                        lang = None;
                    } else {
                        buf.push(line);
                    }
                }
            }
        }
        out.extend(found);
    }
    out
}

/// Terminal escape sequences removed: the pane shows text, not colours.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            match it.peek() {
                Some('[') => {
                    it.next();
                    for n in it.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    it.next();
                    while let Some(n) = it.next() {
                        if n == '\u{7}' || (n == '\u{1b}' && it.peek() == Some(&'\\')) {
                            break;
                        }
                    }
                }
                _ => {
                    it.next();
                }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

/// The program, its arguments and what to send on stdin for a script.
pub fn command_for(shell: Shell, script: &str) -> (String, Vec<String>, String) {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match shell {
        Shell::Pwsh => {
            let prog = if cfg!(windows) { "powershell.exe" } else { "pwsh" };
            (prog.into(), args(&["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", "-"]), format!("{script}\n"))
        }
        Shell::Bash => ("bash".into(), args(&["-s"]), format!("{script}\n")),
        Shell::Sh => ("sh".into(), args(&["-s"]), format!("{script}\n")),
        // cmd reads a script on stdin with prompts and echo; run the lines as one command line instead. The line is wrapped in one pair
        // of quotes that `/S` strips, so quotes inside the script (`--comment "two words"`) reach the program as typed; `run` hands the
        // last argument to Windows exactly as written (Rust would otherwise escape those quotes with backslashes, which cmd ignores).
        Shell::Cmd => {
            let line = script.lines().map(str::trim).filter(|l| !l.is_empty() && !l.to_lowercase().starts_with("rem ")).collect::<Vec<_>>().join(" & ");
            ("cmd.exe".into(), vec!["/Q".into(), "/D".into(), "/S".into(), "/C".into(), format!("\"{line}\"")], String::new())
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// The script is shown; nothing has run.
    Ready,
    Running,
    Done(i32),
    Failed(String),
}

enum Msg {
    Line(String),
    Exit(i32),
    Error(String),
}

pub struct Runner {
    pub blocks: Vec<Block>,
    pub picked: usize,
    pub out: Vec<String>,
    pub state: State,
    pub scroll_up: usize,
    rx: Option<Receiver<Msg>>,
    pid: Arc<AtomicU32>,
}

impl Runner {
    pub fn new(blocks: Vec<Block>) -> Runner {
        Runner { blocks, picked: 0, out: vec![], state: State::Ready, scroll_up: 0, rx: None, pid: Arc::new(AtomicU32::new(0)) }
    }

    pub fn block(&self) -> Option<&Block> {
        self.blocks.get(self.picked)
    }

    pub fn pick(&mut self, i: usize) -> bool {
        if i < self.blocks.len() && self.state != State::Running {
            self.picked = i;
            self.out.clear();
            self.state = State::Ready;
            true
        } else {
            false
        }
    }

    /// Starts the picked script (a second run replaces the first's output). False when there is nothing to run or it is running.
    pub fn run(&mut self) -> bool {
        let Some(b) = self.block().cloned() else { return false };
        if self.state == State::Running {
            return false;
        }
        let Some(shell) = shell_for(&b.lang) else { return false };
        let (prog, args, stdin) = command_for(shell, &b.script);
        self.out.clear();
        self.scroll_up = 0;
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.state = State::Running;
        let pid = self.pid.clone();
        std::thread::spawn(move || {
            let mut cmd = crate::paths::new_command(&prog);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                if prog == "cmd.exe" {
                    let (flags, line) = args.split_at(args.len() - 1);
                    cmd.args(flags).raw_arg(&line[0]);
                } else {
                    cmd.args(&args);
                }
            }
            #[cfg(not(windows))]
            cmd.args(&args);
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).current_dir(crate::paths::home_dir());
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => return drop(tx.send(Msg::Error(format!("could not start {prog}: {e}")))),
            };
            pid.store(child.id(), Ordering::Relaxed);
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(stdin.as_bytes());
            }
            let pump = |r: Box<dyn std::io::Read + Send>, tx: Sender<Msg>| {
                std::thread::spawn(move || {
                    for line in BufReader::new(r).split(b'\n').map_while(|l| l.ok()) {
                        let _ = tx.send(Msg::Line(strip_ansi(&String::from_utf8_lossy(&line))));
                    }
                })
            };
            let a = child.stdout.take().map(|o| pump(Box::new(o), tx.clone()));
            let b = child.stderr.take().map(|e| pump(Box::new(e), tx.clone()));
            let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            for h in [a, b].into_iter().flatten() {
                let _ = h.join();
            }
            pid.store(0, Ordering::Relaxed);
            let _ = tx.send(Msg::Exit(code));
        });
        true
    }

    /// Takes what the script has printed since the last call. True when something changed (the pane needs a redraw).
    pub fn pump(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        let mut changed = false;
        while let Ok(m) = rx.try_recv() {
            changed = true;
            match m {
                Msg::Line(l) => {
                    self.out.push(l);
                    if self.out.len() > KEEP_LINES {
                        self.out.remove(0);
                    }
                }
                Msg::Exit(c) => self.state = State::Done(c),
                Msg::Error(e) => self.state = State::Failed(e),
            }
        }
        changed
    }

    /// Ends the running script and everything it started.
    pub fn kill(&mut self) -> bool {
        let pid = self.pid.swap(0, Ordering::Relaxed);
        if pid == 0 {
            return false;
        }
        let _ = if cfg!(windows) {
            crate::paths::new_command("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output()
        } else {
            crate::paths::new_command("kill").args(["-9", &pid.to_string()]).output()
        };
        self.out.push("[stopped]".into());
        true
    }

    /// The reply text for "here is what happened": the last `n` lines in a fenced block, with the exit code.
    pub fn summary(&self, n: usize) -> String {
        let tail: Vec<&str> = self.out.iter().rev().take(n).map(String::as_str).collect::<Vec<_>>().into_iter().rev().collect();
        let head = match &self.state {
            State::Done(0) => "I ran the script: it finished (exit 0).".to_string(),
            State::Done(c) => format!("I ran the script: it failed (exit {c})."),
            State::Failed(e) => format!("I tried to run the script: {e}"),
            State::Running => "I started the script; it is still running. So far:".to_string(),
            State::Ready => "I have not run the script.".to_string(),
        };
        if tail.is_empty() {
            head
        } else {
            format!("{head}\n```\n{}\n```\n", tail.join("\n"))
        }
    }

    pub fn status(&self) -> String {
        match &self.state {
            State::Ready => "Alt+R runs it".into(),
            State::Running => "running... Alt+K stops it".into(),
            State::Done(0) => "done (exit 0). Alt+Y puts the output in your reply, Alt+R runs it again".into(),
            State::Done(c) => format!("failed (exit {c}). Alt+Y puts the output in your reply, Alt+R runs it again"),
            State::Failed(e) => e.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn runnable_blocks_are_found_newest_first_and_others_ignored() {
        let m = msgs(&[("a", "Run this:\n```powershell\nGet-Date\nWrite-Output hi\n```\nthen\n```json\n{}\n```"), ("b", "or on linux\n```bash\nuname -a\n```\n```\n\n```")]);
        let b = extract_blocks(&m);
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].lang.as_str(), b[0].script.as_str(), b[0].from.as_str()), ("bash", "uname -a", "b"));
        assert_eq!((b[1].lang.as_str(), b[1].script.as_str()), ("powershell", "Get-Date\nWrite-Output hi"));
        assert!(extract_blocks(&msgs(&[("a", "```text\nnope\n```")])).is_empty());
        assert!(extract_blocks(&msgs(&[("a", "```cmd\n   \n```")])).is_empty(), "an empty block is not a script");
    }

    #[test]
    fn colours_and_cursor_codes_are_removed() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m text\r"), "red text");
        assert_eq!(strip_ansi("a\u{1b}]0;title\u{7}b"), "ab");
    }

    #[test]
    fn each_shell_gets_the_script_the_way_it_reads_it() {
        let (p, a, stdin) = command_for(Shell::Pwsh, "Get-Date");
        assert!(p.contains("pwsh") || p.contains("powershell") && a.last().map(String::as_str) == Some("-") && stdin == "Get-Date\n");
        let (p, a, stdin) = command_for(Shell::Cmd, "rem note\necho a\n\n  dir  ");
        assert_eq!((p.as_str(), a.last().map(String::as_str), stdin.as_str()), ("cmd.exe", Some("\"echo a & dir\""), ""));
        assert_eq!(&a[..4], ["/Q", "/D", "/S", "/C"], "/S strips the one outer pair of quotes");
        assert_eq!(shell_for("PS1"), Some(Shell::Pwsh));
        assert_eq!(shell_for("python"), None);
    }

    #[test]
    fn a_script_runs_streams_its_output_and_reports_its_exit_code() {
        let (lang, script) = if cfg!(windows) { ("cmd", "echo one\necho two\nexit /b 3") } else { ("sh", "echo one\necho two\nexit 3") };
        let mut r = Runner::new(vec![Block { lang: lang.into(), script: script.into(), from: "x".into() }]);
        assert_eq!(r.state, State::Ready);
        assert!(r.run() && !r.run(), "a second start while running is refused");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while r.state == State::Running && std::time::Instant::now() < end {
            r.pump();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(r.state, State::Done(3), "{:?} {:?}", r.state, r.out);
        let out = r.out.join("|");
        assert!(out.contains("one") && out.contains("two"), "{out}");
        assert!(r.summary(5).starts_with("I ran the script: it failed (exit 3).\n```\n"));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn quotes_in_a_cmd_script_reach_the_program_as_typed() {
        // `gh pr close 123 --comment "Included in #124."` failed with "too many arguments": the quotes arrived as \" and split the words.
        let mut r = Runner::new(vec![Block { lang: "cmd".into(), script: "echo \"two words\" \"x y\" && echo after".into(), from: "x".into() }]);
        assert!(r.run());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while r.state == State::Running && std::time::Instant::now() < end {
            r.pump();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(r.state, State::Done(0), "{:?} {:?}", r.state, r.out);
        assert!(r.out.iter().any(|l| l.trim() == "\"two words\" \"x y\""), "the quotes arrive as typed: {:?}", r.out);
        assert!(r.out.iter().any(|l| l.trim() == "after"), "&& still chains: {:?}", r.out);
    }

    #[test]
    fn a_powershell_script_runs_from_stdin_and_stderr_is_shown_too() {
        let mut r =
            Runner::new(vec![Block { lang: "powershell".into(), script: "Write-Output 'hello from ps'\n[Console]::Error.WriteLine('oops')\nexit 2".into(), from: "x".into() }]);
        assert!(r.run());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while r.state == State::Running && std::time::Instant::now() < end {
            r.pump();
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let out = r.out.join("|");
        assert_eq!(r.state, State::Done(2), "{:?} {out}", r.state);
        assert!(out.contains("hello from ps") && out.contains("oops"), "{out}");
    }
}
