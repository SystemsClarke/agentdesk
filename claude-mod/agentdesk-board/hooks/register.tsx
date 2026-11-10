import { atom, read, update } from 'claude-code'
import type { Register } from 'claude-code'

import type { Note, OpenThread, Row, Snapshot } from '../types'

const PANE = 'agentdesk-board'
const REFRESH_MS = 30000

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
    await $.ui.open({ id: PANE, title: 'AgentDesk', focus: true })
    void refresh($).catch(() => {}) // draws what it has now; the fresh board lands a moment later
    return { text: 'AgentDesk board opened.' }
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const ui = $.ui.resolve(e)
    const { Box, Text, Button, Markdown } = ui
    // The mobile app draws no text field: there the quick replies and Approve/Reject still work.
    const Input = 'Input' in ui && e.surface !== 'mobile' ? ui.Input : undefined
    const board = await read($, snap)
    const current = await read($, tab)
    const thread = await read($, open)
    const pending = await read($, loading)
    const flash = await read($, note)
    const sending = await read($, busy)
    const count = board?.waiting ?? 0

    const header = (
      <Box flexDirection="column" gap={1}>
        <Box gap={2}>
          <Text bold color="cyan">
            AgentDesk
          </Text>
          {board === null ? (
            <Text dimColor>loading…</Text>
          ) : count > 0 ? (
            <Text bold color="yellow">
              {count} waiting on you
            </Text>
          ) : (
            <Text color="green">nothing waiting on you</Text>
          )}
          {sending !== '' && <Text color="cyan">{sending}</Text>}
          <Button key="refresh" variant="secondary" hotkey="r" onPress={() => refresh($)}>
            Refresh
          </Button>
        </Box>
        <Box gap={1}>
          {TABS.map((t, i) => (
            <Button
              key={`tab-${t.id}`}
              variant={t.id === current && !thread ? 'primary' : 'secondary'}
              hotkey={String(i + 1)}
              onPress={() => showTab($, t.id)}
            >
              {t.id === 'questions' && count > 0 ? `${t.label} (${count})` : t.label}
            </Button>
          ))}
        </Box>
        {flash.text !== '' && (
          <Text bold color={flash.isError ? 'red' : 'green'}>
            {flash.text}
          </Text>
        )}
      </Box>
    )

    if (thread) {
      const id = thread.thread_id
      const proposal = thread.proposal?.status === 'pending' ? thread.proposal : null
      const isLive = thread.channel === 'question' && thread.status === 'open'
      const isIdle = sending === ''
      return (
        <Box flexDirection="column" gap={1} padding={1}>
          {header}
          <Box gap={2}>
            <Button key="back" variant="secondary" hotkey="b" onPress={() => back($)}>
              ← Back
            </Button>
            <Text bold>
              #{id} {thread.subject}
            </Text>
          </Box>
          <Box gap={2}>
            <Text bold color="cyan">
              {thread.opened_by}
            </Text>
            <Text dimColor>{thread.channel}</Text>
            {thread.kind !== '' && <Text color="magenta">{thread.kind}</Text>}
            <Text color={thread.waiting ? 'yellow' : 'green'}>
              {thread.waiting ? 'waiting on you' : thread.status}
            </Text>
          </Box>

          {proposal && (
            <Box flexDirection="column" gap={1} borderStyle="round" borderColor="yellow" paddingX={1}>
              <Text bold color="yellow">
                Permission rule ({proposal.scope})
              </Text>
              <Text>{proposal.rule}</Text>
              {isIdle && (
                <Box gap={2}>
                  <Button key="approve" variant="primary" hotkey="a" onPress={() => decide($, id, true)}>
                    Approve
                  </Button>
                  <Button key="reject" variant="secondary" hotkey="d" onPress={() => decide($, id, false)}>
                    Reject
                  </Button>
                </Box>
              )}
            </Box>
          )}

          {!proposal && thread.waiting && isIdle && (
            <Box gap={2}>
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

          {thread.messages.map(m => {
            const isJohn = m.author_kind === 'human'
            return (
              <Box
                key={`m-${m.id}`}
                flexDirection="column"
                borderStyle="round"
                borderColor={isJohn ? 'green' : 'gray'}
                paddingX={1}
              >
                <Box gap={2}>
                  <Text bold color={isJohn ? 'green' : 'cyan'}>
                    {isJohn ? 'You' : m.author}
                  </Text>
                  <Text dimColor>{m.ts.slice(0, 16).replace('T', ' ')}</Text>
                </Box>
                <Markdown text={m.body} />
              </Box>
            )
          })}

          {Input && (
            <Input
              key={`reply-${id}`}
              label="Reply"
              placeholder="Write a reply. Enter sends."
              submitLabel="send"
              rows={3}
              onSubmit={(text: string) => reply($, id, text)}
            />
          )}
        </Box>
      )
    }

    const page = board?.tabs[current]
    const list: Row[] = page?.rows ?? []
    const isQuestions = current === 'questions'
    return (
      <Box flexDirection="column" gap={1} padding={1}>
        {header}
        {page && !isQuestions && (
          <Text dimColor>
            {page.total > list.length
              ? `Newest ${list.length} of ${page.total}`
              : `${page.total} in all`}
            , most recent activity first
          </Text>
        )}
        {board !== null && list.length === 0 && (
          <Text dimColor>{isQuestions ? 'Nothing is waiting on you.' : 'Nothing here.'}</Text>
        )}
        <Box flexDirection="column">
          {list.map(r =>
            r.thread_id === null ? (
              <Box key={`row-${current}-${r.title}`} gap={2}>
                <Text color="magenta">{r.badge}</Text>
                <Text bold>{r.title}</Text>
                <Text dimColor>
                  {r.who} · {r.when}
                </Text>
              </Box>
            ) : isQuestions ? (
              <Button key={`open-${current}-${r.thread_id}`} variant="secondary" onPress={() => openThread($, r.thread_id as number)}>
                <Text color={r.badge === 'permission' ? 'yellow' : 'magenta'}>{r.badge.padEnd(10)}</Text>
                <Text bold>{r.who}</Text>
                <Text dimColor>
                  {`  ${r.when}  #${r.thread_id}`}
                </Text>
                {pending === r.thread_id ? <Text color="cyan">  opening…</Text> : ''}
              </Button>
            ) : (
              <Button key={`open-${current}-${r.thread_id}`} variant="secondary" onPress={() => openThread($, r.thread_id as number)}>
                <Text color="magenta">{r.badge.padEnd(8)}</Text>
                <Text>{r.title}</Text>
                <Text dimColor>
                  {`  ${r.who} · ${r.when}`}
                </Text>
                {pending === r.thread_id ? <Text color="cyan">  opening…</Text> : ''}
              </Button>
            ),
          )}
        </Box>
      </Box>
    )
  })
}
