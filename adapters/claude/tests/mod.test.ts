import { describe, expect, mock, test } from 'claude-code/testing'

const SHIM = '/h/.pair-watcher/bin/pw-hook'
const ITEM = { id: 'c1', text: '[pair-watcher · Challenge c1 · from the Watcher, not the human]', token: 't1' }

type Run = { argv: readonly string[] }

// A Worker session: the env `run` sets, the mailbox answering `poll` with `answer`.
// `box`: the prompt box, or what successive reads find in it (the last one stays).
function world(on: any, answer: object, box: string | string[] = '', stderr = '', env: Record<string, string> = {}) {
  const boxes = Array.isArray(box) ? [...box] : [box]
  mock.env(on, { HOME: '/h', PAIR_WATCHER_SESSION: 's1', ...env })
  const clock = mock.clock(on)
  const runs: Run[] = []
  const submits: string[] = []
  const statuses: (string | undefined)[] = []
  on('process.run', (_$: any, e: Run) => {
    runs.push({ argv: e.argv })
    // `press` answers with `answer.press`: a refusal line, or nothing.
    const stdout = e.argv[2] === 'poll' ? JSON.stringify(answer) : e.argv[2] === 'press' ? ((answer as any).press ?? '') : ''
    return { value: { exitCode: stderr ? 2 : 0, stdout, stderr, isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('prompt.read', () => {
    const text = boxes.length > 1 ? boxes.shift()! : boxes[0]
    return { value: { text, cursor: text.length } }
  })
  // What lies beneath the plugins: a prompt enters after `delay` ms (slow settings hooks).
  // `startsTurn`: like the engine, `next` resolves once the prompt's own turn started.
  let delay = 0
  let startTurn: ((text: string) => Promise<unknown>) | undefined
  on('prompt.submit', async (_$: any, e: { text: string }) => {
    if (delay) await clock.sleep(delay)
    submits.push(e.text)
    if (startTurn) await startTurn(e.text)
    return { text: e.text }
  })
  on('ui.status', (_$: any, e: { text?: string }) => {
    statuses.push(e.text)
    return { value: undefined }
  })
  on('command.register', () => ({ value: undefined }))
  on('session.start', (_$: any, e: { cwd: string }) => ({ cwd: e.cwd }))
  on('turn.start', (_$: any, e: { turnId: string }) => ({ turnId: e.turnId }))
  on('turn.complete', () => ({ text: '' }))
  // One poll interval: the mod's own timer polls the mailbox.
  const tick = () => clock.advance(1000)
  return {
    runs,
    submits,
    statuses,
    tick,
    clock,
    slowSubmit: (ms: number) => (delay = ms),
    engineSubmit: (fire: (text: string) => Promise<unknown>) => (startTurn = fire),
  }
}

const start = ($: any) => $.session.start({ cwd: '/repo', surface: 'terminal', isInteractive: true })
// The delivery part of each poll: what precedes the band and pane layout args.
const polls = (runs: Run[]) =>
  runs.filter(r => r.argv[2] === 'poll').map(r => r.argv.slice(3, r.argv.indexOf('--band')))

describe('idle delivery (V25, V26, I8)', () => {
  test('idle with an empty box: submit the claimed item, then ack it', async ($, on) => {
    const w = world(on, { status: '1 open · Challenge c1 pending', commentary: [], item: ITEM })
    await start($)
    await w.tick()
    expect(polls(w.runs)).toEqual([['--session', 's1', '--route', 'mod', '--worker', 'idle']])
    expect(w.runs[0].argv[0]).toBe(SHIM)
    expect(w.submits).toEqual([ITEM.text])
    expect(w.runs.at(-1)?.argv.slice(2)).toEqual(['ack', '--session', 's1', '--id', 'c1', '--token', 't1'])
    expect(w.statuses.at(-1)).toBe('1 open · Challenge c1 pending')
  })

  test('a draft in the box holds: worker=draft, nothing submitted', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: ITEM }, 'half-typed ')
    await start($)
    await w.tick()
    expect(polls(w.runs)).toEqual([['--session', 's1', '--route', 'mod', '--worker', 'draft']])
    expect(w.submits).toEqual([])
    expect(w.runs.some(r => r.argv[2] === 'ack')).toBe(false)
    expect(w.statuses.at(-1)).toBeUndefined()
  })

  test('a running turn holds: worker=busy until the turn completes', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null })
    await start($)
    await $.turn.start({ text: 'go', turnId: 'u1' })
    await w.tick()
    await $.turn.complete({ reason: 'answer', answer: 'done', durationMs: 1, isAborted: false, turnId: 'u1' } as any)
    await w.tick()
    expect(polls(w.runs).map(p => p.at(-1))).toEqual(['busy', 'idle'])
  })
})

