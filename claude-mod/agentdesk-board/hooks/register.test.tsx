import { expect, test } from 'claude-code/testing'

// The pane against a fake board: a `process.run` hook stands in for db.py and
// answers each op as db.py would. tests/test_db.py covers db.py itself.

const PANE = {
  plugin: 'agentdesk-board',
  component: 'Pane',
  requestId: 'agentdesk-board',
  props: {
    title: 'AgentDesk',
    isFocused: true,
    bodyColumns: 100,
    placement: 'dock',
    scroll: { offset: 0, bodyRows: 40 },
    view: {},
  },
} as const

type Call = { args: string[]; stdin?: string }

function fakeBoard() {
  const calls: Call[] = []
  const proposal = { id: 57, rule: 'Bash(make:*)', scope: 'user', status: 'pending' }
  const threads: Record<number, any> = {
    1100: {
      thread_id: 1100, channel: 'question', status: 'open', subject: 'permission rule: Bash(make:*)',
      opened_by: 'builder', kind: 'permission', waiting: true, proposal,
      messages: [{ id: 1, ts: '2026-11-10T17:02:03+00:00', author: 'builder', author_kind: 'agent', body: '**Rule:** `Bash(make:*)`' }],
    },
    1101: {
      thread_id: 1101, channel: 'question', status: 'open', subject: 'May I deploy?',
      opened_by: 'builder', kind: '', waiting: true, proposal: null,
      messages: [{ id: 2, ts: '2026-11-10T17:04:39+00:00', author: 'builder', author_kind: 'agent', body: 'Deploy to **staging**?' }],
    },
  }
  const waiting = () => Object.values(threads).filter(t => t.waiting).length
  const state = { fail: '' }
  const answer = (args: string[], stdin?: string): Record<string, unknown> => {
    const [op, a, b] = args
    if (state.fail) return { error: state.fail }
    if (op === 'tab' && a === 'questions')
      return {
        waiting: waiting(),
        rows: Object.values(threads).filter(t => t.waiting).map(t => ({
          thread_id: t.thread_id, title: t.subject, who: t.opened_by, when: '11-10 17:04', badge: t.kind || 'ask',
        })),
      }
    if (op === 'tab' && a === 'goals')
      return { waiting: waiting(), rows: [{ thread_id: null, title: 'Fast-Checkout: ship it', who: 'lead x', when: '11-03 15:38', badge: 'running' }] }
    if (op === 'tab') return { waiting: waiting(), rows: [] }
    if (op === 'thread') return threads[Number(a)] ? { thread: threads[Number(a)] } : { error: `no such thread: ${a}` }
    if (op === 'reply') {
      const t = threads[Number(a)]
      t.messages = [...t.messages, { id: 99, ts: '2026-11-10T18:00:00+00:00', author: 'john', author_kind: 'human', body: stdin }]
      t.waiting = false
      t.status = 'answered'
      return { ok: true, message_id: 99 }
    }
    if (op === 'permission') {
      const t = threads[Number(b)]
      if (t.proposal.status !== 'pending') return { error: `this rule is already ${t.proposal.status}` }
      t.proposal = { ...t.proposal, status: a === 'approve' ? 'approved' : 'rejected' }
      t.waiting = false
      t.status = 'answered'
      return { ok: true, status: t.proposal.status, rule: t.proposal.rule }
    }
    if (op === 'close') {
      threads[Number(a)].waiting = false
      threads[Number(a)].status = 'closed'
      return { ok: true, closed: true }
    }
    return { error: `unknown op: ${args.join(' ')}` }
  }
  return { calls, answer, state }
}

async function opened($: any, on: any) {
  const board = fakeBoard()
  on('command.register', () => ({ value: { command: 'agentdesk' } }))
  on('ui.open', () => ({ value: { isPlaced: true } }))
  on('session.start', (_: unknown, e: { cwd: string }) => ({ cwd: e.cwd }))
  on('process.run', (_: unknown, e: { argv: string[]; init?: { stdin?: string } }) => {
    const args = e.argv.slice(2) // python, db.py, ...op
    board.calls.push({ args, stdin: e.init?.stdin })
    const doc = board.answer(args, e.init?.stdin)
    return { value: { exitCode: doc.error ? 1 : 0, stdout: JSON.stringify(doc), stderr: '' } }
  })
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await $.command.run({ command: 'agentdesk', args: '' })
  return board
}

test('the Questions tab lists what waits, with the count', async ($, on) => {
  await opened($, on)
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  expect((await ui.find({ type: 'Text', text: /waiting on you/ }))?.text).toBe('2 waiting on you')
  expect((await ui.find({ key: 'tab-questions' }))?.text).toBe('Questions (2)')
  expect((await ui.find({ key: 'open-questions-1100' }))?.text).toContain('permission rule: Bash(make:*)')
  await ui.unmount()
})

