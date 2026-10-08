//! Pure pane layout (V66): `layout(board, ui, width, rows)` → styled lines, at most `rows` lines
//! of at most `width` columns, no horizontal scroll, hanging indents. Board (V63), mini mode for
//! one or two rows, Stream and help overlays (V68). No clock, no motion (V64): the same input
//! always draws the same frame.

use std::collections::HashSet;

use unicode_width::UnicodeWidthChar;

use crate::budget::Stage;
use crate::concern::{Concern, Severity, State, Tag};
use crate::escalation::Cause;
use crate::pane::{Board, Entry, Tone, hhmm, money};

/// Width of the Debate log's event column (V63 fixed columns).
const EVENT_COL: usize = 17;
/// IN DEBATE shows this many Debate entries until expanded (V63).
const DEBATE_TAIL: usize = 2;
const TICKER_LINES: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Plain,
    Dim,
    Bold,
    Green,
    Yellow,
    Red,
    RedBold,
    Cyan,
    Magenta,
    /// Inverse red: PAUSED (budget) and BLIND (V64).
    Banner,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub style: Style,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line(pub Vec<Span>);

impl Line {
    fn one(style: Style, text: impl Into<String>) -> Line {
        Line::default().push(style, text)
    }

    /// Control characters (escape sequences in model or payload text) never reach the
    /// terminal.
    fn push(mut self, style: Style, text: impl Into<String>) -> Line {
        let text = text.into().chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        self.0.push(Span { style, text });
        self
    }

    pub fn plain(&self) -> String {
        self.0.iter().map(|s| s.text.as_str()).collect()
    }

    pub fn width(&self) -> usize {
        self.0.iter().map(|s| cols(&s.text)).sum()
    }

    /// With SGR colours; every span resets after itself.
    pub fn ansi(&self) -> String {
        let mut out = String::new();
        for s in &self.0 {
            let code = match s.style {
                Style::Plain => {
                    out.push_str(&s.text);
                    continue;
                }
                Style::Dim => "2",
                Style::Bold => "1",
                Style::Green => "32",
                Style::Yellow => "33",
                Style::Red => "31",
                Style::RedBold => "1;31",
                Style::Cyan => "36",
                Style::Magenta => "35",
                Style::Banner => "1;37;41",
            };
            out.push_str(&format!("\x1b[{code}m{}\x1b[0m", s.text));
        }
        out
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overlay {
    #[default]
    None,
    Stream,
    Help,
    /// `r`: the repo's Standing rulings (V78).
    Rulings,
}

/// A remark being typed for `P` (V20).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub concern: String,
    pub text: String,
}

/// Pane-local view state; never written anywhere (V70).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ui {
    /// Selected Concern or Escalation id; unknown or `None` = the first selectable.
    pub selected: Option<String>,
    /// `f`: one Focus only.
    pub filter: Option<String>,
    /// `c`: CLOSED expanded.
    pub closed: bool,
    pub overlay: Overlay,
    /// Concerns whose Debate log is expanded (Enter).
    pub expanded: HashSet<String>,
    pub input: Option<Input>,
    /// A local line for the ticker (a refused key), newest.
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    pub lines: Vec<Line>,
    /// Concern ids whose row is on screen (V69 `shown`).
    pub visible: Vec<String>,
    /// Selectable ids in screen order (`j`/`k`).
    pub order: Vec<String>,
    /// The selection after defaulting.
    pub selected: Option<String>,
}

impl Frame {
    pub fn plain(&self) -> Vec<String> {
        self.lines.iter().map(Line::plain).collect()
    }
}

/// One body row: what may be trimmed first has the higher `prio`; 0 is never trimmed (V65).
struct Row {
    line: Line,
    prio: u8,
    concern: Option<String>,
    /// A NEEDS YOU item: outranks the `… more` marker when even protected rows overflow.
    needs: bool,
}

fn row(line: Line, prio: u8) -> Row {
    Row { line, prio, concern: None, needs: false }
}

