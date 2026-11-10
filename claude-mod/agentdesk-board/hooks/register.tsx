import { atom, read, update } from 'claude-code'
import type { Register } from 'claude-code'

import type { Note, OpenThread, Row, Snapshot } from '../types'

const PANE = 'agentdesk-board'
const REFRESH_MS = 30000
// From this many body columns the pane shows the list and the open thread side by side.
const SPLIT_COLUMNS = 110

const TABS: { id: string; label: string }[] = [
  { id: 'questions', label: 'Questions' },
  { id: 'discussion', label: 'Discussion' },
  { id: 'work', label: 'Work' },
  { id: 'goals', label: 'Goals' },
  { id: 'prs', label: 'Merge list' },
  { id: 'wiki', label: 'Wiki' },
]

// A shape tag changes when its value's fields change: it declines what an
// earlier version stored, which outlives a reload.
const snap = atom({ plugin: 'agentdesk-board', key: 'snap' } as const, null, { shape: 'snap-1' })
const tab = atom({ plugin: 'agentdesk-board', key: 'tab' } as const, 'questions', { shape: 'tab-1' })
const open = atom({ plugin: 'agentdesk-board', key: 'open' } as const, null, { shape: 'open-3' })
const loading = atom({ plugin: 'agentdesk-board', key: 'loading' } as const, null, { shape: 'loading-1' })
const note = atom(
  { plugin: 'agentdesk-board', key: 'note' } as const,
  { text: '', isError: false },
  { shape: 'note-2' },
)
// What is being sent right now ('' when nothing): it disables the buttons and says so.
const busy = atom({ plugin: 'agentdesk-board', key: 'busy' } as const, '', { shape: 'busy-2' })

type Runner = {
  process: {
    run: (
      argv: readonly string[],
      init?: { stdin?: string; timeoutMs?: number },
    ) => Promise<{ exitCode: number; stdout: string; stderr: string }>
  }
  plugin: { root: string }
}
type Api = Runner & Parameters<typeof update>[0]
type Doc = Record<string, unknown> & { error?: string }

// The interpreter db.py runs on. `python` on Windows is often the Store's
// launcher alias, which costs ~200 ms a call: session.start asks it once for
// the real python.exe and every call after goes straight there.
let python = 'python'

async function findPython($: Runner) {
  try {
    const out = await $.process.run(['python', '-c', 'import sys; print(sys.executable)'], { timeoutMs: 10000 })
    const exe = out.stdout.trim()
    if (out.exitCode === 0 && exe !== '') python = exe
  } catch {
    // keep `python`: the first db call reports it if it is missing
  }
}

// The one thing this mod runs: db.py, which opens AgentDesk's SQLite file with
// Python's own sqlite3. No pipe, no daemon, no HTTP, none of AgentDesk's binaries.
async function db($: Runner, args: string[], stdin?: string): Promise<Doc> {
  try {
    const out = await $.process.run(
      [python, '-I', '-S', `${$.plugin.root}/hooks/db.py`, ...args],
      { stdin, timeoutMs: 30000 },
    )
    try {
      return JSON.parse(out.stdout || '{}') as Doc
    } catch {
      return { error: out.stderr.trim() || 'db.py printed nothing' }
    }
  } catch (e) {
    return { error: `could not run python: ${String(e)}` }
  }
}

async function say($: Api, text: string, isError = false) {
  await update($, note, (): Note => ({ text, isError }))
}

/** One process: every tab and every waiting question. A failed refresh keeps what is drawn and says why. */
async function refresh($: Api) {
  const doc = await db($, ['snapshot'])
  if (doc.error) return say($, doc.error, true)
  await update($, snap, () => doc as unknown as Snapshot)
  const shown = await read($, open)
  const fresh = shown && (doc as unknown as Snapshot).threads[String(shown.thread_id)]
  if (fresh) await update($, open, () => fresh)
}

/** Tabs come out of the snapshot: no process, so the switch is instant. */
async function showTab($: Api, name: string) {
  await update($, open, () => null)
  await update($, tab, () => name)
}

/** A waiting question is in the snapshot already; anything else is one read. */
async function openThread($: Api, id: number) {
  const cached = (await read($, snap))?.threads[String(id)]
  if (cached) return update($, open, () => cached)
  await update($, loading, () => id)
  const doc = await db($, ['thread', String(id)])
  await update($, loading, () => null)
  if (doc.error) return say($, doc.error, true)
  await update($, open, () => doc.thread as OpenThread)
}

