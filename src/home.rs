//! `~/.pair-watcher/` layout, mode 0700 (I2, C8).

use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
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

    pub fn adapters_dir(&self) -> PathBuf {
        self.root.join("adapters")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// The stable command every harness hook runs (V58).
    pub fn hook_shim(&self) -> PathBuf {
        self.root.join("bin").join("pw-hook")
    }

    /// User price table, merged over the embedded one (I2).
    pub fn models_file(&self) -> PathBuf {
        self.root.join("models.toml")
    }

    /// API keys; refused when group/world-readable (I2).
    pub fn credentials_file(&self) -> PathBuf {
        self.root.join("credentials")
    }

    /// Per-repo files: overrides, Standing rulings (I2).
    pub fn repos_dir(&self) -> PathBuf {
        self.root.join("repos")
    }

    /// User per-repo overrides + tool-written `[approved_focus]`.
    pub fn repo_config(&self, root_commit: &str) -> PathBuf {
        self.root.join("repos").join(format!("{root_commit}.toml"))
    }
}

/// Writes a 0600 file via rename, so a reader (harness, hook, human) never sees half of it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(bytes)?;
    fs::rename(&tmp, path)
}
