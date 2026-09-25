"""Who a post is from -- and the reason every row used to say the same thing.

The board stores one author string per message, and that string was whatever
the caller passed. Every Claude Code session passed the obvious name, so the
"opened by" column was a wall of `claude` and the human could not tell which
agent was which. The identity is not the caller's to leave blank: a session may
say *what it wants to be called* (a role, a project), but it cannot post as
`claude`-the-harness, because that name carries no information and there is no
second agent it could distinguish.

So the author is resolved, not accepted. A non-anonymous requested name wins --
which is what keeps an identity posting as `builder` and the board posting as
`agentdesk` -- and anything else falls through to a name derived from the
session's own environment.

THE STORED STRING is small and parseable:

    <name>                      a caller that named itself -- "builder", "john"
    <harness>:<project>#<tag>   a session that did not

`#<tag>` is the first few characters of the harness session id, and it is what
makes two sessions of the same harness in the same project two authors rather
than one. A NAMED role deliberately does not carry the tag, and that is not an
oversight: the acknowledgement protocol is keyed to the author string, and the
core gives an identity a fresh session at every handoff. A rotating tag would strand
the replies that role still owes -- the ack would be queued against a name no
running session has, and the board would say out loud that nobody picked John's
reply up when the only reason is that the name changed. The role is the stable
identity; the tag is for sessions that have nothing else to be called.

TWO-TIER RENDERING, on purpose. The list column is narrow and must not push the
subject out of the window, so it shows label() -- a fitted short form that keeps
the session tag, because that is the part doing the distinguishing. The detail
pane shows describe(), which is the whole thing in words. Nothing is hidden;
it is only deferred to where there is room for it.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Optional

#: Names that identify nothing. A post under one of these is a post from
#: nobody in particular, which is exactly the state this module exists to
#: remove, so they are treated as "no name given" rather than as a choice.
ANONYMOUS = frozenset({
    "", "agent", "ai", "anthropic", "assistant", "claude", "claude-code",
    "claude_code", "unknown", "none", "null",
})

#: The widest label the "by" column holds at its current 80px, measured in the
#: app's own font rather than guessed: 11 characters of letterish text is ~66px
#: against the 58px its own "Opened by" heading already occupies. See
#: scripts/check_identity.py, which re-measures this rather than trusting it.
LABEL_MAX = 11


def is_anonymous(name: Optional[str]) -> bool:
    """True when this is not a name -- empty, or one of the generic ones."""
    return (name or "").strip().lower() in ANONYMOUS


def _harness(env) -> str:
    """What is running the session. Named from what the harness says about
    itself, never from the model: two sessions of different models are still
    two sessions of the same product, and the model name is not an identity."""
    explicit = (env.get("AGENTDESK_HARNESS") or "").strip()
    if explicit:
        return explicit
    if env.get("CLAUDECODE") or env.get("CLAUDE_CODE_ENTRYPOINT"):
        return "claude-code"
    ai_agent = (env.get("AI_AGENT") or "").strip()
    if ai_agent:
        return ai_agent.split("_")[0] or "agent"
    return "agent"


def _project(env, cwd) -> str:
    """Which project the session is in -- the working directory's name, which
    is what a reader recognises, not the whole path, which no column holds."""
    explicit = (env.get("AGENTDESK_PROJECT") or "").strip()
    if explicit:
        return explicit
    path = cwd if cwd is not None else os.getcwd()
    name = Path(path).name.strip()
    return name or "root"


def _tag(env) -> str:
    """The short session discriminator, or "" when the harness has no session
    id to give (a bare script, a test). Losing it costs distinguishability
    between two identical sessions; inventing one would cost the stability the
    acknowledgement protocol is keyed to, so an absent id is left absent."""
    raw = (env.get("AGENTDESK_SESSION") or env.get("CLAUDE_CODE_SESSION_ID")
           or "").strip()
    cleaned = "".join(ch for ch in raw if ch.isalnum())
    return cleaned[:4].lower()


def session_identity(env=None, cwd=None) -> str:
    """The name this session posts under when it has no name of its own."""
    env = os.environ if env is None else env
    base = f"{_harness(env)}:{_project(env, cwd)}"
    tag = _tag(env)
    return f"{base}#{tag}" if tag else base


def resolve(requested: Optional[str] = None, env=None, cwd=None) -> str:
    """The author a post is actually stored under.

    Order: what the caller asked for, then what the session was launched with
    (the core stamps AGENTDESK_AUTHOR so an identity survives a model that forgets
    the argument), then the derived session name. Only a name that identifies
    something is accepted at either of the first two steps.
    """
    env = os.environ if env is None else env
    if not is_anonymous(requested):
        return (requested or "").strip()
    stamped = (env.get("AGENTDESK_AUTHOR") or "").strip()
    if not is_anonymous(stamped):
        return stamped
    return session_identity(env, cwd)


def parse(author: str) -> tuple:
    """(name, harness, project, tag). harness is None for a plain name.

    Never raises: an author string the board already holds is data, and a
    string this module does not understand must render as itself rather than
    break the window that is reading it.
    """
    author = (author or "").strip()
    base, _, tag = author.partition("#")
    harness, sep, project = base.partition(":")
    if not sep:
        return author, None, None, tag or None
    return author, harness, project, tag or None


def _fit(text: str, limit: int = LABEL_MAX) -> str:
    return text if len(text) <= limit else text[: max(1, limit - 1)] + "…"


def label(author: str) -> str:
    """The short form for the list column, never longer than LABEL_MAX.

    For a derived name it keeps the session tag and shortens the project,
    because the tag is the part that tells two sessions apart and a truncated
    project is still recognisable while a truncated tag is noise.
    """
    author = (author or "").strip()
    _name, harness, project, tag = parse(author)
    if harness is None:
        return _fit(author)
    if not tag:
        return _fit(project)
    room = LABEL_MAX - len(tag) - 1
    if room < 2:
        return _fit(tag)
    return f"{_fit(project, room)}#{tag}"


def describe(author: str) -> str:
    """The full identity in words, for the pane that has room for it."""
    author = (author or "").strip()
    _name, harness, project, tag = parse(author)
    if harness is None:
        return author
    text = f"{harness} in {project}"
    return f"{text}, session {tag}" if tag else text
