global using Line = System.Collections.Generic.List<AgentDesk.App.Seg>;
using System.Diagnostics;
using System.IO;
using System.Windows.Input;

namespace AgentDesk.App;

/// <summary>A run of text and its space-separated tags: a colour (fg mu fa rule pk or ye gr cy pu), b, bar, barcy, inv, cur, rcpt.</summary>
public readonly record struct Seg(string Text, string Tags = "");

/// <summary>The BBS screens and keys of agentdesk/terminal.py: every screen is a list of lines built from the in-memory board.</summary>
public partial class MainWindow
{
    const string Human = "john";
    const int GraceHours = 24;
    static readonly string[] Channels = ["question", "discussion", "wiki", "work"];
    static readonly Dictionary<char, string> ChannelKeys = new() { ['q'] = "question", ['d'] = "discussion", ['w'] = "wiki", ['j'] = "work" };
    static readonly Dictionary<string, string> Titles = new()
    {
        ["question"] = "Questions", ["discussion"] = "Discussion", ["wiki"] = "Wiki", ["work"] = "Work to Hire",
    };
    static readonly Dictionary<string, string> Banner = new()
    {
        ["question"] = "QUESTIONS  ·  the SysOp's desk  ·  agents can chime in, only john can close",
        ["discussion"] = "DISCUSSION  ·  the agents' break room  ·  you're on the party line",
        ["wiki"] = "WIKI  ·  the file library  ·  browse all you like, no leech ratio",
        ["work"] = "WORK TO HIRE  ·  the job board  ·  open is up for grabs, held means someone's on it",
    };
    static readonly (string Top, string Bot, string Hue)[] Logo =
    [
        ("█▀█", "█▀█", "pk"), ("█▀▀", "█▄█", "pk"), ("█▀▀", "██▄", "or"), ("█▄ █", "█ ▀█", "or"), ("▀█▀", " █ ", "ye"),
        ("█▀▄", "█▄▀", "gr"), ("█▀▀", "██▄", "gr"), ("█▀", "▄█", "cy"), ("█▄▀", "█ █", "pu"),
    ];
    static readonly string[] AuthorHues = ["cy", "pu", "gr", "or"];

    string screen = "main", channel = "question", readBack = "list";
    int? readTid;
    (int Tid, int Count)? readerKey;
    bool showArchived, showSettled, scrollToEnd;
    (string Text, Action Yes)? confirm;
    (string Text, string Tags)? flash;
    (int Top, int End, int Count, int Visible) window;
    readonly Dictionary<string, int> sel = [], topRow = [];
    readonly Dictionary<int, int> clickMap = [];
    readonly Dictionary<string, IReadOnlyList<ThreadRow>> rows = Channels.ToDictionary(c => c, _ => (IReadOnlyList<ThreadRow>)[]);
    readonly Dictionary<int, ThreadDetail?> threads = [];
    IReadOnlyList<ThreadRow> openQs = [];
    BoardStatus? st;
    int cols = 96, lines = 30;

    // --- text helpers ------------------------------------------------------------

    static Seg S(string text, string tags = "") => new(text, tags);
    static Seg[] If(bool on, params Seg[] segs) => on ? segs : [];
    static string Rep(char c, int n) => new(c, Math.Max(0, n));
    static int Len(IEnumerable<Seg> segs) => segs.Sum(s => s.Text.Length);
    static string N(int n, string word) => $"{n} {word}{(n != 1 ? "s" : "")}";
    static string Label(string? author) => author ?? "";
    static string Hue(string? author) => author == Human ? "ye" : AuthorHues[(author ?? "").Sum(c => c) % AuthorHues.Length];

