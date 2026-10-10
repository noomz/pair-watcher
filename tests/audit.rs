//! §T18 `replay --variant` (V98, V96) and `audit` (V97) against a mock model server.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use common::{Server, anthropic_reply, input_of};
use pair_watcher::audit::{self, Label, Op as AuditOp, Queue, Why};
use pair_watcher::concern::{Change, Concern, Severity, State};
use pair_watcher::credentials::Keys;
use pair_watcher::event::{Event, EventLog, Kind};
use pair_watcher::home::Home;
use pair_watcher::human::Act;
use pair_watcher::mailbox::LOG_FILE;
use pair_watcher::models::{Table, Usage};
use pair_watcher::provider::Api;
use pair_watcher::record::{self, WakeRecord};
use pair_watcher::replay::{self, Current, Variant};
use pair_watcher::report::{Filter, sessions};
use pair_watcher::status::{Coverage, Outcome, Wake};
use pair_watcher::wake::{Input, Section};
use tempfile::TempDir;

const ADD_APPROACH: &str = r#"{"op":"add","severity":"warn","focus":"approach","headline":"retry loop again"}"#;

struct Fx {
    _t: TempDir,
    home: Home,
}

impl Fx {
    fn new() -> Fx {
        let t = TempDir::new().unwrap();
        let home = Home::new(t.path().join("home"));
        home.ensure_layout().unwrap();
        Fx { _t: t, home }
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.home.sessions_dir().join(id)
    }

    fn log(&self, id: &str) -> EventLog {
        EventLog::new(self.dir(id).join(LOG_FILE))
    }

    fn start(&self, id: &str) {
        fs::create_dir_all(self.dir(id)).unwrap();
        self.log(id).append(Event::new(Kind::Session).with("phase", "start").with("repo", "/nowhere")).unwrap();
    }

    fn end(&self, id: &str) {
        self.log(id).append(Event::new(Kind::Session).with("phase", "end")).unwrap();
    }

    /// Wake `n`: its turn text, the recorded answer, its outcome and model.
    fn wake(&self, id: &str, n: u64, turn: &str, output: Option<&str>, outcome: Outcome, model: &str) {
        let rec = WakeRecord {
            n,
            watcher: "companion".into(),
            trigger: "turn-end".into(),
            prompt: "OLD PROMPT".into(),
            briefs: vec![
                ("approach".into(), "old approach brief".into()),
                ("correctness".into(), "old correctness brief".into()),
            ],
            input: Input { sections: vec![Section { title: "Turn".into(), body: turn.into() }], diff_bytes: 0 },
            tree: None,
            head: None,
            output: output.map(Into::into),
        };
        let rel = record::write(&self.dir(id), &rec).unwrap();
        let w = Wake {
            n,
            watcher: "companion".into(),
            trigger: "turn-end".into(),
            outcome,
            model: model.into(),
            spend: 0.01,
            record: Some(rel),
            ..Wake::default()
        };
        self.log(id).append(w.event()).unwrap();
    }

    fn api(&self, server: &Server) -> Api {
        let url = server.url.clone();
        let env = move |k: &str| match k {
            "ANTHROPIC_BASE_URL" => Some(url.clone()),
            "ANTHROPIC_API_KEY" => Some("k".into()),
            _ => None,
        };
        Api::new(Keys::load(&self.home, &env), &env, Duration::from_secs(10))
    }
}

fn concern(id: &str, focus: &str, headline: &str) -> Concern {
    Concern {
        id: id.into(),
        focus: focus.into(),
        severity: Severity::Warn,
        state: State::Open,
        headline: headline.into(),
        detail: vec![],
        refs: vec![],
        challenge: false,
        evidence: vec![],
        alternative: None,
        verified: false,
        tag: None,
        debate: None,
    }
}

fn finding(id: &str, wake: u64, focus: &str) -> AuditOp {
    AuditOp::Finding {
        id: id.into(),
        wake,
        watcher: "companion".into(),
        focus: focus.into(),
        headline: "missed it".into(),
        detail: vec![],
    }
}

/// Session `s1`: wake 1 raised a Concern the human dismissed; wake 2 was silent and the audit
/// confirmed a correctness miss there.
fn replay_fixture(fx: &Fx) {
    fx.start("s1");
    fx.wake("s1", 1, "Wake one turn.", Some(ADD_APPROACH), Outcome::Concern, "claude-sonnet-5-5");
    let log = fx.log("s1");
    log.append(
        Change::Add { watcher: "companion".into(), concern: concern("A1", "approach", "retry loop again") }.event(),
    )
    .unwrap();
    log.append(Act::Dismiss { id: "A1".into() }.event()).unwrap();
    fx.wake("s1", 2, "Wake two turn.", Some(""), Outcome::Silent, "claude-sonnet-5-5");
    log.append(finding("F1", 2, "correctness").event()).unwrap();
    log.append(AuditOp::Label { finding: "F1".into(), label: Label::Miss }.event()).unwrap();
    fx.end("s1");
}

