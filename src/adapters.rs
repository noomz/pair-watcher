//! Adapters embedded in the binary, written to `~/.pair-watcher/adapters/<version>/` (I2, C6).
//! Lock-step with the binary: each version gets its own dir, so a running session keeps its files.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

use crate::home::{Home, write_atomic};

/// (path under the version dir, contents).
const FILES: [(&str, &str); 1] = [(PI_EXTENSION, include_str!("../adapters/pi/pair-watcher.ts"))];

/// pi `-e` target, relative to the version dir.
pub const PI_EXTENSION: &str = "pi/pair-watcher.ts";

/// Writes every adapter file that is missing or differs; returns the version dir.
pub fn write(home: &Home) -> io::Result<PathBuf> {
    let dir = home.adapters_dir().join(env!("CARGO_PKG_VERSION"));
    for (rel, text) in FILES {
        let path = dir.join(rel);
        if fs::read(&path).is_ok_and(|old| old == text.as_bytes()) {
            continue;
        }
        let parent = path.parent().unwrap_or(&dir);
        DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        write_atomic(&path, text.as_bytes())?;
    }
    Ok(dir)
}
