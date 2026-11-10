/** One row of a list tab, as db.py's `tab` op returns it. */
export type Row = {
  thread_id: number | null
  title: string
  who: string
  when: string
  badge: string
}
export type Tab = { rows: Row[]; waiting: number; total: number }
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
/** db.py's `snapshot`: every tab, and every waiting question in full. */
export type Snapshot = {
  tabs: Record<string, Tab>
  threads: Record<string, OpenThread>
  waiting: number
}
/** The line under the tabs: what the last action did, or why it failed. */
export type Note = { text: string; isError: boolean }

declare module 'claude-code' {
  interface PluginState {
    'agentdesk-board': {
      snap: Shaped<Snapshot | null>
      tab: Shaped<string>
      open: Shaped<OpenThread | null>
      loading: Shaped<number | null>
      note: Shaped<Note>
      busy: Shaped<string>
    }
  }
}