/// V98: `--variant prompt` re-renders each recorded wake with the current prompt and the recorded
/// briefs, on the recorded model; scored against labels (misses now caught), dismissals (raised
/// again) and cost.
#[test]
fn replay_prompt_variant_scores_against_labels() {
    let fx = Fx::new();
    replay_fixture(&fx);
    let caught = r#"{"op":"add","severity":"warn","focus":"correctness","headline":"off by one"}"#;
    let server = Server::start(move |b| anthropic_reply(if input_of(b).contains("Wake two") { caught } else { "" }));
    let table = Table::embedded();

    let (all, items) = replay::select(&fx.home, "s1").unwrap();
    assert_eq!(items.len(), 2);
    let briefs = [("approach".to_owned(), "NEW approach brief".to_owned())].into();
    let cur = Current { prompt: "NEW PROMPT", briefs: &briefs, model: Some("auto") };
    let jobs = replay::jobs(&items, Variant::Prompt, &cur).unwrap();
    assert!(replay::estimate(&jobs, &table).unwrap() > 0.0);
    let score = replay::run(&fx.api(&server), &table, &all, &items, &jobs);

    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    let input = input_of(&reqs[0]);
    assert!(input.starts_with("NEW PROMPT") && input.contains("old approach brief") && !input.contains("OLD PROMPT"));
    assert_eq!(reqs[0]["model"], "claude-sonnet-5-5");
    assert_eq!((score.misses_caught, score.misses), (1, 1));
    assert_eq!((score.dismissed_again, score.dismissed), (0, 1));
    assert!((score.original_usd - 0.02).abs() < 1e-9 && score.variant_usd > 0.0);
    let summary = score.summary();
    assert!(summary.iter().any(|l| l.contains("confirmed misses now caught 1/1")), "{summary:#?}");
}

/// V98: `--variant focus` swaps in the briefs on disk (same names), keeps the recorded prompt;
/// `--variant model` runs the configured model, refusing `auto`. Wake sets: `labelled`,
/// `<session>:<n>[-<m>]`.
#[test]
fn replay_focus_and_model_variants_and_wake_sets() {
    let fx = Fx::new();
    replay_fixture(&fx);
    let server = Server::start(|_| anthropic_reply(ADD_APPROACH));
    let table = Table::embedded();
    let briefs = [("approach".to_owned(), "NEW approach brief".to_owned())].into();

    let (all, items) = replay::select(&fx.home, "labelled").unwrap();
    assert_eq!(items.iter().map(|i| i.wake.n).collect::<Vec<_>>(), [2]);
    let cur = Current { prompt: "NEW PROMPT", briefs: &briefs, model: Some("auto") };
    let jobs = replay::jobs(&items, Variant::Focus, &cur).unwrap();
    let score = replay::run(&fx.api(&server), &table, &all, &items, &jobs);
    let input = input_of(&server.requests()[0]);
    assert!(
        input.starts_with("OLD PROMPT")
            && input.contains("NEW approach brief")
            && input.contains("old correctness brief")
    );
    assert_eq!((score.misses_caught, score.misses), (0, 1), "an approach Concern does not catch a correctness miss");

    assert!(replay::jobs(&items, Variant::Model, &cur).unwrap_err().contains("auto"));
    let (all, items) = replay::select(&fx.home, "s1:1-1").unwrap();
    let cur = Current { model: Some("claude-opus-5-5"), ..cur };
    let jobs = replay::jobs(&items, Variant::Model, &cur).unwrap();
    let score = replay::run(&fx.api(&server), &table, &all, &items, &jobs);
    assert_eq!(server.requests()[1]["model"], "claude-opus-5-5");
    assert_eq!((score.dismissed_again, score.dismissed), (1, 1), "the dismissed Concern is raised again");

    assert!(replay::select(&fx.home, "nope").is_err());
    assert!(replay::select(&fx.home, "s1:x").is_err());
}

/// Session `s2` (ended): wakes 1–4 on gpt-6.1-sol; the human corrected the Worker after wake 2
/// (flagged at wake 3); wake 4 sits in a lost span. Session `live` has not ended.
fn audit_fixture(fx: &Fx) {
    fx.start("s2");
    for n in 1..=3 {
        fx.wake("s2", n, &format!("Turn {n}."), Some(""), Outcome::Silent, "gpt-6.1-sol");
    }
    let correction = Act::Correction { prompt_ref: "no, revert that".into(), watcher: "companion".into(), wake: 3 };
    fx.log("s2").append(correction.event()).unwrap();
    // Wake 4 alone in the lost span: a later millisecond than wake 3.
    std::thread::sleep(Duration::from_millis(5));
    fx.wake("s2", 4, "Turn 4.", Some(""), Outcome::Silent, "gpt-6.1-sol");
    let log = fx.log("s2");
    let wakes: Vec<u64> =
        log.read_from(0).unwrap().records.iter().filter(|r| r.body.k == Kind::Wake).map(|r| r.t).collect();
    let to = pair_watcher::jsonl::now_ms() + 1;
    log.append(Coverage::Lost { from: wakes[3], to, cause: "poison-skip".into() }.event()).unwrap();
    fx.end("s2");
    fx.start("live");
    fx.wake("live", 1, "Live.", Some(""), Outcome::Silent, "gpt-6.1-sol");
}

