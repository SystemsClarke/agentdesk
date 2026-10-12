//! The board's MCP tools. Each returns the tool's JSON document as pretty text.

use crate::db::{self, anyhow_msg, err, Db, AGENT};
use crate::identity::{resolve, Caller};
use crate::tools_gen::TOOLS;
use crate::{paths, vault};
use anyhow::Result;
use regex::Regex;
use serde_json::{json, Value};

const ACK_BODY: &str = "Acknowledged - your reply has been picked up.";

pub fn tool_names() -> Vec<&'static str> {
    TOOLS.iter().map(|t| t.0).collect()
}

fn dump(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

fn ok(extra: Value) -> Value {
    let mut o = json!({"ok": true});
    if let (Some(m), Some(e)) = (o.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            m.insert(k.clone(), v.clone());
        }
    }
    o
}

struct Args<'a>(&'a Value);
impl Args<'_> {
    fn str(&self, k: &str) -> Result<String> {
        match self.0.get(k) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(Value::Null) | None => err(format!("missing required argument: {k}")),
            Some(v) => Ok(v.to_string()),
        }
    }
    fn opt(&self, k: &str) -> Option<String> {
        self.0.get(k).and_then(|v| v.as_str()).map(String::from)
    }
    fn int(&self, k: &str) -> Result<i64> {
        match self.0.get(k).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))) {
            Some(n) => Ok(n),
            None => err(format!("missing or non-integer argument: {k}")),
        }
    }
    fn int_or(&self, k: &str, d: i64) -> i64 {
        self.0.get(k).and_then(|v| v.as_i64()).unwrap_or(d)
    }
    fn opt_int(&self, k: &str) -> Option<i64> {
        self.0.get(k).and_then(|v| v.as_i64())
    }
    fn bool_or(&self, k: &str, d: bool) -> bool {
        self.0.get(k).and_then(|v| v.as_bool()).unwrap_or(d)
    }
}

/// After every write: note which session this author writes from, and post the acks it owes. Never fails the tool.
fn deliver_acks(db: &Db, author: &str, c: &Caller) {
    let _ = db.record_session(author, c.session_id.as_deref(), c.cwd.as_deref(), c.pid);
    match db.deliver_pending_acks(author, ACK_BODY) {
        Ok(done) if !done.is_empty() => paths::log("INFO", &format!("agent {author} acknowledged {} reply(ies)", done.len())),
        Err(e) => paths::log("WARN", &format!("ack delivery failed for {author}: {e}")),
        _ => {}
    }
}

fn receipt(db: &Db, thread_id: i64, agent: &str, body: &str) {
    if let Err(e) = db.post_read_receipt(thread_id, agent, body) {
        paths::log("WARN", &format!("read receipt failed for {agent} on #{thread_id}: {e}"));
    }
}

/// Runs one tool call. Always returns text: a failure is `{"error": "..."}`, as the agents expect.
pub fn call(db: &Db, c: &Caller, tool: &str, args: &Value) -> String {
    match dispatch(db, c, tool, &Args(args)) {
        Ok(v) => match v {
            Value::String(s) => s,
            other => dump(&other),
        },
        Err(e) => dump(&json!({"error": anyhow_msg(&e)})),
    }
}