pub fn layout(b: &Board, ui: &Ui, width: usize, rows: usize) -> Frame {
    let width = width.max(20);
    if rows == 0 {
        return Frame::default();
    }
    let order = selectable(b, ui);
    let selected = ui.selected.clone().filter(|s| order.contains(s)).or_else(|| order.first().cloned());
    if rows <= 2 {
        let mut lines = vec![mini(b, width)];
        lines.resize(rows, Line::default());
        return Frame { lines, visible: Vec::new(), order, selected };
    }
    let mut top = header(b, ui, width);
    if let Some(strip) = strip(b, width) {
        top.push(strip);
    }
    let mut bottom = ticker(b, ui, width);
    if !bottom.is_empty() {
        bottom.insert(0, Line::one(Style::Dim, "─".repeat(width)));
    }
    top.truncate(rows - 1);
    let mut body = match ui.overlay {
        Overlay::None => board(b, ui, selected.as_deref(), width, false),
        Overlay::Stream => stream(b, ui, width, rows.saturating_sub(top.len() + bottom.len())),
        Overlay::Help => help(width),
        Overlay::Rulings => rulings(b, selected.as_deref(), width),
    };
    // What may never be trimmed (V65) outranks the ticker, then the wake strip.
    let keep = body.iter().filter(|r| r.prio == 0).count();
    while !bottom.is_empty() && top.len() + bottom.len() + keep > rows {
        bottom.remove(0);
    }
    if bottom.len() == 1 {
        bottom.clear();
    }
    if top.len() + keep > rows && strip_shown(b) {
        top.pop();
    }
    // Still too tall: every item on one line, so each keeps a row (V65).
    if top.len() + keep > rows && ui.overlay == Overlay::None {
        body = board(b, ui, selected.as_deref(), width, true);
    }
    let (body, visible) = fit(body, rows.saturating_sub(top.len() + bottom.len()));
    let lines = top.into_iter().chain(body).chain(bottom).collect();
    let visible = if ui.overlay == Overlay::None { visible } else { Vec::new() };
    Frame { lines, visible, order, selected }
}

/// Ids `j`/`k` move over: NEEDS YOU, IN DEBATE, OPEN, then CLOSED when expanded.
fn selectable(b: &Board, ui: &Ui) -> Vec<String> {
    if ui.overlay == Overlay::Rulings {
        return b.held.iter().map(|r| r.id.clone()).collect();
    }
    let mut out: Vec<String> = b.needs_you().map(|e| e.id.clone()).collect();
    out.extend(debating(b, ui).map(|c| c.id.clone()));
    out.extend(open(b, ui).into_iter().map(|c| c.id.clone()));
    if ui.closed {
        out.extend(closed(b, ui).into_iter().map(|(c, _)| c.id.clone()));
    }
    out
}

fn filtered<'a>(b: &'a Board, ui: &'a Ui) -> impl Iterator<Item = &'a Concern> {
    b.concerns.all().iter().filter(move |c| ui.filter.as_ref().is_none_or(|f| *f == c.focus))
}

fn debating<'a>(b: &'a Board, ui: &'a Ui) -> impl Iterator<Item = &'a Concern> {
    filtered(b, ui).filter(move |c| c.state == State::Open && b.in_debate() == Some(c.id.as_str()))
}

/// Open Concerns not in Debate, `warn` first, oldest first within each. An escalated one waits
/// in NEEDS YOU, then CLOSED.
fn open<'a>(b: &'a Board, ui: &'a Ui) -> Vec<&'a Concern> {
    let mut out: Vec<&Concern> = filtered(b, ui)
        .filter(|c| {
            c.state == State::Open
                && c.tag != Some(Tag::Escalated)
                && b.in_debate() != Some(c.id.as_str())
                && b.closed_as(&c.id).is_none()
        })
        .collect();
    out.sort_by_key(|c| c.severity != Severity::Warn);
    out
}

fn closed<'a>(b: &'a Board, ui: &'a Ui) -> Vec<(&'a Concern, String)> {
    filtered(b, ui).filter_map(|c| b.closed_as(&c.id).map(|end| (c, end))).collect()
}

