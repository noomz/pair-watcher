//! Focus files (I19) and the enabled set (V81). Repo Focuses need a hash approval (V82); this module
//! only reads `[approved_focus]` — the prompt that writes it lives with `run`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use toml::Value;

use crate::config::{Resolved, read_user_file, sha256_hex};
use crate::home::Home;
use crate::problem::{Origin, Problem, Unreadable, nearest};
use crate::repo::Repo;

/// Embedded, versioned per release.
const BUILTINS: [(&str, &str); 3] = [
    ("approach", include_str!("../focus/approach.md")),
    ("correctness", include_str!("../focus/correctness.md")),
    ("security", include_str!("../focus/security.md")),
];

pub fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|(n, _)| *n == name)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FocusSource {
    BuiltIn,
    User(PathBuf),
    Repo(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Focus {
    pub name: String,
    pub brief: String,
    /// Frontmatter `model`: user files only, used in Swarm only.
    pub model: Option<String>,
    pub source: FocusSource,
    /// Hash of the file bytes: approval key (V82) and `config hash` input (V84).
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingApproval {
    pub name: String,
    pub file: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug, Default)]
pub struct FocusSet {
    pub enabled: Vec<Focus>,
    pub pending: Vec<PendingApproval>,
    pub problems: Vec<Problem>,
}

/// Enabled = built-ins − `off` + user custom + approved repo Focuses; `only` = `--focus` names.
pub fn resolve(home: &Home, repo: Option<&Repo>, config: &Resolved, only: &[String]) -> Result<FocusSet, Unreadable> {
    let mut set = FocusSet::default();

    let mut global: BTreeMap<String, Focus> = BUILTINS
        .iter()
        .map(|(name, text)| {
            let focus = Focus {
                name: (*name).into(),
                brief: text.trim().into(),
                model: None,
                source: FocusSource::BuiltIn,
                sha256: sha256_hex(text.as_bytes()),
            };
            ((*name).into(), focus)
        })
        .collect();

    for file in focus_files(&home.focus_dir()) {
        let Some(name) = valid_name(&file, &mut set.problems) else { continue };
        let text = fs::read_to_string(&file).map_err(|_| Unreadable { file: file.clone(), line: None })?;
        let (fields, body) =
            split_frontmatter(&text).map_err(|line| Unreadable { file: file.clone(), line: Some(line) })?;
        let mut model = None;
        for (field, value) in fields {
            match field.as_str() {
                "model" => model = Some(value),
                _ => set.problems.push(unknown_field(&file, field)),
            }
        }
        if let Some(focus) = build(&file, name, body, model, FocusSource::User(file.clone()), &text, &mut set) {
            global.insert(focus.name.clone(), focus);
        }
    }

    let mut repo_focuses: BTreeMap<String, Focus> = BTreeMap::new();
    if let Some(repo) = repo {
        for file in focus_files(&repo.focus_dir()) {
            let Some(name) = valid_name(&file, &mut set.problems) else { continue };
            if global.contains_key(&name) {
                let reason = if is_builtin(&name) { "named like a built-in" } else { "named like a user Focus" };
                set.problems.push(Problem::FocusRefused { file, name, reason });
                continue;
            }
            let Ok(text) = fs::read_to_string(&file) else {
                set.problems.push(Problem::FocusSkipped { file, reason: "unreadable".into() });
                continue;
            };
            let (fields, body) = match split_frontmatter(&text) {
                Ok(parts) => parts,
                Err(line) => {
                    let reason = format!("bad frontmatter at line {line}");
                    set.problems.push(Problem::FocusSkipped { file, reason });
                    continue;
                }
            };
            for (field, _) in fields {
                set.problems.push(match field.as_str() {
                    "model" => Problem::FocusField {
                        file: file.clone(),
                        field,
                        reason: "repo Focus files may not set a model".into(),
                    },
                    _ => unknown_field(&file, field),
                });
            }
            if let Some(focus) = build(&file, name, body, None, FocusSource::Repo(file.clone()), &text, &mut set) {
                repo_focuses.insert(focus.name.clone(), focus);
            }
        }
    }

    let known: Vec<String> = global.keys().chain(repo_focuses.keys()).cloned().collect();
    let off_origin = config.entry("off").origin.clone();
    for name in config.off() {
        let removed = match &off_origin {
            // A repo Focus never mutes a global Focus (V81); built-ins were already dropped by the config filter.
            Origin::Repo(file) if global.contains_key(name) => {
                let problem = Problem::RepoOffGlobal { file: file.clone(), name: name.into() };
                set.problems.push(problem);
                continue;
            }
            Origin::Repo(_) => repo_focuses.remove(name).is_some(),
            _ => global.remove(name).is_some() | repo_focuses.remove(name).is_some(),
        };
        if !removed {
            let hint = nearest(name, known.iter().map(String::as_str)).map(str::to_owned);
            set.problems.push(Problem::UnknownFocus { origin: off_origin.clone(), name: name.into(), hint });
        }
    }

    let approvals = approvals(home, repo)?;
    for (name, focus) in repo_focuses {
        match approvals.get(&name) {
            Some((sha, true)) if *sha == focus.sha256 => {
                global.insert(name, focus);
            }
            // Refused for this exact file: stays off silently until the file changes.
            Some((sha, false)) if *sha == focus.sha256 => {}
            _ => {
                let FocusSource::Repo(file) = focus.source else { unreachable!() };
                set.pending.push(PendingApproval { name, file, sha256: focus.sha256 });
            }
        }
    }

    if !only.is_empty() {
        for name in only {
            if !global.contains_key(name) {
                set.problems.push(Problem::FocusNotEnabled { name: name.clone() });
            }
        }
        global.retain(|name, _| only.contains(name));
    }

    set.enabled = global.into_values().collect();
    Ok(set)
}

fn build(
    file: &Path,
    name: String,
    body: &str,
    model: Option<String>,
    source: FocusSource,
    text: &str,
    set: &mut FocusSet,
) -> Option<Focus> {
    let brief = body.trim();
    if brief.is_empty() {
        set.problems.push(Problem::FocusSkipped { file: file.to_owned(), reason: "empty brief".into() });
        return None;
    }
    Some(Focus { name, brief: brief.into(), model, source, sha256: sha256_hex(text.as_bytes()) })
}

fn unknown_field(file: &Path, field: String) -> Problem {
    let reason = match nearest(&field, ["model"]) {
        Some(h) => format!("unknown field; did you mean `{h}`?"),
        None => "unknown field".into(),
    };
    Problem::FocusField { file: file.to_owned(), field, reason }
}

/// `*.md` files, sorted; missing dir → none.
fn focus_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    files
}

fn valid_name(file: &Path, problems: &mut Vec<Problem>) -> Option<String> {
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let ok = stem.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && stem.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Some(stem.into())
    } else {
        let reason = "name must be lowercase letters, digits, - or _".into();
        problems.push(Problem::FocusSkipped { file: file.to_owned(), reason });
        None
    }
}

