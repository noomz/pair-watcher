//! Code-built session history (V2, V4, V6): turn digest with action signatures, repeat hints,
//! human prompts + answers to blocked questions. Part of the committed cursor state (V10):
//! `ingest` is pure and returns the next state for the caller to commit.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::action::{self, Outcome};
use crate::clip;
use crate::tailer::{Entry, Item};

/// Completed turns kept as lines; older ones fold into counts. Bounds state; render fits ≤4 KB.
const KEEP_TURNS: usize = 64;
/// Repeat hints look back this many turns, and fire at this many sightings.
const HINT_TURNS: u32 = 10;
const HINT_MIN: usize = 3;
/// One task-section item (V6).
pub const ASK_MAX: usize = 1536;
/// Task-section total before the middle folds (V6).
pub const TASK_MAX: usize = 6 * 1024;
/// Prompts the adapters deliver (Challenge, Ruling) start with this; they are not the human's.
pub const OWN_PREFIX: &str = "[pair-watcher ·";

/// A prompt we delivered, not the human's: it opens with our header, or a one-line harness label
/// (`Stop hook feedback:`) then our header. A human quoting our text further down stays human.
pub fn is_own(prompt: &str) -> bool {
    let mut lines = prompt.lines().map(str::trim).filter(|l| !l.is_empty());
    match lines.next() {
        Some(first) if first.starts_with(OWN_PREFIX) => true,
        Some(label) if label.ends_with(':') && label.len() <= LABEL_MAX => {
            lines.next().is_some_and(|l| l.starts_with(OWN_PREFIX))
        }
        _ => false,
    }
}

