//! §T17 pre-filter: modes (V53), eligibility (V54), System One client + skip gating + fail open +
//! threshold keys + fit (V55, I16).

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use pair_watcher::event::{Event, Kind};
use pair_watcher::home::Home;
use pair_watcher::mailbox::Mailbox;
use pair_watcher::prefilter::{
    self, Answer, Client, Fitted, Gate, MIN_POSITIVES, Mode, Sample, Tag, Thresholds, Verdict, Wakeup,
};
use pair_watcher::status::{Outcome, Wake};
use serde_json::{Value, json};
use tempfile::TempDir;

/// One request seen: lowercase header lines + JSON body.
#[derive(Clone, Debug)]
struct Seen {
    headers: Vec<String>,
    body: Value,
}

/// Answers each request with `(status, reply)`.
fn serve(status: u16, reply: Value) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut headers, mut len) = (Vec::new(), 0);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let line = line.trim_end().to_ascii_lowercase();
                if let Some(v) = line.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                headers.push(line);
            }
            let mut buf = vec![0; len];
            reader.read_exact(&mut buf).unwrap();
            log.lock().unwrap().push(Seen { headers, body: serde_json::from_slice(&buf).unwrap_or(Value::Null) });
            let text = reply.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    (url, seen)
}

fn noul(p: f64, model: &str) -> Value {
    json!({"model": model, "answers": {"worth_review": {"type": "noul", "noul": p}}, "usage": {"input_tokens": 50, "output_tokens": 1}})
}

fn client(url: &str, key: Option<&str>, digest: Option<&str>) -> Client {
    Client {
        url: url.into(),
        model: "nimble".into(),
        key: key.map(str::to_owned),
        digest: digest.map(str::to_owned),
        timeout: Duration::from_secs(5),
    }
}

#[test]
fn mode_follows_config_and_probe() {
    // V53: auto = shadow when the model is pulled, else off silently; never skip by default.
    assert_eq!(prefilter::mode("auto", true), Mode::Shadow);
    assert_eq!(prefilter::mode("auto", false), Mode::Off);
    assert_eq!(prefilter::mode("off", true), Mode::Off);
    assert_eq!(prefilter::mode("shadow", false), Mode::Shadow);
    assert_eq!(prefilter::mode("skip", true), Mode::Skip);
}

#[test]
fn only_checkpoint_and_turn_end_wakes_of_new_work_are_eligible() {
    // V54.
    assert!(Wakeup::default().eligible());
    assert!(!Wakeup { blocked: true, ..Wakeup::default() }.eligible());
    assert!(!Wakeup { owes_reply: true, ..Wakeup::default() }.eligible());
    assert!(!Wakeup { catch_up: true, ..Wakeup::default() }.eligible());
}

#[test]
fn local_urls_keep_evidence_on_the_machine() {
    // C8: anything else is labelled.
    for u in ["http://localhost:11434", "http://127.0.0.1:1", "http://[::1]:11434/", "localhost"] {
        assert!(prefilter::local(u), "{u}");
    }
    for u in ["https://api.typesafe.ai", "http://192.168.1.4:11434", "https://localhost.example"] {
        assert!(!prefilter::local(u), "{u}");
    }
}

#[test]
fn client_asks_the_fixed_noul_question() {
    // I16: POST <url>/v1/systemone {model, state, questions}; bearer key for hosted backends.
    let (url, seen) = serve(200, noul(0.12, "jev-1.13.0"));
    let a = client(&url, Some("sk-1"), None).ask("the wake input").unwrap();
    assert_eq!(a, Answer { score: 0.12, version: "jev-1.13.0".into() });
    let seen = seen.lock().unwrap();
    let req = &seen[0];
    assert!(req.headers[0].starts_with("post /v1/systemone "), "{:?}", req.headers);
    assert!(req.headers.contains(&"authorization: bearer sk-1".to_owned()));
    assert_eq!(req.body["model"], "nimble");
    assert_eq!(req.body["state"], "the wake input");
    let q = &req.body["questions"][prefilter::QUESTION];
    assert_eq!(q["type"], "noul");
    assert!(q["criteria"]["true"].is_string() && q["criteria"]["false"].is_string());
}

#[test]
fn ollama_version_carries_the_digest() {
    // V55: the resolved model version, not the alias Ollama echoes back.
    let (url, seen) = serve(200, noul(0.5, "nimble"));
    let a = client(&url, None, Some("0123456789abcdef")).ask("x").unwrap();
    assert_eq!(a.version, "nimble@0123456789ab");
    assert!(!seen.lock().unwrap()[0].headers.iter().any(|h| h.starts_with("authorization")));
    let tags = [Tag { name: "nimble:latest".into(), digest: "d".into() }];
    assert!(prefilter::pulled(&tags, "nimble").is_some());
    assert!(prefilter::pulled(&tags, "nimble:q4").is_none());
}

