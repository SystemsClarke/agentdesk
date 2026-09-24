using System.Globalization;
using System.Text.RegularExpressions;

namespace AgentDesk.App;

/// <summary>```mermaid drawn as box-drawing text, as the Tk app's mdrich did: flowcharts and state diagrams laid out by level,
/// sequence diagrams with lifelines, and pie charts as bars. Anything else, or anything that fails to parse, shows as source.</summary>
public partial class MainWindow
{
    static readonly Regex NodeRx = new(@"^\s*([A-Za-z0-9_\-\.\*\[\]]+?)\s*(\(\(.*?\)\)|\(\[.*?\]\)|\[\[.*?\]\]|\[\(.*?\)\]|\{\{.*?\}\}|\[.*?\]|\(.*?\)|\{.*?\}|>.*?\])?\s*$"),
        // Labelled arrows first: otherwise `A -- label --> B` splits at the leading `--`.
        ArrowRx = new(@"(\s*(?:--\s+[^->|]+?\s+-->|==\s+[^=>|]+?\s+==>|<?-\.+->?|<?-{2,}[>xo]?|<?={2,}>?)\s*(?:\|[^|]*\|)?\s*)"),
        MsgRx = new(@"^\s*([^\-\s>]+?)\s*(-{1,2}>>|-{1,2}>|-{1,2}x|-{1,2}\))\s*([+\-]?)([^:]+?)\s*:\s*(.*)$");
    static readonly (string Open, string Close, string Shape)[] Shapes =
        [("((", "))", "round"), ("([", "])", "round"), ("[[", "]]", "double"), ("[(", ")]", "round"), ("{{", "}}", "diamond"), ("[", "]", "rect"),
         ("(", ")", "round"), ("{", "}", "diamond"), (">", "]", "rect")];

    sealed class Node
    {
        public string Label = "", Shape = "rect";
        public List<string> Lines = [];
        public int X, Y, W, H;
    }

    static List<Line> Mermaid(string[] block, int maxWidth)
    {
        var src = block.Where(l => l.Trim().Length > 0).ToList();
        if (src.Count == 0)
            return [];
        var kind = src[0].Trim().Split(' ')[0].ToLowerInvariant();
        try
        {
            return kind is "graph" or "flowchart" || kind.StartsWith("statediagram") ? DrawFlow(src)
                : kind == "sequencediagram" ? DrawSequence(src) : kind == "pie" ? DrawPie(src, maxWidth) : [];
        }
        catch (Exception e) when (e is ArgumentException or InvalidOperationException or IndexOutOfRangeException or KeyNotFoundException or FormatException)
        {
            return [];
        }
    }

    static List<string> WrapLabel(string label, int width)
    {
        List<string> o = [];
        foreach (var w0 in label.Split(' ', StringSplitOptions.RemoveEmptyEntries))
        {
            var w = w0.Length > width ? w0[..(width - 1)] + "…" : w0;
            if (o.Count > 0 && o[^1].Length + 1 + w.Length <= width)
                o[^1] += " " + w;
            else
                o.Add(w);
        }
        return o.Count > 0 ? o : [""];
    }

    static string? ParseNode(string tok, Dictionary<string, Node> nodes, List<string> order)
    {
        tok = tok.Trim();
        if (tok == "")
            return null;
        var m = NodeRx.Match(tok);
        var (id, part) = m.Success ? (m.Groups[1].Value, m.Groups[2].Value) : (tok, "");
        string? label = null;
        var shape = "rect";
        if (part != "" && Shapes.FirstOrDefault(s => part.StartsWith(s.Open) && part.EndsWith(s.Close)) is { Open: not null } sh)
            (label, shape) = (part[sh.Open.Length..^sh.Close.Length].Trim().Trim('"'), sh.Shape);
        if (!nodes.TryGetValue(id, out var node))
        {
            var star = id == "[*]" ? "●" : id == "[*]end" ? "◉" : null;
            nodes[id] = new Node { Label = label ?? star ?? id, Shape = star != null ? "round" : shape };
            order.Add(id);
        }
        else if (label != null)
            (node.Label, node.Shape) = (label, shape);
        return id;
    }