fn dispatch(db: &Db, c: &Caller, tool: &str, a: &Args) -> Result<Value> {
    match tool {
        "formatting_help" => Ok(Value::String(FORMATTING_HELP.to_string())),
        "post_message" => {
            let channel = a.str("channel")?;
            if !db::CHANNELS.contains(&channel.as_str()) {
                return err(format!("channel must be one of ['question', 'discussion', 'wiki', 'work'], got '{channel}'"));
            }
            let (subject, body) = (a.str("subject")?, a.str("body")?);
            let who = resolve(a.opt("author").as_deref(), c);
            let thread_id = a.opt_int("thread_id");
            if let Some(t) = thread_id {
                db.enforce_closed_question(t)?;
            }
            let tid = db.start_thread(&channel, &subject, &who, AGENT, &body, None, thread_id)?;
            deliver_acks(db, &who, c);
            let mut r = ok(json!({"thread_id": tid}));
            if channel == "wiki" && thread_id.is_none() {
                r["vault"] = vault::mirror_thread(db, tid);
            }
            Ok(r)
        }
        "ask_human" => {
            let (subject, body) = (a.str("subject")?, a.str("body")?);
            let who = resolve(a.opt("author").as_deref(), c);
            let mut meta = serde_json::Map::new();
            if let Some(Value::Object(m)) = a.0.get("meta") {
                for (k, v) in m {
                    if k != "kind" {
                        meta.insert(k.clone(), v.clone());
                    }
                }
            }
            if !body.contains('?') {
                return err("a question has to ask something: John answers questions, he does not read updates. Say what you need decided and end it with the question \
(it must contain a '?'). If it is only news, post it to discussion instead.");
            }
            let tid = db.start_thread("question", &subject, &who, AGENT, &body, Some(&Value::Object(meta)), None)?;
            deliver_acks(db, &who, c);
            Ok(ok(json!({
                "thread_id": tid,
                "wake_on_reply": {"command": wait_command(tid), "run_in_background": true},
                "next": "Start wake_on_reply.command now with Bash run_in_background=true. It exits the moment John answers, which wakes you with his reply. Then carry on with other work or end your turn; do not poll."
            })))
        }
        "list_threads" => Ok(json!({"threads": db.list_threads(
            a.opt("channel").as_deref(), a.opt("status").as_deref(), a.int_or("limit", 50), a.bool_or("include_archived", true), true)?})),
        "read_thread" => {
            let id = a.int("thread_id")?;
            let who = resolve(a.opt("author").as_deref(), c);
            let data = db.get_thread(id)?;
            receipt(db, id, &who, &format!("{who} picked this thread up."));
            Ok(data)
        }
        "open_questions" => {
            let mut r = json!({"open_questions": db.open_questions(a.bool_or("include_archived", false))?});
            if let Some(author) = a.opt("author").filter(|s| !s.is_empty()) {
                r["torch_due"] = json!(db.torch_due(&resolve(Some(&author), c)));
            }
            Ok(r)
        }
        "pass_the_torch" => {
            let handoff = a.str("handoff")?;
            let who = resolve(a.opt("author").as_deref(), c);
            let ts = db.pass_the_torch(&who, &handoff)?;
            let note = format!("**Handoff recorded** ({ts}).\n\n{handoff}");
            let meta = json!({"kind": "handoff"});
            let bio = match db.bio_thread(&format!("bio: {who}"))? {
                Some(id) => {
                    db.reply(id, &who, AGENT, &note, None, Some(&meta))?;
                    id
                }
                None => db.start_thread("discussion", &format!("bio: {who}"), &who, AGENT, &note, Some(&meta), None)?,
            };
            deliver_acks(db, &who, c);
            Ok(ok(json!({"name": who, "updated_ts": ts, "path": null, "bio_thread_id": bio})))
        }
        "answer_thread" => {
            let (id, body) = (a.int("thread_id")?, a.str("body")?);
            let who = resolve(a.opt("author").as_deref(), c);
            db.get_thread(id)?; // a clean "no such thread" beats a foreign-key error
            db.enforce_closed_question(id)?;
            let mid = db.reply(id, &who, AGENT, &body, None, None)?;
            deliver_acks(db, &who, c);
            Ok(ok(json!({"thread_id": id, "message_id": mid})))
        }
        "search_messages" => Ok(json!({"results": db.search(&a.str("query")?, a.int_or("limit", 20))?})),
        "search_vault" => Ok(vault::search(&a.str("query")?, a.int_or("k", 8).max(0) as usize, a.int_or("full", 0))),
        "recent_messages" => Ok(json!({"messages": db.recent(a.int_or("limit", 30))?})),
        "list_mentions" => {
            let who = resolve(a.opt("name").as_deref(), c);
            Ok(json!({"mentions": db.list_mentions(&who, a.int_or("limit", 50))?}))
        }
        "post_work" => post_work(db, c, a),
        "list_work" => Ok(json!({"work": db.list_threads(Some("work"), a.opt("status").as_deref(), a.int_or("limit", 100), true, true)?})),
        "claim_work" => {
            let id = a.int("thread_id")?;
            let who = resolve(a.opt("author").as_deref(), c);
            let claimed = db.claim_task(id, &who)?;
            deliver_acks(db, &who, c);
            if claimed {
                receipt(db, id, &who, &format!("{who} took this work item."));
            }
            Ok(ok(json!({"claimed": claimed})))
        }
        "complete_work" => {
            let (id, note) = (a.int("thread_id")?, a.str("note")?);
            let who = resolve(a.opt("author").as_deref(), c);
            let completed = db.complete_task(id, &who)?;
            if completed {
                db.reply(id, &who, AGENT, &note, None, None)?;
            }
            deliver_acks(db, &who, c);
            Ok(ok(json!({"thread_id": id, "completed": completed})))
        }
        "request_merge" => request_merge(db, c, a),
        "propose_permission_rule" => crate::permissions::propose(db, c, a.0),
        _ => err(format!("no such tool: {tool}")),
    }
}

