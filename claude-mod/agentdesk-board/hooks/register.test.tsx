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

type Call = { args: string[]; argv: string[]; stdin?: string }

function fakeBoard() {
  const calls: Call[] = []
  const state = { fail: '' }
  const threads: Record<number, any> = {
    1100: {
      thread_id: 1100, channel: 'question', status: 'open', subject: 'permission rule: Bash(make:*)',
      opened_by: 'DockerAgents', kind: 'permission', waiting: true,
      proposal: { id: 57, rule: 'Bash(make:*)', scope: 'user', status: 'pending' },
      messages: [{ id: 1, ts: '2026-11-10T17:02:03+00:00', author: 'DockerAgents', author_kind: 'agent', body: '**Rule:** `Bash(make:*)`' }],
    },
    1101: {
      thread_id: 1101, channel: 'question', status: 'open', subject: 'May I deploy?',
      opened_by: 'AWS-Liaison', kind: '', waiting: true, proposal: null,
      messages: [{ id: 2, ts: '2026-11-10T17:04:39+00:00', author: 'AWS-Liaison', author_kind: 'agent', body: 'Deploy to **staging**?' }],
    },
    775: {
      thread_id: 775, channel: 'discussion', status: 'fyi', subject: 'Crowdin repair', opened_by: 'Local_Helper',
      kind: '', waiting: false, proposal: null,
      messages: [{ id: 3, ts: '2026-11-10T18:22:00+00:00', author: 'Local_Helper', author_kind: 'agent', body: 'Found the gap.' }],
    },
  }
  const waitingOnes = () => Object.values(threads).filter(t => t.waiting)
  const snapshot = () => {
    const n = waitingOnes().length
    const row = (t: any) => ({ thread_id: t.thread_id, title: t.subject, who: t.opened_by, when: '11-10 17:04', ts: '2026-11-10T17:04:00+00:00', badge: t.kind || 'ask' })
    return {
      waiting: n,
      threads: Object.fromEntries(waitingOnes().map(t => [String(t.thread_id), t])),
      tabs: {
        questions: { waiting: n, total: n, rows: waitingOnes().map(row) },
        discussion: { waiting: n, total: 425, rows: [{ ...row(threads[775]), who: 'Local_Helper · 9 msgs', badge: 'fyi' }] },
        work: { waiting: n, total: 0, rows: [] },
        goals: { waiting: n, total: 1, rows: [{ thread_id: null, title: 'Fast-Checkout: ship it', who: 'lead x', when: '11-03 15:38', ts: '2026-11-03T15:38:00+00:00', badge: 'running' }] },
        prs: { waiting: n, total: 0, rows: [] },
        wiki: { waiting: n, total: 0, rows: [] },
      },
    }
  }
  const settle = (t: any, status: string) => {
    t.waiting = false
    t.status = status
  }
  const answer = (args: string[], stdin?: string): Record<string, unknown> => {
    const [op, a, b] = args
    if (state.fail) return { error: state.fail }
    if (op === 'snapshot') return snapshot()
    if (op === 'thread') return threads[Number(a)] ? { thread: threads[Number(a)] } : { error: `no such thread: ${a}` }
    if (op === 'reply') {
      const t = threads[Number(a)]
      t.messages = [...t.messages, { id: 99, ts: '2026-11-10T18:00:00+00:00', author: 'john', author_kind: 'human', body: stdin }]
      settle(t, 'answered')
      return { ok: true, message_id: 99, snapshot: snapshot(), thread: t }
    }
    if (op === 'permission') {
      const t = threads[Number(b)]
      if (t.proposal.status !== 'pending') return { error: `this rule is already ${t.proposal.status}` }
      t.proposal = { ...t.proposal, status: a === 'approve' ? 'approved' : 'rejected' }
      settle(t, 'answered')
      return { ok: true, status: t.proposal.status, snapshot: snapshot(), thread: t }
    }
    if (op === 'close') {
      settle(threads[Number(a)], 'closed')
      return { ok: true, closed: true, snapshot: snapshot(), thread: threads[Number(a)] }
    }
    return { error: `unknown op: ${args.join(' ')}` }
  }
  return { calls, answer, state }
}