    static List<Line> DrawFlow(List<string> src)
    {
        var head = src[0].Trim().Split(' ', StringSplitOptions.RemoveEmptyEntries);
        var state = head[0].ToLowerInvariant().StartsWith("state");
        var dir = state ? "TD" : head.Length > 1 ? head[1].ToUpperInvariant() : "TD";
        foreach (var l in src.Skip(1).Where(l => l.Trim().StartsWith("direction ", StringComparison.OrdinalIgnoreCase)))
            dir = l.Trim().Split(' ', StringSplitOptions.RemoveEmptyEntries)[1].ToUpperInvariant();
        Dictionary<string, Node> nodes = [];
        List<string> order = [];
        List<(string A, string B, string? Label, bool Dashed)> edges = [];
        string[] skip = ["subgraph", "end", "style ", "classdef", "class ", "click ", "linkstyle", "direction ", "note ", "state "];
        foreach (var raw in src.Skip(1))
        {
            var s = raw.Trim().TrimEnd(';');
            if (s == "" || s.StartsWith("%%") || skip.Any(k => s.StartsWith(k, StringComparison.OrdinalIgnoreCase)))
                continue;
            string? after = null;
            if (state && s.Contains(':') && s.Contains("-->"))
                (s, after) = (s[..s.IndexOf(':')], s[(s.IndexOf(':') + 1)..].Trim());
            var parts = ArrowRx.Split(s);
            if (parts.Length == 1)
            {
                ParseNode(parts[0], nodes, order);
                continue;
            }
            var prev = parts[0].Split('&').Select(p => ParseNode(p, nodes, order)).ToList();
            for (var i = 1; i + 1 < parts.Length; i += 2)
            {
                var (arrow, target) = (parts[i], parts[i + 1]);
                var lbl = Regex.Match(arrow, @"\|([^|]*)\|") is { Success: true } lm ? lm.Groups[1].Value.Trim() : null;
                lbl = Regex.Match(arrow, @"^\s*(?:--|==)\s+(.+?)\s+(?:-->|==>)") is { Success: true } lm2 ? lm2.Groups[1].Value.Trim() : lbl;
                lbl = after != null && i + 2 >= parts.Length ? after : lbl;
                if (state && target.Trim() == "[*]")
                    target = "[*]end";
                var cur = target.Split('&').Select(t => ParseNode(t, nodes, order)).ToList();
                edges.AddRange(from a in prev from b in cur where a != null && b != null select (a, b, lbl, arrow.Contains('.')));
                prev = cur;
            }
        }
        if (nodes.Count == 0)
            return [];

        // Levels by longest path from the roots; an edge back to a node on the stack is a loop, listed under the drawing.
        var succ = order.ToDictionary(n => n, n => edges.Where(e => e.A == n).Select(e => e.B).ToList());
        var level = order.ToDictionary(n => n, _ => 0);
        HashSet<(string, string)> back = [];
        Dictionary<string, int> seen = [];
        void Dfs(string n)
        {
            seen[n] = 1;
            foreach (var m in succ[n])
                if (seen.GetValueOrDefault(m) == 1)
                    back.Add((n, m));
                else if (level[m] < level[n] + 1)
                {
                    level[m] = level[n] + 1;
                    Dfs(m);
                }
                else if (seen.GetValueOrDefault(m) != 2)
                    Dfs(m);
            seen[n] = 2;
        }
        foreach (var n in order.Where(n => !edges.Any(e => e.B == n)).Concat(order))
            if (!seen.ContainsKey(n))
                Dfs(n);

        var horizontal = dir is "LR" or "RL";
        foreach (var nd in nodes.Values)
        {
            nd.Lines = WrapLabel(nd.Label, horizontal ? 18 : 22);
            (nd.W, nd.H) = (nd.Lines.Max(l => l.Length) + 4, nd.Lines.Count + 2);
        }
        var depth = level.Values.Max() + 1;
        var rows = Enumerable.Range(0, depth).Select(lv => order.Where(n => level[n] == lv).ToList()).ToList();
        Dictionary<string, double> at = [];
        for (var lv = 0; lv < depth; lv++)
        {
            double Bary(string n) => edges.Where(e => e.B == n && at.ContainsKey(e.A) && level[e.A] < lv).Select(e => at[e.A]).DefaultIfEmpty(1e9).Average();
            if (lv > 0) // one barycentre pass so children sit under their parents and edges cross less
                rows[lv] = [.. rows[lv].OrderBy(Bary)];
            for (var i = 0; i < rows[lv].Count; i++)
                at[rows[lv][i]] = i;
        }
        Canvas cv;
        if (!horizontal)
        {
            var widths = rows.Select(r => r.Sum(n => nodes[n].W) + 4 * (r.Count - 1)).ToList();
            var total = widths.Max() + 2;
            var y = 0;
            foreach (var (row, lv) in rows.Select((r, lv) => (r, lv)))
            {
                var x = (total - widths[lv]) / 2;
                var h = row.Count > 0 ? row.Max(n => nodes[n].H) : 0;
                foreach (var nd in row.Select(n => nodes[n]))
                {
                    (nd.X, nd.Y) = (x, y + (h - nd.H) / 2);
                    x += nd.W + 4;
                }
                y += h + 4;
            }
            cv = new Canvas(Math.Max(total, 10) + 24, y);
        }
        else
        {
            var heights = rows.Select(r => r.Sum(n => nodes[n].H) + (r.Count - 1)).ToList();
            var total = heights.Max();
            var x = 0;
            foreach (var (row, lv) in rows.Select((r, lv) => (r, lv)))
            {
                var y = (total - heights[lv]) / 2;
                var w = row.Count > 0 ? row.Max(n => nodes[n].W) : 0;
                foreach (var nd in row.Select(n => nodes[n]))
                {
                    (nd.X, nd.Y) = (x + (w - nd.W) / 2, y);
                    y += nd.H + 1;
                }
                x += w + 8;
            }
            cv = new Canvas(x + 2, total + 1);
        }
        List<(string A, string B, string? Label)> loops = [];
        foreach (var (a, b, lbl, dashed) in edges)
        {
            if (back.Contains((a, b)) || level[b] <= level[a])
            {
                loops.Add((a, b, lbl));
                continue;
            }
            var (na, nb) = (nodes[a], nodes[b]);
            if (!horizontal)
            {
                var (sx, sy, tx, ty) = (na.X + na.W / 2, na.Y + na.H, nb.X + nb.W / 2, nb.Y - 1);
                cv.VLine(sx, sy, sy + 1, "rule", dashed);
                cv.HLine(sx, tx, sy + 1, "rule", dashed);
                cv.VLine(tx, sy + 1, ty, "rule", dashed);
                cv.Put(tx, ty, '▼', "cy");
                if (lbl != null)
                    cv.Text(tx + 2, ty - 1 > sy + 1 ? ty - 1 : ty, lbl[..Math.Min(24, lbl.Length)], "pu", over: false);
            }
            else
            {
                var (sx, sy, tx, ty) = (na.X + na.W, na.Y + na.H / 2, nb.X - 1, nb.Y + nb.H / 2);
                cv.HLine(sx, sx + 2, sy, "rule", dashed);
                cv.VLine(sx + 2, sy, ty, "rule", dashed);
                cv.HLine(sx + 2, tx, ty, "rule", dashed);
                cv.Put(tx, ty, '►', "cy");
                if (lbl != null)
                    cv.Text(sx + 3, ty - 1, lbl[..Math.Min(Math.Max(4, tx - sx - 3), lbl.Length)], "pu", over: false);
            }
        }
        foreach (var n in order)
            cv.Box(nodes[n].X, nodes[n].Y, nodes[n].W, nodes[n].Lines, nodes[n].Shape, nodes[n].Shape == "diamond" ? "pk" : Hues[level[n] % 6]);
        return [.. cv.Render(), .. loops.Select(Line (l) => [S("  ↺ ", "or"), S(nodes[l.A].Label, "fg"), S(" loops back to ", "mu"), S(nodes[l.B].Label, "fg"),
            .. If(l.Label != null, S($"  ({l.Label})", "pu"))])];
    }

