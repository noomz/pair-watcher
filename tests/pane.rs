//! §T12 pane: Board fold, pure layout at 48/64/96 columns (V63–V66), keys (I17), `shown`
//! (V69), open-on-first-event (V67), promote requests (V43), session resolution (I1).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pair_watcher::challenge::{Challenges, Op as ChallengeOp, Origin};
use pair_watcher::concern::{Change, Concern, ConcernList, Severity, State};
use pair_watcher::debate::{Op as DebateOp, Outcome as DebateOutcome, Second, Verdict};
use pair_watcher::delivery::Pending;
use pair_watcher::escalation::{Cause, Op as EscalationOp};
use pair_watcher::event::{Event, EventLog, Kind};
use pair_watcher::home::Home;
use pair_watcher::human::Act;
use pair_watcher::jsonl::Record;
use pair_watcher::layout::{Frame, Overlay, Style, Ui, layout, wrap};
use pair_watcher::mailbox::{self, Mailbox};
use pair_watcher::outbox::{Item, ItemKind, Outbox};
use pair_watcher::output::Decision;
use pair_watcher::pane::{self, Board, Effect, SHOWN_AFTER, Shown};
use pair_watcher::status::{Class, Coverage, Op as StatusOp, Outcome, Wake};
use pair_watcher::term::{self, Key};
use serde_json::json;
use tempfile::TempDir;

const WIDTHS: [usize; 3] = [48, 64, 96];

fn at(h: u64, m: u64) -> u64 {
    (h * 3600 + m * 60) * 1000
}

fn concern(id: &str, focus: &str, severity: Severity, headline: &str) -> Concern {
    Concern {
        id: id.into(),
        focus: focus.into(),
        severity,
        state: State::Open,
        headline: headline.into(),
        detail: vec!["The last four rounds changed only comments and names.".into()],
        refs: vec!["src/review.rs:88".into()],
        challenge: false,
        evidence: vec![],
        alternative: Some("stop reviewing; run the failing test instead".into()),
        verified: false,
        tag: None,
    }
}

fn wake(n: u64, outcome: Outcome) -> Event {
    Wake {
        n,
        watcher: "companion".into(),
        trigger: "turn-end".into(),
        outcome,
        class: None,
        error: None,
        model: "gpt-6.1-sol".into(),
        spend: 0.23,
        replayed: false,
    }
    .event()
}

struct Fx {
    recs: Vec<Record<Event>>,
}

impl Fx {
    fn new() -> Fx {
        let mut f = Fx { recs: Vec::new() };
        let start = Event::new(Kind::Session)
            .with("phase", "start")
            .with("harness", "claude")
            .with("mode", "companion")
            .with("focuses", json!(["approach", "correctness", "security"]))
            .with("ceiling_usd", 5.0)
            .with("notes", json!([]));
        f.at(10, 0, start);
        f.at(
            10,
            0,
            StatusOp::Model {
                watcher: "companion".into(),
                model: "gpt-6.1-sol".into(),
                same_vendor: false,
                shares_quota: false,
            }
            .event(),
        );
        f
    }

    fn at(&mut self, h: u64, m: u64, e: Event) -> &mut Fx {
        self.recs.push(Record::at(at(h, m), e));
        self
    }

    fn board(&self) -> Board {
        Board::rebuild(0, &self.recs)
    }

