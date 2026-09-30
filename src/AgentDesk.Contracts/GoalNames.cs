using System.Text.RegularExpressions;

namespace AgentDesk.Contracts;

public static class GoalNames
{
    /// <summary>What John typed as a goal name, made into one: spaces and punctuation become dashes ("Add GoCD Health" is Add-GoCD-Health),
    /// a leading digit gets "g-" in front, and it is cut to 32. Without this New goal answered "Not done" for any name with a space
    /// in it, briefly, and the goal never appeared.</summary>
    public static string Slug(string? name)
    {
        var s = Regex.Replace(name ?? "", "[^A-Za-z0-9]+", "-").Trim('-');
        if (s.Length > 0 && char.IsAsciiDigit(s[0])) s = "g-" + s;
        return s.Length > 32 ? s[..32].TrimEnd('-') : s;
    }
}
