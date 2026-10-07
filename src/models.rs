//! Model + price table: embedded `models.toml` with `~/.pair-watcher/models.toml` merged over it
//! (I2), spend in API-equivalent USD (V47), `model = auto` (V46), `models update|rollback` (V62).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::ops::AddAssign;
use std::path::Path;

use serde::Deserialize;

use crate::home::{self, Home};
use crate::problem::{line_of, tilde};

pub const EMBEDDED: &str = include_str!("../models.toml");
/// Fixed project URL `models update` fetches from (V62).
pub const UPDATE_URL: &str = "https://raw.githubusercontent.com/noomz/pair-watcher/main/models.toml";
const SCHEMA: u32 = 1;
const PER_TOKEN: f64 = 1e-6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Anthropic,
    Openai,
}

impl Vendor {
    pub fn name(self) -> &'static str {
        match self {
            Vendor::Anthropic => "anthropic",
            Vendor::Openai => "openai",
        }
    }

    /// The vendor behind a provider name as a harness transcript records it (`openai-codex`, …).
    pub fn of_provider(provider: &str) -> Option<Vendor> {
        let p = provider.to_ascii_lowercase();
        if p.starts_with("anthropic") {
            Some(Vendor::Anthropic)
        } else if p.starts_with("openai") {
            Some(Vendor::Openai)
        } else {
            None
        }
    }
}

/// USD per million tokens.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub vendor: Vendor,
    pub input: f64,
    pub cached: f64,
    /// Absent: cache writes bill as plain input.
    pub cache_write: Option<f64>,
    pub output: f64,
}

/// Tokens one call (or a sum of calls) billed. `input` excludes cache reads and writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl AddAssign for Usage {
    fn add_assign(&mut self, o: Usage) {
        self.input += o.input;
        self.cached += o.cached;
        self.cache_write += o.cache_write;
        self.output += o.output;
    }
}

impl Price {
    /// API-equivalent USD, also for subscription backends (V47).
    pub fn cost(&self, u: &Usage) -> f64 {
        let write = self.cache_write.unwrap_or(self.input);
        (u.input as f64 * self.input
            + u.cached as f64 * self.cached
            + u.cache_write as f64 * write
            + u.output as f64 * self.output)
            * PER_TOKEN
    }

    fn rates(&self) -> [(&'static str, Option<f64>); 4] {
        [
            ("input", Some(self.input)),
            ("cached", Some(self.cached)),
            ("cache_write", self.cache_write),
            ("output", Some(self.output)),
        ]
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: Option<u32>,
    #[serde(default)]
    defaults: BTreeMap<Vendor, String>,
    #[serde(default)]
    models: BTreeMap<String, Price>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    /// Mid-tier model per vendor (V46).
    pub defaults: BTreeMap<Vendor, String>,
    pub models: BTreeMap<String, Price>,
}

/// A model the table has no price for: the Watcher refuses to start (V47).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unpriced(pub String);

impl fmt::Display for Unpriced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "model `{}` has no price in the model table (add it to ~/.pair-watcher/models.toml)", self.0)
    }
}

impl Table {
    pub fn embedded() -> Table {
        let mut t = Table { defaults: BTreeMap::new(), models: BTreeMap::new() };
        t.merge(parse_file(EMBEDDED).expect("embedded models.toml parses"));
        t
    }