    /// Unproductive repetition: a correctness note, an approach-level warn auto-Challenged and
    /// held behind the human's draft, a dismissed security note.
    fn repetition() -> Fx {
        let mut f = Fx::new();
        f.at(10, 30, wake(1, Outcome::Silent));
        let c1 = concern("c1", "correctness", Severity::Note, "Retry loop swallows the timeout error");
        f.at(10, 40, Change::Add { watcher: "companion".into(), concern: c1 }.event());
        f.at(10, 40, wake(2, Outcome::Concern));
        let c2 = concern(
            "c2",
            "approach",
            Severity::Warn,
            "5th review round, 0 new findings since round 1; edits are cosmetic",
        );
        f.at(11, 0, Change::Add { watcher: "companion".into(), concern: c2 }.event());
        f.at(
            11,
            0,
            ChallengeOp::Queued {
                id: "c2".into(),
                concern: "c2".into(),
                focus: "approach".into(),
                origin: Origin::Auto,
                remark: None,
            }
            .event(),
        );
        f.at(11, 0, wake(3, Outcome::Challenge));
        let c3 = concern("c3", "security", Severity::Note, "Token printed at debug level");
        f.at(11, 10, Change::Add { watcher: "companion".into(), concern: c3 }.event());
        f.at(11, 12, Act::Dismiss { id: "c3".into() }.event());
        f.at(11, 20, wake(4, Outcome::Silent));
        f.at(11, 40, wake(5, Outcome::Silent));
        f
    }

    /// …the Challenge goes out, the Worker defends twice, the Watcher escalates.
    fn escalated() -> Fx {
        let mut f = Fx::repetition();
        f.at(11, 45, ChallengeOp::Sent { id: "c2".into(), route: "mod".into() }.event());
        let exchange = |n, reason: &str| DebateOp::Exchange {
            challenge: "c2".into(),
            n,
            verdict: Verdict::Defend,
            reason: Some(reason.into()),
            entry: format!("x{n}"),
            turn: vec![],
        };
        f.at(11, 50, exchange(1, "each round found wording issues reviewers flagged").event());
        let decide = |decision, text: &str| DebateOp::Decision {
            challenge: "c2".into(),
            watcher: "companion".into(),
            decision,
            text: text.into(),
            defaulted: false,
        };
        f.at(11, 50, decide(Decision::Rebut, "no reviewer flagged wording since round 1").event());
        f.at(
            11,
            50,
            DebateOp::Queued { challenge: "c2".into(), item: "c2/2".into(), second: Second::Rebuttal }.event(),
        );
        f.at(11, 55, DebateOp::Sent { challenge: "c2".into(), item: "c2/2".into(), route: "mod".into() }.event());
        f.at(12, 0, exchange(2, "the human asked for a polished diff").event());
        f.at(12, 0, decide(Decision::Escalate, "real trade-off: polish vs progress").event());
        f.at(
            12,
            0,
            DebateOp::End {
                challenge: "c2".into(),
                concern: "c2".into(),
                outcome: DebateOutcome::Unresolved,
                escalated: Some(true),
                withdrawn: None,
            }
            .event(),
        );
        f.at(
            12,
            0,
            EscalationOp::Raised {
                id: "e1".into(),
                cause: Cause::Unresolved,
                concern: Some("c2".into()),
                challenge: Some("c2".into()),
                hook: None,
                title: "Debate unresolved: 5th review round, 0 new findings (c2)".into(),
                lines: vec![
                    "Watcher (approach): stop reviewing; run the failing test instead".into(),
                    "Worker: the human asked for a polished diff".into(),
                ],
                rule: "Rule in the pane".into(),
            }
            .event(),
        );
        f.at(12, 0, wake(6, Outcome::Escalation));
        f
    }
}

/// Review findings: wide glyphs, a detail of leading spaces, three Escalations at once.
fn nasty() -> Fx {
    let mut f = Fx::repetition();
    let mut c = concern("c4", "approach", Severity::Warn, &format!("{}{}", "界".repeat(40), "\r\n".repeat(40)));
    c.detail = vec![format!("{}X", " ".repeat(60))];
    f.at(11, 50, Change::Add { watcher: "companion".into(), concern: c }.event());
    for i in 1..=3 {
        f.at(
            12,
            i,
            EscalationOp::Raised {
                id: format!("e{i}"),
                cause: Cause::Blocked,
                concern: None,
                challenge: None,
                hook: Some(i),
                title: format!("Worker blocked {i}: waiting on you"),
                lines: vec!["Allow `rm -rf target`?".into(), "Watcher (opinion): fine, build output".into()],
                rule: "Answer in the Worker".into(),
            }
            .event(),
        );
    }
    f
}

