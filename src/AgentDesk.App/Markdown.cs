using System.Globalization;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;

namespace AgentDesk.App;

/// <summary>Message bodies as the Tk app's mdview drew them. Structure is decided per line first (fences, tables, rules, headings,
/// quotes, lists) and inline spans only run on content, so a bullet's * is never italics and every line break the author typed
/// stays. Everything is word-wrapped to the reader width here, so a wrapped bullet or quote keeps its hanging indent.
/// Extra tags on top of the palette: i u s (italic, underline, strike), pnl (panel background), hd1-hd3 (heading sizes), href:URL.</summary>
public partial class MainWindow
{
    static readonly Regex InlineRx = new(@"`(?<code>[^`\n]+)`|\*\*(?<b>[^*\n]+?)\*\*|__(?<b>[^_\n]+?)__|~~(?<s>[^~\n]+?)~~|\*(?<i>[^*\n]+)\*"
        + @"|!\[(?<img>[^\]\n]*)\]\((?<url>[^)\s]+)\)|\[(?<txt>[^\]\n]+)\]\((?<url>[^)\s]+)\)|<(?<url>https?://[^>\s]+)>|(?<url>https?://[^\s)>\]]+)");
    static readonly Regex HeadingRx = new(@"^(#{1,6})\s+(.+?)\s*#*\s*$"), BulletRx = new(@"^(\s*)[-*+]\s+(.*)$"), NumberedRx = new(@"^(\s*)(\d+)[.)]\s+(.*)$"),
        RuleRx = new(@"^\s*(?:-{3,}|\*{3,}|_{3,})\s*$"), QuoteRx = new(@"^\s*>\s?(.*)$"), TaskRx = new(@"^\[( |x|X)\]\s+(.*)$"),
        DelimRx = new(@"^\s*(:?)-+(:?)\s*$"), CalloutRx = new(@"^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\]\s*(.*)$", RegexOptions.IgnoreCase),
        TokenRx = new(@"\s+|\S+"), CodeRx = new(@"(?<fa>#.*$|//.*$|--\s.*$|/\*.*?\*/)|(?<ye>""(?:\\.|[^""\\])*""|'(?:\\.|[^'\\])*'|`[^`]*`)"
            + @"|(?<pu>\b\d+(?:\.\d+)?\b)|(?<or>\$[A-Za-z_][\w:]*|@[A-Za-z_]\w*)|(?<w>\b[A-Za-z_][\w-]*\b)");
    static readonly Dictionary<string, (string Hue, string Title)> Callouts = new()
    {
        ["NOTE"] = ("cy", "ⓘ NOTE"), ["TIP"] = ("gr", "✓ TIP"), ["IMPORTANT"] = ("pu", "★ IMPORTANT"), ["WARNING"] = ("ye", "⚠ WARNING"), ["CAUTION"] = ("pk", "✖ CAUTION"),
    };
    static readonly HashSet<string> Keywords = [.. ("and as assert async await break case catch class const continue def default del do elif else enum except export "
        + "extends false False finally for foreach from func function if import in interface is lambda let match new None nil not null or param pass private "
        + "protected public raise return self static struct super switch this throw true True try type typeof using var void where while with yield begin end "
        + "select join on group by order insert update delete into values create table Get Set New Remove Write-Host process").Split(' ')];
    static readonly string[] Hues = ["cy", "gr", "ye", "pu", "or", "pk"];

