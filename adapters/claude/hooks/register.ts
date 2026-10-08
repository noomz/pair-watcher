import type { EngineInterface, Register } from 'claude-code'

// pair-watcher Claude mod (I8): idle delivery, status-line field, /commentary, /pw.
// Thin: the Watcher is its own process; this module asks the session mailbox through the hook
// shim and hands claimed items to the Worker. Nothing enters the system prompt, and every
// command returns {} so the model reads none of it.

const POLL_MS = 1000
const NOTE_MS = 5000
const PANE = 'commentary'

type Item = { id: string; text: string; token: string }
type Poll = { status: string | null; commentary: string[]; item: Item | null }

let session: string | undefined
let shim: string | undefined
let started = false
let running = false
// A prompt is entering: its `prompt.submit` chain resolves once its turn started or it was queued
// behind the running one. A typed prompt's turn starts ~2 s (or a slow settings hook later) after
// the box empties, so the box alone says idle too early.
let submitting = 0
// A prompt was queued behind the running turn: it starts next, at a turn.start.
let queued = false
let polling = false
let commentary: string[] = []
let status: string | undefined
let note: { text: string; until: number } | undefined

const adapter = ($: EngineInterface, args: string[]) =>
  $.process.run([shim!, 'adapter', ...args], { timeoutMs: 10_000 })

async function draft($: EngineInterface) {
  return (await $.prompt.read()).text.trim() !== ''
}

const busy = () => running || submitting > 0 || queued

async function showStatus($: EngineInterface) {
  const now = await $.clock.now()
  await $.ui.status(note && now < note.until ? note.text : status)
}

/** Reads the mailbox; at idle with an empty box it may get an item to deliver (V26). */
async function tick($: EngineInterface) {
  if (!session || !shim || polling) return
  polling = true
  try {
    const worker = busy() ? 'busy' : (await draft($)) ? 'draft' : 'idle'
    const r = await adapter($, ['poll', '--session', session, '--route', 'mod', '--worker', worker])
    if (r.exitCode !== 0 || !r.stdout.trim()) return
    const poll = JSON.parse(r.stdout) as Poll
    const changed = poll.commentary.join('\n') !== commentary.join('\n')
    commentary = poll.commentary
    status = poll.status ?? undefined
    await showStatus($)
    if (changed) await $.ui.invalidate('ui.render')
    if (poll.item) await deliver($, poll.item)
  } catch {
    // Fail-silent: the Worker never sees a pair-watcher error.
  } finally {
    polling = false
  }
}

// Never typed into the box, never mid-turn (V25). A draft or a turn that appeared since the poll
// wins: the claim is handed back and the item waits for the next idle or `Stop`.
async function deliver($: EngineInterface, item: Item) {
  const claim = ['--session', session!, '--id', item.id, '--token', item.token]
  if (busy() || (await draft($))) {
    await adapter($, ['release', ...claim])
    return
  }
  // Our own submit skips our `prompt.submit` hook; `polling` holds the next tick until it resolves.
  const r = await $.prompt.submit({ text: item.text })
  if ('drop' in r && r.drop) {
    await adapter($, ['release', ...claim])
    return
  }
  await adapter($, ['ack', ...claim])
}

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    // A Watcher's own `claude -p` loads installed mods too; it must never deliver (V58).
    if (await $.env.get('PAIR_WATCHER_GUARD')) return next(e)
    // `/clear` and resume start the session again: an attached session's id changes with them.
    session = (await $.env.get('PAIR_WATCHER_SESSION')) || (await $.session.id())
    if (started) return next(e)
    started = true
    const home = await $.env.get('HOME')
    shim = home ? `${home}/.pair-watcher/bin/pw-hook` : undefined
    await $.command.register({ name: 'commentary', description: 'Open the pair-watcher Commentary pane', immediate: true })
    await $.command.register({
      name: 'pw',
      description: 'pair-watcher: rule <id> watcher|worker|dismiss, undo, keep, mute <focus>, nochallenge, pause, primary',
      immediate: true,
    })
    $.clock.every(POLL_MS, () => tick($))
    return next(e)
  })

  on('prompt.submit', async ($, e, next) => {
    submitting++
    try {
      const r = await next(e)
      if (running && !('drop' in r && r.drop)) queued = true
      return r
    } finally {
      submitting--
    }
  })

  on('turn.start', async ($, e, next) => {
    running = true
    queued = false
    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    if (!e.agentId) running = false
    return next(e)
  })

  on('command.run', { command: 'commentary' }, async $ => {
    await $.ui.open({ id: PANE, title: 'Commentary' })
    return {}
  })

  // The reply shows on the status line for a few seconds, never in the transcript.
  on('command.run', { command: 'pw' }, async ($, e) => {
    if (!session || !shim) return {}
    const args = e.args.trim().split(/\s+/).filter(Boolean)
    const r = await adapter($, ['pw', '--session', session, '--', ...args]).catch(() => undefined)
    const text = (r?.stdout.trim() || r?.stderr.trim() || '').split('\n')[0]
    if (text) note = { text, until: (await $.clock.now()) + NOTE_MS }
    await showStatus($)
    return {}
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Text } = $.ui.resolve(e)
    const width = Math.max(10, e.props.bodyColumns - 2)
    const lines = commentary.length
      ? commentary.map(line => Text({ wrap: 'wrap', children: [line] }))
      : [Text({ dimColor: true, children: ['silent'] })]
    return Box({ flexDirection: 'column', width, paddingX: 1, children: lines })
  })
}
