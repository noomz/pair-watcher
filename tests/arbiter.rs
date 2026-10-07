use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pair_watcher::arbiter::{self, CAP, Merged, Output, Watcher};
use pair_watcher::concern::{self, Change, ConcernList, Resolution, Severity, State};
use pair_watcher::event::{EventLog, Kind};
use pair_watcher::focus::{Focus, FocusSource};
use pair_watcher::output::{self, Line};
use tempfile::TempDir;

fn watcher(name: &str, focuses: &[&str]) -> Watcher {
    Watcher { name: name.into(), focuses: focuses.iter().map(|f| f.to_string()).collect(), model: None }
}

fn companion() -> Watcher {
    watcher("companion", &["approach", "correctness", "security"])
}

fn lines(jsonl: &str) -> Vec<Line> {
    output::parse(jsonl).unwrap()
}

fn add(sev: &str, headline: &str) -> String {
    format!(r#"{{"op":"add","severity":"{sev}","headline":"{headline}"}}"#)
}

fn add_in(focus: &str, sev: &str, headline: &str) -> String {
    format!(r#"{{"op":"add","severity":"{sev}","focus":"{focus}","headline":"{headline}"}}"#)
}

/// Runs one wake: merge then fold into the list (no log).
fn wake(list: &mut ConcernList, outs: &[(&Watcher, String)]) -> Merged {
    let outputs: Vec<Output> = outs.iter().map(|(w, text)| Output { watcher: w, lines: lines(text) }).collect();
    let merged = arbiter::merge(list, &outputs);
    for c in &merged.changes {
        list.apply(&c.event());
    }
    merged
}

fn ops(m: &Merged) -> Vec<String> {
    m.changes
        .iter()
        .map(|c| match c {
            Change::Add { concern, .. } => format!("add {} {}", concern.id, concern.headline),
            Change::Update { concern, .. } => format!("update {}", concern.id),
            Change::Resolve { id, .. } => format!("resolve {id}"),
            Change::Duplicate { id, of, .. } => format!("duplicate {id} of {of}"),
            Change::Trimmed { headline, id, .. } => format!("trimmed {}", id.as_deref().unwrap_or(headline)),
            Change::Unverifiable { id, .. } => format!("unverifiable {id}"),
        })
        .collect()
}

fn state(list: &ConcernList, id: &str) -> State {
    list.get(id).unwrap().state
}

#[test]
fn add_mints_ids_oldest_first() {
    let mut list = ConcernList::default();
    let w = watcher("approach", &["approach"]);
    let m = wake(&mut list, &[(&w, add("note", "a"))]);
    assert_eq!(ops(&m), ["add c1 a"]);
    wake(&mut list, &[(&w, add("note", "b"))]);
    let ids: Vec<&str> = list.all().iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["c1", "c2"]);
    assert_eq!(list.get("c1").unwrap().focus, "approach");
}

/// V13: update own with new evidence; without new evidence the update is dropped.
#[test]
fn update_needs_new_evidence() {
    let mut list = ConcernList::default();
    let w = companion();
    wake(&mut list, &[(&w, add_in("correctness", "note", "retry has no test"))]);

    let same = wake(&mut list, &[(&w, r#"{"op":"update","id":"c1","severity":"note"}"#.into())]);
    assert!(same.changes.is_empty());
    assert_eq!(same.dropped, ["companion: c1: update without new evidence"]);

    let m = wake(&mut list, &[(&w, r#"{"op":"update","id":"c1","severity":"warn","refs":["src/c.ts:57"]}"#.into())]);
    assert_eq!(ops(&m), ["update c1"]);
    let c = list.get("c1").unwrap();
    assert_eq!(
        (c.severity, c.refs.as_slice(), c.headline.as_str()),
        (Severity::Warn, &["src/c.ts:57".to_owned()][..], "retry has no test")
    );
}

#[test]
fn resolve_own_and_closed_stays_closed() {
    let mut list = ConcernList::default();
    let w = watcher("approach", &["approach"]);
    wake(&mut list, &[(&w, format!("{}\n{}", add("note", "a"), add("note", "b")))]);
    let m = wake(&mut list, &[(&w, r#"{"op":"resolve","id":"c1","reason":"fixed"}"#.into())]);
    assert!(matches!(&m.changes[0], Change::Resolve { reason: Resolution::Fixed, .. }));
    assert_eq!(state(&list, "c1"), State::Resolved);

    let again = wake(&mut list, &[(&w, r#"{"op":"update","id":"c1","severity":"warn"}"#.into())]);
    assert_eq!(again.dropped, ["approach: c1: already resolved"]);

    let both = wake(
        &mut list,
        &[(
            &w,
            r#"{"op":"resolve","id":"c2","reason":"no-longer-applies"}
{"op":"update","id":"c2","severity":"warn"}"#
                .into(),
        )],
    );
    assert_eq!(ops(&both), ["resolve c2"]);
    assert_eq!(both.dropped, ["approach: c2: already changed this wake"]);
}

/// V13: a dismissed Concern never reopens; only a distinct one may be raised.
#[test]
fn dismissal_is_final() {
    let mut list = ConcernList::default();
    let w = watcher("approach", &["approach"]);
    wake(&mut list, &[(&w, add("warn", "Round 3 review is a loop"))]);
    list.apply(&concern::dismissal("c1"));
    assert_eq!(state(&list, "c1"), State::Dismissed);

    let m = wake(
        &mut list,
        &[(
            &w,
            format!(
                "{}\n{}\n{}\n{}",
                r#"{"op":"update","id":"c1","detail":["new"]}"#,
                r#"{"op":"resolve","id":"c1","reason":"fixed"}"#,
                add("warn", "round 3   REVIEW is a loop"),
                add("warn", "Round 4 review requested too"),
            ),
        )],
    );
    assert_eq!(
        m.dropped,
        [
            "approach: c1: dismissed, never reopens",
            "approach: c1: dismissed, never reopens",
            "approach: repeat of dismissed c1",
        ]
    );
    assert_eq!(ops(&m), ["add c2 Round 4 review requested too"]);
    // A dismissal never flips a closed Concern.
    list.apply(&concern::dismissal("c2"));
    list.apply(&concern::dismissal("c2"));
    assert_eq!(state(&list, "c2"), State::Dismissed);
}

/// V13: re-raise without new evidence is dropped; a resolved Concern may return with new evidence.
#[test]
fn repeats_are_dropped() {
    let mut list = ConcernList::default();
    let w = watcher("correctness", &["correctness"]);
    wake(&mut list, &[(&w, add("note", "Retry has no test"))]);
    let open = wake(&mut list, &[(&w, add("note", "retry has no test"))]);
    assert_eq!(open.dropped, ["correctness: repeat of c1"]);

    wake(&mut list, &[(&w, r#"{"op":"resolve","id":"c1","reason":"fixed"}"#.into())]);
    let same = wake(&mut list, &[(&w, add("note", "Retry has no test"))]);
    assert_eq!(same.dropped, ["correctness: re-raise of c1 without new evidence"]);
    let fresh = wake(
        &mut list,
        &[(&w, r#"{"op":"add","severity":"note","headline":"Retry has no test","refs":["src/c.ts:80"]}"#.into())],
    );
    assert_eq!(ops(&fresh), ["add c2 Retry has no test"]);
}

/// V44: in Swarm each Watcher touches only its own Focus's Concerns and may not repeat another's.
#[test]
fn swarm_watchers_touch_only_their_own() {
    let mut list = ConcernList::default();
    let approach = watcher("approach", &["approach"]);
    let security = watcher("security", &["security"]);
    wake(&mut list, &[(&approach, add("warn", "loop")), (&security, add("warn", "token in log"))]);
    assert_eq!(list.get("c2").unwrap().focus, "security");

    let m = wake(
        &mut list,
        &[(
            &approach,
            format!(
                "{}\n{}\n{}\n{}",
                r#"{"op":"update","id":"c2","severity":"note"}"#,
                r#"{"op":"resolve","id":"c2","reason":"fixed"}"#,
                add_in("security", "warn", "other"),
                add("warn", "Token in log"),
            ),
        )],
    );
    assert!(m.changes.is_empty());
    assert_eq!(
        m.dropped,
        [
            "approach: c2: owned by Focus security",
            "approach: c2: owned by Focus security",
            "approach: add for Focus security, not this Watcher's",
            "approach: repeat of c2",
        ]
    );
}

/// V44: a same-wake duplicate is resolved by the newer owner as `duplicate of <id>` next wake.
#[test]
fn duplicate_resolution() {
    let mut list = ConcernList::default();
    let approach = watcher("approach", &["approach"]);
    let correctness = watcher("correctness", &["correctness"]);
    wake(&mut list, &[(&approach, add("note", "loop on reviews")), (&correctness, add("note", "review loop"))]);
    let m = wake(&mut list, &[(&correctness, r#"{"op":"resolve","id":"c2","reason":"duplicate","of":"c1"}"#.into())]);
    assert_eq!(ops(&m), ["duplicate c2 of c1"]);
    assert_eq!(state(&list, "c2"), State::Resolved);
    assert_eq!(state(&list, "c1"), State::Open);

    let unknown =
        wake(&mut list, &[(&approach, r#"{"op":"resolve","id":"c1","reason":"duplicate","of":"c9"}"#.into())]);
    assert_eq!(unknown.dropped, ["approach: c1: duplicate of unknown c9"]);
    let own = wake(&mut list, &[(&approach, r#"{"op":"resolve","id":"c1","reason":"duplicate","of":"c1"}"#.into())]);
    assert_eq!(own.dropped, ["approach: c1: duplicate of unknown c1"]);
}

/// Companion = Swarm of one: one Watcher owns every Focus, so each add names its Focus.
#[test]
fn companion_adds_name_their_focus() {
    let mut list = ConcernList::default();
    let w = companion();
    let m = wake(&mut list, &[(&w, format!("{}\n{}", add("note", "no focus"), add_in("security", "warn", "secret")))]);
    assert_eq!(m.dropped, ["companion: add without focus"]);
    assert_eq!(ops(&m), ["add c1 secret"]);
    assert_eq!(list.get("c1").unwrap().focus, "security");
    let r = wake(&mut list, &[(&w, r#"{"op":"resolve","id":"c1","reason":"fixed"}"#.into())]);
    assert_eq!(ops(&r), ["resolve c1"]);
}

/// V14: ≤3 new-or-updated per wake; trimmed never listed, so a later raise is not a repeat.
#[test]
fn cap_trims_and_trimmed_is_not_a_repeat() {
    let mut list = ConcernList::default();
    let w = watcher("approach", &["approach"]);
    let five: Vec<String> = (1..=5).map(|i| add("note", &format!("n{i}"))).collect();
    let m = wake(&mut list, &[(&w, five.join("\n"))]);
    assert_eq!(ops(&m), ["add c1 n1", "add c2 n2", "add c3 n3", "trimmed n4", "trimmed n5"]);
    assert_eq!(list.all().len(), CAP);

    let later = wake(&mut list, &[(&w, add("note", "n4"))]);
    assert_eq!(ops(&later), ["add c4 n4"]);
}

/// V14: resolutions are free; trimmed updates leave the Concern unchanged.
#[test]
fn resolutions_free_updates_capped() {
    let mut list = ConcernList::default();
    let w = watcher("approach", &["approach"]);
    wake(&mut list, &[(&w, (1..=3).map(|i| add("note", &format!("n{i}"))).collect::<Vec<_>>().join("\n"))]);
    let text = [
        r#"{"op":"resolve","id":"c1","reason":"fixed"}"#.to_owned(),
        add("warn", "w1"),
        add("warn", "w2"),
        add("warn", "w3"),
        r#"{"op":"update","id":"c2","detail":["more"]}"#.to_owned(),
    ]
    .join("\n");
    let m = wake(&mut list, &[(&w, text)]);
    assert_eq!(ops(&m), ["resolve c1", "add c4 w1", "add c5 w2", "add c6 w3", "trimmed c2"]);
    assert!(list.get("c2").unwrap().detail.is_empty());
}

/// V14: fill order challenge-flagged, `warn`, `note`; round-robin across Watchers by ranking.
#[test]
fn cap_order_and_round_robin() {
    let mut list = ConcernList::default();
    let a = watcher("approach", &["approach"]);
    let b = watcher("correctness", &["correctness"]);
    let challenge = r#"{"op":"add","severity":"note","headline":"ch","challenge":true,"alternative":"stop"}"#;
    let m = wake(
        &mut list,
        &[
            (&a, [add("note", "a-note"), add("warn", "a-warn1"), add("warn", "a-warn2")].join("\n")),
            (&b, [add("warn", "b-warn1"), challenge.to_owned(), add("warn", "b-warn2")].join("\n")),
        ],
    );
    assert_eq!(
        ops(&m),
        ["add c1 ch", "add c2 a-warn1", "add c3 b-warn1", "trimmed a-note", "trimmed a-warn2", "trimmed b-warn2"]
    );
}

/// Non-Concern lines pass through with their Watcher for Debate/Escalation/ruling handling.
#[test]
fn other_lines_pass_through() {
    let list = ConcernList::default();
    let w = watcher("approach", &["approach"]);
    let out = [Output { watcher: &w, lines: lines(r#"{"op":"consider","text":"approve deletes cache"}"#) }];
    let m = arbiter::merge(&list, &out);
    assert!(m.changes.is_empty());
    assert_eq!(m.rest, [("approach".to_owned(), Line::Consider { text: "approve deletes cache".into() })]);
}

/// V93: the arbiter writes the log; the list rebuilds from it exactly, dismissals included.
#[test]
fn list_rebuilds_from_event_log() {
    let dir = TempDir::new().unwrap();
    let log = EventLog::new(dir.path().join("log.jsonl"));
    let mut list = ConcernList::default();
    let w = companion();
    let wakes = [
        [add_in("approach", "warn", "loop"), add_in("security", "note", "token"), add_in("correctness", "note", "bug")]
            .join("\n"),
        r#"{"op":"resolve","id":"c3","reason":"fixed"}
{"op":"update","id":"c2","detail":["in auth.log"]}"#
            .to_owned(),
    ];
    for text in &wakes {
        let outputs = [Output { watcher: &w, lines: lines(text) }];
        let m = arbiter::merge(&list, &outputs);
        arbiter::commit(&mut list, &log, &m.changes).unwrap();
    }
    log.append(concern::dismissal("c1")).unwrap();
    list.apply(&concern::dismissal("c1"));

    let batch = log.read_from(0).unwrap();
    assert!(batch.invalid.is_empty());
    assert!(batch.records.iter().all(|r| matches!(r.body.k, Kind::Concern | Kind::Human)));
    let rebuilt = ConcernList::rebuild(batch.records.iter().map(|r| &r.body));
    assert_eq!(rebuilt, list);
    let states: Vec<State> = rebuilt.all().iter().map(|c| c.state).collect();
    assert_eq!(states, [State::Dismissed, State::Open, State::Resolved]);
    assert_eq!(rebuilt.get("c2").unwrap().detail, ["in auth.log"]);
}

fn focus(name: &str, model: Option<&str>) -> Focus {
    Focus {
        name: name.into(),
        brief: format!("{name} brief"),
        model: model.map(str::to_owned),
        source: model.map_or(FocusSource::BuiltIn, |_| FocusSource::User(PathBuf::from(format!("{name}.md")))),
        sha256: String::new(),
    }
}

/// V43/V45: Companion = one Watcher with every brief on the default model; Swarm = one per Focus
/// with its user-file model.
#[test]
fn roster_companion_and_swarm() {
    let enabled = [focus("approach", None), focus("perf", Some("claude-sonnet-5-5"))];
    assert_eq!(
        arbiter::roster(&enabled, false),
        [Watcher { name: "companion".into(), focuses: vec!["approach".into(), "perf".into()], model: None }]
    );
    assert_eq!(
        arbiter::roster(&enabled, true),
        [
            Watcher { name: "approach".into(), focuses: vec!["approach".into()], model: None },
            Watcher { name: "perf".into(), focuses: vec!["perf".into()], model: Some("claude-sonnet-5-5".into()) },
        ]
    );
}

/// V43: every Watcher's call runs at once; results come back in roster order.
#[test]
fn run_calls_watchers_in_parallel() {
    let roster = arbiter::roster(&[focus("approach", None), focus("correctness", None), focus("security", None)], true);
    let started = AtomicUsize::new(0);
    let results = arbiter::run(&roster, |w| {
        started.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while started.load(Ordering::SeqCst) < roster.len() {
            assert!(Instant::now() < deadline, "{} ran alone: calls are not parallel", w.name);
            std::thread::sleep(Duration::from_millis(1));
        }
        w.name.clone()
    });
    assert_eq!(results, ["approach", "correctness", "security"]);
}
