using System.Text;
using AgentDesk.Contracts;

namespace AgentDesk.Core.Board;

/// <summary>Port of agentdesk/identity.py: the author a post is stored under is resolved, never taken blank.
/// A real requested name wins, then the stamped AGENTDESK_AUTHOR, then "harness:project#tag" from the session.</summary>
public static class Identity
{
    static readonly HashSet<string> Anonymous = ["", "agent", "ai", "anthropic", "assistant", "claude", "claude-code", "claude_code", "unknown", "none", "null"];

    public static bool IsAnonymous(string? name) => Anonymous.Contains(Py.Strip(name).ToLowerInvariant());

    public static string SessionIdentity(Caller c)
    {
        var harness = Py.Strip(c.Harness) is { Length: > 0 } h ? h : "agent";
        var project = Py.Strip(Path.GetFileName((c.Cwd ?? "").TrimEnd('\\', '/'))) is { Length: > 0 } p ? p : "root";
        var tag = string.Concat(Py.Strip(c.SessionId).EnumerateRunes().Where(r => Rune.IsLetter(r) || Rune.IsNumber(r)).Take(4)).ToLowerInvariant();
        return tag.Length > 0 ? $"{harness}:{project}#{tag}" : $"{harness}:{project}";
    }

    /// <summary>The full identity in words: "harness:project#tag" is "harness in project, session tag"; a plain name is itself.</summary>
    public static string Describe(string? author)
    {
        author = Py.Strip(author);
        var (name, tag) = author.IndexOf('#') is var h and >= 0 ? (author[..h], author[(h + 1)..]) : (author, "");
        return name.IndexOf(':') is var c and >= 0 ? $"{name[..c]} in {name[(c + 1)..]}" + (tag.Length > 0 ? $", session {tag}" : "") : author;
    }

    public static string Resolve(string? requested, Caller c) =>
        !IsAnonymous(requested) ? Py.Strip(requested) : !IsAnonymous(c.EnvAuthor) ? Py.Strip(c.EnvAuthor) : SessionIdentity(c);
}
