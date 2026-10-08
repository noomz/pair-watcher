//! `install` / `uninstall` (V59): opt-in wiring of a harness's user config (C3), so a Worker
//! started without `run` reports to pair-watcher once its session is `attach`ed (V58). Every entry
//! we write is tagged by its command or path; user entries are never touched, except that
//! `uninstall` moves Codex trust of user hooks whose place shifted.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::harness::Harness;
use crate::home::{Home, write_atomic};
use crate::jsonl::now_ms;
use crate::launch::{self, CLAUDE_EVENTS, CODEX_EVENTS, ListedHook};
use crate::problem::tilde;
use crate::{adapters, hook};

/// Claude's inline plugin dirs, `:`-separated.
pub const PLUGIN_DIRS_ENV: &str = "CLAUDE_CODE_PLUGIN_DIRS";
const MANIFEST: &str = "install.json";

/// Where each harness keeps its user config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub claude_settings: PathBuf,
    pub codex_hooks: PathBuf,
    pub codex_config: PathBuf,
    pub pi_extension: PathBuf,
}

impl Paths {
    /// `$CLAUDE_CONFIG_DIR` (default `~/.claude`), `$CODEX_HOME` (`~/.codex`),
    /// `$PI_CODING_AGENT_DIR` (`~/.pi/agent`).
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> io::Result<Paths> {
        let home = env("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        let dir = |var: &str, default: &str| {
            env(var).filter(|v| !v.is_empty()).map_or_else(|| home.join(default), PathBuf::from)
        };
        let codex = dir("CODEX_HOME", ".codex");
        Ok(Paths {
            claude_settings: dir("CLAUDE_CONFIG_DIR", ".claude").join("settings.json"),
            codex_hooks: codex.join("hooks.json"),
            codex_config: codex.join("config.toml"),
            pi_extension: dir("PI_CODING_AGENT_DIR", ".pi/agent").join("extensions").join("pair-watcher.ts"),
        })
    }
}

/// The tag (V59): a command or path under `~/.pair-watcher/`, or naming a `pair-watcher` file.
pub fn is_ours(home: &Home, text: &str) -> bool {
    let root = format!("{}/", home.root().to_string_lossy());
    text.contains(&root)
        || text
            .split(|c: char| c.is_whitespace() || c == '\'' || c == '"')
            .filter_map(|w| Path::new(w).file_name()?.to_str())
            .any(|name| name.starts_with("pair-watcher"))
}

/// A hook's place in a hooks document: (group, hook) under an event.
pub type Place = (usize, usize);

/// What stripping our hooks changed: our places, and user hooks that shifted to a new place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stripped {
    pub removed: Vec<(String, Place)>,
    pub moved: Vec<(String, Place, Place)>,
}

/// Removes our hooks from `doc.hooks` (Claude settings and Codex `hooks.json` share the shape);
/// a group, event or `hooks` left empty by that goes too.
pub fn strip_hooks(doc: &mut Value, ours: &dyn Fn(&str) -> bool) -> Stripped {
    let mut out = Stripped::default();
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else { return out };
    let mut emptied = Vec::new();
    for (event, groups) in hooks.iter_mut() {
        let Some(groups) = groups.as_array_mut() else { continue };
        let before = out.removed.len();
        let mut kept_groups = Vec::new();
        for (g, mut group) in std::mem::take(groups).into_iter().enumerate() {
            let mut dropped = false;
            if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let mut kept = Vec::new();
                for (h, hook) in std::mem::take(list).into_iter().enumerate() {
                    if ours(hook.get("command").and_then(Value::as_str).unwrap_or_default()) {
                        out.removed.push((event.clone(), (g, h)));
                        continue;
                    }
                    let to = (kept_groups.len(), kept.len());
                    if to != (g, h) {
                        out.moved.push((event.clone(), (g, h), to));
                    }
                    kept.push(hook);
                }
                dropped = kept.is_empty() && out.removed.last().is_some_and(|(e, (rg, _))| e == event && *rg == g);
                *list = kept;
            }
            if !dropped {
                kept_groups.push(group);
            }
        }
        *groups = kept_groups;
        if out.removed.len() > before && groups.is_empty() {
            emptied.push(event.clone());
        }
    }
    for event in &emptied {
        hooks.remove(event);
    }
    if !out.removed.is_empty() && hooks.is_empty() {
        doc.as_object_mut().expect("has hooks").remove("hooks");
    }
    out
}

