using System.Globalization;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using Microsoft.Data.Sqlite;

namespace AgentDesk.Core.Board;

/// <summary>A caller error, returned to the agent as {"error": message}. Python's ValueError.</summary>
public sealed class BoardError(string message) : Exception(message);

/// <summary>Where the board lives. Every tool call opens its own connection (as db.connect() does) and closes it before returning.</summary>
public sealed class BoardStore(string path, Func<DateTimeOffset>? clock = null)
{
    public BoardDb Open() => new(path, clock ?? (() => DateTimeOffset.UtcNow));

    public void Init() { using var db = Open(); db.InitDb(); }
}

/// <summary>One connection, and the parts of agentdesk/db.py the MCP tools reach. SQL is kept textually identical to Python's.</summary>
public sealed partial class BoardDb : IDisposable
{
    public const string Agent = "agent", Human = "human", John = "john";
    public static readonly string[] Channels = ["question", "discussion", "wiki", "work"];
    static readonly string[] ReceiptKinds = ["ack", "ack-note", "read-receipt"];

    const string JohnHasLastWord = "COALESCE((SELECT m.author_kind FROM messages m WHERE m.thread_id = t.id"
        + " AND COALESCE(json_extract(m.meta, '$.kind'), '') NOT IN ('ack', 'ack-note', 'read-receipt') ORDER BY m.id DESC LIMIT 1), 'human') = 'human'";
    /// <summary>"Waiting on John": a live question whose newest non-receipt message is not his.</summary>
    public const string WaitingSql = "(t.channel = 'question' AND t.status IN ('open', 'answered') AND NOT (" + JohnHasLastWord + "))";

    const string OpenQuestionsView = "\nCREATE VIEW IF NOT EXISTS open_questions AS\n    SELECT t.id AS thread_id, t.subject, t.opened_by, t.created_ts, t.updated_ts\n    FROM threads t\n    WHERE " + WaitingSql + ";\n";

    const string Schema = """
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
        CREATE TABLE IF NOT EXISTS identities (name TEXT PRIMARY KEY COLLATE NOCASE, folder TEXT NOT NULL, charter TEXT, host TEXT NOT NULL DEFAULT 'windows',
            claude_session_id TEXT, pid INTEGER, state TEXT NOT NULL DEFAULT 'stopped', autostart INTEGER NOT NULL DEFAULT 0,
            created_ts TEXT NOT NULL, updated_ts TEXT NOT NULL, generation INTEGER NOT NULL DEFAULT 1, phoenix_msg INTEGER);
        CREATE TABLE IF NOT EXISTS phoenix_chain (identity TEXT NOT NULL COLLATE NOCASE, generation INTEGER NOT NULL, claude_session_id TEXT,
            handoff_msg INTEGER, ts TEXT NOT NULL, PRIMARY KEY (identity, generation));
        """;

    const string InsertMessage = "INSERT INTO messages (ts, thread_id, author, author_kind, body, reply_to, meta) VALUES ($ts,$tid,$author,$kind,$body,$replyTo,$meta)";
    const string MessageWithThread = "SELECT m.*, t.subject, t.channel FROM messages m JOIN threads t ON t.id = m.thread_id";

    const string ListThreadsSql = "SELECT t.*, COUNT(m.id) AS message_count,"
        + " (SELECT body FROM messages WHERE thread_id=t.id ORDER BY id DESC LIMIT 1) AS last_body,"
        + " (SELECT author FROM messages WHERE thread_id=t.id AND COALESCE(CASE WHEN json_valid(meta)"
        + " THEN json_extract(meta,'$.kind') END,'') NOT IN ('ack','ack-note','read-receipt') ORDER BY id DESC LIMIT 1) AS last_author,"
        + " (SELECT COALESCE(CASE WHEN a.state='posted' THEN 'picked-up' END, d.state,"
        + " CASE WHEN a.message_id IS NOT NULL THEN 'pending' END, '') || '|' || h.ts"
        + " FROM messages h LEFT JOIN acks a ON a.message_id=h.id LEFT JOIN deliveries d ON d.message_id=h.id"
        + " WHERE h.thread_id=t.id AND h.author_kind='human' ORDER BY h.id DESC LIMIT 1) AS delivery,"
        + " (CASE WHEN " + WaitingSql + " THEN 1 ELSE 0 END) AS waiting"
        + " FROM threads t LEFT JOIN messages m ON m.thread_id = t.id";

