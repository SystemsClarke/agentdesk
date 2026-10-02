using System.Windows.Input;

namespace AgentDesk.Tests;

/// <summary>The window's key handling driven headless: keys go in as a Key and its modifiers, not through the keyboard.</summary>
public sealed class KeySeamTests
{
    [Fact]
    public void A_key_moves_the_selection_and_a_letter_changes_screen()
    {
        KeyHarness.Run(new RecordingBoard(), w =>
        {
            Assert.Equal("main", w.screen);
            Assert.True(w.Press(Key.Q));
            Assert.Equal("list", w.screen);
            DictateWindowTests.Pump(300);
            Assert.Equal(0, w.Sel);
            w.Press(Key.Down);
            Assert.Equal(1, w.Sel);
            w.Press(Key.Up);
            Assert.Equal(0, w.Sel);
        });
    }

    [Fact]
    public void A_key_that_asks_the_core_for_something_is_recorded_on_the_board()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            w.Press(Key.P);
            Assert.Equal("prs", w.screen);
            w.Press(Key.C);
            Assert.Contains(board.Acts, a => a.Request == "ui:check_prs");
        });
    }

    [Fact]
    public void Ctrl_comes_from_the_modifiers_given_and_hang_up_goes_through_the_hide_seam()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            var hidden = 0;
            w.hide = () => hidden++;
            Assert.True(w.Press(Key.X, ModifierKeys.Control)); // Ctrl+anything unbound is swallowed
            Assert.False(w.Press(Key.C, ModifierKeys.Control)); // copy is left to the system
            Assert.NotEmpty(w.BarLine(80));
            Assert.NotEmpty(w.Hints());
            w.Press(Key.G);
            DictateWindowTests.Pump(600);
            Assert.Equal(1, hidden);
        });
    }
}