fn draft_pending() -> Pending {
    Pending { id: "c2".into(), line: "pending: human draft in prompt".into(), waits: vec![at(11, 20), at(11, 40)] }
}

fn golden(name: &str, frame: &Frame) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pane").join(name);
    let text = frame.plain().iter().map(|l| l.trim_end().to_owned()).collect::<Vec<_>>().join("\n") + "\n";
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &text).unwrap();
    }
    let want = fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing {name}; run with UPDATE_GOLDEN=1"));
    assert_eq!(text, want, "{name} (UPDATE_GOLDEN=1 to accept)");
}

fn texts(frame: &Frame) -> String {
    frame.plain().join("\n")
}

#[test]
fn board_goldens_at_48_64_96() {
    let mut b = Fx::repetition().board();
    b.pending = Some(draft_pending());
    for w in WIDTHS {
        golden(&format!("debate-{w}.txt"), &layout(&b, &Ui::default(), w, 24));
    }
    let b = Fx::escalated().board();
    let ui = Ui { closed: true, ..Ui::default() };
    for w in WIDTHS {
        golden(&format!("escalated-{w}.txt"), &layout(&b, &ui, w, 30));
    }
}

#[test]
fn every_frame_fits_its_width_and_rows() {
    let mut debate = Fx::repetition().board();
    debate.pending = Some(draft_pending());
    let mut wide = nasty().board();
    wide.pending = Some(draft_pending());
    let boards = [debate, Fx::escalated().board(), Fx::new().board(), wide];
    let uis = [
        Ui::default(),
        Ui { selected: Some("c4".into()), ..Ui::default() },
        Ui { closed: true, ..Ui::default() },
        Ui { overlay: Overlay::Stream, ..Ui::default() },
        Ui { overlay: Overlay::Help, ..Ui::default() },
    ];
    for b in &boards {
        for ui in &uis {
            for w in 40..=120 {
                for rows in [1, 2, 3, 6, 12, 24, 40] {
                    let f = layout(b, ui, w, rows);
                    assert_eq!(f.lines.len(), rows, "w {w} rows {rows}");
                    for l in &f.lines {
                        assert!(l.width() <= w, "w {w} rows {rows}: {:?}", l.plain());
                    }
                }
            }
        }
    }
}

#[test]
fn board_sections_in_order_and_empty_ones_take_no_rows() {
    let mut b = Fx::repetition().board();
    b.pending = Some(draft_pending());
    let text = texts(&layout(&b, &Ui::default(), 64, 30));
    let pos = |s: &str| text.find(s).unwrap_or_else(|| panic!("no {s:?} in\n{text}"));
    assert!(pos("● watching $1.15/$5 · gpt-6.1-sol · Companion") < pos("wakes ·■■··"));
    assert!(pos("wakes") < pos("IN DEBATE 1") && pos("IN DEBATE 1") < pos("OPEN 1") && pos("OPEN 1") < pos("CLOSED 1"));
    assert!(!text.contains("NEEDS YOU"), "empty section drawn:\n{text}");
    // Pending merges into one line with the wait count (V65).
    assert_eq!(text.matches("pending").count(), 1, "{text}");
    assert!(text.contains("human draft in prompt (2nd wait)"), "{text}");
    // CLOSED is a collapsed count until `c`.
    assert!(!text.contains("c3 dismissed Token"), "{text}");
    let text = texts(&layout(&b, &Ui { closed: true, ..Ui::default() }, 64, 30));
    assert!(text.contains("c3 dismissed Token printed"), "{text}");
}

#[test]
fn small_pane_never_trims_needs_you_pending_or_the_selection() {
    let mut b = Fx::escalated().board();
    // A pending exchange: pretend the rebuttal still waits.
    b.pending = Some(draft_pending());
    for rows in [10, 12, 14] {
        let f = layout(&b, &Ui { closed: true, ..Ui::default() }, 48, rows);
        let text = texts(&f);
        assert!(text.contains("┏") && text.contains("┗"), "rows {rows}:\n{text}");
        assert!(text.contains("Worker: the human asked"), "rows {rows}:\n{text}");
        assert!(text.contains("┃›▲"), "selection kept, rows {rows}:\n{text}");
        assert!(rows < 14 || text.contains("more lines"), "rows {rows}:\n{text}");
    }
}

