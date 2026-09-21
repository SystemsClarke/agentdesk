"""A notify.py sink that posts to John's Teams chat via an existing webhook.

Work item #129, the outbound half of "ping me on Teams so I can answer inline."
John's own words: "build out a feature to allow the agent board to message me
on Teams... for questions it will ping me on Teams so I can answer." This
module is that ping. It is NOT the inline-answer half -- see the investigation
report on thread 129 for why that half is a separate, larger piece of work
with a real security decision in it, not a few more lines here.

Reuses the webhook John already has, rather than asking him to create a
second one for the same destination: `notify-teams.ps1` (used by the
`teams-notifier` Claude agent) already POSTs `{"text": ...}` to a Power
Automate flow that lands the message in his Teams self-chat, and
`webhook-url.txt` next to it holds the URL. Pointing a second consumer at the
same file is the same trust boundary as the first (both run as John, on his
own machine, reading a file under his own home directory) and it means this
sink works the moment this file exists -- no new Teams-side setup, no second
secret to store or rotate. If John would rather AgentDesk own an independent
webhook, repointing `_WEBHOOK_PATH` is the whole change.

Declared, not scanned, per notify.py's own rule -- `notify._DECLARED_SINKS`
lists this module by name; nothing here runs by virtue of merely existing in
this directory.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.request
from pathlib import Path

# The same file notify-teams.ps1 reads, so a webhook John already created for
# this exact destination is reused rather than duplicated. Not under
# %LOCALAPPDATA%\AgentDesk: this long predates AgentDesk and lives where every
# other Claude-side Teams notifier already looks for it.
_WEBHOOK_PATH = Path.home() / ".claude" / "teams-notify" / "webhook-url.txt"

_TIMEOUT_SECONDS = 10


def _webhook_url() -> str:
    """The configured webhook URL. Raises if it is not there to be found --
    see the module docstring on why a raise here is correct, not a bug: a
    sink that cannot send must say so and get disabled, not pretend to
    succeed."""
    text = _WEBHOOK_PATH.read_text(encoding="utf-8").strip()
    if not text:
        raise ValueError(f"{_WEBHOOK_PATH} exists but is empty")
    return text


class _TeamsSink:
    def send(self, title: str, message: str) -> None:
        """POST one message to Teams. Raises on any failure -- caught and
        turned into a disable by notify._notify_sinks, per its own contract.

        The flow behind the webhook expects a bare `{"text": ...}` body (the
        same shape notify-teams.ps1 already sends), so title and message are
        folded into one field rather than assuming the flow understands a
        richer schema it was never built for.
        """
        url = _webhook_url()
        text = f"**{title}**\n\n{message}" if title else message
        payload = json.dumps({"text": text}).encode("utf-8")
        req = urllib.request.Request(
            url, data=payload, method="POST",
            headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=_TIMEOUT_SECONDS) as resp:
            if resp.status >= 300:
                raise urllib.error.HTTPError(
                    url, resp.status, f"Teams webhook returned {resp.status}",
                    resp.headers, None)


SINK = _TeamsSink()