fn header(b: &Board, ui: &Ui, width: usize) -> Vec<Line> {
    if let Some((from, reason)) = b.blind.get(&None) {
        let first = format!("‖ BLIND since {} — not watching; silence ≠ all clear", b.time(*from));
        return banner(&[first, format!("  {reason}")], width);
    }
    if b.budget.stage() == Stage::Paused && !b.ended {
        let first = format!(
            "‖ PAUSED {}/{} — not watching; silence ≠ all clear",
            money(&b.budget, true),
            money(&b.budget, false)
        );
        return banner(&[first, "  press $ to add $5 and resume".to_owned()], width);
    }
    let (word, style) = if b.ended {
        ("■ session ended".to_owned(), Style::Dim)
    } else if b.user_paused {
        ("‖ PAUSED (you)".to_owned(), Style::Yellow)
    } else if let Some(since) = b.lagging.get(&None) {
        (format!("◐ lagging since {} · retrying", b.time(*since)), Style::Yellow)
    } else if b.budget.stage() >= Stage::Thinned {
        ("◐ thinned".to_owned(), Style::Yellow)
    } else {
        ("● watching".to_owned(), Style::Green)
    };
    let mut rest = vec![format!("{}/{}", money(&b.budget, true), money(&b.budget, false))];
    let mut models: Vec<&str> = b.models.values().map(|(m, ..)| m.as_str()).collect();
    models.dedup();
    if !models.is_empty() {
        rest.push(models.join(", "));
    }
    rest.push(if b.swarm { "Swarm" } else { "Companion" }.to_owned());
    if b.models.values().any(|(_, same, _)| *same) {
        rest.push("same vendor as Worker".to_owned());
    }
    if b.models.values().any(|(.., quota)| *quota) {
        rest.push("shares Worker quota".to_owned());
    }
    if let Some(m) = &b.fallback {
        rest.push(format!("fallback {m} (primary failing)"));
    }
    if b.rulings_held > 0 {
        rest.push(format!("rulings {}", b.rulings_held));
    }
    if b.lost_ms >= 60_000 {
        rest.push(format!("lost {}m", b.lost_ms / 60_000));
    }
    if !b.muted.is_empty() {
        rest.push(format!("muted: {}", b.muted.join(", ")));
    }
    if b.nochallenge {
        rest.push("no auto-Challenge".to_owned());
    }
    if b.observe_only {
        rest.push("observe-only".to_owned());
    }
    for focus in b.lagging.keys().flatten() {
        rest.push(format!("lagging: {focus}"));
    }
    if let Some(f) = &ui.filter {
        rest.push(format!("filter: {f}"));
    }
    let text = format!("{word} {}", rest.join(" · "));
    let mut out: Vec<Line> = wrap(&text, width, 2)
        .into_iter()
        .enumerate()
        .map(|(i, l)| match i {
            0 if l.starts_with(&word) => Line::one(style, word.clone()).push(Style::Dim, &l[word.len()..]),
            _ => Line::one(Style::Dim, l),
        })
        .collect();
    // A Swarm Watcher gone BLIND: red, its own line (V64, V89).
    for (focus, (from, reason)) in b.blind.iter().filter_map(|(f, v)| f.as_ref().map(|f| (f, v))) {
        let text = format!("‖ BLIND ({focus}) since {}: {reason}", b.time(*from));
        out.extend(wrap(&text, width, 2).into_iter().map(|l| Line::one(Style::Red, l)));
    }
    out
}

fn banner(texts: &[String], width: usize) -> Vec<Line> {
    texts.iter().flat_map(|t| wrap(t, width, 2)).map(|l| Line::one(Style::Banner, pad(&l, width))).collect()
}

fn strip_shown(b: &Board) -> bool {
    b.last_wake.is_some()
}

/// `  wakes ··■·· · last 11:40 silent` (V63); none before the first wake.
fn strip(b: &Board, width: usize) -> Option<Line> {
    let (t, word, _) = b.last_wake?;
    let suffix = format!(" · last {} {word}", b.time(t));
    let room = width.saturating_sub(cols("  wakes ") + cols(&suffix));
    let cells: String =
        b.cells.iter().rev().take(room).map(|c| c.glyph).collect::<Vec<_>>().into_iter().rev().collect();
    Some(Line::one(Style::Dim, "  wakes ").push(Style::Plain, cells).push(Style::Dim, suffix))
}

