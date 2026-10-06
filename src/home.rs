//! `~/.pair-watcher/` layout, mode 0700 (I2, C8).

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

const SUBDIRS: [&str; 6] = ["adapters", "bin", "focus", "prefilter", "repos", "sessions"];

#[derive(Clone, Debug)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Home { root: root.into() }
    }

    pub fn from_env() -> io::Result<Self> {
        let home =
            std::env::var_os("HOME").ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        Ok(Home::new(Path::new(&home).join(".pair-watcher")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates the layout; existing dirs are reset to 0700.
    pub fn ensure_layout(&self) -> io::Result<()> {
        let dirs = std::iter::once(self.root.clone()).chain(SUBDIRS.iter().map(|d| self.root.join(d)));
        for dir in dirs {
            DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn focus_dir(&self) -> PathBuf {
        self.root.join("focus")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// The stable command every harness hook runs (V58).
    pub fn hook_shim(&self) -> PathBuf {
        self.root.join("bin").join("pw-hook")
    }

    /// User per-repo overrides + tool-written `[approved_focus]`.
    pub fn repo_config(&self, root_commit: &str) -> PathBuf {
        self.root.join("repos").join(format!("{root_commit}.toml"))
    }
}