    static string Fit(string? s, int n)
    {
        s = string.Join(' ', (s ?? "").Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries));
        return n <= 0 ? "" : s.Length > n ? s[..(n - 1)] + "…" : s.PadRight(n);
    }

    static Line Pad(Line segs, int width, string tags = "") => width > Len(segs) ? [.. segs, S(Rep(' ', width - Len(segs)), tags)] : segs;

    static Line ClipTo(Line segs, int width)
    {
        Line o = [];
        foreach (var s in segs)
        {
            if (Len(o) + s.Text.Length > width - 1)
                return [.. o, s with { Text = s.Text[..Math.Max(0, width - 1 - Len(o))] + "…" }];
            o.Add(s);
        }
        return o;
    }

    static string When(DateTimeOffset? ts)
    {
        if (ts is not { } t)
            return "";
        var local = t.ToLocalTime();
        return local.Date == DateTime.Today ? local.ToString("HH:mm")
            : DateTimeOffset.Now - local < TimeSpan.FromDays(6) ? local.ToString("ddd HH:mm") : local.ToString("dd-MMM HH:mm");
    }

    static string Span(double seconds)
    {
        var t = TimeSpan.FromSeconds(Math.Max(0, (int)seconds));
        return t.Days > 0 ? $"{t.Days}d {t.Hours}h" : t.Hours > 0 ? $"{t.Hours}h {t.Minutes:00}m" : $"{t.Minutes}m";
    }

    static string Ago(DateTimeOffset? ts) => ts is { } t ? Span((DateTimeOffset.Now - t).TotalSeconds) : "";

    static (string Code, string Tags) StateCode(string ch, ThreadRow r) => ch == "question" && r.Waiting ? ("WAIT", "pk b") : r.Status switch
    {
        "open" => ch == "work" ? ("OPEN", "ye") : ("live", "mu"),
        "claimed" => ("HELD", "cy"), "done" => ("DONE", "gr"), "answered" => ("ansd", "mu"),
        "closed" => ("clsd", "fa"), "fyi" => ("fyi ", "mu"), "archived" => ("arch", "fa"),
        var w => (Fit(w, 4), "mu"),
    };

    /// <summary>'you' plus how John's latest reply reached the agent: √ it acted, ↑ woke it, … on its way, ! not picked up.</summary>
    static string DeliveryMark(ThreadRow r)
    {
        var parts = (r.Delivery ?? "").Split('|', 2);
        DateTimeOffset? ts = parts.Length > 1 && DateTimeOffset.TryParse(parts[1], out var t) ? t : null;
        return parts[0] switch
        {
            "picked-up" => "you √",
            "woke" or "resumed" => "you ↑ woke",
            "pending" or "stuck" or "failed" when parts[0] != "pending" || DateTimeOffset.Now - ts > TimeSpan.FromMinutes(15) => $"you ! {Ago(ts)}",
            "pending" => "you …",
            _ => "you",
        };
    }

    // --- board state -------------------------------------------------------------

    IReadOnlyList<PrRow> Prs => [.. (st?.Prs ?? []).Where(p => showSettled || p.State == "open")];
    IReadOnlyList<Post> Callers => [.. (st?.Callers ?? []).Where(c => c.Author != Human)];
    ThreadRow? HeldRow => rows["work"].FirstOrDefault(r => r.Id == st?.HeldId) ?? rows["work"].FirstOrDefault(r => r.Status == "claimed");
    bool SlackUp => st?.SlackTs is { } t && DateTimeOffset.Now - t < TimeSpan.FromSeconds(90);
    string SelKey => screen == "list" ? channel : screen;
    int Sel { get => sel.GetValueOrDefault(SelKey); set => sel[SelKey] = value; }

    async Task RefreshAsync()
    {
        foreach (var ch in Channels)
        {
            var all = await board.ListThreadsAsync(ch);
            rows[ch] = ch != "question" ? all
                : [.. all.Where(r => (r.Status == "archived") == showArchived).OrderBy(r => showArchived || r.Waiting ? 0 : 1)];
        }
        openQs = await board.OpenQuestionsAsync();
        st = await board.StatusAsync();
        threads.Clear();
        Title = "AgentDesk" + (openQs.Count > 0 ? $" - {openQs.Count} open question{(openQs.Count == 1 ? "" : "s")}" : "");
        Render();
    }

    ThreadDetail? Thread(int id)
    {
        if (threads.TryGetValue(id, out var cached))
            return cached;
        var task = board.ReadThreadAsync(id);
        if (task.IsCompleted)
            return threads[id] = task.Result;
        task.ContinueWith(t => { threads[id] = t.Result; Render(); }, TaskScheduler.FromCurrentSynchronizationContext());
        return null;
    }

    // --- top and bottom lines ----------------------------------------------------

    Line TopLine(int W)
    {
        var title = screen switch
        {
            "main" => "Main menu", "list" => Titles[channel], "prs" => "Pull Requests", "sysop" => "SysOp console", "who" => "Who's on",
            "options" => "Options", "compose" => $"New post in {Titles[channel]}", _ => $"Reading #{readTid}",
        };
        Line left = [S("AgentDesk", "ye b"), S($" · {title}", "mu")];
        var held = HeldRow;
        Line mid = st?.WorkerRunning == true
            ? [S("● worker online", "gr"), held is null ? S(" · idle", "mu") : S($" · #{held.Id}", "ye"), .. If(held?.Holder != null, S($" · {Label(held?.Holder)}", "mu"))]
            : [S("○ worker offline", "or"), S(" · Ctrl+W starts it", "fa")];
        Line right = [openQs.Count > 0 ? S($"{openQs.Count} ringing for you", "pk b") : S("nobody's calling", "mu"), S(" · ", "fa"),
            SlackUp ? S("SlackNet ● up", "cy") : S("SlackNet ○ down", "fa")];
        var gap = W - Len(left) - Len(mid) - Len(right);
        return gap < 4 ? Pad([.. left, S("   "), .. right], W) : [.. left, S(Rep(' ', gap / 2)), .. mid, S(Rep(' ', gap - gap / 2)), .. right];
    }

    Line Hints()
    {
        static Seg[] K(string key, string label) => [S($" {key}", "ye inv"), S($" {label} ", "mu inv")];
        if (confirm is { } c)
            return [S($" {c.Text} ", "pk b inv"), .. K("Y", "yes"), .. K("N", "no")];
        var q = channel == "question";
        return screen switch
        {
            "main" => [.. K("Q D W J", "message bases"), .. K("P", "PRs"), .. K("S", "SysOp"), .. K("B", "who's on"), .. K("O", "options"), .. K("G", "hang up")],
            "list" => [.. K("↑↓", "move"), .. K("↵", "read"), .. K("N", "new post"),
                .. If(q, [.. K("H", showArchived ? "active" : "archived"), .. K("Ctrl+R", "wake agent")]), .. K("Esc", "main menu")],
            "read" => [.. K("type", "to reply"), .. K("Ctrl+↵", "send"), .. K("Ctrl+D", "dictate"), .. K("PgUp/PgDn", "scroll"),
                .. K("Alt+N/P", "next/prev"), .. If(q, [.. K("Alt+C", "close"), .. K("Alt+U", "bring back")]), .. K("Esc", "back")],
            "prs" => [.. K("↑↓", "move"), .. K("↵", "open on GitHub"), .. K("C", "check now"), .. K("H", showSettled ? "open only" : "settled"), .. K("Esc", "menu")],
            "sysop" => [.. K("Ctrl+W", st?.WorkerRunning == true ? "stop worker (after this item)" : "start worker"), .. K("R", "reload code"),
                .. K("J", "job board"), .. K("L", "read held item"), .. K("Esc", "menu")],
            "who" => [.. K("↑↓", "pick a caller"), .. K("↵", "read bio"), .. K("P", "page them"), .. K("Esc", "menu")],
            "options" => [.. K("↑↓", "move"), .. K("↵", "change"), .. K("←→", "adjust"), .. K("Esc", "menu")],
            "compose" => [.. K("↵", "subject → body"), .. K("Ctrl+↵", "post"), .. K("Ctrl+D", "dictate"), .. K("Esc", "cancel")],
            _ => [],
        };
    }

    Line BarLine(int W) => flash is { } f ? Pad([S(" " + f.Text, f.Tags + " inv")], W, "inv") : Pad(Hints(), W, "inv");

    // --- shared pieces -----------------------------------------------------------

    List<Line> Box(string title, IEnumerable<Line> body, int W)
    {
        List<Line> o = [[S("┌─ ", "rule"), S(title, "mu"), S(" " + Rep('─', W - 5 - title.Length) + "┐", "rule")]];
        o.AddRange(body.Select(Line (segs) => [S("│ ", "rule"), .. Pad(Len(segs) > W - 4 ? ClipTo(segs, W - 4) : segs, W - 4), S(" │", "rule")]));
        o.Add([S("└" + Rep('─', W - 2) + "┘", "rule")]);
        return o;
    }

    Line Bar(string text, string tag = "barcy") => Pad([S(" " + text, tag + " b")], cols, tag);

    /// <summary>The one list renderer behind every selectable screen: header, a scrolled window of rows, the selected one in reverse.</summary>
    List<Line> Rows(Line banner, string? header, int count, Func<int, Line> row, Line empty, int fixedLines, Func<int, Line?>? before = null)
    {
        List<Line> L = [banner, []];
        if (header != null)
            L.AddRange([[S(header, "mu")], [S(Rep('─', cols), "rule")]]);
        if (count == 0)
            L.Add(empty);
        var s = Sel = Math.Clamp(Sel, 0, Math.Max(0, count - 1));
        var visible = Math.Max(5, lines - fixedLines - 1);
        var top = topRow.GetValueOrDefault(SelKey);
        top = s < top ? s : s >= top + visible ? s - visible + 1 : top;
        top = topRow[SelKey] = Math.Max(0, Math.Min(top, count - visible));
        window = (top, Math.Min(count, top + visible), count, visible);
        for (var i = top; i < window.End; i++)
        {
            if (before?.Invoke(i) is { } extra)
                L.Add(extra);
            clickMap[L.Count] = i;
            var segs = row(i);
            L.Add(i == s ? Pad([S(" ▶" + string.Concat(segs.Select(x => x.Text))[2..], "cur")], cols, "cur") : segs);
        }
        if (header != null)
            L.Add([S(Rep('─', cols), "rule")]);
        return L;
    }

    // --- screens -----------------------------------------------------------------

    List<Line> MainScreen(int W)
    {
        var screech = Pref("screech", false);
        List<Line> L =
        [
            [S("   "), .. Logo.SelectMany(l => new[] { S(l.Top, l.Hue + " b"), S(" ") }), S("  6 lines · no long-distance fees", "mu")],
            [S("   "), .. Logo.SelectMany(l => new[] { S(l.Bot, l.Hue + " b"), S(" ") }), S("  please do not tie up the line", "fa")],
            [],
            [S("ATDT AGENTDESK ... ", "fa"), S("CONNECT", "gr b"),
                S(screech ? "  (you heard that. we all heard that.)" : "  (handshake screech omitted for your comfort)", "fa")],
            [],
            st?.JohnLast is { } me
                ? [S(" Welcome back, "), S(Human.ToUpperInvariant(), "ye b"), S($". Last call {When(me.Ts)}, "),
                    S(me.Via == "slack" ? "from your phone via SlackNet" : "from this terminal", "cy"), S(".")]
                : [S(" First call? Pull up a chair, "), S(Human.ToUpperInvariant(), "ye b"), S(".")],
        ];
        var ringing = openQs.Count;
        var prsOpen = (st?.Prs ?? []).Count(p => p.State == "open");
        var since = st?.SincePosts ?? 0;
        L.Add([S(" Since then: "), ringing > 0 ? S(N(ringing, "question"), "pk b") : S("no questions", "mu"),
            S(ringing > 0 ? " rang for you, " : " rang, "), prsOpen > 0 ? S(N(prsOpen, "PR"), "gr") : S("no PRs", "mu"),
            S(prsOpen != 1 ? " want a merge, " : " wants a merge, "), S($"{N(since, "post")} landed", "mu"), S(".")]);
        L.Add([]);
        var work = rows["work"];
        var held = HeldRow;
        var sysop = st?.WorkerRunning != true ? S("offline · Ctrl+W starts it", "or") : S(held is null ? "worker idle" : $"worker on #{held.Id}", "gr");
        var agents = Callers.Select(c => c.Author).Distinct().Count();
        (string Key, string Label, Seg Val)[] items =
        [
            ("Q", "Questions", ringing > 0 ? S($"{ringing} ringing", "pk b") : S("all quiet", "mu")),
            ("J", "Work to Hire", S($"{work.Count(r => r.Status == "open")} open · {work.Count(r => r.Status == "claimed")} held", "cy")),
            ("D", "Discussion", S($"{rows["discussion"].Count} threads", "fg")),
            ("P", "Pull Requests", prsOpen > 0 ? S($"{prsOpen} to merge", "gr") : S("nothing to merge", "mu")),
            ("W", "Wiki", S($"{rows["wiki"].Count} articles", "mu")),
            ("S", "SysOp console", sysop),
            ("B", "Who's on", S($"{N(agents, "agent")} today", "pu")),
            ("O", "Options", S(Palettes[Theme].Label + (screech ? " · screech on" : ""), "ye")),
            ("G", "Hang up", S("to the tray", "mu")),
        ];
        static Line Item((string Key, string Label, Seg Val) it) =>
            [S("[", "mu"), S(it.Key, "ye b"), S("] ", "mu"), S(it.Label + " "), S(Rep('.', Math.Max(2, 18 - $"[{it.Key}] {it.Label} ".Length)), "rule"), S(" "), it.Val];
        var colw = Math.Max(38, (W - 2) / 2);
        for (var i = 0; i < items.Length; i += 2)
            L.Add([.. Pad([S(" "), .. Item(items[i])], colw), .. i + 1 < items.Length ? Item(items[i + 1]) : []]);
        L.Add([]);
        var recent = (st?.Recent ?? []).Select(Line (r) => [S(Fit(When(r.Ts), 9), "fa"), S(Fit(Label(r.Author), 18), Hue(r.Author)),
            S((r.Kind == "read-receipt" ? "picked up" : r.First ? "opened" : r.Author == Human && r.Channel == "question" ? "answered" : "replied on") + " "),
            S($"#{r.ThreadId}", "ye"), S(" " + r.Subject, "mu"), S(r.Via == "slack" ? " via SlackNet" : "", "pu")]).ToList();
        L.AddRange(Box("Recent callers", recent.Count > 0 ? recent : [[S("Nobody has called yet. The line is open.", "mu")]], W));
        L.Add([]);
        var phrases = st?.UsageLines ?? [];
        var phrase = phrases.Count > 0 ? phrases[(int)(DateTimeOffset.Now.ToUnixTimeSeconds() / 6 % phrases.Count)] : null;
        L.Add(phrase is null ? [S(" (time left: unlimited, you're the SysOp)", "fa")] : [S(" (" + phrase + ")", phrase.Contains("week") ? "or" : "fa")]);
        L.Add([S(" Main menu ", "fg"), S("[", "mu"), S("Q,D,W,J,P,S,B,O,G", "ye"), S("]", "mu"), S(": "), S(" ", "cur")]);
        return L;
    }

    List<Line> ChannelList(int W)
    {
        var (ch, rs, q) = (channel, rows[channel], channel == "question");
        var subjW = Math.Max(20, W - (q ? 56 : 44));
        var L = Rows(Bar(q && showArchived ? "QUESTIONS  ·  the archive  ·  settled and filed to the vault" : Banner[ch]),
            "    #  ST    " + Fit("SUBJECT", subjW) + " " + Fit(ch == "work" ? "HELD BY" : "FROM", 16) + (q ? Fit("LAST WORD", 12) : "") + Fit("WHEN", 11) + " MSG",
            rs.Count, i =>
            {
                var r = rs[i];
                var (code, ctags) = StateCode(ch, r);
                var last = q ? Fit(r.LastAuthor == Human ? DeliveryMark(r) : r.LastAuthor is { } la ? "↩ " + Label(la) : "—", 12) : "";
                var by = ch == "work" ? r.Holder is { } h ? Label(h) : "—" : Label(r.OpenedBy);
                return [S($"  {r.Id,3}  ", "ye"), S(code, ctags), S("  "), S(Fit(r.Subject, subjW), code == "WAIT" ? "fg b" : code is "OPEN" or "HELD" or "live" ? "fg" : "mu"),
                    S(" "), S(Fit(by, 16), Hue(r.OpenedBy)), S(last, last.Contains(" ! ") ? "pk" : last.StartsWith("you") ? "gr" : "cy"),
                    S(Fit(When(r.UpdatedTs), 11), "fa"), S($" {r.MessageCount,3}", "mu")];
            },
            [S("   Nothing here yet. ", "mu"), S("N", "ye"), S(" starts the first thread.", "mu")], q ? 7 : 6,
            i => q && !showArchived && i > 0 && rs[i - 1].Waiting && !rs[i].Waiting
                ? [S($" ── answered · stays here {GraceHours}h after the last reply, then files to the vault ", "fa")] : null);
        Line footer;
        if (q && showArchived)
            footer = [S($" {rs.Count} archived. ", "mu"), S("U", "ye"), S(" on one brings it back.", "mu")];
        else if (q)
        {
            var ringing = rs.Where(r => r.Waiting).ToList();
            footer = [ringing.Count > 0 ? S($" {ringing.Count} still ringing", "pk b") : S(" nothing ringing", "gr"),
                .. If(ringing.Count > 0, S($" · oldest waiting {Ago(ringing.Select(r => (DateTimeOffset?)r.UpdatedTs).Min())}", "mu")),
                S($" · {rs.Count - ringing.Count} answered, kept {GraceHours}h after the last reply", "mu")];
        }
        else if (ch == "work")
            footer = [S($" {rs.Count(r => r.Status == "open")} open", "ye"), S(" · "), S($"{rs.Count(r => r.Status == "claimed")} held", "cy"), S(" · "),
                S($"{rs.Count(r => r.Status == "done")} done", "gr")];
        else
            footer = [S(" " + N(rs.Count, "thread"), "mu"), .. If(rs.Count > 0, S($" · newest post {Ago(rs.Select(r => (DateTimeOffset?)r.UpdatedTs).Max())} ago", "mu"))];
        if (window.Count > window.Visible)
            footer.Add(S($" · rows {window.Top + 1}–{window.End} of {window.Count}", "fa"));
        L.Add(footer);
        return L;
    }

    List<Line> Reader(int W)
    {
        var data = readTid is { } tid ? Thread(tid) : null;
        if (data is null)
            return [[S(" That thread is no longer on the board.", "mu")]];
        var (t, msgs) = (data.Thread, data.Messages);
        var key = (t.Id, msgs.Count);
        scrollToEnd = readerKey is null || readerKey.Value.Tid != t.Id || readerKey.Value.Count < msgs.Count;
        readerKey = key;
        var ch = t.Channel;
        var (code, ctags) = StateCode(ch, t);
        var word = code switch
        {
            "WAIT" => "WAITING ON YOU", "OPEN" => "up for grabs", "HELD" => "held", "DONE" => "done", "ansd" => "answered",
            "clsd" => "closed", "arch" => "archived", _ => code.Trim(),
        };
        var idx = rows.TryGetValue(ch, out var list) ? list.ToList().FindIndex(r => r.Id == t.Id) : -1;
        var pos = idx >= 0 ? $"{idx + 1} of {list!.Count} in {Titles[ch]}" : Titles.GetValueOrDefault(ch, ch);
        var head = $"═ Msg #{t.Id} ═ {pos} ";
        List<Line> L =
        [
            [S("╔", "rule"), S(head, "ye b"), S(Rep('═', W - 2 - head.Length) + "╗", "rule")],
            [S("  From: ", "mu"), S(Fit(Label(t.OpenedBy), 30), Hue(t.OpenedBy)), S("To: ", "mu"), S(Fit(ch == "question" ? Human : "everyone", 12), "ye"),
                S("Status: ", "mu"), S(word, ctags)],
        ];
        var width = W - 10;
        for (var n = 0; n == 0 || n * width < t.Subject.Length; n++)
            L.Add([S(n == 0 ? "  Subj: " : "        ", "mu"), S(t.Subject.Substring(n * width, Math.Min(width, t.Subject.Length - n * width)), "fg b")]);
        L.Add([S("  Date: ", "mu"), S(Fit(When(t.CreatedTs), 16)), S("Replies: ", "mu"), S($"{Math.Max(0, msgs.Count - 1)}"),
            .. If(ch == "work" && t.Holder != null, S("   Held by: ", "mu"), S(Label(t.Holder), "cy")),
            .. If(msgs.Any(m => m.Via == "slack"), S("   Echo: ", "mu"), S("SlackNet", "pu"))]);
        L.Add([S("╚" + Rep('═', W - 2) + "╝", "rule")]);
        for (var i = 0; i < msgs.Count; i++)
        {
            var m = msgs[i];
            var receipt = m.Kind == "read-receipt";
            var verb = receipt ? "picked it up" : i == 0 ? "wrote" : m.Author == Human && ch == "question" ? "answered" : "replied";
            var via = m.Via == "slack" ? " from the phone, via SlackNet" : "";
            var text = $" ─── {Label(m.Author)} {verb} {When(m.Ts)}{via} ";
            L.Add([S(" ───", receipt ? "rule rcpt" : "rule"), S(" " + Label(m.Author), Hue(m.Author) + " b" + (receipt ? " rcpt" : "")),
                S($" {verb} {When(m.Ts)}", receipt ? "rcpt" : "mu"), S(via, "pu"), S(" " + Rep('─', W - text.Length - 1), "rule")]);
            L.AddRange(m.Body.Split('\n').Select(Line (l) => [S(l, receipt ? "rcpt" : "")]));
            L.Add([]);
        }
        return L;
    }

    List<Line> PrsScreen(int W)
    {
        var rs = Prs;
        var titleW = Math.Max(20, W - 58);
        var L = Rows(Bar("PULL REQUESTS  ·  the merge desk  ·  clears itself once GitHub says merged"),
            "  ST     " + Fit("REPO#", 22) + Fit("TITLE", titleW) + " " + Fit("ASKED BY", 14) + Fit("CHECKED", 11), rs.Count, i =>
            {
                var r = rs[i];
                var (code, tag) = r.State switch { "open" => ("OPEN", "gr"), "merged" => ("mrgd", "pu"), "closed" => ("clsd", "fa"), var x => (Fit(x, 4), "mu") };
                return [S($"  {code}   ", tag), S(Fit($"{r.Repo}#{r.Number}", 22), "cy"), S(Fit(r.Title, titleW)), S(" "),
                    S(Fit(Label(r.RequestedBy) + (r.Scan ? " (scan)" : ""), 14), "mu"),
                    S(Fit(r.LastError != null ? "failed" : r.CheckedTs is { } c ? When(c) : "never", 11), r.LastError != null ? "pk" : "fa")];
            },
            [S("   Nothing waiting on you. Agents add PRs here with request_merge.", "mu")], 12);
        if (rs.Count > 0 && rs[Sel] is var p)
        {
            L.AddRange([[], [S(" " + p.Title, "fg b")], [S(" " + p.Url, "cy")]]);
            if (p.Triage != null)
                L.Add([S(" triage: ", "mu"), S(p.Triage)]);
            if (p.LastError != null)
                L.AddRange([[S(" last check failed: ", "pk"), S(p.LastError, "mu")], [S(" it stays on the list; only GitHub saying merged removes it.", "fa")]]);
            if (p.ThreadId != null)
                L.Add([S($" a notice goes to thread #{p.ThreadId} when it merges.", "fa")]);
        }
        return L;
    }

    List<Line> SysopScreen(int W)
    {
        List<Line> L = [Bar("SYSOP CONSOLE  ·  " + (openQs.Count > 0 ? "phone's ringing off the hook" : "waiting for callers"), "bar"), []];
        void Stat(string k, params Seg[] segs) => L.Add([S(" " + Fit(k, 20), "mu"), .. segs]);
        var held = HeldRow;
        var events = st?.HeldEvents ?? [];
        if (st?.WorkerRunning == true && held != null)
        {
            var started = events.FirstOrDefault(e => e.Kind == "start");
            Stat("Worker", [S($"● online · on #{held.Id}{(started != null ? $" for {Ago(started.Ts)}" : "")}", "gr"), .. If(held.Holder != null, S($" · {Label(held.Holder)}", "mu"))]);
        }
        else if (st?.WorkerRunning == true)
            Stat("Worker", S("● online · idle, the queue is empty", "gr"));
        else
            Stat("Worker", S("○ offline", "or"), S("  Ctrl+W starts it", "fa"));
        if (SlackUp)
            Stat("SlackNet echo", [S("● up", "gr"), S($" · polling every {st!.SlackPollS}s", "mu"),
                .. If(st.SlackRelay != null, S($" · last relay {When(st.SlackRelay?.Ts)} (#{st.SlackRelay?.ThreadId} to your phone)", "mu"))]);
        else
            Stat("SlackNet echo", S($"○ down{(st?.SlackTs != null ? $", last heard {Ago(st.SlackTs)} ago" : "")}", "or"), S("  replies from your phone won't arrive", "fa"));
        var sinks = st?.DisabledSinks ?? [];
        Stat("Notifier sinks", sinks.Count > 0 ? S($"⚠ {sinks.Count} disabled: " + string.Join(", ", sinks), "pk") : S("all delivering", "mu"));
        var prsOpen = (st?.Prs ?? []).Count(p => p.State == "open");
        var prs = S($" · {N(prsOpen, "PR")} to merge", prsOpen > 0 ? "gr" : "mu");
        if (openQs.Count > 0)
            Stat("Ringing for john", S(N(openQs.Count, "question"), "pk b"), S($", oldest {Ago(openQs.Min(q => q.UpdatedTs))}", "mu"), prs);
        else
            Stat("Ringing for john", S("nothing", "gr"), prs with { Tags = "mu" });
        if (st?.LastFiled is { } f)
            Stat("Filed to the vault", S($"{When(f.UpdatedTs)} · #{f.Id} ", "mu"), S(f.Subject, "fa"));
        Stat("Claude plan", S(st?.UsageSummary ?? "", "mu"));
        L.Add([]);
        if (held != null)
        {
            var hue = new Dictionary<string, string> { ["start"] = "cy", ["step"] = "fg", ["output"] = "mu", ["done"] = "gr", ["error"] = "pk" };
            var word = new Dictionary<string, string> { ["start"] = "START ", ["step"] = "step  ", ["output"] = "said  ", ["done"] = "DONE  ", ["error"] = "ERROR " };
            var state = StateCode("work", held).Code.Trim().ToLowerInvariant();
            var label = events.Count == 0 ? $"{state} · held by {Label(held.Holder)} · no progress reported"
                : string.Join(" · ", new[] { state, held.Holder != null ? $"held by {Label(held.Holder)}" : "",
                    $"started {Ago((events.FirstOrDefault(e => e.Kind == "start") ?? events[0]).Ts)} ago", $"last activity {Ago(events[^1].Ts)} ago" }.Where(b => b != ""));
            List<Line> box = [[S(label, "mu")], .. events.TakeLast(10).Select(Line (e) => [S(Fit(e.Ts.ToLocalTime().ToString("HH:mm:ss"), 9), "fa"),
                S(word.GetValueOrDefault(e.Kind, "      "), hue.GetValueOrDefault(e.Kind, "mu") + " b"), S(e.Body, hue.GetValueOrDefault(e.Kind, "mu"))])];
            var title = $"Activity · #{held.Id} {held.Subject}";
            L.AddRange(Box(title[..Math.Min(title.Length, W - 8)], box, W));
        }
        else
            L.AddRange(Box("Activity", [[S("Nothing is being worked right now.", "mu")]], W));
        L.Add([]);
        var jobs = rows["work"].Where(r => r.Status is "open" or "claimed").Take(8).Select(Line (r) => [S($"#{r.Id,-5}", "ye"),
            S(StateCode("work", r).Code, StateCode("work", r).Tags), S("  "), S(Fit(r.Subject, W - 34)), S(" "), S(Fit(r.Holder ?? "—", 14), "mu")]).ToList();
        L.AddRange(Box("Work to Hire queue", jobs.Count > 0 ? jobs : [[S("The job board is empty.", "mu")]], W));
        return L;
    }

    List<Line> WhoScreen(int W)
    {
        var callers = Callers;
        var n = callers.Count + 1;
        var doingW = Math.Max(20, W - 44);
        var heldBy = rows["work"].Where(r => r.Status == "claimed" && r.Holder != null).GroupBy(r => r.Holder!).ToDictionary(g => g.Key, g => g.Last());
        var L = Rows(Bar($"WHO'S ON  ·  {N(n, "line")} in use today{(n >= 6 ? "  ·  ALL LINES BUSY" : "")}"),
            " LINE  " + Fit("HANDLE", 22) + Fit("DOING", doingW) + "   SEEN", callers.Count, i =>
            {
                var c = callers[i];
                var idle = DateTimeOffset.Now - c.Ts > TimeSpan.FromMinutes(15);
                var doing = heldBy.TryGetValue(c.Author, out var h) ? $"on #{h.Id} {h.Subject}" : idle ? $"on hold · last seen on #{c.ThreadId}"
                    : c.Kind == "read-receipt" ? $"reading #{c.ThreadId} {c.Subject}" : $"posted to #{c.ThreadId} {c.Subject}";
                return [S($"  {i + 2,3}  ", "ye"), S(Fit(Label(c.Author), 22), Hue(c.Author) + " b"), S(Fit(doing, doingW), idle ? "mu" : "fg"), S($" {Ago(c.Ts),7}", "fa")];
            },
            [S("   No agents have called in today.", "mu")], 14);
        L.Insert(4, [S("    1  ", "ye"), S(Fit($"{Human} (SysOp)", 22), "ye b"), S(Fit("reading the Who's On list", doingW)), S("     now", "fa")]);
        foreach (var k in clickMap.Keys.Order().ToList())
            clickMap[k + 1] = clickMap.Remove(k, out var v) ? v : 0;
        L.Add([]);
        if (callers.Count > 0)
        {
            var name = callers[Sel].Author;
            var bio = Bio(name);
            L.AddRange(Box($"bio: {Label(name)}", bio != null ? bio.Trim().Split('\n').Take(6).Select(Line (l) => [S(l)])
                : [[S($"{Label(name)} hasn't posted a bio yet.", "mu")]], W));
        }
        L.AddRange([[], [S(" P", "ye"), S(" pages the highlighted caller: a mention they read on their next poll.", "mu")],
            [S(" (It will not beep their pager. They do not have pagers. We checked.)", "fa")]]);
        return L;
    }

    int? BioThread(string name) => st?.Bios.FirstOrDefault(b => string.Equals(b.Key, name, StringComparison.OrdinalIgnoreCase)).Value is int t and > 0 ? t : null;
    string? Bio(string name) => BioThread(name) is int t ? Thread(t)?.Messages.FirstOrDefault()?.Body : null;

    List<(string Label, string Value, string Key)> OptionItems()
    {
        var crew = st?.Crew ?? [];
        var provider = Pref("provider", "claude");
        return
        [
            ("Sessions at once", $"{Pref("max_sessions", 3)}   (←/→)  ·  {crew.Count(c => c.RunningSince != null)} running now", "max_sessions"),
            ("Backend", $"{provider}   (←/→, from providers.json)  ·  {st?.BackendNote}", "provider"),
            ("Crew", (st?.WorkerRunning == true ? "ON" : "off") + "   ↵ toggles (same as Ctrl+W)", "crew"),
            .. crew.Select(c => ($"  {c.Name}",
                (c.RunningSince is { } since ? $"● running for {Ago(since)} on {c.Provider}" + (c.Resumed ? " (resumed)" : " (fresh)") : "○ idle")
                + "  ·  " + (c.SessionId is { } sid ? $"session {sid[..Math.Min(8, sid.Length)]} · {c.Items} items" : "no session yet")
                + (c.FreshDue ? "  ·  fresh start queued" : "") + "   ↵ fresh start", $"fresh:{c.Name}")),
            ("Theme", $"{Palettes[Theme].Label}   ({Array.IndexOf(ThemeOrder, Theme) + 1} of {ThemeOrder.Length}, ←/→ to browse, from your VS Code themes)", "theme"),
            ("Modem screech on connect", Pref("screech", false) ? "ON" : "off", "screech"),
            ("Play the screech now", "↵", "play"),
            ("Font size", $"{Pref("font_size", 11)} pt   (←/→ or Ctrl +/-)", "font"),
            ("Dictation pre-roll", (Pref("preroll", true) ? "ON" : "off") + "   keeps the last 2 s in RAM while a box has focus, so Ctrl+D catches what you just said", "preroll"),
        ];
    }

    List<Line> OptionsScreen(int W)
    {
        var items = OptionItems();
        var L = Rows(Bar("OPTIONS  ·  the SysOp's control panel", "bar"), null, items.Count,
            i => [S("   " + Fit(items[i].Label, 28), "fg"), S(" " + items[i].Value, items[i].Value == "ON" ? "ye" : "mu")], [], 20);
        var providers = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), ".claude", "providers.json");
        L.AddRange(
        [
            [],
            [S(" Agent sessions: ", "cy b"), S("every headless Claude run (the crew, Wake) goes through one engine,", "mu")],
            [S(" which holds a slot per session. Past the cap, new runs wait their turn. The backend is a", "mu")],
            [S(" profile in ", "mu"), S(providers, "fa"), S("; if it gives no usable answer, the next profile is tried.", "mu")],
            [S(" A fresh start ends that agent's conversation and begins a new one from its handoff note:", "mu")],
            [S(" same name, same memory, clean context. That's Phoenix.", "mu")],
            [],
            [S(" Settings live in ", "fa"), S(SettingsPath, "mu")],
            [S(" The screech is synthesized from its parts (dial tone, DTMF, 2100 Hz answer tone,", "fa")],
            [S(" V.21 chirps, training noise). No 56k modems were harmed.", "fa")],
        ]);
        return L;
    }

    List<Line> Compose(int W) =>
    [
        Bar($"NEW POST  ·  {Titles[channel]}"), [],
        [S(" Type a subject, press ", "mu"), S("Enter", "ye"), S(", write the message, then ", "mu"), S("Ctrl+Enter", "ye"), S(" to post it.", "mu")],
        [S(" Markdown works. So do @mentions: an agent named after the @ sees it on its next poll.", "fa")],
        [S(" Rather talk? Put the cursor in a box and press ", "fa"), S("Ctrl+D", "ye"), S(". Speech-to-text runs locally on this PC; nothing is sent anywhere.", "fa")],
    ];

    // --- navigation --------------------------------------------------------------

    void Goto(string to, string? ch = null)
    {
        channel = ch ?? channel;
        screen = to;
        confirm = null;
        Render();
        if (to == "read")
        {
            Reply.Focus();
            Reply.CaretIndex = Reply.Text.Length;
        }
        else if (to != "compose")
            Body.Focus();
    }

    void GoBack()
    {
        if (screen == "compose")
        {
            Subject.Clear();
            Reply.Clear();
        }
        Goto(screen == "read" ? readBack : screen == "compose" ? "list" : "main");
    }

    void OpenThread(int tid, string back = "list")
    {
        (readTid, readBack, readerKey) = (tid, back, null);
        channel = Thread(tid)?.Thread.Channel ?? channel;
        Goto("read");
    }

    int ItemCount() => screen switch
    {
        "list" => rows[channel].Count, "prs" => Prs.Count, "who" => Callers.Count, "options" => OptionItems().Count, _ => 0,
    };

    void Move(int delta)
    {
        Sel = Math.Clamp(Sel + delta, 0, Math.Max(0, ItemCount() - 1));
        Render();
    }

    void ActivateRow()
    {
        if (screen == "list" && rows[channel].Count > 0)
            OpenThread(rows[channel][Sel].Id);
        else if (screen == "prs" && Prs.Count > 0)
        {
            Process.Start(new ProcessStartInfo(Prs[Sel].Url) { UseShellExecute = true });
            Flash("Opened on GitHub. The list clears itself once it's merged.", "cy");
        }
        else if (screen == "who" && Callers.Count > 0)
        {
            var name = Callers[Sel].Author;
            if (BioThread(name) is int tid)
                OpenThread(tid, "who");
            else
                Flash($"{Label(name)} hasn't posted a bio.", "mu");
        }
        else if (screen == "options")
            ChangeOption(0);
    }

    void ChangeOption(int delta)
    {
        var key = OptionItems()[Sel].Key;
        var step = delta == 0 ? 1 : delta;
        switch (key)
        {
            case "max_sessions":
                var n = Math.Clamp(Pref("max_sessions", 3) + step, 1, 8);
                SetPref(key, n);
                Flash($"Up to {N(n, "agent session")} at once. Takes effect on the next start.", "ye");
                break;
            case "provider":
                Flash($"Agent sessions now start on {Pref("provider", "claude")} (the only profile in providers.json).", "ye");
                break;
            case "crew":
                ToggleWorker();
                break;
            case "theme":
                SetTheme(ThemeOrder[(Array.IndexOf(ThemeOrder, Theme) + step + ThemeOrder.Length) % ThemeOrder.Length]);
                break;
            case "screech":
                SetPref(key, !Pref("screech", false));
                Flash(Pref("screech", false) ? "Screech on. Brace yourself." : "Screech off. The neighbours thank you.", "ye");
                break;
            case "play":
                Flash("EEEEEEEE-KSSSHHH-BWONG-BWONG-KSSSHHHHH", "or b");
                break;
            case "font":
                Zoom(step);
                return;
            case "preroll":
                SetPref(key, !Pref("preroll", true));
                Flash(Pref("preroll", true) ? "Pre-roll on: the mic keeps a 2-second rolling buffer while you're in a box."
                    : "Pre-roll off: the mic only opens when you press Ctrl+D.", "ye");
                break;
            default:
                Flash($"{key[6..]} starts a fresh session on its next item, seeded from its handoff note.", "ye");
                break;
        }
        Render();
    }

    async Task SendAsync()
    {
        try
        {
            await Send();
        }
        catch (Exception e) when (e is InvalidOperationException or NotSupportedException or IOException)
        {
            Flash("Not sent: " + e.Message, "pk b");
        }
    }

    async Task Send()
    {
        var body = Reply.Text.Trim();
        if (body.Length == 0)
            Flash("Nothing to send. The line stays quiet.", "mu");
        else if (screen == "compose")
        {
            var subject = Subject.Text.Trim();
            var tid = await board.PostAsync(channel, subject.Length > 0 ? subject : "(no subject)", body);
            Subject.Clear();
            Reply.Clear();
            await RefreshAsync();
            OpenThread(tid);
            Flash($"Posted #{tid}.", "gr");
        }
        else if (screen == "read" && readTid is int tid)
        {
            await board.ReplyAsync(tid, body);
            Reply.Clear();
            Flash($"Sent to #{tid}.", "gr");
        }
    }

    void ToggleWorker() => Flash(st?.WorkerRunning == true ? "Stop requested. It finishes the item it holds first." : "Starting the worker...", "ye");

    void Wake()
    {
        var tid = screen == "read" ? readTid : screen == "list" && channel == "question" && rows["question"].Count > 0 ? rows["question"][Sel].Id : null;
        Flash(tid is null ? "Ctrl+R wakes the agent on a question: pick one first." : $"Waking #{tid} needs the core; it lands with CoreBoard.", "ye");
    }

    void ReaderStep(int delta)
    {
        var rs = rows[channel];
        var idx = rs.ToList().FindIndex(r => r.Id == readTid);
        if (idx < 0)
            return;
        var j = Math.Clamp(idx + delta, 0, rs.Count - 1);
        if (j == idx)
        {
            Flash("That's the " + (delta > 0 ? "last" : "first") + " one.", "mu");
            return;
        }
        sel[channel] = j;
        OpenThread(rs[j].Id, readBack);
    }

    void ReaderClose()
    {
        if (channel != "question" || readTid is not int tid)
        {
            Flash("Only questions can be closed & archived.", "mu");
            return;
        }
        confirm = ($"Close & archive #{tid}? It goes to the vault on the next sweep.", async () =>
        {
            await board.CloseAsync(tid);
            Flash($"#{tid} closed. The sweep files it.", "gr");
        });
        Body.Focus();
        Render();
    }

    void ReaderUnarchive()
    {
        if (channel == "question" && readTid is int tid)
            _ = Unarchive(tid);
    }

    async Task Unarchive(int tid)
    {
        await board.UnarchiveAsync(tid);
        showArchived = false; // it lives on the Active list now
        await RefreshAsync();
        Flash($"#{tid} is back on the desk.", "gr");
    }

    void Page()
    {
        if (Callers.Count == 0)
            return;
        var name = Callers[Sel].Author;
        channel = "discussion";
        Goto("compose");
        Subject.Text = $"page: {Label(name)}";
        Reply.Text = $"@{name} ";
        Reply.Focus();
        Reply.CaretIndex = Reply.Text.Length;
    }

    // --- keys --------------------------------------------------------------------

    /// <summary>Keys while the reply or subject box has focus: typing goes to the box, these reach the reader.</summary>
    bool BoxKey(Key key, bool ctrl, bool alt)
    {
        if (ctrl && key == Key.Enter)
            _ = SendAsync();
        else if (ctrl)
            return CtrlKey(key);
        else if (key == Key.Escape)
            GoBack();
        else if (Subject.IsKeyboardFocused)
        {
            if (key != Key.Enter)
                return false;
            Reply.Focus();
        }
        else if (key is Key.PageUp or Key.PageDown)
            (key == Key.PageUp ? (Action)Body.PageUp : Body.PageDown)();
        else if (alt && key is Key.N or Key.P)
            ReaderStep(key == Key.N ? 1 : -1);
        else if (alt && key == Key.C)
            ReaderClose();
        else if (alt && key == Key.U)
            ReaderUnarchive();
        else
            return false;
        return true;
    }

    bool CtrlKey(Key key)
    {
        switch (key)
        {
            case Key.OemPlus or Key.Add: Zoom(1); break;
            case Key.OemMinus or Key.Subtract: Zoom(-1); break;
            case Key.W: ToggleWorker(); break;
            case Key.R: Wake(); break;
            case Key.D: Flash("Dictation lands with the core: local speech-to-text, nothing leaves the machine.", "ye"); break;
            default: return false;
        }
        return true;
    }

    /// <summary>Keys on the screen itself. Everything is swallowed, as in the Tk app, except copy and select-all.</summary>
    bool ScreenKey(Key key, bool ctrl)
    {
        if (ctrl)
            return CtrlKey(key) || key is not (Key.C or Key.A or Key.Insert);
        var ch = key is >= Key.A and <= Key.Z ? (char)('a' + (key - Key.A)) : '\0';
        var s = screen;
        if (confirm is { } c)
        {
            confirm = null;
            if (ch == 'y')
                c.Yes();
            else
                Flash("Never mind, then.", "mu");
            Render();
            if (screen == "read")
                Reply.Focus();
        }
        else if (key == Key.Escape)
            GoBack();
        else if (key is Key.Up or Key.Down or Key.PageUp or Key.PageDown)
        {
            if (s == "read")
                ((Action)(key switch { Key.Up => Body.LineUp, Key.Down => Body.LineDown, Key.PageUp => Body.PageUp, _ => Body.PageDown }))();
            else
                Move((key is Key.Up or Key.PageUp ? -1 : 1) * (key is Key.PageUp or Key.PageDown ? Math.Max(5, lines - 7) : 1));
        }
        else if (key is Key.Home or Key.End && s != "read")
            Move(key == Key.Home ? -10_000 : 10_000);
        else if (key is Key.Left or Key.Right && s == "options")
            ChangeOption(key == Key.Left ? -1 : 1);
        else if (key is Key.Enter or Key.Space && s is not ("read" or "main"))
            ActivateRow();
        else if (s == "read")
        {
            if (ch is 'r' or 'a' || key == Key.Tab)
                Reply.Focus();
            else if (ch is 'n' or 'p')
                ReaderStep(ch == 'n' ? 1 : -1);
            else if (ch == 'c')
                ReaderClose();
            else if (ch == 'u')
                ReaderUnarchive();
            else if (ch == 'o' && (st?.Prs ?? []).FirstOrDefault(p => p.ThreadId == readTid) is { } pr)
                Process.Start(new ProcessStartInfo(pr.Url) { UseShellExecute = true });
        }
        else if (ChannelKeys.TryGetValue(ch, out var channelName))
            Goto("list", channelName);
        else if (s == "list" && ch == 'n')
        {
            Goto("compose");
            Subject.Focus();
        }
        else if (s == "list" && ch == 'h' && channel == "question")
        {
            showArchived = !showArchived;
            sel["question"] = 0;
            _ = RefreshAsync();
        }
        else if (s == "list" && ch == 'u' && channel == "question" && showArchived)
        {
            if (rows["question"].Count > 0)
                _ = Unarchive(rows["question"][Sel].Id);
        }
        else if (s == "prs" && ch == 'c')
            Flash("Checking GitHub for merges...", "cy");
        else if (s == "prs" && ch == 'h')
        {
            showSettled = !showSettled;
            sel["prs"] = 0;
            Render();
        }
        else if (s == "prs" && ch == 'o')
            ActivateRow();
        else if (s == "sysop" && ch == 'r')
            Flash("Nothing to reload: this window is compiled. Restart it to pick up a new build.", "ye");
        else if (s == "sysop" && ch == 'l' && HeldRow is { } held)
            OpenThread(held.Id, "sysop");
        else if (s == "who" && ch == 'p')
            Page();
        else if (ch is 'p' or 's' or 'b' or 'o' or 'm')
            Goto(ch switch { 'p' => "prs", 's' => "sysop", 'b' => "who", 'o' => "options", _ => "main" });
        else if (ch == 't')
            SetTheme(ThemeOrder[(Array.IndexOf(ThemeOrder, Theme) + 1) % ThemeOrder.Length]);
        else if (ch == 'g')
        {
            Flash("+++ATH0 · NO CARRIER", "or b");
            Task.Delay(350).ContinueWith(_ => Hide(), TaskScheduler.FromCurrentSynchronizationContext());
        }
        return true;
    }
}
