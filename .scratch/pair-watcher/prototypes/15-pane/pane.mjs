#!/usr/bin/env node
// PROTOTYPE — throwaway. Answers ticket 15 "Pane prototype": what should `pair-watcher pane` look like?
// Three structurally different variants of the pane, replaying one canned event file
// (unproductive-repetition trial scenario). Switch variants with ←/→. Not production code.
//
//   node pane.mjs                         interactive (A Stream, B Board, C Quiet)
//   node pane.mjs --variant=B             start on variant B
//   node pane.mjs --fallback              90% step = fallback_model takeover instead of pause
//   node pane.mjs --dump --variant=A --at=31 --width=64 --height=30   print one frame, no TTY needed

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const args = Object.fromEntries(process.argv.slice(2).map(a => {
  const [k, v] = a.replace(/^--/, '').split('=');
  return [k, v ?? true];
}));
const here = dirname(fileURLToPath(import.meta.url));
let events = readFileSync(args.events ?? join(here, 'repetition.jsonl'), 'utf8')
  .split('\n').filter(Boolean).map(l => JSON.parse(l));

if (args.fallback) {
  events = events.flatMap(e => {
    if (e.k === 'status' && e.state === 'paused')
      return [{ t: e.t, k: 'status', state: 'thinned', spend: e.spend, model: 'sonnet-5.5', notes: ['fallback_model', 'same vendor as Worker'], why: '90% of ceiling: fallback_model sonnet-5.5 takes over instead of pausing' }];
    if (e.t === '20:00' && (e.k === 'human' || e.k === 'status')) return [];
    if (e.k === 'wake' && e.why === 'catch-up') return [{ ...e, why: 'turn end' }];
    return [e];
  });
}

