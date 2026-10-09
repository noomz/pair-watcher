//! Wake input (V2–V7, V9, V11). Stateless: built per wake from committed history, entries since
//! the cursor, the snapshot diff and the arbiter's lists; nothing a Watcher wrote. The shared
//! `Input` is identical for every Watcher; `render` adds only its Focus brief(s) (V9).

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::action::{self, Outcome};
use crate::clip;
use crate::concern::{Concern, State};
use crate::focus::Focus;
use crate::history::{History, TASK_MAX};
use crate::snapshot::FileDiff;
use crate::tailer::{Entry, Item};
use crate::trigger::Trigger;

const K: usize = 1024;
// Section caps (V7). Only turn content borrows unused space from the shared sections.
pub const PROMPT_CAP: usize = 6 * K;
pub const TASK_CAP: usize = TASK_MAX;
pub const CONCERNS_CAP: usize = 8 * K;
pub const DEBATE_CAP: usize = 4 * K;
pub const DIGEST_CAP: usize = 4 * K;
pub const RULINGS_CAP: usize = 2 * K;
pub const TURN_CAP: usize = 16 * K;
pub const TURN_MAX: usize = 24 * K;
pub const DIFF_CAP: usize = 8 * K;
pub const STAT_CAP: usize = 2 * K;
pub const BLOCKED_CAP: usize = 2 * K;
const HINTS_CAP: usize = K;
// Turn content clipping (V3, V7).
const CALL_INPUT: usize = 2 * K;
const BODY_HEAD: usize = 600;
const BODY_TAIL: usize = 400;
const INPUT_LINE: usize = 200;
const TEXT_MIN: usize = 120;
/// Room kept for a `[N more hunks]` / `[N more files]` line.
const MORE_LINE: usize = 24;

/// Repo instruction files, listed by path for the Watcher to read on demand (V6).
const INSTRUCTION_FILES: [&str; 5] =
    ["AGENTS.md", "AGENTS.override.md", "CLAUDE.md", "CLAUDE.local.md", ".claude/CLAUDE.md"];
/// Hook payload fields that say nothing about the question.
const PAYLOAD_NOISE: [&str; 5] = ["session_id", "transcript_path", "cwd", "hook_event_name", "permission_mode"];

pub fn instruction_files(root: &Path) -> Vec<String> {
    INSTRUCTION_FILES.iter().filter(|f| root.join(f).is_file()).map(|f| f.to_string()).collect()
}

/// What the arbiter and stores contribute, already rendered or listed.
pub struct Context<'a> {
    /// Oldest first, all Focuses (V44).
    pub concerns: &'a [Concern],
    pub debate: Option<&'a str>,
    /// Escalations awaiting the human's ruling (`Rulings::awaiting`): turn-end wakes show them
    /// with the open Debate, so a Watcher can infer a ruling from the human's prompt (V71).
    pub awaiting: Option<&'a str>,
    /// Standing rulings as rendered by their store; never trimmed (V7, V78).
    pub rulings: &'a str,
    pub instructions: &'a [String],
}

