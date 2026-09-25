using System.Text.Json;
using System.Text.Json.Nodes;

namespace AgentDesk.Contracts;

public sealed record ToolInfo(string Name, string Description, string InputSchema);

public static partial class Tools
{
    /// <summary>Run one tool call against the board. Bad arguments become an {"error": ...} document, as in Python.</summary>
    public static async Task<string> Dispatch(IAgentBoard board, Caller caller, string tool, JsonElement args)
    {
        try { return await Call(board, caller, tool, new Args(args)); }
        catch (ArgumentException e) { return Error(e.Message); }
    }

    public static string Error(string message) =>
        new JsonObject { ["error"] = message }.ToJsonString(Wire.Indented);
}

/// <summary>Typed reads over a tool call's JSON arguments.</summary>
public readonly struct Args(JsonElement json)
{
    bool Has(string name, out JsonElement v)
    {
        v = default;
        return json.ValueKind == JsonValueKind.Object && json.TryGetProperty(name, out v) && v.ValueKind != JsonValueKind.Null;
    }

    JsonElement Need(string name) => Has(name, out var v) ? v : throw new ArgumentException($"missing required argument: {name}");

    public string String(string name) => Need(name).GetString()!;
    public string String(string name, string fallback) => Has(name, out var v) ? v.GetString()! : fallback;
    public string? StringOrNull(string name) => Has(name, out var v) ? v.GetString() : null;
    public int Int(string name) => Need(name).GetInt32();
    public int Int(string name, int fallback) => Has(name, out var v) ? v.GetInt32() : fallback;
    public int? IntOrNull(string name) => Has(name, out var v) ? v.GetInt32() : null;
    public double? DoubleOrNull(string name) => Has(name, out var v) ? v.GetDouble() : null;
    public bool Bool(string name, bool fallback) => Has(name, out var v) ? v.GetBoolean() : fallback;
    public bool? BoolOrNull(string name) => Has(name, out var v) ? v.GetBoolean() : null;

    public Dictionary<string, object?>? DictOrNull(string name) =>
        Has(name, out var v) && v.ValueKind == JsonValueKind.Object
            ? v.EnumerateObject().ToDictionary(p => p.Name, p => (object?)p.Value.Clone())
            : null;
}
