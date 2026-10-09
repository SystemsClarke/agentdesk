using System.Text.Json;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>Agents propose permission rules; only John's approval writes settings.json.</summary>
public sealed class PermissionsTests : IDisposable
{
    readonly string dir = Directory.CreateTempSubdirectory("perms-").FullName;
    readonly BoardStore store;
    readonly string settings;
    readonly Permissions perms;
    static readonly Caller John = new(null, null, null, "ui", 1);
    static readonly Caller Agent = new("sid-1", "builder", @"C:\work", "claude-code", 2, "builder");

    public PermissionsTests()
    {
        store = new BoardStore(Path.Combine(dir, "agentdesk.db"));
        store.Init();
        settings = Path.Combine(dir, "claude", "settings.json");
        Directory.CreateDirectory(Path.GetDirectoryName(settings)!);
        File.WriteAllText(settings, """{"model":"opus","permissions":{"allow":["Read(*)"],"deny":["Bash(rm:*)"]},"env":{"A":"é<b>"}}""");
        perms = new Permissions(store, dir, settings);
    }

    public void Dispose() { try { Directory.Delete(dir, true); } catch (IOException) { } }

    static void Refuses(Func<Task<string>> call) => Assert.Throws<ArgumentException>(() => { _ = call(); }); // the core throws before it returns a Task
    static JsonObject J(Task<string> t) => JsonNode.Parse(t.Result)!.AsObject();
    JsonObject Settings() => JsonNode.Parse(File.ReadAllText(settings))!.AsObject();

    [Fact]
    public void A_proposal_rings_john_and_writes_nothing()
    {
        var r = J(perms.Propose(Agent, "Bash(python tools/trigger_pipeline.py:*)", null, "John asked me to trigger mainline builds", "python tools/trigger_pipeline.py --run", null, null));
        Assert.Equal("pending", (string)r["status"]!);
        using var db = store.Open();
        var thread = db.Rows("SELECT * FROM threads WHERE id=$i", ("i", (long)r["thread_id"]!)).Single();
        Assert.Equal("question", (string)thread["channel"]!);
        Assert.Equal("open", (string)thread["status"]!);
        Assert.Equal(["Read(*)"], Settings()["permissions"]!["allow"]!.AsArray().Select(a => (string)a!)); // untouched
        // the same rule again is not a second proposal
        Assert.Equal((long)r["id"]!, (long)J(perms.Propose(Agent, "Bash(python tools/trigger_pipeline.py:*)", null, "again", null, null, null))["id"]!);
        Assert.Single(db.Rows("SELECT * FROM permission_proposals"));
    }

    [Fact]
    public void An_agent_cannot_approve_reject_or_revert()
    {
        var id = (long)J(perms.Propose(Agent, "Bash(git status:*)", null, "why", null, null, null))["id"]!;
        Refuses(() => perms.Decide(Agent, id, true));
        Refuses(() => perms.Decide(Agent, id, false));
        Refuses(() => perms.Revert(Agent, id));
        Assert.DoesNotContain("Bash(git status:*)", File.ReadAllText(settings));
    }