fn section(name: &str, n: usize, hints: &str, width: usize) -> Line {
    let head = format!("{name} {n} ");
    let room = width.saturating_sub(cols(&head));
    let line = Line::one(Style::Bold, head);
    if room > cols(hints) + 3 && !hints.is_empty() {
        line.push(Style::Dim, format!("{} {hints}", "─".repeat(room - cols(hints) - 1)))
    } else {
        line.push(Style::Dim, "─".repeat(room))
    }
}

/// `compact`: one line per item (no boxes, details or wrapping) for a pane too short for them.
fn board(b: &Board, ui: &Ui, selected: Option<&str>, width: usize, compact: bool) -> Vec<Row> {
    let mut out = Vec::new();
    let sel = |id: &str| selected == Some(id);

    let needs: Vec<_> = b.needs_you().collect();
    if !needs.is_empty() {
        // The header goes before any item does (V65).
        // `y`/`n` rule an unresolved Debate only; a blocked one is answered in the Worker (V72).
        let unresolved = needs.iter().any(|e| e.cause == Cause::Unresolved);
        let hints = if unresolved { "y · n · d · u · +" } else { "d · u · +" };
        out.push(row(section("NEEDS YOU", needs.len(), hints, width), 1));
        for e in needs {
            let at = b.raised.get(&e.id).map(|t| b.time(*t)).unwrap_or_default();
            if compact {
                let mark = if sel(&e.id) { "›" } else { " " };
                let line = Line::one(Style::RedBold, cut(&format!("{mark}▲ {at} {}", e.title), width));
                out.push(Row { needs: true, ..row(line, 0) });
                continue;
            }
            let inner = width - 4;
            let plus = if b.info.get(&e.id).is_some_and(|i| i.helpful) { " +" } else { "" };
            let mut body: Vec<Line> = wrap(&format!("▲ {at} {}{plus}", e.title), inner, 2)
                .into_iter()
                .map(|l| Line::one(Style::RedBold, l))
                .collect();
            for l in &e.lines {
                body.extend(wrap(l, inner, 2).into_iter().map(|l| Line::one(Style::Plain, l)));
            }
            let mark = if sel(&e.id) { "›" } else { " " };
            out.push(row(Line::one(Style::Red, format!("┏{}┓", "━".repeat(width - 2))), 0));
            for (i, l) in body.into_iter().enumerate() {
                let fill = " ".repeat(inner.saturating_sub(l.width()));
                let mut line = Line::one(Style::Red, format!("┃{}", if i == 0 { mark } else { " " }));
                line.0.extend(l.0);
                out.push(row(line.push(Style::Plain, fill).push(Style::Red, " ┃"), 0));
            }
            out.push(row(Line::one(Style::Red, format!("┗{}┛", "━".repeat(width - 2))), 0));
        }
    }

    let debate: Vec<&Concern> = debating(b, ui).collect();
    if !debate.is_empty() {
        out.push(row(section("IN DEBATE", debate.len(), "y · n · d · ⏎ log · +", width), 1));
        for c in debate {
            out.extend(concern_row(b, c, sel(&c.id), width, 0, compact));
            let entries = b.info.get(&c.id).map(|i| i.entries.as_slice()).unwrap_or_default();
            let expanded = ui.expanded.contains(&c.id);
            let skip = if expanded { 0 } else { entries.len().saturating_sub(DEBATE_TAIL) };
            if skip > 0 {
                out.push(row(Line::one(Style::Dim, format!("  … {skip} earlier [enter]")), 3));
            }
            for e in &entries[skip..] {
                out.extend(entry(b, e, width, expanded, Style::Cyan).into_iter().map(|l| row(l, 1)));
            }
            if let Some(p) = &b.pending {
                let t = p.waits.last().copied().or(entries.last().map(|e| e.t)).unwrap_or(0);
                let mut reason = p.line.trim_start_matches("pending: ").to_owned();
                if p.waits.len() >= 2 {
                    reason.push_str(&format!(" ({} wait)", ordinal(p.waits.len())));
                }
                let e = Entry { t, event: "pending".to_owned(), reason };
                out.extend(entry(b, &e, width, !compact, Style::Dim).into_iter().map(|l| row(l, 0)));
            }
        }
    }

    let open = open(b, ui);
    if !open.is_empty() {
        out.push(row(section("OPEN", open.len(), "d · P · + · m", width), 2));
        for c in open {
            let selected = sel(&c.id);
            out.extend(concern_row(b, c, selected, width, if selected { 0 } else { 2 }, compact));
            if selected && !compact {
                let refs = (!c.refs.is_empty()).then(|| format!("refs: {}", c.refs.join(", ")));
                for d in c.detail.iter().cloned().chain(refs) {
                    out.extend(
                        wrap(&d, width - 5, 2).into_iter().map(|l| row(Line::one(Style::Dim, format!("     {l}")), 1)),
                    );
                }
            }
        }
    }

    let closed = closed(b, ui);
    if !closed.is_empty() {
        out.push(row(section("CLOSED", closed.len(), "c · s", width), 4));
        if ui.closed {
            for (c, end) in closed {
                let selected = sel(&c.id);
                let mark = if selected { "›" } else { " " };
                let line = Line::one(if selected { Style::Bold } else { Style::Plain }, format!("{mark} {} ", c.id))
                    .push(Style::Dim, format!("{end} "))
                    .push(Style::Dim, c.headline.clone());
                let line = clip_line(line, width);
                out.push(Row { line, prio: if selected { 0 } else { 4 }, concern: Some(c.id.clone()), needs: false });
                if selected && !compact {
                    for e in b.info.get(&c.id).map(|i| i.entries.as_slice()).unwrap_or_default() {
                        out.extend(entry(b, e, width, true, Style::Dim).into_iter().map(|l| row(l, 1)));
                    }
                }
            }
        }
    }
    out
}