describe('turn boundaries (V25, V21)', () => {
  test('a typed prompt is busy until its turn starts, however slow the settings hooks', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null })
    await start($)
    w.slowSubmit(30_000)
    const entering = $.prompt.submit({ text: 'human prompt' })
    await w.clock.advance(15_000)
    expect(polls(w.runs).every(p => p.at(-1) === 'busy')).toBe(true)
    await w.clock.advance(15_000)
    await entering
    await w.tick()
    expect(polls(w.runs).at(-1)?.at(-1)).toBe('idle')
  })

  test('a prompt whose own turn started before submit resolved is not queued: idle after it completes', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null })
    await start($)
    w.engineSubmit(text => $.turn.start({ text, turnId: 't1' }))
    await $.prompt.submit({ text: 'human prompt' })
    await w.tick()
    await $.turn.complete({ reason: 'answer', answer: 'done', durationMs: 1, isAborted: false, turnId: 't1' } as any)
    await w.tick()
    expect(polls(w.runs).map(p => p.at(-1))).toEqual(['busy', 'idle'])
  })

  test('a prompt queued behind the running turn keeps it busy past turn.complete', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null })
    await start($)
    await $.turn.start({ text: 'go', turnId: 'u1' })
    await $.prompt.submit({ text: 'next, please' })
    await $.turn.complete({ reason: 'answer', answer: 'done', durationMs: 1, isAborted: false, turnId: 'u1' } as any)
    await w.tick()
    await $.turn.start({ text: 'next, please', turnId: 'u2' })
    await $.turn.complete({ reason: 'answer', answer: 'ok', durationMs: 1, isAborted: false, turnId: 'u2' } as any)
    await w.tick()
    expect(polls(w.runs).map(p => p.at(-1))).toEqual(['busy', 'idle'])
  })

  test('an attached session follows its new id after /clear', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null }, '', '', { PAIR_WATCHER_SESSION: '' })
    const ids = ['claude-a', 'claude-b']
    on('session.id', () => ({ value: ids.shift() }))
    await start($)
    await start($)
    await w.tick()
    expect(polls(w.runs)).toEqual([['--session', 'claude-b', '--route', 'mod', '--worker', 'idle']])
  })

  test('a draft typed between poll and submit wins: the claim is released', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: ITEM }, ['', 'half'])
    await start($)
    await w.tick()
    expect(w.submits).toEqual([])
    expect(w.runs.at(-1)?.argv.slice(2)).toEqual(['release', '--session', 's1', '--id', 'c1', '--token', 't1'])
  })

  test('a session that starts again keeps one poll timer', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null })
    await start($)
    await start($)
    await w.tick()
    expect(polls(w.runs).length).toBe(1)
  })
})

describe('Watcher guard (V58)', () => {
  test('under the guard the mod never polls or delivers', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: ITEM }, '', '', { PAIR_WATCHER_GUARD: '1' })
    await start($)
    await w.tick()
    expect(w.runs).toEqual([])
    expect(w.submits).toEqual([])
  })
})

describe('commands return {} (I8)', () => {
  test('/pw runs the session command and shows its answer on the status line only', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null }, '', 'pair-watcher: not implemented yet (T13)\n')
    await start($)
    const r = await $.command.run({ command: 'pw', args: 'rule c1 watcher' })
    expect(r).toEqual({})
    expect(w.runs.at(-1)?.argv.slice(1)).toEqual(['adapter', 'pw', '--session', 's1', '--', 'rule', 'c1', 'watcher'])
    expect(w.statuses.at(-1)).toBe('pair-watcher: not implemented yet (T13)')
  })

  test('/commentary opens the pane, which draws the open Concerns', async ($, on) => {
    const w = world(on, { status: '1 open', commentary: ['warn c2 [correctness] off-by-one in the pager'], item: null })
    const opened: string[] = []
    on('ui.open', (_$: any, e: { id: string }) => {
      opened.push(e.id)
      return { value: { isPlaced: true } }
    })
    await start($)
    await w.tick()
    expect(await $.command.run({ command: 'commentary' })).toEqual({})
    expect(opened).toEqual(['commentary'])
    const ui = await $.ui.mount({
      plugin: 'pair-watcher',
      surface: 'terminal',
      component: 'Pane',
      requestId: 'commentary',
      props: { title: 'Commentary', isFocused: false, bodyColumns: 48, placement: 'dock', scroll: { offset: 0, bodyRows: 40 }, view: {} },
    } as any)
    const texts = (await ui.findAll({ type: 'Text' })).map((t: { text: string }) => t.text).join('\n')
    expect(texts).toContain('off-by-one in the pager')
  })
})

