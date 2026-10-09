//! Wake pre-filter (V53–V55): one System One client (I16) asks a fixed question of each eligible
//! wake input. Shadow scores and logs; `skip` acts only on a threshold fitted for this exact
//! (backend, resolved model version, question) key. Every failure is a wake, never a score.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::event::Kind;
use crate::home::{Home, write_atomic};
use crate::status::{Outcome, Wake};

/// The fixed question. Its id and version are part of every threshold key (V55).
pub const QUESTION: &str = "worth_review";
pub const QUESTION_VERSION: u32 = 1;
const INSTRUCTIONS: &str = "This is a code reviewer's input for one wake: the coding agent's latest work and the code \
     changes since the last review. Does it contain anything a reviewer with the given focus should comment on?";
const IF_TRUE: &str = "Something here is worth a reviewer's full look.";
const IF_FALSE: &str = "Nothing new here a reviewer would comment on.";

pub const DEFAULT_URL: &str = "http://localhost:11434";
pub const DEFAULT_MODEL: &str = "nimble";
/// Hosted backends: env first, then `credentials` (I16).
pub const KEY_ENV: &str = "SYSTEMONE_API_KEY";
/// One call; local prefill of a full wake takes seconds.
pub const CLOCK: Duration = Duration::from_secs(15);
/// Above this estimate the backend would cut the state: fail open without calling (V55).
pub const MAX_STATE_TOKENS: usize = 32_000;
/// Reported input under this share of the estimate = the backend cut the state.
const TRUNCATED_BELOW: f64 = 0.5;
/// `prefilter fit` refuses a key with fewer wakes that produced a Concern.
pub const MIN_POSITIVES: usize = 50;
pub const DEFAULT_TARGET: f64 = 0.98;
pub const LEAVES: &str = "wake evidence leaves this machine";

/// What the pre-filter does this session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// Scores, never skips (V53).
    Shadow,
    /// Skips below a fitted threshold for the call's key; else as shadow (V55).
    Skip,
}

/// `prefilter.mode` against the probe: `auto` = shadow when the model is pulled, else off silently.
pub fn mode(configured: &str, pulled: bool) -> Mode {
    match configured {
        "shadow" => Mode::Shadow,
        "skip" => Mode::Skip,
        "auto" if pulled => Mode::Shadow,
        _ => Mode::Off,
    }
}

/// Which wakes may be scored (V54): checkpoint and turn-end wakes of new work only.
#[derive(Clone, Copy, Debug, Default)]
pub struct Wakeup {
    pub blocked: bool,
    /// A Debate reply is owed.
    pub owes_reply: bool,
    /// A re-wake, recovery replay, merged span or the final drain.
    pub catch_up: bool,
}

impl Wakeup {
    pub fn eligible(&self) -> bool {
        !self.blocked && !self.owes_reply && !self.catch_up
    }
}

/// `localhost`-like: evidence stays on this machine (C8).
pub fn local(url: &str) -> bool {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = match rest.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => rest.split(['/', ':']).next().unwrap_or_default(),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// One model an Ollama server has pulled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    pub digest: String,
}

/// `GET <url>/api/tags`.
pub fn ollama_tags(url: &str) -> Result<Vec<Tag>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(2))).build().into();
    let body: Value = agent
        .get(&format!("{}/api/tags", url.trim_end_matches('/')))
        .call()
        .and_then(|mut r| r.body_mut().read_json())
        .map_err(|e| e.to_string())?;
    Ok(body["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(Tag {
                name: m["name"].as_str()?.to_owned(),
                digest: m["digest"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .collect())
}

/// The pulled tag `model` names: exact, or `:latest` for an untagged name.
pub fn pulled<'a>(tags: &'a [Tag], model: &str) -> Option<&'a Tag> {
    let latest = format!("{model}:latest");
    tags.iter().find(|t| t.name == model || (!model.contains(':') && t.name == latest))
}

/// The threshold key (V55): backend, resolved model version, question id + version.
pub fn key(url: &str, version: &str) -> String {
    format!("{}|{version}|{QUESTION}@{QUESTION_VERSION}", url.trim_end_matches('/'))
}

/// One backend's keys share this prefix and suffix.
fn same_backend(key: &str, url: &str) -> bool {
    key.starts_with(&format!("{}|", url.trim_end_matches('/')))
        && key.ends_with(&format!("|{QUESTION}@{QUESTION_VERSION}"))
}

/// A System One answer to the fixed question.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// P(wake worth a full call).
    pub score: f64,
    /// Resolved model version (alias + digest for Ollama).
    pub version: String,
}

/// I16: base URL + model + key.
#[derive(Clone, Debug)]
pub struct Client {
    pub url: String,
    pub model: String,
    pub key: Option<String>,
    /// Ollama's digest of `model`: the response names only the alias.
    pub digest: Option<String>,
    pub timeout: Duration,
}