/** A write is one process: db.py makes it and answers with the board as it now is. */
async function act($: Api, args: string[], sending: string, done: string, stdin?: string) {
  if ((await read($, busy)) !== '') return
  await update($, busy, () => sending)
  try {
    const doc = await db($, args, stdin)
    if (doc.error) return await say($, doc.error, true)
    await update($, snap, () => doc.snapshot as Snapshot)
    await update($, open, () => doc.thread as OpenThread)
    await say($, done)
  } finally {
    await update($, busy, () => '')
  }
}

async function reply($: Api, id: number, text: string) {
  if (!text.trim()) return say($, 'The reply is empty.', true)
  return act($, ['reply', String(id)], 'Sending…', `Replied on #${id}.`, text)
}

async function decide($: Api, id: number, approve: boolean) {
  return approve
    ? act($, ['permission', 'approve', String(id)], 'Approving…', `Approved #${id}: the rule is in settings.json.`)
    : act($, ['permission', 'reject', String(id)], 'Rejecting…', `Rejected #${id}.`)
}

async function closeQuestion($: Api, id: number) {
  return act($, ['close', String(id)], 'Closing…', `Closed #${id} without an answer.`)
}

async function back($: Api) {
  await say($, '')
  await update($, open, () => null)
}

/** "now", "12m ago", "3h ago", "2d ago": how a list is read at a glance. */
function ago(ts: string, now: number) {
  const then = Date.parse(ts)
  if (Number.isNaN(then)) return ''
  const s = Math.max(0, Math.round((now - then) / 1000))
  if (s < 60) return 'now'
  if (s < 3600) return `${Math.floor(s / 60)}m ago`
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`
  return `${Math.floor(s / 86400)}d ago`
}

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    await $.command.register({
      name: 'agentdesk',
      description: 'Open the AgentDesk board: questions, discussion, work, goals',
    })
    // Background reads: a failure is drawn by refresh itself, and one that
    // outlives the module (a reload mid-read) has nothing left to tell.
    void findPython($).then(() => refresh($)).catch(() => {})
    $.clock.every(REFRESH_MS, () => void refresh($).catch(() => {}))
    return next(e)
  })

  on('command.run', { command: 'agentdesk' }, async $ => {
    await say($, '')
    // Ask for all the room the surface will give: tall inline, wide when docked.
    await $.ui.open({ id: PANE, title: 'AgentDesk', focus: true, rows: 60, columns: 160 })
    void refresh($).catch(() => {}) // draws what it has now; the fresh board lands a moment later
    return { text: 'AgentDesk board opened.' }
  })

  // A press on a card of the Client list (hooks/cards.tsx). The post is code's
  // word, not the engine's, so it is checked before it opens anything.
  on('ui.message', { requestId: PANE }, async ($, e, next) => {
    const data = e.data as { open?: unknown } | null
    if (e.module.endsWith('cards.tsx') && data && Number.isInteger(data.open)) await openThread($, data.open as number)
    return next(e)
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const ui = $.ui.resolve(e)
    const { Box, Text, Button, Markdown } = ui
    // The mobile app draws no text field: there the quick replies and Approve/Reject still work.
    const Input = 'Input' in ui && e.surface !== 'mobile' ? ui.Input : undefined
    // Where the surface draws a Client (terminal, desktop), the list is one: a
    // press anywhere on a card opens it. Elsewhere each card is a Button.
    const Client = 'Client' in ui && (e.surface === 'terminal' || e.surface === 'desktop') ? ui.Client : undefined
    const board = await read($, snap)
    const current = await read($, tab)
    const thread = await read($, open)
    const pending = await read($, loading)
    const flash = await read($, note)
    const sending = await read($, busy)
    const count = board?.waiting ?? 0
    const now = Date.now()
    // Wide enough for the list and the thread side by side: the pane fills
    // with both, and a press on a row opens it on the right.
    const isSplit = e.surface !== 'mobile' && (e.props?.bodyColumns ?? 0) >= SPLIT_COLUMNS

    // ---- the header: what this is, how much waits, the tabs

    const header = (
      <Box flexDirection="column" gap="sm">
        <Box justifyContent="space-between" alignItems="center">
          <Box gap="sm" alignItems="center">
            <Text color="claude" size="title" weight="bold">
              ✻ AgentDesk
            </Text>
            {board === null ? (
              <Text color="inactive" size="footnote">
                loading…
              </Text>
            ) : count > 0 ? (
              <Text color="warning" weight="semibold">
                {count} waiting on you
              </Text>
            ) : (
              <Text color="success">nothing waiting on you</Text>
            )}
          </Box>
          <Box gap="sm" alignItems="center">
            {sending !== '' && <Text color="claude">{sending}</Text>}
            <Button key="refresh" plain dimColor hotkey="r" onPress={() => refresh($)}>
              Refresh
            </Button>
          </Box>
        </Box>
        <Box gap="xs" flexWrap="wrap">
          {TABS.map((t, i) => (
            <Button
              key={`tab-${t.id}`}
              variant={t.id === current && (isSplit || !thread) ? 'primary' : 'secondary'}
              hotkey={String(i + 1)}
              onPress={() => showTab($, t.id)}
            >
              {t.id === 'questions' && count > 0 ? `${t.label} (${count})` : t.label}
            </Button>
          ))}
        </Box>
        {flash.text !== '' && (
          <Text color={flash.isError ? 'error' : 'success'} weight="semibold">
            {flash.isError ? '✗ ' : '✓ '}
            {flash.text}
          </Text>
        )}
      </Box>
    )

    // ---- the list: one row per thread, the whole row the button

    const page = board?.tabs[current]
    const list: Row[] = page?.rows ?? []
    const isQuestions = current === 'questions'
    const chip = (badge: string) =>
      badge === 'permission' ? 'permission' : badge === 'open' || badge === 'ask' || badge === 'running' ? 'warning' : 'inactive'

    const listView = (
      <Box flexDirection="column" gap="xs">
        {page && !isQuestions && (
          <Text color="inactive" size="footnote">
            {page.total > list.length ? `Newest ${list.length} of ${page.total}` : `${page.total} in all`}, most recent activity first
          </Text>
        )}
        {board !== null && list.length === 0 && (
          <Box paddingY="md">
            <Text color="inactive">{isQuestions ? 'Nothing is waiting on you. ✓' : 'Nothing here.'}</Text>
          </Box>
        )}
        {Client && list.length > 0 ? (
          <Client
            key={`cards-${current}`}
            module="./cards.tsx"
            width="100%"
            props={{
              cards: list
                .filter(r => r.thread_id !== null)
                .map(r => {
                  const badge = r.badge === 'fyi' ? '' : r.badge
                  return {
                    id: r.thread_id as number,
                    title: isQuestions ? r.who : r.title,
                    badge,
                    badgeColor: chip(badge),
                    details: [isQuestions ? `#${r.thread_id}` : r.who, ago(r.ts, now), pending === r.thread_id ? 'opening…' : '']
                      .filter(Boolean)
                      .join('  ·  '),
                    isOpen: thread?.thread_id === r.thread_id,
                  }
                }),
            }}
          />
        ) : null}
        {Client && list.some(r => r.thread_id === null) && (
          <Text color="inactive">{list.filter(r => r.thread_id === null).map(r => `${r.badge}  ·  ${r.title}`).join('\n')}</Text>
        )}
        {!Client && list.map(r => {
          const isOpen = thread?.thread_id === r.thread_id && r.thread_id !== null
          const age = ago(r.ts, now)
          const label = isQuestions ? r.who : r.title
          // The badge leads the second line, so a long title never pushes it out
          // of the card; "fyi" is every discussion's and says nothing, so it goes.
          const badge = r.badge === 'fyi' ? '' : r.badge
          const meta = [isQuestions ? `#${r.thread_id}` : r.who, age, r.thread_id !== null && pending === r.thread_id ? 'opening…' : '']
            .filter(Boolean)
            .join('  ·  ')
          // Title and details are one Button: the whole card is the target.
          const face = [
            ' ',
            <Text weight="semibold" color={isOpen ? 'claude' : 'text'} wrap="wrap">
              {label}
            </Text>,
            '\n ',
            badge !== '' ? (
              <Text color={chip(badge)} size="footnote" weight="semibold">
                {badge}
                {'  ·  '}
              </Text>
            ) : (
              ''
            ),
            <Text color="inactive" size="footnote" wrap="wrap">
              {meta}
            </Text>,
          ]
          return (
            // No padding between the card and its Button, and the Button stretched
            // across it: the whole bubble is the click target, not just the words.
            // The breathing room lives inside the Button's own label instead.
            <Box
              key={`row-${current}-${r.thread_id ?? r.title}`}
              flexDirection="column"
              alignItems="stretch"
              borderStyle="round"
              borderColor={isOpen ? 'claude' : 'promptBorder'}
              borderDimColor={!isOpen}
              hover={{ borderColor: 'claude' }}
            >
              {r.thread_id === null ? (
                <Text>{face}</Text>
              ) : (
                <Button key={`open-${current}-${r.thread_id}`} plain onPress={() => openThread($, r.thread_id as number)}>
                  {face}
                </Button>
              )}
            </Box>
          )
        })}
      </Box>
    )

    // ---- the thread: like a transcript, with the decision at the top

    const threadView = (t: OpenThread) => {
      const id = t.thread_id
      const proposal = t.proposal?.status === 'pending' ? t.proposal : null
      const isLive = t.channel === 'question' && t.status === 'open'
      const isIdle = sending === ''
      return (
        <Box flexDirection="column" gap="md">
          <Box flexDirection="column" gap="xs">
            {!isSplit && (
              <Button key="back" plain dimColor hotkey="b" onPress={() => back($)}>
                ← Back
              </Button>
            )}
            <Text size="heading" weight="bold">
              {t.subject}
            </Text>
            <Box gap="sm">
              <Text color="claude" weight="semibold">
                {t.opened_by}
              </Text>
              <Text color="inactive">#{id}</Text>
              <Text color="inactive">{t.channel}</Text>
              {t.kind !== '' && <Text color={chip(t.kind)}>{t.kind}</Text>}
              <Text color={t.waiting ? 'warning' : 'success'}>{t.waiting ? 'waiting on you' : t.status}</Text>
            </Box>
          </Box>

          {proposal && (
            <Box flexDirection="column" gap="sm" borderStyle="round" borderColor="permission" paddingX="md" paddingY="sm">
              <Text color="permission" weight="bold">
                Allow this permission rule? ({proposal.scope})
              </Text>
              <Text mono>{proposal.rule}</Text>
              {isIdle && (
                <Box gap="sm">
                  <Button key="approve" variant="primary" hotkey="a" autoFocus onPress={() => decide($, id, true)}>
                    Approve
                  </Button>
                  <Button key="reject" variant="secondary" hotkey="d" onPress={() => decide($, id, false)}>
                    Reject
                  </Button>
                </Box>
              )}
            </Box>
          )}

          {!proposal && t.waiting && isIdle && (
            <Box gap="sm">
              <Button key="yes" variant="primary" hotkey="y" onPress={() => reply($, id, 'Yes, go ahead.')}>
                Yes, go ahead
              </Button>
              <Button key="no" variant="secondary" hotkey="n" onPress={() => reply($, id, 'No, do not.')}>
                No
              </Button>
              {isLive && (
                <Button key="close" variant="secondary" hotkey="x" onPress={() => closeQuestion($, id)}>
                  Close unanswered
                </Button>
              )}
            </Box>
          )}

          <Box flexDirection="column" gap="md">
            {t.messages.map(m => {
              const isJohn = m.author_kind === 'human'
              return (
                <Box key={`m-${m.id}`} flexDirection="column" gap="xs">
                  <Box gap="sm">
                    <Text color={isJohn ? 'suggestion' : 'claude'}>{isJohn ? '❯' : '⏺'}</Text>
                    <Text weight="semibold">{isJohn ? 'You' : m.author}</Text>
                    <Text color="inactive" size="footnote">
                      {ago(m.ts, now)}
                    </Text>
                  </Box>
                  <Box paddingLeft={2}>
                    <Markdown text={m.body} />
                  </Box>
                </Box>
              )
            })}
          </Box>

          {Input && (
            <Box flexDirection="column" width="100%">
              <Input
                key={`reply-${id}`}
                placeholder="Reply to the agent. Enter sends."
                submitLabel="send"
                rows={4}
                onSubmit={(text: string) => reply($, id, text)}
              />
            </Box>
          )}
        </Box>
      )
    }

    const empty = (
      <Box paddingY="lg" justifyContent="center">
        <Text color="inactive">{isQuestions && count > 0 ? '← Pick a question to read it here.' : 'Pick a thread to read it here.'}</Text>
      </Box>
    )

    if (isSplit)
      return (
        <Box flexDirection="column" gap="md" padding="md">
          {header}
          <Box gap="lg" alignItems="flex-start" width="100%">
            <Box flexDirection="column" width="40%" flexShrink={0}>
              {listView}
            </Box>
            {/* minWidth 0 lets the thread shrink to what is left instead of running
                past the pane's edge; a wide table is clipped, not the whole column. */}
            <Box flexDirection="column" flexGrow={1} flexShrink={1} minWidth={0} overflow="hidden">
              {thread ? threadView(thread) : empty}
            </Box>
          </Box>
        </Box>
      )

    return (
      <Box flexDirection="column" gap="md" padding="md">
        {header}
        {thread ? threadView(thread) : listView}
      </Box>
    )
  })
}
