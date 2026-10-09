//! C6: the release binary carries its adapters. A copy run outside the repo writes every shipped
//! adapter file, byte for byte, under its own version dir.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Tracked adapter files a session needs: tests and editor config stay in the repo.
fn shipped() -> BTreeMap<String, Vec<u8>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("git").args(["ls-files", "adapters"]).current_dir(root).output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|p| !p.contains("/test/") && !p.contains("/tests/") && !p.ends_with("tsconfig.json"))
        .map(|p| (p.trim_start_matches("adapters/").to_owned(), fs::read(root.join(p)).unwrap()))
        .collect()
}

fn written(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
                files.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    files
}

#[test]
fn binary_outside_the_repo_writes_every_shipped_adapter() {
    let dir = TempDir::new().unwrap();
    let exe = dir.path().join("bin/pair-watcher");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_pair-watcher"), &exe).unwrap();
    let home = dir.path().join("home");
    fs::create_dir(&home).unwrap();

    let status = Command::new(&exe)
        .args(["install", "pi"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));

    let version_dir = home.join(".pair-watcher/adapters").join(env!("CARGO_PKG_VERSION"));
    let got = written(&version_dir);
    let want = shipped();
    assert_eq!(got.keys().collect::<Vec<_>>(), want.keys().collect::<Vec<_>>());
    assert!(got == want, "embedded adapter differs from the repo file");
}