test('opening a permission question draws it, and Approve decides it', async ($, on) => {
  const board = await opened($, on)
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'open-questions-1100' })

  expect((await ui.find({ type: 'Text', text: /#1100/ }))?.text).toContain('permission rule')
  expect(await ui.find({ key: 'approve' })).toBeDefined()
  expect(await ui.find({ type: 'Markdown' })).toBeDefined()

  await ui.press({ key: 'approve' })
  expect(board.calls.some(c => c.args.join(' ') === 'permission approve 1100')).toBe(true)
  expect((await ui.find({ type: 'Text', text: /Approved #1100/ }))?.text).toContain('settings.json')
  expect(await ui.find({ key: 'approve' })).toBeUndefined() // decided: the box is gone
  expect((await ui.find({ type: 'Text', text: /waiting on you/ }))?.text).toBe('1 waiting on you')
  await ui.unmount()
})

test('Reject sends a reject', async ($, on) => {
  const board = await opened($, on)
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'open-questions-1100' })
  await ui.press({ key: 'reject' })
  expect(board.calls.some(c => c.args.join(' ') === 'permission reject 1100')).toBe(true)
  expect((await ui.find({ type: 'Text', text: /Rejected #1100/ }))).toBeDefined()
  await ui.unmount()
})

test('a plain question answers Yes, No or a typed reply', async ($, on) => {
  const board = await opened($, on)
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'open-questions-1101' })
  expect(await ui.find({ key: 'approve' })).toBeUndefined()
  expect(await ui.find({ key: 'close' })).toBeDefined()

  await ui.input({ key: 'reply-1101', text: 'Ship it, then post the run.' })
  const sent = board.calls.find(c => c.args[0] === 'reply')
  expect(sent?.args).toEqual(['reply', '1101'])
  expect(sent?.stdin).toBe('Ship it, then post the run.')
  expect((await ui.find({ type: 'Text', text: /Replied on #1101/ }))).toBeDefined()
  expect((await ui.find({ type: 'Text', text: /^You$/ }))).toBeDefined()
  expect(await ui.find({ key: 'yes' })).toBeUndefined() // answered: no longer waiting
  await ui.unmount()
})

test('Yes sends a short reply', async ($, on) => {
  const board = await opened($, on)
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'open-questions-1101' })
  await ui.press({ key: 'yes' })
  expect(board.calls.find(c => c.args[0] === 'reply')?.stdin).toBe('Yes, go ahead.')
  await ui.unmount()
})

test('Back returns to the list', async ($, on) => {
  await opened($, on)
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'open-questions-1101' })
  await ui.press({ key: 'back' })
  expect(await ui.find({ key: 'open-questions-1100' })).toBeDefined()
  await ui.unmount()
})

test('tabs switch, and a row with no thread draws as text', async ($, on) => {
  await opened($, on)
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  await ui.press({ key: 'tab-goals' })
  expect((await ui.find({ type: 'Text', text: /Fast-Checkout/ }))?.text).toBe('Fast-Checkout: ship it')
  await ui.press({ key: 'tab-wiki' })
  expect((await ui.find({ type: 'Text', text: /Nothing here/ }))).toBeDefined()
  await ui.unmount()
})

test('a database error shows in red instead of a blank pane', async ($, on) => {
  const board = await opened($, on)
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' })
  board.state.fail = 'database is locked'
  await ui.press({ key: 'open-questions-1100' })
  expect((await ui.find({ type: 'Text', text: /database is locked/ }))?.text).toBe('database is locked')
  expect(await ui.find({ key: 'open-questions-1100' })).toBeDefined() // the list is still there
  board.state.fail = ''
  await ui.press({ key: 'open-questions-1100' })
  await ui.press({ key: 'approve' })
  expect((await ui.find({ type: 'Text', text: /database is locked/ }))).toBeUndefined() // a success replaces it
  await ui.unmount()
})

test('every surface draws the list and a thread', async ($, on) => {
  await opened($, on)
  for (const surface of ['terminal', 'desktop', 'vscode', 'mobile'] as const) {
    const ui = await $.ui.mount({ ...PANE, surface })
    await ui.press({ key: 'open-questions-1101' })
    expect((await ui.find({ type: 'Text', text: /#1101/ }))).toBeDefined()
    if (surface === 'mobile') expect(await ui.find({ key: 'reply-1101' })).toBeUndefined()
    else expect(await ui.find({ key: 'reply-1101' })).toBeDefined()
    await ui.press({ key: 'back' })
    await ui.unmount()
  }
})
