using System.Text;
using System.Text.Json;
using AgentDesk.Contracts;

namespace AgentDesk.Cli;

/// <summary>
/// The agentdesk MCP server: lists the board's tools and relays each call to the core. MCP over stdio is newline-delimited JSON-RPC 2.0, and
/// the server needs five things (initialize, notifications/initialized, ping, tools/list, tools/call), so it is written out here instead of
/// carrying the MCP SDK, which was most of agentdesk.exe's size and starts in every Claude Code session.
/// </summary>
static class Mcp
{
    /// <summary>The newest protocol version this server speaks; a client that asks for another is answered with the nearest it knows of.</summary>
    const string Latest = "2025-06-18";

    static readonly string[] Known = ["2024-11-05", "2025-03-26", "2025-06-18"];

    public static async Task Serve(CoreConnection core, TextReader input, TextWriter output)
    {
        var gate = new SemaphoreSlim(1, 1);
        var running = new List<Task>();
        async Task Send(string json)
        {
            await gate.WaitAsync();
            try { await output.WriteLineAsync(json); await output.FlushAsync(); }
            finally { gate.Release(); }
        }
        while (await input.ReadLineAsync() is { } line)
        {
            if (line.Trim().Length == 0) continue;
            JsonDocument doc;
            try { doc = JsonDocument.Parse(line); }
            catch (JsonException) { await Send(Error(null, -32700, "parse error")); continue; }
            var request = doc; // a request is answered on its own task: one slow tool call must not hold up a ping
            running.Add(Task.Run(async () =>
            {
                using (request)
                {
                    try { if (await Handle(core, request.RootElement) is { } reply) await Send(reply); }
                    catch (Exception e) { await Send(Error(Id(request.RootElement), -32603, "internal error: " + e.Message)); }
                }
            }));
            running.RemoveAll(t => t.IsCompleted);
        }
        await Task.WhenAll(running); // stdin closed: let the calls in flight answer
    }

    public static Task Serve(CoreConnection core) => Serve(core, new StreamReader(Console.OpenStandardInput(), new UTF8Encoding(false)),
        new StreamWriter(Console.OpenStandardOutput(), new UTF8Encoding(false)));

    /// <summary>The reply to one message, or null for a notification (a message with no id gets none).</summary>
    static async Task<string?> Handle(CoreConnection core, JsonElement message)
    {
        if (message.ValueKind != JsonValueKind.Object || !message.TryGetProperty("method", out var m) || m.GetString() is not { } method)
            return message.ValueKind == JsonValueKind.Object && message.TryGetProperty("id", out _) && !message.TryGetProperty("result", out _) && !message.TryGetProperty("error", out _)
                ? Error(Id(message), -32600, "invalid request") : null; // a response to a request we never send is ignored
        var id = Id(message);
        if (id is null) return null; // notifications/initialized, notifications/cancelled, ...
        var args = message.TryGetProperty("params", out var p) ? p : default;
        switch (method)
        {
            case "initialize":
                return Result(id, w =>
                {
                    var asked = args.ValueKind == JsonValueKind.Object && args.TryGetProperty("protocolVersion", out var v) ? v.GetString() : null;
                    w.WriteString("protocolVersion", asked is not null && Known.Contains(asked) ? asked : Latest);
                    w.WriteStartObject("capabilities");
                    w.WriteStartObject("tools");
                    w.WriteEndObject();
                    w.WriteEndObject();
                    w.WriteStartObject("serverInfo");
                    w.WriteString("name", "agentdesk");
                    w.WriteString("version", "2.0");
                    w.WriteEndObject();
                    w.WriteString("instructions", Tools.Instructions);
                });
            case "ping":
                return Result(id, _ => { });
            case "tools/list":
                return Result(id, w =>
                {
                    w.WriteStartArray("tools");
                    foreach (var t in Tools.All)
                    {
                        w.WriteStartObject();
                        w.WriteString("name", t.Name);
                        w.WriteString("description", t.Description);
                        w.WritePropertyName("inputSchema");
                        w.WriteRawValue(t.InputSchema);
                        w.WriteEndObject();
                    }
                    w.WriteEndArray();
                });
            case "tools/call":
                if (args.ValueKind != JsonValueKind.Object || !args.TryGetProperty("name", out var n) || n.GetString() is not { } name)
                    return Error(id, -32602, "tools/call needs a tool name");
                if (!Tools.All.Any(t => t.Name == name)) return Error(id, -32602, $"unknown tool: {name}");
                JsonElement? arguments = args.TryGetProperty("arguments", out var a) && a.ValueKind == JsonValueKind.Object ? a.Clone() : null;
                string text;
                bool failed = false;
                try { text = await core.Call(name, arguments); }
                catch (Exception e) when (e is IOException or TimeoutException or InvalidOperationException)
                {
                    (text, failed) = ($"The AgentDesk core did not answer: {e.Message}", true);
                }
                return Result(id, w =>
                {
                    w.WriteStartArray("content");
                    w.WriteStartObject();
                    w.WriteString("type", "text");
                    w.WriteString("text", text);
                    w.WriteEndObject();
                    w.WriteEndArray();
                    if (failed) w.WriteBoolean("isError", true);
                });
            default:
                return Error(id, -32601, $"method not found: {method}");
        }
    }

    /// <summary>The request's id exactly as sent (a number or a string), or null when there is none.</summary>
    static string? Id(JsonElement message) =>
        message.ValueKind == JsonValueKind.Object && message.TryGetProperty("id", out var id) && id.ValueKind is JsonValueKind.Number or JsonValueKind.String ? id.GetRawText() : null;

    static string Result(string id, Action<Utf8JsonWriter> body)
    {
        using var stream = new MemoryStream();
        using (var w = new Utf8JsonWriter(stream))
        {
            w.WriteStartObject();
            w.WriteString("jsonrpc", "2.0");
            w.WritePropertyName("id");
            w.WriteRawValue(id);
            w.WriteStartObject("result");
            body(w);
            w.WriteEndObject();
            w.WriteEndObject();
        }
        return Encoding.UTF8.GetString(stream.ToArray());
    }

    static string Error(string? id, int code, string message)
    {
        using var stream = new MemoryStream();
        using (var w = new Utf8JsonWriter(stream))
        {
            w.WriteStartObject();
            w.WriteString("jsonrpc", "2.0");
            w.WritePropertyName("id");
            if (id is null) w.WriteNullValue(); else w.WriteRawValue(id);
            w.WriteStartObject("error");
            w.WriteNumber("code", code);
            w.WriteString("message", message);
            w.WriteEndObject();
            w.WriteEndObject();
        }
        return Encoding.UTF8.GetString(stream.ToArray());
    }
}
