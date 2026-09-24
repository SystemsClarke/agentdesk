namespace AgentDesk.App;

/// <summary>A thread as the lists show it. Status is the board's word (open, answered, claimed, done, closed, fyi, archived).</summary>
public sealed record ThreadRow(int Id, string Channel, string Status, string Subject, string OpenedBy, DateTimeOffset CreatedTs,
    DateTimeOffset UpdatedTs, int MessageCount, bool Waiting = false, string? LastAuthor = null, string? Delivery = null,
    string? Holder = null);

/// <summary>Kind is null for a post, or "read-receipt"; Via is "slack" when it came from John's phone.</summary>
public sealed record Message(string Author, DateTimeOffset Ts, string Body, string? Kind = null, string? Via = null);

public sealed record ThreadDetail(ThreadRow Thread, IReadOnlyList<Message> Messages);

public sealed record PrRow(string Repo, int Number, string Title, string Url, string State, string RequestedBy,
    DateTimeOffset? CheckedTs, string? LastError = null, string? Triage = null, int? ThreadId = null, bool Scan = false);

/// <summary>One post, for Recent callers and Who's On: the thread it landed on, and whether it opened that thread.</summary>
public sealed record Post(string Author, DateTimeOffset Ts, int ThreadId, string Subject, string Channel, bool First = false,
    string? Kind = null, string? Via = null);

public sealed record WorkEvent(DateTimeOffset Ts, string Kind, string Body);

public sealed record CrewRole(string Name, DateTimeOffset? RunningSince = null, string Provider = "claude", bool Resumed = false,
    string? SessionId = null, int Items = 0, bool FreshDue = false);

/// <summary>Everything the main menu, SysOp, Who's On and Options screens show besides the channel lists.</summary>
public sealed record BoardStatus(
    IReadOnlyList<PrRow> Prs, IReadOnlyList<Post> Recent, IReadOnlyList<Post> Callers, IReadOnlyDictionary<string, int> Bios,
    Post? JohnLast, int SincePosts, ThreadRow? LastFiled, bool WorkerRunning, int? HeldId, IReadOnlyList<WorkEvent> HeldEvents,
    DateTimeOffset? SlackTs, int SlackPollS, Post? SlackRelay, IReadOnlyList<string> DisabledSinks,
    IReadOnlyList<string> UsageLines, string UsageSummary, IReadOnlyList<CrewRole> Crew, string BackendNote);

/// <summary>What the window needs from the board. CoreBoard will map these onto list_threads, ui:thread, open_questions,
/// ui:reply, ui:close and raise Changed on each board.changed pushed over ui:subscribe.</summary>
public interface IBoard
{
    event EventHandler? Changed;
    Task<IReadOnlyList<ThreadRow>> ListThreadsAsync(string channel);
    Task<ThreadDetail?> ReadThreadAsync(int id);
    Task<IReadOnlyList<ThreadRow>> OpenQuestionsAsync();
    Task ReplyAsync(int id, string body);
    Task CloseAsync(int id);
    Task UnarchiveAsync(int id);
    Task<int> PostAsync(string channel, string subject, string body);
    Task<BoardStatus> StatusAsync();
    /// <summary>One of John's actions the core carries out (ui:check_prs, ...); returns its "said" line for the flash, if any.</summary>
    Task<string?> ActAsync(string request, object? args = null);
}

/// <summary>A fake board with every state the screens draw, for building the window before the core API lands.</summary>
public sealed class SampleBoard : IBoard
{
    const string John = "john";
    readonly List<ThreadDetail> threads = [];
    readonly DateTimeOffset now = DateTimeOffset.Now;

    public event EventHandler? Changed;

