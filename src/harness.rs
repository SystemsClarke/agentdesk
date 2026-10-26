//! The harness seam: everything that starts, resumes, types into or runs headless a coding-agent session goes through `Harness`, so the
//! board, the supervisor and the goals engine never need to know which agent program is on the other end. Claude Code is the first
//! implementation (and the default for every row that names none); `Generic` is any command-line agent that takes its prompt as a
//! trailing argument and cannot resume.
//!
//! What a session is told (the chain charter, the state file) stays AgentDesk's and is built in `identities`; a harness only decides how
//! to hand it over.

use crate::identities as ids;
use crate::paths;
use crate::sessions::Host;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The harness of a row that names none.
pub const DEFAULT: &str = "claude-code";

/// What a harness can do, so callers can ask instead of assuming Claude Code.
#[cfg_attr(not(test), allow(dead_code))] // read by tests now, by the later stages that drop Claude-only paths for other harnesses
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// A stopped session can be started again with its conversation intact.
    pub resume: bool,
    /// It runs AgentDesk's hooks (replies injected, the Stop hook, the context meter's warnings).
    pub hooks: bool,
    /// It can run a prompt to completion with no terminal (recurring jobs).
    pub headless: bool,
    /// It reports how full its context is (Phoenix's 60% warning).
    pub context_meter: bool,
    /// The model tiers it accepts, cheapest first; empty when it takes none.
    pub tiers: &'static [&'static str],
}

/// How to start a session: the program, its arguments, and the variables added to its environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// What a new or resumed session should be.
pub struct StartSpec<'a> {
    /// The session id on record: resumed when the harness can, else used for a new conversation (a new one is made when there is none).
    pub session_id: Option<&'a str>,
    /// The tier asked for; a harness ignores one it does not offer.
    pub model: Option<&'a str>,
    /// The instructions the session runs under (the identity's charter).
    pub system_prompt: &'a str,
    /// A folder the session may also edit (the state folder).
    pub add_dir: Option<&'a Path>,
    /// The first message.
    pub prompt: Option<&'a str>,
}

/// When Enter is pressed after typed text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enter {
    /// Text, a pause, then Enter: Claude Code's terminal reads text and Enter arriving in one write as a paste, and does not submit it.
    Apart,
    /// Text and Enter in one write (what the supervisor's reply wake has always done).
    Together,
}

/// How long `Enter::Apart` waits between the text and the Enter.
pub const ENTER_AFTER: Duration = Duration::from_millis(300);

pub trait Harness: Send + Sync {
    #[cfg_attr(not(test), allow(dead_code))]
    fn id(&self) -> &'static str;
    #[cfg_attr(not(test), allow(dead_code))]
    fn caps(&self) -> Caps;
    /// How to launch the session, and the session id to record for it.
    fn start(&self, spec: &StartSpec) -> (Launch, String);
    /// Whether the conversation `session_id` can be resumed.
    fn can_resume(&self, session_id: &str) -> bool;
    /// A run of `prompt` to completion with no terminal, at `model`.
    fn headless(&self, prompt: &str, model: &str) -> Launch;
    /// Types `text` and presses Enter through `write` (a session's terminal, the attach socket).
    fn type_into(&self, write: &mut dyn FnMut(&[u8]) -> Result<()>, text: &str, enter: Enter) -> Result<()> {
        type_line(write, text, enter)
    }
    /// Types `text` into the running session `name` of `host` and presses Enter, as if at the keyboard.
    fn send(&self, host: &mut Host, name: &str, text: &str, enter: Enter) -> Result<()> {
        self.type_into(&mut |b: &[u8]| host.input(name, b), text, enter)
    }
}

fn type_line(write: &mut dyn FnMut(&[u8]) -> Result<()>, text: &str, enter: Enter) -> Result<()> {
    match enter {
        Enter::Together => write(format!("{text}\r").as_bytes()),
        Enter::Apart => {
            write(text.as_bytes())?;
            std::thread::sleep(ENTER_AFTER);
            write(b"\r")
        }
    }
}