fn concern_row(b: &Board, c: &Concern, selected: bool, width: usize, prio: u8, compact: bool) -> Vec<Row> {
    let mark = if selected { "›" } else { " " };
    let tag = match c.tag {
        Some(Tag::Conceded) => " (Worker conceded; awaiting fix)",
        _ => "",
    };
    let plus = if b.info.get(&c.id).is_some_and(|i| i.helpful) { " +" } else { "" };
    let lead = format!("{mark} {} ", c.id);
    let sev = c.severity.name();
    let text = format!("{lead}{sev} [{}] {}{tag}{plus}", c.focus, c.headline);
    let base = if selected { Style::Bold } else { Style::Plain };
    let sev_style = if c.severity == Severity::Warn { Style::Yellow } else { Style::Dim };
    let lines = if compact { vec![cut(&text, width)] } else { wrap(&text, width, 4) };
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let line = if i == 0 && l.starts_with(&format!("{lead}{sev}")) {
                Line::one(base, lead.clone()).push(sev_style, sev).push(base, l[lead.len() + sev.len()..].to_owned())
            } else {
                Line::one(base, l)
            };
            Row { line, prio, concern: Some(c.id.clone()), needs: false }
        })
        .collect()
}

/// `  HH:MM event·padded reason` (V63); `full` wraps the reason instead of cutting it.
fn entry(b: &Board, e: &Entry, width: usize, full: bool, style: Style) -> Vec<Line> {
    let lead = format!("  {} {} ", b.time(e.t), pad(&cut(&e.event, EVENT_COL), EVENT_COL));
    let room = width.saturating_sub(cols(&lead));
    if !full || room < 10 {
        return vec![clip_line(Line::one(style, lead).push(Style::Plain, e.reason.clone()), width)];
    }
    let indent = " ".repeat(cols(&lead));
    wrap(&e.reason, room, 0)
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                Line::one(style, lead.clone()).push(Style::Plain, l)
            } else {
                Line::one(Style::Plain, format!("{indent}{l}"))
            }
        })
        .collect()
}