/// Longest harness label line [`is_own`] looks past.
const LABEL_MAX: usize = 40;
/// Claude's blocked-question tool: its result is the human's answer.
const ASK_TOOL: &str = "AskUserQuestion";

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct History {
    folded: Folded,
    turns: Vec<Turn>,
    /// The turn in progress (a checkpoint wake cut it).
    open: Option<Turn>,
    asks: Vec<Ask>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct Folded {
    turns: u32,
    actions: u32,
    by_tool: BTreeMap<String, u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Turn {
    n: u32,
    /// First line of the latest assistant text, ≤120 B.
    text: String,
    actions: Vec<Action>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Action {
    id: String,
    tool: String,
    sig: String,
    /// `None` until its result arrives.
    outcome: Option<Outcome>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Ask {
    pub answer: bool,
    pub text: String,
    /// Middle item reduced to its first line (V6).
    pub folded: bool,
}

impl History {
    pub fn ingest(&self, entries: &[Entry]) -> History {
        let mut next = self.clone();
        for item in entries.iter().flat_map(|e| &e.items) {
            match item {
                Item::Prompt(text) if !is_own(text) => next.ask(false, text),
                Item::Text(text) if !text.trim().is_empty() => next.open_turn().text = clip::first_line(text, 120),
                Item::ToolCall { id, name, input } => next.open_turn().actions.push(Action {
                    id: id.clone(),
                    tool: name.clone(),
                    sig: action::signature(name, input),
                    outcome: None,
                }),
                Item::ToolResult { id, output, error } => {
                    let call = next.open.as_mut().and_then(|t| t.actions.iter_mut().rev().find(|a| a.id == *id));
                    if let Some(call) = call {
                        call.outcome = Some(Outcome::of(&call.tool, output, *error));
                        if call.tool == ASK_TOOL {
                            next.ask(true, output);
                        }
                    }
                }
                Item::TurnEnd => {
                    let turn = next.open_turn().clone();
                    next.open = None;
                    next.turns.push(turn);
                    if next.turns.len() > KEEP_TURNS {
                        let old = next.turns.remove(0);
                        next.folded.add(&old);
                    }
                }
                _ => {}
            }
        }
        next
    }

    fn open_turn(&mut self) -> &mut Turn {
        let n = self.folded.turns + self.turns.len() as u32 + 1;
        self.open.get_or_insert_with(|| Turn { n, text: String::new(), actions: Vec::new() })
    }

    /// Once the full items pass `TASK_MAX`, the middle (all but first + latest 3) folds for good:
    /// the total only grows.
    fn ask(&mut self, answer: bool, text: &str) {
        self.asks.push(Ask { answer, text: clip::middle(text.trim(), ASK_MAX), folded: false });
        if self.asks.iter().map(|a| a.text.len()).sum::<usize>() > TASK_MAX {
            let middle = 1..self.asks.len().saturating_sub(3).max(1);
            for ask in &mut self.asks[middle] {
                if !ask.folded {
                    ask.text = clip::first_line(&ask.text, 160);
                    ask.folded = true;
                }
            }
        }
    }

    /// `(tool, signature, outcome)` of every action still held per turn (older turns are folded
    /// into counts), open turn included; outcome `None` while running.
    pub fn actions(&self) -> impl Iterator<Item = (&str, &str, Option<Outcome>)> {
        self.turns
            .iter()
            .chain(&self.open)
            .flat_map(|t| &t.actions)
            .map(|a| (a.tool.as_str(), a.sig.as_str(), a.outcome))
    }

    pub fn asks(&self) -> &[Ask] {
        &self.asks
    }

    /// One line per earlier turn, newest kept, oldest folded into counts, ≤`max` bytes (V4).
    pub fn digest(&self, max: usize) -> String {
        let mut lines: Vec<String> = Vec::new();
        let mut used = 0;
        let mut folded = self.folded.clone();
        let turns: Vec<&Turn> = self.turns.iter().chain(&self.open).collect();
        let mut cut = 0;
        // Room for the fold line: ≤160 B.
        for (i, turn) in turns.iter().enumerate().rev() {
            let line = turn.line(self.open.as_ref().is_some_and(|o| o.n == turn.n));
            if used + line.len() + 1 + 160 > max {
                cut = i + 1;
                break;
            }
            used += line.len() + 1;
            lines.push(line);
        }
        for turn in &turns[..cut] {
            folded.add(turn);
        }
        if folded.turns > 0 {
            lines.push(folded.line());
        }
        lines.reverse();
        clip::head(&lines.join("\n"), max)
    }

    /// `hint: signature "<sig>" seen N× in last M turns` for signatures seen ≥3× within the last
    /// 10 turns, open turn included (V4). Input only: the Watcher decides.
    pub fn hints(&self) -> Vec<String> {
        let turns: Vec<&Turn> = self.turns.iter().chain(&self.open).collect();
        let Some(last) = turns.last().map(|t| t.n) else { return Vec::new() };
        let mut seen: BTreeMap<&str, (usize, u32)> = BTreeMap::new();
        for turn in turns.iter().filter(|t| t.n + HINT_TURNS > last) {
            for a in &turn.actions {
                let e = seen.entry(&a.sig).or_insert((0, turn.n));
                e.0 += 1;
            }
        }
        let mut hints: Vec<(usize, u32, &str)> = seen
            .into_iter()
            .filter(|(_, (count, _))| *count >= HINT_MIN)
            .map(|(sig, (c, first))| (c, last - first + 1, sig))
            .collect();
        hints.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.cmp(b.2)));
        hints
            .into_iter()
            .map(|(count, span, sig)| format!("hint: signature \"{sig}\" seen {count}× in last {span} turns"))
            .collect()
    }
}

impl Turn {
    /// `T<n> "<text>" · <sig> → <outcome>[ ×k] · …`, ≤400 B; identical actions merge.
    fn line(&self, open: bool) -> String {
        let mut acts: Vec<(String, usize)> = Vec::new();
        for a in &self.actions {
            let s = format!("{} → {}", a.sig, a.outcome.map_or_else(|| "running".to_owned(), |o| o.to_string()));
            match acts.iter_mut().find(|(x, _)| *x == s) {
                Some((_, k)) => *k += 1,
                None => acts.push((s, 1)),
            }
        }
        let mut line = format!("T{}{} \"{}\"", self.n, if open { " (so far)" } else { "" }, self.text);
        for (s, k) in acts {
            line.push_str(" · ");
            line.push_str(&s);
            if k > 1 {
                line.push_str(&format!(" ×{k}"));
            }
        }
        clip::head(&line, 400)
    }
}

impl Folded {
    fn add(&mut self, turn: &Turn) {
        self.turns += 1;
        self.actions += turn.actions.len() as u32;
        for a in &turn.actions {
            *self.by_tool.entry(a.tool.clone()).or_default() += 1;
        }
    }

    fn line(&self) -> String {
        let mut tools: Vec<(&String, &u32)> = self.by_tool.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let tools: Vec<String> = tools.iter().take(5).map(|(t, n)| format!("{t} {n}")).collect();
        let first = if self.turns == 1 { "T1".to_owned() } else { format!("T1–T{}", self.turns) };
        clip::head(
            &format!("{first}: {} turns folded · {} actions ({})", self.turns, self.actions, tools.join(", ")),
            160,
        )
    }
}