static CLAUDE_CODE: ClaudeCode = ClaudeCode;
static GENERIC: Generic = Generic;

/// The harness called `id`. An unknown one is Claude Code, with a warning in the core log.
pub fn for_id(id: &str) -> &'static dyn Harness {
    match id {
        "claude-code" => &CLAUDE_CODE,
        "generic" => &GENERIC,
        other => {
            paths::log("WARN", &format!("unknown harness {other:?}: using {DEFAULT}"));
            &CLAUDE_CODE
        }
    }
}

/// Whether `id` names a harness this build has.
pub fn known(id: &str) -> bool {
    matches!(id, "claude-code" | "generic")
}

/// The harness a row (an identity, a job) names in its `harness` field; one that names none is Claude Code, without a warning.
pub fn of(row: &Value) -> &'static dyn Harness {
    match row["harness"].as_str().filter(|h| !h.is_empty()) {
        Some(id) => for_id(id),
        None => &CLAUDE_CODE,
    }
}

/// A v4-shaped UUID from the clock and the process: a session only needs it unique and well-formed.
pub fn new_session_id() -> String {
    let mut h: [u8; 16] = [0; 16];
    let seed = format!("{:?}{}{:?}", std::time::SystemTime::now(), std::process::id(), std::thread::current().id());
    let mut x: u64 = seed.bytes().fold(0xcbf29ce484222325, |a, b| (a ^ u64::from(b)).wrapping_mul(0x100000001b3));
    for b in h.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 24) as u8;
    }
    h[6] = (h[6] & 0x0f) | 0x40;
    h[8] = (h[8] & 0x3f) | 0x80;
    let hex: Vec<String> = h.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", hex[0..4].concat(), hex[4..6].concat(), hex[6..8].concat(), hex[8..10].concat(), hex[10..16].concat())
}

// ---- Claude Code

pub struct ClaudeCode;

impl Harness for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn caps(&self) -> Caps {
        Caps { resume: true, hooks: true, headless: true, context_meter: true, tiers: &ids::MODELS }
    }

    /// `--resume <id>` when there is a conversation to resume, else `--session-id` a new (or never-used) id, the model tier, the state
    /// folder, the charter as an appended system prompt, then the prompt as the first message.
    fn start(&self, spec: &StartSpec) -> (Launch, String) {
        let resume = spec.session_id.map(|id| self.can_resume(id)).unwrap_or(false);
        let id = spec.session_id.map(String::from).unwrap_or_else(new_session_id);
        let tier = spec.model.filter(|m| ids::MODELS.contains(m));
        let mut args = vec![if resume { "--resume" } else { "--session-id" }.to_string(), id.clone()];
        if let Some(t) = tier {
            args.extend(["--model".to_string(), t.to_string()]);
        }
        if let Some(dir) = spec.add_dir {
            args.extend(["--add-dir".to_string(), dir.display().to_string()]);
        }
        args.extend(["--append-system-prompt".to_string(), spec.system_prompt.to_string()]);
        if let Some(p) = spec.prompt {
            args.push(p.to_string());
        }
        // A daemon started from inside a Claude Code session inherits `CLAUDE_CODE_CHILD_SESSION`, and claude then skips saving the
        // transcript of every session it hosts: `--resume` finds nothing. (The session host also sets this on every session it starts.)
        let env = vec![("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE".to_string(), "1".to_string())];
        (Launch { program: paths::claude_program(), args, env }, id)
    }

    /// claude writes no transcript until the first message, so only a conversation that has one can be resumed.
    fn can_resume(&self, session_id: &str) -> bool {
        let root = std::env::var_os("AGENTDESK_CLAUDE_PROJECTS").map(PathBuf::from).unwrap_or_else(|| paths::home_dir().join(".claude").join("projects"));
        std::fs::read_dir(root).into_iter().flatten().flatten().any(|d| d.path().join(format!("{session_id}.jsonl")).exists())
    }

    fn headless(&self, prompt: &str, model: &str) -> Launch {
        Launch { program: paths::claude_program(), args: vec!["-p".into(), prompt.into(), "--model".into(), model.into()], env: vec![] }
    }
}

