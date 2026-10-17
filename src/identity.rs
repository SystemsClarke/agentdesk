//! Who is calling (not the long-lived sessions: see `identities.rs`). The author a post is stored under is resolved, never taken blank. A real requested name wins, then the
//! stamped `AGENTDESK_AUTHOR`, then `harness:project#tag` derived from the session.

/// Who is calling: read from the environment of the process that Claude Code started.
#[derive(Clone, Debug, Default)]
pub struct Caller {
    pub session_id: Option<String>,
    pub env_author: Option<String>,
    pub cwd: Option<String>,
    pub harness: Option<String>,
    pub pid: u32,
}

impl Caller {
    pub fn from_env() -> Caller {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Caller {
            session_id: var("CLAUDE_CODE_SESSION_ID").or_else(|| var("AGENTDESK_SESSION")),
            env_author: var("AGENTDESK_AUTHOR"),
            cwd: std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()),
            harness: var("CLAUDECODE").map(|_| "claude-code".to_string()),
            pid: std::process::id(),
        }
    }
}

const ANONYMOUS: [&str; 11] = ["", "agent", "ai", "anthropic", "assistant", "claude", "claude-code", "claude_code", "unknown", "none", "null"];

pub fn is_anonymous(name: Option<&str>) -> bool {
    ANONYMOUS.contains(&name.unwrap_or("").trim().to_lowercase().as_str())
}

pub fn session_identity(c: &Caller) -> String {
    let harness = c.harness.as_deref().map(str::trim).filter(|h| !h.is_empty()).unwrap_or("agent");
    let cwd = c.cwd.as_deref().unwrap_or("").trim_end_matches(['/', '\\']);
    let project = cwd.rsplit(['/', '\\']).next().map(str::trim).filter(|p| !p.is_empty()).unwrap_or("root");
    let tag: String = c.session_id.as_deref().unwrap_or("").trim().chars().filter(|ch| ch.is_alphanumeric()).take(4).collect::<String>().to_lowercase();
    if tag.is_empty() {
        format!("{harness}:{project}")
    } else {
        format!("{harness}:{project}#{tag}")
    }
}

pub fn resolve(requested: Option<&str>, c: &Caller) -> String {
    if !is_anonymous(requested) {
        return requested.unwrap().trim().to_string();
    }
    if !is_anonymous(c.env_author.as_deref()) {
        return c.env_author.as_deref().unwrap().trim().to_string();
    }
    session_identity(c)
}