/// V97: candidates = the wake before a correction + a random sample of silent/skipped wakes from
/// auditable coverage (ended sessions only); auditor = next tier up from the other vendor;
/// unpriced refused.
#[test]
fn audit_candidates_and_auditor_choice() {
    let fx = Fx::new();
    audit_fixture(&fx);
    let table = Table::embedded();
    let all = sessions(&fx.home, &Filter::default()).unwrap();

    assert_eq!(audit::auditor(&table, "gpt-6.1-sol", None).unwrap(), "claude-opus-5-5");
    assert_eq!(audit::auditor(&table, "claude-sonnet-5-5", None).unwrap(), "gpt-6-astra");
    assert!(audit::auditor(&table, "gpt-6.1-sol", Some("mystery-1")).unwrap_err().contains("no price"));

    let c = audit::candidates(&all, 20, &table, None, 7).unwrap();
    let picked: Vec<(u64, Why)> = c.iter().map(|c| (c.wake, c.why)).collect();
    assert_eq!(picked[0], (2, Why::Correction));
    let mut sampled: Vec<u64> = picked[1..].iter().map(|(n, _)| *n).collect();
    sampled.sort_unstable();
    assert_eq!(sampled, [1, 3], "wake 4 lost, wake 2 already a candidate, live session skipped");
    assert!(c.iter().all(|c| c.session == "s2" && c.auditor == "claude-opus-5-5"));
    assert_eq!(audit::candidates(&all, 1, &table, None, 7).unwrap().len(), 2);
}

/// V97: the run logs candidate + findings to the session log, stops before passing `--max-usd`
/// (the queue saved for `--resume`), never re-audits; an invalid answer fails that wake only; the
/// human's labels are what count.
#[test]
fn audit_run_resume_and_labels() {
    let fx = Fx::new();
    audit_fixture(&fx);
    let table = Table::embedded();
    let all = sessions(&fx.home, &Filter::default()).unwrap();
    let line = r#"{"focus":"correctness","headline":"the revert was predictable"}"#;
    let server = Server::start(move |b| {
        let input = input_of(b);
        anthropic_reply(if input.contains("Turn 3.") { "not json" } else { line })
    });
    let api = fx.api(&server);
    let left = audit::candidates(&all, 20, &table, None, 7).unwrap();
    assert_eq!(left.len(), 3);
    // Room for the first call, not for the second's estimate on top of what the first spent.
    let first_spend =
        table.price("claude-opus-5-5").unwrap().cost(&Usage { input: 1000, output: 100, ..Usage::default() });
    let budget = first_spend + audit::estimate(&all, &table, &left[1..2]) - 1e-6;
    assert!(budget >= audit::estimate(&all, &table, &left[..1]));

    let ran = audit::run(&fx.home, &api, &table, &all, Queue { left: left.clone(), spent: 0.0 }, budget).unwrap();
    assert!(ran.stopped && ran.audited == 1 && ran.findings == 1);
    assert_eq!(audit::load_queue(&fx.home).left.len(), 2, "rest kept for --resume");

    let ran = audit::run(&fx.home, &api, &table, &all, audit::load_queue(&fx.home), 5.0).unwrap();
    assert_eq!((ran.audited, ran.findings, ran.failed, ran.stopped), (1, 1, 1, false));
    assert!(audit::load_queue(&fx.home).left.is_empty());
    let input = input_of(&server.requests()[0]);
    assert!(input.contains("You audit a Watcher") && input.contains("OLD PROMPT") && input.contains("(silent)"));

    let all = sessions(&fx.home, &Filter::default()).unwrap();
    let again = audit::candidates(&all, 20, &table, None, 7).unwrap();
    assert_eq!(again.iter().map(|c| c.wake).collect::<Vec<_>>(), [3], "only the failed wake again");

    let todo = audit::unlabelled(&all);
    assert_eq!(todo.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), ["A1", "A2"]);
    audit::label(&todo[0], Label::Miss).unwrap();
    audit::label(&todo[1], Label::AlreadySaid).unwrap();
    let all = sessions(&fx.home, &Filter::default()).unwrap();
    assert!(audit::unlabelled(&all).is_empty());
    let stats = pair_watcher::report::Stats::fold(&all, &Filter::default());
    assert_eq!(stats.audit["correctness"], (1, 1, 0));
    let spent: f64 = all[0]
        .records
        .iter()
        .filter_map(|r| match r.body.parse::<AuditOp>() {
            Some(AuditOp::Candidate { spend, .. }) => Some(spend),
            _ => None,
        })
        .sum();
    assert!(spent > 0.0);
}