// ---- any other command-line agent

/// A command-line agent that takes its prompt as a trailing argument and keeps no conversation: `AGENTDESK_GENERIC_AGENT`, else
/// settings.json's `generic_agent`, else `agent`. It has no system prompt, so the charter leads its first message.
pub struct Generic;

impl Generic {
    fn program() -> String {
        std::env::var("AGENTDESK_GENERIC_AGENT")
            .ok()
            .filter(|p| !p.is_empty())
            .or_else(|| paths::setting("generic_agent").as_str().filter(|p| !p.is_empty()).map(String::from))
            .unwrap_or_else(|| "agent".into())
    }
}

impl Harness for Generic {
    fn id(&self) -> &'static str {
        "generic"
    }

    fn caps(&self) -> Caps {
        Caps { resume: false, hooks: false, headless: true, context_meter: false, tiers: &[] }
    }

    fn start(&self, spec: &StartSpec) -> (Launch, String) {
        let first = match spec.prompt {
            Some(p) if spec.system_prompt.is_empty() => p.to_string(),
            Some(p) => format!("{}\n\n{p}", spec.system_prompt),
            None => spec.system_prompt.to_string(),
        };
        let args = if first.is_empty() { vec![] } else { vec![first] };
        (Launch { program: Self::program(), args, env: vec![] }, new_session_id())
    }

    fn can_resume(&self, _session_id: &str) -> bool {
        false
    }

    fn headless(&self, prompt: &str, _model: &str) -> Launch {
        Launch { program: Self::program(), args: vec![prompt.to_string()], env: vec![] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_types_text_then_enter_apart_or_together_as_the_caller_asks() {
        let mut writes: Vec<Vec<u8>> = vec![];
        let mut record = |b: &[u8]| -> Result<()> {
            writes.push(b.to_vec());
            Ok(())
        };
        for_id(DEFAULT).type_into(&mut record, "hi there", Enter::Apart).unwrap();
        for_id(DEFAULT).type_into(&mut record, "hi there", Enter::Together).unwrap();
        assert_eq!(writes, vec![b"hi there".to_vec(), b"\r".to_vec(), b"hi there\r".to_vec()], "apart: two writes, so the terminal does not read one paste");
        let mut tries = 0;
        let mut gone = |_: &[u8]| -> Result<()> {
            tries += 1;
            Err(anyhow::anyhow!("gone"))
        };
        assert!(for_id(DEFAULT).type_into(&mut gone, "x", Enter::Apart).is_err());
        assert_eq!(tries, 1, "no Enter after text that did not arrive");
    }

    #[test]
    fn an_unknown_harness_is_claude_code_and_a_row_that_names_none_is_too() {
        assert_eq!(for_id("no-such-agent").id(), "claude-code");
        assert_eq!(for_id("generic").id(), "generic");
        assert_eq!(of(&serde_json::json!({"name": "x"})).id(), "claude-code");
        assert_eq!(of(&serde_json::json!({"harness": ""})).id(), "claude-code");
        assert_eq!(of(&serde_json::json!({"harness": "generic"})).id(), "generic");
        assert!(known("claude-code") && known("generic") && !known("no-such-agent"));
        let caps = for_id(DEFAULT).caps();
        assert!(caps.resume && caps.hooks && caps.headless && caps.context_meter);
        assert_eq!(caps.tiers, &["haiku", "sonnet", "opus"]);
        assert!(!for_id("generic").caps().resume && for_id("generic").caps().tiers.is_empty());
    }

    #[test]
    fn claude_code_runs_a_headless_job_as_claude_p_at_its_model() {
        let l = for_id(DEFAULT).headless("do it", "haiku");
        assert_eq!(l.args, vec!["-p", "do it", "--model", "haiku"]);
        assert!(l.env.is_empty());
    }
}