// ---------- colour + width helpers ----------
const useColor = !args.dump || args.color;
const sgr = code => s => useColor ? `\x1b[${code}m${s}\x1b[0m` : s;
const C = { dim: sgr('2'), bold: sgr('1'), red: sgr('31'), yel: sgr('33'), grn: sgr('32'), cyan: sgr('36'), mag: sgr('35'), inv: sgr('7'), redb: sgr('1;31') };
const ANSI = /\x1b\[[0-9;]*m/g;
const vis = s => s.replace(ANSI, '').length;
function trunc(s, w) {
  if (vis(s) <= w) return s;
  let out = '', n = 0;
  for (const tok of s.split(/(\x1b\[[0-9;]*m)/)) {
    if (ANSI.test(tok)) { ANSI.lastIndex = 0; out += tok; continue; }
    for (const ch of tok) { if (n >= w - 1) break; out += ch; n++; }
  }
  return out + '…' + (useColor ? '\x1b[0m' : '');
}
const pad = (s, w) => { s = trunc(s, w); return s + ' '.repeat(Math.max(0, w - vis(s))); };
function wrap(text, w, indent = 0) {
  const words = text.split(' '), lines = [];
  let cur = '';
  for (const word of words) {
    const lim = lines.length ? w - indent : w;
    if (cur && (cur + ' ' + word).length > lim) { lines.push(cur); cur = word; }
    else cur = cur ? cur + ' ' + word : word;
  }
  if (cur) lines.push(cur);
  return lines.map((l, i) => (i ? ' '.repeat(indent) : '') + l);
}

// ---------- reducer: events[0..pos] -> pane state ----------
const LEVEL = { silent: '▁', note: '▃', warn: '▅', loud: '█' };
function reduce(evs, pos) {
  const st = {
    status: {}, concerns: new Map(), esc: [], log: [], timeline: [], wakes: [],
    calls: 0, tok: 0, openChallenge: null, worker: '', lastAdvice: null, ended: false, lastNotify: null, t: '00:00',
  };
  const tl = (e, sym, text, color = s => s, extra = {}) => st.timeline.push({ t: e.t, sym, text, color, ...extra });
  const speak = (e, lvl) => { const w = st.wakes.at(-1); if (w && w.t === e.t) w.level = lvl === 'loud' || w.level === 'loud' ? 'loud' : (lvl === 'warn' || w.level === 'warn' ? 'warn' : lvl); };
  for (const e of evs.slice(0, pos + 1)) {
    st.t = e.t;
    const c = e.id && st.concerns.get(e.id);
    switch (e.k) {
      case 'status':
        Object.assign(st.status, e);
        if (e.why) {
          const sym = e.state === 'paused' ? '‖' : e.state === 'thinned' ? '◐' : '●';
          tl(e, sym, `${e.state}: ${e.why}`, e.state === 'paused' ? C.red : C.yel, { kind: 'status' });
          st.log.push({ t: e.t, kind: 'status', text: `${e.state}: ${e.why}` });
        }
        break;
      case 'worker': st.worker = `${e.t}  ${e.text}`; break;
      case 'wake':
        st.wakes.push({ t: e.t, why: e.why, level: 'silent' });
        st.calls += e.calls; st.tok += e.tok; st.status.spend = e.spend;
        break;
      case 'concern':
        if (e.op === 'add') {
          const n = { ...e, state: 'open', debate: [], at: e.t };
          st.concerns.set(e.id, n); st.lastAdvice = n; speak(e, e.sev);
          tl(e, '+', `${e.id} ${e.sev} [${e.focus}] ${e.head}`, e.sev === 'warn' ? C.yel : s => s, { id: e.id, focus: e.focus });
        } else if (e.op === 'update') {
          Object.assign(c, { sev: e.sev, head: e.head, detail: e.detail, refs: e.refs, at: e.t }); st.lastAdvice = c; speak(e, e.sev);
          tl(e, '↑', `${e.id} ${e.sev} [${c.focus}] ${e.head}`, e.sev === 'warn' ? C.yel : s => s, { id: e.id, focus: c.focus });
        } else if (e.op === 'resolve') {
          if (st.openChallenge === e.id) st.openChallenge = null;
          if (c.state !== 'withdrawn') c.state = 'resolved';
          c.why = e.why;
          tl(e, '✓', `${e.id} resolved: ${e.why}`, C.grn, { id: e.id, focus: c.focus });
        }
        break;
      case 'human':
        if (e.op === 'dismiss') { c.state = 'dismissed'; tl(e, '✗', `${e.id} dismissed by human (final)`, C.dim, { id: e.id, focus: c.focus }); }
        if (e.op === 'promote') { c.remark = e.remark; tl(e, '⇡', `${e.id} promoted by human: "${e.remark}"`, C.cyan, { id: e.id, focus: c.focus }); }
        if (e.op === 'promote-refused') { tl(e, '·', `promote ${e.id} refused: ${e.why}`, C.dim, { kind: 'log' }); st.log.push({ t: e.t, kind: 'refused', text: `promote ${e.id} refused: ${e.why}` }); }
        if (e.op === 'raise-ceiling') { tl(e, '$', `human added $${e.by}; resuming`, C.cyan, { kind: 'status' }); }
        break;
      case 'challenge':
        if (e.op === 'ready') {
          c.state = 'challenge'; st.openChallenge = e.id; speak(e, 'loud');
          const text = e.auto ? 'auto-Challenge queued (approach-level, evidence verified)' : `Challenge queued (promoted)${c.remark ? ` + remark "${c.remark}"` : ''}`;
          c.debate.push({ t: e.t, kind: 'ready', text });
          tl(e, '⚑', `${e.id} ${text}`, C.cyan, { id: e.id, focus: c.focus });
        } else if (e.op === 'delivered') {
          c.state = 'debate'; c.pending = null; c.exch = e.n;
          const text = `Challenge ${e.n}/2 sent via ${e.route}`;
          c.debate.push({ t: e.t, kind: 'sent', text });
          tl(e, '│', text, C.cyan, { id: e.id, focus: c.focus, indent: true });
        }
        break;
      case 'pending': {
        c.pending = e.why;
        const text = `pending: ${e.why}`;
        c.debate.push({ t: e.t, kind: 'pending', text });
        tl(e, '│', text, C.dim, { id: e.id, focus: c.focus, indent: true });
        break;
      }
      case 'verdict': {
        const text = `Worker: ${e.v} — ${e.reason}`;
        c.debate.push({ t: e.t, kind: 'verdict', text });
        tl(e, '│', text, s => s, { id: e.id, focus: c.focus, indent: true });
        break;
      }
      case 'rebuttal': {
        const text = `Watcher (2/2): "${e.quote}" → ${e.ground}`;
        c.debate.push({ t: e.t, kind: 'rebut', text }); speak(e, 'loud');
        tl(e, '│', text, C.cyan, { id: e.id, focus: c.focus, indent: true });
        break;
      }
      case 'debate': {
        st.openChallenge = null;
        c.outcome = e.outcome;
        c.state = e.outcome === 'unresolved' ? (e.escalate ? 'escalated' : 'unresolved-quiet') : e.outcome;
        const text = e.outcome === 'unresolved'
          ? (e.escalate ? 'unresolved → Escalation (notified)' : 'unresolved, not escalated (below escalate_unresolved)')
          : `${e.outcome}${e.why ? `: ${e.why}` : ''}`;
        c.debate.push({ t: e.t, kind: 'outcome', text });
        tl(e, '└', text, e.escalate ? C.red : C.dim, { id: e.id, focus: c.focus, indent: true });
        break;
      }
      case 'escalation':
        if (e.op === 'ruled') {
          const x = st.esc.find(x => x.id === e.id); x.open = false; x.ruledBy = e.by;
          if (c && c.state === 'escalated') c.state = 'ruled';
          tl(e, '■', `${e.id} Escalation closed: ${e.by}`, C.dim, { kind: 'esc' });
        } else {
          st.esc.push({ ...e, open: true }); st.lastNotify = e; speak(e, 'loud');
          tl(e, '▲', e.title, C.redb, { kind: 'esc' });
        }
        break;
      case 'blocked': break;
      case 'log': {
        const sym = e.kind === 'diag' ? '!' : '·';
        tl(e, sym, e.text, e.kind === 'diag' ? C.mag : C.dim, { kind: 'log' });
        st.log.push(e);
        break;
      }
      case 'session':
        st.ended = true;
        let n = 0;
        for (const x of st.concerns.values()) if (['open', 'challenge', 'debate'].includes(x.state)) { x.state = 'closed'; n++; }
        tl(e, '■', `session ended; ${n} open Concerns closed`, C.dim, { kind: 'status' });
        break;
    }
  }
  return st;
}

// ---------- shared pieces ----------
const isOpen = c => ['open', 'challenge', 'debate'].includes(c.state);
const focusOk = (ui, f) => !ui.focus || !f || f === ui.focus;
const money = n => `$${n.toFixed(2)}`;

function statusLine(st, w) {
  const s = st.status, spend = `${money(s.spend ?? 0)}/$${s.ceiling}`;
  const notes = (s.notes ?? []).length ? C.yel(` · ${s.notes.join(' · ')}`) : '';
  if (s.state === 'paused') return [C.inv(C.red(pad(` ‖ PAUSED ${spend} — not watching; silence ≠ all clear `, w))), C.red('   press $ to add $5 and resume')];
  if (s.state === 'thinned') return [C.yel(`◐ thinned ${spend}`) + C.dim(` checkpoints off · ${s.model}`) + notes];
  return [C.grn(`● watching ${spend}`) + C.dim(` · ${s.model} · Companion`) + notes];
}

function escBox(x, w) {
  const inner = w - 4;
  const lines = [...wrap(`▲ ${x.t} ${x.title}`, inner, 2).map(l => C.redb(l)), ...x.lines.flatMap(l => wrap(l, inner, 9))];
  return [C.red('┏' + '━'.repeat(w - 2) + '┓'), ...lines.map(l => C.red('┃ ') + pad(l, inner) + C.red(' ┃')), C.red('┗' + '━'.repeat(w - 2) + '┛')];
}

// ---------- Variant A: Stream (chronological log, newest at bottom, Escalations pinned) ----------
function variantA(st, ui, w, h, bare = false) {
  const head = bare ? [] : statusLine(st, w);
  const pinned = bare ? [] : st.esc.filter(x => x.open).flatMap(x => escBox(x, w));
  const body = [];
  for (const e of st.timeline) {
    if (!focusOk(ui, e.focus)) continue;
    const prefix = `${e.t} ${e.indent ? '  ' : ''}${e.sym} `;
    wrap(e.text, w - prefix.length).forEach((l, i) => body.push(i ? e.color(' '.repeat(prefix.length) + l) : C.dim(e.t) + e.color(prefix.slice(5) + l)));
  }
  const room = h - head.length - pinned.length - (bare ? 0 : 1);
  const shown = body.slice(Math.max(0, body.length - room));
  return bare ? shown : [...head, ...pinned, C.dim('─'.repeat(w)), ...shown];
}

// ---------- Variant B: Board (state-grouped Concern list, Debate log under its Concern) ----------
function variantB(st, ui, w, h) {
  const out = [...statusLine(st, w)];
  const openEsc = st.esc.filter(x => x.open);
  const sec = (name, n) => out.push(C.bold(`${name}${n !== undefined ? ` (${n})` : ''}`) + C.dim(' ' + '─'.repeat(Math.max(0, w - name.length - 5))));
  if (openEsc.length) { sec('NEEDS YOU', openEsc.length); openEsc.forEach(x => out.push(...escBox(x, w))); }
  const all = [...st.concerns.values()].filter(c => focusOk(ui, c.focus));
  const debating = all.filter(c => c.state === 'challenge' || c.state === 'debate');
  const open = all.filter(c => c.state === 'open').sort((a, b) => (b.sev === 'warn') - (a.sev === 'warn'));
  const closed = all.filter(c => !isOpen(c));
  const selectable = [...debating, ...open, ...(ui.showClosed ? closed : [])];
  ui.selIds = selectable.map(c => c.id);
  const selId = selectable[Math.min(ui.sel, selectable.length - 1)]?.id;
  const row = (c, extra) => {
    const sel = c.id === selId;
    const sevTxt = c.sev === 'warn' ? C.yel('warn') : C.dim('note');
    wrap(`${sel ? '›' : ' '} ${c.id} ${c.sev} [${c.focus}] ${c.head}`, w, 3).forEach((l, i) => {
      if (i === 0) l = l.replace(` ${c.sev} [${c.focus}]`, ` ${sevTxt} ${C.dim(`[${c.focus}]`)}`);
      out.push(sel ? C.bold(l) : l);
    });
    if (extra) extra();
  };
  if (debating.length) {
    sec('IN DEBATE', debating.length);
    debating.forEach(c => row(c, () => c.debate.forEach(d => {
      const col = d.kind === 'pending' ? C.dim : d.kind === 'verdict' ? (s => s) : C.cyan;
      wrap(`${d.t} ${d.text}`, w - 5, 6).forEach(l => out.push(col('   │ ' + l)));
    })));
  }
  sec('OPEN', open.length);
  if (!open.length) out.push(C.dim('  nothing open'));
  open.forEach(c => row(c, c.id === selId ? () => {
    [...(c.detail ?? []), ...(c.refs?.length ? [`refs: ${c.refs.join(', ')}`] : [])].forEach(d => wrap(d, w - 5, 5).forEach(l => out.push(C.dim('     ' + l))));
  } : null));
  sec(`CLOSED ${ui.showClosed ? '' : '[c]'}`, closed.length);
  if (ui.showClosed) closed.forEach(c => {
    const label = { resolved: C.grn('resolved'), dismissed: C.dim('dismissed'), ruled: C.dim('escalated → human ruled'), escalated: C.red('escalated'), withdrawn: C.dim('withdrawn'), closed: C.dim('closed'), 'unresolved-quiet': C.dim('unresolved, quiet') }[c.state] ?? c.state;
    out.push(trunc(`  ${c.id} ${label} ${C.dim(c.head)}`, w));
    if (c.debate.length && c.id === selId) c.debate.forEach(d => wrap(`${d.t} ${d.text}`, w - 5, 6).forEach(l => out.push(C.dim('   │ ' + l))));
  });
  const ticker = st.log.slice(-2).map(l => (l.kind === 'diag' ? C.mag : C.dim)(trunc(`${l.t} ${l.kind === 'diag' ? '!' : '·'} ${l.text}`, w)));
  const room = h - ticker.length - 1;
  const body = out.length > room ? [...out.slice(0, room - 1), C.dim(`  … ${out.length - room + 1} more lines`)] : out;
  return [...body, ...Array(Math.max(0, room - body.length)).fill(''), C.dim('─'.repeat(w)), ...ticker];
}

// ---------- Variant C: Quiet (identity header + attention level; only what needs the human) ----------
function variantC(st, ui, w, h) {
  const s = st.status;
  const openC = [...st.concerns.values()].filter(c => isOpen(c) && focusOk(ui, c.focus));
  const openEsc = st.esc.filter(x => x.open);
  const level = s.state === 'paused' ? C.red('PAUSED') : openEsc.length ? C.redb('NEEDS YOU') : st.openChallenge ? C.cyan('DEBATE') : openC.some(c => c.sev === 'warn') ? C.yel('WATCH') : C.grn('QUIET');
  const spark = st.wakes.slice(-16).map(x => LEVEL[x.level]).join('');
  const out = [
    C.bold(`Watcher · Companion · ${s.model}`) + ((s.notes ?? []).length ? C.yel(` · ${s.notes.join(' · ')}`) : ''),
    C.dim('observes only · never edits · never blocks the Worker'),
    `attention ${level}  ${C.dim(spark)}`,
    C.dim(`wakes ${st.wakes.length} · calls ${st.calls} · read ${Math.round(st.tok / 1000)}K · ${money(s.spend ?? 0)}/$${s.ceiling}`),
    ...(s.state === 'watching' ? [] : statusLine(st, w)),
    C.dim('─'.repeat(w)),
  ];
  if (ui.overlay === 'log') return [...out, C.dim('full log  [l] close'), ...variantA(st, ui, w, h - out.length - 1, true)];
  openEsc.forEach(x => out.push(...escBox(x, w)));
  if (st.openChallenge) {
    const c = st.concerns.get(st.openChallenge), last = c.debate.at(-1);
    out.push(C.cyan(trunc(`⚑ ${c.id} ${c.exch ? `Debate ${c.exch}/2` : 'Challenge queued'} · ${c.head}`, w)), C.dim(trunc(`  ${last.t} ${last.text}`, w)));
  }
  if (!openEsc.length && !st.openChallenge && s.state !== 'paused') out.push('', C.grn('  Nothing needs you.'), '');
  const adv = st.lastAdvice;
  if (adv) {
    const tail = isOpen(adv) ? '' : ` (${adv.state})`;
    out.push(C.dim('last advice ') + trunc(`${adv.at} ${adv.id} ${adv.head}`, w - 12 - tail.length) + C.dim(tail));
  }
  const warns = openC.filter(c => c.sev === 'warn').length, notes = openC.length - warns;
  out.push(C.dim(`open: ${warns} warn · ${notes} note    [l] log`));
  const diag = st.log.filter(l => l.kind === 'diag').at(-1);
  if (diag) out.push(C.mag(trunc(`! ${diag.t} ${diag.text}`, w)));
  return out;
}

const VARIANTS = [['A', 'Stream', variantA], ['B', 'Board', variantB], ['C', 'Quiet', variantC]];

// ---------- frame ----------
function frame(ui, cols, rows) {
  const st = reduce(events, ui.pos);
  const [key, name, fn] = VARIANTS[ui.v];
  const w = ui.width, inner = w - 2, h = rows - 6;
  const lines = fn(st, ui, inner, h - 2).slice(0, h - 2);
  const title = ` pair-watcher pane ${ui.focus ? `· Focus: ${ui.focus} ` : ''}`;
  const out = [
    '┌' + title + '─'.repeat(Math.max(0, inner - title.length)) + '┐',
    ...lines.map(l => '│' + pad(l, inner) + '│'),
    ...Array(Math.max(0, h - 2 - lines.length)).fill('│' + ' '.repeat(inner) + '│'),
    '└' + '─'.repeat(inner) + '┘',
  ];
  const ev = events[ui.pos];
  const notify = st.lastNotify ? C.red(`  last notification: ${st.lastNotify.t} ${st.lastNotify.title}`) : '';
  out.push(
    C.inv(` ◀ ${key} · ${name} ▶ `) + `  event ${ui.pos + 1}/${events.length} [${ev.t}] ${ui.playing ? '▶ playing' : '❚❚'}` + notify,
    C.dim('←/→ variant  space play  . , step  g end  r restart  w width  f focus  j/k select  d dismiss  P promote  $ +$5  c closed  l log  q quit'),
    C.dim(trunc(`scenario ${st.worker}`, cols)),
    C.dim(trunc(`event    ${JSON.stringify(ev)}`, cols)),
  );
  return { text: out.map(l => trunc(l, cols)).join('\n'), st };
}

// ---------- dump mode ----------
const ui = {
  v: Math.max(0, VARIANTS.findIndex(v => v[0] === String(args.variant ?? 'A').toUpperCase())),
  pos: args.at !== undefined ? Math.min(events.length - 1, Number(args.at) - 1) : 0,
  width: Number(args.width ?? 64), playing: false, focus: null, sel: 0, showClosed: false, overlay: null, selIds: [],
};
if (args.dump) {
  if (args.closed) ui.showClosed = true;
  if (args.log) ui.overlay = 'log';
  console.log(frame(ui, 200, Number(args.height ?? 34)).text);
  process.exit(0);
}

// ---------- interactive mode ----------
const out = process.stdout;
const FOCUSES = [null, 'approach', 'tests', 'security', 'best practice'];
let lastNotified; // undefined = don't ring on this draw
function draw() {
  const { text, st } = frame(ui, out.columns || 120, out.rows || 40);
  out.write('\x1b[H\x1b[2J' + text);
  if (st.lastNotify && st.lastNotify !== lastNotified && lastNotified !== undefined) out.write('\x07');
  lastNotified = st.lastNotify;
}
function inject(ev) { events.splice(ui.pos + 1, 0, { t: events[ui.pos].t, ...ev }); ui.pos++; }
function target(st) {
  if (ui.v === 1 && ui.selIds.length) return st.concerns.get(ui.selIds[Math.min(ui.sel, ui.selIds.length - 1)]);
  return [...st.concerns.values()].filter(isOpen).at(-1);
}
out.write('\x1b[?1049h\x1b[?25l');
process.stdin.setRawMode(true); process.stdin.resume(); process.stdin.setEncoding('utf8');
const quit = () => { out.write('\x1b[?25h\x1b[?1049l'); process.exit(0); };
process.stdin.on('data', k => {
  const st = reduce(events, ui.pos);
  if (k === 'q' || k === '\x03') return quit();
  if (k === '\x1b[C') ui.v = (ui.v + 1) % VARIANTS.length;
  else if (k === '\x1b[D') ui.v = (ui.v + VARIANTS.length - 1) % VARIANTS.length;
  else if (['1', '2', '3'].includes(k)) ui.v = Number(k) - 1;
  else if (k === ' ') ui.playing = !ui.playing;
  else if (k === '.') ui.pos = Math.min(events.length - 1, ui.pos + 1);
  else if (k === ',') ui.pos = Math.max(0, ui.pos - 1);
  else if (k === 'g') ui.pos = events.length - 1;
  else if (k === 'r') { ui.pos = 0; lastNotified = undefined; }
  else if (k === 'w') ui.width = ui.width === 64 ? Math.min(110, (out.columns || 110) - 1) : 64;
  else if (k === 'f') ui.focus = FOCUSES[(FOCUSES.indexOf(ui.focus) + 1) % FOCUSES.length];
  else if (k === 'j' || k === '\x1b[B') ui.sel = Math.min(ui.selIds.length - 1, ui.sel + 1);
  else if (k === 'k' || k === '\x1b[A') ui.sel = Math.max(0, ui.sel - 1);
  else if (k === 'c') ui.showClosed = !ui.showClosed;
  else if (k === 'l') ui.overlay = ui.overlay === 'log' ? null : 'log';
  else if (k === 'd') { const c = target(st); if (c && isOpen(c)) inject({ k: 'human', op: 'dismiss', id: c.id }); }
  else if (k === 'P') {
    const c = target(st);
    if (c && st.openChallenge) inject({ k: 'human', op: 'promote-refused', id: c.id, why: `Challenge ${st.openChallenge} is open` });
    else if (c && c.state === 'open') { inject({ k: 'human', op: 'promote', id: c.id, remark: '(none)' }); inject({ k: 'challenge', op: 'ready', id: c.id, auto: false }); }
  } else if (k === '$') {
    const s = st.status, ceiling = s.ceiling + 5;
    inject({ k: 'human', op: 'raise-ceiling', by: 5 });
    inject({ k: 'status', state: s.spend / ceiling < 0.6 ? 'watching' : 'thinned', ceiling, spend: s.spend, why: 'resumed: catch-up wake (cumulative diff + digest)' });
  }
  draw();
});
out.on('resize', draw);
setInterval(() => {
  if (!ui.playing) return;
  if (ui.pos >= events.length - 1) ui.playing = false; else ui.pos++;
  draw();
}, 900);
draw();