    /// Embedded table with the user's `models.toml` merged over it, schema-checked.
    pub fn load(home: &Home) -> Result<Table, String> {
        let path = home.models_file();
        let mut t = Table::embedded();
        match fs::read_to_string(&path) {
            Ok(text) => t.merge(parse_file(&text).map_err(|e| format!("{}{e}", tilde(&path)))?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", tilde(&path))),
        }
        t.check().map_err(|e| format!("{}: {e}", tilde(&path)))?;
        Ok(t)
    }

    /// A user entry replaces the whole embedded entry of that name.
    fn merge(&mut self, f: File) {
        self.defaults.extend(f.defaults);
        self.models.extend(f.models);
    }

    /// No zero/negative/non-finite price; every default priced, under its own vendor (V62).
    pub fn check(&self) -> Result<(), String> {
        for (name, p) in &self.models {
            for (field, rate) in p.rates() {
                if let Some(r) = rate
                    && !(r.is_finite() && r > 0.0)
                {
                    return Err(format!("model `{name}`: {field} price must be above zero"));
                }
            }
        }
        for (vendor, name) in &self.defaults {
            match self.models.get(name) {
                Some(p) if p.vendor == *vendor => {}
                Some(_) => return Err(format!("default for {} `{name}` is another vendor's model", vendor.name())),
                None => return Err(format!("default for {} `{name}` has no price", vendor.name())),
            }
        }
        Ok(())
    }

    pub fn price(&self, model: &str) -> Result<&Price, Unpriced> {
        self.models.get(model).ok_or_else(|| Unpriced(model.to_owned()))
    }

    /// Human-readable changes from `self` to `new`, one line each.
    pub fn diff(&self, new: &Table) -> Vec<String> {
        let mut out = Vec::new();
        for (vendor, name) in &new.defaults {
            match self.defaults.get(vendor) {
                Some(old) if old == name => {}
                Some(old) => out.push(format!("~ default {}: {old} → {name}", vendor.name())),
                None => out.push(format!("+ default {}: {name}", vendor.name())),
            }
        }
        for (name, p) in &new.models {
            let Some(old) = self.models.get(name) else {
                out.push(format!("+ {name} ({}) {}", p.vendor.name(), rates_text(p)));
                continue;
            };
            for ((field, a), (_, b)) in old.rates().into_iter().zip(p.rates()) {
                if a != b {
                    out.push(format!("~ {name} {field}: {} → {}", rate_text(a), rate_text(b)));
                }
            }
        }
        out.extend(self.models.keys().filter(|n| !new.models.contains_key(*n)).map(|n| format!("- {n}")));
        out
    }
}

fn rates_text(p: &Price) -> String {
    p.rates().iter().filter_map(|(f, r)| r.map(|r| format!("{f} {r}"))).collect::<Vec<_>>().join(" / ")
}

fn rate_text(r: Option<f64>) -> String {
    r.map_or_else(|| "none".to_owned(), |r| r.to_string())
}

fn parse_file(text: &str) -> Result<File, String> {
    let f: File = toml::from_str(text).map_err(|e| {
        let at = e.span().map(|s| format!(":{}", line_of(text, s.start))).unwrap_or_default();
        format!("{at}: {}", e.message())
    })?;
    match f.version {
        Some(v) if v != SCHEMA => Err(format!(": schema version {v}, this build reads {SCHEMA}")),
        _ => Ok(f),
    }
}

/// The model one Watcher runs for every wake and Debate turn (V46).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub model: String,
    pub vendor: Vendor,
    /// `auto` had only the Worker's own vendor configured: pane note `same vendor as Worker`.
    pub same_vendor: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChoiceError {
    Unpriced(Unpriced),
    /// No API key for the model's vendor.
    NoKey(Vendor),
    /// `auto` with no vendor configured.
    NoBackend,
}

impl fmt::Display for ChoiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChoiceError::Unpriced(u) => u.fmt(f),
            ChoiceError::NoKey(v) => write!(f, "no {} API key (env or `pair-watcher auth`)", v.name()),
            ChoiceError::NoBackend => {
                f.write_str("no model provider configured (set an API key or run `pair-watcher auth`)")
            }
        }
    }
}

/// `setting` = config `model` (or `fallback_model`); `worker` = the Worker's vendor if known;
/// `configured` = vendors with an API key. `auto` picks the mid-tier of a configured vendor other
/// than the Worker's, else the only one configured.
pub fn choose(
    table: &Table,
    setting: &str,
    worker: Option<Vendor>,
    configured: &[Vendor],
) -> Result<Choice, ChoiceError> {
    if setting != "auto" {
        let price = table.price(setting).map_err(ChoiceError::Unpriced)?;
        if !configured.contains(&price.vendor) {
            return Err(ChoiceError::NoKey(price.vendor));
        }
        return Ok(Choice { model: setting.to_owned(), vendor: price.vendor, same_vendor: false });
    }
    let mut candidates: Vec<(Vendor, &String)> =
        table.defaults.iter().filter(|(v, _)| configured.contains(v)).map(|(v, m)| (*v, m)).collect();
    candidates.sort_by_key(|(v, _)| Some(*v) == worker);
    let (vendor, model) = candidates.first().ok_or(ChoiceError::NoBackend)?;
    Ok(Choice { model: (*model).clone(), vendor: *vendor, same_vendor: Some(*vendor) == worker })
}

/// `models update`: the fetched file must be a complete, valid table on its own.
/// Returns the new effective table and the diff from the current one.
pub fn prepare_update(home: &Home, fetched: &str) -> Result<(Table, Vec<String>), String> {
    let f = parse_file(fetched).map_err(|e| format!("fetched table{e}"))?;
    if f.version.is_none() {
        return Err("fetched table: no schema version".into());
    }
    let mut alone = Table { defaults: BTreeMap::new(), models: BTreeMap::new() };
    alone.merge(f.clone());
    alone.check().map_err(|e| format!("fetched table: {e}"))?;
    if alone.defaults.is_empty() {
        return Err("fetched table: no defaults".into());
    }
    let current = Table::load(home)?;
    let mut new = Table::embedded();
    new.merge(f);
    new.check().map_err(|e| format!("fetched table: {e}"))?;
    let diff = current.diff(&new);
    Ok((new, diff))
}

/// Writes the fetched table as the user file; the previous one (empty when absent) is kept for
/// `rollback`.
pub fn apply_update(home: &Home, fetched: &str) -> io::Result<()> {
    let path = home.models_file();
    let previous = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    home::write_atomic(&prev_file(&path), &previous)?;
    home::write_atomic(&path, fetched.as_bytes())
}

/// Restores the table before the last update. `Ok(false)` when there is nothing to roll back.
pub fn rollback(home: &Home) -> io::Result<bool> {
    let path = home.models_file();
    let prev = prev_file(&path);
    let bytes = match fs::read(&prev) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if bytes.is_empty() {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    } else {
        home::write_atomic(&path, &bytes)?;
    }
    fs::remove_file(prev)?;
    Ok(true)
}

/// HTTPS only, from the fixed project URL.
pub fn fetch() -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    agent
        .get(UPDATE_URL)
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .map_err(|e| format!("{UPDATE_URL}: {e}"))
}

fn prev_file(path: &Path) -> std::path::PathBuf {
    path.with_extension("toml.prev")
}
