import type { EngineInterface, Register } from 'claude-code'

// pair-watcher Claude mod (I8): idle delivery, status-line field, /commentary, /pw (watch and
// stop included, V112), the band
// (mini line + Buttons 1/2/3) and the mod pane (the Board as Button rows, keys p b h y n d s u).
// Thin: the Watcher is its own process; this module asks the session mailbox through the hook
// shim, hands claimed items to the Worker and forwards keys. The Board is drawn by the binary;
// the module holds only the focused row and the expanded logs. Nothing enters the system prompt,
// and every command returns {} so the model reads none of it.
// `claude-session` backend (V107): the poll also claims the Watcher's completion requests; each
// runs on this session's own client by `$.model.complete` and its result goes back to the mailbox.
// Only a claimed request is ever run.

const POLL_MS = 1000
const NOTE_MS = 5000
const PANE = 'commentary'
const ROW = 'row:'

type Item = { id: string; text: string; token: string }
type Span = { style: Style; text: string }
type Style = 'plain' | 'dim' | 'bold' | 'green' | 'yellow' | 'red' | 'red_bold' | 'cyan' | 'magenta' | 'banner'
type Band = { mini: Span[]; ask?: { id: string; title: string; sides: boolean }; ruled?: string }
type Board = {
  opens: boolean
  band?: Band
  pane?: { lines: Span[][]; items: (string | null)[]; order: string[]; selected: string | null }
}
// A Watcher completion request (Rust `session::Request`); `prompt` blocks are ModelTextBlocks.
type Completion = {
  id: string
  model: string
  system: string
  prompt: { text: string; cache?: true }[]
  maxTokens: number
  timeoutMs?: number
}
type Poll = {
  status: string | null
  commentary: string[]
  item: Item | null
  board?: Board
  completions?: Completion[]
}

// The pane's keys (I17): each acts on the focused row; `p` asks for a remark first (V20).
const KEYS: [string, string][] = [
  ['p', 'promote'],
  ['b', '+$5'],
  ['h', 'helpful'],
  ['y', 'Watcher'],
  ['n', 'Worker'],
  ['d', 'dismiss'],
  ['s', 'keep'],
  ['u', 'undo'],
]

const TEXT: Record<Style, object> = {
  plain: {},
  dim: { dimColor: true },
  bold: { bold: true },
  green: { color: 'success' },
  yellow: { color: 'warning' },
  red: { color: 'error' },
  red_bold: { color: 'error', bold: true },
  cyan: { color: 'suggestion' },
  magenta: { color: 'claude' },
  banner: { color: 'error', inverse: true },
}

let session: string | undefined
let shim: string | undefined
// The Worker's cwd: `/pw watch` names it, with the harness, for a session with no transcript yet.
let cwd: string | undefined
let started = false
let running = false
// A prompt is entering: its `prompt.submit` chain resolves once its turn started or it was queued
// behind the running one. A typed prompt's turn starts ~2 s (or a slow settings hook later) after
// the box empties, so the box alone says idle too early.
let submitting = 0
// A prompt was queued behind the running turn: it starts next, at a turn.start.
let queued = false
// `turn.start`s so far: a prompt whose own turn started while it entered is not queued, though
// its `prompt.submit` chain resolves with that turn already running.
let starts = 0
let polling = false
let commentary: string[] = []
let status: string | undefined
let note: { text: string; until: number } | undefined
let board: Board | undefined
// What the binary lays the band and the pane out for: the widths and rows they were last drawn at.
let bandColumns = 80
let paneSize: { columns: number; rows: number } | undefined
let opened = false
// The mod pane's focused row (an Escalation or Concern id): the arrows and Tab move the ring over
// the row Buttons, and `ui.focus` names the one it lands on.
let focused: string | undefined
const expanded = new Set<string>()
// `p` pressed: the remark Input stands under the rows for this Concern.
let remarkFor: string | undefined
let empty = true
// The band's last ruling, hidden once the human prompts again.
let ruledSeen: string | undefined

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

async function say($: EngineInterface, text: string) {
  if (text) note = { text, until: (await $.clock.now()) + NOTE_MS }
  await showStatus($)
}

/** A pane key or band Button: the binary logs it; a refusal shows on the status line. */
async function press($: EngineInterface, key: string, band: boolean, remark = '') {
  if (!session || !shim) return
  const args = ['press', '--session', session, '--key', key]
  if (band) args.push('--band')
  if (!band && focused) args.push('--selected', focused)
  if (remark) args.push('--remark', remark)
  const r = await adapter($, args).catch(() => undefined)
  await say($, r?.stdout.trim().split('\n')[0] ?? '')
  await tick($)
}

