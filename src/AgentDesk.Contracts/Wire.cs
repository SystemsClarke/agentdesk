using System.Text.Encodings.Web;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace AgentDesk.Contracts;

/// <summary>Shim → core: one JSON object per line.</summary>
public sealed record Request(int Id, string Tool, JsonElement Args, Caller Caller);

/// <summary>Core → shim: the tool's JSON text for the request with the same id.</summary>
public sealed record Response(int Id, string Text);

[JsonSourceGenerationOptions(PropertyNamingPolicy = JsonKnownNamingPolicy.CamelCase)]
[JsonSerializable(typeof(Request))]
[JsonSerializable(typeof(Response))]
public sealed partial class WireJson : JsonSerializerContext;

/// <summary>
/// The pipe protocol: newline-delimited JSON with source-generated serializers. No reflection,
/// so the core and the shim compile Native AOT.
/// </summary>
public static class Wire
{
    /// <summary>How tool results are written: indented, non-ASCII kept as-is (Python's ensure_ascii=False).</summary>
    public static readonly JsonSerializerOptions Indented = new()
    {
        WriteIndented = true,
        Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping,
    };

    public static async Task<T?> Read<T>(StreamReader r, System.Text.Json.Serialization.Metadata.JsonTypeInfo<T> type, CancellationToken ct)
        => await r.ReadLineAsync(ct) is { } line ? JsonSerializer.Deserialize(line, type) : default;

    public static async Task Write<T>(StreamWriter w, T value, System.Text.Json.Serialization.Metadata.JsonTypeInfo<T> type, SemaphoreSlim gate)
    {
        var line = JsonSerializer.Serialize(value, type);
        await gate.WaitAsync();
        try { await w.WriteLineAsync(line); await w.FlushAsync(); }
        finally { gate.Release(); }
    }
}