impl Client {
    pub fn ask(&self, state: &str) -> Result<Answer, String> {
        let estimate = estimate_tokens(state);
        if estimate > MAX_STATE_TOKENS {
            return Err(format!("oversized input (~{estimate} tokens)"));
        }
        let body = json!({
            "model": self.model,
            "state": state,
            "questions": {QUESTION: {
                "type": "noul",
                "instructions": INSTRUCTIONS,
                "criteria": {"true": IF_TRUE, "false": IF_FALSE},
            }},
        });
        let agent: ureq::Agent =
            ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(self.timeout)).build().into();
        let mut req = agent.post(&format!("{}/v1/systemone", self.url.trim_end_matches('/')));
        if let Some(key) = &self.key {
            req = req.header("authorization", &format!("Bearer {key}"));
        }
        let mut resp = req.send_json(&body).map_err(|e| match e {
            ureq::Error::Timeout(_) => "timeout".to_owned(),
            e => e.to_string(),
        })?;
        let status = resp.status().as_u16();
        let reply: Value = resp.body_mut().read_json().map_err(|e| format!("HTTP {status}: {e}"))?;
        if status != 200 {
            return Err(format!("HTTP {status}"));
        }
        self.answer(&reply, estimate)
    }

    fn answer(&self, reply: &Value, estimate: usize) -> Result<Answer, String> {
        let a = &reply["answers"][QUESTION];
        let score = match (a["type"].as_str(), a["noul"].as_f64()) {
            (Some("noul") | None, Some(p)) if (0.0..=1.0).contains(&p) => p,
            _ => return Err(format!("unexpected answer: {a}")),
        };
        if let Some(read) = reply["usage"]["input_tokens"].as_u64()
            && (read as f64) < estimate as f64 * TRUNCATED_BELOW
        {
            return Err(format!("truncated input ({read} of ~{estimate} tokens read)"));
        }
        let model = reply["model"].as_str().filter(|m| !m.is_empty()).unwrap_or(&self.model);
        let version = match &self.digest {
            Some(d) => format!("{model}@{}", &d[..d.len().min(12)]),
            None => model.to_owned(),
        };
        Ok(Answer { score, version })
    }
}

/// A rough token count (≈4 bytes per token) for oversized and truncation checks.
pub fn estimate_tokens(state: &str) -> usize {
    state.len().div_ceil(4)
}

/// What one Watcher's wake got from the pre-filter: logged on its `wake` event (V94).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Verdict {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Unset with no fitted threshold for the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub would_skip: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    /// Failed open: the wake ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Skip iff mode is `skip`, the key has a fitted `skip_below` and `score < skip_below` (V55).
pub fn decide(answer: Result<Answer, String>, url: &str, thresholds: &Thresholds, mode: Mode) -> Verdict {
    match answer {
        Err(error) => Verdict { error: Some(error), ..Verdict::default() },
        Ok(a) => {
            let key = key(url, &a.version);
            let would_skip = thresholds.get(&key).map(|t| a.score < t.skip_below);
            let skipped = mode == Mode::Skip && would_skip == Some(true);
            Verdict { score: Some(a.score), key: Some(key), would_skip, skipped, error: None }
        }
    }
}

/// One fitted threshold in `prefilter/thresholds.json`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Fitted {
    pub skip_below: f64,
    /// Recall target on Concern wakes; Challenge/Escalation wakes are held at 1.0.
    #[serde(default)]
    pub target: f64,
    /// Scored wakes the fit read, and those that produced a Concern.
    #[serde(default)]
    pub n: usize,
    #[serde(default)]
    pub positives: usize,
}

pub type Thresholds = BTreeMap<String, Fitted>;

pub fn thresholds_path(home: &Home) -> PathBuf {
    home.root().join("prefilter").join("thresholds.json")
}

