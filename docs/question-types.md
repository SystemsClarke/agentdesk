# Question kinds: ask, run, permission, packet

Kinds of question on the one Questions list. The agent picks one with `meta.type` on `ask_human`; `question_help` returns the
shape and the writing rules (the agent prompt lives there, in `src/kinds.rs`, not here).

## How it works

- **The kind** is `meta.type` on the question thread: `ask` (the default), `run`, `permission`, `packet` (`src/kinds.rs`). The
  Questions list has a KIND column, and a packet shows `[done/total]` in front of its subject.
- **ask_human by kind.** An ask is unchanged. A run question needs its script in a fenced block, and a packet needs `meta.packet` with
  all seven parts; it is refused, with every missing part named, if any is absent. `permission` is refused here: use
  `propose_permission_rule`, which now marks its question `type: permission`. Run, permission and packet are not held to the
  "contains a ?" and "500 words" rules, and run and packet stay open past John's first message (the "one question, one answer" rule
  does not apply to them while open).
- **Two tools.** `update_question` (the agent that asked revises its open packet; John's ticks on steps that still exist are kept)
  and `question_help` (the kinds, the shape of `meta.packet`, and the packet prompt).
- **Packet as a web page** (`src/packet_page.rs`, routes in `src/web.rs`): the question in the list is one line, "Fill out this packet" with
  the result and progress; the packet itself is server-rendered HTML on the ops console, `http://127.0.0.1:<port>/packet/<id>` (the
  same key cookie, Host and Origin checks as the console; posts must be JSON). John ticks steps, picks choices, adds notes (with a
  step, or to the agent) and finishes it there; each is the same `kinds::step`/`note`/`finish` call the window makes, so the agent is
  woken as before. The page polls `/packet/<id>/version` and reloads itself when the agent revises the packet (it keeps what John is
  typing and shows a bar instead). Opening a packet in the window opens the page once; `Alt+O` opens it again, and `agentdesk packet <id>`
  does the same from a shell (`--print` shows the address). Commands in the packet have a copy button and a run button: Run (after a confirm) runs it on the
  core's machine as John, with the script runner the window uses, and the output streams under the command; a reload keeps it, Stop ends it, and
  "send output to the agent" posts the tail as a note (nothing is sent unless he asks). The browser never sends a script: it names a block
  by a hash of its text and the server finds that block in the packet (so a revised packet cannot run an old command, and a finished one
  runs nothing). At most four run at once. The page's colours are the window's saved theme (the same palette, as CSS variables). The page
  needs the core running (it serves the console): after an upgrade, restart the core.
- **Packet in the window** (`src/tui/`): the reader still draws the packet's steps live at the bottom, above the reply box (where the reader
  opens, so they stay in view as ticks add messages). `Alt+Up/Down` move the cursor, `Alt+T` ticks or un-ticks the step (what is in
  the reply box becomes its note), `Alt+V` cycles a choice step's options (and ticks it), `Alt+D` finishes the packet (it asks first and
  says which steps were left). Typing and `Ctrl+S` sends a note and leaves the packet open.
- **Permission in the window.** On a permission question `Alt+A` approves, `Alt+D` rejects, `Alt+V` takes an approved rule back; each asks first. Typing is a note.
  A permission proposal made before kinds existed is still recognised through its row in `permission_proposals`.
- **Run in the window** reuses the existing script runner (`src/tui/runner.rs`) and does not add another: a run question opens the
  runner pane at once with the script shown, not run; `Alt+R` again runs it as John, `Alt+Y` puts the output in the reply.
- **Slack.** The message names its kind ("Packet", "Run request", "Permission rule") with the right hint for how to respond; a packet
  is the one line, since its page is on the laptop. A reply on a packet or a permission question is a note, not an answer; on a run question it is the answer.
- **Links** (`src/links.rs`): Safe Links and similar wrappers are unwrapped, tracking parameters and fragments stripped, non-default
  ports kept, credentials dropped, duplicates merged (with a count), unknown domains flagged, at most 12 shown. Pure string work.

- **Meeting hours** (`src/schedule.rs`, tool `filter_meeting_slots`): John's rules are enforced in code, because Outlook's availability
  search is not limited to working hours. An agent passes the slots it found through the tool and offers only what comes back: the
  earliest three that start no earlier than 10:00, end no later than 16:30, miss 13:00 to 14:00 and fall on a weekday, with every
  dropped slot and the reason. The rules are `meeting_start`, `meeting_end`, `lunch_start`, `lunch_end` and `meeting_weekdays_only` in
  `settings.json` (those are the defaults; an unreadable value falls back to them, a null `lunch_start` turns lunch off). Times are
  wall-clock in his mailbox zone; one with a `Z` or an offset is refused, not guessed.