#[test]
fn failures_are_errors_never_scores() {
    // V55: oversized input is refused before any call.
    let (url, seen) = serve(200, noul(0.1, "m"));
    let big = "x".repeat(prefilter::MAX_STATE_TOKENS * 4 + 8);
    assert!(client(&url, None, None).ask(&big).unwrap_err().contains("oversized"));
    assert!(seen.lock().unwrap().is_empty());

    // Detected truncation: the backend read far fewer tokens than the state holds.
    let (url, _) = serve(200, noul(0.1, "m"));
    let state = "y".repeat(40_000);
    assert!(client(&url, None, None).ask(&state).unwrap_err().contains("truncated"));

    // Status and shape errors.
    let (url, _) = serve(422, json!({"error": "questions"}));
    assert!(client(&url, None, None).ask("x").unwrap_err().contains("422"));
    let (url, _) = serve(200, json!({"answers": {"worth_review": {"type": "choice", "choice": "a"}}}));
    assert!(client(&url, None, None).ask("x").unwrap_err().contains("unexpected answer"));
    let (url, _) = serve(200, noul(1.5, "m"));
    assert!(client(&url, None, None).ask("x").is_err());
    assert!(client("http://127.0.0.1:1", None, None).ask("x").is_err());
}

fn fitted(skip_below: f64) -> Fitted {
    Fitted { skip_below, target: 0.98, n: 100, positives: 60 }
}

#[test]
fn skip_only_below_a_threshold_fitted_for_the_exact_key() {
    // V55: skip iff mode skip AND a fitted skip_below for (backend, version, question) AND score < it.
    let url = "http://localhost:11434";
    let key = prefilter::key(url, "nimble@abc");
    assert_eq!(key, "http://localhost:11434|nimble@abc|worth_review@1");
    let t: Thresholds = [(key.clone(), fitted(0.2))].into();
    let ans = |p: f64, v: &str| Ok(Answer { score: p, version: v.into() });

    let v = prefilter::decide(ans(0.1, "nimble@abc"), url, &t, Mode::Skip);
    assert_eq!(
        v,
        Verdict { score: Some(0.1), key: Some(key.clone()), would_skip: Some(true), skipped: true, error: None }
    );
    // At the threshold: wake.
    assert!(!prefilter::decide(ans(0.2, "nimble@abc"), url, &t, Mode::Skip).skipped);
    // Shadow logs would-skip, never skips.
    let v = prefilter::decide(ans(0.1, "nimble@abc"), url, &t, Mode::Shadow);
    assert_eq!((v.would_skip, v.skipped), (Some(true), false));
    // Model re-pulled: new key, no threshold → wake, would-skip unknown.
    let v = prefilter::decide(ans(0.01, "nimble@def"), url, &t, Mode::Skip);
    assert_eq!((v.would_skip, v.skipped), (None, false));
    // Failure: wake with the error, no score.
    let v = prefilter::decide(Err("timeout".into()), url, &t, Mode::Skip);
    assert_eq!(v, Verdict { error: Some("timeout".into()), ..Verdict::default() });
}

#[test]
fn gate_probes_labels_and_refuses_unfitted_skip() {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path());
    home.ensure_layout().unwrap();
    let none = |_: &str| -> Result<Vec<Tag>, String> { Err("refused".into()) };
    let nimble =
        |_: &str| -> Result<Vec<Tag>, String> { Ok(vec![Tag { name: "nimble:latest".into(), digest: "feed".into() }]) };
    let local = "http://localhost:11434";

    // V53: auto without Ollama is off, silently; with nimble pulled it is shadow.
    assert!(Gate::open(&home, "auto", local, "nimble", None, &none).is_none());
    let g = Gate::open(&home, "auto", local, "nimble", None, &nimble).unwrap();
    assert_eq!((g.mode, g.client.digest.as_deref()), (Mode::Shadow, Some("feed")));
    assert!(g.notes.is_empty());
    assert!(Gate::open(&home, "off", local, "nimble", None, &nimble).is_none());

    // Hosted: labelled; skip with no fitted key for the backend: refused, runs shadow.
    let hosted = "https://api.typesafe.ai";
    let g = Gate::open(&home, "skip", hosted, "jev-latest", Some("k".into()), &none).unwrap();
    assert!(g.notes.iter().any(|n| n.contains(prefilter::LEAVES)), "{:?}", g.notes);
    assert!(g.notes.iter().any(|n| n.contains("skip refused")), "{:?}", g.notes);
    let t: Thresholds = [(prefilter::key(hosted, "jev-1.13.0"), fitted(0.1))].into();
    fs::write(prefilter::thresholds_path(&home), serde_json::to_vec(&t).unwrap()).unwrap();
    let g = Gate::open(&home, "skip", hosted, "jev-latest", Some("k".into()), &none).unwrap();
    assert!(!g.notes.iter().any(|n| n.contains("skip refused")));
    assert_eq!(g.thresholds, t);
}