#[test]
fn too_short_a_pane_goes_compact_and_keeps_every_item() {
    let mut b = nasty().board();
    b.pending = Some(draft_pending());
    let ui = Ui { selected: Some("c4".into()), ..Ui::default() };
    let f = layout(&b, &ui, 48, 12);
    let text = texts(&f);
    for want in ["Worker blocked 1", "Worker blocked 2", "Worker blocked 3", "human draft in prompt", "› c4"] {
        assert!(text.contains(want), "no {want:?} in\n{text}");
    }
    // Only rows actually drawn count as on screen (V69).
    for id in &f.visible {
        assert!(text.contains(&format!(" {id} ")), "{id} visible but not drawn:\n{text}");
    }
    assert!(!f.visible.contains(&"c1".to_owned()), "{text}");
    // Status line + one row per Escalation: headers go first.
    let text = texts(&layout(&nasty().board(), &Ui::default(), 48, 4));
    for want in ["Worker blocked 1", "Worker blocked 2", "Worker blocked 3"] {
        assert!(text.contains(want), "no {want:?} in\n{text}");
    }
}

#[test]
fn selected_open_concern_shows_detail_and_refs() {
    let b = Fx::repetition().board();
    let ui = Ui { selected: Some("c1".into()), ..Ui::default() };
    let text = texts(&layout(&b, &ui, 64, 30));
    assert!(text.contains("› c1 note [correctness]"), "{text}");
    assert!(text.contains("refs: src/review.rs:88"), "{text}");
}

#[test]
fn debate_log_shows_last_two_until_enter() {
    let b = Fx::escalated().board();
    // Put the Debate back in progress: drop the end and the Escalation.
    let mut fx = Fx::escalated();
    fx.recs.truncate(fx.recs.len() - 3);
    let b2 = fx.board();
    let f = layout(&b2, &Ui::default(), 96, 40);
    let text = texts(&f);
    assert!(text.contains("earlier [enter]"), "{text}");
    assert!(!text.contains("Challenge queued"), "{text}");
    let mut ui = Ui::default();
    assert_eq!(pane::key(&b2, &mut ui, &f, Key::Enter, 0), Effect::None);
    let text = texts(&layout(&b2, &ui, 96, 40));
    assert!(text.contains("Challenge queued") && !text.contains("earlier [enter]"), "{text}");
    // Ended: the Concern left IN DEBATE.
    assert!(!texts(&layout(&b, &Ui::default(), 96, 40)).contains("IN DEBATE"));
}

#[test]
fn paused_and_blind_are_inverse_red_banners() {
    let mut f = Fx::repetition();
    for n in 6..30 {
        f.at(12, 0, wake(n, Outcome::Silent));
    }
    let b = f.board();
    let frame = layout(&b, &Ui::default(), 64, 20);
    assert!(frame.lines[0].plain().starts_with("‖ PAUSED $6.67/$5 — not watching;"), "{:?}", frame.lines[0].plain());
    assert!(frame.lines[0].0.iter().all(|s| s.style == Style::Banner));
    assert!(texts(&frame).contains("press $ to add $5 and resume"));
    assert!(texts(&frame).contains("budget 90%: normal wakes stop"), "ladder ticker:\n{}", texts(&frame));

    let mut f = Fx::repetition();
    f.at(12, 3, StatusOp::Blind { focus: None, reason: "API key rejected (401)".into() }.event());
    let frame = layout(&f.board(), &Ui::default(), 64, 20);
    assert!(frame.lines[0].plain().starts_with("‖ BLIND since 12:03 — not watching; silence ≠ all clear"));
    assert!(texts(&frame).contains("API key rejected (401)"));
}

