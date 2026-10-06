//! Non-fatal config/Focus problems and the one fatal outcome (V83).

use std::fmt;
use std::path::{Path, PathBuf};

use toml::Value;

/// Where a resolved value came from, highest precedence last (V79).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Default,
    Global(PathBuf),
    Repo(PathBuf),
    UserRepo(PathBuf),
    Env(String),
    Flag,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Default => f.write_str("default"),
            Origin::Flag => f.write_str("flag"),
            Origin::Env(var) => write!(f, "env {var}"),
            Origin::Global(p) | Origin::Repo(p) | Origin::UserRepo(p) => f.write_str(&tilde(p)),
        }
    }
}

/// A config problem: shown as one ticker line, listed in full by `doctor`. Never blocks the Worker.
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    UnknownKey {
        origin: Origin,
        key: String,
        hint: Option<&'static str>,
    },
    BadValue {
        origin: Origin,
        key: &'static str,
        expected: String,
    },
    BadFlag {
        arg: String,
    },
    /// Repo file value outside the key's repo direction (V80, C9).
    RepoIgnored {
        file: PathBuf,
        key: &'static str,
        value: Value,
        rule: &'static str,
    },
    RepoUnparseable {
        file: PathBuf,
        line: Option<usize>,
    },
    UnknownFocus {
        origin: Origin,
        name: String,
        hint: Option<String>,
    },
    FocusNotEnabled {
        name: String,
    },
    /// Repo Focus that would shadow a global one (V81).
    FocusRefused {
        file: PathBuf,
        name: String,
        reason: &'static str,
    },
    /// Repo `off` naming a global Focus (V81).
    RepoOffGlobal {
        file: PathBuf,
        name: String,
    },
    FocusSkipped {
        file: PathBuf,
        reason: String,
    },
    FocusField {
        file: PathBuf,
        field: String,
        reason: String,
    },
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::UnknownKey { origin, key, hint } => {
                write!(f, "{origin}: unknown key `{key}`")?;
                if let Some(h) = hint {
                    write!(f, " (did you mean `{h}`?)")?;
                }
                Ok(())
            }
            Problem::BadValue { origin, key, expected } => {
                write!(f, "{origin}: bad value for `{key}` (expected {expected}); using default")
            }
            Problem::BadFlag { arg } => write!(f, "flag -c `{arg}`: expected key=value"),
            Problem::RepoIgnored { file, key, value, rule } => {
                write!(f, "{}: ignored `{key} = {value}` ({rule})", tilde(file))
            }
            Problem::RepoUnparseable { file, line } => {
                write!(f, "{}: unparseable, ignored", at_line(file, *line))
            }
            Problem::UnknownFocus { origin, name, hint } => {
                write!(f, "{origin}: unknown Focus `{name}`")?;
                if let Some(h) = hint {
                    write!(f, " (did you mean `{h}`?)")?;
                }
                Ok(())
            }
            Problem::FocusNotEnabled { name } => write!(f, "--focus `{name}`: Focus not enabled"),
            Problem::FocusRefused { file, name, reason } => {
                write!(f, "{}: repo Focus `{name}` refused ({reason})", tilde(file))
            }
            Problem::RepoOffGlobal { file, name } => {
                write!(f, "{}: ignored off `{name}` (repo may not turn off a global Focus)", tilde(file))
            }
            Problem::FocusSkipped { file, reason } => write!(f, "{}: Focus skipped ({reason})", tilde(file)),
            Problem::FocusField { file, field, reason } => {
                write!(f, "{}: frontmatter `{field}` ignored ({reason})", tilde(file))
            }
        }
    }
}

/// Unparseable user file: the Watcher goes BLIND rather than run on half the settings (V83).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreadable {
    pub file: PathBuf,
    pub line: Option<usize>,
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config unreadable: {}", at_line(&self.file, self.line))
    }
}

/// The single ticker line for all problems (V83).
pub fn ticker_line(problems: &[Problem]) -> Option<String> {
    match problems.len() {
        0 => None,
        1 => Some("config: 1 problem — pair-watcher doctor".into()),
        n => Some(format!("config: {n} problems — pair-watcher doctor")),
    }
}

/// Nearest known name for an unknown one, if any is close enough.
pub fn nearest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    candidates
        .into_iter()
        .map(|c| (strsim::jaro_winkler(name, c), c))
        .filter(|(score, _)| *score >= 0.8)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, c)| c)
}

/// 1-based line of a byte offset.
pub fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

pub fn tilde(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

fn at_line(file: &Path, line: Option<usize>) -> String {
    match line {
        Some(l) => format!("{}:{l}", tilde(file)),
        None => tilde(file),
    }
}
