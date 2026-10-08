//! §T16 `doctor` (V85): one line per check; non-zero only when the Watcher would be BLIND or
//! cannot start.

mod common;

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use pair_watcher::doctor::{self, Level, Line, Probes};
use pair_watcher::harness::{Harness, Version};
use pair_watcher::home::Home;
use pair_watcher::launch::{CODEX_EVENTS, ListedHook};
use tempfile::TempDir;

struct World {
    _dir: TempDir,
    home: Home,
    repo: PathBuf,
    key: Option<&'static str>,
    versions: [Option<Version>; 3],
    listed: Vec<ListedHook>,
    ollama: Result<Vec<String>, String>,
    now: SystemTime,
}

impl World {
    fn new() -> World {
        let dir = TempDir::new().unwrap();
        let home = Home::new(dir.path().join(".pair-watcher"));
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        for args in [&["init", "-q"][..], &["commit", "-q", "--allow-empty", "-m", "root"]] {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok);
        }
        let tested = |h: Harness| Some(h.compat().1);
        World {
            _dir: dir,
            home,
            repo,
            key: Some("sk-test"),
            versions: [tested(Harness::Claude), tested(Harness::Codex), tested(Harness::Pi)],
            listed: Vec::new(),
            ollama: Ok(vec!["nimble:latest".into()]),
            now: SystemTime::now(),
        }
    }

    fn check(&self) -> Vec<Line> {
        let key = self.key;
        let env = move |k: &str| (k == "ANTHROPIC_API_KEY").then_some(key).flatten().map(str::to_owned);
        let versions = self.versions;
        let version = move |h: Harness| versions[Harness::ALL.iter().position(|x| *x == h).unwrap()];
        let listed = self.listed.clone();
        let codex_list = move || -> io::Result<Vec<ListedHook>> { Ok(listed.clone()) };
        let ollama = self.ollama.clone();
        let ollama = move |_: &str| ollama.clone();
        let probes = Probes { env: &env, version: &version, codex_list: &codex_list, ollama: &ollama, now: self.now };
        doctor::check(&self.home, &self.repo, &probes)
    }

    fn global(&self, toml: &str) {
        common::write(&self.home.config_file(), toml);
    }
}

fn texts(lines: &[Line], level: Level) -> Vec<String> {
    lines.iter().filter(|l| l.level == level).map(|l| l.text.clone()).collect()
}

fn has(lines: &[Line], level: Level, needle: &str) -> bool {
    lines.iter().any(|l| l.level == level && l.text.contains(needle))
}

#[test]
fn a_healthy_setup_passes() {
    let w = World::new();
    let lines = w.check();
    assert!(texts(&lines, Level::Warn).is_empty(), "{lines:#?}");
    assert!(!doctor::failed(&lines));
    assert!(has(&lines, Level::Ok, "model claude-sonnet-5-5 (anthropic)"), "{lines:#?}");
    assert!(has(&lines, Level::Ok, "pre-filter nimble at http://localhost:11434"));
    assert!(has(&lines, Level::Ok, "store 0.00 GB of 2 GB"));
    assert_eq!(lines[0].to_string(), "ok   config");
}

#[test]
fn unparseable_user_config_fails_as_blind() {
    let w = World::new();
    w.global("model = [");
    let lines = w.check();
    assert!(doctor::failed(&lines));
    assert!(has(&lines, Level::Fail, "config unreadable"), "V83: {lines:#?}");
}

#[test]
fn no_key_or_unpriced_model_cannot_start() {
    let mut w = World::new();
    w.key = None;
    assert!(has(&w.check(), Level::Fail, "no model provider configured"));
    w.key = Some("sk");
    w.global("model = \"mystery-1\"");
    let lines = w.check();
    assert!(has(&lines, Level::Fail, "mystery-1"), "V47: {lines:#?}");
    w.global("fallback_model = \"mystery-2\"");
    let lines = w.check();
    assert!(!doctor::failed(&lines), "a bad fallback is advice: {lines:#?}");
    assert!(has(&lines, Level::Warn, "fallback_model mystery-2"));
}

#[test]
fn config_problems_repo_values_and_pending_focuses_are_listed() {
    let w = World::new();
    w.global("modle = \"auto\"");
    common::write(&w.repo.join(".pair-watcher/config.toml"), "model = \"claude-haiku-4-5\"\n");
    common::write(&w.repo.join(".pair-watcher/focus/perf.md"), "Watch for slow loops.\n");
    let lines = w.check();
    let warns = texts(&lines, Level::Warn).join("\n");
    assert!(warns.contains("modle"), "{warns}");
    assert!(warns.contains("model"), "V80: ignored repo value: {warns}");
    assert!(warns.contains("repo Focus `perf` awaits approval: pair-watcher focus approve perf"), "{warns}");
    assert!(!doctor::failed(&lines));
}

