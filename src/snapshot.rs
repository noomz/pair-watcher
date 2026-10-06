//! Tree snapshots of the Worker's work tree, untracked files included, ignored ones not (V5).
//! The store is a bare repo in the mailbox with its own index; it borrows the Worker's objects
//! read-only through `alternates`, so new blobs land only in the store and the Worker repo,
//! `.git` included, is never written (C2).

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::repo::Repo;

/// Env that would point git at another repo or index than the ones passed explicitly.
const GIT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
];

#[derive(Clone, Debug)]
pub struct Store {
    git_dir: PathBuf,
    index: PathBuf,
    root: PathBuf,
}

/// One changed file between two trees.
#[derive(Clone, Debug, PartialEq)]
pub struct FileDiff {
    pub path: String,
    /// Lines added / removed; `None` for binary files.
    pub added: Option<u64>,
    pub removed: Option<u64>,
    /// Unified patch from its `diff --git` header (empty from `stat`).
    pub patch: String,
}

impl Store {
    /// Opens `<mailbox>/snapshots.git`, creating it on first use.
    pub fn open(mailbox: &Path, repo: &Repo) -> io::Result<Store> {
        let git_dir = mailbox.join("snapshots.git");
        let store = Store { index: mailbox.join("snapshots.index"), root: repo.root.clone(), git_dir };
        if !store.git_dir.join("HEAD").is_file() {
            DirBuilder::new().recursive(true).mode(0o700).create(&store.git_dir)?;
            git(Command::new("git").args(["init", "--quiet", "--bare"]).arg(&store.git_dir))?;
        }
        let objects = git(Command::new("git").arg("-C").arg(&repo.root).args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]))?;
        let alternates = store.git_dir.join("objects/info/alternates");
        fs::write(&alternates, format!("{}\n", Path::new(objects.trim()).join("objects").display()))?;
        Ok(store)
    }

    /// Snapshots the work tree; returns the tree id.
    pub fn take(&self) -> io::Result<String> {
        git(self.cmd(true).args(["add", "--all"]))?;
        Ok(git(self.cmd(true).arg("write-tree"))?.trim().to_owned())
    }

    /// Changed files with line counts, no patches (session `--stat`).
    pub fn stat(&self, from: &str, to: &str) -> io::Result<Vec<FileDiff>> {
        let out = git(self.cmd(false).args(["diff-tree", "-r", "-z", "--numstat", from, to]))?;
        let mut files = Vec::new();
        for rec in out.split('\0').filter(|r| !r.is_empty()) {
            let mut f = rec.splitn(3, '\t');
            let (Some(a), Some(r), Some(path)) = (f.next(), f.next(), f.next()) else { continue };
            files.push(FileDiff {
                path: path.to_owned(),
                added: a.parse().ok(),
                removed: r.parse().ok(),
                patch: String::new(),
            });
        }
        Ok(files)
    }

    /// Changed files with their patches.
    pub fn diff(&self, from: &str, to: &str) -> io::Result<Vec<FileDiff>> {
        let mut files = self.stat(from, to)?;
        let patch = git(self.cmd(false).args([
            "diff-tree",
            "-r",
            "-p",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            from,
            to,
        ]))?;
        let patches = split_patches(&patch);
        if patches.len() == files.len() {
            for (file, patch) in files.iter_mut().zip(patches) {
                file.patch = patch.to_owned();
            }
        }
        Ok(files)
    }

    fn cmd(&self, work_tree: bool) -> Command {
        let mut cmd = Command::new("git");
        for var in GIT_ENV {
            cmd.env_remove(var);
        }
        cmd.arg("--git-dir").arg(&self.git_dir);
        if work_tree {
            cmd.arg("--work-tree").arg(&self.root).env("GIT_INDEX_FILE", &self.index);
        }
        cmd.args(["-c", "core.quotePath=false", "-c", "core.fsmonitor=false"]);
        cmd
    }
}

/// Patch text split at each `diff --git` header, in git's file order.
fn split_patches(patch: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = patch
        .match_indices("diff --git ")
        .map(|(i, _)| i)
        .filter(|&i| i == 0 || patch.as_bytes()[i - 1] == b'\n')
        .collect();
    starts.push(patch.len());
    starts.windows(2).map(|w| &patch[w[0]..w[1]]).collect()
}

fn git(cmd: &mut Command) -> io::Result<String> {
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!("git failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
