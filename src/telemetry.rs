//! Opt-in weekly aggregate telemetry (V99, C8): off by default; the same ratios as `report`,
//! never text, paths, repo names or prompts; built-in Focus names as-is, custom ones `custom`;
//! rulings as counts only. A failed send is dropped: no retry, nothing queued past 7 days.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use toml_edit::{DocumentMut, value};

use crate::home::{Home, write_atomic};
use crate::problem::tilde;
use crate::report::{self, Filter, Stats};

const WEEK_MS: u64 = 7 * 86_400_000;
const SCHEMA: u32 = 1;
/// Names sent as-is; everything else is `custom`.
const KNOWN: [&str; 4] = ["approach", "correctness", "security", "companion"];
/// The project's collector. None in this build: a due send is dropped (V99), `preview` still
/// shows the payload.
pub const ENDPOINT: Option<&str> = None;

/// When the last weekly window closed.
#[derive(Serialize, Deserialize, Default)]
struct State {
    last_sent: u64,
}

fn state_path(home: &Home) -> PathBuf {
    home.root().join("telemetry.json")
}

fn load(home: &Home) -> State {
    fs::read(state_path(home)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(home: &Home, state: &State) -> io::Result<()> {
    write_atomic(&state_path(home), &serde_json::to_vec(state)?)
}

/// `telemetry on|off`: sets `telemetry` in the global config, keeping the rest as written. `on`
/// starts the first weekly window now.
pub fn set(home: &Home, on: bool, now: u64) -> io::Result<()> {
    let path = home.config_file();
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", tilde(&path)))
    })?;
    doc.insert("telemetry", value(on));
    write_atomic(&path, doc.to_string().as_bytes())?;
    if on {
        save(home, &State { last_sent: now })?;
    }
    Ok(())
}

fn name(n: &str) -> String {
    if KNOWN.contains(&n) { n.to_owned() } else { "custom".to_owned() }
}

/// The next payload: sessions started since the last window closed (at most 7 days back).
pub fn payload(home: &Home, now: u64) -> io::Result<Value> {
    let from = load(home).last_sent.max(now.saturating_sub(WEEK_MS));
    let mut sessions = report::sessions(home, &Filter::default())?;
    sessions.retain(|s| s.start().is_some_and(|r| r.t >= from && r.t <= now));
    let stats = Stats::fold(&sessions, &Filter::default());
    let held: usize = fs::read_dir(home.repos_dir())
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().ends_with(".rulings.toml"))
                .map(|e| crate::standing::load(&e.path()).map_or(0, |v| v.len()))
                .sum()
        })
        .unwrap_or(0);
    let cited: std::collections::BTreeSet<(&PathBuf, &String)> = stats.cited.iter().map(|(d, id, _)| (d, id)).collect();
    let mut v = report::aggregate(&stats, &name);
    v["schema"] = json!(SCHEMA);
    v["window_ms"] = json!([from, now]);
    v["rulings"] =
        json!({ "held": held, "added": stats.rulings_added, "removed": stats.rulings_removed, "cited": cited.len() });
    Ok(v)
}

/// What a due weekly send did.
#[derive(Debug, PartialEq, Eq)]
pub enum Sent {
    NotDue,
    Sent,
    /// Dropped, not retried (V99).
    Dropped(String),
}

/// At `run` when `telemetry = true`: sends the week's payload once 7 days have passed since the
/// last window; the window closes whether or not the send worked.
pub fn weekly(home: &Home, now: u64, post: &dyn Fn(&Value) -> Result<(), String>) -> io::Result<Sent> {
    let state = load(home);
    if state.last_sent == 0 {
        save(home, &State { last_sent: now })?;
        return Ok(Sent::NotDue);
    }
    if now < state.last_sent + WEEK_MS {
        return Ok(Sent::NotDue);
    }
    let body = payload(home, now)?;
    save(home, &State { last_sent: now })?;
    Ok(match post(&body) {
        Ok(()) => Sent::Sent,
        Err(e) => Sent::Dropped(e),
    })
}

/// HTTPS POST to [`ENDPOINT`].
pub fn post(body: &Value) -> Result<(), String> {
    let Some(url) = ENDPOINT else { return Err("no collector in this build".into()) };
    ureq::post(url).send_json(body).map(|_| ()).map_err(|e| e.to_string())
}