/// Appends one group per event.
pub fn add_hooks(doc: &mut Value, groups: Map<String, Value>) -> io::Result<()> {
    let hooks = object(doc)?.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks.as_object_mut().ok_or_else(|| invalid("`hooks` is not an object"))?;
    for (event, new) in groups {
        let list = hooks.entry(event.clone()).or_insert_with(|| Value::Array(Vec::new()));
        let list = list.as_array_mut().ok_or_else(|| invalid(&format!("`hooks.{event}` is not an array")))?;
        list.extend(new.as_array().into_iter().flatten().cloned());
    }
    Ok(())
}

/// Removes our dirs from Claude's `env.CLAUDE_CODE_PLUGIN_DIRS`; an emptied var or `env` goes.
pub fn strip_plugin_dir(doc: &mut Value, ours: &dyn Fn(&str) -> bool) -> bool {
    let Some(env) = doc.get_mut("env").and_then(Value::as_object_mut) else { return false };
    let Some(dirs) = env.get(PLUGIN_DIRS_ENV).and_then(Value::as_str) else { return false };
    let all: Vec<&str> = dirs.split(':').collect();
    let kept: Vec<&str> = all.iter().copied().filter(|d| !ours(d)).collect();
    if kept.len() == all.len() {
        return false;
    }
    let kept: Vec<&str> = kept.into_iter().filter(|d| !d.is_empty()).collect();
    if kept.is_empty() {
        env.remove(PLUGIN_DIRS_ENV);
    } else {
        env.insert(PLUGIN_DIRS_ENV.into(), kept.join(":").into());
    }
    if env.is_empty() {
        doc.as_object_mut().expect("has env").remove("env");
    }
    true
}

pub fn add_plugin_dir(doc: &mut Value, dir: &Path) -> io::Result<()> {
    let env = object(doc)?.entry("env").or_insert_with(|| Value::Object(Map::new()));
    let env = env.as_object_mut().ok_or_else(|| invalid("`env` is not an object"))?;
    let dir = dir.to_string_lossy();
    let joined = match env.get(PLUGIN_DIRS_ENV).and_then(Value::as_str).filter(|d| !d.is_empty()) {
        Some(dirs) => format!("{dirs}:{dir}"),
        None => dir.into_owned(),
    };
    env.insert(PLUGIN_DIRS_ENV.into(), joined.into());
    Ok(())
}

/// Codex trust key: `<hooks file>:<event_snake>:<group>:<hook>`.
pub fn trust_key(prefix: &str, event: &str, (g, h): Place) -> String {
    let mut snake = String::new();
    for (i, c) in event.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            snake.push('_');
        }
        snake.push(c.to_ascii_lowercase());
    }
    format!("{prefix}:{snake}:{g}:{h}")
}

/// `[hooks.state]` of a Codex `config.toml`: key → `trusted_hash`.
pub fn trust_of(doc: &DocumentMut) -> BTreeMap<String, String> {
    let Some(state) = doc.get("hooks").and_then(|h| h.get("state")).and_then(Item::as_table_like) else {
        return BTreeMap::new();
    };
    state.iter().filter_map(|(k, v)| Some((k.to_owned(), v.get("trusted_hash")?.as_str()?.to_owned()))).collect()
}

