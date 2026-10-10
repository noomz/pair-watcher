mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use common::executable;
use pair_watcher::placement::{Placement, pane_command};
use tempfile::TempDir;

fn detect(vars: &[(&str, &str)]) -> Placement {
    let env: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Placement::detect(&|var: &str| env.get(var).cloned())
}

#[test]
fn herdr_beats_tmux_beats_print() {
    let both = detect(&[("HERDR_PANE_ID", "w:p1"), ("HERDR_BIN_PATH", "/x/herdr"), ("TMUX", "/tmp/t,1,0")]);
    assert_eq!(both, Placement::Herdr { bin: "/x/herdr".into(), pane: "w:p1".into() });
    assert_eq!(detect(&[("HERDR_PANE_ID", "w:p1")]), Placement::Herdr { bin: "herdr".into(), pane: "w:p1".into() });
    let tmux = detect(&[("TMUX", "/tmp/t,1,0"), ("TMUX_PANE", "%3")]);
    assert_eq!(tmux, Placement::Tmux { bin: "tmux".into(), pane: Some("%3".into()) });
    assert_eq!(detect(&[]), Placement::Print);
}

/// Logs each call's argv as one line; `pane split` answers like Herdr does.
fn logging_bin(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    executable(
        &path,
        r#"#!/bin/sh
for a in "$@"; do printf '[%s]' "$a"; done >> "$0.log"; echo >> "$0.log"
if [ "$1 $2" = "pane split" ]; then echo '{"id":"x","result":{"pane":{"pane_id":"w:p9"}}}'; fi
"#,
    );
    path
}

#[test]
fn herdr_splits_beside_the_worker_then_runs_the_pane() {
    let dir = TempDir::new().unwrap();
    let bin = logging_bin(dir.path(), "herdr");
    let cmd = pane_command(Path::new("/opt/pw dir/pair-watcher"), "abc-1");
    Placement::Herdr { bin: bin.clone(), pane: "w:p1".into() }.open(&cmd).unwrap();
    let log = fs::read_to_string(bin.with_extension("log")).unwrap();
    assert_eq!(
        log,
        "[pane][split][--pane][w:p1][--direction][right][--no-focus]\n[pane][run][w:p9]['/opt/pw dir/pair-watcher' pane --session abc-1]\n"
    );
}

#[test]
fn tmux_splits_without_focus() {
    let dir = TempDir::new().unwrap();
    let bin = logging_bin(dir.path(), "tmux");
    let cmd = pane_command(Path::new("/bin/pair-watcher"), "abc-1");
    Placement::Tmux { bin: bin.clone(), pane: Some("%3".into()) }.open(&cmd).unwrap();
    let log = fs::read_to_string(bin.with_extension("log")).unwrap();
    assert_eq!(log, "[split-window][-h][-d][-t][%3][/bin/pair-watcher pane --session abc-1]\n");
    assert!(Placement::Print.open(&cmd).is_err());
}