/// Snapshot diffs: since the last wake, and the session `--stat` since its start (V5).
pub struct Diff<'a> {
    pub wake: &'a [FileDiff],
    pub session: &'a [FileDiff],
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Section {
    pub title: String,
    pub body: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Input {
    pub sections: Vec<Section>,
    /// Full (unclipped) patch bytes since the last wake, for the wake record (V5).
    pub diff_bytes: usize,
}

impl Input {
    /// One Watcher's input: prompt + its brief(s) (≤6 K), then the shared sections.
    pub fn render(&self, prompt: &str, briefs: &[&Focus]) -> String {
        let named: Vec<(&str, &str)> = briefs.iter().map(|f| (f.name.as_str(), f.brief.as_str())).collect();
        self.render_named(prompt, &named)
    }

    /// `render` from (Focus name, brief) pairs, as a wake record keeps them.
    pub fn render_named(&self, prompt: &str, briefs: &[(&str, &str)]) -> String {
        let mut head = prompt.trim().to_owned();
        for (name, brief) in briefs {
            head.push_str(&format!("\n\n## Focus: {name}\n{}", brief.trim()));
        }
        let mut out = clip::head(&head, PROMPT_CAP);
        for s in &self.sections {
            out.push_str(&format!("\n\n## {}\n{}", s.title, s.body));
        }
        out.push('\n');
        out
    }

    pub fn section(&self, title: &str) -> Option<&str> {
        self.sections.iter().find(|s| s.title == title).map(|s| s.body.as_str())
    }
}

/// Turn-end or checkpoint wake. Returns the input and the history to commit with the cursor.
pub fn normal(
    trigger: Trigger,
    history: &History,
    entries: &[Entry],
    diff: Option<&Diff>,
    ctx: &Context,
) -> (Input, History) {
    let next = history.ingest(entries);
    let inputs: Vec<&str> = entries
        .iter()
        .flat_map(|e| &e.items)
        .filter_map(|i| match i {
            Item::ToolCall { input, .. } => Some(input.as_str()),
            _ => None,
        })
        .collect();
    let touched = |path: &str| inputs.iter().any(|i| i.contains(path));
    let (diff_body, diff_bytes) = diff_section(diff, &touched);

    let shared = [
        ("Task", task_section(&next, ctx.instructions), TASK_CAP),
        ("Standing rulings", ctx.rulings.trim().to_owned(), RULINGS_CAP),
        ("Concerns", concern_section(ctx.concerns, false), CONCERNS_CAP),
        ("Open Debate", clip::middle(debate_section(trigger, ctx).trim(), DEBATE_CAP), DEBATE_CAP),
        ("History", history_section(history, &next), DIGEST_CAP),
        ("Diff", diff_body, DIFF_CAP + STAT_CAP),
    ];
    let lend: usize = shared.iter().map(|(_, body, cap)| cap.saturating_sub(body.len())).sum();
    let turn = turn_section(entries, (TURN_CAP + lend).min(TURN_MAX));

    let mut sections = vec![Section { title: "Wake".into(), body: wake_line(trigger).to_owned() }];
    let (diff_part, rest): (Vec<_>, Vec<_>) = shared.into_iter().partition(|(t, _, _)| *t == "Diff");
    sections.extend(
        rest.into_iter()
            .filter(|(_, b, _)| !b.is_empty())
            .map(|(title, body, _)| Section { title: title.into(), body }),
    );
    sections.push(Section { title: "Turn".into(), body: turn });
    sections.extend(diff_part.into_iter().map(|(title, body, _)| Section { title: title.into(), body }));
    (Input { sections, diff_bytes }, next)
}

/// The open Debate, then (turn end only) the Escalations awaiting a ruling (V7 Open Debate cap).
fn debate_section(trigger: Trigger, ctx: &Context) -> String {
    let awaiting = ctx.awaiting.filter(|_| trigger == Trigger::TurnEnd);
    [ctx.debate, awaiting].into_iter().flatten().map(str::trim).collect::<Vec<_>>().join("\n\n")
}

/// Slim blocked wake: question + task + rulings + open Concerns + digest; no turn content, no
/// diff, no tools (V11).
pub fn blocked(history: &History, payload: &Value, ctx: &Context) -> Input {
    let sections = [
        ("Wake", wake_line(Trigger::Blocked).to_owned()),
        ("Blocked question", question(payload)),
        ("Task", task_section(history, ctx.instructions)),
        ("Standing rulings", ctx.rulings.trim().to_owned()),
        ("Concerns", concern_section(ctx.concerns, true)),
        ("History", history_section(history, history)),
    ];
    let sections = sections
        .into_iter()
        .filter(|(_, b)| !b.is_empty())
        .map(|(title, body)| Section { title: title.into(), body })
        .collect();
    Input { sections, diff_bytes: 0 }
}

fn wake_line(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::TurnEnd => "turn end",
        Trigger::Checkpoint => "checkpoint (mid-turn)",
        Trigger::Blocked => "blocked: the Worker waits on the human",
    }
}

fn question(payload: &Value) -> String {
    let mut payload = payload.clone();
    if let Value::Object(map) = &mut payload {
        for k in PAYLOAD_NOISE {
            map.remove(k);
        }
    }
    let text = match payload {
        Value::String(s) => s,
        v => v.to_string(),
    };
    clip::head(&text, BLOCKED_CAP)
}

