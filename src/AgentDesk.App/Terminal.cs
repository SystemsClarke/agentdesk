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
    IReadOnlyList<Identity> agents = [];
    IReadOnlyList<Adoptable>? adoptables; // read on the Adopt screen only: it scans ~/.claude/projects
    string? webUrl, adoptNote;
    IReadOnlyList<Slot> slots = [];
    (string Name, GoalDetail? Detail)? goal; // the goal open in the reader, and what ui:goal_status said (null: not read yet)

    /// <summary>A few one-line questions asked in the subject box, one at a time (New agent, Adopt's name), then Done with the answers.</summary>
    sealed record Ask(string Title, string Banner, string Back, (string Label, string Help, bool Optional)[] Fields, Func<string[], Task> Done);
    Ask? ask;
    readonly List<string> answers = [];

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
    ThreadRow? HeldRow => rows["work"].FirstOrDefault(r => r.Id == st?.HeldId); // the newest item the Concierge holds
    bool SlackUp => st?.SlackTs is { } t && DateTimeOffset.Now - t < TimeSpan.FromSeconds(90);
    string SelKey => screen == "list" ? channel : screen;
    int Sel { get => sel.GetValueOrDefault(SelKey); set => sel[SelKey] = value; }

    /// <summary>Every goal, the Concierge among them as a standing goal even before it is first turned on.</summary>
    IReadOnlyList<GoalRow> Goals => st?.Goals is { } g && g.Any(x => x.Standing && x.Name == "concierge") ? g
        : [.. st?.Goals ?? [], new GoalRow("concierge", "off", "Keep Work to Hire drained", "concierge-lead", "value <= 0", 0, null, 0, 3, true)];

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
        agents = await board.IdentitiesAsync();
        webUrl = await board.WebUrlAsync();
        slots = await board.SlotsAsync();
        if (screen == "goal" && goal is { } g)
            goal = (g.Name, await board.GoalAsync(g.Name) ?? GoalDetailOf(g.Name));
        if (screen == "adopt")
            adoptables = await board.AdoptableAsync();
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
            "options" => "Options", "compose" => $"New post in {Titles[channel]}", "agents" => "Agents", "adopt" => "Adopt a session",
            "goals" => "Goals", "goal" => $"Goal {goal?.Name}", "ask" => ask?.Title ?? "", _ => $"Reading #{readTid}",
        };
        Line left = [S("AgentDesk", "ye b"), S($" · {title}", "mu")];
        var held = HeldRow;
        Line mid = st?.ConciergeOn == true
            ? [S("● Concierge on", "gr"), held is null ? S(" · idle", "mu") : S($" · #{held.Id}", "ye"), .. If(held?.Holder != null, S($" · {Label(held?.Holder)}", "mu"))]
            : [S("○ Concierge off", "or"), S(" · Ctrl+W starts it", "fa")];
        Line right = [openQs.Count > 0 ? S($"{openQs.Count} ringing for you", "pk b") : S("nobody's calling", "mu"), S(" · ", "fa"),
            SlackUp ? S("SlackNet ● up", "cy") : S("SlackNet ○ down", "fa")];
        if (st?.Budget is { Samples: > 0 } b && W - Len(left) - Len(mid) - Len(right) >= 20) // the governor, where it fits
            right = [S($"week {b.Remaining:0.#}% left", b.Remaining < 10 ? "pk" : "cy"), S(" · ", "fa"), .. right];
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
            "main" => [.. K("Q D W J", "message bases"), .. K("P", "PRs"), .. K("S", "SysOp"), .. K("B", "who's on"), .. K("A", "agents"), .. K("E", "goals"), .. K("O", "options"),
                .. K("G", "hang up")],
            "list" => [.. K("↑↓", "move"), .. K("↵", "read"), .. K("N", "new post"),
                .. If(q, [.. K("H", showArchived ? "active" : "archived"), .. K("Ctrl+R", "wake agent")]), .. K("Esc", "main menu")],
            "read" => [.. K("type", "to reply"), .. K("Ctrl+↵", "send"), .. K("Ctrl+D", "dictate"), .. K("PgUp/PgDn", "scroll"),
                .. K("Alt+N/P", "next/prev"), .. If(q, [.. K("Alt+C", "close"), .. K("Alt+U", "bring back")]), .. K("Esc", "back")],
            "prs" => [.. K("↑↓", "move"), .. K("↵", "open on GitHub"), .. K("C", "check now"), .. K("H", showSettled ? "open only" : "settled"), .. K("Esc", "menu")],
            "sysop" => [.. K("Ctrl+W", st?.ConciergeOn == true ? "stop the Concierge" : "start the Concierge"), .. K("R", "reload code"),
                .. K("J", "job board"), .. K("L", "read held item"), .. K("Esc", "menu")],
            "who" => [.. K("↑↓", "pick a caller"), .. K("↵", "read bio"), .. K("P", "page them"), .. K("Esc", "menu")],
            "options" => [.. K("↑↓", "move"), .. K("↵", "change"), .. K("←→", "adjust"), .. K("C", "ops console"), .. K("Esc", "menu")],
            "compose" => [.. K("↵", "subject → body"), .. K("Ctrl+↵", "post"), .. K("Ctrl+D", "dictate"), .. K("Esc", "cancel")],
            "agents" => [.. K("↑↓", "move"), .. K("↵", "attach"), .. K("S", "start/stop"), .. K("N", "new agent"), .. K("F", "forget"),
                .. K("A", "adopt a session"), .. K("Esc", "menu")],
            "adopt" => [.. K("↑↓", "move"), .. K("↵", "adopt"), .. K("Esc", "agents")],
            "goals" => [.. K("↑↓", "move"), .. K("↵", "read"), .. K("A", "approve"), .. K("X", "stop"), .. K("N", "new goal"), .. K("L", "attach to lead"),
                .. K("Esc", "menu")],
            "goal" => [.. K("A", "approve"), .. K("X", "stop"), .. K("L", "attach to lead"), .. K("↑↓", "scroll"), .. K("Esc", "goals")],
            "ask" => [.. K("↵", "next"), .. K("Esc", "cancel")],
            _ => [],
        };
    }

    Line BarLine(int W) => flash is { } f ? Pad([S(" " + f.Text, f.Tags + " inv")], W, "inv") : Pad(Hints(), W, "inv");

    // --- shared pieces -----------------------------------------------------------

    static List<Line> Box(string title, IEnumerable<Line> body, int W)
    {
        List<Line> o = [[S("┌─ ", "rule"), S(title, "mu"), S(" " + Rep('─', W - 5 - title.Length) + "┐", "rule")]];
        o.AddRange(body.Select(Line (segs) => [S("│ ", "rule"), .. Pad(Len(segs) > W - 4 ? ClipTo(segs, W - 4) : segs, W - 4), S(" │", "rule")]));
        o.Add([S("└" + Rep('─', W - 2) + "┘", "rule")]);
        return o;
    }

    Line Bar(string text, string tag = "barcy") => Pad([S(" " + text, tag + " b")], cols, tag);

    /// <summary>The one list renderer behind every selectable screen: header, a scrolled window of rows, the selected one in reverse.</summary>
    List<Line> Rows(Line banner, string? header, int count, Func<int, Line> row, Line empty, int fixedLines, Func<int, Line?>? before = null,
        IEnumerable<Line>? head = null)
    {
        List<Line> L = [banner, []];
        if (head != null)
            L.AddRange([.. head, []]);
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
        var sysop = st?.ConciergeOn != true ? S("Concierge off · Ctrl+W starts it", "or") : S(held is null ? "Concierge idle" : $"Concierge on #{held.Id}", "gr");
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
            ("A", "Agents", this.agents.Count == 0 ? S("none signed up", "mu")
                : S($"{this.agents.Count(a => a.State == "running")} running · {this.agents.Count(a => a.State == "queued")} queued", "gr")),
            ("E", "Goals", Goals.Count(g => g.State == "running") is var on and > 0 ? S($"{on} running", "gr")
                : S("none running", "mu")),
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
        L.Add([S(" Main menu ", "fg"), S("[", "mu"), S("Q,D,W,J,P,S,B,O,A,E,G", "ye"), S("]", "mu"), S(": "), S(" ", "cur")]);
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
                var last = q ? Fit(r.LastAuthor == Human ? DeliveryMark(r) : r.FollowUp is { } fu ? "↩ " + fu : r.LastAuthor is { } la ? "↩ " + Label(la) : "—", 12) : "";
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
            L.AddRange(Markdown(m.Body, W - 2).Select(Line (l) => [S(" "), .. receipt ? l.Select(s => s with { Tags = (s.Tags + " rcpt").Trim() }) : l]));
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
        void Stat(string k, params Seg[] segs) => L.Add(ClipTo([S(" " + Fit(k, 20), "mu"), .. segs], W + 1));
        var held = HeldRow;
        var swarm = st?.Swarm ?? [];
        if (st?.ConciergeOn == true && held != null)
            Stat("Concierge", [S($"● on · #{held.Id} for {Ago(held.UpdatedTs)}", "gr"), .. If(held.Holder != null, S($" · {Label(held.Holder)}", "mu")),
                S($" · {N(swarm.Count, "member")}", "mu")]);
        else if (st?.ConciergeOn == true)
            Stat("Concierge", S("● on · idle, the queue is empty", "gr"));
        else
            Stat("Concierge", S("○ off", "or"), S("  Ctrl+W starts it", "fa"));
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
        Stat("Usage governor", S(GovLine(st?.Budget), "mu"));
        L.Add([]);
        if (swarm.Count > 0)
        {
            List<Line> box = [.. swarm.Select(Line (m) => [S(Fit(Label(m.Identity), 22), "cy"), S(m.WorkId is { } w ? $"#{w,-5} " : "      ", "ye"),
                S(Fit(m.Task.ReplaceLineEndings(" "), Math.Max(10, W - 38)), "mu")])];
            L.AddRange(Box($"The Concierge's swarm · {N(swarm.Count, "member")}", box, W));
        }
        else
            L.AddRange(Box("The Concierge's swarm", [[S(st?.ConciergeOn == true ? "No members running: nothing is being worked right now." : "Off. Ctrl+W starts the Concierge.", "mu")]], W));
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
        return
        [
            ("Sessions at once", $"{Pref("max_sessions", 3)}   (←/→)  ·  {st?.LiveSessions ?? 0} running now", "max_sessions"),
            ("Concierge", (st?.ConciergeOn == true ? "ON" : "off") + "   ↵ toggles (same as Ctrl+W)", "concierge"),
            ("Theme", $"{Palettes[Theme].Label}   ({Array.IndexOf(ThemeOrder, Theme) + 1} of {ThemeOrder.Length}, ←/→ to browse, from your VS Code themes)", "theme"),
            ("Modem screech on connect", Pref("screech", false) ? "ON" : "off", "screech"),
            ("Play the screech now", "↵", "play"),
            ("Font size", $"{Pref("font_size", 11)} pt   (←/→ or Ctrl +/-)", "font"),
            ("Dictation pre-roll", (Pref("preroll", true) ? "ON" : "off") + "   keeps the last 2 s in RAM while a box has focus, so Ctrl+D catches what you just said", "preroll"),
            ("Ops console", webUrl is null ? "not running (the core log says why)" : $"{Unkeyed(webUrl)}   (key hidden)  ·  ↵ or C opens it in the browser", "web"),
        ];
    }

    /// <summary>The release this window came from (build.ps1 -Package stamps it); a local dev build says so.</summary>
    static readonly string Version = System.Reflection.Assembly.GetEntryAssembly()?
        .GetCustomAttributes(typeof(System.Reflection.AssemblyInformationalVersionAttribute), false)
        .OfType<System.Reflection.AssemblyInformationalVersionAttribute>().FirstOrDefault()?.InformationalVersion.Split('+')[0] is { } v
        && v != "1.0.0" ? v : "dev build";

    List<Line> OptionsScreen(int W)
    {
        var items = OptionItems();
        var L = Rows(Bar("OPTIONS  ·  the SysOp's control panel", "bar"), null, items.Count,
            i => [S("   " + Fit(items[i].Label, 28), "fg"), S(" " + items[i].Value, items[i].Value == "ON" ? "ye" : "mu")], [], 20);
        L.AddRange(
        [
            [],
            [S(" Agent sessions: ", "cy b"), S("every agent the core runs (the Concierge and its swarm, goals, Wake) is an", "mu")],
            [S(" identity holding one slot. Past the cap, new ones queue and start as slots free up.", "mu")],
            [S(" The Concierge ", "cy b"), S("keeps Work to Hire drained: its lead claims each open item and hands it to a", "mu")],
            [S(" small swarm, which completes it with a report on its thread. Off until you turn it on.", "mu")],
            [],
            [S(" Version ", "fa"), S(Version, "ye"), S("   ·   updates arrive from GitHub Releases; the tray offers Restart to update", "fa")],
            [S(" Settings live in ", "fa"), S(SettingsPath, "mu")],
            [S(" The screech is synthesized from its parts (dial tone, DTMF, 2100 Hz answer tone,", "fa")],
            [S(" V.21 chirps, training noise). No 56k modems were harmed.", "fa")],
        ]);
        return L;
    }

    /// <summary>The ops console's address without its key, safe to show on screen.</summary>
    internal static string Unkeyed(string url) => url.Split('?')[0];

    internal static Line AgentRow(Identity a, int folderW) =>
    [
        S("  " + Fit(a.Name, 20), Hue(a.Name) + " b"),
        S(Fit(a.State, 9), a.State switch { "running" => "gr", "queued" => "ye", _ => "fa" }),
        S(Fit($"{a.Generation}", 5), "mu"), S(Fit(a.Host, 13), "cy"), S(Fit(a.Model ?? "—", 8), "mu"), S(Fit(a.Folder, folderW), "fa"),
    ];

    static readonly string Home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);

    /// <summary>A folder with the home folder as ~, so the part that tells them apart fits.</summary>
    internal static string Tilde(string folder) => folder.StartsWith(Home + "\\", StringComparison.OrdinalIgnoreCase) ? "~" + folder[Home.Length..] : folder;

    internal static Line AdoptRow(Adoptable a, int folderW, int msgW) =>
        [S("  " + Fit(Tilde(a.Folder), folderW), "cy"), S(" " + Fit(When(a.LastActivity), 11), "fa"), S(Fit(a.FirstMessage, msgW))];

    List<Line> AgentsScreen(int W)
    {
        var folderW = Math.Max(20, W - 57);
        var L = Rows(Bar("AGENTS  ·  the switchboard  ·  long-lived agents, one line each, and Phoenix keeps them going"),
            "  " + Fit("NAME", 20) + Fit("STATE", 9) + Fit("GEN", 5) + Fit("HOST", 13) + Fit("MODEL", 8) + "FOLDER", agents.Count,
            i => AgentRow(agents[i], folderW),
            [S("   No agents yet. ", "mu"), S("N", "ye"), S(" signs one up; ", "mu"), S("A", "ye"), S(" adopts a live Claude session.", "mu")], adoptNote is null ? 11 : 15);
        L.Add([S($" {agents.Count(a => a.State == "running")} running", "gr"), S(" · "), S($"{agents.Count(a => a.State == "queued")} queued", "ye"), S(" · "),
            S($"{agents.Count(a => a.State == "stopped")} stopped", "fa"), S($" · at most {Pref("max_sessions", 3)} at once (Options)", "mu"),
            .. If(window.Count > window.Visible, S($" · rows {window.Top + 1}–{window.End} of {window.Count}", "fa"))]);
        L.AddRange([[], [S(" ↵", "ye"), S(" opens a console attached to it (", "mu"), S("agentdesk attach <name>", "cy"), S("). Ctrl+] detaches; it keeps running.", "mu")],
            [S(" A", "ye"), S(" adopts a Claude Code conversation from the desktop app, so it runs here instead.", "mu")]]);
        if (adoptNote != null)
            L.AddRange([[], .. Box("Adopted", [[S(adoptNote, "ye")]], W)]);
        return L;
    }

    List<Line> AdoptScreen(int W)
    {
        var rs = adoptables ?? [];
        var (folderW, msgW) = (Math.Min(44, W / 3), Math.Max(20, W - Math.Min(44, W / 3) - 15));
        var L = Rows(Bar("ADOPT  ·  take over a live Claude session  ·  same conversation, now AgentDesk runs it"),
            "  " + Fit("FOLDER", folderW) + " " + Fit("LAST", 11) + "FIRST MESSAGE", rs.Count, i => AdoptRow(rs[i], folderW, msgW),
            [S(adoptables is null ? "   Looking through ~/.claude/projects..." : "   No Claude Code conversations in the last 24 hours that anyone typed in.", "mu")], 9);
        L.Add([S($" {N(rs.Count, "conversation")} from the last 24 hours, newest first", "mu"),
            .. If(window.Count > window.Visible, S($" · rows {window.Top + 1}–{window.End} of {window.Count}", "fa"))]);
        L.AddRange([[], [S(" ↵", "ye"), S(" asks for a name, then starts it here as an agent in its own folder, resuming the conversation.", "mu")],
            [S(" Then close it in the Claude desktop app: two programs writing one conversation will garble it.", "fa")]]);
        return L;
    }

    /// <summary>The governor in one line, for SysOp: the week left, its reset, the forecast, and today's allowance.</summary>
    internal static string GovLine(Budget? b) => b is not { Samples: > 0 } ? "no usage samples yet: the core reads /usage every 5 minutes"
        : $"{b.Remaining:0.#}% left · resets in {Span(b.ResetInHours * 3600)} · forecast {b.ProjectedEnd:0.#}% · "
            + $"{N(b.Sessions, "session")} ({b.Swarms}x{b.Members})" + (b.Mode is { } m ? $" · {m}" : "");

    /// <summary>The Goals screen's budget panel: the week, the forecast and the mode, today's allowance and why, and the week so far.</summary>
    internal static List<Line> BudgetLines(Budget? b, int W)
    {
        if (b is not { Samples: > 0 })
            return [[S("No usage samples yet. The core reads /usage every 5 minutes; the governor starts from the first reading.", "mu")]];
        return
        [
            [S($"{b.Remaining:0.#}%", b.Remaining < 10 ? "pk b" : "ye b"), S(" of the week left · resets in ", "mu"), S(Span(b.ResetInHours * 3600)),
                S(" · forecast ", "mu"), S($"{b.ProjectedEnd:0.#}%", b.ProjectedEnd > 100 ? "pk" : "gr"), S(" at the reset", "mu"),
                .. If(b.Mode != null, S("   "), S($" {b.Mode} ", b.Mode == "enforcing" ? "pk b inv" : "cy inv"))],
            [S("Today's allowance: ", "mu"), S(N(b.Sessions, "session"), "fg b"), S(" · ", "mu"), S($"{N(b.Swarms, "swarm")} x {N(b.Members, "member")}")],
            [S(b.Reason, "fa")],
            .. b.Series.Count > 1 ? [[S("This week  ", "mu"), .. Spark(b.Series, "cy", "%", W - 36)]] : new List<Line>(),
        ];
    }

    static string Value(double? v) => v is { } x ? x.ToString("0.##", System.Globalization.CultureInfo.CurrentCulture) : "—";
    static string LineOf(string? success) => success is null ? "no line yet" : success.StartsWith("value ") ? success[6..] : success;
    static string GoalState(GoalRow g) => g.Standing && g.State == "running" ? "standing" : g.State;
    static string StateHue(string state) => state switch { "running" or "standing" => "gr", "draft" => "ye", "succeeded" => "cy", _ => "fa" };

    /// <summary>One goal on the Goals screen: its state, slot, last value against its line, experiments, crew, and lead with its generation.</summary>
    internal static Line GoalLine(GoalRow g, int? slot, Identity? lead, int leadW) =>
    [
        S("  " + Fit(g.Name, 18), Hue(g.Name) + " b"), S(Fit(GoalState(g), 10), StateHue(GoalState(g))), S(Fit(slot is { } n ? $"{n}" : "—", 5), "cy"),
        S(Fit($"{Value(g.LastValue)} · {LineOf(g.Success)}", 20)), S(Fit($"{g.Experiments}", 5), "mu"), S(Fit($"{g.Members}/{g.MaxMembers}", 7), "mu"),
        S(Fit(lead is null ? g.Lead : $"{g.Lead} · gen {lead.Generation}", leadW), lead?.State == "running" ? "cy" : "fa"),
    ];

    static IEnumerable<string> Wrap(string text, int width)
    {
        var line = "";
        foreach (var word in text.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
        {
            if (line.Length > 0 && line.Length + 1 + word.Length > width)
            {
                yield return line;
                line = "";
            }
            line = line.Length == 0 ? word : line + " " + word;
        }
        yield return line;
    }

    /// <summary>The goal reader: the goal and its budget, its history as a sparkline, the last 10 experiments, the crew, the summary.</summary>
    internal static List<Line> GoalReader(GoalDetail d, Identity? lead, int? slot, int W)
    {
        var g = d.Row;
        var head = $"═ Goal {g.Name} ═ {GoalState(g)}{(slot is { } n ? $" in slot {n}" : "")} ";
        List<Line> L = [[S("╔", "rule"), S(head, "ye b"), S(Rep('═', W - 2 - head.Length) + "╗", "rule")]];
        void Field(string label, string? text, string tags = "fg")
        {
            foreach (var (l, i) in Wrap(text ?? "none yet", W - 16).Select((l, i) => (l, i)))
                L.Add([S(i == 0 ? "  " + Fit(label, 12) : Rep(' ', 14), "mu"), S(l, text is null ? "fa" : tags)]);
        }
        Field("Objective", g.Objective, "fg b");
        Field("Hypothesis", d.Hypothesis ?? (g.State == "draft" ? "none yet: the lead proposes one, then A approves it" : null));
        Field("Measure", d.Measure, "cy");
        Field("Success", g.Success, "ye");
        var spent = d.Started is { } t ? Span((DateTimeOffset.Now - t).TotalSeconds) : "not started";
        Field("Budget", g.Standing ? $"{g.Members} of {g.MaxMembers} members · standing: it never ends by its hours · a wake every {d.CadenceMinutes:0.#} min"
            : $"{g.Members} of {g.MaxMembers} members · {spent} of {d.MaxHours:0.#}h · a wake every {d.CadenceMinutes:0.#} min");
        L.Add([S("╚" + Rep('═', W - 2) + "╝", "rule")]);
        L.Add([S("  " + Fit("History", 12), "mu"), .. d.History.Count > 0 ? Spark(d.History, "cy", "", W - 40) : [S("nothing measured yet", "fa")]]);
        L.Add([]);
        var changeW = Math.Max(16, W - 50);
        var log = d.Experiments.TakeLast(10).Select(Line (e) => [S($"#{e.N,-4}", "ye"), S(Fit(e.Change, changeW)), S(" "), S(Fit(e.Owner ?? "—", 20), Hue(e.Owner)),
            S(Fit(Value(e.Value), 8)), S(Fit(e.Verdict ?? "measuring", 12), e.Verdict switch
            {
                null => "ye", "met" => "gr b", "improved" => "gr", "no gain" => "mu", _ => "pk",
            })]).ToList();
        L.AddRange(Box(d.Experiments.Count > 10 ? $"Experiments · last 10 of {d.Experiments.Count}" : $"Experiments · {d.Experiments.Count}",
            log.Count > 0 ? log : [[S("No experiments yet.", "mu")]], W));
        L.Add([]);
        L.AddRange(Box($"Members · {g.Members} of {g.MaxMembers}", d.Members.Count > 0 ? d.Members.Select(Line (m) => [S(Fit(m.Identity, 24), "cy"),
            S(m.WorkId is { } w ? $"#{w,-5} " : "      ", "ye"), S(Fit(m.Task.ReplaceLineEndings(" "), Math.Max(10, W - 38)), "mu")]) : [[S("None running.", "mu")]], W));
        L.Add([]);
        L.AddRange(Box("Summary · what its agents are woken with", d.Summary.Length > 0
            ? d.Summary.Trim().Split('\n').SelectMany(l => Wrap(l.TrimEnd(), W - 4)).Take(12).Select(Line (l) => [S(l, "mu")]) : [[S("(none)", "fa")]], W));
        L.AddRange([[], [S(" L", "ye"), S(" attaches to ", "mu"), S(g.Lead, "cy"),
            S(lead is null ? " (not an agent now)" : $" (generation {lead.Generation}, {lead.State})", "mu"), S(": agentdesk attach " + g.Lead, "cy")]]);
        return L;
    }

    int? SlotOf(string goal) => slots.FirstOrDefault(s => string.Equals(s.Goal, goal, StringComparison.OrdinalIgnoreCase))?.N;
    Identity? LeadOf(GoalRow g) => agents.FirstOrDefault(a => string.Equals(a.Name, g.Lead, StringComparison.OrdinalIgnoreCase));

    List<Line> GoalsScreen(int W)
    {
        var gs = Goals;
        var leadW = Math.Max(16, W - 67);
        var budget = Box("Budget · the usage governor", BudgetLines(st?.Budget, W - 4), W);
        var L = Rows(Bar("GOALS  ·  the war room  ·  each swarm is a hypothesis, a measure and a line to cross"),
            "  " + Fit("GOAL", 18) + Fit("STATE", 10) + Fit("SLOT", 5) + Fit("LAST · LINE", 20) + Fit("EXP", 5) + Fit("CREW", 7) + "LEAD", gs.Count,
            i => GoalLine(gs[i], SlotOf(gs[i].Name), LeadOf(gs[i]), leadW),
            [S("   No goals yet. ", "mu"), S("N", "ye"), S(" starts one.", "mu")], 12 + budget.Count, head: budget);
        L.Add([S($" {gs.Count(g => g.State == "running")} running", "gr"), S(" · "), S($"{gs.Count(g => g.State == "draft")} draft", "ye"), S(" · "),
            S($"{slots.Count(s => s.Goal != null)} of {Math.Max(10, slots.Count)} Slack slots in use", "cy"),
            .. If(window.Count > window.Visible, S($" · rows {window.Top + 1}–{window.End} of {window.Count}", "fa"))]);
        L.AddRange([[], [S(" ↵", "ye"), S(" reads a goal. ", "mu"), S("A", "ye"), S(" approves a proposed draft (or starts a stopped one); ", "mu"), S("X", "ye"),
                S(" stops it.", "mu")],
            [S(" N", "ye"), S(" starts a goal: its lead proposes a hypothesis, a measure and a line, and waits for your A.", "mu")]]);
        return L;
    }

    GoalDetail? GoalDetailOf(string name) => Goals.FirstOrDefault(g => g.Name == name) is { } r
        ? new(r, null, r.Standing ? "internal:open_work" : null, 0, 10, null, [], [], [], r.State == "off" ? "Off. A (or Ctrl+W) turns the Concierge on." : "")
        : null;

    List<Line> GoalScreen(int W) => goal is not { } g ? [] : g.Detail is null ? [[S(" Reading the goal's log...", "mu")]]
        : GoalReader(g.Detail, LeadOf(g.Detail.Row), SlotOf(g.Name), W);

    List<Line> AskScreen(int W)
    {
        List<Line> L = [Bar(ask!.Banner), []];
        for (var i = 0; i < ask.Fields.Length; i++)
        {
            var (label, help, _) = ask.Fields[i];
            L.Add(i < answers.Count ? [S("   " + Fit(label, 10), "mu"), answers[i].Length > 0 ? S(answers[i], "fg b") : S("(none)", "fa")]
                : i == answers.Count ? [S(" ▶ " + Fit(label, 10), "ye b"), S(help, "fg")] : [S("   " + Fit(label, 10), "fa"), S(help, "fa")]);
        }
        L.AddRange([[], [S(" Type in the box below and press ", "mu"), S("Enter", "ye"), S(" for the next one. ", "mu"), S("Esc", "ye"), S(" cancels.", "mu")]]);
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
        else if (to == "ask")
            Subject.Focus();
        else if (to != "compose")
            Body.Focus();
        if (to is "adopt" or "goal")
            _ = RefreshAsync();
    }

    void GoBack()
    {
        if (screen is "compose" or "ask")
            Subject.Clear();
        if (screen == "compose")
            Reply.Clear();
        if (screen == "agents")
            adoptNote = null;
        Goto(screen switch { "read" => readBack, "compose" => "list", "ask" => ask!.Back, "adopt" => "agents", "goal" => "goals", _ => "main" });
    }

    void AskFor(Ask a, string prefill = "")
    {
        (ask, adoptNote) = (a, null);
        answers.Clear();
        Goto("ask");
        Subject.Text = prefill;
        Subject.CaretIndex = prefill.Length;
    }

    /// <summary>Enter in the box: take this answer, then ask the next one, or go back and hand them all to Done.</summary>
    async Task AskNext()
    {
        var (a, text) = (ask!, Subject.Text.Trim());
        var field = a.Fields[answers.Count];
        if (text.Length == 0 && !field.Optional)
        {
            Flash($"It needs a {field.Label}. Esc cancels.", "ye");
            return;
        }
        answers.Add(text);
        Subject.Clear();
        if (answers.Count < a.Fields.Length)
        {
            Render();
            return;
        }
        var got = answers.ToArray();
        GoBack();
        try
        {
            await a.Done(got);
        }
        catch (Exception e) when (e is InvalidOperationException or IOException)
        {
            Flash("Not done: " + e.Message, "pk b");
        }
    }

    void NewAgent() => AskFor(new("New agent", "NEW AGENT  ·  sign up a long-lived agent  ·  it keeps one Claude conversation and hands off at 60%", "agents",
        [("name", "What to call it: agentdesk attach <name> and its board posts use this.", false),
         ("folder", "The folder it works in, e.g. C:\\Users\\you\\src\\repo.", false),
         ("charter", "Optional: what it is for, appended to its system prompt. Enter skips it.", true)],
        async a =>
        {
            await board.ActAsync("ui:identity_create", new { name = a[0], folder = a[1], charter = a[2].Length > 0 ? a[2] : null });
            await RefreshAsync();
            Sel = agents.ToList().FindIndex(x => x.Name == a[0]);
            Flash($"{a[0]} is signed up. S starts it.", "gr");
        }));

    void Adopt()
    {
        if (adoptables is not { Count: > 0 } rs)
            return;
        var s = rs[Sel];
        AskFor(new("Adopt a session", $"ADOPT  ·  {s.Folder}  ·  {When(s.LastActivity)}", "adopt",
            [("name", "What to call it from now on: agentdesk attach <name> and its board posts use this.", false)],
            async a =>
            {
                var note = await board.ActAsync("ui:adopt", new { session_id = s.SessionId, name = a[0] });
                Goto("agents");
                adoptNote = note ?? "If this conversation is still open in the Claude desktop app, close it there.";
                await RefreshAsync();
                Sel = agents.ToList().FindIndex(x => x.Name == a[0]);
                Flash($"{a[0]} adopted. Close that conversation in the Claude desktop app.", "gr");
            }), Path.GetFileName(s.Folder.TrimEnd('\\', '/')).ToLowerInvariant());
    }

    /// <summary>Enter on Agents: a new console running agentdesk attach, beside this window or from the install folder.</summary>
    void Attach(Identity a)
    {
        if (a.State != "running")
        {
            Flash(a.State == "queued" ? $"{a.Name} is waiting for a free slot." : $"{a.Name} isn't running. S starts it.", "ye");
            return;
        }
        var exe = new[] { AppContext.BaseDirectory, Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "AgentDeskApp", "current") }
            .Select(d => Path.Combine(d, "agentdesk.exe")).FirstOrDefault(File.Exists);
        if (exe is null)
        {
            Flash("agentdesk.exe isn't beside this window or in the install folder.", "pk b");
            return;
        }
        Process.Start(new ProcessStartInfo(exe, $"attach \"{a.Name}\"") { UseShellExecute = true });
        Flash($"Attached to {a.Name} in a new console. Ctrl+] detaches; it keeps running.", "cy");
    }

    async void StartStop()
    {
        if (agents.Count == 0)
            return;
        var a = agents[Sel];
        var stop = a.State != "stopped";
        Flash(stop ? $"Stopping {a.Name}..." : $"Starting {a.Name}...", "ye");
        try
        {
            await board.ActAsync(stop ? "ui:identity_stop" : "ui:identity_start", new { name = a.Name });
            await RefreshAsync();
            var now = agents.FirstOrDefault(x => x.Name == a.Name)?.State;
            Flash(now switch
            {
                "running" => $"{a.Name} is running. ↵ attaches.",
                "queued" => $"{a.Name} is queued: {Pref("max_sessions", 3)} already running. It starts when one stops.",
                _ => $"{a.Name} stopped. Its conversation is kept; S resumes it.",
            }, now == "running" ? "gr" : "ye");
        }
        catch (Exception e) when (e is InvalidOperationException or IOException)
        {
            Flash("Not done: " + e.Message, "pk b");
        }
    }

    void Forget()
    {
        if (agents.Count == 0)
            return;
        var name = agents[Sel].Name;
        confirm = ($"Forget {name}? It stops and leaves the list; its Claude conversation stays on disk.", async () =>
        {
            await board.ActAsync("ui:identity_forget", new { name });
            await RefreshAsync();
            Flash($"{name} forgotten.", "gr");
        });
        Render();
    }

    /// <summary>The goal under the cursor, or the one open in the reader.</summary>
    GoalRow? SelGoal => screen == "goal" ? goal?.Detail?.Row : Goals.Count > 0 ? Goals[Math.Min(Sel, Goals.Count - 1)] : null;

    /// <summary>A: John approves a proposed goal (ui:goal_approve), or starts a stopped one; the Concierge is turned on as Ctrl+W does.</summary>
    async void Approve()
    {
        if (SelGoal is not { } g)
            return;
        try
        {
            await (g.Standing ? board.ActAsync("ui:concierge", new { on = true }) : board.ActAsync("ui:goal_approve", new { name = g.Name }));
            await RefreshAsync();
            Flash($"{g.Name} is running: until its line, its budget, or X.", "gr");
        }
        catch (Exception e) when (e is InvalidOperationException or IOException)
        {
            Flash("Not approved: " + e.Message, "pk b");
        }
    }

    void StopGoal()
    {
        if (SelGoal is not { } g)
            return;
        confirm = ($"Stop {g.Name}? Its members are forgotten and its lead stops; the log stays.", async () =>
        {
            try
            {
                await (g.Standing ? board.ActAsync("ui:concierge", new { on = false }) : board.ActAsync("ui:goal_stop", new { name = g.Name }));
                await RefreshAsync();
                Flash($"{g.Name} stopped. A starts it again.", "gr");
            }
            catch (Exception e) when (e is InvalidOperationException or IOException)
            {
                Flash("Not stopped: " + e.Message, "pk b");
            }
        });
        Render();
    }

    void NewGoal() => AskFor(new("New goal", "NEW GOAL  ·  start a swarm  ·  its lead proposes a hypothesis, a measure and a line, then you approve", "goals",
        [("name", "A short name: its lead is <name>-lead, and Slack's swarm slot uses it too.", false),
         ("folder", "The folder the lead works and measures in, e.g. C:\\Users\\you\\src\\repo.", false),
         ("objective", "What it should achieve, in a sentence. The lead turns it into a hypothesis.", false)],
        async a =>
        {
            await board.ActAsync("ui:goal_create", new { name = a[0], objective = a[2], folder = a[1] });
            await RefreshAsync();
            Sel = Goals.ToList().FindIndex(x => x.Name == a[0]);
            Flash($"{a[0]} is a draft. Its lead is proposing a hypothesis; A approves it.", "gr");
        }));

    /// <summary>L: attach to the goal's lead, as Enter on Agents does.</summary>
    void AttachLead()
    {
        if (SelGoal is not { } g)
            return;
        if (LeadOf(g) is { } lead)
            Attach(lead);
        else
            Flash($"{g.Lead} isn't an agent now. A starts the goal and its lead.", "ye");
    }

    async void OpenConsole()
    {
        if (await board.WebUrlAsync() is not { } url)
        {
            Flash("The ops console isn't running. The core log says why.", "pk b");
            return;
        }
        Process.Start(new ProcessStartInfo(url) { UseShellExecute = true });
        Flash("Opened the ops console in your browser.", "cy");
    }

    void OpenThread(int tid, string back = "list")
    {
        (readTid, readBack, readerKey) = (tid, back, null);
        channel = Thread(tid)?.Thread.Channel ?? channel;
        Goto("read");
    }

    int ItemCount() => screen switch
    {
        "list" => rows[channel].Count, "prs" => Prs.Count, "who" => Callers.Count, "options" => OptionItems().Count, "agents" => agents.Count,
        "adopt" => adoptables?.Count ?? 0, "goals" => Goals.Count, _ => 0,
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
        else if (screen == "agents" && agents.Count > 0)
            Attach(agents[Sel]);
        else if (screen == "adopt")
            Adopt();
        else if (screen == "goals" && Goals.Count > 0)
        {
            goal = (Goals[Sel].Name, null);
            Goto("goal");
        }
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
            case "concierge":
                ToggleConcierge();
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
            case "web":
                OpenConsole();
                return;
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

    /// <summary>Ctrl+W: John turns the Concierge on or off (ui:concierge); turning it on is its approval.</summary>
    async void ToggleConcierge()
    {
        var on = st?.ConciergeOn != true;
        Flash(on ? "Turning the Concierge on..." : "Concierge off: its swarm is stopped and the items it held go back on the queue.", "ye");
        try
        {
            await board.ActAsync("ui:concierge", new { on });
            await Task.Delay(400); // re-read rather than assume: the start may fail
            await RefreshAsync();
        }
        catch (Exception e) when (e is InvalidOperationException or IOException)
        {
            Flash(e.Message, "pk b");
        }
    }

    /// <summary>Ctrl+R: resume the asking agent's session with John's reply. Only ever on this keypress: a human decides each wake.</summary>
    async void Wake()
    {
        var tid = screen == "read" ? readTid : screen == "list" && channel == "question" && rows["question"].Count > 0 ? rows["question"][Sel].Id : null;
        if (tid is null)
        {
            Flash("Ctrl+R wakes the agent on a question: pick one first.", "ye");
            return;
        }
        try
        {
            var said = await board.ActAsync("ui:wake", new { thread_id = tid }) ?? "";
            Flash(said, said.StartsWith("woke") ? "gr" : "ye");
        }
        catch (Exception e) when (e is InvalidOperationException or IOException)
        {
            Flash("Not woken: " + e.Message, "pk b");
        }
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
            _ = screen == "ask" ? AskNext() : SendAsync();
        else if (ctrl)
            return CtrlKey(key);
        else if (key == Key.Escape)
            GoBack();
        else if (Subject.IsKeyboardFocused)
        {
            if (key != Key.Enter)
                return false;
            if (screen == "ask")
                _ = AskNext();
            else
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
            case Key.W: ToggleConcierge(); break;
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
            if (s is "read" or "goal")
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
        {
            _ = board.ActAsync("ui:check_prs");
            Flash("Checking GitHub for merges...", "cy");
        }
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
        else if (s == "options" && ch == 'c')
            OpenConsole();
        else if (s == "agents" && ch is 's' or 'n' or 'f' or 'a')
            ((Action)(ch switch { 's' => StartStop, 'n' => NewAgent, 'f' => Forget, _ => () => Goto("adopt") }))();
        else if (s is "goals" or "goal" && ch is 'a' or 'x' or 'n' or 'l')
            ((Action)(ch switch { 'a' => Approve, 'x' => StopGoal, 'n' => NewGoal, _ => AttachLead }))();
        else if (ch == 'a')
            Goto("agents");
        else if (ch == 'e')
            Goto("goals");
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