/// `agentdesk wait <thread>` as a shell command that works for this install.
pub fn wait_command(thread_id: i64) -> String {
    if cfg!(test) {
        return format!("wait {thread_id}");
    }
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "agentdesk".into());
    format!("\"{exe}\" wait {thread_id}")
}

fn post_work(db: &Db, c: &Caller, a: &Args) -> Result<Value> {
    let (subject, body) = (a.str("subject")?, a.str("body")?);
    let claim = a.opt("claim").unwrap_or_else(|| "auto".into());
    if claim != "auto" && claim != "anyone" {
        return err(format!("claim must be one of ['auto', 'anyone'], got '{claim}'"));
    }
    let priority = a.int_or("priority", 2);
    if !(0..=4).contains(&priority) {
        return err(format!("priority is 0 (urgent) to 4 (whenever), got {priority}"));
    }
    let model = a.opt("model");
    if let Some(m) = &model {
        if !["haiku", "sonnet", "opus"].contains(&m.as_str()) {
            return err(format!("model is haiku, sonnet or opus, got '{m}'"));
        }
    }
    let mut ids = Vec::new();
    for part in a.opt("after").unwrap_or_default().split([',', ' ']).filter(|p| !p.is_empty()) {
        match part.parse::<i64>() {
            Ok(n) => ids.push(n),
            Err(_) => return err(format!("after is work item ids separated by commas, got '{}'", a.opt("after").unwrap_or_default())),
        }
    }
    let every = a.opt("every");
    if let Some(e) = &every {
        if !valid_every(e) {
            return err(format!("every is HH:MM (daily, local time) or minutes like 30 or 2h, got '{e}'"));
        }
    }
    let mut meta = json!({"claim": claim});
    if let Some(e) = &every {
        meta["recur"] = json!(e.trim());
    }
    if priority != 2 {
        meta["priority"] = json!(priority);
    }
    if let Some(m) = model {
        meta["model"] = json!(m);
    }
    if a.bool_or("eager", false) {
        meta["eager"] = json!(true);
    }
    if a.bool_or("triage", false) {
        meta["triage"] = json!(true);
    }
    if !ids.is_empty() {
        meta["after"] = json!(ids);
    }
    let who = resolve(a.opt("author").as_deref(), c);
    let tid = db.start_thread("work", &subject, &who, AGENT, &body, Some(&meta), None)?;
    deliver_acks(db, &who, c);
    Ok(ok(json!({"thread_id": tid, "claim": claim})))
}

fn valid_every(e: &str) -> bool {
    let e = e.trim();
    Regex::new(r"^(\d{1,2}:\d{2}|\d+(\.\d+)?h?|\d+m)$").unwrap().is_match(e)
}

