using System.Buffers.Text;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using AgentDesk.Contracts;

namespace AgentDesk.Core.Host;

/// <summary>Runs one request the ops console sends to POST /api/&lt;op&gt;, with its JSON body; returns the reply's JSON text.</summary>
public delegate Task<string> WebHandler(string op, JsonElement args);

/// <summary>
/// The ops console: one page (Web.html) and POST /api/&lt;op&gt;, served by the core on http://127.0.0.1:&lt;port&gt; and
/// nowhere else; the port is last start's (web.port) while it is free, else any free one. Every request needs the key, persistent in web.key in the data folder so a bookmark keeps working:
/// ?k=&lt;key&gt; once, which sets an HttpOnly SameSite=Strict cookie and redirects the key out of the address bar.
/// A Host header other than 127.0.0.1:&lt;port&gt; is refused (DNS rebinding), and there is no CORS.
/// Plain HTTP/1.1 over a TcpListener, one request per connection: the console is one page and a handful of calls, and ASP.NET (Kestrel) was
/// most of the core's size for it.
/// </summary>
public static class Web
{
    static readonly byte[] Page = ReadPage();

    /// <summary>Starts serving; returns the URL with the key, which ui:web_url and the tray's menu open.</summary>
    public static async Task<string> Start(string data, WebHandler handle)
    {
        var (key, portFile) = (Key(data), Path.Combine(data, "web.port"));
        int.TryParse(File.Exists(portFile) ? File.ReadAllText(portFile) : "", out var last); // the same port as last time, so a bookmark works
        var server = Build(key, handle, last);
        try { server.Start(); }
        catch (SocketException) when (last != 0) { server = Build(key, handle); server.Start(); } // taken: any free port
        File.WriteAllText(portFile, server.Port.ToString());
        await Task.CompletedTask;
        return $"{server.Url}/?k={key}";
    }

    /// <summary>The key in web.key, made (32 random bytes) the first time.</summary>
    public static string Key(string data)
    {
        var file = Path.Combine(data, "web.key");
        if (File.Exists(file) && File.ReadAllText(file).Trim() is { Length: 43 } key) return key;
        File.WriteAllText(file, key = Base64Url.EncodeToString(RandomNumberGenerator.GetBytes(32)));
        return key;
    }

    public static WebServer Build(string key, WebHandler handle, int port = 0) => new(key, handle, port);

    static bool Same(string? a, string b) => a is not null && CryptographicOperations.FixedTimeEquals(Encoding.UTF8.GetBytes(a), Encoding.UTF8.GetBytes(b));

    static byte[] ReadPage()
    {
        using var s = typeof(Web).Assembly.GetManifestResourceStream("AgentDesk.Core.Host.Web.html")!;
        using var m = new MemoryStream();
        s.CopyTo(m);
        return m.ToArray();
    }

    /// <summary>One request, parsed.</summary>
    sealed record Request(string Method, string Path, string Query, Dictionary<string, string> Headers, byte[] Body);

    public sealed class WebServer : IDisposable
    {
        const int MaxHeaderBytes = 16 * 1024, MaxBodyBytes = 1024 * 1024, MaxConnections = 32;
        readonly string key;
        readonly WebHandler handle;
        readonly TcpListener listener;
        readonly CancellationTokenSource stop = new();
        readonly SemaphoreSlim slots = new(MaxConnections);

        internal WebServer(string key, WebHandler handle, int port)
        {
            (this.key, this.handle) = (key, handle);
            listener = new TcpListener(IPAddress.Loopback, port);
        }

        public int Port { get; private set; }

        public string Url => $"http://127.0.0.1:{Port}";

        /// <summary>Begins listening (throws a SocketException when the port is taken) and accepting in the background.</summary>
        public void Start()
        {
            listener.Start();
            Port = ((IPEndPoint)listener.LocalEndpoint).Port;
            _ = Task.Run(Accept);
        }

        public void Dispose()
        {
            stop.Cancel();
            listener.Stop();
        }

        async Task Accept()
        {
            while (!stop.IsCancellationRequested)
            {
                TcpClient client;
                try { client = await listener.AcceptTcpClientAsync(stop.Token); }
                catch (Exception e) when (e is OperationCanceledException or ObjectDisposedException or SocketException) { return; }
                if (!await slots.WaitAsync(0)) { client.Dispose(); continue; } // a flood is refused, not queued
                _ = Task.Run(async () =>
                {
                    try { await Serve(client); }
                    catch (Exception e) when (e is IOException or SocketException or OperationCanceledException or ObjectDisposedException) { }
                    catch (Exception e) { Log.Warn($"web request failed: {e}"); }
                    finally { slots.Release(); client.Dispose(); }
                });
            }
        }

        async Task Serve(TcpClient client)
        {
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(stop.Token);
            timeout.CancelAfter(TimeSpan.FromSeconds(30)); // a client that never finishes its request, or never reads the reply, is dropped
            var stream = client.GetStream();
            var request = await Read(stream, timeout.Token);
            (int Status, List<(string, string)> Headers, byte[] Body) reply = request is null ? (400, [], []) : await Route(request);
            var (status, headers, body) = reply;
            var head = new StringBuilder($"HTTP/1.1 {status} {Reason(status)}\r\n");
            foreach (var (k, v) in headers) head.Append(k).Append(": ").Append(v).Append("\r\n");
            head.Append("Content-Length: ").Append(body.Length).Append("\r\nConnection: close\r\n\r\n");
            await stream.WriteAsync(Encoding.ASCII.GetBytes(head.ToString()), timeout.Token);
            if (body.Length > 0) await stream.WriteAsync(body, timeout.Token);
        }