#[test]
fn header_shows_only_non_zero_notes() {
    let mut f = Fx::repetition();
    let b = f.board();
    let line = layout(&b, &Ui::default(), 96, 20).lines[0].plain();
    assert_eq!(line.trim_end(), "● watching $1.15/$5 · gpt-6.1-sol · Companion");
    f.at(12, 0, Act::Mute { focus: "security".into() }.event());
    f.at(12, 0, Act::Nochallenge.event());
    f.at(12, 0, Coverage::Lost { from: at(11, 0), to: at(11, 7), cause: "poison-skip".into() }.event());
    f.at(12, 0, StatusOp::Fallback { model: "sonnet-5.5".into(), class: Class::Service }.event());
    f.at(12, 0, StatusOp::Lagging { focus: None }.event());
    let text = texts(&layout(&f.board(), &Ui::default(), 200, 20));
    for want in [
        "◐ lagging since 12:00 · retrying",
        "fallback sonnet-5.5 (primary failing)",
        "lost 7m",
        "muted: security",
        "no auto-Challenge",
        "░ · last",
    ] {
        assert!(text.contains(want), "no {want:?} in\n{text}");
    }
}

#[test]
fn muted_focus_folds_open_concerns_into_closed() {
    let mut f = Fx::repetition();
    f.at(12, 0, Act::Mute { focus: "correctness".into() }.event());
    let b = f.board();
    assert_eq!(b.closed_as("c1").as_deref(), Some("muted"));
    let text = texts(&layout(&b, &Ui { closed: true, ..Ui::default() }, 64, 30));
    assert!(text.contains("c1 muted"), "{text}");
    assert!(!text.contains("OPEN"), "{text}");
}

#[test]
fn mini_mode_priority() {
    let mini = |b: &Board| layout(b, &Ui::default(), 64, 1).plain()[0].clone();
    let mut quiet = Fx::new();
    quiet.at(10, 5, wake(1, Outcome::Silent));
    assert_eq!(mini(&quiet.board()), "● watching · silent since 10:00 · 0 open");

    let mut b = Fx::repetition().board();
    b.pending = Some(draft_pending());
    assert!(mini(&b).starts_with("⚑ c2 pending: human draft in prompt · 5th review"), "{}", mini(&b));

    let mut f = Fx::escalated();
    assert!(mini(&f.board()).starts_with("▲ Debate unresolved"));
    f.at(12, 5, StatusOp::Lagging { focus: Some("security".into()) }.event());
    assert!(mini(&f.board()).starts_with("▲ Debate unresolved"), "NEEDS YOU outranks lagging");
    f.at(12, 6, StatusOp::Blind { focus: None, reason: "watcher crashed".into() }.event());
    assert_eq!(mini(&f.board()), "‖ BLIND — watcher crashed; not watching");

    let mut f = Fx::repetition();
    f.at(12, 0, StatusOp::UserPause.event());
    assert_eq!(mini(&f.board()), "‖ PAUSED (you) · Z resumes");
}

#[test]
fn stream_overlay_is_chronological_with_debate_indented_and_filtered() {
    let b = Fx::escalated().board();
    let ui = Ui { overlay: Overlay::Stream, ..Ui::default() };
    let text = texts(&layout(&b, &ui, 96, 40));
    let first = text.find("c1 note [correctness]").unwrap();
    let challenge = text.find("  │ Challenge queued").unwrap();
    let end = text.find("e1 Debate unresolved").unwrap();
    assert!(first < challenge && challenge < end, "{text}");
    let ui = Ui { overlay: Overlay::Stream, filter: Some("security".into()), ..Ui::default() };
    let text = texts(&layout(&b, &ui, 96, 40));
    assert!(text.contains("c3") && !text.contains("c1 note"), "{text}");
    // An overlay is not "on screen" for `shown`.
    assert!(layout(&b, &ui, 96, 40).visible.is_empty());
}