    List<Line> Markdown(string body, int W)
    {
        List<Line> L = [];
        var lines = body.Replace("\r", "").Split('\n');
        for (var i = 0; i < lines.Length; i++)
        {
            var line = lines[i];
            Match m;
            if (line.Trim().StartsWith("```"))
            {
                var start = i = i + 1; // verbatim to the closing fence; an unterminated fence takes the rest
                while (i < lines.Length && !lines[i].Trim().StartsWith("```"))
                    i++;
                L.AddRange(Fence(line.Trim()[3..].Trim().ToLowerInvariant(), lines[start..Math.Min(i, lines.Length)], W));
            }
            else if (TableAt(lines, i) is { } t) // before rules: a delimiter row is hyphens too
            {
                L.AddRange(Table(t.Rows, t.Aligns, W));
                i = t.End - 1;
            }
            else if (RuleRx.IsMatch(line))
                L.Add([S(Rep('─', W), "rule")]);
            else if ((m = HeadingRx.Match(line)).Success)
            {
                var (depth, pt) = (m.Groups[1].Length, Pref("font_size", 11));
                var head = Inline(m.Groups[2].Value, $"ye b hd{Math.Min(depth, 4)}");
                L.AddRange(Wrap(head, W * pt / (pt + Math.Max(0, 4 - depth))));
                if (depth == 1)
                    L.Add([S(Rep('═', Math.Min(W, Len(head) + 2)), "ye")]);
            }
            else if (QuoteRx.IsMatch(line))
            {
                List<string> q = [];
                for (; i < lines.Length && QuoteRx.Match(lines[i]) is { Success: true } qm; i++)
                    q.Add(qm.Groups[1].Value);
                i--;
                var c = CalloutRx.Match(q[0].Trim());
                var (hue, title) = c.Success ? Callouts[c.Groups[1].Value.ToUpperInvariant()] : ("rule", null);
                if (title != null)
                {
                    L.Add([S("▌ ", hue), S(title, hue + " b")]);
                    q = [.. q.Skip(1).Prepend(c.Groups[2].Value).Where((x, k) => k > 0 || x != "")];
                }
                foreach (var x in q)
                    L.AddRange(Wrap([S("▌ ", hue), .. Inline(x, title is null ? "mu" : "")], W, [S("▌ ", hue)]));
            }
            else if ((m = BulletRx.Match(line)).Success || (m = NumberedRx.Match(line)).Success)
            {
                var level = Math.Min(3, m.Groups[1].Value.Replace("\t", "    ").Length / 2);
                var numbered = m.Groups.Count == 4;
                var text = m.Groups[m.Groups.Count - 1].Value;
                var task = numbered ? Match.Empty : TaskRx.Match(text);
                var done = task.Success && task.Groups[1].Value != " ";
                var mark = Rep(' ', 2 * level) + (numbered ? m.Groups[2].Value + ". " : task.Success ? done ? "☑ " : "☐ " : "•◦▪·"[level] + " ");
                L.AddRange(Wrap([S(mark, task.Success ? done ? "gr" : "mu" : "cy"), .. Inline(task.Success ? task.Groups[2].Value : text, done ? "mu" : "")],
                    W, [S(Rep(' ', mark.Length))]));
            }
            else
                L.AddRange(Wrap(Inline(line), W));
        }
        return L;
    }

    /// <summary>One line's inline spans. Code first so a backtick span is never marked up further, bold before italic.</summary>
    static Line Inline(string text, string tags = "")
    {
        Line o = [];
        var pos = 0;
        string T(string t) => (t + " " + tags).Trim();
        foreach (Match m in InlineRx.Matches(text))
        {
            var g = m.Groups;
            o.Add(S(text[pos..m.Index], tags));
            o.Add(g["code"].Success ? S(g["code"].Value, T("or pnl")) : g["b"].Success ? S(g["b"].Value, T("b"))
                : g["s"].Success ? S(g["s"].Value, T("s")) : g["i"].Success ? S(g["i"].Value, T("i"))
                : S(g["img"].Success ? "▣ " + (g["img"].Value is "" ? "image" : g["img"].Value) : g["txt"].Success ? g["txt"].Value : g["url"].Value,
                    T("cy u href:" + g["url"].Value)));
            pos = m.Index + m.Length;
        }
        return [.. o, S(text[pos..], tags)];
    }

    /// <summary>Word-wrap to width; continuation lines start with hang. A word wider than the line is cut at its edge.</summary>
    static List<Line> Wrap(Line segs, int width, Line? hang = null)
    {
        List<Line> o = [[]];
        var fresh = false; // the current line is a continuation holding only its hang
        void Break()
        {
            o[^1] = Merge(o[^1]);
            o.Add([.. hang ?? []]);
            fresh = true;
        }
        foreach (var s in segs)
            foreach (var t0 in TokenRx.Matches(s.Text).Select(m => m.Value))
            {
                var t = t0;
                if (char.IsWhiteSpace(t[0]))
                {
                    if (!fresh)
                        o[^1].Add(s with { Text = t });
                    continue;
                }
                if (!fresh && Len(o[^1]) + t.Length > width && Len(o[^1]) > Len(hang ?? []))
                    Break();
                for (int n; Len(o[^1]) + t.Length > width; t = t[n..], Break())
                    o[^1].Add(s with { Text = t[..(n = Math.Max(1, width - Len(o[^1])))] });
                o[^1].Add(s with { Text = t });
                fresh = false;
            }
        o[^1] = Merge(o[^1]);
        return o;
    }