fn ticker(b: &Board, ui: &Ui, width: usize) -> Vec<Line> {
    let mut out: Vec<Line> = b
        .ticker
        .iter()
        .rev()
        .take(TICKER_LINES)
        .rev()
        .map(|t| {
            let style = match t.tone {
                Tone::Plain => Style::Dim,
                Tone::Diag => Style::Magenta,
                Tone::Refusal => Style::Red,
            };
            Line::one(style, cut(&format!("{} {}", b.time(t.t), t.text), width))
        })
        .collect();
    if let Some(note) = &ui.note {
        out.push(Line::one(Style::Red, cut(note, width)));
    }
    if let Some(input) = &ui.input {
        let prompt = format!("remark for {} (⏎ send · esc cancel): ", input.concern);
        let text = format!("{prompt}{}▏", input.text);
        // Keep the end of a long remark in view.
        out.push(Line::one(Style::Cyan, tail(&text, width)));
    }
    let keep = out.len().saturating_sub(TICKER_LINES);
    out.split_off(keep)
}

/// Drops the most trimmable rows, bottom first, until the body fits `rows`, leaving a
/// `… n more lines` marker; rows with priority 0 always stay (V65). Pads to `rows`.
fn fit(mut body: Vec<Row>, rows: usize) -> (Vec<Line>, Vec<String>) {
    let mut dropped = 0;
    if body.len() > rows {
        let target = rows.saturating_sub(1);
        while body.len() > target {
            let Some(max) = body.iter().map(|r| r.prio).max().filter(|p| *p > 0) else { break };
            let i = body.iter().rposition(|r| r.prio == max).expect("max exists");
            body.remove(i);
            dropped += 1;
        }
    }
    // Last resort, all of it protected: the bottom goes, and says so, unless the marker would
    // take a NEEDS YOU item's row.
    if body.len() > rows {
        let keep = if body[rows.saturating_sub(1)].needs { rows } else { rows.saturating_sub(1) };
        dropped += body.len() - keep;
        body.truncate(keep);
    }
    let mut visible: Vec<String> = Vec::new();
    for c in body.iter().filter_map(|r| r.concern.clone()) {
        if !visible.contains(&c) {
            visible.push(c);
        }
    }
    let mut lines: Vec<Line> = body.into_iter().map(|r| r.line).collect();
    if dropped > 0 && lines.len() < rows {
        lines.push(Line::one(Style::Dim, format!("  … {dropped} more lines")));
    }
    lines.resize(rows, Line::default());
    (lines, visible)
}

/// One line by priority: PAUSED = full BLIND > NEEDS YOU > partial BLIND / lagging > in Debate >
/// watching (V66).
fn mini(b: &Board, width: usize) -> Line {
    let (style, text) = if let Some((_, reason)) = b.blind.get(&None) {
        (Style::Banner, format!("‖ BLIND — {reason}; not watching"))
    } else if b.budget.stage() == Stage::Paused && !b.ended {
        (
            Style::Banner,
            format!("‖ PAUSED {}/{} — not watching · $ +$5", money(&b.budget, true), money(&b.budget, false)),
        )
    } else if let Some(e) = b.needs_you().next() {
        let more = b.needs_you().count() - 1;
        let more = if more > 0 { format!(" · +{more}") } else { String::new() };
        (Style::RedBold, format!("▲ {}{more}", e.title))
    } else if let Some((focus, _)) = b.blind.iter().find_map(|(f, v)| f.as_ref().map(|f| (f, v))) {
        (Style::Red, format!("‖ BLIND ({focus}) · others watching"))
    } else if let Some(since) = b.lagging.values().next() {
        (Style::Yellow, format!("◐ lagging since {} · retrying", b.time(*since)))
    } else if b.user_paused {
        (Style::Yellow, "‖ PAUSED (you) · Z resumes".to_owned())
    } else if let Some(id) = b.in_debate() {
        let head = b.concerns.get(id).map(|c| c.headline.as_str()).unwrap_or_default();
        let state = match (&b.pending, b.debate.open()) {
            (Some(p), _) => p.line.clone(),
            (None, Some(o)) => format!("Debate {}/2", o.n),
            (None, None) => "Challenge queued".to_owned(),
        };
        (Style::Cyan, format!("⚑ {id} {state} · {head}"))
    } else {
        let open = b.concerns.all().iter().filter(|c| c.state == State::Open).count();
        let since =
            b.last_spoke.or(b.started).map(|t| format!(" · silent since {}", hhmm(t, b.tz))).unwrap_or_default();
        let word = if b.ended { "■ session ended" } else { "● watching" };
        (Style::Green, format!("{word}{since} · {open} open"))
    };
    Line::one(style, cut(&text, width))
}

