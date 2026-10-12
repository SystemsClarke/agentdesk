//! The board store: one SQLite file (WAL) that every process opens for itself.
//! The SQL is kept textually close to the original `db.py` / `BoardStore.cs` so an
//! existing board opens unchanged.

use anyhow::Result;
use rusqlite::{types::ValueRef, Connection, ToSql};
use serde_json::{json, Map, Value};
use std::cell::Cell;
use std::path::Path;

pub const AGENT: &str = "agent";
pub const HUMAN: &str = "human";
pub const JOHN: &str = "john";
pub const CHANNELS: [&str; 4] = ["question", "discussion", "wiki", "work"];
pub const RECEIPT_KINDS: [&str; 3] = ["ack", "ack-note", "read-receipt"];
pub const QUESTION_WORDS: usize = 500;

/// A caller error, returned to the agent as `{"error": message}`.
#[derive(Debug)]
pub struct BoardError(pub String);
impl std::fmt::Display for BoardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for BoardError {}
pub fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(BoardError(msg.into()).into())
}

/// "Waiting on John": a question still open whose newest non-receipt message is not his.
pub const WAITING_SQL: &str = "(t.channel = 'question' AND t.status = 'open' AND NOT (\
COALESCE((SELECT m.author_kind FROM messages m WHERE m.thread_id = t.id \
 AND COALESCE(json_extract(m.meta, '$.kind'), '') NOT IN ('ack', 'ack-note', 'read-receipt') ORDER BY m.id DESC LIMIT 1), 'human') = 'human'))";

const FOLLOW_UP_WHERE: &str = "t.channel = 'question' AND f.author = t.opened_by AND f.author_kind <> 'human' \
 AND COALESCE(CASE WHEN json_valid(f.meta) THEN json_extract(f.meta,'$.kind') END,'') NOT IN ('ack','ack-note','read-receipt') \
 AND f.id > (SELECT MAX(h.id) FROM messages h WHERE h.thread_id = t.id AND h.author_kind = 'human')";
const FOLLOW_UP_MARK: &str = "CASE WHEN lower(substr(ltrim(f.body,' *_#>'),1,4)) = 'done' AND substr(ltrim(f.body,' *_#>'),5,1) NOT GLOB '[A-Za-z]' \
 THEN 'done' ELSE 'follow-up' END";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS threads (id INTEGER PRIMARY KEY AUTOINCREMENT, created_ts TEXT NOT NULL, updated_ts TEXT NOT NULL,
    channel TEXT NOT NULL, subject TEXT NOT NULL, opened_by TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'open', meta TEXT);
CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, thread_id INTEGER REFERENCES threads(id),
    author TEXT NOT NULL, author_kind TEXT NOT NULL, body TEXT NOT NULL, reply_to INTEGER REFERENCES messages(id), meta TEXT);
