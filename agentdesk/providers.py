"""Which provider a spawned crew agent runs on.

THE BUG THIS EXISTS FOR. `crew.py` built its child environment with
``child_env = dict(os.environ)``, so every crew agent inherited the *parent
session's* provider override -- ``ANTHROPIC_BASE_URL``, ``ANTHROPIC_AUTH_TOKEN``,
``ANTHROPIC_MODEL``. A crew started from a session pinned to another provider
therefore sent Anthropic model names to that provider and every run died the
same way::

    API Error: 400 FireRouter cannot route 'claude-haiku-4-5':
    no usable anthropic credential
    is_error=True   input_tokens=0   cost_usd=0

That string is the ``[claude-code:unrecognized_model]`` warning this repo's
crew docstring already recorded as harmless stderr noise. It was never
harmless -- it was the symptom, and because it arrives on stderr next to a
well-formed zero-token JSON envelope on stdout, it read as "the model answered
badly" rather than "the request never arrived".

The same bug was in ``ladder/engines/cli_engine.py``, with the same fix.

THE CONTRACT IS THE FILE, NOT THIS MODULE. Profiles live in
``~/.claude/providers.json`` and are shared with Ladder, which carries the
reference implementation at ``ladder/providers.py``. Edit the JSON to add or
change a profile; these ~40 lines only resolve it. Keeping the list in one
place is the point -- two copies of "which providers exist" would drift the way
this bug's two copies of the environment did.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Optional

CONFIG_PATH = Path(
    os.environ.get("CLAUDE_PROVIDERS_FILE")
    or (Path(os.environ.get("USERPROFILE") or Path.home()) / ".claude" / "providers.json")
)

#: Everything that redirects a `claude` child away from its authenticated
#: harness default. Mirrors ladder/providers.py; scrub all of it, always.
#: ANTHROPIC_API_KEY belongs here even though it does not change *where* a
#: request goes: it takes precedence over the claude.ai login, silently
#: downgrading a subscription run to key-based auth.
OVERRIDE_VARS = (
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
)

#: The profile a crew runs on when nothing names one. The Claude subscription,
#: because that is what an agentic worker needs: the local tier returns empty
#: answers under the full harness (measured 2026-09-19). Ladder is how the crew
#: gets cheap *sub-work*, not how it runs itself.
DEFAULT_PROFILE = "claude"

#: Used when the config cannot be read at all. It names a model deliberately:
#: a settings.json `model` pin outranks a scrubbed environment, so a
#: subscription fallback that named no model would keep running the pinned
#: provider's model and fail. A safety net that reproduces the bug is not one.
_FALLBACK_SPEC: dict[str, Any] = {"model": "claude-sonnet-5"}


def _load() -> dict[str, Any]:
    try:
        data = json.loads(CONFIG_PATH.read_text(encoding="utf-8"))
        if isinstance(data, dict) and isinstance(data.get("profiles"), dict):
            return data
    except Exception:  # noqa: BLE001 - absent, unreadable, or not JSON
        pass
    return {"profiles": {}}


def env_for(profile: Optional[str] = None,
            base: Optional[dict] = None) -> dict[str, str]:
    """The environment to spawn a crew agent under `profile`.

    Never raises. An unknown profile name, an unreadable config, or a missing
    token all resolve to *something that runs* -- a typo should cost a cheap
    wrong-provider call, not a crew that cannot start.
    """
    cfg = _load()
    profiles = cfg.get("profiles", {})
    name = profile if (profile and profile in profiles) else DEFAULT_PROFILE
    spec = profiles.get(name) or _FALLBACK_SPEC

    env = dict(os.environ if base is None else base)
    for var in OVERRIDE_VARS:
        env.pop(var, None)

    # A custom endpoint needs a credential; the subscription does not. Note the
    # asymmetry -- the token is set only alongside a base_url, but the MODEL is
    # set always. That asymmetry is the second half of the bug: a
    # settings.json `model` pin outranks a scrubbed environment, so scrubbing
    # alone still ran the pinned Fireworks model and failed with "There's an
    # issue with the selected model". Measured 2026-09-19.
    if spec.get("base_url"):
        env["ANTHROPIC_BASE_URL"] = str(spec["base_url"])
        if spec.get("auth_token"):
            env["ANTHROPIC_AUTH_TOKEN"] = str(spec["auth_token"])
        elif spec.get("auth_token_env"):
            src = os.environ.get(str(spec["auth_token_env"]))
            if src:
                env["ANTHROPIC_AUTH_TOKEN"] = src
    if spec.get("model"):
        env["ANTHROPIC_MODEL"] = str(spec["model"])
    return env


def effort_for(profile: Optional[str] = None) -> Optional[str]:
    """The `--effort` level a profile asks for, or None to leave the CLI default.

    A CLI flag rather than part of env_for's environment: effort is not an
    endpoint setting, and a profile pointed at a non-Anthropic model (`local`)
    should not be handed a flag its model cannot use.
    """
    profiles = _load().get("profiles", {})
    name = profile if (profile and profile in profiles) else DEFAULT_PROFILE
    spec = profiles.get(name) or _FALLBACK_SPEC
    effort = spec.get("effort")
    return str(effort) if effort else None


def chain(preferred: Optional[str] = None) -> list[str]:
    """Profile names to try, in order: `preferred` first, then the configured
    `order`, then anything else defined. Deduped, and never empty.

    This is a FALLBACK chain and not load balancing, deliberately. The same
    provider is tried first every time, so a month's spend lands on one
    account predictably; rotating between accounts by accident is how a "free"
    tier quietly becomes an invoice. The tail of the chain, `local`, is the
    safety net: free, no account, works with the network down.
    """
    cfg = _load()
    profiles = cfg.get("profiles", {})
    order = cfg.get("order")

    names: list[str] = []
    if preferred:
        names.append(str(preferred))
    if isinstance(order, list):
        names.extend(str(n) for n in order)
    names.extend(profiles.keys())
    if not names:
        names.append(DEFAULT_PROFILE)

    seen: set[str] = set()
    out: list[str] = []
    for n in names:
        if n and n not in seen:
            seen.add(n)
            out.append(n)
    return out


def env_chain(preferred: Optional[str] = None) -> list[tuple[str, dict[str, str]]]:
    """(name, env) pairs to try in order. This is the whole failover contract.

    Every env is built from the CURRENT environment, never from the previous
    attempt's dict. Composing the second env out of the first is the original
    bug wearing a new hat: the first provider's ANTHROPIC_* would ride along
    into the second attempt, and the second provider would fail for a reason
    that has nothing to do with the second provider.
    """
    return [(name, env_for(name)) for name in chain(preferred)]


def describe(profile: Optional[str] = None) -> str:
    """One line naming what a crew would run on, for the log and the board."""
    cfg = _load()
    profiles = cfg.get("profiles", {})
    name = profile if (profile and profile in profiles) else DEFAULT_PROFILE
    spec = profiles.get(name)
    if spec is None:
        return (f"{name} (no {CONFIG_PATH.name} entry for it -- running on the "
                f"Claude subscription)")
    if not spec.get("base_url"):
        return (f"{name}: Claude subscription, model={spec.get('model')} "
                f"(all ANTHROPIC_* overrides removed)")
    return f"{name}: {spec.get('base_url')} model={spec.get('model')}"