    /// <summary>Drop trailing blanks and empty runs, and join neighbours that share tags: fewer Runs to paint.</summary>
    static Line Merge(Line l)
    {
        Line o = [];
        foreach (var s in l.Where(s => s.Text.Length > 0))
            if (o.Count > 0 && o[^1].Tags == s.Tags)
                o[^1] = s with { Text = o[^1].Text + s.Text };
            else
                o.Add(s);
        while (o.Count > 0 && o[^1].Text.TrimEnd() is var x && x.Length < o[^1].Text.Length)
            if (x.Length == 0)
                o.RemoveAt(o.Count - 1);
            else
                o[^1] = o[^1] with { Text = x };
        return o;
    }

    // --- tables ------------------------------------------------------------------

    static string[] Cells(string line)
    {
        var s = line.Trim().Replace("\\|", "\0"); // an escaped pipe belongs to its cell
        s = s.StartsWith('|') ? s[1..] : s;
        s = s.EndsWith('|') ? s[..^1] : s;
        return [.. s.Split('|').Select(c => c.Trim().Replace('\0', '|'))];
    }

    static bool IsDelim(string line) =>
        line.Contains('-') && line.All(c => c is '|' or '-' or ':' || char.IsWhiteSpace(c)) && Cells(line).All(DelimRx.IsMatch);

    /// <summary>A table at lines[i]: a header with a pipe over a delimiter row. A pipeless delimiter must match the header's width,
    /// or it is a rule under a line that happened to hold a pipe.</summary>
    static (List<string[]> Rows, string Aligns, int End)? TableAt(string[] lines, int i)
    {
        if (i + 1 >= lines.Length || !lines[i].Contains('|') || !IsDelim(lines[i + 1]))
            return null;
        var (head, delim) = (Cells(lines[i]), Cells(lines[i + 1]));
        if (!lines[i + 1].Contains('|') && delim.Length != head.Length)
            return null;
        List<string[]> rows = [head];
        var j = i + 2;
        for (; j < lines.Length && lines[j].Trim() is { Length: > 0 } s && s.Contains('|') && !s.StartsWith("```") && !IsDelim(s)
            && !HeadingRx.IsMatch(s) && !RuleRx.IsMatch(s); j++)
            rows.Add(Cells(lines[j]));
        var aligns = Enumerable.Range(0, Math.Max(delim.Length, rows.Max(r => r.Length))).Select(k => k < delim.Length && DelimRx.Match(delim[k]) is var d
            ? d.Groups[1].Value == ":" && d.Groups[2].Value == ":" ? 'c' : d.Groups[2].Value == ":" ? 'r' : 'l' : 'l');
        return (rows, new string([.. aligns]), j);
    }

    /// <summary>A boxed monospace grid fitted to the width, header bold, body zebra-striped. No cell is ever dropped.</summary>
    static List<Line> Table(List<string[]> rows, string aligns, int W)
    {
        var n = aligns.Length;
        static string Look(int y) => y == 0 ? "b" : y % 2 == 0 ? "pnl" : "";
        var cells = rows.Select((r, y) => Enumerable.Range(0, n).Select(k => Inline(k < r.Length ? r[k] : "", Look(y))).ToArray()).ToList();
        var widths = FitWidths([.. Enumerable.Range(0, n).Select(k => Math.Max(1, cells.Max(r => Len(r[k]))))], W - 4);
        Line Rule(char l, char m, char r) => [S(l + string.Join(m, widths.Select(w => Rep('─', w + 2))) + r, "rule")];
        List<Line> L = [Rule('┌', '┬', '┐')];
        for (var y = 0; y < cells.Count; y++)
        {
            var parts = Enumerable.Range(0, n).Select(k => Wrap(cells[y][k], widths[k])).ToArray();
            var tag = Look(y);
            for (var row = 0; row < parts.Max(p => p.Count); row++)
            {
                Line o = [S("│ ", "rule")];
                for (var k = 0; k < n; k++)
                {
                    var c = row < parts[k].Count ? parts[k][row] : [];
                    var gap = Math.Max(0, widths[k] - Len(c));
                    var left = aligns[k] == 'r' ? gap : aligns[k] == 'c' ? gap / 2 : 0;
                    o.AddRange([S(Rep(' ', left), tag), .. c, S(Rep(' ', gap - left), tag), S(k < n - 1 ? " │ " : " │", "rule")]);
                }
                L.Add(o);
            }
            if (y == 0)
                L.Add(Rule('├', '┼', '┤'));
        }
        L.Add(Rule('└', '┴', '┘'));
        return L;
    }