    readonly SqliteConnection db;
    readonly Func<DateTimeOffset> now;

    internal BoardDb(string path, Func<DateTimeOffset> clock)
    {
        now = clock;
        Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(path))!);
        db = new SqliteConnection(new SqliteConnectionStringBuilder { DataSource = path, Pooling = false, DefaultTimeout = 30 }.ToString());
        db.Open();
        Exec("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=30000; PRAGMA foreign_keys=ON");
    }

    public void Dispose() => db.Dispose();

    public string NowIso() => now().UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss'+00:00'", CultureInfo.InvariantCulture);

    // ---- helpers: $name parameters come from a name/value list

    SqliteCommand Cmd(string sql, (string, object?)[] args)
    {
        var c = db.CreateCommand();
        c.CommandText = sql;
        foreach (var (k, v) in args) c.Parameters.AddWithValue("$" + k, v ?? DBNull.Value);
        return c;
    }

    public int Exec(string sql, params (string, object?)[] args) { using var c = Cmd(sql, args); return c.ExecuteNonQuery(); }

    public object? Scalar(string sql, params (string, object?)[] args) { using var c = Cmd(sql, args); return c.ExecuteScalar() is var v and not DBNull ? v : null; }

    long Insert(string sql, params (string, object?)[] args) { Exec(sql, args); return (long)Scalar("SELECT last_insert_rowid()")!; }

    /// <summary>Rows as ordered objects, the equivalent of [dict(r) for r in rows].</summary>
    public List<JsonObject> Rows(string sql, params (string, object?)[] args)
    {
        using var c = Cmd(sql, args);
        using var r = c.ExecuteReader();
        var rows = new List<JsonObject>();
        while (r.Read())
        {
            var o = new JsonObject();
            for (int i = 0; i < r.FieldCount; i++)
                o[r.GetName(i)] = r.GetValue(i) switch { long l => l, double d => d, string s => s, DBNull => null, var v => v.ToString() };
            rows.Add(o);
        }
        return rows;
    }

    T Tx<T>(Func<T> body)
    {
        Exec("BEGIN IMMEDIATE");
        try { var r = body(); Exec("COMMIT"); return r; }
        catch { Exec("ROLLBACK"); throw; }
    }

    static JsonObject? ParseObject(string? json)
    {
        try { return string.IsNullOrEmpty(json) ? null : JsonNode.Parse(json) as JsonObject; }
        catch (Exception) { return null; }
    }

    static string? Str(JsonNode? n) => n is JsonValue v && v.TryGetValue(out string? s) ? s : null;

    // ---- schema

    public void InitDb()
    {
        Exec(Schema);
        Exec("DROP VIEW IF EXISTS open_questions;");  // so an edited view definition reaches boards that already have one
        Exec(OpenQuestionsView);
        var cols = Rows("PRAGMA table_info(pull_requests)").Select(r => Str(r["name"])).ToHashSet();
        if (!cols.Contains("source")) Exec("ALTER TABLE pull_requests ADD COLUMN source TEXT NOT NULL DEFAULT 'agent-registered'");
        foreach (var col in new[] { "triage", "triage_ts" })
            if (!cols.Contains(col)) Exec($"ALTER TABLE pull_requests ADD COLUMN {col} TEXT");
        var ids = Rows("PRAGMA table_info(identities)").Select(r => Str(r["name"])).ToHashSet();
        if (!ids.Contains("generation")) Exec("ALTER TABLE identities ADD COLUMN generation INTEGER NOT NULL DEFAULT 1");
        if (!ids.Contains("phoenix_msg")) Exec("ALTER TABLE identities ADD COLUMN phoenix_msg INTEGER");
        if (!ids.Contains("model")) Exec("ALTER TABLE identities ADD COLUMN model TEXT NOT NULL DEFAULT 'sonnet'"); // haiku|sonnet|opus
        Exec(Governor.Schema); // the usage governor's samples
    }

    // ---- writes

    void TouchPresence(string author, string kind)
    {
        if (kind == Agent)
            Exec("INSERT INTO presence (author, seen_ts) VALUES ($a,$ts) ON CONFLICT(author) DO UPDATE SET seen_ts=excluded.seen_ts", ("a", author), ("ts", NowIso()));
    }

    static void EnforceQuestionLength(string channel, string kind, string body)
    {
        var words = Py.Words(body);
        if (channel != "question" || kind != Agent || words <= 400) return;
        throw new BoardError($"this question is {words} words; John asked for questions and replies in a question thread to stay under 400. "
            + "State the decision you need in a few sentences and put any supporting detail in a linked discussion thread or document instead of in the question itself.");
    }

    // "@" must start the token, so e-mail addresses are not mentions; fenced code is ignored. [\p{L}\p{N}_] is Python's \w.
    [GeneratedRegex(@"(?<![\p{L}\p{N}_.])@([A-Za-z][\p{L}\p{N}_:#-]*)")] private static partial Regex MentionRe();
    [GeneratedRegex("```.*?```", RegexOptions.Singleline)] private static partial Regex FenceRe();

    /// <summary>The message's meta plus "mentions", only when the body names someone.</summary>
    static string? MessageMeta(JsonObject? meta, string body)
    {
        var names = MentionRe().Matches(FenceRe().Replace(body, "")).Select(m => m.Groups[1].Value).Distinct().ToArray();
        if (names.Length == 0) return Py.Dumps(meta);
        var copy = meta?.DeepClone().AsObject() ?? [];
        copy["mentions"] = new JsonArray([.. names.Select(n => (JsonNode?)n)]);
        return Py.Dumps(copy);
    }

    /// <summary>Post a message, creating a thread unless threadId is given. Returns the THREAD id.</summary>
    public long StartThread(string channel, string subject, string openedBy, string kind, string body, JsonObject? meta = null, long? threadId = null)
    {
        if (!Channels.Contains(channel)) throw new BoardError($"channel must be one of ('question', 'discussion', 'wiki', 'work'), got {Py.Repr(channel)}");
        EnforceQuestionLength(channel, kind, body);
        var ts = NowIso();
        if (threadId is null)
            threadId = Insert("INSERT INTO threads (created_ts, updated_ts, channel, subject, opened_by, status, meta) VALUES ($ts,$ts,$channel,$subject,$by,$status,$meta)",
                ("ts", ts), ("channel", channel), ("subject", subject), ("by", openedBy), ("status", channel is "question" or "work" ? "open" : "fyi"), ("meta", Py.Dumps(meta)));
        else if (Scalar("SELECT 1 FROM threads WHERE id=$id", ("id", threadId)) is null)
            throw new BoardError($"no such thread: {threadId}");
        else
            Exec("UPDATE threads SET updated_ts=$ts WHERE id=$id", ("ts", ts), ("id", threadId));
        Exec(InsertMessage, ("ts", ts), ("tid", threadId), ("author", openedBy), ("kind", kind), ("body", body), ("replyTo", null), ("meta", MessageMeta(meta, body)));
        TouchPresence(openedBy, kind);
        return threadId.Value;
    }

    public long Reply(long threadId, string author, string kind, string body, long? replyTo = null, JsonObject? meta = null)
    {
        if (Scalar("SELECT channel FROM threads WHERE id=$id", ("id", threadId)) is string channel) EnforceQuestionLength(channel, kind, body);
        var ts = NowIso();
        var id = Insert(InsertMessage, ("ts", ts), ("tid", threadId), ("author", author), ("kind", kind), ("body", body), ("replyTo", replyTo), ("meta", MessageMeta(meta, body)));
        Exec("UPDATE threads SET updated_ts=$ts WHERE id=$id", ("ts", ts), ("id", threadId));
        TouchPresence(author, kind);
        return id;
    }

    // ---- the work queue: claim and complete are each one conditional UPDATE, so racing agents resolve in SQLite

    JsonObject Thread(long id) =>
        Rows("SELECT channel, status, meta FROM threads WHERE id=$id", ("id", id)).FirstOrDefault() ?? throw new BoardError($"no such thread: {id}");

    JsonObject ThreadMeta(long id) => ParseObject(Str(Thread(id)["meta"])) ?? [];

    bool MoveWork(long id, string agent, string ts, JsonObject meta, string from, string to)
    {
        var moved = Exec("UPDATE threads SET status=$to, updated_ts=$ts, meta=$meta WHERE id=$id AND channel='work' AND status=$from",
            ("to", to), ("ts", ts), ("meta", Py.Dumps(meta)), ("id", id), ("from", from)) == 1;
        if (moved) TouchPresence(agent, Agent);
        return moved;
    }

    public bool ClaimTask(long id, string agent)
    {
        var ts = NowIso();
        var meta = ThreadMeta(id);
        meta["assignee"] = agent;
        meta["claimed_ts"] = ts;
        return MoveWork(id, agent, ts, meta, "open", "claimed");
    }

    public bool CompleteTask(long id, string agent)
    {
        var ts = NowIso();
        var meta = ThreadMeta(id);
        if (Str(meta["assignee"]) != agent) return false;
        meta["completed_ts"] = ts;
        return MoveWork(id, agent, ts, meta, "claimed", "done");
    }

    // ---- acknowledgements of John's replies: the state flip and the ack message share one transaction

    public List<long> DeliverPendingAcks(string agent, string body)
    {
        var delivered = new List<long>();
        foreach (var row in Rows("SELECT message_id, thread_id FROM acks WHERE agent=$a AND state='pending' ORDER BY message_id", ("a", agent)))
        {
            var mid = (long)row["message_id"]!;
            if (Tx(() =>
                {
                    if (Exec("UPDATE acks SET state='posted', settled_ts=$ts WHERE message_id=$m AND agent=$a AND state='pending'", ("ts", NowIso()), ("m", mid), ("a", agent)) != 1)
                        return false;
                    var tid = Scalar("SELECT thread_id FROM messages WHERE id=$m", ("m", mid)) as long? ?? throw new BoardError($"no such message: {mid}");
                    Reply(tid, agent, Agent, body, mid, new JsonObject { ["kind"] = "ack", ["ack_for"] = mid });
                    return true;
                }))
                delivered.Add(mid);
        }
        return delivered;
    }

    // ---- John, from AgentDesk's window (app.py post_reply and close_question, plus the watcher queueing the ack)

    /// <summary>John's reply: it stops an open question asking, and queues the asking agent's ack. Returns the message id.</summary>
    public long JohnReplies(long threadId, string body)
    {
        var t = Thread(threadId);
        var mid = Reply(threadId, John, Human, body);
        if (Str(t["channel"]) != "question") return mid;
        if (Str(t["status"]) == "open") Exec("UPDATE threads SET status='answered', updated_ts=$ts WHERE id=$tid", ("ts", NowIso()), ("tid", threadId));
        if (Scalar("SELECT author_kind FROM messages WHERE thread_id=$tid ORDER BY id LIMIT 1", ("tid", threadId)) as string == Agent)
            Exec("INSERT OR IGNORE INTO acks (message_id, thread_id, agent, state, created_ts) SELECT $mid, id, opened_by, 'pending', $ts FROM threads WHERE id=$tid",
                ("mid", mid), ("ts", NowIso()), ("tid", threadId));
        return mid;
    }

    /// <summary>John closes a question without answering; clearing archive_hold lets the sweep file it. False if not a live question.</summary>
    public bool CloseQuestion(long threadId)
    {
        var t = Thread(threadId);
        if (Str(t["channel"]) != "question" || Str(t["status"]) == "archived") return false;
        var meta = ParseObject(Str(t["meta"])) ?? [];
        meta.Remove("archive_hold");
        return Exec("UPDATE threads SET status='closed', updated_ts=$ts, meta=$meta WHERE id=$tid", ("ts", NowIso()), ("meta", Py.Dumps(meta)), ("tid", threadId)) == 1;
    }

    /// <summary>John brings an archived question back (db.py unarchive_thread): the status it was settled with, held off the sweep. False if not archived.</summary>
    public bool Unarchive(long threadId)
    {
        var t = Thread(threadId);
        if (Str(t["status"]) != "archived") return false;
        var meta = ParseObject(Str(t["meta"])) ?? [];
        var settled = Str(meta["archived_from"]) is { Length: > 0 } s ? s : "answered";
        meta.Remove("archived_from");
        meta["archive_hold"] = true;
        return Exec("UPDATE threads SET status=$s, updated_ts=$ts, meta=$meta WHERE id=$tid", ("s", settled), ("ts", NowIso()), ("meta", Py.Dumps(meta)), ("tid", threadId)) == 1;
    }

    // ---- read receipts: once per (agent, thread) by PRIMARY KEY; no presence, no updated_ts bump

    public bool PostReadReceipt(long threadId, string agent, string body)
    {
        var warranted = Rows("SELECT author, meta FROM messages WHERE thread_id = $t", ("t", threadId))
            .Any(r => Str(r["author"]) != agent && !ReceiptKinds.Contains(Str(ParseObject(Str(r["meta"]))?["kind"])));
        if (!warranted) return false;
        var ts = NowIso();
        return Tx(() =>
        {
            if (Exec("INSERT OR IGNORE INTO receipts (agent, thread_id, created_ts) VALUES ($a,$t,$ts)", ("a", agent), ("t", threadId), ("ts", ts)) != 1) return false;
            var mid = Insert(InsertMessage, ("ts", ts), ("tid", threadId), ("author", agent), ("kind", Agent), ("body", body), ("replyTo", null),
                ("meta", Py.Dumps(new JsonObject { ["kind"] = "read-receipt" })));
            Exec("UPDATE receipts SET message_id=$m WHERE agent=$a AND thread_id=$t", ("m", mid), ("a", agent), ("t", threadId));
            return true;
        });
    }

    /// <summary>Put a PR on the merge list. The UNIQUE url decides created.</summary>
    public (long Id, bool Created) RegisterPr(string url, string repo, long number, string title, string by, long? threadId)
    {
        if (Exec("INSERT OR IGNORE INTO pull_requests (url, repo, number, title, requested_by, requested_ts, thread_id, state, source)"
                + " VALUES ($url,$repo,$number,$title,$by,$ts,$tid,'open','agent-registered')",
                ("url", url), ("repo", repo), ("number", number), ("title", title), ("by", by), ("ts", NowIso()), ("tid", threadId)) > 0)
            return ((long)Scalar("SELECT last_insert_rowid()")!, true);
        return (Scalar("SELECT id FROM pull_requests WHERE url=$url", ("url", url)) as long? ?? 0, false);
    }

    public string PassTheTorch(string name, string body)
    {
        var ts = NowIso();
        Exec("INSERT INTO handoffs (name, body, path, updated_ts, updated_by, torch_due) VALUES ($n, $b, NULL, $ts, $n, 0)"
            + " ON CONFLICT(name) DO UPDATE SET body=excluded.body, path=excluded.path, updated_ts=excluded.updated_ts, updated_by=excluded.updated_by, torch_due=0",
            ("n", name), ("b", body), ("ts", ts));
        return ts;
    }

    public void RecordSession(string author, string? sessionId, string? cwd, int pid)
    {
        if (string.IsNullOrEmpty(author) || string.IsNullOrEmpty(sessionId)) return;
        Exec("INSERT INTO sessions(author, session_id, cwd, pid, channels, seen_ts) VALUES($a,$s,$cwd,$pid,0,$ts)"
            + " ON CONFLICT(author) DO UPDATE SET session_id=excluded.session_id, cwd=excluded.cwd, pid=excluded.pid, channels=excluded.channels, seen_ts=excluded.seen_ts",
            ("a", author), ("s", sessionId), ("cwd", cwd), ("pid", pid), ("ts", NowIso()));
    }

    // ---- reads

    public JsonObject GetThread(long id)
    {
        var t = Rows("SELECT t.*, (CASE WHEN " + WaitingSql + " THEN 1 ELSE 0 END) AS waiting FROM threads t WHERE id=$id", ("id", id)).FirstOrDefault()
            ?? throw new BoardError($"no such thread: {id}");
        return new() { ["thread"] = t, ["messages"] = Arr(Rows("SELECT * FROM messages WHERE thread_id=$id ORDER BY id", ("id", id))) };
    }

    public List<JsonObject> ListThreads(string? channel, string? status, int limit, bool includeArchived = true)
    {
        var where = new List<string>();
        if (!string.IsNullOrEmpty(channel)) where.Add("t.channel = $channel");
        if (!string.IsNullOrEmpty(status)) where.Add("t.status = $status");
        if (!includeArchived && status != "archived") where.Add("t.status <> 'archived'");  // an explicit status='archived' wins
        return Rows(ListThreadsSql + (where.Count > 0 ? " WHERE " + string.Join(" AND ", where) : "") + " GROUP BY t.id ORDER BY t.updated_ts DESC LIMIT $limit",
            ("channel", channel), ("status", status), ("limit", limit));
    }

    public List<JsonObject> OpenQuestions(bool includeArchived) => includeArchived
        ? Rows("SELECT id AS thread_id, subject, opened_by, created_ts, updated_ts FROM threads WHERE channel='question' AND status IN ('open', 'archived') ORDER BY updated_ts DESC")
        : Rows("SELECT * FROM open_questions ORDER BY updated_ts DESC");

    public bool TorchDue(string name) => Scalar("SELECT torch_due FROM handoffs WHERE name=$n", ("n", name)) is long due && due != 0;

    public long? BioThread(string subject) => Scalar("SELECT id FROM threads WHERE channel='discussion' AND subject=$s ORDER BY id LIMIT 1", ("s", subject)) as long?;

    public List<JsonObject> Recent(int limit) => Rows(MessageWithThread + " ORDER BY m.id DESC LIMIT $limit", ("limit", limit));

    public List<JsonObject> ListMentions(string name, int limit) => Rows(
        "SELECT m.*, t.subject, t.channel FROM messages m JOIN threads t ON t.id = m.thread_id, json_each(m.meta, '$.mentions') je"
        + " WHERE m.meta IS NOT NULL AND json_valid(m.meta) AND json_extract(m.meta, '$.mentions') IS NOT NULL"
        + " AND lower(je.value) = lower($name) ORDER BY m.id DESC LIMIT $limit", ("name", name), ("limit", limit));

    /// <summary>FTS plus substring, because identifiers and hostnames tokenise badly. A query FTS cannot parse is a miss, not an error.</summary>
    public List<JsonObject> Search(string query, int limit)
    {
        var found = new List<JsonObject>();
        try
        {
            found.AddRange(Rows("SELECT m.*, t.subject, t.channel FROM messages_fts f JOIN messages m ON m.id = f.rowid JOIN threads t ON t.id = m.thread_id"
                + " WHERE messages_fts MATCH $q ORDER BY m.id DESC LIMIT $limit", ("q", query), ("limit", limit)));
        }
        catch (SqliteException) { }
        var seen = found.Select(r => (long)r["id"]!).ToHashSet();
        found.AddRange(Rows(MessageWithThread + " WHERE m.body LIKE $like OR t.subject LIKE $like ORDER BY m.id DESC LIMIT $limit", ("like", $"%{query}%"), ("limit", limit))
            .Where(r => seen.Add((long)r["id"]!)));
        return Py.Head(found, limit);
    }

    public static JsonArray Arr(IEnumerable<JsonNode?> rows) => [.. rows];
}