#[test]
fn exposed_credentials_and_companion_focus_model_warn() {
    let w = World::new();
    common::write(&w.home.credentials_file(), "OPENAI_API_KEY = \"x\"\n");
    fs::set_permissions(w.home.credentials_file(), fs::Permissions::from_mode(0o644)).unwrap();
    common::write(&w.home.focus_dir().join("perf.md"), "---\nmodel: claude-haiku-4-5\n---\nSlow loops.\n");
    let lines = w.check();
    assert!(has(&lines, Level::Warn, "credentials:"), "{lines:#?}");
    assert!(has(&lines, Level::Warn, "Focus `perf` model claude-haiku-4-5 ignored in Companion"), "{lines:#?}");
    w.global("mode = \"swarm\"");
    assert!(!has(&w.check(), Level::Warn, "ignored in Companion"));
}

#[test]
fn harness_versions_against_the_compat_table() {
    let mut w = World::new();
    w.versions = [Some(Version(2, 1, 1)), None, Some(Version(9, 0, 0))];
    let lines = w.check();
    assert!(has(&lines, Level::Warn, "claude 2.1.1 below minimum 2.1.287 → observe-only"), "{lines:#?}");
    assert!(has(&lines, Level::Warn, "pi 9.0.0 untested (last tested 1.0.2)"));
    assert!(!lines.iter().any(|l| l.text.starts_with("codex")), "absent harness: no line");
    assert!(!doctor::failed(&lines), "V60: observe-only still runs");
}

fn manifest(home: &Home, harness: &str, version: &str) {
    let m = format!(r#"{{"installed": {{"{harness}": {{"version": "{version}", "at": 0, "files": []}}}}}}"#);
    common::write(&home.root().join("install.json"), &m);
}

fn ours(home: &Home, key: &str, status: &str) -> ListedHook {
    ListedHook {
        key: key.into(),
        source: "user".into(),
        trust_status: status.into(),
        current_hash: "sha256:x".into(),
        command: format!("{}/bin/pw-hook Stop", home.root().display()),
    }
}

#[test]
fn installed_codex_hooks_must_be_trusted_and_current() {
    let mut w = World::new();
    w.home.ensure_layout().unwrap();
    manifest(&w.home, "codex", "0.0.1");
    w.listed = (0..CODEX_EVENTS.len()).map(|i| ours(&w.home, &format!("k{i}"), "trusted")).collect();
    let lines = w.check();
    assert!(has(&lines, Level::Ok, "codex hooks trusted"), "{lines:#?}");
    assert!(has(&lines, Level::Warn, "codex adapters installed by 0.0.1"), "{lines:#?}");
    w.listed[2].trust_status = "modified".into();
    assert!(has(&w.check(), Level::Warn, "codex hooks not trusted: k2: pair-watcher install codex"));
    w.listed.truncate(2);
    assert!(has(&w.check(), Level::Warn, "codex hooks 2 of 6 hooks present"));
}

#[test]
fn prefilter_checks_follow_the_mode() {
    let mut w = World::new();
    w.ollama = Ok(vec!["llama3:latest".into()]);
    assert!(has(&w.check(), Level::Warn, "ollama pull nimble"), "V53");
    w.ollama = Err("connection refused".into());
    assert!(has(&w.check(), Level::Warn, "Ollama not reachable"));
    w.global("[prefilter]\nmode = \"skip\"\nurl = \"https://prefilter.example\"\n");
    let lines = w.check();
    assert!(has(&lines, Level::Warn, "prefilter.mode = skip refused"), "V55: {lines:#?}");
    assert!(has(&lines, Level::Warn, "wake evidence leaves this machine"), "V55");
    common::write(&w.home.root().join("prefilter/thresholds.json"), r#"{"k": {"skip_below": 0.1}}"#);
    assert!(!has(&w.check(), Level::Warn, "skip refused"));
    w.global("[prefilter]\nmode = \"off\"\n");
    assert!(!w.check().iter().any(|l| l.text.contains("pre-filter")), "off: no probe");
}

#[test]
fn stale_price_table_and_store_over_cap_warn() {
    let mut w = World::new();
    w.home.ensure_layout().unwrap();
    let models = Path::new(env!("CARGO_MANIFEST_DIR")).join("models.toml");
    fs::write(w.home.models_file(), fs::read(models).unwrap()).unwrap();
    w.now = SystemTime::now() + Duration::from_secs(91 * 86_400);
    assert!(has(&w.check(), Level::Warn, "price table updated 91 days ago: pair-watcher models update"));
    w.now = SystemTime::now();
    common::write(&w.home.sessions_dir().join("s1/log.jsonl"), &"x".repeat(2_000));
    w.global("[store]\ncap_gb = 0.000001\n");
    assert!(has(&w.check(), Level::Warn, "over cap 0.000001 GB: pair-watcher prune"));
}