#[test]
fn ticker_lines_come_from_the_log() {
    let mut f = Fx::repetition();
    f.at(12, 0, ChallengeOp::PromotionRefused { concern: "c1".into(), reason: "Challenge c2 is open".into() }.event());
    f.at(
        12,
        1,
        Event::new(Kind::Wake)
            .with("n", 7)
            .with("watcher", "companion")
            .with("trigger", "blocked")
            .with("outcome", "failed")
            .with("model", "m"),
    );
    let frame = layout(&f.board(), &Ui::default(), 64, 30);
    let n = frame.lines.len();
    assert_eq!(frame.lines[n - 2].plain(), "12:00 promote c1 refused: Challenge c2 is open");
    assert_eq!(frame.lines[n - 2].0[0].style, Style::Red);
    assert_eq!(frame.lines[n - 1].plain(), "12:01 blocked: considerations unavailable");
}

#[test]
fn keys_write_human_facts_only() {
    let mut b = Fx::repetition().board();
    let mut ui = Ui::default();
    let press = |b: &Board, ui: &mut Ui, k: Key| {
        let f = layout(b, ui, 64, 30);
        ui.selected = f.selected.clone();
        pane::key(b, ui, &f, k, 0)
    };
    // Selection starts at IN DEBATE c2, then OPEN c1.
    assert_eq!(press(&b, &mut ui, Key::Char('j')), Effect::None);
    assert_eq!(ui.selected.as_deref(), Some("c1"));
    assert_eq!(press(&b, &mut ui, Key::Char('d')), Effect::Append(vec![Act::Dismiss { id: "c1".into() }.event()]));
    assert_eq!(press(&b, &mut ui, Key::Char('+')), Effect::Append(vec![Act::Helpful { id: "c1".into() }.event()]));
    assert_eq!(
        press(&b, &mut ui, Key::Char('m')),
        Effect::Append(vec![Act::Mute { focus: "correctness".into() }.event()])
    );
    // `P` opens a one-line remark; Enter sends the request.
    assert_eq!(press(&b, &mut ui, Key::Char('P')), Effect::None);
    for c in "too slow".chars() {
        press(&b, &mut ui, Key::Char(c));
    }
    let text = texts(&layout(&b, &ui, 64, 30));
    assert!(text.contains("remark for c1 (⏎ send · esc cancel): too slow"), "{text}");
    assert_eq!(
        press(&b, &mut ui, Key::Enter),
        Effect::Append(vec![Act::Promote { id: "c1".into(), remark: Some("too slow".into()) }.event()])
    );
    assert_eq!(press(&b, &mut ui, Key::Char('a')), Effect::Append(vec![Act::Nochallenge.event()]));
    assert_eq!(press(&b, &mut ui, Key::Char('Z')), Effect::Append(vec![StatusOp::UserPause.event()]));
    assert_eq!(press(&b, &mut ui, Key::Char('$')), Effect::Append(vec![StatusOp::Raise.event()]));
    // A dismissed Concern: refused locally, nothing written.
    b.apply(&Record::at(at(12, 0), Act::Dismiss { id: "c1".into() }.event()));
    ui.closed = true;
    ui.selected = Some("c3".into());
    assert_eq!(press(&b, &mut ui, Key::Char('d')), Effect::None);
    assert_eq!(ui.note.as_deref(), Some("c3 is already dismissed"));
    assert_eq!(press(&b, &mut ui, Key::Char('P')), Effect::None);
    assert!(ui.input.is_none());
    // Pane-local keys write nothing.
    for k in ['f', 'c', 'l', '?', 'k'] {
        assert_eq!(press(&b, &mut ui, Key::Char(k)), Effect::None, "{k}");
    }
    assert_eq!(ui.filter.as_deref(), Some("approach"));
    assert_eq!(press(&b, &mut ui, Key::Char('q')), Effect::Quit);
}