    static List<Line> DrawSequence(List<string> src)
    {
        List<string> parts = [];
        Dictionary<string, string> alias = [];
        List<string[]> events = []; // msg a b text dashed | note who text | frame kind text
        void Part(string p)
        {
            if (!parts.Contains(p))
                parts.Add(p);
        }
        foreach (var s in src.Skip(1).Select(l => l.Trim()).Where(s => s != "" && !s.StartsWith("%%")))
        {
            Match m;
            if ((m = Regex.Match(s, @"^(participant|actor)\s+(\S+)(?:\s+as\s+(.+))?$", RegexOptions.IgnoreCase)).Success)
            {
                alias[m.Groups[2].Value] = (m.Groups[3].Success ? m.Groups[3].Value : m.Groups[2].Value).Trim();
                Part(m.Groups[2].Value);
            }
            else if ((m = MsgRx.Match(s)).Success)
            {
                var (a, b) = (m.Groups[1].Value, m.Groups[4].Value.Trim());
                Part(a);
                Part(b);
                events.Add(["msg", a, b, m.Groups[5].Value, m.Groups[2].Value.StartsWith("--") ? "1" : ""]);
            }
            else if ((m = Regex.Match(s, @"^note\s+(over|left of|right of)\s+([^:]+):\s*(.*)$", RegexOptions.IgnoreCase)).Success)
                events.Add(["note", m.Groups[2].Value.Split(',')[0].Trim(), m.Groups[3].Value]);
            else if ((m = Regex.Match(s, @"^(loop|alt|else|opt|par|and|critical|break|rect)\b\s*(.*)$", RegexOptions.IgnoreCase)).Success)
                events.Add(["frame", m.Groups[1].Value.ToLowerInvariant(), m.Groups[2].Value]);
            else if (s.Equals("end", StringComparison.OrdinalIgnoreCase))
                events.Add(["frame", "end", ""]);
        }
        if (parts.Count == 0)
            return [];
        var names = parts.Select(p => alias.GetValueOrDefault(p, p)).ToList();
        var widths = names.Select(n => n.Length + 4).ToList();
        var gaps = Enumerable.Repeat(12, parts.Count).ToArray();
        foreach (var e in events.Where(e => e[0] == "msg"))
        {
            var (i, j) = (Math.Min(parts.IndexOf(e[1]), parts.IndexOf(e[2])), Math.Max(parts.IndexOf(e[1]), parts.IndexOf(e[2])));
            if (j == i + 1)
                gaps[i] = Math.Max(gaps[i], e[3].Length + 6 - (widths[i] + widths[j]) / 2);
        }
        List<int> centers = [];
        var x = 2;
        for (var i = 0; i < widths.Count; i++)
        {
            centers.Add(x + widths[i] / 2);
            x += widths[i] + (i < widths.Count - 1 ? gaps[i] : 0);
        }
        var cv = new Canvas(x + 30, 4 + events.Sum(e => e[0] == "msg" ? 2 : 1));
        for (var i = 0; i < names.Count; i++)
            cv.Box(centers[i] - widths[i] / 2, 0, widths[i], [names[i]], "round", "cy");
        var y = 3;
        foreach (var e in events)
        {
            if (e[0] == "msg" && e[1] == e[2])
                cv.Text(centers[parts.IndexOf(e[1])] + 2, y++, $"↺ {e[3]}", "pu");
            else if (e[0] == "msg")
            {
                var (x1, x2) = (centers[parts.IndexOf(e[1])], centers[parts.IndexOf(e[2])]);
                var (lo, hi) = (Math.Min(x1, x2), Math.Max(x1, x2));
                cv.Text(lo + 2, y, e[3][..Math.Min(e[3].Length, Math.Max(3, hi - lo - 3))], "fg");
                cv.HLine(lo + 1, hi - 1, y + 1, "ye", e[4] != "");
                cv.Put(x2 > x1 ? x2 - 1 : x2 + 1, y + 1, x2 > x1 ? '►' : '◄', "ye");
            }
            else if (e[0] == "note")
                cv.Text(centers[Math.Max(0, parts.IndexOf(e[1]))] + 2, y, $"▌ {e[2]}", "or");
            else
                cv.Text(1, y, $"┆ {e[1]} {e[2]}".TrimEnd(), "pu");
            y += e[0] == "msg" ? e[1] == e[2] ? 1 : 2 : 1;
        }
        foreach (var c in centers)
            cv.VLine(c, 3, y, "rule", dashed: true);
        return cv.Render();
    }

