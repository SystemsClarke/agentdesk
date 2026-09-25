using System.Net;
using System.Text;
using System.Text.Json;
using AgentDesk.Contracts;
using AgentDesk.Core;
using AgentDesk.Core.Host;
using Microsoft.AspNetCore.Builder;

namespace AgentDesk.Tests;

/// <summary>The ops console (Host/Web.cs) on a real loopback port, in front of a fake core.</summary>
public sealed class WebTests : IAsyncLifetime
{
    const string Key = "k3y-for-tests-only_0123456789abcdefghijklmn";
    readonly List<string> calls = [];
    readonly HttpClient http = new(new HttpClientHandler { UseCookies = false, AllowAutoRedirect = false });
    WebApplication app = null!;
    string root = "";

    public async Task InitializeAsync()
    {
        app = Web.Build(Key, (op, args) =>
        {
            lock (calls) calls.Add($"{op} {args.GetRawText()}");
            return Task.FromResult(op == "identity_list" ? """{"identities": [{"name": "builder", "state": "running"}]}""" : Tools.Error($"unknown request: {op}"));
        });
        await app.StartAsync();
        root = app.Urls.First();
    }

    public async Task DisposeAsync() { http.Dispose(); await app.DisposeAsync(); }

    Task<HttpResponseMessage> Send(HttpMethod method, string path, string? cookie = null, string? host = null, string? body = null, string? origin = null)
    {
        var req = new HttpRequestMessage(method, root + path);
        if (cookie is not null) req.Headers.Add("Cookie", $"k={cookie}");
        if (host is not null) req.Headers.Host = host;
        if (origin is not null) req.Headers.Add("Origin", origin);
        if (body is not null) req.Content = new StringContent(body, Encoding.UTF8, "application/json");
        return http.SendAsync(req);
    }

    [Fact]
    public void Listens_on_loopback_only() => Assert.StartsWith("http://127.0.0.1:", root);

    [Fact]
    public async Task Every_request_needs_the_key()
    {
        Assert.Equal(HttpStatusCode.Unauthorized, (await Send(HttpMethod.Get, "/")).StatusCode);
        Assert.Equal(HttpStatusCode.Unauthorized, (await Send(HttpMethod.Post, "/api/identity_list", body: "{}")).StatusCode);
        Assert.Equal(HttpStatusCode.Unauthorized, (await Send(HttpMethod.Get, "/?k=wrong")).StatusCode);
        Assert.Equal(HttpStatusCode.Unauthorized, (await Send(HttpMethod.Post, "/api/identity_list", cookie: "wrong", body: "{}")).StatusCode);
        Assert.Empty(calls);
    }

    [Fact]
    public async Task A_foreign_host_header_or_origin_is_refused_even_with_the_key()
    {
        var port = new Uri(root).Port;
        foreach (var host in new[] { $"localhost:{port}", "attacker.example", $"attacker.example:{port}" })
        {
            Assert.Equal(HttpStatusCode.BadRequest, (await Send(HttpMethod.Get, "/", cookie: Key, host: host)).StatusCode);
            Assert.Equal(HttpStatusCode.BadRequest, (await Send(HttpMethod.Get, $"/?k={Key}", host: host)).StatusCode);
        }
        Assert.Equal(HttpStatusCode.BadRequest, (await Send(HttpMethod.Post, "/api/identity_list", cookie: Key, body: "{}", origin: "http://attacker.example")).StatusCode);
        Assert.Empty(calls);
    }

    [Fact]
    public async Task The_key_in_the_url_becomes_a_strict_httponly_cookie_and_leaves_the_address_bar()
    {
        var first = await Send(HttpMethod.Get, $"/?k={Key}");
        Assert.Equal(HttpStatusCode.Redirect, first.StatusCode);
        Assert.Equal("/", first.Headers.Location!.OriginalString);
        var cookie = Assert.Single(first.Headers.GetValues("Set-Cookie"));
        Assert.StartsWith($"k={Key};", cookie);
        Assert.Contains("httponly", cookie, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("samesite=strict", cookie, StringComparison.OrdinalIgnoreCase);

        var page = await Send(HttpMethod.Get, "/", cookie: Key);
        Assert.Equal(HttpStatusCode.OK, page.StatusCode);
        Assert.Contains("<h1>AgentDesk ops console</h1>", await page.Content.ReadAsStringAsync());
        Assert.Contains("default-src 'none'", page.Headers.GetValues("Content-Security-Policy").Single());
        Assert.False(page.Headers.Contains("Access-Control-Allow-Origin"));
    }

    [Fact]
    public async Task Api_calls_reach_the_core_with_their_arguments()
    {
        var r = await Send(HttpMethod.Post, "/api/identity_list", cookie: Key, body: """{"x": 1}""", origin: root);
        Assert.Equal(HttpStatusCode.OK, r.StatusCode);
        Assert.Equal("builder", JsonDocument.Parse(await r.Content.ReadAsStringAsync()).RootElement.GetProperty("identities")[0].GetProperty("name").GetString());
        Assert.Equal(["identity_list {\"x\": 1}"], calls);
        Assert.Contains("\"error\"", await (await Send(HttpMethod.Post, "/api/identity_list", cookie: Key, body: "{not json")).Content.ReadAsStringAsync());
    }

    [Fact]
    public void The_key_persists_in_the_data_folder()
    {
        var data = Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), $"web-{Guid.NewGuid():N}")).FullName;
        var key = Web.Key(data);
        Assert.Equal(43, key.Length); // 32 bytes, base64url
        Assert.Equal(key, Web.Key(data));
        Assert.NotEqual(key, Web.Key(Directory.CreateDirectory(data + "2").FullName));
    }

    [Fact]
    public async Task A_dev_build_reports_not_installed_and_never_restarts()
    {
        Setup.Run(); // as the core starts: Velopack finds (here: fails to find) its install; no hook argument, so it returns
        foreach (var args in new[] { "{}", """{"apply": true}""" })
        {
            var text = await Setup.Update(new Args(JsonDocument.Parse(args).RootElement), "test");
            var doc = JsonDocument.Parse(text).RootElement;
            Assert.True(doc.TryGetProperty("installed", out var installed), text);
            Assert.False(installed.GetBoolean());
            Assert.False(doc.GetProperty("restarting").GetBoolean());
            Assert.False(doc.GetProperty("downloaded").GetBoolean());
            Assert.Equal(JsonValueKind.Null, doc.GetProperty("pending").ValueKind);
        }
        Assert.Throws<InvalidOperationException>(() => new Args(JsonDocument.Parse("""{"apply": "yes"}""").RootElement).Bool("apply", false));
    }

    [Fact]
    public void Log_tail_returns_the_newest_lines()
    {
        var marker = Guid.NewGuid().ToString();
        Log.Info(marker);
        Assert.Contains(Log.Tail(1000), l => l.EndsWith(marker));
        Assert.Single(Log.Tail(1));
    }
}