#[test]
fn shown_after_five_seconds_once() {
    let b = Fx::repetition().board();
    let mut s = Shown::default();
    let t0 = Instant::now();
    let vis = vec!["c1".to_owned(), "c2".to_owned()];
    assert!(s.due(&b, &vis, t0).is_empty());
    assert!(s.due(&b, &vis, t0 + Duration::from_secs(4)).is_empty());
    assert_eq!(s.due(&b, &vis, t0 + SHOWN_AFTER), vec!["c1", "c2"]);
    assert!(s.due(&b, &vis, t0 + Duration::from_secs(9)).is_empty(), "once");
    // Hidden resets the clock; already logged is never repeated.
    let mut s = Shown::default();
    s.due(&b, &vis, t0);
    s.due(&b, &[], t0 + Duration::from_secs(3));
    assert!(s.due(&b, &vis, t0 + Duration::from_secs(6)).is_empty());
    let mut f = Fx::repetition();
    f.at(12, 0, pair_watcher::human::shown("c1"));
    let mut s = Shown::default();
    s.due(&f.board(), &vis, t0);
    assert_eq!(s.due(&f.board(), &vis, t0 + SHOWN_AFTER), vec!["c2"]);
    // The Board shows which rows are on screen.
    assert_eq!(layout(&b, &Ui::default(), 64, 30).visible, vec!["c2", "c1"]);
}

#[test]
fn first_concern_escalation_or_blind_opens_the_pane() {
    let fx = Fx::escalated();
    let first = fx.recs.iter().position(|r| pane::opens(&r.body)).unwrap();
    assert_eq!(fx.recs[first].body.str("op"), Some("add"));
    assert!(!fx.recs[..first].iter().any(|r| pane::opens(&r.body)));
    assert!(pane::opens(&StatusOp::Blind { focus: None, reason: "x".into() }.event()));
    assert!(!pane::opens(&StatusOp::Lagging { focus: None }.event()));
}

#[test]
fn promote_request_is_answered_by_the_arbiter_once() {
    let dir = TempDir::new().unwrap();
    let log = EventLog::new(dir.path().join("log.jsonl"));
    let outbox = Outbox::new(dir.path().join("outbox.jsonl"));
    let c1 = concern("c1", "correctness", Severity::Note, "a");
    let c2 = concern("c2", "approach", Severity::Warn, "b");
    for c in [c1, c2] {
        log.append(Change::Add { watcher: "companion".into(), concern: c }.event()).unwrap();
    }
    log.append(Act::Promote { id: "c1".into(), remark: Some("look".into()) }.event()).unwrap();
    log.append(Act::Promote { id: "c2".into(), remark: None }.event()).unwrap();
    let events = || log.read_from(0).unwrap().records.into_iter().map(|r| r.body).collect::<Vec<_>>();
    let list = ConcernList::rebuild(&events());
    let mut ch = Challenges::rebuild(&events());
    let refused = ch.answer_requests(&log, &outbox, &list, 1).unwrap();
    assert_eq!(refused, vec!["promotion refused: Challenge c1 is open"]);
    assert_eq!(ch.slot().map(|s| s.id.as_str()), Some("c1"));
    assert_eq!(outbox.pending().unwrap().iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["c1"]);
    // Rebuilt from the log: both answered, nothing asked again.
    let mut again = Challenges::rebuild(&events());
    assert!(again.answer_requests(&log, &outbox, &list, 2).unwrap().is_empty());
    assert_eq!(events().iter().filter(|e| e.k == Kind::Challenge).count(), 2);
}

#[test]
fn outbox_counts_draft_waits() {
    let dir = TempDir::new().unwrap();
    let outbox = Outbox::new(dir.path().join("outbox.jsonl"));
    outbox.queue(Item { id: "c2".into(), kind: ItemKind::Challenge, text: "x".into() }, 1).unwrap();
    outbox.hold("c2", "mod", Some("draft"), 2).unwrap();
    outbox.hold("c2", "mod", Some("draft"), 3).unwrap();
    outbox.hold("c2", "mod", None, 4).unwrap();
    outbox.hold("c2", "mod", Some("draft"), 5).unwrap();
    assert_eq!(outbox.waits("c2").unwrap().len(), 2);
}

#[test]
fn control_characters_never_reach_the_terminal() {
    let mut f = Fx::new();
    let c = concern("c1", "approach", Severity::Warn, "evil \x1b]0;pwned\x07 title");
    f.at(10, 1, Change::Add { watcher: "companion".into(), concern: c }.event());
    let frame = layout(&f.board(), &Ui::default(), 64, 20);
    for l in &frame.lines {
        assert!(!l.plain().chars().any(|c| c.is_control()), "{:?}", l.plain());
        assert!(!l.ansi().contains("\x1b]"), "{:?}", l.ansi());
    }
}

