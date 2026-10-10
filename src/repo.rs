//! The Worker's repo: top-level dir and root-commit key. Read-only git calls (C2).

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub root: PathBuf,
    /// Oldest parentless commit; `None` before the first commit.
    pub root_commit: Option<String>,
}

impl Repo {
    pub fn config_file(&self) -> PathBuf {
        self.root.join(".pair-watcher").join("config.toml")
    }

    pub fn focus_dir(&self) -> PathBuf {
        self.root.join(".pair-watcher").join("focus")
    }
}

/// `None` outside a git work tree: no repo layers apply.
pub fn discover(dir: &Path) -> Option<Repo> {
    let root = git(dir, &["rev-parse", "--show-toplevel"])?;
    let root_commit =
        git(dir, &["rev-list", "--max-parents=0", "HEAD"]).and_then(|out| out.lines().last().map(str::to_owned));
    Some(Repo { root: PathBuf::from(root), root_commit })
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}