fn state_table(doc: &mut DocumentMut) -> io::Result<&mut Table> {
    let hooks = doc.entry("hooks").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    let hooks = hooks.as_table_mut().ok_or_else(|| invalid("`hooks` is not a table"))?;
    let state = hooks.entry("state").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    state.as_table_mut().ok_or_else(|| invalid("`hooks.state` is not a table"))
}

/// Persists `trusted_hash` for ours (V59).
pub fn set_trust(doc: &mut DocumentMut, trust: &BTreeMap<String, String>) -> io::Result<()> {
    let state = state_table(doc)?;
    for (key, hash) in trust {
        let mut entry = Table::new();
        entry.insert("trusted_hash", value(hash.as_str()));
        state.insert(key, Item::Table(entry));
    }
    Ok(())
}

/// Drops our keys, then moves each shifted user hook's unchanged entry to its new key. Moves only
/// go to lower places and run in order, so a target is free or already moved on.
pub fn rekey_trust(doc: &mut DocumentMut, prefix: &str, ours: &[String], stripped: &Stripped) -> io::Result<()> {
    if doc.get("hooks").and_then(|h| h.get("state")).is_none() {
        return Ok(());
    }
    let state = state_table(doc)?;
    let removed = stripped.removed.iter().map(|(e, p)| trust_key(prefix, e, *p));
    for key in ours.iter().cloned().chain(removed) {
        state.remove(&key);
    }
    for (event, from, to) in &stripped.moved {
        if let Some(entry) = state.remove(&trust_key(prefix, event, *from)) {
            state.insert(&trust_key(prefix, event, *to), entry);
        }
    }
    Ok(())
}

/// The trust key prefix Codex uses for `hooks.json`: from a key it reported, else the path.
fn prefix_of(keys: &BTreeMap<String, String>, hooks_json: &Path) -> String {
    let reported = keys.keys().next().and_then(|k| {
        let mut parts = k.rsplitn(4, ':');
        let (_, _, _) = (parts.next()?, parts.next()?, parts.next()?);
        parts.next().map(str::to_owned)
    });
    reported.unwrap_or_else(|| hooks_json.to_string_lossy().into_owned())
}

/// `install.json`: what `install` wrote, per harness.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Manifest {
    #[serde(default)]
    pub installed: BTreeMap<String, Installed>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Installed {
    pub version: String,
    pub at: u64,
    /// Files written.
    pub files: Vec<PathBuf>,
    /// Claude: the mod dir in `CLAUDE_CODE_PLUGIN_DIRS`.
    #[serde(default)]
    pub plugin_dir: Option<PathBuf>,
    /// Codex: our trust keys and hashes, as Codex reported them.
    #[serde(default)]
    pub trust: BTreeMap<String, String>,
}

pub fn manifest(home: &Home) -> io::Result<Manifest> {
    match fs::read(home.root().join(MANIFEST)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| invalid(&format!("{MANIFEST}: {e}"))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Manifest::default()),
        Err(e) => Err(e),
    }
}

fn save(home: &Home, m: &Manifest) -> io::Result<()> {
    write_atomic(&home.root().join(MANIFEST), &serde_json::to_vec_pretty(m)?)
}

/// `install` recorded this harness: `run` adds no per-session hooks then, or they would fire twice.
pub fn installed(home: &Home, harness: Harness) -> Option<Installed> {
    manifest(home).ok()?.installed.remove(harness.name())
}

