using System.Buffers.Text;
using System.Net;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using AgentDesk.Contracts;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Http;
using Microsoft.Extensions.Logging;

namespace AgentDesk.Core.Host;

/// <summary>Runs one request the ops console sends to POST /api/&lt;op&gt;, with its JSON body; returns the reply's JSON text.</summary>
public delegate Task<string> WebHandler(string op, JsonElement args);

/// <summary>
/// The ops console: one page (Web.html) and POST /api/&lt;op&gt;, served by the core on http://127.0.0.1:&lt;port&gt; and
/// nowhere else; the port is last start's (web.port) while it is free, else any free one. Every request needs the key, persistent in web.key in the data folder so a bookmark keeps working:
/// ?k=&lt;key&gt; once, which sets an HttpOnly SameSite=Strict cookie and redirects the key out of the address bar.
/// A Host header other than 127.0.0.1:&lt;port&gt; is refused (DNS rebinding), and there is no CORS.
/// </summary>
public static class Web
{
    static readonly byte[] Page = ReadPage();

    /// <summary>Starts serving; returns the URL with the key, which ui:web_url and the tray's menu open.</summary>
    public static async Task<string> Start(string data, WebHandler handle)
    {
        var (key, portFile) = (Key(data), Path.Combine(data, "web.port"));
        int.TryParse(File.Exists(portFile) ? File.ReadAllText(portFile) : "", out var last); // the same port as last time, so a bookmark works
        var app = Build(key, handle, last);
        try { await app.StartAsync(); }
        catch (IOException) when (last != 0) { await app.DisposeAsync(); app = Build(key, handle); await app.StartAsync(); } // taken: any free port
        var url = app.Urls.First();
        File.WriteAllText(portFile, new Uri(url).Port.ToString());
        return $"{url}/?k={key}";
    }

    /// <summary>The key in web.key, made (32 random bytes) the first time.</summary>
    public static string Key(string data)
    {
        var file = Path.Combine(data, "web.key");
        if (File.Exists(file) && File.ReadAllText(file).Trim() is { Length: 43 } key) return key;
        File.WriteAllText(file, key = Base64Url.EncodeToString(RandomNumberGenerator.GetBytes(32)));
        return key;
    }

    public static WebApplication Build(string key, WebHandler handle, int port = 0)
    {
        var builder = WebApplication.CreateSlimBuilder(new WebApplicationOptions { Args = [], ContentRootPath = AppContext.BaseDirectory });
        builder.Logging.ClearProviders();
        builder.WebHost.ConfigureKestrel(k => k.Listen(IPAddress.Loopback, port));
        var app = builder.Build();
        app.Use(async (ctx, next) =>
        {
            var host = $"127.0.0.1:{ctx.Connection.LocalPort}";
            var origin = ctx.Request.Headers.Origin.ToString();
            if (ctx.Request.Host.Value != host || origin.Length > 0 && origin != $"http://{host}") { ctx.Response.StatusCode = 400; return; }
            if (ctx.Request.Query["k"].ToString() is { Length: > 0 } k && Same(k, key))
            {
                ctx.Response.Cookies.Append("k", key, new CookieOptions { HttpOnly = true, SameSite = SameSiteMode.Strict, MaxAge = TimeSpan.FromDays(400) });
                ctx.Response.Redirect(ctx.Request.Path);
                return;
            }
            if (!Same(ctx.Request.Cookies["k"], key)) { ctx.Response.StatusCode = 401; return; }
            ctx.Response.Headers.CacheControl = "no-store";
            await next();
        });
        app.MapGet("/", async ctx =>
        {
            ctx.Response.ContentType = "text/html; charset=utf-8";
            ctx.Response.Headers.ContentSecurityPolicy = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'";
            ctx.Response.Headers.XContentTypeOptions = "nosniff";
            await ctx.Response.Body.WriteAsync(Page);
        });
        app.MapPost("/api/{op}", async ctx =>
        {
            var op = (string)ctx.Request.RouteValues["op"]!;
            string text;
            try
            {
                using var body = ctx.Request.ContentLength > 0 ? await JsonDocument.ParseAsync(ctx.Request.Body) : JsonDocument.Parse("{}");
                text = await handle(op, body.RootElement);
            }
            catch (Exception e) when (e is ArgumentException or JsonException or InvalidOperationException) { text = Tools.Error(e.Message); }
            catch (Exception e) { Log.Warn($"web {op} failed: {e}"); text = Tools.Error($"internal error: {e.Message}"); }
            ctx.Response.ContentType = "application/json; charset=utf-8";
            await ctx.Response.WriteAsync(text);
        });
        return app;
    }

    static bool Same(string? a, string b) => a is not null && CryptographicOperations.FixedTimeEquals(Encoding.UTF8.GetBytes(a), Encoding.UTF8.GetBytes(b));

    static byte[] ReadPage()
    {
        using var s = typeof(Web).Assembly.GetManifestResourceStream("AgentDesk.Core.Host.Web.html")!;
        using var m = new MemoryStream();
        s.CopyTo(m);
        return m.ToArray();
    }
}
