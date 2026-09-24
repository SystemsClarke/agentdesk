using System.Text.Json.Nodes;
using AgentDesk.Core.Board;
using static AgentDesk.Tests.Parity.Harness;

namespace AgentDesk.Tests.Parity;

/// <summary>
/// Each test is one scenario run through Python and C# from the same seeded board (threads 1-4, see Harness.Seed);
/// every step's document and the final contents of every table must match. The comments say what each step pins down.
/// </summary>
public class ParityTests
{
    static string Words(int n) => string.Join(" ", Enumerable.Repeat("word", n));

    [Fact]
    public void PostReadReplyAndReceipts() => Run(
        Step("formatting_help", "alpha"),
        Step("post_message", "alpha", new { channel = "chat", subject = "x", body = "y" }),                  // bad channel: list repr in the error
        Step("post_message", "alpha", new { channel = "discussion", subject = "Findings", body =
            "Found it on build-01.vispero.local, see @builder and @claude-code:proj-beta#be7a, not user@example.com.\n```\n@echo off\n```" }), // mentions, not e-mail or fenced code
        Step("post_message", "beta", new { channel = "discussion", subject = "", body = "Thanks, on it.", author = "claude", thread_id = 5 }), // anonymous author -> session name; delivers the seed ack
        Step("post_message", "alpha", new { channel = "discussion", subject = "", body = "?", thread_id = 999 }),  // no such thread
        Step("post_message", "alpha", new { channel = "wiki", subject = "Trap: flag X is off", body = "Because." }), // mirrored to the vault
        Step("post_message", "alpha", new { channel = "wiki", subject = "FAIL note", body = "Boom." }),   // a failed mirror is reported, not raised
        Step("post_message", "alpha", new { channel = "wiki", subject = "", body = "More.", thread_id = 6 }), // a wiki reply is not re-mirrored
        Step("read_thread", "beta", new { thread_id = 5 }),                                                // receipt
        Step("read_thread", "beta", new { thread_id = 5 }),                                                // ...once only
        Step("read_thread", "alpha", new { thread_id = 6 }),                                               // own thread: no receipt warranted
        Step("read_thread", "alpha", new { thread_id = 999 }),
        Step("read_thread", "alpha", new { thread_id = 3 }),                                               // the ack is a receipt, John's reply is not
        Step("answer_thread", "crew", new { thread_id = 5, body = "Builder here." }),                      // stamped AGENTDESK_AUTHOR
        Step("answer_thread", "bare", new { thread_id = 999, body = "?" }),
        Step("recent_messages", "alpha", new { limit = 5 }),
        Step("recent_messages", "alpha"));

    [Fact]
    public void QuestionsWaitingAndAcks() => Run(
        Step("open_questions", "alpha"),                                                                   // John had the last word on #3
        Step("ask_human", "alpha", new { subject = "Restart?", body = "Can I restart @builder's host? Ünïcode ✓",
            meta = new { kind = "sneaky", priority = 2, ratio = 1.5, whole = 1.0, tags = new[] { "a", "ü" }, mentions = new[] { "x" } } }), // kind stripped, Python json.dumps text
        Step("ask_human", "beta", new { subject = "Too long", body = Words(401) }),                        // over the limit
        Step("ask_human", "beta", new { subject = "Just fits", body = Words(400) }),                       // at the limit; delivers beta's seed ack
        Step("open_questions", "alpha"),
        Step("open_questions", "alpha", new { include_archived = true }),
        Step("list_threads", "alpha", new { channel = "question", include_archived = false }),
        Step("list_threads", "alpha", new { status = "answered" }),
        Step("list_threads", "alpha", new { limit = 3 }),
        JohnReply(5, "Yes, go ahead."),
        Step("open_questions", "alpha"),                                                                   // #5 no longer waiting
        Step("list_threads", "alpha", new { channel = "question" }),                                       // delivery 'pending'
        Step("read_thread", "alpha", new { thread_id = 5 }),                                               // reading is not a write: no ack yet
        Step("answer_thread", "alpha", new { thread_id = 5, body = "Done, restarted." }),                  // reply, then the ack
        Step("open_questions", "alpha"),                                                                   // the agent re-asked: waiting again
        Step("list_threads", "alpha", new { channel = "question" }),                                       // delivery 'picked-up', last_author skips the ack
        Step("answer_thread", "alpha", new { thread_id = 6, body = Words(401) }),                          // limit applies to question-thread replies
        Step("post_message", "alpha", new { channel = "discussion", subject = "", body = Words(401), thread_id = 6 }), // Python checks the passed channel, not the thread's
        JohnReply(6, "No."),
        Step("open_questions", "beta"));