// §T19: the band (mini + Buttons 1/2/3, V76) and the mod pane (Button rows, keys p b h y n d s u).
const MINI = [{ style: 'red_bold', text: '▲ Debate unresolved: 5th review round (c2)' }]
const ASK = { id: 'e1', title: 'Debate unresolved: 5th review round (c2)', sides: true }
const PANE_BOARD = {
  opens: true,
  band: { mini: MINI, ask: ASK },
  pane: {
    lines: [
      [{ style: 'bold', text: 'NEEDS YOU 1 ' }],
      [{ style: 'red', text: '┃ ' }, { style: 'red_bold', text: '▲ 12:00 Debate unresolved' }],
      [{ style: 'plain', text: '  Watcher (approach): run the failing test' }],
      [{ style: 'bold', text: 'OPEN 1 ' }],
      [{ style: 'plain', text: '  c1 ' }, { style: 'dim', text: 'note' }, { style: 'plain', text: ' [correctness] Retry loop' }],
    ],
    items: [null, 'e1', 'e1', null, 'c1'],
    order: ['e1', 'c1'],
    selected: 'e1',
  },
}
const BAND_PROPS = { hasSurvey: false, isWorking: false, maxRows: 10, bodyColumns: 70, scroll: { offset: 0, bodyRows: 10 }, view: {} }
const PANE_PROPS = { title: 'pair-watcher', isFocused: true, bodyColumns: 66, placement: 'dock', scroll: { offset: 0, bodyRows: 30 }, view: {} }

const presses = (runs: Run[]) => runs.filter(r => r.argv[2] === 'press').map(r => r.argv.slice(3))

function opens(on: any) {
  const opened: string[] = []
  on('ui.open', (_$: any, e: { id: string }) => {
    opened.push(e.id)
    return { value: { isPlaced: true } }
  })
  on('ui.focus', () => ({}))
  return opened
}

const band = ($: any) =>
  $.ui.mount({ plugin: 'pair-watcher', surface: 'terminal', component: 'AbovePrompt', props: BAND_PROPS } as any)
const pane = ($: any) =>
  $.ui.mount({ plugin: 'pair-watcher', surface: 'terminal', component: 'Pane', requestId: 'commentary', props: PANE_PROPS } as any)

describe('band (I8, I17, V76)', () => {
  test('mini line and digit Buttons; 1 rules for the Watcher from the band', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null, board: { opens: true, band: { mini: MINI, ask: ASK } } })
    opens(on)
    await start($)
    await w.tick()
    expect(polls(w.runs)[0]).toEqual(['--session', 's1', '--route', 'mod', '--worker', 'idle'])
    expect(w.runs.find(r => r.argv[2] === 'poll')?.argv.slice(9)).toEqual(['--band', '80'])
    const ui = await band($)
    const texts = (await ui.findAll({ type: 'Text' })).map((t: { text: string }) => t.text).join('\n')
    expect(texts).toContain('Debate unresolved')
    const buttons = await ui.findAll({ type: 'Button' })
    expect(buttons.map((b: any) => [b.key, b.props.hotkey, b.props.label])).toEqual([
      ['band:1', '1', 'Watcher'],
      ['band:2', '2', 'Worker'],
      ['band:3', '3', 'dismiss'],
    ])
    await ui.press({ key: 'band:1' })
    expect(presses(w.runs)).toEqual([['--session', 's1', '--key', '1', '--band']])
  })

  test('no digit Buttons while the human has a draft (V76)', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null, board: { opens: true, band: { mini: MINI, ask: ASK } } }, 'half-typed')
    opens(on)
    await start($)
    await w.tick()
    expect(await (await band($)).findAll({ type: 'Button' })).toEqual([])
  })

  test('a blocked Escalation offers 3 only (V72)', async ($, on) => {
    const blocked = { ...ASK, sides: false }
    const w = world(on, { status: null, commentary: [], item: null, board: { opens: true, band: { mini: MINI, ask: blocked } } })
    opens(on)
    await start($)
    await w.tick()
    expect((await (await band($)).findAll({ type: 'Button' })).map((b: any) => b.key)).toEqual(['band:3'])
  })

  test('after a ruling: `<id> → <side> · u undo` until the human prompts again', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null, board: { opens: true, band: { mini: MINI, ruled: 'e1 → Watcher' } } })
    opens(on)
    await start($)
    await w.tick()
    const ui = await band($)
    expect((await ui.find({ type: 'Text', text: /e1 → Watcher · / }))).toBeDefined()
    await ui.press({ key: 'band:u' })
    expect(presses(w.runs)).toEqual([['--session', 's1', '--key', 'u', '--band']])
    await $.prompt.submit({ text: 'carry on' })
    await w.tick()
    expect(await (await band($)).findAll({ type: 'Button' })).toEqual([])
  })
})