**Not built:** resolving short links and checking links in a headless browser; creating the calendar event or hold (the agent does
that through the Microsoft 365 connection, and an invite is sent only on John's click); "verified by the agent" as a state.
**Not verified:** how it looks in a real terminal and against real Claude sessions (it is tested against the window's test backend
and a scratch board), and that a tick wakes a waiting agent (inferred from how replies work; a test covers that a tick is a message
from John after the agent's baseline).

## The kinds

| Kind | Use it when | John does | It closes when | Real example |
|---|---|---|---|---|
| ask | the agent is stuck and needs a short answer | answers | he answers | #795, backlog order |
| run | John must run a command or script | runs it here, or copies it | it ran and the agent checked | #573, IIS log retention install |
| permission | the agent needs a rule or access | approves or rejects | he decides | #801, the `cargo clippy` rule |
| packet | there is work for John, with steps and context | ticks steps, adds notes, reviews drafts | he finishes, or the agent verifies | #800, inbox triage |

**Rules by kind.**

| | `?` required | word limit | closes on |
|---|---|---|---|
| ask | yes | 500 | first answer (unchanged) |
| run | no | none on the command block | the agent confirms the run |
| permission | no | none | Approve or Reject |
| packet | no | none; parts are structured | John finishes, or the agent verifies |

### Packet kind

A packet has seven required parts. The creating tool rejects one that is missing any, so John never has to ask "what is this?".

1. Outcome: one sentence, "Done when ...".
2. Why you: what only a human can do here, and who is waiting.
3. What happened: the history, in order, each fact linked to its source.
4. What the agent found: links and evidence, cleaned and checked (below).
5. Your steps: small, checkable, with inputs: choices, notes, paste boxes.
6. What the agent does next: what each tick triggers, and what it checks itself.
7. Deadline: a date, the reason, and what slips if it is missed.

Plus a folded **Show my work**: recommendation, confidence, what was ruled out, what would change the agent's mind, who helped.
The agent can update a packet after John answers. It changes only what the answer changes and says what changed.

### Run kind

A command block (or a script shown in full), a **Run here** button, and a place for the output. John presses Enter; the core runs it
as John; the exit code and the end of the output are saved on the question and the agent checks the result itself.

- Nothing runs until John presses Enter. The core never runs one on its own.
- The full command or script is shown. Destructive patterns are flagged.
- Elevated or windowed commands stay **copy-only**.
- A command an agent wrote can have been shaped by something it read. John reads it first. That is the safeguard.
- **To verify:** whether the core carries John's Kerberos tickets. Agent sessions often cannot reach servers John can (thread #450). If
  the core can, a run question solves the #573 problem.

### Links

Every link goes through the same clean-up before John sees it:

1. Unwrap known wrappers (Outlook Safe Links) by reading the address out of the link. No request.
2. Normalise: strip tracking parameters and fragments, lower-case the host, drop the default port.
3. Resolve short-link domains from the core (not built).
4. De-duplicate on the final address; one entry shows how many facts cite it.
5. Label with the domain, flag unknown domains, cap at a dozen.
6. Check each remaining link in a headless browser (not built).

### Meetings and the calendar

When the next step is a meeting, the agent does the scheduling and John picks a time.

- It uses the Microsoft 365 availability search (`find_meeting_availability` / `outlook_find_available_time`). If someone's calendar
  cannot be read, their times are "unconfirmed", not guessed.
- John's rules: meetings 10:00 to 16:30, no meetings 13:00 to 14:00. Outlook's search is **not limited to working hours**, so the core
  filters its results. The rules live in settings and are enforced in code, not in the prompt.
- `outlook_create_event` with an attendee **emails the invitation immediately**; there is no draft. So the agent proposes holds on
  John's own calendar (no attendees) freely, and sends an invite only on John's click.
- Defaults to confirm: 30 minutes, a 15-minute gap, weekdays, the Outlook time zone.
- Packet deadlines get a proposed calendar hold the same way.

### Safety

- The researcher reads and creates or updates a question. It cannot send, merge, delete, or touch the calendar.
- Anything it read (mail, tickets, pages) is shown as text, never run, and never treated as instructions.
- Pages come from a fixed template and a structured spec. An agent cannot put script or free HTML on the page.
- "Send" means "make an Outlook draft". The last click is John's.
