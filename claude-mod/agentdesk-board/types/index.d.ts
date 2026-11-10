/** One row of a list tab, as db.py's `tab` op returns it. */
export type Row = {
  thread_id: number | null
  title: string
  who: string
  when: string
  badge: string
}
export type Message = {
  id: number
  ts: string
  author: string
  author_kind: string
  body: string
}
export type Proposal = { id: number; rule: string; scope: string; status: string }
/** An open thread, as db.py's `thread` op returns it. */
export type OpenThread = {
  thread_id: number
  channel: string
  status: string
  subject: string
  opened_by: string
  kind: string
  waiting: boolean
  proposal: Proposal | null
  messages: Message[]
}
/** The line under the tabs: what the last action did, or why it failed. */
export type Note = { text: string; isError: boolean }

declare module 'claude-code' {
  interface PluginState {
    'agentdesk-board': {
      tab: Shaped<string>
      rows: Shaped<Row[]>
      waiting: Shaped<number>
      open: Shaped<OpenThread | null>
      note: Shaped<Note>
      busy: Shaped<boolean>
    }
  }
}