async function opened($: any, on: any) {
  const board = fakeBoard()
  on('command.register', () => ({ value: { command: 'agentdesk' } }))
  on('ui.open', () => ({ value: { isPlaced: true } }))
  on('clock.every', () => ({ value: { cancel: () => {} } }))
  on('session.start', (_: unknown, e: { cwd: string }) => ({ cwd: e.cwd }))
  on('process.run', (_: unknown, e: { argv: string[]; init?: { stdin?: string } }) => {
    if (e.argv[1] === '-c') return { value: { exitCode: 0, stdout: 'C:\\Python\\python.exe\r\n', stderr: '' } }
    const args = e.argv.slice(4) // python.exe, -I, -S, db.py, ...op
    board.calls.push({ args, argv: e.argv, stdin: e.init?.stdin })
    const doc = board.answer(args, e.init?.stdin)
    return { value: { exitCode: doc.error ? 1 : 0, stdout: JSON.stringify(doc), stderr: '' } }
  })
  await $.session.start({ cwd: '/', surface: 'desktop', isInteractive: true })
  await $.command.run({ command: 'agentdesk', args: '' })
  return board
}

const mount = ($: any, surface: 'terminal' | 'desktop' | 'vscode' | 'mobile' = 'desktop') =>
  $.ui.mount({ ...PANE, surface })