/// Frontmatter fields + body.
type Parts<'a> = (Vec<(String, String)>, &'a str);

/// Optional `---` frontmatter of `key: value` lines; `Err(line)` when malformed or unterminated.
fn split_frontmatter(text: &str) -> Result<Parts<'_>, usize> {
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return Ok((Vec::new(), text));
    }
    let mut offset = text.find('\n').map_or(text.len(), |i| i + 1);
    let mut fields = Vec::new();
    for (i, line) in lines.enumerate() {
        offset += line.len();
        let trimmed = line.trim();
        if trimmed == "---" {
            return Ok((fields, &text[offset..]));
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (k, v) = trimmed.split_once(':').ok_or(i + 2)?;
        let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
        fields.push((k.trim().to_owned(), v.to_owned()));
    }
    Err(1)
}

/// `[approved_focus]` in the user per-repo file: `name = { sha256 = "…", approved = bool }`.
fn approvals(home: &Home, repo: Option<&Repo>) -> Result<BTreeMap<String, (String, bool)>, Unreadable> {
    let Some(rc) = repo.and_then(|r| r.root_commit.as_deref()) else { return Ok(BTreeMap::new()) };
    let Some(table) = read_user_file(&home.repo_config(rc))? else { return Ok(BTreeMap::new()) };
    let Some(Value::Table(entries)) = table.get("approved_focus") else { return Ok(BTreeMap::new()) };
    Ok(entries
        .iter()
        .filter_map(|(name, v)| {
            let sha = v.get("sha256")?.as_str()?;
            let approved = v.get("approved")?.as_bool()?;
            Some((name.clone(), (sha.to_owned(), approved)))
        })
        .collect())
}
