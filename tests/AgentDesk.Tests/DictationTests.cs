using System.Text.Json;
using System.Text.Json.Nodes;
using AgentDesk.Core;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>ui:dictate's core half, over a stand-in for the Python plugin (the speech model and microphone are the plugin's).</summary>
public sealed class DictationTests
{
    sealed class Plugin(Func<string, JsonElement> answer) : IPythonPlugins
    {
        public readonly List<string> Calls = [];
        public Task<JsonElement> Call(string method, JsonObject args)
        {
            Calls.Add(method);
            return Task.FromResult(answer(method));
        }
    }

    [Fact]
    public async Task Start_poll_and_stop_go_to_the_plugin_and_its_answer_comes_back_as_is()
    {
        var plugin = new Plugin(_ => JsonDocument.Parse("""{"state":"listening","text":"hello there","error":null,"progress":0.0}""").RootElement);
        var dictation = new Dictation(plugin);
        foreach (var action in new[] { "start", "poll", "stop" })
            Assert.Equal("hello there", JsonDocument.Parse(await dictation.Run(action)).RootElement.GetProperty("text").GetString());
        Assert.Equal(["dictate.start", "dictate.poll", "dictate.stop"], plugin.Calls);
    }

    [Fact]
    public async Task Anything_else_is_refused_before_it_reaches_the_plugin()
    {
        var plugin = new Plugin(_ => throw new InvalidOperationException("must not be called"));
        await Assert.ThrowsAsync<ArgumentException>(() => new Dictation(plugin).Run("format-disk"));
        Assert.Empty(plugin.Calls);
    }

    [Fact]
    public async Task A_plugin_that_fails_is_reported_to_the_window_not_thrown()
    {
        foreach (Exception failure in new Exception[] { new PythonPluginException("PortAudioError('no microphone')"), new IOException("python plugin host exited") })
        {
            var reply = JsonDocument.Parse(await new Dictation(new Plugin(_ => throw failure)).Run("start")).RootElement;
            Assert.Equal("done", reply.GetProperty("state").GetString());
            Assert.Equal(failure.Message, reply.GetProperty("error").GetString());
        }
    }
}