test('the Questions tab lists who is asking, with the count', async ($, on) => {
  await opened($, on)
  const ui = await mount($)
  expect((await ui.find({ type: 'Text', text: /waiting on you/ }))?.text).toBe('2 waiting on you')
  expect((await ui.find({ key: 'tab-questions' }))?.text).toBe('Questions (2)')
  const row = await ui.find({ key: 'open-questions-1100' })
  // The whole card is the button: the agent, then its kind, number and age. No preview.
  expect(row?.text).toMatch(/^ DockerAgents\n permission {2}· {2}#1100 {2}· {2}(now|\d+[mhd] ago)$/)
  expect(await ui.find({ type: 'Text', text: /Bash\(make/ })).toBeUndefined()
  await ui.unmount()
})

test('db.py runs on the real interpreter, isolated', async ($, on) => {
  const board = await opened($, on)
  const first = board.calls[0]
  expect(first?.args).toEqual(['snapshot'])
  expect(first?.argv.slice(0, 3)).toEqual(['C:\\Python\\python.exe', '-I', '-S'])
})

test('switching tabs and opening a waiting question start no process', async ($, on) => {
  const board = await opened($, on)
  const ui = await mount($)
  const before = board.calls.length
  await ui.press({ key: 'tab-discussion' })
  await ui.press({ key: 'tab-goals' })
  await ui.press({ key: 'tab-questions' })
  await ui.press({ key: 'open-questions-1101' })
  expect((await ui.find({ type: 'Text', text: /May I deploy/ }))?.text).toBe('May I deploy?')
  expect(board.calls.length).toBe(before)
  await ui.unmount()
})

test('a list tab says how much it shows of how many', async ($, on) => {
  await opened($, on)
  const ui = await mount($)
  await ui.press({ key: 'tab-discussion' })
  expect((await ui.find({ type: 'Text', text: /Newest/ }))?.text).toBe('Newest 1 of 425, most recent activity first')
  await ui.unmount()
})

test('opening a discussion thread reads it once', async ($, on) => {
  const board = await opened($, on)
  const ui = await mount($)
  await ui.press({ key: 'tab-discussion' })
  await ui.press({ key: 'open-discussion-775' })
  expect(board.calls.at(-1)?.args).toEqual(['thread', '775'])
  expect((await ui.find({ type: 'Text', text: /^Crowdin repair$/ }))).toBeDefined()
  await ui.unmount()
})

test('Approve decides a permission rule in one process', async ($, on) => {
  const board = await opened($, on)
  const ui = await mount($)
  await ui.press({ key: 'open-questions-1100' })
  expect(await ui.find({ type: 'Markdown' })).toBeDefined()
  const before = board.calls.length
  await ui.press({ key: 'approve' })
  expect(board.calls.slice(before).map(c => c.args.join(' '))).toEqual(['permission approve 1100'])
  expect((await ui.find({ type: 'Text', text: /Approved #1100/ }))?.text).toContain('settings.json')
  expect(await ui.find({ key: 'approve' })).toBeUndefined()
  expect((await ui.find({ type: 'Text', text: /waiting on you/ }))?.text).toBe('1 waiting on you')
  await ui.unmount()
})

test('Reject sends a reject', async ($, on) => {
  const board = await opened($, on)
  const ui = await mount($)
  await ui.press({ key: 'open-questions-1100' })
  await ui.press({ key: 'reject' })
  expect(board.calls.some(c => c.args.join(' ') === 'permission reject 1100')).toBe(true)
  expect(await ui.find({ type: 'Text', text: /Rejected #1100/ })).toBeDefined()
  await ui.unmount()
})

test('a plain question answers with a typed reply', async ($, on) => {
  const board = await opened($, on)
  const ui = await mount($)
  await ui.press({ key: 'open-questions-1101' })
  expect(await ui.find({ key: 'approve' })).toBeUndefined()
  expect(await ui.find({ key: 'close' })).toBeDefined()
  await ui.input({ key: 'reply-1101', text: 'Ship it, then post the run.' })
  const sent = board.calls.find(c => c.args[0] === 'reply')
  expect(sent?.args).toEqual(['reply', '1101'])
  expect(sent?.stdin).toBe('Ship it, then post the run.')
  expect(await ui.find({ type: 'Text', text: /Replied on #1101/ })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /^You$/ })).toBeDefined()
  expect(await ui.find({ key: 'yes' })).toBeUndefined()
  await ui.unmount()
})

test('Yes and Close each send one write', async ($, on) => {
  const board = await opened($, on)
  const ui = await mount($)
  await ui.press({ key: 'open-questions-1101' })
  await ui.press({ key: 'yes' })
  expect(board.calls.find(c => c.args[0] === 'reply')?.stdin).toBe('Yes, go ahead.')
  await ui.press({ key: 'back' })
  await ui.press({ key: 'open-questions-1100' })
  expect(await ui.find({ key: 'close' })).toBeUndefined() // a permission rule is approved or rejected, not closed
  await ui.unmount()
})

test('Back returns to the list', async ($, on) => {
  await opened($, on)
  const ui = await mount($)
  await ui.press({ key: 'open-questions-1101' })
  await ui.press({ key: 'back' })
  expect(await ui.find({ key: 'open-questions-1100' })).toBeDefined()
  await ui.unmount()
})

test('a row with no thread draws as text', async ($, on) => {
  await opened($, on)
  const ui = await mount($)
  await ui.press({ key: 'tab-goals' })
  const goal = await ui.find({ type: 'Text', text: /Fast-Checkout/ })
  expect(goal?.text).toMatch(/^ Fast-Checkout: ship it\n running {2}· {2}lead x {2}· {2}\d+d ago$/)
  expect(goal?.text).not.toContain('opening') // a row with no thread is never "opening"
  expect(await ui.find({ type: 'Button', text: /Fast-Checkout/ })).toBeUndefined()
  await ui.press({ key: 'tab-wiki' })
  expect(await ui.find({ type: 'Text', text: /Nothing here/ })).toBeDefined()
  await ui.unmount()
})

test('a database error shows in red and keeps what is drawn', async ($, on) => {
  const board = await opened($, on)
  const ui = await mount($)
  board.state.fail = 'database is locked'
  await ui.press({ key: 'refresh' })
  expect((await ui.find({ type: 'Text', text: /database is locked/ }))?.text).toBe('✗ database is locked')
  expect(await ui.find({ key: 'open-questions-1100' })).toBeDefined()
  board.state.fail = ''
  await ui.press({ key: 'open-questions-1100' })
  await ui.press({ key: 'approve' })
  expect(await ui.find({ type: 'Text', text: /database is locked/ })).toBeUndefined()
  await ui.unmount()
})

test('every surface draws the list and a thread', async ($, on) => {
  await opened($, on)
  for (const surface of ['terminal', 'desktop', 'vscode', 'mobile'] as const) {
    const ui = await mount($, surface)
    await ui.press({ key: 'open-questions-1101' })
    expect(await ui.find({ type: 'Text', text: /#1101/ })).toBeDefined()
    if (surface === 'mobile') expect(await ui.find({ key: 'reply-1101' })).toBeUndefined()
    else expect(await ui.find({ key: 'reply-1101' })).toBeDefined()
    await ui.press({ key: 'back' })
    await ui.unmount()
  }
})

test('a wide pane shows the list and the open thread side by side', async ($, on) => {
  await opened($, on)
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop', props: { ...PANE.props, bodyColumns: 160 } })
  expect(await ui.find({ type: 'Text', text: /Pick a question/ })).toBeDefined()
  await ui.press({ key: 'open-questions-1100' })
  expect(await ui.find({ key: 'approve' })).toBeDefined()
  expect(await ui.find({ key: 'open-questions-1101' })).toBeDefined() // the list stays beside it
  expect(await ui.find({ key: 'back' })).toBeUndefined() // nothing to go back to
  await ui.press({ key: 'open-questions-1101' })
  expect(await ui.find({ key: 'yes' })).toBeDefined()
  await ui.unmount()
})
