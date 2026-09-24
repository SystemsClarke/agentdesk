using System.Globalization;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace AgentDesk.Core.Board;

/// <summary>Python's text rules where they leak into stored data or messages: json.dumps(ensure_ascii=False),
/// repr(str), float repr, str.isspace/split/strip, and negative-index slicing.</summary>
static class Py
{
    public static string? Dumps(JsonNode? n) => n is null ? null : Write(new StringBuilder(), n).ToString();

    static StringBuilder Write(StringBuilder sb, JsonNode? n)
    {
        int i = 0;
        switch (n)
        {
            case null: return sb.Append("null");
            case JsonObject o:
                sb.Append('{');
                foreach (var (k, v) in o) Write(Str(sb.Append(i++ > 0 ? ", " : ""), k).Append(": "), v);
                return sb.Append('}');
            case JsonArray a:
                sb.Append('[');
                foreach (var v in a) Write(sb.Append(i++ > 0 ? ", " : ""), v);
                return sb.Append(']');
            default:
                var raw = n.ToJsonString();
                return n.GetValueKind() switch
                {
                    JsonValueKind.String => Str(sb, n.GetValue<string>()),
                    JsonValueKind.Number => sb.Append(raw.TrimStart('-').All(char.IsAsciiDigit) ? raw : Float(double.Parse(raw, CultureInfo.InvariantCulture))),
                    _ => sb.Append(raw),
                };
        }
    }

    static StringBuilder Str(StringBuilder sb, string s)
    {
        sb.Append('"');
        foreach (var c in s)
            sb.Append(c switch
            {
                '"' => "\\\"", '\\' => "\\\\", '\n' => "\\n", '\r' => "\\r", '\t' => "\\t", '\b' => "\\b", '\f' => "\\f",
                < ' ' => $"\\u{(int)c:x4}",
                _ => c.ToString(),
            });
        return sb.Append('"');
    }

    /// <summary>repr(float): shortest round-trip digits, fixed notation for exponents -4..15, else d.ddde+XX.</summary>
    public static string Float(double d)
    {
        if (double.IsNaN(d)) return "NaN";
        if (double.IsInfinity(d)) return d > 0 ? "Infinity" : "-Infinity";
        var s = d.ToString("R", CultureInfo.InvariantCulture);
        var sign = s.StartsWith('-') ? "-" : "";
        s = s.TrimStart('-');
        int ei = s.IndexOf('E'), e = ei >= 0 ? int.Parse(s[(ei + 1)..], CultureInfo.InvariantCulture) : 0;
        if (ei >= 0) s = s[..ei];
        var digits = s.Replace(".", "");
        int point = (s.IndexOf('.') is var dot and >= 0 ? dot : s.Length) + e;
        int lead = digits.Length - digits.TrimStart('0').Length;
        digits = digits.Trim('0');
        point -= lead;
        if (digits.Length == 0) return sign + "0.0";
        int exp = point - 1;
        if (exp is >= -4 and < 16)
            return sign + (point <= 0 ? "0." + new string('0', -point) + digits
                : point >= digits.Length ? digits + new string('0', point - digits.Length) + ".0"
                : digits[..point] + "." + digits[point..]);
        return $"{sign}{(digits.Length == 1 ? digits : digits[0] + "." + digits[1..])}e{(exp < 0 ? '-' : '+')}{Math.Abs(exp):00}";
    }

    public static string Repr(string? s)
    {
        if (s is null) return "None";
        char q = s.Contains('\'') && !s.Contains('"') ? '"' : '\'';
        var sb = new StringBuilder().Append(q);
        foreach (var r in s.EnumerateRunes())
        {
            int c = r.Value;
            sb.Append(c == q || c == '\\' ? "\\" + (char)c : c switch
            {
                '\n' => "\\n", '\r' => "\\r", '\t' => "\\t",
                _ when c != ' ' && Rune.GetUnicodeCategory(r) is UnicodeCategory.Control or UnicodeCategory.Format
                    or UnicodeCategory.Surrogate or UnicodeCategory.PrivateUse or UnicodeCategory.OtherNotAssigned
                    or UnicodeCategory.LineSeparator or UnicodeCategory.ParagraphSeparator or UnicodeCategory.SpaceSeparator
                    => c < 0x100 ? $"\\x{c:x2}" : c < 0x10000 ? $"\\u{c:x4}" : $"\\U{c:x8}",
                _ => r.ToString(),
            });
        }
        return sb.Append(q).ToString();
    }

    public static bool IsSpace(char c) => char.IsWhiteSpace(c) || c is >= '\x1c' and <= '\x1f';

    public static string Strip(string? s)
    {
        s ??= "";
        int a = 0, b = s.Length;
        while (a < b && IsSpace(s[a])) a++;
        while (b > a && IsSpace(s[b - 1])) b--;
        return s[a..b];
    }

    public static int Words(string? s) => (s ?? "").Select((c, i) => !IsSpace(c) && (i == 0 || IsSpace(s![i - 1]))).Count(x => x);

    /// <summary>xs[:n], including Python's meaning for a negative n.</summary>
    public static List<T> Head<T>(IList<T> xs, int n) => xs.Take(n >= 0 ? n : Math.Max(0, xs.Count + n)).ToList();
}