    [Fact]
    public void Approval_adds_the_rule_once_keeps_every_other_key_backs_up_and_answers_on_the_thread()
    {
        var p = J(perms.Propose(Agent, "Bash(git status:*)", null, "why", null, null, null));
        var id = (long)p["id"]!;
        var done = J(perms.Decide(John, id, true));
        Assert.Equal("approved", (string)done["status"]!);
        var s = Settings();
        Assert.Equal(["Read(*)", "Bash(git status:*)"], s["permissions"]!["allow"]!.AsArray().Select(a => (string)a!));
        Assert.Equal(["Bash(rm:*)"], s["permissions"]!["deny"]!.AsArray().Select(a => (string)a!));
        Assert.Equal("opus", (string)s["model"]!);
        Assert.Contains("é<b>", File.ReadAllText(settings)); // not rewritten as \u00E9 \u003C
        Assert.Contains("\"Read(*)\"", File.ReadAllText(Directory.GetFiles(Path.Combine(dir, "permission_backups")).Single())); // the file as it was
        using var db = store.Open();
        var msgs = db.Rows("SELECT author, body FROM messages WHERE thread_id=$t ORDER BY id", ("t", (long)p["thread_id"]!));
        Assert.Contains(msgs, m => ((string)m["body"]!).Contains("is now in `permissions.allow`"));
        Assert.Equal("answered", (string)db.Rows("SELECT status FROM threads WHERE id=$t", ("t", (long)p["thread_id"]!)).Single()["status"]!); // John's line settles the question
        Refuses(() => perms.Decide(John, id, true)); // decided once
        // already allowed now: a new proposal says so
        Assert.True((bool)J(perms.Propose(Agent, "Bash(git status:*)", null, "x", null, null, null))["already_allowed"]!);
    }

    [Fact]
    public void Rejection_writes_nothing_and_revert_takes_an_approved_rule_back_out()
    {
        var no = (long)J(perms.Propose(Agent, "Bash(curl:*)", null, "why", null, null, null))["id"]!;
        perms.Decide(John, no, false);
        Assert.DoesNotContain("curl", File.ReadAllText(settings));
        Refuses(() => perms.Revert(John, no)); // only an approved rule can be reverted

        var yes = (long)J(perms.Propose(Agent, "Bash(git log:*)", null, "why", null, null, null))["id"]!;
        perms.Decide(John, yes, true);
        Assert.Equal("reverted", (string)J(perms.Revert(John, yes))["status"]!);
        Assert.Equal(["Read(*)"], Settings()["permissions"]!["allow"]!.AsArray().Select(a => (string)a!));
    }

    [Fact]
    public void A_settings_file_that_is_not_json_is_refused_untouched_and_a_bad_rule_is_not_proposed()
    {
        var id = (long)J(perms.Propose(Agent, "Bash(git diff:*)", null, "why", null, null, null))["id"]!;
        File.WriteAllText(settings, "{ not json");
        Refuses(() => perms.Decide(John, id, true));
        Assert.Equal("{ not json", File.ReadAllText(settings));
        using (var db = store.Open()) Assert.Equal("pending", (string)db.Rows("SELECT status FROM permission_proposals WHERE id=$i", ("i", id)).Single()["status"]!);
        foreach (var bad in new[] { "", "Bash(a\nb)", "rm -rf /", "Bash(" })
            Refuses(() => perms.Propose(Agent, bad, null, "why", null, null, null));
        Refuses(() => perms.Propose(Agent, "Bash(x:*)", null, "", null, null, null));
    }

    [Fact]
    public void A_broad_rule_is_flagged_and_a_project_rule_goes_to_the_projects_own_settings()
    {
        Assert.True(Permissions.IsBroad("Bash"));
        Assert.True(Permissions.IsBroad("Bash(*)"));
        Assert.True(Permissions.IsBroad("Bash(:*)"));
        Assert.False(Permissions.IsBroad("Bash(git status:*)"));
        var proj = Directory.CreateDirectory(Path.Combine(dir, "proj")).FullName;
        var id = (long)J(perms.Propose(Agent, "Bash(make:*)", "project", "why", null, null, proj))["id"]!;
        perms.Decide(John, id, true);
        var file = Path.Combine(proj, ".claude", "settings.json");
        Assert.Equal(["Bash(make:*)"], JsonNode.Parse(File.ReadAllText(file))!["permissions"]!["allow"]!.AsArray().Select(a => (string)a!));
        Assert.DoesNotContain("make", File.ReadAllText(settings)); // the user's file is not the project's
        Assert.Contains(J(perms.List())["proposals"]!.AsArray(), x => (string)x!["rule"]! == "Bash(make:*)" && (bool)x["broad"]! == false);
    }
}