CREATE INDEX IF NOT EXISTS idx_messages_thread  ON messages(thread_id, id);
CREATE INDEX IF NOT EXISTS idx_messages_ts      ON messages(ts);
CREATE INDEX IF NOT EXISTS idx_threads_channel  ON threads(channel, status);
CREATE INDEX IF NOT EXISTS idx_threads_updated  ON threads(updated_ts);
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(body, content='messages', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
    INSERT INTO messages_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TABLE IF NOT EXISTS presence (author TEXT PRIMARY KEY, seen_ts TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS acks (message_id INTEGER PRIMARY KEY, thread_id INTEGER NOT NULL REFERENCES threads(id), agent TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending', created_ts TEXT NOT NULL, noted_ts TEXT, settled_ts TEXT);
CREATE TABLE IF NOT EXISTS receipts (agent TEXT NOT NULL, thread_id INTEGER NOT NULL REFERENCES threads(id), created_ts TEXT NOT NULL,
    message_id INTEGER, PRIMARY KEY (agent, thread_id));
CREATE TABLE IF NOT EXISTS pull_requests (id INTEGER PRIMARY KEY AUTOINCREMENT, url TEXT NOT NULL UNIQUE, repo TEXT NOT NULL,
    number INTEGER NOT NULL, title TEXT NOT NULL, requested_by TEXT NOT NULL, requested_ts TEXT NOT NULL, thread_id INTEGER REFERENCES threads(id),
    state TEXT NOT NULL DEFAULT 'open', checked_ts TEXT, settled_ts TEXT, last_error TEXT, notified_ts TEXT);
CREATE INDEX IF NOT EXISTS idx_prs_state ON pull_requests(state, id);
CREATE TABLE IF NOT EXISTS work_events (id INTEGER PRIMARY KEY AUTOINCREMENT, work_id INTEGER NOT NULL REFERENCES threads(id),
    ts TEXT NOT NULL, kind TEXT NOT NULL, body TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_work_events ON work_events(work_id, id);
CREATE TABLE IF NOT EXISTS handoffs (name TEXT PRIMARY KEY, body TEXT NOT NULL DEFAULT '', path TEXT, updated_ts TEXT NOT NULL,
    updated_by TEXT NOT NULL, torch_due INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS sessions (author TEXT PRIMARY KEY, session_id TEXT NOT NULL, cwd TEXT, pid INTEGER,
    channels INTEGER NOT NULL DEFAULT 0, seen_ts TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS deliveries (message_id INTEGER PRIMARY KEY, method TEXT NOT NULL, state TEXT NOT NULL, detail TEXT, ts TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS recurring_jobs (name TEXT PRIMARY KEY COLLATE NOCASE, folder TEXT NOT NULL, prompt TEXT NOT NULL,
    model TEXT NOT NULL DEFAULT 'sonnet', at TEXT, days TEXT NOT NULL DEFAULT 'daily', every_minutes REAL, catch_up_minutes REAL NOT NULL DEFAULT 240,
    enabled INTEGER NOT NULL DEFAULT 1, next_run_ts TEXT, last_run_ts TEXT, last_status TEXT, runs INTEGER NOT NULL DEFAULT 0,
    created_ts TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS permission_proposals (id INTEGER PRIMARY KEY AUTOINCREMENT, rule TEXT NOT NULL, scope TEXT NOT NULL, project_dir TEXT,
    reason TEXT NOT NULL, blocked TEXT, requested_by TEXT NOT NULL, thread_id INTEGER, status TEXT NOT NULL DEFAULT 'pending',
    created_ts TEXT NOT NULL, decided_ts TEXT, reverted_ts TEXT, settings_path TEXT, backup TEXT);
CREATE TABLE IF NOT EXISTS usage_samples (id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL UNIQUE, weekly_pct REAL NOT NULL,
    weekly_reset_ts TEXT, five_hour_pct REAL, five_hour_reset_ts TEXT, swarm_sessions INTEGER NOT NULL DEFAULT 0,
    haiku_sessions INTEGER NOT NULL DEFAULT 0, sonnet_sessions INTEGER NOT NULL DEFAULT 0, opus_sessions INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS identities (name TEXT PRIMARY KEY COLLATE NOCASE, folder TEXT NOT NULL, charter TEXT, host TEXT NOT NULL DEFAULT 'local',
    claude_session_id TEXT, pid INTEGER, state TEXT NOT NULL DEFAULT 'stopped', autostart INTEGER NOT NULL DEFAULT 0,
    created_ts TEXT NOT NULL, updated_ts TEXT NOT NULL, generation INTEGER NOT NULL DEFAULT 1, phoenix_msg INTEGER,
    model TEXT NOT NULL DEFAULT 'sonnet', running_model TEXT, phoenix_go INTEGER NOT NULL DEFAULT 0, wake_prompt TEXT);
CREATE TABLE IF NOT EXISTS phoenix_chain (identity TEXT NOT NULL COLLATE NOCASE, generation INTEGER NOT NULL, claude_session_id TEXT,
    handoff_msg INTEGER, ts TEXT NOT NULL, PRIMARY KEY (identity, generation));
CREATE TABLE IF NOT EXISTS goals (name TEXT PRIMARY KEY COLLATE NOCASE, objective TEXT NOT NULL, hypothesis TEXT, measure_cmd TEXT,
    measure_folder TEXT NOT NULL, success TEXT, samples INTEGER NOT NULL DEFAULT 1, max_members INTEGER NOT NULL DEFAULT 3,
    max_hours REAL NOT NULL DEFAULT 24, max_tokens INTEGER, cadence_minutes REAL NOT NULL DEFAULT 30, state TEXT NOT NULL DEFAULT 'draft',
    lead TEXT NOT NULL, thread_id INTEGER, started_ts TEXT, woke_ts TEXT, created_ts TEXT NOT NULL, updated_ts TEXT NOT NULL,
    standing INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS experiments (id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL COLLATE NOCASE, n INTEGER NOT NULL,
    change TEXT NOT NULL, owner TEXT, started_ts TEXT NOT NULL, measured_ts TEXT, value REAL, verdict TEXT, measuring INTEGER NOT NULL DEFAULT 0, UNIQUE (goal, n));
CREATE TABLE IF NOT EXISTS goal_members (identity TEXT PRIMARY KEY COLLATE NOCASE, goal TEXT NOT NULL COLLATE NOCASE, task TEXT NOT NULL,
    created_ts TEXT NOT NULL, work_id INTEGER);
CREATE TABLE IF NOT EXISTS slots (n INTEGER PRIMARY KEY CHECK (n BETWEEN 1 AND 10), channel_id TEXT, channel_name TEXT,
    persona_name TEXT, persona_icon TEXT, goal TEXT COLLATE NOCASE, updated_ts TEXT NOT NULL);
"#;

pub type Named<'a> = &'a [(&'a str, &'a dyn ToSql)];

pub struct Db {
    pub conn: Connection,
    /// Where this board lives: a background thread opens its own connection to the same file.
    pub path: std::path::PathBuf,
    depth: Cell<u32>,
}

thread_local! {
    /// Tests pin the clock to a tick counter (one second per timestamp) so recorded ids and timestamps match exactly.
    static TICK: Cell<Option<i64>> = const { Cell::new(None) };
}

#[cfg(test)]
pub fn pin_clock(start: Option<i64>) {
    TICK.with(|t| t.set(start));
}

pub fn now_iso() -> String {
    let at = match TICK.with(|t| t.get()) {
        Some(n) => {
            TICK.with(|t| t.set(Some(n + 1)));
            chrono::DateTime::from_timestamp(1767225600 + n, 0).unwrap()
        }
        None => chrono::Utc::now(),
    };
    at.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

/// Python's `json.dumps(ensure_ascii=False)`: `", "` and `": "` separators. The board's meta columns were written this
/// way by the original, and the golden tests compare them as text.
pub fn py_dumps(v: &Value) -> String {
    match v {
        Value::Array(a) => format!("[{}]", a.iter().map(py_dumps).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!("{{{}}}", o.iter().map(|(k, v)| format!("{}: {}", Value::String(k.clone()), py_dumps(v))).collect::<Vec<_>>().join(", ")),
        other => other.to_string(),
    }
}

fn s(v: &Value) -> Option<&str> {
    v.as_str()
}

pub fn dumps(meta: Option<&Value>) -> Option<String> {
    meta.map(py_dumps)
}

pub fn parse_obj(json: Option<&str>) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(json?) {
        Ok(Value::Object(o)) => Some(o),
        _ => None,
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-16000;")?;
        Ok(Db { conn, path: path.to_path_buf(), depth: Cell::new(0) })
    }

    /// The `PRAGMA user_version` this build stamps: an FNV hash of everything `init` does, so a board already at it is left alone. The
    /// original DDL, view rebuild and ALTERs took a write lock on every start (a hook is one process per call). Change `MIGRATION` whenever
    /// `init` gains an ALTER or a data fix. (The .NET build stamps its own hash; two builds sharing one board just re-run the idempotent
    /// init once each time the other has run.)
    fn schema_version() -> i64 {
        const MIGRATION: &str = "rust-2026-10-12.1";
        let mut h: u32 = 2166136261;
        for b in SCHEMA.bytes().chain(WAITING_SQL.bytes()).chain(MIGRATION.bytes()) {
            h = (h ^ u32::from(b)).wrapping_mul(16777619);
        }
        i64::from((h & 0x7fff_ffff) | 1)
    }

    fn add_column(&self, table: &str, column: &str, ddl: &str) -> Result<()> {
        let have: Vec<String> = self.rows(&format!("PRAGMA table_info({table})"), &[])?.iter().filter_map(|r| r["name"].as_str().map(String::from)).collect();
        if !have.iter().any(|c| c == column) {
            self.conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"))?;
        }
        Ok(())
    }

    /// Creates the schema and rebuilds the `open_questions` view from this file's rule.
    pub fn init(&self) -> Result<()> {
        let _: String = self.conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        let version = Self::schema_version();
        if self.conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? == version {
            return Ok(());
        }
        // one process at a time rebuilds the schema: twelve first-opens at once raced on DROP/CREATE VIEW
        self.tx(|| {
            if self.conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? == version {
                return Ok(());
            }
            self.conn.execute_batch(SCHEMA)?;
            self.conn.execute_batch("DROP VIEW IF EXISTS open_questions;")?;
            self.conn.execute_batch(&format!(
                "CREATE VIEW open_questions AS SELECT t.id AS thread_id, t.subject, t.opened_by, t.created_ts, t.updated_ts FROM threads t WHERE {WAITING_SQL};"
            ))?;
            let cols: Vec<String> = self.rows("PRAGMA table_info(pull_requests)", &[])?.iter().filter_map(|r| r["name"].as_str().map(String::from)).collect();
            if !cols.iter().any(|c| c == "source") {
                self.conn.execute_batch("ALTER TABLE pull_requests ADD COLUMN source TEXT NOT NULL DEFAULT 'agent-registered'")?;
            }
            for col in ["triage", "triage_ts"] {
                if !cols.iter().any(|c| c == col) {
                    self.conn.execute_batch(&format!("ALTER TABLE pull_requests ADD COLUMN {col} TEXT"))?;
                }
            }
            // Boards made by older builds: the columns later versions added.
            self.add_column("identities", "generation", "INTEGER NOT NULL DEFAULT 1")?;
            self.add_column("identities", "phoenix_msg", "INTEGER")?;
            self.add_column("identities", "model", "TEXT NOT NULL DEFAULT 'sonnet'")?;
            self.add_column("identities", "running_model", "TEXT")?;
            self.add_column("identities", "wake_prompt", "TEXT")?; // John's reply, for a stopped identity to start with (the Rust build's own column)
            self.add_column("identities", "phoenix_go", "INTEGER NOT NULL DEFAULT 0")?; // the turn that passed the torch has ended (the Rust build's own column)
            self.add_column("goals", "standing", "INTEGER NOT NULL DEFAULT 0")?;
            self.add_column("experiments", "measuring", "INTEGER NOT NULL DEFAULT 0")?; // experiment_done asked for a measurement (the Rust build's own column)
            self.add_column("goal_members", "work_id", "INTEGER")?;
            for tier in ["haiku", "sonnet", "opus"] {
                self.add_column("usage_samples", &format!("{tier}_sessions"), "INTEGER NOT NULL DEFAULT 0")?;
            }
            self.conn.execute_batch(&format!("PRAGMA user_version={version}"))?;
            Ok(())
        })
    }

    // ---- helpers

    pub fn exec(&self, sql: &str, p: Named) -> Result<usize> {
        Ok(self.conn.prepare_cached(sql)?.execute(p)?)
    }

    pub fn rows(&self, sql: &str, p: Named) -> Result<Vec<Value>> {
        let mut st = self.conn.prepare_cached(sql)?;
        let names: Vec<String> = st.column_names().iter().map(|c| c.to_string()).collect();
        let mut out = Vec::new();
        let mut q = st.query(p)?;
        while let Some(r) = q.next()? {
            let mut o = Map::new();
            for (i, n) in names.iter().enumerate() {
                o.insert(
                    n.clone(),
                    match r.get_ref(i)? {
                        ValueRef::Null | ValueRef::Blob(_) => Value::Null,
                        ValueRef::Integer(v) => json!(v),
                        ValueRef::Real(v) => json!(v),
                        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
                    },
                );
            }
            out.push(Value::Object(o));
        }
        Ok(out)
    }

    pub fn row(&self, sql: &str, p: Named) -> Result<Option<Value>> {
        Ok(self.rows(sql, p)?.into_iter().next())
    }

    pub fn scalar_i64(&self, sql: &str, p: Named) -> Result<Option<i64>> {
        let mut st = self.conn.prepare_cached(sql)?;
        let mut q = st.query(p)?;
        Ok(match q.next()? {
            Some(r) => r.get::<_, Option<i64>>(0)?,
            None => None,
        })
    }

    pub fn scalar_str(&self, sql: &str, p: Named) -> Result<Option<String>> {
        let mut st = self.conn.prepare_cached(sql)?;
        let mut q = st.query(p)?;
        Ok(match q.next()? {
            Some(r) => r.get::<_, Option<String>>(0)?,
            None => None,
        })
    }

    fn insert(&self, sql: &str, p: Named) -> Result<i64> {
        self.exec(sql, p)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// One transaction (one fsync). Inside another transaction it simply runs.
    pub fn tx<T>(&self, body: impl FnOnce() -> Result<T>) -> Result<T> {
        if self.depth.get() > 0 {
            return body();
        }
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        self.depth.set(1);
        let r = body();
        self.depth.set(0);
        match r {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    pub fn data_version(&self) -> i64 {
        self.scalar_i64("PRAGMA data_version", &[]).ok().flatten().unwrap_or(0)
    }

    // ---- writes

    fn touch_presence(&self, author: &str, kind: &str) -> Result<()> {
        if kind == AGENT {
            self.exec(
                "INSERT INTO presence (author, seen_ts) VALUES (:a,:ts) ON CONFLICT(author) DO UPDATE SET seen_ts=excluded.seen_ts",
                &[(":a", &author), (":ts", &now_iso())],
            )?;
        }
        Ok(())
    }

    fn enforce_question_length(channel: &str, kind: &str, body: &str) -> Result<()> {
        let words = body.split_whitespace().count();
        if channel != "question" || kind != AGENT || words <= QUESTION_WORDS {
            return Ok(());
        }
        err(format!(
            "this question is {words} words; John asked for questions and replies in a question thread to stay under {QUESTION_WORDS}. \
State the decision you need in a few sentences and put any supporting detail in a linked discussion thread or document instead of in the question itself."
        ))
    }

    /// A question is one question and one answer: after John answered, an agent gets one closing note.
    pub fn enforce_closed_question(&self, thread_id: i64) -> Result<()> {
        let Some(t) = self.row("SELECT channel FROM threads WHERE id=:id", &[(":id", &thread_id)])? else { return Ok(()) };
        if s(&t["channel"]) != Some("question") {
            return Ok(());
        }
        let Some(john) = self.scalar_i64("SELECT MAX(id) FROM messages WHERE thread_id=:t AND author_kind='human'", &[(":t", &thread_id)])? else { return Ok(()) };
        let since = self.scalar_i64(
            "SELECT COUNT(*) FROM messages WHERE thread_id=:t AND id>:j AND author_kind<>'human' \
             AND COALESCE(CASE WHEN json_valid(meta) THEN json_extract(meta,'$.kind') END,'') NOT IN ('ack','ack-note','read-receipt','phoenix','pr-request')",
            &[(":t", &thread_id), (":j", &john)],
        )?;
        if since.unwrap_or(0) >= 1 {
            return err("John already answered this question and you have had your closing note: a question is one question and one answer. \
If you need something else from him, ask a new question with ask_human.");
        }
        Ok(())
    }

    /// The message's meta plus `mentions`, only when the body names someone.
    fn message_meta(meta: Option<&Value>, body: &str) -> Option<String> {
        let names = mentions(body);
        if names.is_empty() {
            return dumps(meta);
        }
        let mut copy = match meta {
            Some(Value::Object(o)) => o.clone(),
            _ => Map::new(),
        };
        copy.insert("mentions".into(), json!(names));
        Some(py_dumps(&Value::Object(copy)))
    }

    /// Post a message, creating a thread unless `thread_id` is given. Returns the THREAD id.
    #[allow(clippy::too_many_arguments)]
    pub fn start_thread(&self, channel: &str, subject: &str, opened_by: &str, kind: &str, body: &str, meta: Option<&Value>, thread_id: Option<i64>) -> Result<i64> {
        if !CHANNELS.contains(&channel) {
            return err(format!("channel must be one of ('question', 'discussion', 'wiki', 'work'), got '{channel}'"));
        }
        Self::enforce_question_length(channel, kind, body)?;
        self.tx(|| {
            let ts = now_iso();
            let tid = match thread_id {
                None => self.insert(
                    "INSERT INTO threads (created_ts, updated_ts, channel, subject, opened_by, status, meta) VALUES (:ts,:ts,:channel,:subject,:by,:status,:meta)",
                    &[
                        (":ts", &ts),
                        (":channel", &channel),
                        (":subject", &subject),
                        (":by", &opened_by),
                        (":status", &if channel == "question" || channel == "work" { "open" } else { "fyi" }),
                        (":meta", &dumps(meta)),
                    ],
                )?,
                Some(id) => {
                    if self.scalar_i64("SELECT 1 FROM threads WHERE id=:id", &[(":id", &id)])?.is_none() {
                        return err(format!("no such thread: {id}"));
                    }
                    self.exec("UPDATE threads SET updated_ts=:ts WHERE id=:id", &[(":ts", &ts), (":id", &id)])?;
                    id
                }
            };
            self.insert(
                "INSERT INTO messages (ts, thread_id, author, author_kind, body, reply_to, meta) VALUES (:ts,:tid,:author,:kind,:body,NULL,:meta)",
                &[(":ts", &ts), (":tid", &tid), (":author", &opened_by), (":kind", &kind), (":body", &body), (":meta", &Self::message_meta(meta, body))],
            )?;
            self.touch_presence(opened_by, kind)?;
            Ok(tid)
        })
    }

    pub fn reply(&self, thread_id: i64, author: &str, kind: &str, body: &str, reply_to: Option<i64>, meta: Option<&Value>) -> Result<i64> {
        if let Some(ch) = self.scalar_str("SELECT channel FROM threads WHERE id=:id", &[(":id", &thread_id)])? {
            Self::enforce_question_length(&ch, kind, body)?;
        }
        self.tx(|| {
            let ts = now_iso();
            let id = self.insert(
                "INSERT INTO messages (ts, thread_id, author, author_kind, body, reply_to, meta) VALUES (:ts,:tid,:author,:kind,:body,:rt,:meta)",
                &[(":ts", &ts), (":tid", &thread_id), (":author", &author), (":kind", &kind), (":body", &body), (":rt", &reply_to), (":meta", &Self::message_meta(meta, body))],
            )?;
            self.exec("UPDATE threads SET updated_ts=:ts WHERE id=:id", &[(":ts", &ts), (":id", &thread_id)])?;
            self.touch_presence(author, kind)?;
            Ok(id)
        })
    }

    // ---- the work queue: claim and complete are each one conditional UPDATE

    fn thread_meta(&self, id: i64) -> Result<Map<String, Value>> {
        let Some(t) = self.row("SELECT channel, status, meta FROM threads WHERE id=:id", &[(":id", &id)])? else {
            return err(format!("no such thread: {id}"));
        };
        Ok(parse_obj(s(&t["meta"])).unwrap_or_default())
    }

    fn move_work(&self, id: i64, agent: &str, ts: &str, meta: &Map<String, Value>, from: &str, to: &str) -> Result<bool> {
        let moved = self.exec(
            "UPDATE threads SET status=:to, updated_ts=:ts, meta=:meta WHERE id=:id AND channel='work' AND status=:from",
            &[(":to", &to), (":ts", &ts), (":meta", &py_dumps(&Value::Object(meta.clone()))), (":id", &id), (":from", &from)],
        )? == 1;
        if moved {
            self.touch_presence(agent, AGENT)?;
        }
        Ok(moved)
    }

    pub fn claim_task(&self, id: i64, agent: &str) -> Result<bool> {
        let ts = now_iso();
        let mut meta = self.thread_meta(id)?;
        meta.insert("assignee".into(), json!(agent));
        meta.insert("claimed_ts".into(), json!(ts));
        self.move_work(id, agent, &ts, &meta, "open", "claimed")
    }

    pub fn complete_task(&self, id: i64, agent: &str) -> Result<bool> {
        let ts = now_iso();
        let mut meta = self.thread_meta(id)?;
        if meta.get("assignee").and_then(|v| v.as_str()) != Some(agent) {
            return Ok(false);
        }
        meta.insert("completed_ts".into(), json!(ts));
        self.move_work(id, agent, &ts, &meta, "claimed", "done")
    }

    /// A claimed work item changes hands: only the current assignee can pass it on.
    pub fn hand_over_task(&self, id: i64, from: &str, to: &str) -> Result<bool> {
        let mut meta = self.thread_meta(id)?;
        if meta.get("assignee").and_then(|v| v.as_str()) != Some(from) {
            return Ok(false);
        }
        meta.insert("assignee".into(), json!(to));
        Ok(self.exec(
            "UPDATE threads SET updated_ts=:ts, meta=:meta WHERE id=:id AND channel='work' AND status='claimed'",
            &[(":ts", &now_iso()), (":meta", &py_dumps(&Value::Object(meta))), (":id", &id)],
        )? == 1)
    }

    /// Puts a claimed work item back on the queue, open and unassigned.
    pub fn reopen_task(&self, id: i64) -> Result<bool> {
        let mut meta = self.thread_meta(id)?;
        meta.remove("assignee");
        meta.remove("claimed_ts");
        Ok(self.exec(
            "UPDATE threads SET status='open', updated_ts=:ts, meta=:meta WHERE id=:id AND channel='work' AND status='claimed'",
            &[(":ts", &now_iso()), (":meta", &py_dumps(&Value::Object(meta))), (":id", &id)],
        )? == 1)
    }

    // ---- acknowledgements of John's replies

    pub fn deliver_pending_acks(&self, agent: &str, body: &str) -> Result<Vec<i64>> {
        let mut delivered = Vec::new();
        for row in self.rows("SELECT message_id, thread_id FROM acks WHERE agent=:a AND state='pending' ORDER BY message_id", &[(":a", &agent)])? {
            let mid = row["message_id"].as_i64().unwrap_or(0);
            let done = self.tx(|| {
                if self.exec(
                    "UPDATE acks SET state='posted', settled_ts=:ts WHERE message_id=:m AND agent=:a AND state='pending'",
                    &[(":ts", &now_iso()), (":m", &mid), (":a", &agent)],
                )? != 1
                {
                    return Ok(false);
                }
                let Some(tid) = self.scalar_i64("SELECT thread_id FROM messages WHERE id=:m", &[(":m", &mid)])? else {
                    return err(format!("no such message: {mid}"));
                };
                self.reply(tid, agent, AGENT, body, Some(mid), Some(&json!({"kind": "ack", "ack_for": mid})))?;
                Ok(true)
            })?;
            if done {
                delivered.push(mid);
            }
        }
        Ok(delivered)
    }

    // ---- John

    fn thread_row(&self, id: i64) -> Result<Value> {
        match self.row("SELECT channel, status, meta FROM threads WHERE id=:id", &[(":id", &id)])? {
            Some(t) => Ok(t),
            None => err(format!("no such thread: {id}")),
        }
    }

    /// John's reply: settles an open question and queues the asking agent's ack. Returns the message id.
    pub fn john_replies(&self, thread_id: i64, body: &str) -> Result<i64> {
        let t = self.thread_row(thread_id)?;
        let mid = self.reply(thread_id, JOHN, HUMAN, body, None, None)?;
        if s(&t["channel"]) != Some("question") {
            return Ok(mid);
        }
        if s(&t["status"]) == Some("open") {
            self.exec("UPDATE threads SET status='answered', updated_ts=:ts WHERE id=:tid", &[(":ts", &now_iso()), (":tid", &thread_id)])?;
        }
        if self.scalar_str("SELECT author_kind FROM messages WHERE thread_id=:tid ORDER BY id LIMIT 1", &[(":tid", &thread_id)])?.as_deref() == Some(AGENT) {
            self.exec(
                "INSERT OR IGNORE INTO acks (message_id, thread_id, agent, state, created_ts) SELECT :mid, id, opened_by, 'pending', :ts FROM threads WHERE id=:tid",
                &[(":mid", &mid), (":ts", &now_iso()), (":tid", &thread_id)],
            )?;
        }
        Ok(mid)
    }

    /// John closes a question without answering. False if it is not a live question.
    pub fn close_question(&self, thread_id: i64) -> Result<bool> {
        let t = self.thread_row(thread_id)?;
        if s(&t["channel"]) != Some("question") || s(&t["status"]) == Some("archived") {
            return Ok(false);
        }
        let mut meta = parse_obj(s(&t["meta"])).unwrap_or_default();
        meta.remove("archive_hold");
        Ok(self.exec(
            "UPDATE threads SET status='closed', updated_ts=:ts, meta=:meta WHERE id=:tid",
            &[(":ts", &now_iso()), (":meta", &py_dumps(&Value::Object(meta))), (":tid", &thread_id)],
        )? == 1)
    }

    /// John brings an archived question back with the status it was settled with, held off the sweep.
    pub fn unarchive(&self, thread_id: i64) -> Result<bool> {
        let t = self.thread_row(thread_id)?;
        if s(&t["status"]) != Some("archived") {
            return Ok(false);
        }
        let mut meta = parse_obj(s(&t["meta"])).unwrap_or_default();
        let settled = meta.get("archived_from").and_then(|v| v.as_str()).filter(|v| !v.is_empty()).unwrap_or("answered").to_string();
        meta.remove("archived_from");
        meta.insert("archive_hold".into(), json!(true));
        Ok(self.exec(
            "UPDATE threads SET status=:s, updated_ts=:ts, meta=:meta WHERE id=:tid",
            &[(":s", &settled), (":ts", &now_iso()), (":meta", &py_dumps(&Value::Object(meta))), (":tid", &thread_id)],
        )? == 1)
    }

    /// Archives a question thread (the sweep: a settled question leaves the Questions tab).
    pub fn archive(&self, thread_id: i64) -> Result<bool> {
        let t = self.thread_row(thread_id)?;
        let status = s(&t["status"]).unwrap_or("");
        if s(&t["channel"]) != Some("question") || !(status == "answered" || status == "closed") {
            return Ok(false);
        }
        let mut meta = parse_obj(s(&t["meta"])).unwrap_or_default();
        meta.insert("archived_from".into(), json!(status));
        Ok(self.exec(
            "UPDATE threads SET status='archived', updated_ts=:ts, meta=:meta WHERE id=:tid",
            &[(":ts", &now_iso()), (":meta", &py_dumps(&Value::Object(meta))), (":tid", &thread_id)],
        )? == 1)
    }

    // ---- read receipts: once per (agent, thread) by PRIMARY KEY

    pub fn post_read_receipt(&self, thread_id: i64, agent: &str, body: &str) -> Result<bool> {
        let warranted = self.rows("SELECT author, meta FROM messages WHERE thread_id = :t", &[(":t", &thread_id)])?.iter().any(|r| {
            let kind = parse_obj(s(&r["meta"])).and_then(|m| m.get("kind").and_then(|k| k.as_str().map(String::from)));
            s(&r["author"]) != Some(agent) && !kind.map(|k| RECEIPT_KINDS.contains(&k.as_str())).unwrap_or(false)
        });
        if !warranted {
            return Ok(false);
        }
        let ts = now_iso();
        self.tx(|| {
            if self.exec("INSERT OR IGNORE INTO receipts (agent, thread_id, created_ts) VALUES (:a,:t,:ts)", &[(":a", &agent), (":t", &thread_id), (":ts", &ts)])? != 1 {
                return Ok(false);
            }
            let mid = self.insert(
                "INSERT INTO messages (ts, thread_id, author, author_kind, body, reply_to, meta) VALUES (:ts,:tid,:author,:kind,:body,NULL,:meta)",
                &[(":ts", &ts), (":tid", &thread_id), (":author", &agent), (":kind", &AGENT), (":body", &body), (":meta", &py_dumps(&json!({"kind": "read-receipt"})))],
            )?;
            self.exec("UPDATE receipts SET message_id=:m WHERE agent=:a AND thread_id=:t", &[(":m", &mid), (":a", &agent), (":t", &thread_id)])?;
            Ok(true)
        })
    }

    /// Put a PR on the merge list. The UNIQUE url decides `created`.
    pub fn register_pr(&self, url: &str, repo: &str, number: i64, title: &str, by: &str, thread_id: Option<i64>) -> Result<(i64, bool)> {
        if self.exec(
            "INSERT OR IGNORE INTO pull_requests (url, repo, number, title, requested_by, requested_ts, thread_id, state, source) \
             VALUES (:url,:repo,:number,:title,:by,:ts,:tid,'open','agent-registered')",
            &[(":url", &url), (":repo", &repo), (":number", &number), (":title", &title), (":by", &by), (":ts", &now_iso()), (":tid", &thread_id)],
        )? > 0
        {
            return Ok((self.conn.last_insert_rowid(), true));
        }
        Ok((self.scalar_i64("SELECT id FROM pull_requests WHERE url=:url", &[(":url", &url)])?.unwrap_or(0), false))
    }

    pub fn pass_the_torch(&self, name: &str, body: &str) -> Result<String> {
        let ts = now_iso();
        self.exec(
            "INSERT INTO handoffs (name, body, path, updated_ts, updated_by, torch_due) VALUES (:n, :b, NULL, :ts, :n, 0) \
             ON CONFLICT(name) DO UPDATE SET body=excluded.body, path=excluded.path, updated_ts=excluded.updated_ts, updated_by=excluded.updated_by, torch_due=0",
            &[(":n", &name), (":b", &body), (":ts", &ts)],
        )?;
        Ok(ts)
    }

    pub fn record_session(&self, author: &str, session_id: Option<&str>, cwd: Option<&str>, pid: u32) -> Result<()> {
        let Some(sid) = session_id.filter(|v| !v.is_empty()) else { return Ok(()) };
        if author.is_empty() {
            return Ok(());
        }
        self.exec(
            "INSERT INTO sessions(author, session_id, cwd, pid, channels, seen_ts) VALUES(:a,:s,:cwd,:pid,0,:ts) \
             ON CONFLICT(author) DO UPDATE SET session_id=excluded.session_id, cwd=excluded.cwd, pid=excluded.pid, channels=excluded.channels, seen_ts=excluded.seen_ts",
            &[(":a", &author), (":s", &sid), (":cwd", &cwd), (":pid", &(pid as i64)), (":ts", &now_iso())],
        )?;
        Ok(())
    }

    // ---- reads

    pub fn get_thread(&self, id: i64) -> Result<Value> {
        let sql = format!("SELECT t.*, (CASE WHEN {WAITING_SQL} THEN 1 ELSE 0 END) AS waiting FROM threads t WHERE id=:id");
        let Some(t) = self.row(&sql, &[(":id", &id)])? else { return err(format!("no such thread: {id}")) };
        Ok(json!({"thread": t, "messages": self.rows("SELECT * FROM messages WHERE thread_id=:id ORDER BY id", &[(":id", &id)])?}))
    }

    pub fn list_threads(&self, channel: Option<&str>, status: Option<&str>, limit: i64, include_archived: bool, last_body: bool) -> Result<Vec<Value>> {
        let mut filters = Vec::new();
        let mut args: Vec<(&str, &dyn ToSql)> = vec![(":limit", &limit)];
        let ch = channel.filter(|c| !c.is_empty());
        let st = status.filter(|c| !c.is_empty());
        if let Some(c) = &ch {
            filters.push("t.channel = :channel");
            args.push((":channel", c));
        }
        if let Some(c) = &st {
            filters.push("t.status = :status");
            args.push((":status", c));
        }
        if !include_archived && st != Some("archived") {
            filters.push("t.status <> 'archived'");
        }
        let filter = if filters.is_empty() { String::new() } else { format!(" WHERE {}", filters.join(" AND ")) };
        let last_body_sql = " (SELECT body FROM messages WHERE thread_id=t.id ORDER BY id DESC LIMIT 1) AS last_body,";
        let sql = format!(
            "SELECT t.*, COUNT(m.id) AS message_count,{} \
             (SELECT author FROM messages WHERE thread_id=t.id AND COALESCE(CASE WHEN json_valid(meta) THEN json_extract(meta,'$.kind') END,'') NOT IN ('ack','ack-note','read-receipt') ORDER BY id DESC LIMIT 1) AS last_author, \
             (SELECT COALESCE(CASE WHEN a.state='posted' THEN 'picked-up' END, d.state, CASE WHEN a.message_id IS NOT NULL THEN 'pending' END, '') || '|' || h.ts \
              FROM messages h LEFT JOIN acks a ON a.message_id=h.id LEFT JOIN deliveries d ON d.message_id=h.id \
              WHERE h.thread_id=t.id AND h.author_kind='human' ORDER BY h.id DESC LIMIT 1) AS delivery, \
             (CASE WHEN {WAITING_SQL} THEN 1 ELSE 0 END) AS waiting, \
             (SELECT {FOLLOW_UP_MARK} FROM messages f WHERE f.thread_id=t.id AND {FOLLOW_UP_WHERE} ORDER BY f.id DESC LIMIT 1) AS follow_up \
             FROM (SELECT * FROM threads t{filter} ORDER BY t.updated_ts DESC, t.id DESC LIMIT :limit) t LEFT JOIN messages m ON m.thread_id = t.id \
             GROUP BY t.id ORDER BY t.updated_ts DESC, t.id DESC",
            if last_body { last_body_sql } else { "" }
        );
        self.rows(&sql, &args)
    }

    pub fn open_questions(&self, include_archived: bool) -> Result<Vec<Value>> {
        if include_archived {
            self.rows("SELECT id AS thread_id, subject, opened_by, created_ts, updated_ts FROM threads WHERE channel='question' AND status IN ('open', 'archived') ORDER BY updated_ts DESC", &[])
        } else {
            self.rows("SELECT * FROM open_questions ORDER BY updated_ts DESC", &[])
        }
    }

    /// Every opener's follow-up with an id in (after, up_to], oldest first (the follow-up toasts).
    pub fn follow_ups(&self, after: i64, up_to: i64) -> Result<Vec<Value>> {
        self.rows(
            &format!(
                "SELECT f.id AS message_id, f.thread_id, t.subject, f.author, f.body, {FOLLOW_UP_MARK} AS mark \
                 FROM messages f JOIN threads t ON t.id = f.thread_id WHERE f.id > :after AND f.id <= :upTo AND {FOLLOW_UP_WHERE} ORDER BY f.id"
            ),
            &[(":after", &after), (":upTo", &up_to)],
        )
    }

    pub fn max_message_id(&self) -> i64 {
        self.scalar_i64("SELECT MAX(id) FROM messages", &[]).ok().flatten().unwrap_or(0)
    }

    pub fn torch_due(&self, name: &str) -> bool {
        self.scalar_i64("SELECT torch_due FROM handoffs WHERE name=:n", &[(":n", &name)]).ok().flatten().unwrap_or(0) != 0
    }

    pub fn bio_thread(&self, subject: &str) -> Result<Option<i64>> {
        self.scalar_i64("SELECT id FROM threads WHERE channel='discussion' AND subject=:s ORDER BY id LIMIT 1", &[(":s", &subject)])
    }

    pub fn recent(&self, limit: i64) -> Result<Vec<Value>> {
        self.rows("SELECT m.*, t.subject, t.channel FROM messages m JOIN threads t ON t.id = m.thread_id ORDER BY m.id DESC LIMIT :limit", &[(":limit", &limit)])
    }

    pub fn list_mentions(&self, name: &str, limit: i64) -> Result<Vec<Value>> {
        self.rows(
            "SELECT m.*, t.subject, t.channel FROM messages m JOIN threads t ON t.id = m.thread_id, json_each(m.meta, '$.mentions') je \
             WHERE m.meta IS NOT NULL AND json_valid(m.meta) AND json_extract(m.meta, '$.mentions') IS NOT NULL \
             AND lower(je.value) = lower(:name) ORDER BY m.id DESC LIMIT :limit",
            &[(":name", &name), (":limit", &limit)],
        )
    }

    /// FTS plus substring, because identifiers and hostnames tokenise badly. A query FTS cannot parse is a miss.
    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Value>> {
        let mut found = self
            .rows(
                "SELECT m.*, t.subject, t.channel FROM messages_fts f JOIN messages m ON m.id = f.rowid JOIN threads t ON t.id = m.thread_id \
                 WHERE messages_fts MATCH :q ORDER BY m.id DESC LIMIT :limit",
                &[(":q", &query), (":limit", &limit)],
            )
            .unwrap_or_default();
        let seen: std::collections::HashSet<i64> = found.iter().filter_map(|r| r["id"].as_i64()).collect();
        let like = format!("%{query}%");
        for r in self.rows(
            "SELECT m.*, t.subject, t.channel FROM messages m JOIN threads t ON t.id = m.thread_id WHERE m.body LIKE :like OR t.subject LIKE :like ORDER BY m.id DESC LIMIT :limit",
            &[(":like", &like), (":limit", &limit)],
        )? {
            if !seen.contains(&r["id"].as_i64().unwrap_or(-1)) {
                found.push(r);
            }
        }
        // Python's `out[:limit]`: a negative limit drops that many from the end
        let keep = if limit < 0 { found.len().saturating_sub(limit.unsigned_abs() as usize) } else { limit as usize };
        found.truncate(keep);
        Ok(found)
    }
}

/// `@name` mentions: `@` must start the token (so e-mail addresses are not mentions); fenced code is ignored.
pub fn mentions(body: &str) -> Vec<String> {
    let fence = regex::Regex::new(r"(?s)```.*?```").unwrap();
    let text = fence.replace_all(body, "");
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        if c != '@' {
            continue;
        }
        if i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_' || chars[i - 1] == '.') {
            continue;
        }
        if !chars.get(i + 1).map(|c| c.is_ascii_alphabetic()).unwrap_or(false) {
            continue;
        }
        let name: String = chars[i + 1..].iter().take_while(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '#' | '-')).collect();
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

pub fn anyhow_msg(e: &anyhow::Error) -> String {
    if let Some(b) = e.downcast_ref::<BoardError>() {
        return b.0.clone();
    }
    if let Some(rusqlite::Error::SqliteFailure(_, Some(m))) = e.downcast_ref::<rusqlite::Error>() {
        return format!("database error: {m}");
    }
    match e.downcast_ref::<rusqlite::Error>() {
        Some(x) => format!("database error: {x}"),
        None => e.to_string(),
    }
}