    static List<Line> DrawPie(List<string> src, int maxWidth)
    {
        var title = Regex.Replace(src[0].Trim(), @"^pie\s*(title\s*)?", "", RegexOptions.IgnoreCase).Trim();
        List<(string K, double V)> items = [];
        foreach (var s in src.Skip(1).Select(l => l.Trim()))
            if (s.StartsWith("title", StringComparison.OrdinalIgnoreCase))
                title = s[5..].Trim();
            else if (Regex.Match(s, @"^""?(.+?)""?\s*:\s*([\d.]+)\s*$") is { Success: true } m)
                items.Add((m.Groups[1].Value, double.Parse(m.Groups[2].Value, CultureInfo.InvariantCulture)));
        if (items.Count == 0)
            return [];
        var total = items.Sum(i => i.V) is var t && t > 0 ? t : 1;
        var labw = Math.Min(24, items.Max(i => i.K.Length));
        var barw = Math.Max(10, Math.Min(40, maxWidth - labw - 16));
        return [.. If(title != "", S(title, "ye")).Select(Line (s) => [s]), .. items.Select(Line (it, i) => [S(Fit(it.K, labw) + "  ", "fg"),
            S(Rep('█', (int)Math.Round(barw * it.V / total)), Hues[i % 6]), S(Rep('░', barw - (int)Math.Round(barw * it.V / total)), "rule"),
            S($"  {it.V:g}  {100 * it.V / total:0}%", "mu")])];
    }