fn sample(score: f64, outcome: Outcome) -> Sample {
    Sample { key: "k".into(), score, outcome }
}

#[test]
fn fit_keeps_recall_targets() {
    // V55: recall ≥ target on Concern wakes, 1.0 on Challenge/Escalation wakes.
    let mut s: Vec<Sample> = (0..100).map(|i| sample(i as f64 / 100.0, Outcome::Concern)).collect();
    s.extend((0..40).map(|_| sample(0.001, Outcome::Silent)));
    let f = prefilter::fit(&s, 0.98)["k"].clone().unwrap();
    // 2 of 100 Concern wakes may fall below: skip_below = the 3rd lowest score.
    assert_eq!(f.skip_below, 0.02);
    assert_eq!((f.n, f.positives), (140, 100));
    let missed = s.iter().filter(|x| x.outcome != Outcome::Silent && x.score < f.skip_below).count();
    assert!(missed <= 2);

    // A Challenge wake scored low pulls the threshold under it.
    s.push(sample(0.005, Outcome::Challenge));
    assert_eq!(prefilter::fit(&s, 0.98)["k"].clone().unwrap().skip_below, 0.005);
    // Target 1.0: no Concern wake may be skipped.
    assert_eq!(prefilter::fit(&s, 1.0)["k"].clone().unwrap().skip_below, 0.0);

    // Too few Concern wakes: not fitted.
    let few: Vec<Sample> = (0..MIN_POSITIVES - 1).map(|_| sample(0.5, Outcome::Concern)).collect();
    assert!(prefilter::fit(&few, 0.98)["k"].is_err());
}

fn wake(outcome: Outcome, v: Option<Verdict>) -> Wake {
    Wake {
        n: 1,
        watcher: "companion".into(),
        trigger: "turn-end".into(),
        outcome,
        class: None,
        error: None,
        model: "m".into(),
        spend: 0.0,
        replayed: false,
        rulings: vec![],
        prefilter: v,
        input: None,
    }
}

fn scored(p: f64) -> Option<Verdict> {
    Some(Verdict { score: Some(p), key: Some("http://h|m|worth_review@1".into()), ..Verdict::default() })
}

#[test]
fn fit_reads_scored_wakes_that_ran() {
    let skipped = Some(Verdict { skipped: true, ..scored(0.0).unwrap() });
    let wakes = [
        wake(Outcome::Concern, scored(0.4)),
        wake(Outcome::Silent, scored(0.1)),
        wake(Outcome::Failed, scored(0.1)),
        wake(Outcome::Skipped, skipped),
        wake(Outcome::Concern, None),
        wake(Outcome::Silent, Some(Verdict { error: Some("timeout".into()), ..Verdict::default() })),
    ];
    let s = prefilter::samples(&wakes);
    assert_eq!(
        s.iter().map(|x| (x.score, x.outcome)).collect::<Vec<_>>(),
        [(0.4, Outcome::Concern), (0.1, Outcome::Silent)]
    );
}

#[test]
fn fit_cmd_writes_fitted_keys_and_keeps_others() {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path());
    home.ensure_layout().unwrap();
    let other: Thresholds = [("x|y|worth_review@1".to_owned(), fitted(0.3))].into();
    fs::write(prefilter::thresholds_path(&home), serde_json::to_vec(&other).unwrap()).unwrap();
    assert!(prefilter::fit_cmd(&home, 1.5).is_err());

    let log = Mailbox::register(&home, "s1").unwrap().log();
    for i in 0..60 {
        log.append(wake(Outcome::Concern, scored(0.2 + i as f64 / 100.0)).event()).unwrap();
        log.append(wake(Outcome::Silent, scored(0.01)).event()).unwrap();
    }
    log.append(Event::new(Kind::Session).with("phase", "start")).unwrap();
    let lines = prefilter::fit_cmd(&home, 0.98).unwrap();
    assert!(lines[0].starts_with("http://h|m|worth_review@1: skip_below 0.2100"), "{lines:?}");
    let t = prefilter::load_thresholds(&home);
    assert_eq!(t["http://h|m|worth_review@1"].skip_below, 0.21);
    assert_eq!(t["x|y|worth_review@1"], fitted(0.3));
}
