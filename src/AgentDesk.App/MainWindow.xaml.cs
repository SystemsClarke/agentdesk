using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Windows;
using System.Windows.Automation.Peers;
using System.Windows.Documents;
using System.Windows.Input;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Threading;

namespace AgentDesk.App;

/// <summary>The window: paints the terminal's lines into one read-only RichTextBox and routes keys to Terminal.cs.</summary>
public partial class MainWindow : Window
{
    static readonly string[] PaletteKeys = ["bg", "panel", "line", "fg", "mu", "fa", "rule", "pk", "or", "ye", "gr", "cy", "pu", "on_bar", "textsel"];
    static readonly Dictionary<string, (string Label, bool Dark, string Colors)> Palettes = new()
    {
        ["monokai-pro"] = ("Monokai Pro", true,
            "#221f22 #2d2a2e #403e41 #fcfcfa #939293 #727072 #5b595c #ff6188 #fc9867 #ffd866 #a9dc76 #78dce8 #ab9df2 #221f22 #5b595c"),
        ["monokai-pro-light"] = ("Monokai Pro Light", false,
            "#faf4f2 #ede7e5 #e0dad9 #29242a #706b6e #918c8e #d3cdcc #e14775 #e16032 #cc7a0a #269d69 #1c8ca8 #7058be #faf4f2 #d3cdcc"),
    };
    static readonly string[] ThemeOrder = [.. Palettes.Keys];
    static readonly string SettingsPath = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "AgentDesk", "settings.json");

    readonly IBoard board;
    readonly JsonObject prefs = LoadPrefs();
    readonly List<Paragraph> paras = [];
    readonly Dictionary<string, (Brush? Fg, Brush? Bg, bool Bold, string[] T)> looks = [];
    readonly DispatcherTimer flashTimer = new() { Interval = TimeSpan.FromMilliseconds(3200) };
    readonly DispatcherTimer clock = new() { Interval = TimeSpan.FromSeconds(6) };
    List<Line> painted = [];

    string Theme => Palettes.ContainsKey(Pref("theme", "")) ? Pref("theme", "") : "monokai-pro";

    public MainWindow(IBoard board, int? openThread)
    {
        this.board = board;
        InitializeComponent();
        ApplyFont();
        ApplyTheme();
        flashTimer.Tick += (_, _) => { flashTimer.Stop(); flash = null; Render(); };
        clock.Tick += (_, _) => { if (screen == "main") Render(); };
        // Heartbeats (SlackNet, the worker) change without the board changing, so re-read status on a timer, as Tk did.
        var beats = new DispatcherTimer { Interval = TimeSpan.FromSeconds(30) };
        beats.Tick += async (_, _) => await RefreshAsync();
        beats.Start();
        // A busy board pushes several changes a second; fold each burst into one refresh so the screen doesn't flicker.
        var changed = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(750) };
        changed.Tick += async (_, _) => { changed.Stop(); await RefreshAsync(); };
        board.Changed += (_, _) => Dispatcher.InvokeAsync(() => { if (!changed.IsEnabled) changed.Start(); });
        PreviewKeyDown += OnKey;
        Body.PreviewMouseLeftButtonDown += OnClick;
        Body.SizeChanged += (_, _) => MeasureScreen();
        SourceInitialized += (_, _) => ColourTitleBar();
        Loaded += async (_, _) =>
        {
            await RefreshAsync();
            clock.Start();
            if (openThread is int tid)
                OpenThread(tid);
            else
                Body.Focus();
        };
    }

    /// <summary>A second launch landed here: come to the front, and open the thread it asked for.</summary>
    public void Summon(int? tid)
    {
        Show();
        if (WindowState == WindowState.Minimized)
            WindowState = WindowState.Normal;
        Activate();
        if (tid is int t)
            OpenThread(t);
    }

    // --- painting ----------------------------------------------------------------

    void Render()
    {
        if (!IsLoaded)
            return;
        clickMap.Clear();
        scrollToEnd = false;
        var reading = screen is "read" or "compose";
        var width = reading ? double.NaN : 10_000;
        if (!Doc.PageWidth.Equals(width)) // setting it, even to the same value, re-lays out the whole document: a flicker
            Doc.PageWidth = width;
        var body = screen switch
        {
            "main" => MainScreen(cols), "list" => ChannelList(cols), "prs" => PrsScreen(cols), "sysop" => SysopScreen(cols),
            "who" => WhoScreen(cols), "options" => OptionsScreen(cols), "compose" => Compose(cols), _ => Reader(cols),
        };
        for (var i = 0; i < body.Count; i++)
        {
            if (i < painted.Count && painted[i].SequenceEqual(body[i]))
                continue;
            if (i == paras.Count)
            {
                paras.Add(new Paragraph());
                Doc.Blocks.Add(paras[i]);
            }
            paras[i].Inlines.Clear();
            paras[i].Inlines.AddRange(body[i].Select(ToRun));
        }
        for (; paras.Count > body.Count; paras.RemoveAt(paras.Count - 1))
            Doc.Blocks.Remove(paras[^1]);
        painted = body;
        PaintLine(TopText, TopLine(cols));
        PaintLine(BarText, BarLine(cols));
        Input.Visibility = reading ? Visibility.Visible : Visibility.Collapsed;
        SubjectRow.Visibility = screen == "compose" ? Visibility.Visible : Visibility.Collapsed;
        if (scrollToEnd)
            Body.ScrollToEnd();
        else if (!reading && screen != shownScreen) // only on arriving at a screen: a refresh must not jump the view
            Body.ScrollToHome();
        shownScreen = screen;
    }

    string? shownScreen;
    readonly Dictionary<System.Windows.Controls.TextBlock, Line> shownLine = [];

    void PaintLine(System.Windows.Controls.TextBlock block, Line line)
    {
        if (shownLine.TryGetValue(block, out var was) && was.SequenceEqual(line))
            return; // unchanged: repainting it anyway is what made the bars blink
        shownLine[block] = line;
        block.Inlines.Clear();
        block.Inlines.AddRange(line.Select(ToRun));
    }

    /// <summary>Tag priority follows the Tk app's tag order: rcpt over cur over inv over the bars over a plain colour.</summary>
    Inline ToRun(Seg s)
    {
        if (!looks.TryGetValue(s.Tags, out var look))
        {
            var t = s.Tags.Split(' ');
            var fg = t.Contains("rcpt") ? "fa" : t.Contains("cur") || t.Contains("bar") || t.Contains("barcy") ? "on_bar"
                : t.FirstOrDefault(x => x.Length == 2 || x is "rule");
            var bg = t.Contains("cur") ? "ye" : t.Contains("inv") ? "line" : t.Contains("barcy") ? "cy" : t.Contains("bar") ? "ye" : t.Contains("pnl") ? "panel" : null;
            looks[s.Tags] = look = (fg is null ? null : (Brush)Resources[fg], bg is null ? null : (Brush)Resources[bg], t.Contains("b") || t.Contains("cur"), t);
        }
        var run = new Run(s.Text);
        if (look.Fg != null)
            run.Foreground = look.Fg;
        if (look.Bg != null)
            run.Background = look.Bg;
        if (look.Bold)
            run.FontWeight = FontWeights.Bold;
        if (look.T.Contains("i"))
            run.FontStyle = FontStyles.Italic;
        if (look.T.Contains("s"))
            run.TextDecorations = TextDecorations.Strikethrough;
        if (look.T.FirstOrDefault(x => x is "hd1" or "hd2" or "hd3") is { } hd) // the Tk app's heading sizes: +3, +2, +1 pt
            run.FontSize = FontSize * (Pref("font_size", 11) + '4' - hd[2]) / Pref("font_size", 11);
        if (look.T.FirstOrDefault(x => x.StartsWith("href:")) is not { } href || !Uri.TryCreate(href[5..], UriKind.Absolute, out var uri) || uri.Scheme is not ("http" or "https"))
            return run;
        var link = new Hyperlink(run) { NavigateUri = uri, Foreground = run.Foreground, ToolTip = uri.ToString() };
        link.RequestNavigate += (_, e) => Process.Start(new ProcessStartInfo(e.Uri.ToString()) { UseShellExecute = true });
        return link;
    }

    internal void Flash(string text, string tags = "fg")
    {
        flash = (text, tags);
        flashTimer.Stop();
        flashTimer.Start();
        PaintLine(BarText, BarLine(cols));
        UIElementAutomationPeer.FromElement(BarText)?.RaiseAutomationEvent(AutomationEvents.LiveRegionChanged);
    }

    void MeasureScreen()
    {
        var m = new FormattedText("M", CultureInfo.InvariantCulture, FlowDirection.LeftToRight,
            new Typeface(FontFamily, FontStyles.Normal, FontWeights.Bold, FontStretches.Normal), FontSize, Brushes.White,
            VisualTreeHelper.GetDpi(this).PixelsPerDip);
        var c = Math.Max(64, (int)((Body.ActualWidth - 24) / m.WidthIncludingTrailingWhitespace) - 2);
        var n = (int)((Body.ActualHeight - 16) / (FontFamily.LineSpacing * FontSize));
        if (c != cols || n != lines)
        {
            (cols, lines) = (c, n);
            Render();
        }
    }

    // --- input -------------------------------------------------------------------

    void OnKey(object sender, KeyEventArgs e)
    {
        var key = e.Key == Key.System ? e.SystemKey : e.Key;
        var ctrl = Keyboard.Modifiers.HasFlag(ModifierKeys.Control);
        e.Handled = Reply.IsKeyboardFocused || Subject.IsKeyboardFocused
            ? BoxKey(key, ctrl, Keyboard.Modifiers.HasFlag(ModifierKeys.Alt))
            : ScreenKey(key, ctrl);
    }

    void OnClick(object sender, MouseButtonEventArgs e)
    {
        Body.Focus();
        if (screen is "read" or "compose")
            return;
        e.Handled = true;
        if (Body.GetPositionFromPoint(e.GetPosition(Body), true)?.Paragraph is not { } p || !clickMap.TryGetValue(paras.IndexOf(p), out var i))
            return;
        if (e.ClickCount == 2 || i == Sel)
            ActivateRow();
        else
        {
            Sel = i;
            Render();
        }
    }

    // --- settings, theme, font ---------------------------------------------------

    static JsonObject LoadPrefs()
    {
        try
        {
            return JsonNode.Parse(File.ReadAllText(SettingsPath)) as JsonObject ?? [];
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return [];
        }
    }

    T Pref<T>(string key, T fallback)
    {
        try
        {
            return prefs[key] is JsonValue v && v.TryGetValue<T>(out var value) ? value : fallback;
        }
        catch (InvalidOperationException)
        {
            return fallback;
        }
    }

    void SetPref(string key, JsonNode? value)
    {
        prefs[key] = value;
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(SettingsPath)!);
            File.WriteAllText(SettingsPath, prefs.ToJsonString(new JsonSerializerOptions { WriteIndented = true }));
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            Flash("Couldn't save settings: " + e.Message, "pk");
        }
    }

    void SetTheme(string key)
    {
        SetPref("theme", key);
        ApplyTheme();
    }

    void ApplyTheme()
    {
        var colors = Palettes[Theme].Colors.Split(' ');
        for (var i = 0; i < PaletteKeys.Length; i++)
        {
            var brush = new SolidColorBrush((Color)ColorConverter.ConvertFromString(colors[i]));
            brush.Freeze();
            Resources[PaletteKeys[i]] = brush;
        }
        looks.Clear();
        painted = [];
        shownLine.Clear();
        ColourTitleBar();
        Render();
    }

    void Zoom(int delta)
    {
        SetPref("font_size", Math.Clamp(Pref("font_size", 11) + delta, 8, 22));
        ApplyFont();
        painted = [];
        shownLine.Clear();
        MeasureScreen();
        Render();
    }

    void ApplyFont()
    {
        FontSize = Pref("font_size", 11) * 96.0 / 72;
        Doc.FontSize = FontSize;
        Doc.FontFamily = FontFamily;
    }

    /// <summary>Colour the native title bar to match, as the Tk app does (Windows 11 honours the exact colours).</summary>
    void ColourTitleBar()
    {
        var hwnd = new WindowInteropHelper(this).Handle;
        if (hwnd == 0)
            return;
        static int Ref(object brush) => ((SolidColorBrush)brush).Color is var c ? c.B << 16 | c.G << 8 | c.R : 0;
        foreach (var (attr, value) in new[] { (20, Palettes[Theme].Dark ? 1 : 0), (35, Ref(Resources["panel"])), (36, Ref(Resources["fg"])), (34, Ref(Resources["panel"])) })
        {
            var v = value;
            _ = DwmSetWindowAttribute(hwnd, attr, ref v, sizeof(int));
        }
    }

    [DllImport("dwmapi.dll")]
    static extern int DwmSetWindowAttribute(nint hwnd, int attr, ref int value, int size);
}