/// Files touched by one harness's install or uninstall, restored together on failure.
#[derive(Default)]
struct Txn {
    saved: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl Txn {
    fn read(&mut self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        let bytes = match fs::read(path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if !self.saved.iter().any(|(p, _)| p == path) {
            self.saved.push((path.to_path_buf(), bytes.clone()));
        }
        Ok(bytes)
    }

    fn write(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.read(path)?;
        write_user_file(path, bytes)
    }

    fn remove(&mut self, path: &Path) -> io::Result<()> {
        self.read(path)?;
        match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn rollback(self) {
        for (path, bytes) in self.saved.into_iter().rev() {
            let _ = match bytes {
                Some(b) => write_user_file(&path, &b),
                None => fs::remove_file(&path),
            };
        }
    }
}

/// Atomic rename beside the real file (a symlinked dotfile stays a symlink), keeping its mode.
fn write_user_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mode = fs::metadata(&path).map_or(0o600, |m| m.permissions().mode() & 0o7777);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.pair-watcher.tmp"));
    fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
    fs::write(&tmp, bytes)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, &path)
}

fn read_json(txn: &mut Txn, path: &Path) -> io::Result<Value> {
    match txn.read(path)? {
        Some(b) if !b.iter().all(u8::is_ascii_whitespace) => {
            serde_json::from_slice(&b).map_err(|e| invalid(&format!("{}: {e}", tilde(path)))).and_then(|v: Value| {
                if v.is_object() { Ok(v) } else { Err(invalid(&format!("{}: not an object", tilde(path)))) }
            })
        }
        _ => Ok(Value::Object(Map::new())),
    }
}

fn read_toml(txn: &mut Txn, path: &Path) -> io::Result<DocumentMut> {
    let text = txn.read(path)?.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    text.parse().map_err(|e: toml_edit::TomlError| invalid(&format!("{}: {e}", tilde(path))))
}

fn json_bytes(v: &Value) -> io::Result<Vec<u8>> {
    let mut b = serde_json::to_vec_pretty(v)?;
    b.push(b'\n');
    Ok(b)
}

/// What install needs beyond files: the shim target and Codex's `hooks/list`.
pub struct Ctx<'a> {
    pub home: &'a Home,
    pub paths: &'a Paths,
    pub exe: &'a Path,
    /// `codex app-server` `hooks/list` over the user config.
    pub codex_list: &'a dyn Fn() -> io::Result<Vec<ListedHook>>,
}

/// `install <harness>`: idempotent (ours replaced, never doubled); every file of the harness is
/// written or none. Returns one line per file written.
pub fn install(cx: &Ctx, harness: Harness) -> Result<Vec<String>, String> {
    let shim = hook::write_shim(cx.home, cx.exe).map_err(|e| e.to_string())?;
    let adapters = adapters::write(cx.home).map_err(|e| e.to_string())?;
    let mut m = manifest(cx.home).map_err(|e| e.to_string())?;
    let previous = m.installed.get(harness.name()).cloned();
    let mut txn = Txn::default();
    let result = wire(cx, harness, &shim, &adapters, previous.as_ref(), &mut txn);
    let entry = match result {
        Ok(entry) => entry,
        Err(e) => {
            txn.rollback();
            return Err(e.to_string());
        }
    };
    let lines = entry.files.iter().map(|f| format!("{}: wrote {}", harness.name(), tilde(f))).collect();
    m.installed.insert(harness.name().into(), entry);
    if let Err(e) = save(cx.home, &m) {
        txn.rollback();
        return Err(format!("{}: {e}", tilde(&cx.home.root().join(MANIFEST))));
    }
    Ok(lines)
}

