using System.Text.Json;
using AgentDesk.App;
using AgentDesk.Core;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>The folder suggestions behind New agent and New goal: ranking, where they are gathered from, and how the dialog filters and takes one.</summary>
public sealed class FoldersTests : IDisposable
{
    static readonly DateTimeOffset Now = new(2026, 9, 29, 12, 0, 0, TimeSpan.Zero);
    readonly string dir = Directory.CreateTempSubdirectory("folders-").FullName;

    public void Dispose() => Directory.Delete(dir, true);

    [Fact]
    public void Recent_use_beats_old_use_an_agent_counts_for_more_and_spellings_of_one_folder_merge()
    {
        Folders.Use[] uses =
        [
            new(@"C:\a\repo", Now.AddDays(-1), 1), new("c:/a/repo/", Now.AddDays(-2), 1),
            new(@"C:\b\old", Now.AddDays(-80), 1), new(@"C:\b\old", Now.AddDays(-81), 1), new(@"C:\b\old", Now.AddDays(-82), 1),
            new(@"C:\c\agent", Now.AddDays(-3), 3, "builder"), new("", Now, 1),
        ];
        var picks = Folders.Rank(uses, Now, _ => true);
        Assert.Equal([@"C:\c\agent", @"C:\a\repo", @"C:\b\old"], picks.Select(p => p.Path));
        Assert.Equal(2, picks[1].Uses);
        Assert.Equal(["builder"], picks[0].Who);
        Assert.Equal(Now.AddDays(-1), picks[1].Last);
    }

    [Fact]
    public void Folders_that_are_gone_are_dropped_and_at_most_three_names_and_the_take_limit_apply()
    {
        var uses = new[] { "w", "x", "y", "z" }.Select((who, i) => new Folders.Use(@"C:\shared", Now.AddHours(-i), 3, who)).Append(new(@"C:\gone", Now, 3, "ghost"))
            .Concat(Enumerable.Range(1, 5).Select(i => new Folders.Use($@"C:\f{i}", Now.AddDays(-i), 1)));
        var picks = Folders.Rank(uses, Now, p => p != @"C:\gone", take: 3);
        Assert.Equal(3, picks.Count);
        Assert.Equal(@"C:\shared", picks[0].Path);
        Assert.Equal(["w", "x", "y"], picks[0].Who); // most recent first, three at most
        Assert.Equal(4, picks[0].Uses);
        Assert.DoesNotContain(picks, p => p.Path == @"C:\gone");
    }

    [Fact]
    public void Gather_reads_identities_goals_agent_sessions_and_recent_transcripts()
    {
        var (a, b, c, d) = (Directory.CreateDirectory(Path.Combine(dir, "a")).FullName, Directory.CreateDirectory(Path.Combine(dir, "b")).FullName,
            Directory.CreateDirectory(Path.Combine(dir, "c")).FullName, Directory.CreateDirectory(Path.Combine(dir, "d")).FullName);
        var store = new BoardStore(Path.Combine(dir, "agentdesk.db"));
        store.Init();
        using (var db = store.Open())
        {
            db.Exec("INSERT INTO identities (name, folder, created_ts, updated_ts) VALUES ('builder', $f, '2026-09-28T10:00:00Z', '2026-09-28T10:00:00Z')", ("f", a));
            db.Exec("INSERT INTO identities (name, folder, host, created_ts, updated_ts) VALUES ('linuxy', '/home/j', 'wsl:Ubuntu', '2026-09-28T10:00:00Z', '2026-09-28T10:00:00Z')");
            db.Exec("INSERT INTO goals (name, objective, measure_folder, lead, created_ts, updated_ts) VALUES ('speed', 'go', $f, 'speed-lead', '2026-09-28T10:00:00Z', '2026-09-28T10:00:00Z')", ("f", b));
            db.Exec("INSERT INTO sessions (author, session_id, cwd, seen_ts) VALUES ('claude:x#1', 's1', $f, '2026-09-27T10:00:00Z')", ("f", c));
        }
        var projects = Directory.CreateDirectory(Path.Combine(dir, "projects", "C--d")).FullName;
        File.WriteAllText(Path.Combine(projects, "recent.jsonl"), JsonSerializer.Serialize(new { type = "summary" }) + "\n" + JsonSerializer.Serialize(new { cwd = d }) + "\n{cut off");
        var old = Path.Combine(projects, "old.jsonl");
        File.WriteAllText(old, JsonSerializer.Serialize(new { cwd = Path.Combine(dir, "ancient") }));
        File.SetLastWriteTimeUtc(old, Now.UtcDateTime.AddDays(-200));
        File.WriteAllText(Path.Combine(projects, "nocwd.jsonl"), "{}\n{}\n");
        File.SetLastWriteTimeUtc(Path.Combine(projects, "recent.jsonl"), Now.UtcDateTime.AddDays(-1));

        var picks = Folders.Rank(Folders.Gather(store, Path.Combine(dir, "projects"), Now), Now, Directory.Exists);
        Assert.Equal(new[] { a, b, c, d }.Order(), picks.Select(p => p.Path).Order()); // no wsl folder, no ancient one, no transcript without a cwd
        Assert.Equal(["builder"], picks.Single(p => p.Path == a).Who);
        Assert.Equal(["speed"], picks.Single(p => p.Path == b).Who);
        Assert.Empty(picks.Single(p => p.Path == d).Who);
    }

