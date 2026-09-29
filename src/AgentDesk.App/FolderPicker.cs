using System.IO;
using System.Windows.Input;

namespace AgentDesk.App;

/// <summary>The folder step of New agent and New goal: the folders John works in, most likely first, filtered as he types
/// (or dictates). ↑↓ picks, Tab copies the pick into the box to edit, Enter takes a typed path that exists, else the pick,
/// and a lone digit takes that row.</summary>
public partial class MainWindow
{
    const int FolderRows = 9;
    internal IReadOnlyList<FolderPick> folderPicks = [];
    internal int folderSel;

    internal bool OnFolderStep => screen == "ask" && ask is { } a && answers.Count < a.Fields.Length && a.Fields[answers.Count].Label == "folder";

    async Task LoadFolders()
    {
        try
        {
            folderPicks = await board.FoldersAsync();
        }
        catch (Exception e) when (e is InvalidOperationException or IOException)
        {
            folderPicks = []; // no suggestions is the old dialog: type the path
        }
        folderSel = 0;
        if (OnFolderStep)
            Render();
    }

    static string Norm(string s) => new([.. s.Where(char.IsLetterOrDigit).Select(char.ToLowerInvariant)]);

    /// <summary>Picks whose path or people contain what was typed, ignoring case, spaces and punctuation, so "agent desk" finds AgentDesk.</summary>
    internal static IReadOnlyList<FolderPick> FilterFolders(IReadOnlyList<FolderPick> picks, string query)
    {
        var (q, digit) = (Norm(query), query.Trim() is [var c] && char.IsAsciiDigit(c)); // a lone digit picks a row, it does not filter
        return q.Length == 0 || digit ? picks : [.. picks.Where(p => Norm(p.Path + string.Concat(p.Who)).Contains(q))];
    }

    /// <summary>What Enter means: nothing typed takes the highlighted pick; a lone digit takes that row; a path that exists is
    /// taken as typed (~ is home); anything else takes the highlighted match, or null when nothing matches.</summary>
    internal static string? ResolveFolder(string text, IReadOnlyList<FolderPick> matches, int selected, Func<string, bool> exists)
    {
        text = text.Trim();
        var pick = matches.Count == 0 ? null : matches[Math.Clamp(selected, 0, matches.Count - 1)].Path;
        if (text.Length == 0)
            return pick;
        var typed = text.StartsWith("~\\") || text.StartsWith("~/") || text == "~" ? Home + text[1..] : text;
        if (exists(typed))
            return typed;
        if (text.Length == 1 && char.IsAsciiDigit(text[0]) && text[0] != '0' && text[0] - '1' < Math.Min(matches.Count, FolderRows))
            return matches[text[0] - '1'].Path;
        return pick;
    }

    internal List<Line> FolderLines(int W)
    {
        var matches = FilterFolders(folderPicks, Subject.Text);
        if (folderPicks.Count == 0)
            return [[], [S(" Type the folder's full path.", "mu")]];
        if (matches.Count == 0)
            return [[], [S(" Nothing you use matches that. Enter takes what you typed if the folder exists.", "ye")]];
        var top = Math.Max(0, Math.Min(folderSel, matches.Count - 1) - (FolderRows - 1));
        var pathW = Math.Max(24, Math.Min(52, W / 2));
        List<Line> L = [[], [S(" Folders you use", "mu"), S(" (↑↓ pick, Tab fills the box, Enter takes it, or a number)", "fa")]];
        for (var i = top; i < Math.Min(matches.Count, top + FolderRows); i++)
        {
            var p = matches[i];
            var who = p.Who.Count > 0 ? string.Join(", ", p.Who) + " · " : "";
            var text = $" {(i - top + 1),1}  {Fit(Tilde(p.Path), pathW)} {who}{p.Uses} use{(p.Uses == 1 ? "" : "s")} · {When(p.Last)}";
            L.Add(i == Math.Min(folderSel, matches.Count - 1) ? [S(Fit(text, W - 2), "cur")] : [S(text, "cy")]);
        }
        if (matches.Count > FolderRows)
            L.Add([S($" {matches.Count} match{(matches.Count == 1 ? "" : "es")}; keep typing to narrow them", "fa")]);
        return L;
    }

    /// <summary>↑↓ and Tab in the subject box on the folder step.</summary>
    internal bool FolderKey(Key key)
    {
        if (!OnFolderStep || folderPicks.Count == 0)
            return false;
        var matches = FilterFolders(folderPicks, Subject.Text);
        if (matches.Count == 0)
            return false;
        if (key is Key.Down or Key.Up)
            folderSel = Math.Clamp(folderSel + (key == Key.Down ? 1 : -1), 0, matches.Count - 1);
        else if (key == Key.Tab)
        {
            Subject.Text = matches[Math.Min(folderSel, matches.Count - 1)].Path;
            Subject.CaretIndex = Subject.Text.Length;
        }
        else
            return false;
        Render();
        return true;
    }
}