#[test]
fn wrap_keeps_hanging_indent_and_width() {
    let lines = wrap("aaa bbb ccc ddd eee fff ggg", 12, 4);
    assert_eq!(lines, vec!["aaa bbb ccc", "    ddd eee", "    fff ggg"]);
    assert_eq!(wrap("abcdefghijklmnopqrstuvwxyz", 10, 2), vec!["abcdefghij", "  klmnopqr", "  stuvwxyz"]);
}

#[test]
fn terminal_keys_parse() {
    assert_eq!(
        term::parse(b"jk\x1b[A\x1b[B\r\x1b\x7f\x03P$"),
        vec![
            Key::Char('j'),
            Key::Char('k'),
            Key::Up,
            Key::Down,
            Key::Enter,
            Key::Esc,
            Key::Backspace,
            Key::CtrlC,
            Key::Char('P'),
            Key::Char('$'),
        ]
    );
    assert_eq!(term::parse(b"\x1b[5~x"), vec![Key::Char('x')], "unknown sequence skipped");
}

fn session(home: &Home, id: &str, repo: &Path, live: bool) -> Mailbox {
    let m = Mailbox::register(home, id).unwrap();
    m.log()
        .append(Event::new(Kind::Session).with("phase", "start").with("repo", repo.to_string_lossy().into_owned()))
        .unwrap();
    if !live {
        m.unregister().unwrap();
    }
    m
}

#[test]
fn session_default_is_the_only_live_one_in_this_repo() {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path().join(".pair-watcher"));
    home.ensure_layout().unwrap();
    let (here, there) = (dir.path().join("here"), dir.path().join("there"));
    assert!(mailbox::resolve(&home, None, &here, false).unwrap_err().contains("no live session"));
    session(&home, "s1", &here, true);
    session(&home, "s2", &there, true);
    session(&home, "s0", &here, false);
    assert_eq!(mailbox::resolve(&home, None, &here, false).unwrap().id(), "s1");
    session(&home, "s3", &here, true);
    let err = mailbox::resolve(&home, None, &here, false).unwrap_err();
    assert!(err.contains("2 live sessions") && err.contains("s1") && err.contains("s3"), "{err}");
    assert_eq!(mailbox::resolve(&home, Some("s3"), &here, false).unwrap().id(), "s3");
    // An ended session: the pane may show it; commands may not act on it.
    assert!(mailbox::resolve(&home, Some("s0"), &here, false).unwrap_err().contains("has ended"));
    assert_eq!(mailbox::resolve(&home, Some("s0"), &here, true).unwrap().id(), "s0");
    assert!(mailbox::resolve(&home, Some("../x"), &here, true).is_err());
}

#[test]
fn cli_mute_and_pause_append_facts() {
    let dir = TempDir::new().unwrap();
    let home_dir = dir.path().to_path_buf();
    let home = Home::new(home_dir.join(".pair-watcher"));
    home.ensure_layout().unwrap();
    let m = session(&home, "s1", dir.path(), true);
    m.log().append(Event::new(Kind::Session).with("phase", "start").with("focuses", json!(["approach"]))).unwrap();
    let pw = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_pair-watcher"))
            .args(args)
            .current_dir(dir.path())
            .env("HOME", &home_dir)
            .output()
            .unwrap()
    };
    let out = pw(&["mute", "approach", "--session", "s1"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!pw(&["mute", "nope", "--session", "s1"]).status.success());
    assert!(pw(&["pause", "--session", "s1"]).status.success());
    assert!(pw(&["pause", "--session", "s1"]).status.success());
    assert!(pw(&["nochallenge", "--session", "s1"]).status.success());
    let b = Board::rebuild(0, &m.log().read_from(0).unwrap().records);
    assert_eq!(b.muted, vec!["approach"]);
    assert!(!b.user_paused && b.nochallenge);
}
