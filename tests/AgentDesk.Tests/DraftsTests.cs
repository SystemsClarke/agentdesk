using System.IO;
using System.Windows.Input;
using AgentDesk.App;
using Xunit;

namespace AgentDesk.Tests;

/// <summary>Unsent replies and posts survive the window going away (an update's restart, a crash).</summary>
[Collection("window")]
public class DraftsTests
{
    static string TempFile() => Path.Combine(Directory.CreateTempSubdirectory("drafts-").FullName, "drafts.json");

    [Fact]
    public void Drafts_round_trip_and_empty_ones_leave_no_file()
    {
        var file = TempFile();
        DraftStore.Save(file, DraftStore.Serialize(new Dictionary<int, string> { [12] = "half a reply \"quoted\"\nsecond line", [13] = "" }, ("subj", "body")));
        var (replies, compose) = DraftStore.Load(file);
        Assert.Equal("half a reply \"quoted\"\nsecond line", Assert.Single(replies).Value);
        Assert.Equal(12, replies.Keys.Single());
        Assert.Equal(("subj", "body"), compose);

        DraftStore.Save(file, DraftStore.Serialize(new Dictionary<int, string>(), null));
        Assert.False(File.Exists(file));
    }

    [Fact]
    public void A_damaged_or_missing_file_is_no_drafts()
    {
        var file = TempFile();
        Assert.Empty(DraftStore.Load(file).Replies);
        File.WriteAllText(file, "{ not json");
        var (replies, compose) = DraftStore.Load(file);
        Assert.Empty(replies);
        Assert.Null(compose);
    }

    [Fact]
    public void A_new_post_typed_before_the_restart_is_back_in_the_box()
    {
        var file = TempFile();
        File.WriteAllText(file, DraftStore.Serialize(new Dictionary<int, string>(), ("Plan for Friday", "Half written")));
        MainWindow.DraftsFile = file;
        try
        {
            KeyHarness.Run(new RecordingBoard(), w =>
            {
                w.Press(Key.D); // Discussion
                w.Press(Key.N); // new post: the saved draft comes back
                Assert.Equal("compose", w.screen);
                Assert.Equal(("Plan for Friday", "Half written"), (w.Subject.Text, w.Reply.Text));
            });
        }
        finally { MainWindow.DraftsFile = null; }
    }
}