function pollArgs(worker: string) {
  const args = ['poll', '--session', session!, '--route', 'mod', '--worker', worker, '--band', String(bandColumns), '--completions']
  if (paneSize) {
    args.push('--pane', `${paneSize.columns}x${paneSize.rows}`)
    if (focused) args.push('--selected', focused)
    for (const id of expanded) args.push('--expanded', id)
  }
  return args
}

/** Reads the mailbox; at idle with an empty box it may get an item to deliver (V26). */
async function tick($: EngineInterface) {
  if (!session || !shim || polling) return
  polling = true
  try {
    const hasDraft = await draft($)
    const worker = busy() ? 'busy' : hasDraft ? 'draft' : 'idle'
    const r = await adapter($, pollArgs(worker))
    if (r.exitCode !== 0) return
    // Not watched (never, stopped, or ended): nothing of an earlier Watcher stays up (V112).
    if (!r.stdout.trim()) return void (await unwatched($))
    const poll = JSON.parse(r.stdout) as Poll
    for (const c of poll.completions ?? []) void complete($, c)
    const changed =
      poll.commentary.join('\n') !== commentary.join('\n') ||
      JSON.stringify(poll.board) !== JSON.stringify(board) ||
      empty === hasDraft
    commentary = poll.commentary
    board = poll.board
    empty = !hasDraft
    status = poll.status ?? undefined
    await showStatus($)
    // The pane opens on the first Concern, Escalation or BLIND (V67); after that the human's.
    if (board?.opens && !opened) {
      opened = true
      await $.ui.open({ id: PANE, title: 'pair-watcher' })
    }
    if (changed) await $.ui.invalidate('ui.render')
    if (poll.item) await deliver($, poll.item)
  } catch {
    // Fail-silent: the Worker never sees a pair-watcher error.
  } finally {
    polling = false
  }
}

async function unwatched($: EngineInterface) {
  if (status === undefined && !board && !commentary.length) return
  status = undefined
  board = undefined
  commentary = []
  await showStatus($)
  await $.ui.invalidate('ui.render')
}