    [Fact]
    public void Scratch_folders_under_appdata_and_the_temp_folder_are_never_suggested()
    {
        const string profile = @"C:\Users\j";
        Assert.True(Folders.Noise(@"C:\Users\j\AppData\Local\Temp", profile, @"C:\Users\j\AppData\Local\Temp\"));
        Assert.True(Folders.Noise(@"c:/users/j/appdata/roaming/Claude/scratch-workspaces/x/y", profile, @"D:\tmp\"));
        Assert.True(Folders.Noise(@"D:\tmp\run1", profile, @"D:\tmp\"));
        Assert.False(Folders.Noise(@"C:\Users\j\AppDataStuff", profile, @"D:\tmp\")); // a sibling that merely starts the same
        Assert.False(Folders.Noise(@"C:\Users\j\NoOneDrive\AgentDesk", profile, @"D:\tmp\"));
    }

    static readonly IReadOnlyList<FolderPick> Picks =
    [
        new(@"C:\Users\j\NoOneDrive\AgentDesk", ["agentdesk-terminal", "build-speed"], 20, Now),
        new(@"C:\Users\j\NoOneDrive\FastBuild", ["build-speed-lead"], 9, Now),
        new(@"D:\src\other", [], 1, Now),
    ];

    [Fact]
    public void Typing_filters_the_list_ignoring_case_spaces_and_punctuation_and_matches_who_works_there()
    {
        Assert.Equal(3, MainWindow.FilterFolders(Picks, "").Count);
        Assert.Equal([Picks[0].Path], MainWindow.FilterFolders(Picks, "agent desk").Select(p => p.Path));
        Assert.Equal([Picks[1].Path], MainWindow.FilterFolders(Picks, "FASTBUILD").Select(p => p.Path));
        Assert.Equal([Picks[0].Path, Picks[1].Path], MainWindow.FilterFolders(Picks, "build-speed").Select(p => p.Path)); // by who, too
        Assert.Empty(MainWindow.FilterFolders(Picks, "nothing here"));
        Assert.Equal(3, MainWindow.FilterFolders(Picks, "2").Count); // a lone digit picks a row; it does not filter
    }

    [Fact]
    public void Enter_takes_a_typed_folder_that_exists_a_numbered_row_or_the_highlighted_match()
    {
        bool Exists(string p) => p is @"C:\real\one" || p == Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "src");
        Assert.Equal(Picks[1].Path, MainWindow.ResolveFolder("", Picks, 1, Exists)); // nothing typed: the highlighted one
        Assert.Equal(@"C:\real\one", MainWindow.ResolveFolder(@"C:\real\one", Picks, 1, Exists)); // a real path wins over the highlight
        Assert.Equal(Picks[2].Path, MainWindow.ResolveFolder("3", Picks, 0, Exists));
        Assert.Equal(Picks[0].Path, MainWindow.ResolveFolder("9", Picks, 0, Exists)); // no row 9: the highlighted match
        var narrowed = MainWindow.FilterFolders(Picks, "fast");
        Assert.Equal(Picks[1].Path, MainWindow.ResolveFolder("fast", narrowed, 0, Exists));
        Assert.Null(MainWindow.ResolveFolder(@"C:\typo", MainWindow.FilterFolders(Picks, @"C:\typo"), 0, Exists));
        Assert.Equal(Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "src"), MainWindow.ResolveFolder(@"~\src", Picks, 0, Exists));
    }
}