fn request_merge(db: &Db, c: &Caller, a: &Args) -> Result<Value> {
    let pr_url = a.str("pr_url")?;
    let who = resolve(a.opt("author").as_deref(), c);
    let re = Regex::new(r"(?i)^https?://(?:www\.)?github\.com/(?P<owner>[A-Za-z0-9._-]+)/(?P<repo>[A-Za-z0-9._-]+)/pull/(?P<number>\d+)(?:[/?#].*)?$").unwrap();
    let Some(m) = re.captures(pr_url.trim()) else {
        return err(format!("not a GitHub pull request URL: '{pr_url}' (expected https://github.com/<owner>/<repo>/pull/<number>)"));
    };
    let repo = format!("{}/{}", &m["owner"], &m["repo"]);
    let number: i64 = m["number"].parse().unwrap_or(0);
    let url = format!("https://github.com/{repo}/pull/{number}");
    let note = a.opt("note");
    let first = note.as_deref().map(str::trim).unwrap_or("");
    let title = if first.is_empty() { url.clone() } else { first.lines().next().unwrap_or(&url).to_string() };
    let title: String = title.chars().take(200).collect();
    let thread_id = a.opt_int("thread_id");
    let (pr_id, created) = db.register_pr(&url, &repo, number, &title, &who, thread_id)?;
    if let (Some(tid), true) = (thread_id, created) {
        let extra = note.as_deref().filter(|n| !n.is_empty()).map(|n| format!("\n\n{n}")).unwrap_or_default();
        db.reply(tid, &who, AGENT, &format!("Asking John to merge {repo}#{number}: {url}{extra}"), None, Some(&json!({"kind": "pr-request"})))?;
    }
    deliver_acks(db, &who, c);
    Ok(ok(json!({"pr_id": pr_id, "url": url, "repo": repo, "number": number, "created": created})))
}

const FORMATTING_HELP: &str = r##"How to format board messages. Everything here renders the same every time.

MARKDOWN: headings, **bold**, *italic*, ~~strike~~, `code`, [links](url), bare URLs,
bullets (nest by indenting 2 spaces), numbered lists, - [ ] / - [x] task lists, > quotes,
--- rules, and GitHub callouts: > [!NOTE] / [!TIP] / [!IMPORTANT] / [!WARNING] / [!CAUTION].

TABLES: ordinary pipe tables. Colons in the delimiter row align columns (|:--|--:|:-:|).
Shown as an aligned grid in the terminal window. Do NOT paste pre-drawn box-character tables in code blocks.

CODE: ```lang fences get a language label.

DIAGRAMS: ```mermaid blocks are kept as text in the terminal window.

Charts: put JSON in a ```chart block. Fields common to all: "type", "title", optional "unit".
  line      {"type":"line","title":"Compile time","unit":"min","x":["09-01","09-08"],"series":{"JAWS":[41,29]},"goal":25}
  bar       {"type":"bar","title":"Builds per lane","labels":["compile","sign"],"values":[41,12]}
  progress  {"type":"progress","title":"Migration","items":[{"label":"groups","done":7,"total":20}]}
  burndown  {"type":"burndown","title":"Sprint 2609","x":["Mon","Tue","Wed"],"remaining":[30,24,19]}
  timeline  {"type":"timeline","title":"Cutover","events":[{"when":"09-01","label":"canary"}]}
  sparkline {"type":"sparkline","title":"queue depth","values":[3,5,2,8,4]}
  stat      {"type":"stat","tiles":[{"label":"green builds","value":"94%","delta":"+6%"}]}
  table     {"type":"table","columns":["run","result"],"rows":[["#45","pass"],["#46","fail"]]}

Tips: one chart per idea; keep x labels short (dates as MM-DD); use "goal" on line charts
and "lower_is_better" on sparklines/stat tiles where down is good. A bad spec is shown
with its error rather than failing the message."##;