fn stream(b: &Board, ui: &Ui, width: usize, rows: usize) -> Vec<Row> {
    let mut lines = Vec::new();
    for s in
        b.stream.iter().filter(|s| s.focus.is_none() || ui.filter.as_ref().is_none_or(|f| s.focus.as_ref() == Some(f)))
    {
        let lead = format!("{} {}{} ", b.time(s.t), if s.indent { "  " } else { "" }, s.sym);
        let style = match s.sym {
            '▲' => Style::RedBold,
            '│' => Style::Cyan,
            '✓' => Style::Green,
            '✗' | '■' => Style::Dim,
            _ => Style::Plain,
        };
        let indent = " ".repeat(cols(&lead));
        for (i, l) in wrap(&s.text, width - cols(&lead), 0).into_iter().enumerate() {
            let first =
                if i == 0 { Line::one(Style::Dim, lead.clone()) } else { Line::one(Style::Plain, indent.clone()) };
            lines.push(first.push(style, l));
        }
    }
    let room = rows.saturating_sub(1);
    let skip = lines.len().saturating_sub(room);
    std::iter::once(row(section("STREAM", b.stream.len(), "l close · f", width), 0))
        .chain(lines.into_iter().skip(skip).map(|l| row(l, 0)))
        .collect()
}

const KEYS: [(&str, &str); 20] = [
    ("j k ↑ ↓", "select"),
    ("⏎", "expand / collapse the Debate log"),
    ("y", "NEEDS YOU / IN DEBATE: side with the Watcher (it tells the Worker)"),
    ("n", "NEEDS YOU / IN DEBATE: side with the Worker"),
    ("d", "dismiss: the Concern (final), or a NEEDS YOU / IN DEBATE item"),
    ("u", "undo the last ruling"),
    ("s", "keep the last ruling or dismissal (or the selected CLOSED row) for this repo"),
    ("r", "Standing rulings for this repo"),
    ("x", "in r: remove the selected Standing ruling"),
    ("P", "promote to a Challenge, with a remark"),
    ("+", "mark helpful"),
    ("f", "Focus filter"),
    ("c", "show / hide CLOSED"),
    ("l", "Stream (history)"),
    ("m", "mute the Concern's Focus (session)"),
    ("a", "no auto-Challenge (session)"),
    ("Z", "pause / resume the Watcher (you)"),
    ("$", "add $5 and resume"),
    ("?", "this help"),
    ("q", "quit the pane"),
];

fn help(width: usize) -> Vec<Row> {
    std::iter::once(row(section("KEYS", KEYS.len(), "? close", width), 0))
        .chain(KEYS.iter().flat_map(|(k, what)| {
            wrap(&format!("  {k:<8} {what}"), width, 11).into_iter().map(|l| row(Line::one(Style::Plain, l), 0))
        }))
        .collect()
}

