using System.Windows.Input;
using AgentDesk.App;
using Xunit;

namespace AgentDesk.Tests;

/// <summary>Permission rules: X from the menu, A approves and R rejects the pending rule under the cursor, each after a question.</summary>
[Collection("window")]
public class RulesScreenTests
{
    [Fact]
    public void The_screen_approves_rejects_and_reverts_through_the_core_after_asking()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            w.Press(Key.X);
            Assert.Equal("rules", w.screen);
            DictateWindowTests.Pump(300);

            w.Press(Key.A); // asks first
            Assert.Empty(board.Acts);
            w.Press(Key.N); // anything but Y is no
            Assert.Empty(board.Acts);
            w.Press(Key.A);
            w.Press(Key.Y);
            var (req, args) = Assert.Single(board.Acts);
            Assert.Equal("ui:permission_decide", req);
            Assert.Contains("id = 2", args!.ToString()); // the newest pending one is on top
            Assert.Contains("approve = True", args.ToString());

            board.Acts.Clear();
            w.Press(Key.R);
            w.Press(Key.Y);
            Assert.Contains("approve = False", Assert.Single(board.Acts).Args!.ToString());

            board.Acts.Clear();
            w.Press(Key.V); // a pending rule is not revertible
            w.Press(Key.Y);
            Assert.Empty(board.Acts);

            w.Press(Key.H); // history shows the approved rule, which V can revert
            w.Press(Key.End);
            w.Press(Key.V);
            w.Press(Key.Y);
            Assert.Equal("ui:permission_revert", Assert.Single(board.Acts).Request);

            w.Press(Key.Escape);
            Assert.Equal("main", w.screen);
        });
    }
}