/// Repo instruction paths, then every human prompt and blocked answer; past the cap, first +
/// latest 3 stay, the middle (already first lines) fits newest-first, the rest counted (V6).
fn task_section(history: &History, instructions: &[String]) -> String {
    let mut n = 0;
    let items: Vec<String> = history
        .asks()
        .iter()
        .map(|a| {
            if a.answer {
                format!("[answer] {}", a.text)
            } else {
                n += 1;
                format!("[prompt {n}{}] {}", if a.folded { ", folded" } else { "" }, a.text)
            }
        })
        .collect();
    let mut out = String::new();
    if !instructions.is_empty() {
        out = format!("Repo instructions (read on demand): {}\n", instructions.join(", "));
    }
    let total: usize = items.iter().map(|i| i.len() + 1).sum();
    if out.len() + total <= TASK_CAP || items.len() <= 4 {
        return clip::head(&(out + &items.join("\n")), TASK_CAP);
    }
    let (first, rest) = items.split_first().expect("more than 4 items");
    let (middle, latest) = rest.split_at(rest.len() - 3);
    let fixed: usize = out.len() + first.len() + latest.iter().map(|i| i.len() + 1).sum::<usize>() + 40;
    let mut room = TASK_CAP.saturating_sub(fixed);
    let mut kept: Vec<&String> = Vec::new();
    for item in middle.iter().rev() {
        if item.len() + 1 > room {
            break;
        }
        room -= item.len() + 1;
        kept.push(item);
    }
    let dropped = middle.len() - kept.len();
    let mut lines = vec![first.clone()];
    if dropped > 0 {
        lines.push(format!("[{dropped} earlier prompts folded]"));
    }
    lines.extend(kept.into_iter().rev().cloned());
    lines.extend(latest.iter().cloned());
    clip::head(&(out + &lines.join("\n")), TASK_CAP)
}

/// Repeat hints (≤1 K) then the digest, ≤4 K together (V4). Digest of turns before this wake's
/// entries; hints include them.
fn history_section(before: &History, next: &History) -> String {
    let mut hints = String::new();
    for h in next.hints() {
        if hints.len() + h.len() + 1 > HINTS_CAP {
            break;
        }
        hints.push_str(&h);
        hints.push('\n');
    }
    let digest = before.digest(DIGEST_CAP - hints.len());
    (hints + &digest).trim_end().to_owned()
}

/// Open Concerns in full, dismissed/resolved as id + headline (V7). Over the cap: oldest resolved
/// dropped, then oldest open collapse to headline, then oldest dismissed dropped.
fn concern_section(concerns: &[Concern], open_only: bool) -> String {
    let list: Vec<&Concern> = concerns.iter().filter(|c| !open_only || c.state == State::Open).collect();
    if list.is_empty() {
        return "(none)".into();
    }
    let mut lines: Vec<Option<String>> = list.iter().map(|c| Some(concern_line(c, true))).collect();
    let size = |lines: &[Option<String>]| lines.iter().flatten().map(|l| l.len() + 1).sum::<usize>();
    let steps: [(State, bool); 4] =
        [(State::Resolved, false), (State::Open, true), (State::Dismissed, false), (State::Ruled, false)];
    for (state, collapse) in steps {
        for (i, c) in list.iter().enumerate() {
            if size(&lines) <= CONCERNS_CAP {
                break;
            }
            if c.state == state {
                lines[i] = collapse.then(|| concern_line(c, false));
            }
        }
    }
    let order = |open: bool| {
        list.iter().zip(&lines).filter(move |(c, _)| (c.state == State::Open) == open).filter_map(|(_, l)| l.clone())
    };
    let all: Vec<String> = order(true).chain(order(false)).collect();
    clip::head(&all.join("\n"), CONCERNS_CAP)
}

fn concern_line(c: &Concern, full: bool) -> String {
    if c.state != State::Open {
        return format!("{} ({}) {}: {}", c.id, c.focus, c.state.name(), c.headline);
    }
    let mut line = format!("{} [{}] ({}) {}", c.id, c.severity.name(), c.focus, c.headline);
    if full {
        for d in &c.detail {
            line.push_str(&format!("\n  {d}"));
        }
        if !c.refs.is_empty() {
            line.push_str(&format!("\n  refs: {}", c.refs.join(", ")));
        }
    }
    line
}