    [Fact]
    public void WorkQueue() => Run(
        Step("post_work", "alpha", new { subject = "x", body = "y", claim = "mine" }),
        Step("post_work", "alpha", new { subject = "Build it", body = "Please build." }),
        Step("list_work", "alpha"),
        Step("list_work", "alpha", new { status = "open" }),
        Step("claim_work", "beta", new { thread_id = 5 }),                                                  // claimed + receipt
        Step("claim_work", "crew", new { thread_id = 5 }),                                                  // already held: false, no receipt
        Step("claim_work", "crew", new { thread_id = 4 }),                                                  // the seed's claim=anyone item
        Step("complete_work", "crew", new { thread_id = 5, note = "Not mine." }),                           // not the assignee
        Step("complete_work", "beta", new { thread_id = 5, note = "Built." }),
        Step("complete_work", "beta", new { thread_id = 5, note = "Again." }),                              // already done
        Step("claim_work", "beta", new { thread_id = 999 }),
        Step("complete_work", "beta", new { thread_id = 999, note = "?" }),
        Step("list_work", "alpha", new { status = "done" }),
        Step("read_thread", "alpha", new { thread_id = 5 }));

    [Fact]
    public void RequestMerge() => Run(
        Step("request_merge", "alpha", new { pr_url = "https://gitlab.com/x/y/merge_requests/1" }),
        Step("request_merge", "alpha", new { pr_url = " https://www.GitHub.com/palencharj/agentdesk/pull/007/files ", thread_id = 1, note = "  Fix the thing\nDetails here." }),
        Step("request_merge", "beta", new { pr_url = "https://github.com/palencharj/agentdesk/pull/7", thread_id = 1, note = "dupe" }), // created=false, no second post
        Step("request_merge", "alpha", new { pr_url = "http://github.com/a/b/pull/9?diff=split" }),                    // no note: title is the url
        Step("request_merge", "alpha", new { pr_url = "https://github.com/a/b/pull/11", thread_id = 999 }),           // foreign key error text
        Step("read_thread", "beta", new { thread_id = 1 }),
        Step("recent_messages", "alpha", new { limit = 3 }));

    [Fact]
    public void RequestMergeBlankNoteFallsBackToUrl()
    {
        // Python raises IndexError on a whitespace-only note ("   ".strip().splitlines()[0]); the port uses the url as the title.
        var step = Step("request_merge", "alpha", new { pr_url = "https://github.com/a/b/pull/10", thread_id = 1, note = "   " });
        step["python_raises"] = "IndexError";
        var o = Run(step);
        Assert.Equal("IndexError", (string?)o.Python[0]?["exception"]);
        Assert.True((bool)JsonNode.Parse(o.CSharp[0])!["created"]!);
        using var db = new BoardStore(o.CSharpDb).Open();
        Assert.Equal("https://github.com/a/b/pull/10", db.Scalar("SELECT title FROM pull_requests WHERE number=10"));
        Assert.Equal("Asking John to merge a/b#10: https://github.com/a/b/pull/10\n\n   ", db.Scalar("SELECT body FROM messages WHERE meta LIKE '%pr-request%'"));
    }

    [Fact]
    public void TorchAndMentions() => Run(
        Step("open_questions", "crew", new { author = "builder" }),                                        // torch_due true from the seed
        Step("open_questions", "alpha", new { author = "claude" }),                                        // anonymous -> alpha's session name
        Step("pass_the_torch", "crew", new { handoff = "Owns X. Next: Y." }),                              // replies on the existing bio thread #2
        Step("open_questions", "crew", new { author = "builder" }),                                        // cleared
        Step("pass_the_torch", "alpha", new { handoff = "Mid-way through Z." }),                           // no bio yet: starts one
        Step("list_threads", "alpha", new { channel = "discussion" }),
        Step("post_message", "alpha", new { channel = "discussion", subject = "ping", body = "ping @claude-code:proj-beta#be7a and @Builder" }),
        Step("list_mentions", "alpha", new { name = "builder" }),
        Step("list_mentions", "alpha", new { name = "BUILDER", limit = 1 }),                               // case-insensitive, limited
        Step("list_mentions", "beta"));                                                                    // defaults to the caller's own name

    [Fact]
    public void Search() => Run(
        Step("post_message", "alpha", new { channel = "discussion", subject = "Deploy log", body = "deploy step one" }),
        Step("post_message", "beta", new { channel = "discussion", subject = "", body = "deploy step two, then builder", thread_id = 5 }),
        Step("search_messages", "alpha", new { query = "deploy" }),                                        // FTS hits newest first, then substring-only hits
        Step("search_messages", "alpha", new { query = "deploy", limit = 2 }),
        Step("search_messages", "alpha", new { query = "build-01.vispero" }),                              // FTS cannot parse it: substring fallback
        Step("search_messages", "alpha", new { query = "\"unbalanced" }),
        Step("search_messages", "alpha", new { query = "builder", limit = 1 }),
        Step("search_messages", "alpha", new { query = "o", limit = -1 }),                                 // Python's out[:-1]
        Step("search_messages", "alpha", new { query = "nothingmatches" }),
        Step("search_vault", "alpha", new { query = "flag", full = 1 }),
        Step("search_vault", "alpha", new { query = "flag", k = 2, full = -1 }),
        Step("search_vault", "alpha", new { query = "offline" }));
}
