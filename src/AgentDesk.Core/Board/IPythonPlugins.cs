using System.Text.Json;
using System.Text.Json.Nodes;

namespace AgentDesk.Core.Board;

/// <summary>The Python the board still calls into:
/// "vault.mirror_thread" {thread_id} returns vault.mirror_thread's result row;
/// "vault.search" {query, k, full} returns the whole search_vault document ({"hits": [...]} or {"error": ...}).</summary>
public interface IPythonPlugins
{
    Task<JsonElement> Call(string method, JsonObject args);
}

/// <summary>A Python exception from a plugin call; Message is Python's repr(exc), e.g. RuntimeError('vault down').</summary>
public sealed class PythonPluginException(string repr) : Exception(repr);
