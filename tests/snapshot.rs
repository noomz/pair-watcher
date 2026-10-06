use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use pair_watcher::config::sha256_hex;
use pair_watcher::repo::Repo;
use pair_watcher::snapshot::Store;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// Every file under `dir` with its content hash.
fn tree_hashes(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.display().to_string(), sha256_hex(&fs::read(&p).unwrap()));
            }
        }
    }
    out
}

struct Fixture {
    _dir: TempDir,
    root: std::path::PathBuf,
    store: Store,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("work");
    fs::create_dir_all(root.join("ignored")).unwrap();
    fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
    fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
    fs::write(root.join("ignored/big.bin"), "x").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "init"]);
    let mailbox = dir.path().join("mailbox");
    fs::create_dir_all(&mailbox).unwrap();
    let repo = Repo { root: root.clone(), root_commit: None };
    let store = Store::open(&mailbox, &repo).unwrap();
    Fixture { _dir: dir, root, store }
}

/// V5: untracked files are in the snapshot, ignored ones are not; diff and stat between trees.
#[test]
fn snapshot_includes_untracked_excludes_ignored() {
    let f = fixture();
    let start = f.store.take().unwrap();
    fs::write(f.root.join("a.txt"), "one\nTWO\nthree\n").unwrap();
    fs::write(f.root.join("new.txt"), "fresh\n").unwrap();
    fs::write(f.root.join("ignored/more.bin"), "y").unwrap();
    let now = f.store.take().unwrap();
    assert_ne!(start, now);
    assert_eq!(f.store.take().unwrap(), now, "same work tree, same tree id");

    let diff = f.store.diff(&start, &now).unwrap();
    let paths: Vec<&str> = diff.iter().map(|d| d.path.as_str()).collect();
    assert_eq!(paths, ["a.txt", "new.txt"]);
    assert_eq!((diff[0].added, diff[0].removed), (Some(2), Some(1)));
    assert!(diff[0].patch.starts_with("diff --git a/a.txt") && diff[0].patch.contains("+TWO"));
    assert!(diff[1].patch.contains("+fresh"));
    let stat = f.store.stat(&start, &now).unwrap();
    assert!(stat.iter().all(|s| s.patch.is_empty()) && stat.len() == 2);
}

/// C2: snapshots never write into the Worker repo, `.git` included.
#[test]
fn worker_git_never_written() {
    let f = fixture();
    fs::write(f.root.join("untracked.txt"), "u\n").unwrap();
    fs::write(f.root.join("a.txt"), "changed\n").unwrap();
    let git_dir = f.root.join(".git");
    let before = tree_hashes(&git_dir);
    let a = f.store.take().unwrap();
    fs::write(f.root.join("untracked.txt"), "u2\n").unwrap();
    let b = f.store.take().unwrap();
    f.store.diff(&a, &b).unwrap();
    f.store.stat(&a, &b).unwrap();
    assert_eq!(tree_hashes(&git_dir), before);
}

/// V5: binary files carry no line counts.
#[test]
fn binary_has_no_counts() {
    let f = fixture();
    let a = f.store.take().unwrap();
    fs::write(f.root.join("img.bin"), [0u8, 159, 146, 150, 0, 1]).unwrap();
    let b = f.store.take().unwrap();
    let diff = f.store.diff(&a, &b).unwrap();
    assert_eq!((diff[0].added, diff[0].removed), (None, None));
}