fn wire(
    cx: &Ctx,
    harness: Harness,
    shim: &Path,
    adapters: &Path,
    previous: Option<&Installed>,
    txn: &mut Txn,
) -> io::Result<Installed> {
    let ours = |s: &str| is_ours(cx.home, s);
    let mut entry = Installed {
        version: env!("CARGO_PKG_VERSION").into(),
        at: now_ms(),
        files: vec![],
        plugin_dir: None,
        trust: BTreeMap::new(),
    };
    match harness {
        Harness::Claude => {
            let path = &cx.paths.claude_settings;
            let mut doc = read_json(txn, path)?;
            strip_hooks(&mut doc, &ours);
            strip_plugin_dir(&mut doc, &ours);
            add_hooks(&mut doc, launch::hook_groups(shim, &CLAUDE_EVENTS))?;
            let dir = adapters.join(adapters::CLAUDE_MOD);
            add_plugin_dir(&mut doc, &dir)?;
            txn.write(path, &json_bytes(&doc)?)?;
            let check = read_json(&mut Txn::default(), path)?;
            if count_ours(&check, &ours) != CLAUDE_EVENTS.len() {
                return Err(invalid(&format!("{}: our hooks did not stick", tilde(path))));
            }
            entry.files.push(path.clone());
            entry.plugin_dir = Some(dir);
        }
        Harness::Codex => {
            let (hooks_path, config_path) = (&cx.paths.codex_hooks, &cx.paths.codex_config);
            let old_trust = previous.map(|p| p.trust.clone()).unwrap_or_default();
            let prefix = prefix_of(&old_trust, hooks_path);
            let mut doc = read_json(txn, hooks_path)?;
            let stripped = strip_hooks(&mut doc, &ours);
            add_hooks(&mut doc, launch::hook_groups(shim, &CODEX_EVENTS))?;
            txn.write(hooks_path, &json_bytes(&doc)?)?;
            let listed = (cx.codex_list)().map_err(|e| io::Error::other(format!("codex hooks/list: {e}")))?;
            let hooks_file = hooks_path.to_string_lossy();
            let canonical = fs::canonicalize(hooks_path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let trust: BTreeMap<String, String> = listed
                .iter()
                .filter(|h| ours(&h.command))
                .filter(|h| h.key.starts_with(&format!("{hooks_file}:")) || h.key.starts_with(&format!("{canonical}:")))
                .map(|h| (h.key.clone(), h.current_hash.clone()))
                .collect();
            if trust.len() != CODEX_EVENTS.len() {
                return Err(invalid(&format!(
                    "codex listed {} of our {} hooks in {}",
                    trust.len(),
                    CODEX_EVENTS.len(),
                    tilde(hooks_path)
                )));
            }
            let mut config = read_toml(txn, config_path)?;
            let old: Vec<String> = old_trust.keys().cloned().collect();
            rekey_trust(&mut config, &prefix, &old, &stripped)?;
            set_trust(&mut config, &trust)?;
            txn.write(config_path, config.to_string().as_bytes())?;
            entry.files.extend([hooks_path.clone(), config_path.clone()]);
            entry.trust = trust;
        }
        Harness::Pi => {
            let path = &cx.paths.pi_extension;
            txn.write(path, adapters::PI_SOURCE.as_bytes())?;
            entry.files.push(path.clone());
        }
    }
    Ok(entry)
}

fn count_ours(doc: &Value, ours: &dyn Fn(&str) -> bool) -> usize {
    let groups = doc.get("hooks").and_then(Value::as_object).into_iter().flat_map(|h| h.values());
    groups
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|g| g.get("hooks")?.as_array())
        .flatten()
        .filter(|h| ours(h.get("command").and_then(Value::as_str).unwrap_or_default()))
        .count()
}

/// `uninstall <harness>`: removes only ours, keeps every user entry, re-keys shifted user Codex
/// hooks, then checks nothing of ours remains. Returns one line per file changed.
pub fn uninstall(home: &Home, paths: &Paths, harness: Harness) -> Result<Vec<String>, String> {
    let mut m = manifest(home).map_err(|e| e.to_string())?;
    let previous = m.installed.get(harness.name()).cloned();
    let mut txn = Txn::default();
    match unwire(home, paths, harness, previous.as_ref(), &mut txn).and_then(|lines| {
        verify_gone(home, paths, harness, previous.as_ref())?;
        Ok(lines)
    }) {
        Ok(lines) => {
            m.installed.remove(harness.name());
            if let Err(e) = save(home, &m) {
                txn.rollback();
                return Err(e.to_string());
            }
            Ok(lines)
        }
        Err(e) => {
            txn.rollback();
            Err(e.to_string())
        }
    }
}