        /// <summary>Null for anything that is not a well-formed, small request.</summary>
        static async Task<Request?> Read(NetworkStream stream, CancellationToken ct)
        {
            var buffer = new byte[MaxHeaderBytes];
            var have = 0;
            int end;
            while ((end = IndexOfHeaderEnd(buffer, have)) < 0)
            {
                if (have == buffer.Length) return null; // headers too large
                var n = await stream.ReadAsync(buffer.AsMemory(have), ct);
                if (n == 0) return null;
                have += n;
            }
            var lines = Encoding.ASCII.GetString(buffer, 0, end).Split("\r\n");
            if (lines[0].Split(' ') is not [var method, var target, var version] || !version.StartsWith("HTTP/1.")) return null;
            var headers = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
            foreach (var line in lines.Skip(1))
            {
                if (line.IndexOf(':') is var c and > 0) headers[line[..c].Trim()] = line[(c + 1)..].Trim();
                else return null;
            }
            if (headers.ContainsKey("Transfer-Encoding")) return null; // the console sends fixed-length bodies only
            var length = headers.TryGetValue("Content-Length", out var cl) ? (int.TryParse(cl, out var l) && l is >= 0 and <= MaxBodyBytes ? l : -1) : 0;
            if (length < 0) return null;
            var bodyStart = end + 4;
            var body = new byte[length];
            var already = Math.Min(length, have - bodyStart);
            Array.Copy(buffer, bodyStart, body, 0, already);
            for (var got = already; got < length;)
            {
                var n = await stream.ReadAsync(body.AsMemory(got), ct);
                if (n == 0) return null;
                got += n;
            }
            var q = target.IndexOf('?');
            return new(method, q < 0 ? target : target[..q], q < 0 ? "" : target[(q + 1)..], headers, body);
        }

        static int IndexOfHeaderEnd(byte[] b, int have)
        {
            for (var i = 3; i < have; i++)
                if (b[i] == '\n' && b[i - 1] == '\r' && b[i - 2] == '\n' && b[i - 3] == '\r') return i - 3;
            return -1;
        }

        async Task<(int Status, List<(string, string)> Headers, byte[] Body)> Route(Request r)
        {
            var host = $"127.0.0.1:{Port}";
            var origin = r.Headers.GetValueOrDefault("Origin") ?? "";
            if (r.Headers.GetValueOrDefault("Host") != host || origin.Length > 0 && origin != $"http://{host}") return (400, [], []);
            if (Query(r.Query, "k") is { Length: > 0 } k && Same(k, key))
                return (302, [("Location", r.Path), ("Set-Cookie", $"k={key}; max-age={TimeSpan.FromDays(400).TotalSeconds:0}; path=/; samesite=strict; httponly")], []);
            if (!Same(Cookie(r.Headers.GetValueOrDefault("Cookie"), "k"), key)) return (401, [], []);
            List<(string, string)> json = [("Cache-Control", "no-store"), ("Content-Type", "application/json; charset=utf-8")];
            if (r.Path == "/")
                return r.Method == "GET"
                    ? (200, [("Cache-Control", "no-store"), ("Content-Type", "text/html; charset=utf-8"),
                        ("Content-Security-Policy", "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'"),
                        ("X-Content-Type-Options", "nosniff")], Page)
                    : (405, [("Allow", "GET")], []);
            if (r.Path.StartsWith("/api/") && r.Path.Length > 5 && !r.Path[5..].Contains('/'))
            {
                if (r.Method != "POST") return (405, [("Allow", "POST")], []);
                var op = Uri.UnescapeDataString(r.Path[5..]);
                string text;
                try
                {
                    using var body = r.Body.Length > 0 ? JsonDocument.Parse(r.Body) : JsonDocument.Parse("{}");
                    text = await handle(op, body.RootElement);
                }
                catch (Exception e) when (e is ArgumentException or JsonException or InvalidOperationException) { text = Tools.Error(e.Message); }
                catch (Exception e) { Log.Warn($"web {op} failed: {e}"); text = Tools.Error($"internal error: {e.Message}"); }
                return (200, json, Encoding.UTF8.GetBytes(text));
            }
            return (404, [], []);
        }

        /// <summary>The first value of a query-string parameter, decoded.</summary>
        static string? Query(string query, string name)
        {
            foreach (var pair in query.Split('&', StringSplitOptions.RemoveEmptyEntries))
                if (pair.IndexOf('=') is var e and > 0 && Uri.UnescapeDataString(pair[..e]) == name) return Uri.UnescapeDataString(pair[(e + 1)..].Replace('+', ' '));
            return null;
        }

        static string? Cookie(string? header, string name)
        {
            foreach (var part in (header ?? "").Split(';', StringSplitOptions.TrimEntries))
                if (part.IndexOf('=') is var e and > 0 && part[..e] == name) return part[(e + 1)..];
            return null;
        }

        static string Reason(int status) => status switch { 200 => "OK", 302 => "Found", 400 => "Bad Request", 401 => "Unauthorized", 404 => "Not Found", 405 => "Method Not Allowed", _ => "Error" };
    }
}
