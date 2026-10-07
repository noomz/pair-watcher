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
    const stdout = e.argv[2] === 'poll' ? JSON.stringify(answer) : ''
    return { value: { exitCode: stderr ? 2 : 0, stdout, stderr, isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('prompt.read', () => {
    const text = boxes.length > 1 ? boxes.shift()! : boxes[0]
    return { value: { text, cursor: text.length } }
  })
  // What lies beneath the plugins: a prompt enters after `delay` ms (slow settings hooks).
  let delay = 0
  on('prompt.submit', async (_$: any, e: { text: string }) => {
    if (delay) await clock.sleep(delay)
    submits.push(e.text)
    return { text: e.text }
  })
  on('ui.status', (_$: any, e: { text?: string }) => {
    statuses.push(e.text)
    return { value: undefined }
  })
  on('ui.invalidate', () => ({ value: undefined }))
  on('command.register', () => ({ value: undefined }))
  on('session.start', (_$: any, e: { cwd: string }) => ({ cwd: e.cwd }))
  on('turn.start', (_$: any, e: { turnId: string }) => ({ turnId: e.turnId }))
  on('turn.complete', () => ({ text: '' }))
  // One poll interval: the mod's own timer polls the mailbox.
  const tick = () => clock.advance(1000)
  return { runs, submits, statuses, tick, clock, slowSubmit: (ms: number) => (delay = ms) }
}

const start = ($: any) => $.session.start({ cwd: '/repo', surface: 'terminal', isInteractive: true })
const polls = (runs: Run[]) => runs.filter(r => r.argv[2] === 'poll').map(r => r.argv.slice(3))

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
