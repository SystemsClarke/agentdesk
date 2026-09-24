using System.Security.Principal;

namespace AgentDesk.Contracts;

/// <summary>
/// Who is calling, as the agent's own process sees it. The MCP shim runs inside the agent's
/// session, so it fills this from its environment; the core derives the board name from it
/// (a port of agentdesk/identity.py), which keeps two sessions from ever posting as one author.
/// </summary>
public sealed record Caller(
    string? SessionId,   // CLAUDE_CODE_SESSION_ID or AGENTDESK_SESSION
    string? EnvAuthor,   // AGENTDESK_AUTHOR (crew roles stamp this)
    string? Cwd,         // the agent's working directory
    string? Harness,     // e.g. "claude-code"
    int Pid);            // the shim's own pid: lives exactly as long as the agent session

/// <summary>
/// The agent-facing board: one method per MCP tool, same names and arguments as
/// agentdesk/mcp_server.py. Each returns the tool's JSON text, so the shim hands it to the
/// agent unchanged and parity with the Python server can be tested document for document.
/// </summary>
public interface IAgentBoard
{
    Task<string> FormattingHelp();
    Task<string> PostMessage(Caller caller, string channel, string subject, string body, string? author, int? threadId);
    Task<string> AskHuman(Caller caller, string subject, string body, string? author, Dictionary<string, object?>? meta);
    Task<string> ListThreads(string? channel, string? status, int limit, bool includeArchived);
    Task<string> ReadThread(Caller caller, int threadId, string? author);
    Task<string> OpenQuestions(Caller caller, bool includeArchived, string? author);
    Task<string> PassTheTorch(Caller caller, string handoff, string? author);
    Task<string> AnswerThread(Caller caller, int threadId, string body, string? author);
    Task<string> SearchMessages(string query, int limit);
    Task<string> SearchVault(string query, int k, int full);
    Task<string> RecentMessages(int limit);
    Task<string> ListMentions(Caller caller, string? name, int limit);
    Task<string> PostWork(Caller caller, string subject, string body, string? author, string claim);
    Task<string> ListWork(string? status, int limit);
    Task<string> ClaimWork(Caller caller, int threadId, string? author);
    Task<string> CompleteWork(Caller caller, int threadId, string note, string? author);
    Task<string> RequestMerge(Caller caller, string prUrl, int? threadId, string? note, string? author);
}

public static class PipeNames
{
    /// <summary>Per-user pipe; the server's ACL admits only this account.</summary>
    public static string Board => $"agentdesk-{WindowsIdentity.GetCurrent().User?.Value ?? Environment.UserName}";
}
