//! Layered config: precedence (V79), repo direction filter (V80, C9), forgiving validation (V83),
//! canonical form + `config hash` (V84). Keys and defaults per I18.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};
use toml::{Table, Value};

use crate::focus::{self, Focus};
use crate::home::Home;
use crate::problem::{Origin, Problem, Unreadable, line_of, nearest};
use crate::repo::Repo;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Str,
    Bool,
    /// Non-negative number, stored as float.
    Num,
    /// Non-negative integer.
    Int,
    Choice(&'static [&'static str]),
    List,
}

/// The one value a repo file may move each key toward (V80).
#[derive(Clone, Copy, Debug)]
enum RepoRule {
    Free,
    Ignored,
    LowerOnly,
    TrueOnly,
    FalseOnly,
    TowardNote,
    OffOnly,
    NotBuiltins,
}

impl RepoRule {
    fn text(self) -> &'static str {
        match self {
            RepoRule::Free => "",
            RepoRule::Ignored => "repo may not set it",
            RepoRule::LowerOnly => "repo may only lower it",
            RepoRule::TrueOnly => "repo may only set true",
            RepoRule::FalseOnly => "repo may only set false",
            RepoRule::TowardNote => "repo may only move it toward note",
            RepoRule::OffOnly => "repo may only set off",
            RepoRule::NotBuiltins => "repo may not turn off built-in Focuses",
        }
    }

    fn allows(self, new: &Value, current: Option<&Value>) -> bool {
        match self {
            RepoRule::Free => true,
            RepoRule::Ignored | RepoRule::NotBuiltins => false,
            RepoRule::LowerOnly => match (as_f64(new), current.and_then(as_f64)) {
                (Some(n), Some(c)) => n <= c,
                _ => false,
            },
            RepoRule::TrueOnly => new.as_bool() == Some(true),
            RepoRule::FalseOnly => new.as_bool() == Some(false),
            RepoRule::TowardNote => new.as_str() == Some("note") || Some(new) == current,
            RepoRule::OffOnly => new.as_str() == Some("off"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Default {
    Unset,
    Str(&'static str),
    Bool(bool),
    Num(f64),
    Int(i64),
    EmptyList,
}

impl Default {
    fn value(self) -> Option<Value> {
        match self {
            Default::Unset => None,
            Default::Str(s) => Some(Value::String(s.into())),
            Default::Bool(b) => Some(Value::Boolean(b)),
            Default::Num(n) => Some(Value::Float(n)),
            Default::Int(i) => Some(Value::Integer(i)),
            Default::EmptyList => Some(Value::Array(Vec::new())),
        }
    }
}

struct Key {
    name: &'static str,
    kind: Kind,
    default: Default,
    repo: RepoRule,
}

const fn key(name: &'static str, kind: Kind, default: Default, repo: RepoRule) -> Key {
    Key { name, kind, default, repo }
}

const BACKENDS: Kind = Kind::Choice(&["api", "claude-p", "codex-exec", "claude-session"]);
/// Runs only under a Claude Worker (V106).
const CLAUDE_SESSION: &str = "claude-session";

/// `[harness.<h>]` overrides (V109): unset by default, never from a repo file (V80).
const fn per_harness(name: &'static str, kind: Kind) -> Key {
    key(name, kind, Default::Unset, RepoRule::Ignored)
}

const KEYS: &[Key] = &[
    key("auto_challenge", Kind::Bool, Default::Bool(true), RepoRule::TrueOnly),
    key("backend", BACKENDS, Default::Str("api"), RepoRule::Ignored),
    key("band_digits", Kind::Bool, Default::Bool(true), RepoRule::Ignored),
    key("ceiling_usd", Kind::Num, Default::Num(5.0), RepoRule::LowerOnly),
    key("checkpoints", Kind::Bool, Default::Bool(true), RepoRule::Ignored),
    key("escalate_unresolved", Kind::Choice(&["warn", "note", "off"]), Default::Str("warn"), RepoRule::TowardNote),
    key("fallback_model", Kind::Str, Default::Unset, RepoRule::Ignored),
    per_harness("harness.claude.backend", BACKENDS),
    per_harness("harness.claude.fallback_model", Kind::Str),
    per_harness("harness.claude.model", Kind::Str),
    per_harness("harness.codex.backend", BACKENDS),
    per_harness("harness.codex.fallback_model", Kind::Str),
    per_harness("harness.codex.model", Kind::Str),
    per_harness("harness.pi.backend", BACKENDS),
    per_harness("harness.pi.fallback_model", Kind::Str),
    per_harness("harness.pi.model", Kind::Str),
    key("infer_rulings", Kind::Bool, Default::Bool(true), RepoRule::Ignored),
    key("mode", Kind::Choice(&["companion", "swarm"]), Default::Str("companion"), RepoRule::Free),
    key("model", Kind::Str, Default::Str("auto"), RepoRule::Ignored),
    key("off", Kind::List, Default::EmptyList, RepoRule::NotBuiltins),
    key("prefilter.mode", Kind::Choice(&["auto", "off", "shadow", "skip"]), Default::Str("auto"), RepoRule::OffOnly),
    key("prefilter.model", Kind::Str, Default::Str("nimble"), RepoRule::Ignored),
    key("prefilter.url", Kind::Str, Default::Str("http://localhost:11434"), RepoRule::Ignored),
    key("store.cap_gb", Kind::Num, Default::Num(2.0), RepoRule::LowerOnly),
    key("store.retention_days", Kind::Int, Default::Int(30), RepoRule::LowerOnly),
    key("telemetry", Kind::Bool, Default::Bool(false), RepoRule::FalseOnly),
];

/// Tool-written table in the user per-repo file; not a config key.
const APPROVED_FOCUS: &str = "approved_focus";

fn spec(name: &str) -> Option<&'static Key> {
    KEYS.iter().find(|k| k.name == name)
}

pub fn key_names() -> impl Iterator<Item = &'static str> {
    KEYS.iter().map(|k| k.name)
}

/// `PAIR_WATCHER_<KEY>`, dots → `__` (I18).
pub fn env_var(key: &str) -> String {
    format!("PAIR_WATCHER_{}", key.to_uppercase().replace('.', "__"))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub value: Option<Value>,
    pub origin: Origin,
}

#[derive(Clone, Debug)]
pub struct Resolved {
    entries: BTreeMap<&'static str, Entry>,
    pub problems: Vec<Problem>,
    /// Config layer files present at resolve time (logged at session start, V84).
    pub layer_files: Vec<std::path::PathBuf>,
}

impl Resolved {
    pub fn entry(&self, key: &str) -> &Entry {
        &self.entries[key]
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.get(key).and_then(|e| e.value.as_ref())
    }

    /// `key` for a Worker on `harness` (V109): within one layer `[harness.<harness>]` beats the
    /// top-level key; across layers precedence holds (V79), so a top-level env or flag beats a
    /// file's table.
    pub fn for_harness(&self, key: &str, harness: &str) -> &Entry {
        let top = self.entry(key);
        match self.entries.get(format!("harness.{harness}.{key}").as_str()) {
            Some(h) if h.value.is_some() && h.origin.rank() >= top.origin.rank() => h,
            _ => top,
        }
    }

    pub fn get_for(&self, key: &str, harness: &str) -> Option<&Value> {
        self.for_harness(key, harness).value.as_ref()
    }

    /// `backend` for a Worker on `harness`. `claude-session` under a Codex or pi Worker is a bad
    /// value (V106, V83): the key default applies and the problem is returned.
    pub fn backend_for(&self, harness: &str) -> (String, Option<Problem>) {
        let entry = self.for_harness("backend", harness);
        let value = entry.value.as_ref().and_then(Value::as_str).unwrap_or("api");
        if value != CLAUDE_SESSION || harness == "claude" {
            return (value.to_owned(), None);
        }
        let table = format!("harness.{harness}.backend");
        let key = match key_names().find(|k| *k == table) {
            Some(k) if !std::ptr::eq(entry, self.entry("backend")) => k,
            _ => "backend",
        };
        let expected = format!("api|claude-p|codex-exec for a {harness} Worker; claude-session runs only for Claude");
        let default = spec("backend").and_then(|k| k.default.value());
        let fallback = default.as_ref().and_then(Value::as_str).unwrap_or("api").to_owned();
        (fallback, Some(Problem::BadValue { origin: entry.origin.clone(), key, expected }))
    }

    pub fn entries(&self) -> impl Iterator<Item = (&'static str, &Entry)> {
        self.entries.iter().map(|(k, e)| (*k, e))
    }

    pub fn off(&self) -> Vec<&str> {
        self.get("off")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// Sorted `key = value` lines; unset keys omitted.
    pub fn canonical(&self) -> String {
        self.entries.iter().filter_map(|(k, e)| e.value.as_ref().map(|v| format!("{k} = {v}\n"))).collect()
    }
}

/// V84: resolved config (merged, filtered, canonical) + each enabled brief hash.
pub fn config_hash(config: &Resolved, enabled: &[Focus]) -> String {
    let mut text = config.canonical();
    let mut briefs: Vec<_> = enabled.iter().map(|f| (&f.name, &f.sha256)).collect();
    briefs.sort();
    for (name, sha) in briefs {
        text.push_str(&format!("focus {name} {sha}\n"));
    }
    sha256_hex(text.as_bytes())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Sources<'a> {
    pub home: &'a Home,
    pub repo: Option<&'a Repo>,
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// `key=value` from `-c` and the `--mode`/`--ceiling` shortcuts, in order.
    pub flags: &'a [String],
}

/// Merges all layers. Only an unparseable user file is fatal (BLIND); everything else is a Problem.
pub fn resolve(src: &Sources) -> Result<Resolved, Unreadable> {
    let mut r = Resolved {
        entries: KEYS.iter().map(|k| (k.name, Entry { value: k.default.value(), origin: Origin::Default })).collect(),
        problems: Vec::new(),
        layer_files: Vec::new(),
    };

    let global = src.home.config_file();
    if let Some(table) = read_user_file(&global)? {
        r.layer_files.push(global.clone());
        r.apply_table(table, &Origin::Global(global));
    }

    if let Some(repo) = src.repo {
        let file = repo.config_file();
        match read_toml(&file) {
            Ok(Some(table)) => {
                r.layer_files.push(file.clone());
                r.apply_table(table, &Origin::Repo(file));
            }
            Ok(None) => {}
            Err(line) => r.problems.push(Problem::RepoUnparseable { file, line }),
        }

        if let Some(rc) = &repo.root_commit {
            let file = src.home.repo_config(rc);
            if let Some(mut table) = read_user_file(&file)? {
                table.remove(APPROVED_FOCUS);
                r.layer_files.push(file.clone());
                r.apply_table(table, &Origin::UserRepo(file));
            }
        }
    }

    for k in KEYS {
        let var = env_var(k.name);
        if let Some(raw) = (src.env)(&var) {
            r.apply(k, parse_cli_value(&raw, k.kind), Origin::Env(var));
        }
    }

    for arg in src.flags {
        let Some((name, raw)) = arg.split_once('=') else {
            r.problems.push(Problem::BadFlag { arg: arg.clone() });
            continue;
        };
        let name = name.trim();
        match spec(name) {
            Some(k) => r.apply(k, parse_cli_value(raw.trim(), k.kind), Origin::Flag),
            None => r.unknown(Origin::Flag, name),
        }
    }

    Ok(r)
}

impl Resolved {
    fn apply_table(&mut self, table: Table, origin: &Origin) {
        let mut flat = Vec::new();
        flatten(table, "", &mut flat);
        for (name, value) in flat {
            match spec(&name) {
                Some(k) => self.apply(k, value, origin.clone()),
                None => self.unknown(origin.clone(), &name),
            }
        }
    }

    fn unknown(&mut self, origin: Origin, key: &str) {
        let hint = nearest(key, key_names());
        self.problems.push(Problem::UnknownKey { origin, key: key.into(), hint });
    }

    fn apply(&mut self, k: &'static Key, raw: Value, origin: Origin) {
        let pi = k.name == "backend" && raw.as_str().is_some_and(|v| v.trim().starts_with("pi"));
        let value = match normalize(k.kind, raw) {
            Some(v) => Some(v),
            None => {
                let mut expected = expected(k.kind);
                // C10: no pi backend; a pi subscription login as Watcher backend is refused.
                if pi {
                    expected.push_str(
                        "; pi with a Claude or Gemini subscription login is refused as a Watcher backend (terms)",
                    );
                }
                self.problems.push(Problem::BadValue { origin: origin.clone(), key: k.name, expected });
                k.default.value()
            }
        };

        if let Origin::Repo(file) = &origin {
            let Some(value) = self.repo_filter(k, value, file) else { return };
            self.entries.insert(k.name, Entry { value: Some(value), origin });
            return;
        }
        self.entries.insert(k.name, Entry { value, origin });
    }

    /// Keeps a repo value only inside its direction; a loosening is ignored + reported (V80).
    fn repo_filter(&mut self, k: &'static Key, value: Option<Value>, file: &Path) -> Option<Value> {
        let value = value?;
        if let RepoRule::NotBuiltins = k.repo {
            let names = value.as_array().cloned().unwrap_or_default();
            let (builtin, kept): (Vec<_>, Vec<_>) =
                names.into_iter().partition(|n| n.as_str().is_some_and(focus::is_builtin));
            if !builtin.is_empty() {
                self.ignore(file, k, Value::Array(builtin));
            }
            return (!kept.is_empty()).then_some(Value::Array(kept));
        }
        if k.repo.allows(&value, self.get(k.name)) {
            Some(value)
        } else {
            self.ignore(file, k, value);
            None
        }
    }

    fn ignore(&mut self, file: &Path, k: &'static Key, value: Value) {
        self.problems.push(Problem::RepoIgnored { file: file.to_owned(), key: k.name, value, rule: k.repo.text() });
    }
}

/// Dotted paths for every leaf a known key could live at; unknown tables reported whole.
fn flatten(table: Table, prefix: &str, out: &mut Vec<(String, Value)>) {
    for (k, v) in table {
        let path = format!("{prefix}{k}");
        let is_parent = KEYS.iter().any(|key| key.name.starts_with(&format!("{path}.")));
        match v {
            Value::Table(t) if spec(&path).is_none() && is_parent => flatten(t, &format!("{path}."), out),
            v => out.push((path, v)),
        }
    }
}

fn normalize(kind: Kind, v: Value) -> Option<Value> {
    match (kind, v) {
        (Kind::Str, Value::String(s)) if !s.trim().is_empty() => Some(Value::String(s)),
        (Kind::Bool, Value::Boolean(b)) => Some(Value::Boolean(b)),
        (Kind::Num, Value::Float(f)) if f.is_finite() && f >= 0.0 => Some(Value::Float(f)),
        (Kind::Num, Value::Integer(i)) if i >= 0 => Some(Value::Float(i as f64)),
        (Kind::Int, Value::Integer(i)) if i >= 0 => Some(Value::Integer(i)),
        (Kind::Choice(options), Value::String(s)) if options.contains(&s.as_str()) => Some(Value::String(s)),
        (Kind::List, Value::Array(a)) if a.iter().all(Value::is_str) => Some(Value::Array(a)),
        _ => None,
    }
}

fn expected(kind: Kind) -> String {
    match kind {
        Kind::Str => "a non-empty string".into(),
        Kind::Bool => "true or false".into(),
        Kind::Num => "a non-negative number".into(),
        Kind::Int => "a non-negative integer".into(),
        Kind::Choice(options) => options.join("|"),
        Kind::List => "a list of names".into(),
    }
}

/// Flag/env text → value: TOML literal when it parses, else the bare string; lists also take `a,b`.
fn parse_cli_value(raw: &str, kind: Kind) -> Value {
    let parsed = format!("v = {raw}").parse::<Table>().ok().and_then(|mut t| t.remove("v"));
    match (kind, parsed) {
        (Kind::Str | Kind::Choice(_), Some(Value::String(s))) => Value::String(s),
        (Kind::Str | Kind::Choice(_), _) => Value::String(raw.into()),
        (Kind::List, Some(v @ Value::Array(_))) => v,
        (Kind::List, _) => Value::Array(
            raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| Value::String(s.into())).collect(),
        ),
        (_, Some(v)) => v,
        (_, None) => Value::String(raw.into()),
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

/// Missing → `None`; unparseable → `Err(line)`.
fn read_toml(path: &Path) -> Result<Option<Table>, Option<usize>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(None),
    };
    text.parse::<Table>().map(Some).map_err(|e| e.span().map(|s| line_of(&text, s.start)))
}

pub(crate) fn read_user_file(path: &Path) -> Result<Option<Table>, Unreadable> {
    read_toml(path).map_err(|line| Unreadable { file: path.to_owned(), line })
}