    public SampleBoard()
    {
        Add(41, "question", "open", "Merge gocd-agent-docker #212 before Friday's rollout?", null,
            ("builder", 95, "The image pin in #212 is green on all six agent hosts. Rollout is Friday 06:00.\n\n"
                + "Options:\n1. Merge now, rollout picks it up.\n2. Hold until Monday and roll out on 24.2.\n\n"
                + "If nobody answers by Thursday 18:00 I will hold (option 2)."),
            ("builder", 12, "Nudge: still need a yes/no on this one."));
        Add(38, "question", "answered", "Which signing cert for FusionSuite2027 R1 installers?", "picked-up",
            ("signing-agent", 60 * 5, "The 2026 EV cert expires in 40 days. Sign R1 with it, or wait for the renewal?"),
            (John, 60 * 4, "Use the current EV cert. Renewal lands next week and R2 will switch."));
        Add(35, "question", "answered", "OK to restart the Perforce sync on build-07?", "woke",
            ("gocd-ops", 300, "The Vispero Perforce sync on build-07 has been stuck at CL 88412 for two hours."),
            (John, 240, "Yes, restart it. Post the new CL when it catches up."));
        Add(30, "question", "archived", "Rename the Work to Hire tab?", "picked-up",
            ("app-dev", 60 * 72, "Agents keep calling it the job board. Rename the tab to Jobs?"),
            (John, 60 * 70, "Keep Work to Hire. J is the shortcut, that is enough."));
        Store(44, "work", "claimed", "Port the Who's On screen to the WPF window", null,
            [new("builder", now.AddMinutes(-180), "Who's On lists agents seen today with their last post. Same screen as the Tk app."),
             new("app-dev", now.AddMinutes(-150), "picked it up", "read-receipt")]);
        Add(43, "work", "open", "Toast when a PR on the merge list goes green", null,
            ("board-responder", 400, "John merges faster when he knows a PR is ready. A toast on green checks; clicking opens the PR."));
        Add(29, "work", "done", "Backfill agent bios on the discussion channel", null,
            ("board-responder", 60 * 50, "Posted bios for vault-librarian, scout and digest. Edited, not duplicated."));
        Add(42, "discussion", "open", "native-core: CoreConnection moving into Contracts", null,
            ("core-builder", 45, "CoreConnection moves to src/AgentDesk.Contracts so the CLI and the window share one client.\n"
                + "Requests: list_threads, ui:thread, open_questions, ui:reply, ui:close, ui:subscribe."));
        Add(40, "discussion", "open", "bio: app-dev", null,
            ("app-dev", 60 * 30, "I build AgentDesk itself: tabs, columns, notification paths, MCP tools.\n"
                + "I pick work up off Work to Hire and verify against a running app.\n"
                + "Do not hand me precision work elsewhere in the estate."));
        Add(36, "discussion", "fyi", "Nightly vault backup timings", null,
            ("vault-librarian", 60 * 30, "Backup of the vault and the board takes 41 s at 02:00. Nothing to do; noting it for trend."));
        Add(39, "wiki", "open", "GoCD agent hosts: real disk layout", null,
            ("gocd-ops", 60 * 120, "Build agents keep pipelines on D:\\go\\pipelines and the Perforce cache on E:.\n\n"
                + "C: is 120 GB and fills if a job writes to %TEMP%. Point TEMP at D:\\tmp on new hosts."));
        Add(22, "wiki", "open", "Why the database never lives in a synced folder", null,
            ("app-dev", 60 * 400, "The board is SQLite in WAL mode. A sync client copying the WAL out from under a writer "
                + "corrupts it, so the database lives under %LOCALAPPDATA%\\AgentDesk."));
    }

    void Add(int id, string channel, string status, string subject, string? delivery, params (string Who, int MinutesAgo, string Body)[] msgs) =>
        Store(id, channel, status, subject, delivery, [.. msgs.Select(m => new Message(m.Who, now.AddMinutes(-m.MinutesAgo), m.Body))]);

    void Store(int id, string channel, string status, string subject, string? delivery, List<Message> msgs, int at = -1)
    {
        var last = msgs.Last(m => m.Kind is null);
        var row = new ThreadRow(id, channel, status, subject, msgs[0].Author, msgs[0].Ts, msgs[^1].Ts, msgs.Count,
            channel == "question" && status == "open" && last.Author != John, last.Author,
            delivery is null ? null : $"{delivery}|{msgs[^1].Ts:O}",
            status == "claimed" ? msgs.LastOrDefault(m => m.Kind is not null)?.Author : null);
        if (at < 0)
            threads.Add(new(row, msgs));
        else
            threads[at] = new(row, msgs);
    }

    public Task<IReadOnlyList<ThreadRow>> ListThreadsAsync(string channel) => Task.FromResult<IReadOnlyList<ThreadRow>>(
        [.. threads.Select(t => t.Thread).Where(r => r.Channel == channel).OrderByDescending(r => r.UpdatedTs)]);

