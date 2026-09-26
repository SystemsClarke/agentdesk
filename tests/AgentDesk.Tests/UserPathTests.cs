using AgentDesk.Core;

namespace AgentDesk.Tests;

/// <summary>The user PATH edit Setup makes at install, update and uninstall: strings only, and a fake registry.
/// Nothing here reads or writes the real HKCU\Environment.</summary>
public sealed class UserPathTests
{
    const string Dir = @"C:\Users\x\AppData\Local\AgentDeskApp\current";

    [Theory]
    [InlineData(null, Dir)]
    [InlineData("", Dir)]
    [InlineData(@"C:\a", @"C:\a;" + Dir)]
    [InlineData(@"C:\a;", @"C:\a;" + Dir)] // no empty entry left behind
    [InlineData(@"%USERPROFILE%\bin;C:\a", @"%USERPROFILE%\bin;C:\a;" + Dir)] // variables stay unexpanded
    public void On_appends_it_once(string? path, string expected) => Assert.Equal(expected, UserPath.With(path, Dir, true));

    [Theory]
    [InlineData(Dir)]
    [InlineData(@"C:\a;" + Dir + @";C:\b")]
    [InlineData(@"C:\a;c:\users\X\appdata\local\agentdeskapp\CURRENT\")] // case and a trailing backslash
    [InlineData("C:\\a;\"" + Dir + "\"")] // quoted
    public void On_is_idempotent(string path) => Assert.Null(UserPath.With(path, Dir, true));

    [Fact]
    public void On_matches_an_entry_written_with_a_variable()
    {
        var local = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
        Assert.Null(UserPath.With(@"%LOCALAPPDATA%\AgentDeskApp\current", Path.Combine(local, "AgentDeskApp", "current"), true));
    }

    [Theory]
    [InlineData(Dir, "")]
    [InlineData(@"C:\a;" + Dir, @"C:\a")]
    [InlineData(@"C:\a;" + Dir + @";C:\b", @"C:\a;C:\b")]
    [InlineData(Dir + @"\;C:\a;" + Dir, @"C:\a")] // every copy
    [InlineData(@"C:\a;;" + Dir + @";%X%\y", @"C:\a;;%X%\y")] // the rest exactly as it was
    public void Off_removes_only_it(string path, string expected) => Assert.Equal(expected, UserPath.With(path, Dir, false));

    [Theory]
    [InlineData(null)]
    [InlineData(@"C:\a;C:\Users\x\AppData\Local\AgentDeskApp\current2")] // a longer name is not it
    public void Off_without_it_changes_nothing(string? path) => Assert.Null(UserPath.With(path, Dir, false));

    [Fact]
    public void Update_writes_only_when_the_value_changes()
    {
        string? reg = @"C:\a";
        var writes = 0;
        bool Run(bool on) => UserPath.Update(() => reg, v => { reg = v; writes++; }, Dir, on);

        Assert.True(Run(true));
        Assert.False(Run(true)); // reinstall, update
        Assert.Equal(@"C:\a;" + Dir, reg);
        Assert.True(Run(false));
        Assert.False(Run(false));
        Assert.Equal(@"C:\a", reg);
        Assert.Equal(2, writes);
    }
}