/// The `r` overlay (V78): each Standing ruling's id, date, Focus, verdict, headline and reason.
fn rulings(b: &Board, selected: Option<&str>, width: usize) -> Vec<Row> {
    let mut out = vec![row(section("RULINGS", b.held.len(), "x remove · r close", width), 0)];
    if b.held.is_empty() {
        let none = "  none held; s keeps a ruling or dismissal for this repo";
        out.extend(wrap(none, width, 2).into_iter().map(|l| row(Line::one(Style::Dim, l), 0)));
    }
    for r in &b.held {
        let mark = if selected == Some(r.id.as_str()) { "›" } else { " " };
        let head = format!("{mark}{} {} {} · {}", r.id, r.date, r.focus, r.words());
        out.push(row(Line::one(Style::Bold, cut(&head, width)), 0));
        out.extend(
            wrap(&format!("    {}", r.headline), width, 4).into_iter().map(|l| row(Line::one(Style::Plain, l), 0)),
        );
        if let Some(reason) = &r.reason {
            let reason = format!("    — {reason}");
            out.extend(wrap(&reason, width, 6).into_iter().map(|l| row(Line::one(Style::Dim, l), 0)));
        }
    }
    out
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, r) if r != 11 => "st",
        (2, r) if r != 12 => "nd",
        (3, r) if r != 13 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Terminal columns of one char as drawn: wide CJK glyphs take two, and a control char is
/// drawn as one space (`Line::push`).
fn char_cols(c: char) -> usize {
    if c.is_control() { 1 } else { UnicodeWidthChar::width(c).unwrap_or(0) }
}

/// Terminal columns as drawn.
pub fn cols(s: &str) -> usize {
    s.chars().map(char_cols).sum()
}

/// The longest prefix of at most `width` columns, and the rest. Never empty while `s` is not,
/// so a glyph wider than `width` still makes progress.
fn split_cols(s: &str, width: usize) -> (&str, &str) {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let w = char_cols(c);
        if used + w > width {
            let i = if i == 0 { c.len_utf8() } else { i };
            return s.split_at(i);
        }
        used += w;
    }
    (s, "")
}

/// At most `width` columns; a cut ends in `…`.
fn cut(s: &str, width: usize) -> String {
    if cols(s) <= width {
        return s.to_owned();
    }
    format!("{}…", split_cols(s, width.saturating_sub(1)).0)
}

/// The last `width` columns of `s`.
fn tail(s: &str, width: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices().rev() {
        used += char_cols(c);
        if used > width {
            return &s[i + c.len_utf8()..];
        }
    }
    s
}

fn pad(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(cols(s))))
}

/// Cuts a styled line to `width` columns, `…` at the cut.
fn clip_line(line: Line, width: usize) -> Line {
    if line.width() <= width {
        return line;
    }
    let mut left = width.saturating_sub(1);
    let mut out = Line::default();
    for s in line.0 {
        let (text, _) = split_cols(&s.text, left);
        if text.is_empty() || cols(text) > left {
            break;
        }
        left -= cols(text);
        out.0.push(Span { style: s.style, text: text.to_owned() });
    }
    out.push(Style::Plain, "…")
}

/// Word wrap to `width` columns, a hard limit; lines after the first are indented `indent`
/// columns. Leading spaces indent every line. A word longer than a line is split.
pub fn wrap(text: &str, width: usize, indent: usize) -> Vec<String> {
    let width = width.max(2);
    let lead = (text.len() - text.trim_start_matches(' ').len()).min(width / 2);
    if lead > 0 {
        let mut lines = wrap(text[lead..].trim_start_matches(' '), width - lead, indent.saturating_sub(lead));
        for l in &mut lines {
            l.insert_str(0, &text[..lead]);
        }
        return lines;
    }
    let indent = indent.min(width / 2);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let room = |lines: &Vec<String>| if lines.is_empty() { width } else { width - indent };
    for word in text.split(' ') {
        let mut word = word;
        loop {
            let limit = room(&lines);
            let need = if cur.is_empty() { cols(word) } else { cols(&cur) + 1 + cols(word) };
            if need <= limit {
                if !cur.is_empty() {
                    cur.push(' ');
                }
                cur.push_str(word);
                break;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                continue;
            }
            // A word longer than the whole line: split it.
            let (head, rest) = split_cols(word, limit);
            lines.push(head.to_owned());
            word = rest;
            if word.is_empty() {
                break;
            }
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines.into_iter().enumerate().map(|(i, l)| if i == 0 { l } else { format!("{}{l}", " ".repeat(indent)) }).collect()
}
