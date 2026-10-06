#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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

pub fn executable(path: &Path, text: &str) {
    write(path, text);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A `codex` stand-in whose `app-server` answers `initialize` then `hooks/list`, one hook per
/// `-c hooks.<Event>=` flag it was given, and logs its argv.
pub fn fake_codex(dir: &Path, list_reply: Option<&str>) -> PathBuf {
    let reply = list_reply.map_or_else(
        || {
            r#"keys=""; for a in "$@"; do case "$a" in hooks.state=*) ;; hooks.*=*) ev=${a#hooks.}; ev=${ev%%=*}; keys="$keys{\"key\":\"/<session-flags>/config.toml:$ev:0:0\",\"source\":\"sessionFlags\",\"trustStatus\":\"untrusted\",\"currentHash\":\"sha256:$ev\",\"command\":\"x\"},";; esac; done; keys=${keys%,}
  printf '{"id":2,"result":{"data":[{"cwd":"x","hooks":[%s,{"key":"u","source":"user","trustStatus":"trusted","currentHash":"sha256:u","command":"y"}],"warnings":[],"errors":[]}]}}\n' "$keys""#
                .to_owned()
        },
        |r| format!("printf '%s\\n' '{r}'"),
    );
    let script = format!(
        r#"#!/bin/sh
log="$(dirname "$0")/codex.argv"
printf '%s\n' "$*" >> "$log"
if [ "$1" = "--version" ]; then echo "codex-cli 0.160.0"; exit 0; fi
if [ "$1" = "app-server" ]; then
  shift
  read -r init
  printf '{{"id":1,"result":{{}}}}\n'
  read -r initialized
  read -r list
  {reply}
  read -r _ || exit 0
fi
"#
    );
    let path = dir.join("codex");
    executable(&path, &script);
    path
}
