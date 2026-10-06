#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use pair_watcher::config::{self, Resolved, Sources};
use pair_watcher::focus::{self, FocusSet};
use pair_watcher::home::Home;
use pair_watcher::problem::Unreadable;
use pair_watcher::repo::Repo;
use tempfile::TempDir;

pub const ROOT_COMMIT: &str = "abc123";

/// A scratch home + repo; files are written relative to each.
pub struct World {
    _dir: TempDir,
    pub home: Home,
    pub repo: Repo,
    pub env: HashMap<String, String>,
    pub flags: Vec<String>,
}

impl World {
    pub fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let home = Home::new(dir.path().join("home/.pair-watcher"));
        home.ensure_layout().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let repo = Repo { root, root_commit: Some(ROOT_COMMIT.into()) };
        World { _dir: dir, home, repo, env: HashMap::new(), flags: Vec::new() }
    }

    pub fn global(&self, toml: &str) -> &Self {
        write(&self.home.config_file(), toml);
        self
    }

    pub fn repo_file(&self, toml: &str) -> &Self {
        write(&self.repo.config_file(), toml);
        self
    }

    pub fn user_repo(&self, toml: &str) -> &Self {
        write(&self.home.repo_config(ROOT_COMMIT), toml);
        self
    }

    pub fn user_focus(&self, name: &str, text: &str) -> &Self {
        write(&self.home.focus_dir().join(format!("{name}.md")), text);
        self
    }

    pub fn repo_focus(&self, name: &str, text: &str) -> &Self {
        write(&self.repo.focus_dir().join(format!("{name}.md")), text);
        self
    }

    pub fn try_resolve(&self) -> Result<Resolved, Unreadable> {
        let env = |var: &str| self.env.get(var).cloned();
        config::resolve(&Sources { home: &self.home, repo: Some(&self.repo), env: &env, flags: &self.flags })
    }

    pub fn resolve(&self) -> Resolved {
        self.try_resolve().unwrap()
    }

    pub fn focuses(&self, only: &[&str]) -> FocusSet {
        let only: Vec<String> = only.iter().map(|s| s.to_string()).collect();
        focus::resolve(&self.home, Some(&self.repo), &self.resolve(), &only).unwrap()
    }
}

pub fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

pub fn names(set: &FocusSet) -> Vec<&str> {
    set.enabled.iter().map(|f| f.name.as_str()).collect()
}