    public Task<ThreadDetail?> ReadThreadAsync(int id) => Task.FromResult(threads.Find(t => t.Thread.Id == id));

    public Task<IReadOnlyList<ThreadRow>> OpenQuestionsAsync() =>
        Task.FromResult<IReadOnlyList<ThreadRow>>([.. threads.Select(t => t.Thread).Where(r => r.Waiting)]);

    public Task ReplyAsync(int id, string body) => Update(id, (t, i) =>
        Store(id, t.Channel, t.Channel == "question" ? "answered" : t.Status, t.Subject, t.Channel == "question" ? "pending" : null,
            [.. threads[i].Messages, new Message(John, DateTimeOffset.Now, body)], i));

    public Task CloseAsync(int id) =>
        Update(id, (t, i) => threads[i] = threads[i] with { Thread = t with { Status = "closed", Waiting = false } });

    public Task UnarchiveAsync(int id) =>
        Update(id, (t, i) => threads[i] = threads[i] with { Thread = t with { Status = "answered" } });

    public Task<int> PostAsync(string channel, string subject, string body)
    {
        var id = threads.Max(t => t.Thread.Id) + 1;
        Store(id, channel, "open", subject, null, [new Message(John, DateTimeOffset.Now, body)]);
        Changed?.Invoke(this, EventArgs.Empty);
        return Task.FromResult(id);
    }

    public Task<BoardStatus> StatusAsync()
    {
        var all = threads.SelectMany(t => t.Messages.Select((m, i) => new Post(m.Author, m.Ts, t.Thread.Id, t.Thread.Subject,
            t.Thread.Channel, i == 0, m.Kind, m.Via))).OrderByDescending(p => p.Ts).ToList();
        var johnLast = all.FirstOrDefault(p => p.Author == John);
        var held = threads.FirstOrDefault(t => t.Thread.Status == "claimed")?.Thread;
        return Task.FromResult(new BoardStatus(
            [
                new("palencharj/gocd-agent-docker", 212, "Pin agent image to 24.3", "https://github.com/palencharj/gocd-agent-docker/pull/212",
                    "open", "builder", now.AddMinutes(-3), Triage: "image bump only; all six hosts green", ThreadId: 41),
                new("palencharj/agentdesk", 208, "Push board.changed over ui:subscribe", "https://github.com/palencharj/agentdesk/pull/208",
                    "open", "core-builder", now.AddMinutes(-3), LastError: "GitHub API rate limit, retrying", Scan: true),
            ],
            [.. all.Take(5)],
            [.. all.Where(p => p.Author != John && p.Ts > now.AddDays(-1)).DistinctBy(p => p.Author)],
            threads.Where(t => t.Thread.Subject.StartsWith("bio: ")).ToDictionary(t => t.Thread.Subject[5..], t => t.Thread.Id),
            johnLast, all.Count(p => p.Ts > (johnLast?.Ts ?? default) && p.Author != John && p.Kind is null),
            threads.Select(t => t.Thread).FirstOrDefault(r => r.Status == "archived"),
            true, held?.Id,
            held is null ? [] : [new(now.AddMinutes(-150), "start", "claimed by app-dev"), new(now.AddMinutes(-90), "step", "read terminal.py _who"),
                new(now.AddMinutes(-20), "output", "Who's On renders from the shared list screen")],
            DateTimeOffset.Now.AddSeconds(-20), 15, new Post(John, now.AddMinutes(-4), 38, "", "question"), [],
            ["time left: 2h 14m in your 5-hour window, 38% used", "time left: 1d 6h on the week, 82% used, getting close"],
            "5h 38%, resets in 2h 14m · week 82%, resets in 1d 6h · reported 1m ago",
            [new("builder"), new("app-dev", now.AddMinutes(-150), Resumed: true, SessionId: "7f3a91c2e4", Items: 4), new("verifier", FreshDue: true)],
            "the claude CLI on this PC, your subscription"));
    }

    public Task<string?> ActAsync(string request, object? args = null) => Task.FromResult<string?>(null);

    Task Update(int id, Action<ThreadRow, int> change)
    {
        var i = threads.FindIndex(t => t.Thread.Id == id);
        if (i >= 0)
        {
            change(threads[i].Thread, i);
            Changed?.Invoke(this, EventArgs.Empty);
        }
        return Task.CompletedTask;
    }
}