    /// <summary>A character grid. Lines record which directions meet in each cell, so corners and junctions resolve to the right glyph.</summary>
    sealed class Canvas
    {
        const int U = 1, D = 2, L = 4, R = 8;
        const string Glyphs = " │││─┘┐┤─└┌├─┴┬┼"; // indexed by the U|D|L|R mask
        readonly int w, h;
        readonly char[,] ch;
        readonly string?[,] col;
        readonly int[,] mask;
        readonly bool[,] dash;

        public Canvas(int w, int h)
        {
            (this.w, this.h) = (w, h);
            (ch, col, mask, dash) = (new char[h, w], new string?[h, w], new int[h, w], new bool[h, w]);
            for (var y = 0; y < h; y++)
                for (var x = 0; x < w; x++)
                    ch[y, x] = ' ';
        }

        bool In(int x, int y) => x >= 0 && y >= 0 && x < w && y < h;

        public void Put(int x, int y, char c, string? color, bool over = true)
        {
            if (In(x, y) && (over || ch[y, x] == ' ' && mask[y, x] == 0))
                (ch[y, x], col[y, x], mask[y, x]) = (c, color, 0);
        }

        public void Text(int x, int y, string s, string? color, bool over = true)
        {
            for (var i = 0; i < s.Length; i++)
                Put(x + i, y, s[i], color, over);
        }

        void Mark(int x, int y, int bits, string? color, bool dashed)
        {
            if (In(x, y) && ch[y, x] == ' ')
                (mask[y, x], col[y, x], dash[y, x]) = (mask[y, x] | bits, color, dash[y, x] || dashed);
        }

        public void HLine(int x1, int x2, int y, string? color, bool dashed = false)
        {
            var (lo, hi) = (Math.Min(x1, x2), Math.Max(x1, x2));
            for (var x = lo; lo < hi && x <= hi; x++) // a zero-length hop draws nothing, or straight edges get a stray ┼
                Mark(x, y, (x > lo ? L : 0) | (x < hi ? R : 0), color, dashed);
        }

        public void VLine(int x, int y1, int y2, string? color, bool dashed = false)
        {
            var (lo, hi) = (Math.Min(y1, y2), Math.Max(y1, y2));
            for (var y = lo; lo < hi && y <= hi; y++)
                Mark(x, y, (y > lo ? U : 0) | (y < hi ? D : 0), color, dashed);
        }

        public void Box(int x, int y, int bw, List<string> lines, string shape, string color)
        {
            var bh = lines.Count + 2;
            var c = shape switch { "round" => "╭╮╰╯", "diamond" => "◆◆◆◆", "double" => "╔╗╚╝", _ => "┌┐└┘" };
            var (hz, vt) = shape == "double" ? ('═', '║') : ('─', '│');
            Put(x, y, c[0], color);
            Put(x + bw - 1, y, c[1], color);
            Put(x, y + bh - 1, c[2], color);
            Put(x + bw - 1, y + bh - 1, c[3], color);
            for (var i = 1; i < bw - 1; i++)
            {
                Put(x + i, y, hz, color);
                Put(x + i, y + bh - 1, hz, color);
            }
            for (var j = 1; j < bh - 1; j++)
            {
                Put(x, y + j, vt, color);
                Put(x + bw - 1, y + j, vt, color);
                Text(x + 1, y + j, new string(' ', bw - 2), null);
                Text(x + 1 + (bw - 2 - lines[j - 1].Length) / 2, y + j, lines[j - 1], "fg");
            }
        }

        public List<Line> Render()
        {
            List<Line> o = [];
            for (var y = 0; y < h; y++)
            {
                Line row = [];
                for (var x = 0; x < w; x++)
                {
                    var c = mask[y, x] != 0 && ch[y, x] == ' ' ? Glyphs[mask[y, x]] : ch[y, x];
                    c = mask[y, x] != 0 && dash[y, x] ? c switch { '│' => '┆', '─' => '┄', _ => c } : c;
                    row.Add(S(c.ToString(), col[y, x] ?? ""));
                }
                o.Add(Merge(row));
            }
            while (o.Count > 0 && o[^1].Count == 0)
                o.RemoveAt(o.Count - 1);
            return o;
        }
    }
}
