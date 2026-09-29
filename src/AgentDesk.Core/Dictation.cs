using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>ui:dictate: the window's Ctrl+D. Local speech-to-text in the Python plugin (agentdesk/dictate.py): "start" begins
/// listening, "poll" returns {state, text, error, progress} for the words so far, "stop" ends it. "arm" (a box has focus) opens the
/// microphone and keeps its last 2 seconds so "start" catches what was said just before it; "disarm" lets the microphone go. A plugin that is missing its
/// microphone or model answers with an error, which the window shows; it is never a crash.</summary>
public sealed class Dictation(IPythonPlugins py)
{
    public async Task<string> Run(string action)
    {
        if (action is not ("start" or "poll" or "stop" or "arm" or "disarm")) throw new ArgumentException("action is start, poll, stop, arm or disarm");
        try { return (await py.Call($"dictate.{action}", new JsonObject())).GetRawText(); }
        catch (Exception e) when (e is PythonPluginException or IOException)
        {
            return new JsonObject { ["state"] = "done", ["text"] = "", ["error"] = e.Message, ["progress"] = 0 }.ToJsonString(Wire.Indented);
        }
    }
}