    /// <summary>Narrow columns to fit, each in proportion to its room above a 5-character floor, so the shape of the table survives.
    /// A table that cannot fit even at the floor keeps its natural widths and the pane wraps it.</summary>
    static int[] FitWidths(int[] natural, int avail)
    {
        const int Floor = 5;
        var slack = natural.Sum() + 3 * (natural.Length - 1) - avail;
        var room = natural.Sum(w => Math.Max(0, w - Floor));
        if (slack <= 0 || room < slack)
            return natural;
        var widths = natural.Select(w => w - (int)((long)slack * Math.Max(0, w - Floor) / room)).ToArray();
        while (widths.Sum() + 3 * (widths.Length - 1) > avail)
            widths[Array.IndexOf(widths, widths.Max())]--;
        return widths;
    }

    // --- fenced blocks -----------------------------------------------------------

    /// <summary>Code in a labelled box with highlighting; ```mermaid drawn as a diagram and ```chart as a text chart when they can be.</summary>
    List<Line> Fence(string lang, string[] block, int W)
    {
        if (lang == "chart")
            try
            {
                return Chart(JsonNode.Parse(string.Join('\n', block)) ?? throw new FormatException("the spec is empty"), W);
            }
            catch (Exception e) when (e is JsonException or FormatException or InvalidOperationException or ArgumentException)
            {
                lang = $"chart · {e.Message}";
            }
        if (lang == "mermaid")
        {
            var drawn = Mermaid(block, W);
            if (drawn.Count > 0 && drawn.Max(Len) <= W)
                return drawn;
            lang = drawn.Count > 0 ? "mermaid · too wide to draw here, widen the window" : "mermaid · source";
        }
        var label = $"╭─ {(lang == "" ? "code" : lang)} ";
        var code = lang.Split(' ')[0];
        List<Line> L = [[S(label + Rep('─', W - 1 - label.Length), "rule pnl")],
            .. block.SelectMany(raw => Wrap([S("│ ", "rule pnl"), .. Highlight(raw, code).Select(s => s with { Tags = (s.Tags + " pnl").Trim() })], W, [S("│ ", "rule pnl")])),
            [S("╰" + Rep('─', W - 2), "rule pnl")]];
        return [.. L.Select(l => Pad(l, W, "pnl"))];
    }

    /// <summary>Generic, forgiving highlighting: comments, strings, numbers, variables, keywords and calls.</summary>
    static Line Highlight(string line, string lang)
    {
        if (lang is "text" or "txt" or "log" or "output")
            return [S(line)];
        if (line.Length > 2000) // the token pattern backtracks on long quote runs
            return [.. Highlight(line[..2000], lang), S(line[2000..])];
        Line o = [];
        var pos = 0;
        foreach (Match m in CodeRx.Matches(line))
        {
            o.Add(S(line[pos..m.Index]));
            pos = m.Index + m.Length;
            var hue = new[] { "fa", "ye", "pu", "or" }.FirstOrDefault(g => m.Groups[g].Success);
            if (hue == "fa" && m.Value[0] == '#' && lang is "c" or "cpp" or "cs" or "csharp" or "js" or "ts" or "javascript" or "typescript" or "java" or "go" or "rust")
                o.AddRange([S("#"), .. Highlight(m.Value[1..], lang)]); // #include, #region: a directive, not a comment
            else
                o.Add(S(m.Value, hue ?? (Keywords.Contains(m.Value) || m.Value.ToLowerInvariant() is "select" or "from" or "where" ? "pk"
                    : pos < line.Length && line[pos] == '(' ? "gr" : "")));
        }
        return [.. o, S(line[pos..])];
    }

    /// <summary>A sparkline in block characters and its range, at most <paramref name="width"/> wide (evenly sampled, first and last kept).</summary>
    internal static Seg[] Spark(IReadOnlyList<double> vals, string hue, string unit = "", int width = int.MaxValue)
    {
        static string F(double v) => v.ToString("#,0.#", CultureInfo.CurrentCulture);
        width = Math.Max(2, width);
        var shown = vals.Count <= width ? vals : [.. Enumerable.Range(0, width).Select(i => vals[(int)((long)i * (vals.Count - 1) / (width - 1))])];
        var (lo, hi) = (vals.Min(), vals.Max());
        return [S(string.Concat(shown.Select(v => "▁▂▃▄▅▆▇█"[hi == lo ? 3 : (int)((v - lo) / (hi - lo) * 7)])), hue),
            S($"  {F(lo)}–{F(hi)}{unit}, last {F(vals[^1])}{unit}", "mu")];
    }

