import { atom, read, update } from 'claude-code'
import type { Register } from 'claude-code'

import type { Note, OpenThread, Row } from '../types'

const PANE = 'agentdesk-board'

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
const tab = atom({ plugin: 'agentdesk-board', key: 'tab' } as const, 'questions', {
  shape: 'tab-1',
})
const rows = atom({ plugin: 'agentdesk-board', key: 'rows' } as const, [], {
  shape: 'rows-2',
})
const waiting = atom({ plugin: 'agentdesk-board', key: 'waiting' } as const, 0, {
  shape: 'waiting-1',
})
const open = atom({ plugin: 'agentdesk-board', key: 'open' } as const, null, {
  shape: 'open-2',
})
const note = atom(
  { plugin: 'agentdesk-board', key: 'note' } as const,
  { text: '', isError: false },
  { shape: 'note-2' },
)
const busy = atom({ plugin: 'agentdesk-board', key: 'busy' } as const, false, {
  shape: 'busy-1',
})

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

// The one thing this mod runs: db.py, which opens AgentDesk's SQLite file with
// Python's own sqlite3. No pipe, no daemon, no HTTP, none of AgentDesk's binaries.
async function db($: Runner, args: string[], stdin?: string): Promise<Doc> {
  try {
    const out = await $.process.run(
      ['python', `${$.plugin.root}/hooks/db.py`, ...args],
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

async function load($: Api, name: string) {
  await update($, tab, () => name)
  await update($, open, () => null)
  const doc = await db($, ['tab', name])
  if (doc.error) return say($, doc.error, true)
  await update($, rows, () => doc.rows as Row[])
  await update($, waiting, () => Number(doc.waiting ?? 0))
}

async function show($: Api, id: number) {
  const doc = await db($, ['thread', String(id)])
  if (doc.error) return say($, doc.error, true)
  await update($, open, () => doc.thread as OpenThread)
}

/** Runs one write, then re-reads the thread and the waiting count. */
async function act($: Api, id: number, args: string[], done: string, stdin?: string) {
  if (await read($, busy)) return
  await update($, busy, () => true)
  try {
    const doc = await db($, args, stdin)
    if (doc.error) return await say($, doc.error, true)
    await say($, done)
    await show($, id)
    const fresh = await db($, ['tab', 'questions'])
    if (!fresh.error) await update($, waiting, () => Number(fresh.waiting ?? 0))
  } finally {
    await update($, busy, () => false)
  }
}

async function reply($: Api, id: number, text: string) {
  if (!text.trim()) return say($, 'The reply is empty.', true)
  return act($, id, ['reply', String(id)], `Replied on #${id}.`, text)
}

async function decide($: Api, id: number, approve: boolean) {
  const verb = approve ? 'approve' : 'reject'
  return act($, id, ['permission', verb, String(id)], approve ? `Approved #${id}: the rule is in settings.json.` : `Rejected #${id}.`)
}

async function closeQuestion($: Api, id: number) {
  return act($, id, ['close', String(id)], `Closed #${id} without an answer.`)
}

async function back($: Api) {
  await say($, '')
  return load($, await read($, tab))
}

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    await $.command.register({
      name: 'agentdesk',
      description: 'Open the AgentDesk board: questions, discussion, work, goals',
    })
    return next(e)
  })

  on('command.run', { command: 'agentdesk' }, async $ => {
    await $.ui.open({ id: PANE, title: 'AgentDesk', focus: true })
    await say($, '')
    await load($, 'questions')
    return { text: 'AgentDesk board opened.' }
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const ui = $.ui.resolve(e)
    const { Box, Text, Button, Markdown } = ui
    // The mobile app draws no text field: there the quick replies and Approve/Reject still work.
    const Input = 'Input' in ui ? ui.Input : undefined
    const current = await read($, tab)
    const list = await read($, rows)
    const count = await read($, waiting)
    const thread = await read($, open)
    const flash = await read($, note)
    const isBusy = await read($, busy)

    const header = (
      <Box flexDirection="column" gap={1}>
        <Box gap={2}>
          <Text bold color="cyan">
            AgentDesk
          </Text>
          {count > 0 ? (
            <Text bold color="yellow">
              {count} waiting on you
            </Text>
          ) : (
            <Text color="green">nothing waiting on you</Text>
          )}
          {isBusy && <Text dimColor>working…</Text>}
          <Button key="refresh" variant="secondary" hotkey="r" onPress={() => (thread ? show($, thread.thread_id) : load($, current))}>
            Refresh
          </Button>
        </Box>
        <Box gap={1}>
          {TABS.map((t, i) => (
            <Button
              key={`tab-${t.id}`}
              variant={t.id === current && !thread ? 'primary' : 'secondary'}
              hotkey={String(i + 1)}
              onPress={() => say($, '').then(() => load($, t.id))}
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
      const isQuestion = thread.channel === 'question'
      const isLive = isQuestion && thread.status === 'open'
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
            <Text dimColor>{thread.channel}</Text>
            {thread.kind !== '' && <Text color="magenta">{thread.kind}</Text>}
            <Text color={thread.waiting ? 'yellow' : 'green'}>
              {thread.waiting ? 'waiting on you' : thread.status}
            </Text>
            <Text dimColor>from {thread.opened_by}</Text>
          </Box>

          {proposal && (
            <Box flexDirection="column" gap={1} borderStyle="round" borderColor="yellow" paddingX={1}>
              <Text bold color="yellow">
                Permission rule ({proposal.scope})
              </Text>
              <Text>{proposal.rule}</Text>
              <Box gap={2}>
                <Button key="approve" variant="primary" hotkey="a" onPress={() => decide($, id, true)}>
                  Approve
                </Button>
                <Button key="reject" variant="secondary" hotkey="d" onPress={() => decide($, id, false)}>
                  Reject
                </Button>
              </Box>
            </Box>
          )}

          {!proposal && thread.waiting && (
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

    return (
      <Box flexDirection="column" gap={1} padding={1}>
        {header}
        {list.length === 0 && flash.text === '' && <Text dimColor>Nothing here.</Text>}
        {list.map((r, i) => (
          <Box
            key={`r-${current}-${i}`}
            flexDirection="column"
            borderStyle="round"
            borderColor={r.badge === 'permission' ? 'yellow' : 'gray'}
            paddingX={1}
          >
            <Box gap={2}>
              <Text color={r.badge === 'permission' ? 'yellow' : 'magenta'}>{r.badge}</Text>
              <Text dimColor>
                {r.who} · {r.when}
              </Text>
            </Box>
            {r.thread_id !== null ? (
              <Button key={`open-${current}-${r.thread_id}`} variant="secondary" onPress={() => show($, r.thread_id as number)}>
                {`#${r.thread_id}  ${r.title}`}
              </Button>
            ) : (
              <Text bold>{r.title}</Text>
            )}
          </Box>
        ))}
      </Box>
    )
  })
}
