//! Tree snapshots of the Worker's work tree, untracked files included, ignored ones not (V5).
//! The store is a bare repo in the mailbox with its own index; it borrows the Worker's objects
//! read-only through `alternates`, so new blobs land only in the store and the Worker repo,
//! `.git` included, is never written (C2). The Worker's own excludes (`.git/info/exclude`,
//! repo-local `core.excludesFile`) apply as they do for the Worker: an excluded file never
//! reaches a snapshot or the wake input sent to the model provider (V5). The Watcher's read tools
//! are served from here, never from the live work tree (I15).

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
    /// The Worker's git common dir (objects, `info/exclude`).
    common: PathBuf,
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
        if !git_dir.join("HEAD").is_file() {
            DirBuilder::new().recursive(true).mode(0o700).create(&git_dir)?;
            git(Command::new("git").args(["init", "--quiet", "--bare"]).arg(&git_dir))?;
        }
        let common = git(worker(&repo.root).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]))?;
        let common = PathBuf::from(common.trim());
        fs::write(git_dir.join("objects/info/alternates"), format!("{}\n", common.join("objects").display()))?;
        Ok(Store { index: mailbox.join("snapshots.index"), root: repo.root.clone(), git_dir, common })
    }

    /// Snapshots the work tree; returns the tree id. Excludes are re-read each time, so an edit
    /// mid-session applies from the next snapshot.
    pub fn take(&self) -> io::Result<String> {
        let exclude = self.git_dir.join("info/exclude");
        match fs::read(self.common.join("info/exclude")) {
            Ok(bytes) => {
                DirBuilder::new().recursive(true).mode(0o700).create(self.git_dir.join("info"))?;
                fs::write(&exclude, bytes)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let _ = fs::remove_file(&exclude);
            }
            Err(e) => return Err(e),
        }
        let mut add = self.cmd(true);
        // Repo-local setting only: the global one already applies to the store as is.
        if let Ok(file) =
            git(worker(&self.root).args(["config", "--local", "--type=path", "--get", "core.excludesFile"]))
        {
            add.arg("-c").arg(format!("core.excludesFile={}", self.root.join(file.trim()).display()));
        }
        git(add.args(["add", "--all"]))?;
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
        // Without rename detection both header paths are the file's; a path git must quote has
        // no match and stays stat-only, never costing the other files their hunks.
        let patches = split_patches(&patch);
        for file in &mut files {
            let header = format!("diff --git a/{0} b/{0}\n", file.path);
            if let Some(p) = patches.iter().find(|p| p.starts_with(&header)) {
                file.patch = (*p).to_owned();
            }
        }
        Ok(files)
    }

    /// The Worker's HEAD commit, recorded with a snapshot so `git log`/`git show` tools answer as
    /// of that wake. `None` before the first commit.
    pub fn head(&self) -> Option<String> {
        let out = git(worker(&self.root).args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])).ok()?;
        Some(out.trim().to_owned()).filter(|h| !h.is_empty())
    }

    /// A file's bytes as snapshotted in `tree`.
    pub fn read(&self, tree: &str, path: &str) -> io::Result<Vec<u8>> {
        let out = self.cmd(false).args(["cat-file", "blob", &format!("{tree}:{path}")]).output()?;
        if !out.status.success() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{path}: not in the snapshot")));
        }
        Ok(out.stdout)
    }

    /// `git grep -n` (extended regex, text files only) over `tree`, as `path:line:text` lines.
    pub fn grep(&self, tree: &str, pattern: &str, path: Option<&str>) -> io::Result<String> {
        let mut cmd = self.cmd(false);
        cmd.args(["grep", "-n", "-I", "-E", "--no-color", "-e", pattern, tree, "--"]);
        cmd.args(path);
        let out = cmd.output()?;
        match out.status.code() {
            Some(0) => {}
            Some(1) => return Ok(String::new()),
            _ => return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_owned())),
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let prefix = format!("{tree}:");
        Ok(text.lines().map(|l| format!("{}\n", l.strip_prefix(&prefix).unwrap_or(l))).collect())
    }

    /// One-line log from `head` (optionally limited to `path`), newest first.
    pub fn log(&self, head: &str, path: Option<&str>, max: usize) -> io::Result<String> {
        let mut cmd = self.cmd(false);
        cmd.args(["log", "--no-color", "--date=short", "--format=%h %ad %an: %s", &format!("-n{max}")]);
        cmd.args(["--end-of-options", head, "--"]).args(path);
        git(&mut cmd)
    }

    /// `git show` of a commit (stat + patch) or `<rev>:<path>`. A leading `HEAD` means `head`.
    pub fn show(&self, head: Option<&str>, rev: &str) -> io::Result<String> {
        let rev = match (rev.strip_prefix("HEAD"), head) {
            (Some(rest), Some(h)) => format!("{h}{rest}"),
            (Some(_), None) => return Err(io::Error::other("the Worker repo has no commits")),
            (None, _) => rev.to_owned(),
        };
        git(self.cmd(false).args([
            "show",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--stat",
            "-p",
            "--end-of-options",
            &rev,
        ]))
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

/// Read-only git in the Worker repo.
fn worker(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    for var in GIT_ENV {
        cmd.env_remove(var);
    }
    cmd.arg("-C").arg(root);
    cmd
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