/// Changed files since the last wake: hunks of files the turn's calls name first, the rest as
/// stat lines (≤8 K), then the session stat (≤2 K) (V5). Returns body and full patch bytes.
fn diff_section(diff: Option<&Diff>, touched: &dyn Fn(&str) -> bool) -> (String, usize) {
    let Some(diff) = diff else { return ("(not a git repo: no diff)".into(), 0) };
    let bytes = diff.wake.iter().map(|f| f.patch.len()).sum();
    let mut out = String::new();
    if diff.wake.is_empty() {
        out.push_str("(no change since last wake)\n");
    } else {
        let mut files: Vec<&FileDiff> = diff.wake.iter().collect();
        files.sort_by_key(|f| !touched(&f.path));
        let reserve = files.iter().map(|f| stat_line(f).len() + 1).sum::<usize>().min(STAT_CAP);
        let budget = DIFF_CAP - reserve;
        let mut stat_only = Vec::new();
        for f in files {
            let hunks = hunks(&f.patch);
            let header = format!("=== {}\n", stat_line(f));
            let mut shown = 0;
            for h in &hunks {
                // Room for a `[N more hunks]` line after it.
                let need = h.len() + if shown == 0 { header.len() } else { 0 } + MORE_LINE;
                if out.len() + need > budget {
                    break;
                }
                if shown == 0 {
                    out.push_str(&header);
                }
                out.push_str(h);
                shown += 1;
            }
            match (shown, hunks.len() - shown) {
                (0, _) => stat_only.push(f),
                (_, 0) => {}
                (_, more) => out.push_str(&format!("[{more} more hunks]\n")),
            }
        }
        if !stat_only.is_empty() {
            out.push_str("Other changes:\n");
            out.push_str(&stat_lines(&stat_only, DIFF_CAP.saturating_sub(out.len())));
        }
    }
    let (added, removed) =
        diff.session.iter().fold((0, 0), |(a, r), f| (a + f.added.unwrap_or(0), r + f.removed.unwrap_or(0)));
    let head = format!("Session since start: {} files, +{added} -{removed}\n", diff.session.len());
    let session: Vec<&FileDiff> = diff.session.iter().collect();
    out.push_str(&head);
    out.push_str(&stat_lines(&session, STAT_CAP.saturating_sub(head.len())));
    (out.trim_end().to_owned(), bytes)
}

fn stat_line(f: &FileDiff) -> String {
    match (f.added, f.removed) {
        (Some(a), Some(r)) => format!("{} | +{a} -{r}", f.path),
        _ => format!("{} | binary", f.path),
    }
}

/// Stat lines within `room`; the rest counted.
fn stat_lines(files: &[&FileDiff], room: usize) -> String {
    let mut out = String::new();
    for (i, f) in files.iter().enumerate() {
        let line = stat_line(f) + "\n";
        let reserve = if i + 1 < files.len() { MORE_LINE } else { 0 };
        if out.len() + line.len() + reserve > room {
            let more = format!("[{} more files]\n", files.len() - i);
            if out.len() + more.len() <= room {
                out.push_str(&more);
            }
            break;
        }
        out.push_str(&line);
    }
    out
}

/// A file patch's hunks, each from its `@@` line; the git header is dropped.
fn hunks(patch: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = patch.match_indices("\n@@").map(|(i, _)| i + 1).collect();
    if patch.starts_with("@@") {
        starts.insert(0, 0);
    }
    starts.push(patch.len());
    starts.windows(2).map(|w| &patch[w[0]..w[1]]).collect()
}

enum Seg {
    Text(String),
    Call(Call),
    TurnEnd,
}

struct Call {
    tool: String,
    id: String,
    input: String,
    /// Outcome, result size, error-looking line; `None` while the call runs.
    result: Option<(Outcome, usize, Option<String>)>,
    output: Option<String>,
    body: Option<String>,
}

impl Seg {
    fn render(&self) -> String {
        match self {
            Seg::Text(t) => format!("{t}\n"),
            Seg::TurnEnd => "— turn end —\n".into(),
            Seg::Call(c) => {
                let mut s = format!("▸ {} {}\n  → ", c.tool, c.input);
                match &c.result {
                    None => s.push_str("running"),
                    Some((o, size, err)) => {
                        s.push_str(&format!("{o} · {size} B"));
                        if let Some(e) = err {
                            s.push_str(&format!(" · \"{e}\""));
                        }
                    }
                }
                s.push('\n');
                for line in c.body.iter().flat_map(|b| b.lines()) {
                    s.push_str("  | ");
                    s.push_str(line);
                    s.push('\n');
                }
                s
            }
        }
    }
}