/** Runs one claimed request; its result, whatever it is, goes back to the Watcher (V108). */
async function complete($: EngineInterface, c: Completion) {
  const request = { model: c.model, system: c.system, prompt: c.prompt, maxTokens: c.maxTokens }
  const timeout = c.timeoutMs && c.timeoutMs > 0 ? { timeoutMs: Math.min(c.timeoutMs, 2_147_483_647) } : {}
  // A request the engine refuses to send (a blocked model, a bad cap) is the Watcher's to fix.
  const zero = { input_tokens: 0, output_tokens: 0, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 }
  const result = await $.model
    .complete({ ...request, ...timeout })
    .catch(e => ({ isAnswered: false, reason: 'api-error', status: 400, error: String(e).slice(0, 200), usage: zero }))
  await adapter($, ['complete-result', '--session', session!, '--id', c.id, '--result', JSON.stringify(result)]).catch(
    () => undefined,
  )
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
    cwd = e.cwd
    if (started) return next(e)
    started = true
    const home = await $.env.get('HOME')
    shim = home ? `${home}/.pair-watcher/bin/pw-hook` : undefined
    await $.command.register({ name: 'commentary', description: 'Open the pair-watcher Commentary pane', immediate: true })
    await $.command.register({
      name: 'pw',
      description: 'pair-watcher: watch, stop, rule <id> watcher|worker|dismiss, undo, keep, mute <focus>, nochallenge, pause, primary',
      immediate: true,
    })
    $.clock.every(POLL_MS, () => tick($))
    return next(e)
  })

  on('prompt.submit', async ($, e, next) => {
    ruledSeen = board?.band?.ruled
    submitting++
    const before = starts
    try {
      const r = await next(e)
      if (running && starts === before && !('drop' in r && r.drop)) queued = true
      return r
    } finally {
      submitting--
    }
  })

  on('turn.start', async ($, e, next) => {
    starts++
    running = true
    queued = false
    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    if (!e.agentId) running = false
    return next(e)
  })

  on('command.run', { command: 'commentary' }, async $ => {
    opened = true
    await $.ui.open({ id: PANE, title: 'pair-watcher' })
    return {}
  })

  on('ui.close', async ($, e, next) => {
    if (e.id === PANE) paneSize = undefined
    return next(e)
  })

  // The mod-held current row: where the ring lands on a row Button. The key Buttons keep it.
  on('ui.focus', { requestId: PANE }, async ($, e, next) => {
    const r = await next(e)
    if (!r.deny && e.element?.startsWith(ROW) && e.element.slice(ROW.length) !== focused) {
      focused = e.element.slice(ROW.length)
      remarkFor = undefined
      await tick($)
    }
    return r
  })

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    bandColumns = e.props.bodyColumns
    const band = board?.band
    if (e.props.hasSurvey || !band) return next(e)
    const { Box, Button, Text } = $.ui.resolve(e)
    const children = [Text({ wrap: 'truncate', children: spans(Text, band.mini) })]
    const ask = band.ask
    // Digit Buttons only while the box is empty (V76); a blocked Escalation takes `3` only (V72).
    if (ask && empty) {
      const digits: [string, string][] = ask.sides ? [['1', 'Watcher'], ['2', 'Worker']] : []
      digits.push(['3', 'dismiss'])
      const buttons = digits.map(([key, label]) =>
        Button({ key: `band:${key}`, hotkey: key, plain: true, label, onPress: () => press($, key, true) }),
      )
      children.push(Box({ gap: 2, children: [Text({ dimColor: true, children: [ask.id] }), ...buttons] }))
    } else if (band.ruled && band.ruled !== ruledSeen) {
      const undo = Button({ key: 'band:u', hotkey: 'u', plain: true, label: 'undo', onPress: () => press($, 'u', true) })
      children.push(Box({ children: [Text({ dimColor: true, children: [`${band.ruled} · `] }), undo] }))
    }
    return Box({ flexDirection: 'column', children })
  })

  // The reply shows on the status line for a few seconds, never in the transcript. `/pw watch`
  // starts a Watcher on this very session (`attach <own id>`), `/pw stop` ends it (V112).
  on('command.run', { command: 'pw' }, async ($, e) => {
    if (!session || !shim) return {}
    const args = e.args.trim().split(/\s+/).filter(Boolean)
    const hint = args[0] === 'watch' && cwd ? ['--harness', 'claude', '--cwd', cwd] : []
    const r = await adapter($, ['pw', '--session', session, ...hint, '--', ...args]).catch(() => undefined)
    const text = (r?.stdout.trim() || r?.stderr.trim() || '').split('\n')[0]
    if (text) note = { text, until: (await $.clock.now()) + NOTE_MS }
    await showStatus($)
    await tick($)
    return {}
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const elements = $.ui.resolve(e)
    const { Box, Button, Text } = elements
    // A surface without text entry promotes without a remark.
    const Input = 'Input' in elements ? elements.Input : undefined
    const width = Math.max(20, e.props.bodyColumns - 2)
    // Room for the key row and the remark Input under the Board.
    const size = { columns: width, rows: Math.max(3, e.props.scroll.bodyRows - 2) }
    if (size.columns !== paneSize?.columns || size.rows !== paneSize?.rows) {
      paneSize = size
      void tick($)
    }
    const pane = board?.pane
    if (!pane) {
      const lines = commentary.length
        ? commentary.map(line => Text({ wrap: 'wrap', children: [line] }))
        : [Text({ dimColor: true, children: ['silent'] })]
      return Box({ flexDirection: 'column', width, paddingX: 1, children: lines })
    }
    // One Button per item: its first line; the item's other lines follow as text.
    const rows = []
    for (let i = 0; i < pane.lines.length; i++) {
      const id = pane.items[i]
      const line = spans(Text, pane.lines[i] ?? [])
      if (id && pane.items[i - 1] !== id) {
        const onPress = () => toggle($, id)
        rows.push(Button({ key: ROW + id, plain: true, onPress, children: line }))
      } else {
        rows.push(Text({ wrap: 'truncate', children: line.length ? line : [' '] }))
      }
    }
    if (remarkFor && Input) {
      const id = remarkFor
      const onSubmit = (text: string) => {
        remarkFor = undefined
        $.ui.invalidate('ui.render')
        return press($, 'p', false, text.trim())
      }
      rows.push(Input({ key: 'remark', label: `remark for ${id}: `, placeholder: 'optional', submitLabel: 'promote', autoFocus: true, onSubmit }))
    }
    const keys = KEYS.map(([key, label]) =>
      Button({ key: `key:${key}`, hotkey: key, plain: true, dimColor: true, label, onPress: () => onKey($, key, !!Input) }),
    )
    rows.push(Box({ gap: 1, flexWrap: 'wrap', children: keys }))
    return Box({ flexDirection: 'column', width, paddingX: 1, children: rows })
  })
}

type Elements = ReturnType<EngineInterface['ui']['resolve']>

const spans = (Text: Elements['Text'], line: Span[]) =>
  line.map(s => Text({ ...TEXT[s.style], children: [s.text] }))

async function onKey($: EngineInterface, key: string, canType: boolean) {
  if (key === 'p') {
    if (!focused) return say($, 'focus a Concern row to promote')
    if (!canType) return press($, 'p', false)
    remarkFor = focused
    return $.ui.invalidate('ui.render')
  }
  await press($, key, false)
}

/** Enter on a row: its Debate log expands or folds (V63). */
async function toggle($: EngineInterface, id: string) {
  focused = id
  if (!expanded.delete(id)) expanded.add(id)
  await tick($)
}
