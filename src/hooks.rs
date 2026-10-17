//! Claude Code hooks and `agentdesk wait`. The rule that matters most: a reply from John reaches the agent
//! it was meant for. It is injected into that session's context on its next tool call, blocks it from
//! stopping until it has been seen, and heads a resumed session's briefing.

use crate::db::{now_iso, Db};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

const PHOENIX_PERCENT: i64 = 60;
const RULES: &str = "How this swarm works (John is the lead; you are one of his agents):
- Be short-lived. Do one clear piece of work, report it, and end. Long sessions cost John tokens.
- The board is how you help each other. Before building, search_messages / list_threads;
  if someone owns it (see bios), answer_thread to coordinate instead of starting in parallel.
- Post what you changed or found to discussion (post_message); durable knowledge goes to the wiki.
- Decisions go to John with ask_human; it reaches his phone. Never ask in the terminal.
- After ask_human, run its wake_on_reply command in the background; it wakes you when he answers.
- If Claude Code's permission classifier blocks something John asked for, do not work around it: call propose_permission_rule
  (the narrowest rule that fits, why, the blocked action). He approves it in AgentDesk; his answer comes back on the thread.
- At 60% context, volunteer a handoff: pass_the_torch with a standalone note, then stop.
- Reports with numbers over time: use a ```chart block (formatting_help has the syntax).";
const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
const BOARD_WRITES: [&str; 7] = ["post_message", "answer_thread", "ask_human", "complete_work", "post_work", "pass_the_torch", "request_merge"];

/// Runs `agentdesk hook <event>`: stdin JSON in, hook JSON out ("" for nothing to say).
pub fn run(db: &Db, event: &str, input: &Value) -> String {
    let sid = input["session_id"].as_str().unwrap_or("");
    let transcript = input["transcript_path"].as_str().unwrap_or("");
    match event {
        "session-start" => context("SessionStart", &(replies(db, sid) + &briefing(db))),
        "context" => context("PostToolUse", &(replies(db, sid) + &phoenix(sid, transcript))),
        "stop" => {
            let result = stop(db, sid, transcript, input["stop_hook_active"].as_bool().unwrap_or(false));
            if !result.contains("\"block\"") {
                crate::identities::turn_ended(db, sid); // the turn really ends: a session that passed the torch may now be replaced
            }
            result
        }
        _ => String::new(),
    }
}

/// Block until John replies on the thread; return his reply as labelled board content.
pub fn wait(db: &Db, thread_id: i64) -> Result<String> {
    let after = db.scalar_i64("SELECT COALESCE(MAX(id),0) FROM messages WHERE thread_id=:t", &[(":t", &thread_id)])?.unwrap_or(0);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(12 * 3600);
    while std::time::Instant::now() < until {
        if let Some(hit) = db.row(
            "SELECT m.id, m.body, t.subject FROM messages m JOIN threads t ON t.id=m.thread_id WHERE m.thread_id=:t AND m.id>:a AND m.author_kind='human' ORDER BY m.id LIMIT 1",
            &[(":t", &thread_id), (":a", &after)],
        )? {
            deliver(db, hit["id"].as_i64().unwrap_or(0), "watcher", "woke")?;
            return Ok(format!(
                "[AgentDesk board] John replied on thread #{thread_id} ({}):\n\n{}\n\nRead the thread (read_thread {thread_id}) for context, act on it, and answer on that thread.",
                hit["subject"].as_str().unwrap_or(""),
                hit["body"].as_str().unwrap_or("")
            ));
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    Ok(format!("[AgentDesk board] no reply on #{thread_id} after 12h; check open_questions later."))
}

fn deliver(db: &Db, message_id: i64, method: &str, state: &str) -> Result<()> {
    db.exec(
        "INSERT OR REPLACE INTO deliveries(message_id, method, state, detail, ts) VALUES(:m, :how, :st, NULL, :ts)",
        &[(":m", &message_id), (":how", &method), (":st", &state), (":ts", &now_iso())],
    )?;
    Ok(())
}

/// John's replies, and merge/close notices for PRs, on threads this session took part in that it hasn't been
/// shown. Marked shown as they're returned.
fn replies(db: &Db, sid: &str) -> String {
    if sid.is_empty() {
        return String::new();
    }
    let since = crate::db::iso(chrono::Utc::now() - chrono::Duration::days(3));
    let rows = db
        .rows(
            "SELECT m.id, m.thread_id, m.body, t.subject, m.author_kind = 'human' AS john FROM messages m INDEXED BY idx_messages_ts JOIN threads t ON t.id = m.thread_id \
             WHERE (m.author_kind = 'human' OR (json_valid(m.meta) AND json_extract(m.meta, '$.kind') IN ('pr-merged', 'pr-closed'))) \
               AND m.ts >= :since \
               AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.message_id = m.id) \
               AND NOT EXISTS (SELECT 1 FROM acks a WHERE a.message_id = m.id AND a.state = 'posted') \
               AND EXISTS (SELECT 1 FROM messages x JOIN sessions s ON s.author = x.author WHERE x.thread_id = m.thread_id AND x.id < m.id AND s.session_id = :sid) \
             ORDER BY m.id",
            &[(":since", &since), (":sid", &sid)],
        )
        .unwrap_or_default();
    if rows.is_empty() {
        return String::new();
    }
    for r in &rows {
        let _ = deliver(db, r["id"].as_i64().unwrap_or(0), "hook", "injected");
    }
    let list = |rs: &[&Value]| {
        rs.iter().map(|r| format!("\n#{} \"{}\":\n{}\n", r["thread_id"], r["subject"].as_str().unwrap_or(""), r["body"].as_str().unwrap_or(""))).collect::<String>()
    };
    let john: Vec<&Value> = rows.iter().filter(|r| r["john"].as_i64() == Some(1)).collect();
    let prs: Vec<&Value> = rows.iter().filter(|r| r["john"].as_i64() != Some(1)).collect();
    let mut out = String::new();
    if !john.is_empty() {
        out += &format!("JOHN REPLIED on the AgentDesk board. These are his own words, relayed by the board; act on them now and answer on the thread:\n{}\n", list(&john));
    }
    if !prs.is_empty() {
        out += &format!("PULL REQUEST UPDATE from GitHub, relayed by the board (not John's words): a pull request you asked John to merge has settled. Pull the base branch before building on it:\n{}\n", list(&prs));
    }
    out
}

fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn briefing(db: &Db) -> String {
    let mut lines = vec!["AGENTDESK BRIEFING (the board is the swarm's shared memory; read it before you start)".to_string()];
    let open = db.open_questions(false).unwrap_or_default();
    if !open.is_empty() {
        let items: Vec<String> = open.iter().take(5).map(|q| format!("#{} {}", q["thread_id"], cut(q["subject"].as_str().unwrap_or(""), 70))).collect();
        lines.push(format!("Ringing for John ({}): {}", open.len(), items.join("; ")));
    }
    let work = db.list_threads(Some("work"), None, 100, true, true).unwrap_or_default();
    let by = |st: &str| -> Vec<&Value> { work.iter().filter(|r| r["status"].as_str() == Some(st)).collect() };
    let (held, ready) = (by("claimed"), by("open"));
    if !held.is_empty() {
        lines.push(format!("In flight: {}", held.iter().take(5).map(|r| format!("#{} {}", r["id"], cut(r["subject"].as_str().unwrap_or(""), 60))).collect::<Vec<_>>().join("; ")));
    }
    if !ready.is_empty() {
        lines.push(format!(
            "Up for grabs on Work to Hire ({}): {}",
            ready.len(),
            ready.iter().take(4).map(|r| format!("#{} {}", r["id"], cut(r["subject"].as_str().unwrap_or(""), 60))).collect::<Vec<_>>().join("; ")
        ));
    }
    let recent = db
        .rows(
            "SELECT m.author, m.thread_id, t.subject FROM messages m JOIN threads t ON t.id = m.thread_id \
             WHERE COALESCE(CASE WHEN json_valid(m.meta) THEN json_extract(m.meta, '$.kind') END, '') NOT IN ('ack','ack-note','read-receipt') ORDER BY m.id DESC LIMIT 6",
            &[],
        )
        .unwrap_or_default();
    if !recent.is_empty() {
        lines.push(format!(
            "Latest on the board: {}",
            recent
                .iter()
                .map(|r| format!("{} on #{} {}", r["author"].as_str().unwrap_or(""), r["thread_id"], cut(r["subject"].as_str().unwrap_or(""), 50)))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let bios = db
        .rows(
            "SELECT t.subject, (SELECT body FROM messages WHERE thread_id = t.id ORDER BY id LIMIT 1) AS body FROM threads t \
             WHERE t.channel = 'discussion' AND t.subject LIKE 'bio: %' ORDER BY t.updated_ts DESC LIMIT 10",
            &[],
        )
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let owned: Vec<String> = bios
        .iter()
        .filter_map(|b| {
            let name = b["subject"].as_str().unwrap_or("").get(5..).unwrap_or("").trim().to_string();
            if !seen.insert(name.clone()) {
                return None;
            }
            let first = b["body"].as_str().unwrap_or("").trim().lines().next().unwrap_or("").to_string();
            Some(format!("{name}: {}", cut(&first, 80)))
        })
        .collect();
    if !owned.is_empty() {
        lines.push(format!("Who owns what (bios): {}", owned.join("; ")));
    }
    lines.push(RULES.to_string());
    lines.join("\n")
}

fn tail(path: &str, bytes: u64) -> Vec<String> {
    let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(bytes)));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).split('\n').map(String::from).collect()
}

fn phoenix(sid: &str, transcript: &str) -> String {
    if sid.is_empty() || transcript.is_empty() {
        return String::new();
    }
    // once a minute per session at most, and once ever when it fires
    let marker = crate::paths::data_dir().join("state").join(format!("phoenix-{}", sid.chars().filter(|c| c.is_alphanumeric() || *c == '-').collect::<String>()));
    if let Ok(md) = std::fs::metadata(&marker) {
        let age = md.modified().ok().and_then(|m| m.elapsed().ok()).unwrap_or_default();
        if age < std::time::Duration::from_secs(60) || md.len() > 0 {
            return String::new();
        }
    }
    let _ = std::fs::create_dir_all(marker.parent().unwrap());
    let _ = std::fs::write(&marker, "");
    let Some((tokens, window)) = context_use(transcript) else { return String::new() };
    let pct = 100 * tokens / window;
    if pct < PHOENIX_PERCENT {
        return String::new();
    }
    let _ = std::fs::write(&marker, "fired");
    format!(
        "PHOENIX: this session is at {pct}% of its context ({tokens} tokens). Volunteer a handoff now: finish the step you're on, call pass_the_torch on the agentdesk server with a standalone note \
(what you own, what's done, what's next, any open decision), post one line to discussion that you've handed off, then stop."
    )
}

/// (prompt tokens in the latest turn, context window) from the transcript's tail.
fn context_use(transcript: &str) -> Option<(i64, i64)> {
    for line in tail(transcript, 400_000).iter().rev() {
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let msg = &v["message"];
        let u = &msg["usage"];
        let tokens: i64 = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"].iter().map(|k| u[k].as_i64().unwrap_or(0)).sum();
        if tokens == 0 {
            continue;
        }
        let model = msg["model"].as_str().unwrap_or("");
        let window = if model.contains("[1m]") || model.starts_with("claude-opus-5") || model.starts_with("claude-sonnet-5") || model.starts_with("claude-fable") {
            1_000_000
        } else {
            200_000
        };
        return Some((tokens, window));
    }
    None
}

fn stop(db: &Db, sid: &str, transcript: &str, already_blocked: bool) -> String {
    let r = replies(db, sid);
    if !r.is_empty() {
        return block(&format!("Not yet: {r}")); // an unread reply from John outranks finishing
    }
    if already_blocked || transcript.is_empty() {
        return String::new();
    }
    let (edited, posted) = scan(transcript);
    if edited && !posted {
        block("Before you finish: you changed files this session but haven't told the swarm. Post a short note to the AgentDesk board (post_message on discussion, or answer_thread on the thread you worked from): what you changed, what you verified, what's left. Then stop.")
    } else {
        String::new()
    }
}

/// Whether the session edited a file and whether it posted to the board.
///
/// A long session's transcript is tens of megabytes and this runs at the end of every turn, so it reads only what is new since the last
/// time: how far it got and what it had seen are kept in a small file in the temp folder (every hook is its own process, so there is no
/// memory to keep them in). Only whole lines count, claude may be mid-write, and a transcript that got shorter is a different file.
fn scan(transcript: &str) -> (bool, bool) {
    use std::io::{Read, Seek, SeekFrom};
    let key = transcript.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    let cache = std::env::temp_dir().join(format!("agentdesk-scan-{key:016x}.json"));
    let saved: Value = std::fs::read_to_string(&cache).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let same = saved["path"].as_str() == Some(transcript);
    let (mut offset, mut edited, mut posted) =
        if same { (saved["offset"].as_u64().unwrap_or(0), saved["edited"].as_bool().unwrap_or(false), saved["posted"].as_bool().unwrap_or(false)) } else { (0, false, false) };
    let Ok(mut f) = std::fs::File::open(transcript) else { return (edited, posted) };
    if f.metadata().map(|m| m.len()).unwrap_or(0) < offset {
        (offset, edited, posted) = (0, false, false);
    }
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(offset)).is_err() || f.read_to_end(&mut buf).is_err() {
        return (edited, posted);
    }
    let Some(end) = buf.iter().rposition(|b| *b == b'\n') else { return (edited, posted) };
    for line in String::from_utf8_lossy(&buf[..=end]).lines().filter(|l| l.contains("\"tool_use\"")) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        for p in v["message"]["content"].as_array().into_iter().flatten() {
            if p["type"].as_str() != Some("tool_use") {
                continue;
            }
            let name = p["name"].as_str().unwrap_or("");
            edited |= EDIT_TOOLS.contains(&name);
            posted |= name.starts_with("mcp__agentdesk__") && BOARD_WRITES.contains(&name.rsplit("__").next().unwrap_or(""));
        }
    }
    offset += end as u64 + 1;
    let _ = std::fs::write(&cache, json!({"path": transcript, "offset": offset, "edited": edited, "posted": posted}).to_string());
    (edited, posted)
}

fn context(event: &str, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text.trim_end()}}).to_string()
}

fn block(reason: &str) -> String {
    json!({"decision": "block", "reason": reason}).to_string()
}