/// Missing = none fitted; unreadable = none either (skip refused, fail open).
pub fn load_thresholds(home: &Home) -> Thresholds {
    fs::read(thresholds_path(home)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// `skip` has a fitted key for this backend and question (`doctor`, start ticker).
pub fn fitted_for(thresholds: &Thresholds, url: &str) -> bool {
    thresholds.keys().any(|k| same_backend(k, url))
}

/// The session's pre-filter, built when the Watcher starts (the probe "at `run`", V53).
#[derive(Clone, Debug)]
pub struct Gate {
    pub mode: Mode,
    pub client: Client,
    pub thresholds: Thresholds,
    /// Ticker lines for the start: egress label, refused `skip`.
    pub notes: Vec<String>,
    /// A failure was put on the ticker; cleared by the next answer (V86: ticker once).
    pub down: bool,
}

impl Gate {
    /// `None` = off. `probe` = the Ollama tags at `url`.
    pub fn open(
        home: &Home,
        configured: &str,
        url: &str,
        model: &str,
        key: Option<String>,
        probe: &dyn Fn(&str) -> Result<Vec<Tag>, String>,
    ) -> Option<Gate> {
        if configured == "off" {
            return None;
        }
        let tag = probe(url).ok().and_then(|tags| pulled(&tags, model).cloned());
        let mode = mode(configured, tag.is_some());
        if mode == Mode::Off {
            return None;
        }
        let thresholds = load_thresholds(home);
        let mut notes = Vec::new();
        if !local(url) {
            notes.push(format!("pre-filter at {url}: {LEAVES}"));
        }
        if mode == Mode::Skip && !fitted_for(&thresholds, url) {
            notes.push(
                "prefilter.mode = skip refused: no fitted skip_below (pair-watcher prefilter fit); runs shadow".into(),
            );
        }
        let client = Client {
            url: url.to_owned(),
            model: model.to_owned(),
            key,
            digest: tag.map(|t| t.digest).filter(|d| !d.is_empty()),
            timeout: CLOCK,
        };
        Some(Gate { mode, client, thresholds, notes, down: false })
    }

    pub fn score(&self, input: &str) -> Verdict {
        decide(self.client.ask(input), &self.client.url, &self.thresholds, self.mode)
    }
}

/// One scored wake that ran (fit input).
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub key: String,
    pub score: f64,
    pub outcome: Outcome,
}

/// The wake events that can fit a threshold: scored, ran to an accepted outcome, not skipped.
pub fn samples(wakes: &[Wake]) -> Vec<Sample> {
    wakes
        .iter()
        .filter(|w| matches!(w.outcome, Outcome::Silent | Outcome::Concern | Outcome::Challenge | Outcome::Escalation))
        .filter_map(|w| {
            let v = w.prefilter.as_ref().filter(|v| !v.skipped)?;
            Some(Sample { key: v.key.clone()?, score: v.score?, outcome: w.outcome })
        })
        .collect()
}

/// Largest `skip_below` per key keeping recall ≥ `target` on Concern wakes (a Challenge or
/// Escalation wake also produced one) and 1.0 on Challenge/Escalation wakes (V55, V98).
pub fn fit(samples: &[Sample], target: f64) -> BTreeMap<String, Result<Fitted, String>> {
    let mut by_key: BTreeMap<&str, Vec<&Sample>> = BTreeMap::new();
    for s in samples {
        by_key.entry(&s.key).or_default().push(s);
    }
    by_key
        .into_iter()
        .map(|(key, all)| {
            let mut pos: Vec<f64> = all.iter().filter(|s| s.outcome != Outcome::Silent).map(|s| s.score).collect();
            pos.sort_by(f64::total_cmp);
            let fitted = if pos.len() < MIN_POSITIVES {
                Err(format!("{} Concern wakes scored, need {MIN_POSITIVES}", pos.len()))
            } else {
                // Skips are `score < skip_below`: at most `misses` positives fall below pos[misses].
                let misses = ((1.0 - target) * pos.len() as f64 + 1e-9).floor() as usize;
                let strict = all
                    .iter()
                    .filter(|s| matches!(s.outcome, Outcome::Challenge | Outcome::Escalation))
                    .map(|s| s.score)
                    .fold(f64::INFINITY, f64::min);
                Ok(Fitted { skip_below: pos[misses].min(strict), target, n: all.len(), positives: pos.len() })
            };
            (key.to_owned(), fitted)
        })
        .collect()
}

/// Every `wake` event under `sessions/*/`.
pub fn wakes(home: &Home) -> io::Result<Vec<Wake>> {
    let mut out = Vec::new();
    let Ok(dirs) = fs::read_dir(home.sessions_dir()) else { return Ok(out) };
    for dir in dirs.filter_map(Result::ok) {
        let log = crate::event::EventLog::new(dir.path().join(crate::mailbox::LOG_FILE));
        if !log.path().exists() {
            continue;
        }
        let batch = log.read_from(0)?;
        out.extend(batch.records.iter().filter(|r| r.body.k == Kind::Wake).filter_map(|r| r.body.parse::<Wake>()));
    }
    Ok(out)
}

/// `prefilter fit`: writes each fitted key, keeps the rest; one line per key.
pub fn fit_cmd(home: &Home, target: f64) -> io::Result<Vec<String>> {
    if !(target > 0.0 && target <= 1.0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("--target {target}: must be in (0, 1]")));
    }
    let samples = samples(&wakes(home)?);
    if samples.is_empty() {
        return Ok(vec!["no scored wakes yet: run with prefilter.mode = shadow (or auto with Ollama)".into()]);
    }
    let mut thresholds = load_thresholds(home);
    let mut lines = Vec::new();
    for (key, fitted) in fit(&samples, target) {
        match fitted {
            Ok(f) => {
                lines.push(format!(
                    "{key}: skip_below {:.4} (recall ≥ {target} on {} Concern wakes, n {})",
                    f.skip_below, f.positives, f.n
                ));
                thresholds.insert(key, f);
            }
            Err(why) => lines.push(format!("{key}: not fitted: {why}")),
        }
    }
    save_thresholds(&thresholds_path(home), &thresholds)?;
    Ok(lines)
}

fn save_thresholds(path: &Path, t: &Thresholds) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    write_atomic(path, &serde_json::to_vec_pretty(t)?)
}