/// All assistant text since the cursor, each call (input ≤2 K) with its outcome line; bodies
/// (head 600 + tail 400) for failed results and the last result per tool (V3). Over `cap`,
/// trimmed per V7 in order: bodies oldest-first, assistant text middle-clipped, inputs to their
/// first line oldest-first, oldest calls folded into a count.
fn turn_section(entries: &[Entry], cap: usize) -> String {
    let mut segs: Vec<Seg> = Vec::new();
    for item in entries.iter().flat_map(|e| &e.items) {
        match item {
            Item::Text(t) if !t.trim().is_empty() => segs.push(Seg::Text(t.trim().to_owned())),
            Item::ToolCall { id, name, input } => segs.push(Seg::Call(Call {
                tool: name.clone(),
                id: id.clone(),
                input: clip::head(input, CALL_INPUT),
                result: None,
                output: None,
                body: None,
            })),
            Item::ToolResult { id, output, error } => {
                let found = segs.iter().rposition(|s| matches!(s, Seg::Call(c) if c.id == *id));
                let i = found.unwrap_or_else(|| {
                    segs.push(Seg::Call(Call {
                        tool: "(earlier call)".into(),
                        id: id.clone(),
                        input: String::new(),
                        result: None,
                        output: None,
                        body: None,
                    }));
                    segs.len() - 1
                });
                if let Seg::Call(c) = &mut segs[i] {
                    let outcome = Outcome::of(&c.tool, output, *error);
                    let line = outcome.failed().then(|| action::error_line(output)).flatten();
                    (c.result, c.output) = (Some((outcome, output.len(), line)), Some(output.clone()));
                }
            }
            Item::TurnEnd => segs.push(Seg::TurnEnd),
            _ => {}
        }
    }
    let mut last_of_tool: Vec<(String, usize)> = Vec::new();
    for (i, s) in segs.iter().enumerate() {
        if let Seg::Call(c) = s
            && c.output.is_some()
        {
            match last_of_tool.iter_mut().find(|(t, _)| *t == c.tool) {
                Some(e) => e.1 = i,
                None => last_of_tool.push((c.tool.clone(), i)),
            }
        }
    }
    for (i, s) in segs.iter_mut().enumerate() {
        if let Seg::Call(c) = s
            && let Some(out) = c.output.take()
        {
            let failed = c.result.as_ref().is_some_and(|r| r.0.failed());
            if failed || last_of_tool.iter().any(|(_, j)| *j == i) {
                c.body = Some(clip::head_tail(out.trim_end(), BODY_HEAD, BODY_TAIL));
            }
        }
    }
    if segs.is_empty() {
        return "(no Worker activity since last wake)".into();
    }
    trim(segs, cap)
}

fn trim(mut segs: Vec<Seg>, cap: usize) -> String {
    let size = |segs: &[Seg]| segs.iter().map(|s| s.render().len()).sum::<usize>();
    let mut total = size(&segs);
    // 1. Bodies, oldest first.
    for s in segs.iter_mut() {
        if total <= cap {
            break;
        }
        let before = s.render().len();
        if let Seg::Call(c) = s {
            c.body = None;
        }
        total -= before - s.render().len();
    }
    // 2. Assistant text, middle-clipped in proportion to its share of the text.
    if total > cap {
        let excess = total - cap;
        let text: usize = segs.iter().map(|s| if let Seg::Text(t) = s { t.len() } else { 0 }).sum();
        for s in segs.iter_mut() {
            if let Seg::Text(t) = s
                && t.len() > TEXT_MIN
            {
                let cut = (excess * t.len()).div_ceil(text.max(1));
                *t = clip::middle(t, t.len().saturating_sub(cut).max(TEXT_MIN));
            }
        }
        total = size(&segs);
    }
    // 3. Tool inputs to their first line, oldest first.
    for s in segs.iter_mut() {
        if total <= cap {
            break;
        }
        if let Seg::Call(c) = s {
            let before = c.input.len();
            c.input = clip::first_line(&c.input, INPUT_LINE);
            total -= before - c.input.len();
        }
    }
    // 4. Oldest segments fold into a count of their calls.
    let (mut ok, mut failed) = (0, 0);
    let fold = |ok: usize, failed: usize| format!("[{} earlier calls: ok {ok}, failed {failed}]\n", ok + failed);
    let mut start = 0;
    while start + 1 < segs.len() && total + if start > 0 { fold(ok, failed).len() } else { 0 } > cap {
        total -= segs[start].render().len();
        if let Seg::Call(c) = &segs[start] {
            match c.result {
                Some((o, _, _)) if o.failed() => failed += 1,
                _ => ok += 1,
            }
        }
        start += 1;
    }
    let mut out = if start > 0 { fold(ok, failed) } else { String::new() };
    for s in &segs[start..] {
        out.push_str(&s.render());
    }
    clip::middle(out.trim_end(), cap)
}