describe('mod pane (I8, I17, V67)', () => {
  test('opens itself on the first Concern, Escalation or BLIND', async ($, on) => {
    const answer: any = { status: null, commentary: [], item: null, board: { opens: false, band: { mini: MINI } } }
    const w = world(on, answer)
    const opened = opens(on)
    await start($)
    await w.tick()
    expect(opened).toEqual([])
    answer.board = PANE_BOARD
    await w.tick()
    await w.tick()
    expect(opened).toEqual(['commentary'])
  })

  test('Board rows are Buttons; keys act on the focused row; p asks for a remark', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null, board: PANE_BOARD })
    opens(on)
    await start($)
    await w.tick()
    const ui = await pane($)
    // The drawn size goes to the binary for the layout.
    await w.tick()
    expect(w.runs.filter(r => r.argv[2] === 'poll').at(-1)?.argv).toContain('64x28')
    const rows = (await ui.findAll({ type: 'Button' })).map((b: any) => b.key)
    expect(rows).toEqual(['row:e1', 'row:c1', 'key:p', 'key:b', 'key:h', 'key:y', 'key:n', 'key:d', 'key:s', 'key:u'])
    const keys = (await ui.findAll({ type: 'Button' })).filter((b: any) => b.key.startsWith('key:'))
    expect(keys.map((b: any) => b.props.hotkey).join('')).toBe('pbhynd su'.replace(' ', ''))

    // The ring lands on the c1 row: `h` and `d` act on c1.
    await $.ui.focus({ component: 'Pane', requestId: 'commentary', plugin: 'pair-watcher', element: 'row:c1', origin: { kind: 'person' } } as any)
    expect(w.runs.filter(r => r.argv[2] === 'poll').at(-1)?.argv).toContain('c1')
    await ui.press({ key: 'key:h' })
    expect(presses(w.runs).at(-1)).toEqual(['--session', 's1', '--key', 'h', '--selected', 'c1'])

    // `p`: the remark Input, then the promotion with it (V20).
    await ui.press({ key: 'key:p' })
    expect(presses(w.runs)).toHaveLength(1)
    await ui.input({ key: 'remark', text: ' too slow ' })
    expect(presses(w.runs).at(-1)).toEqual(['--session', 's1', '--key', 'p', '--selected', 'c1', '--remark', 'too slow'])
    expect(await ui.find({ type: 'Input' })).toBeUndefined()

    // Enter on a row: its Debate log expands.
    await ui.press({ key: 'row:e1' })
    expect(w.runs.filter(r => r.argv[2] === 'poll').at(-1)?.argv.slice(-4)).toEqual(['--selected', 'e1', '--expanded', 'e1'])
  })

  test('a refused key shows on the status line', async ($, on) => {
    const w = world(on, { status: null, commentary: [], item: null, board: PANE_BOARD, press: 'nothing selected\n' })
    opens(on)
    await start($)
    await w.tick()
    const ui = await pane($)
    await ui.press({ key: 'key:d' })
    expect(w.statuses).toContain('nothing selected')
  })
})