fn unwire(
    home: &Home,
    paths: &Paths,
    harness: Harness,
    previous: Option<&Installed>,
    txn: &mut Txn,
) -> io::Result<Vec<String>> {
    let ours = |s: &str| is_ours(home, s);
    let mut lines = Vec::new();
    let mut changed = |path: &Path| lines.push(format!("{}: cleaned {}", harness.name(), tilde(path)));
    match harness {
        Harness::Claude => {
            let path = &paths.claude_settings;
            if txn.read(path)?.is_some() {
                let mut doc = read_json(txn, path)?;
                let stripped = strip_hooks(&mut doc, &ours);
                if strip_plugin_dir(&mut doc, &ours) | !stripped.removed.is_empty() {
                    txn.write(path, &json_bytes(&doc)?)?;
                    changed(path);
                }
            }
        }
        Harness::Codex => {
            let trust = previous.map(|p| p.trust.clone()).unwrap_or_default();
            let prefix = prefix_of(&trust, &paths.codex_hooks);
            let mut stripped = Stripped::default();
            if txn.read(&paths.codex_hooks)?.is_some() {
                let mut doc = read_json(txn, &paths.codex_hooks)?;
                stripped = strip_hooks(&mut doc, &ours);
                if !stripped.removed.is_empty() {
                    txn.write(&paths.codex_hooks, &json_bytes(&doc)?)?;
                    changed(&paths.codex_hooks);
                }
            }
            if txn.read(&paths.codex_config)?.is_some() {
                let mut config = read_toml(txn, &paths.codex_config)?;
                let before = config.to_string();
                let keys: Vec<String> = trust.keys().cloned().collect();
                rekey_trust(&mut config, &prefix, &keys, &stripped)?;
                if config.to_string() != before {
                    txn.write(&paths.codex_config, config.to_string().as_bytes())?;
                    changed(&paths.codex_config);
                }
            }
        }
        Harness::Pi => {
            if txn.read(&paths.pi_extension)?.is_some() {
                txn.remove(&paths.pi_extension)?;
                lines.push(format!("pi: removed {}", tilde(&paths.pi_extension)));
            }
        }
    }
    Ok(lines)
}

fn verify_gone(home: &Home, paths: &Paths, harness: Harness, previous: Option<&Installed>) -> io::Result<()> {
    let ours = |s: &str| is_ours(home, s);
    let left = |what: String| Err(invalid(&format!("uninstall {}: {what} remains", harness.name())));
    let mut txn = Txn::default();
    match harness {
        Harness::Claude => {
            let doc = read_json(&mut txn, &paths.claude_settings)?;
            let dirs = doc.pointer(&format!("/env/{PLUGIN_DIRS_ENV}")).and_then(Value::as_str).unwrap_or_default();
            if count_ours(&doc, &ours) > 0 || dirs.split(':').any(ours) {
                return left(format!("an entry of ours in {}", tilde(&paths.claude_settings)));
            }
        }
        Harness::Codex => {
            if count_ours(&read_json(&mut txn, &paths.codex_hooks)?, &ours) > 0 {
                return left(format!("a hook of ours in {}", tilde(&paths.codex_hooks)));
            }
            let trust = trust_of(&read_toml(&mut txn, &paths.codex_config)?);
            let ours_left = previous.into_iter().flat_map(|p| &p.trust).any(|(k, h)| trust.get(k) == Some(h));
            if ours_left {
                return left(format!("our hook trust in {}", tilde(&paths.codex_config)));
            }
        }
        Harness::Pi => {
            if paths.pi_extension.exists() {
                return left(tilde(&paths.pi_extension));
            }
        }
    }
    Ok(())
}

fn object(doc: &mut Value) -> io::Result<&mut Map<String, Value>> {
    doc.as_object_mut().ok_or_else(|| invalid("not a JSON object"))
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())
}