    /// <summary>A ```chart spec (the Tk app's charts.py types) drawn in text: bars and sparklines in block characters.</summary>
    List<Line> Chart(JsonNode spec, int W)
    {
        static string Str(JsonNode? n) => n?.ToString() ?? "";
        static double Num(JsonNode? n) => n?.GetValue<double>() ?? 0;
        static string F(double v) => v.ToString("#,0.#", CultureInfo.CurrentCulture);
        static JsonArray Arr(JsonNode? n) => n as JsonArray ?? [];
        static bool On(JsonNode? n) => Str(n) is not ("" or "false" or "0");
        var unit = Str(spec["unit"]);
        var u = unit is "" or "%" ? unit : " " + unit; // 94%, but 29 min
        List<Line> L = spec["title"] is { } title ? [[S(Str(title), "ye b"), .. If(unit != "", S($"  in {unit}", "mu"))]] : [];
        List<Line> Bars(IEnumerable<(string Label, double V, double Max, string Note)> items, bool colorful)
        {
            var list = items.ToList();
            var labw = Math.Min(24, list.Max(x => x.Label.Length));
            var barw = Math.Max(10, Math.Min(40, W - labw - 22));
            return [.. list.Select(Line (x, k) => [S(Fit(x.Label, labw) + "  "), S(Rep('█', (int)Math.Round(barw * Math.Clamp(x.V / x.Max, 0, 1))), colorful ? Hues[k % 6] : "cy"),
                S(Rep('░', barw - (int)Math.Round(barw * Math.Clamp(x.V / x.Max, 0, 1))), "rule"), S("  " + x.Note, "mu")])];
        }
        switch (Str(spec["type"]))
        {
            case "bar":
                var values = Arr(spec["values"]).Select(Num).ToList();
                L.AddRange(Bars(Arr(spec["labels"]).Select((l, k) => (Str(l), values[k], values.Max(), F(values[k]) + u)), On(spec["colorful"])));
                break;
            case "progress":
                L.AddRange(Bars(Arr(spec["items"]).Select(it => (Str(it?["label"]), Num(it?["done"] ?? it?["value"]), it?["total"] is { } t ? Num(t) : 100,
                    $"{F(Num(it?["done"] ?? it?["value"]))}/{F(it?["total"] is { } t2 ? Num(t2) : 100)}")), false));
                break;
            case "line" or "area" or "burndown" or "sparkline":
                List<(string Name, List<double> Vals)> series = Str(spec["type"]) switch
                {
                    "burndown" => [("remaining", Arr(spec["remaining"]).Select(Num).ToList())],
                    "sparkline" => [("", Arr(spec["values"]).Where(v => v is not null).Select(Num).ToList())],
                    _ => (spec["series"] as JsonObject ?? []).Select(kv => (kv.Key, Arr(kv.Value).Select(Num).ToList())).ToList(),
                };
                var labw = series.Max(x => x.Name.Length);
                foreach (var ((name, vals), k) in series.Select((x, k) => (x, k)).Where(x => x.x.Vals.Count > 0))
                {
                    L.Add([.. If(labw > 0, S(name.PadRight(labw) + "  ")), .. Spark(vals, Hues[k % 6], u)]);
                }
                var x = Arr(spec["x"]);
                if (x.Count > 1)
                    L.Add([S(Rep(' ', labw > 0 ? labw + 2 : 0) + $"{Str(x[0])} → {Str(x[^1])}", "fa")]);
                if (spec["goal"] is { } goal)
                    L.Add([S($"goal {F(Num(goal))}{u}", "ye")]);
                break;
            case "timeline":
                L.AddRange(Arr(spec["events"]).Select(Line (e) => [S($"  {Str(e?["when"]),-8} ", "mu"),
                    S("● ", On(e?["done"]) ? "gr" : On(e?["now"]) ? "ye" : "cy"), S(Str(e?["label"]), On(e?["now"]) ? "fg b" : "fg")]));
                break;
            case "stat":
                L.Add([.. Arr(spec["tiles"]).SelectMany(t => new[] { S("  " + Str(t?["value"]), "fg b"), S(" " + Str(t?["label"]), "mu"),
                    S(" " + Str(t?["delta"]), Str(t?["delta"]).StartsWith('+') != On(t?["lower_is_better"]) ? "gr" : "pk") })]);
                break;
            case "table":
                var columns = Arr(spec["columns"]).Select(Str).ToArray();
                var align = Arr(spec["align"]);
                L.AddRange(Table([columns, .. Arr(spec["rows"]).Select(r => Arr(r).Select(Str).ToArray())],
                    new string([.. columns.Select((_, k) => k < align.Count && Str(align[k]) is { Length: > 0 } a ? a[0] : 'l')]), W));
                break;
            default:
                throw new FormatException("\"type\" must be one of: line, area, bar, progress, burndown, timeline, sparkline, stat, table");
        }
        return L;
    }
}
