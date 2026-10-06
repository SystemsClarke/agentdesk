# Phoenix and the state file

An identity (a long-lived agent) hands off at 60% of its context with `pass_the_torch`: the core ends the session after the turn and starts
its successor, a new conversation whose first prompt is the handoff. A handoff is one summary, written at the last moment, and a summary is
where detail gets lost (Shao et al., "Context Language Models", arXiv 2609.37725: summary compaction loses or invents facts that must be kept
verbatim, which in-place edited state did not).

## The state file

Each identity has one file, `%LOCALAPPDATA%\AgentDesk\state\<name>.md` (`Identities.StatePath`). Its charter tells it to keep the file current
by **editing it in place**, not by appending a log: DECISIONS (with why), OPEN (what is mid-flight and its next step), TRIED and UNTRIED,
KEY FACTS to keep verbatim (ids, paths, exact strings, commands) and WHO owns what. The session is started with `--add-dir` for the state
folder so it can edit the file without asking.

When a handoff restarts the identity, the successor's first prompt is: the handoff, then the state file exactly as it was left, then the
core's usual context. The file is cut at 12,000 characters (a note says where to read the rest; `"phoenix_state_chars"` in `settings.json`
changes it, 500 to 20,000): the prompt is a command line, and Windows allows 32,767 characters in all.

The state file does not replace the handoff, and it does not change anything else: an identity that is resumed (not handed off) keeps its
whole conversation as before.

## Switching it off

`settings.json`: `"phoenix_state_file": false` removes the charter paragraph, the `--add-dir` and the successor's copy. It is on by default.

## What is not known yet

Nobody has measured whether successors do better with it. The paper's gains were measured with a harness that lets a model edit its live
context, which Claude Code does not offer; this borrows only the idea (an edited-in-place ledger instead of a one-shot summary).
Look at a few real handoffs: does the successor pick up what was mid-flight, and does the file stay short and true?
