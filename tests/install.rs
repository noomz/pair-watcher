mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use pair_watcher::adapters;
use pair_watcher::harness::Harness;
use pair_watcher::home::Home;
use pair_watcher::install::{self, Ctx, PLUGIN_DIRS_ENV, Paths, Stripped};
use pair_watcher::launch::{CLAUDE_EVENTS, CODEX_EVENTS, ListedHook};
use serde_json::{Value, json};
use tempfile::TempDir;
use toml_edit::DocumentMut;

struct World {
    _dir: TempDir,
    home: Home,
    paths: Paths,
    exe: PathBuf,
}

impl World {
    fn new() -> World {
        let dir = TempDir::new().unwrap();
        let user = dir.path().join("u");
        let home = Home::new(user.join(".pair-watcher"));
        home.ensure_layout().unwrap();
        let env = |k: &str| (k == "HOME").then(|| user.to_string_lossy().into_owned());
        let paths = Paths::from_env(&env).unwrap();
        let exe = dir.path().join("bin/pair-watcher");
        common::executable(&exe, "#!/bin/sh\n");
        World { _dir: dir, home, paths, exe }
    }

    fn install(&self, harness: Harness) -> Result<Vec<String>, String> {
        self.install_with(harness, &|| Ok(listed(&self.paths.codex_hooks)))
    }

    fn install_with(
        &self,
        harness: Harness,
        list: &dyn Fn() -> io::Result<Vec<ListedHook>>,
    ) -> Result<Vec<String>, String> {
        let cx = Ctx { home: &self.home, paths: &self.paths, exe: &self.exe, codex_list: list };
        install::install(&cx, harness)
    }

    fn uninstall(&self, harness: Harness) -> Result<Vec<String>, String> {
        install::uninstall(&self.home, &self.paths, harness)
    }

    fn ours(&self, s: &str) -> bool {
        install::is_ours(&self.home, s)
    }
}

/// What Codex `hooks/list` reports for `hooks.json`: key by place, hash by command.
fn listed(hooks_json: &Path) -> Vec<ListedHook> {
    let doc: Value = serde_json::from_slice(&fs::read(hooks_json).unwrap()).unwrap();
    let mut out = Vec::new();
    for (event, groups) in doc["hooks"].as_object().unwrap() {
        for (g, group) in groups.as_array().unwrap().iter().enumerate() {
            for (h, hook) in group["hooks"].as_array().unwrap().iter().enumerate() {
                let command = hook["command"].as_str().unwrap().to_owned();
                out.push(ListedHook {
                    key: install::trust_key(&hooks_json.to_string_lossy(), event, (g, h)),
                    source: "user".into(),
                    trust_status: "untrusted".into(),
                    current_hash: format!("sha256:{command}"),
                    command,
                });
            }
        }
    }
    out
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn write(path: &Path, text: &str) {
    common::write(path, text);
}

fn user_hook(cmd: &str) -> Value {
    json!({"hooks": [{"type": "command", "command": cmd}]})
}

#[test]
fn paths_follow_harness_env_overrides() {
    let env = |k: &str| match k {
        "HOME" => Some("/h".to_owned()),
        "CODEX_HOME" => Some("/c".to_owned()),
        "PI_CODING_AGENT_DIR" => Some(String::new()),
        _ => None,
    };
    let p = Paths::from_env(&env).unwrap();
    assert_eq!(p.claude_settings, Path::new("/h/.claude/settings.json"));
    assert_eq!(p.codex_hooks, Path::new("/c/hooks.json"));
    assert_eq!(p.codex_config, Path::new("/c/config.toml"));
    assert_eq!(p.pi_extension, Path::new("/h/.pi/agent/extensions/pair-watcher.ts"), "empty var = default");
}

#[test]
fn tag_is_a_path_under_our_home_or_a_pair_watcher_file() {
    let w = World::new();
    let root = w.home.root().to_string_lossy().into_owned();
    assert!(w.ours(&format!("{root}/bin/pw-hook Stop")));
    assert!(w.ours(&format!("'{root}/bin/pw-hook' Stop")));
    assert!(w.ours("/usr/local/bin/pair-watcher hook Stop"));
    assert!(w.ours("/x/extensions/pair-watcher.ts"));
    assert!(!w.ours(&format!("{root}-old/bin/pw-hook Stop")), "a sibling dir is not ours");
    assert!(!w.ours("bash ~/.codex/herdr-agent-state.sh idle"));
    assert!(!w.ours("echo watcher"));
}

#[test]
fn trust_keys_use_codex_snake_case_events() {
    assert_eq!(
        install::trust_key("/c/hooks.json", "PermissionRequest", (1, 0)),
        "/c/hooks.json:permission_request:1:0"
    );
    assert_eq!(install::trust_key("/c/hooks.json", "PreToolUse", (0, 2)), "/c/hooks.json:pre_tool_use:0:2");
    assert_eq!(install::trust_key("/c/hooks.json", "Stop", (0, 0)), "/c/hooks.json:stop:0:0");
}

#[test]
fn strip_removes_ours_and_reports_shifted_user_hooks() {
    let ours = |s: &str| s.starts_with("OURS");
    let mut doc = json!({
        "other": 1,
        "hooks": {
            "Stop": [user_hook("a"), user_hook("OURS Stop"), user_hook("b"),
                     {"hooks": [{"command": "c"}, {"command": "OURS x"}, {"command": "d"}]}],
            "SessionStart": [user_hook("OURS SessionStart")],
            "Empty": []
        }
    });
    let s = install::strip_hooks(&mut doc, &ours);
    assert_eq!(
        doc,
        json!({"other": 1, "hooks": {"Stop": [user_hook("a"), user_hook("b"), {"hooks": [{"command": "c"}, {"command": "d"}]}], "Empty": []}}),
        "an event emptied by us goes; a user's empty event stays"
    );
    assert_eq!(
        s,
        Stripped {
            removed: vec![("Stop".into(), (1, 0)), ("Stop".into(), (3, 1)), ("SessionStart".into(), (0, 0))],
            moved: vec![
                ("Stop".into(), (2, 0), (1, 0)),
                ("Stop".into(), (3, 0), (2, 0)),
                ("Stop".into(), (3, 2), (2, 1))
            ],
        }
    );
    let mut only = json!({"hooks": {"Stop": [user_hook("OURS")]}});
    install::strip_hooks(&mut only, &ours);
    assert_eq!(only, json!({}), "a `hooks` emptied by us goes");
}

#[test]
fn claude_install_is_idempotent_and_uninstall_keeps_user_edits() {
    let w = World::new();
    let settings = &w.paths.claude_settings;
    write(
        settings,
        r#"{"theme": "dark", "env": {"FOO": "1", "CLAUDE_CODE_PLUGIN_DIRS": "/mine"}, "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}}"#,
    );
    fs::set_permissions(settings, fs::Permissions::from_mode(0o644)).unwrap();
    let before = read_json(settings);

    let lines = w.install(Harness::Claude).unwrap();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let doc = read_json(settings);
    assert_eq!(doc["theme"], "dark");
    let keys: Vec<&String> = doc.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["theme", "env", "hooks"], "user key order kept");
    for ev in CLAUDE_EVENTS {
        let groups = doc["hooks"][ev].as_array().unwrap();
        let cmd = groups.last().unwrap()["hooks"][0]["command"].as_str().unwrap();
        assert!(w.ours(cmd) && cmd.ends_with(ev), "{cmd}");
    }
    assert_eq!(doc["hooks"]["Stop"][0]["hooks"][0]["command"], "say done", "user hook first, untouched");
    let dirs = doc["env"][PLUGIN_DIRS_ENV].as_str().unwrap();
    let mod_dir = w.home.adapters_dir().join(env!("CARGO_PKG_VERSION")).join(adapters::CLAUDE_MOD);
    assert_eq!(dirs, format!("/mine:{}", mod_dir.display()));
    assert!(mod_dir.join("hooks/register.ts").is_file(), "the mod dir it names exists");
    assert!(w.home.hook_shim().is_file());
    assert_eq!(fs::metadata(settings).unwrap().permissions().mode() & 0o777, 0o644, "mode kept");

    let once = fs::read(settings).unwrap();
    w.install(Harness::Claude).unwrap();
    assert_eq!(fs::read(settings).unwrap(), once, "V59: idempotent");
    assert!(install::installed(&w.home, Harness::Claude).is_some());

    // The user edits after install; uninstall keeps it.
    let mut edited = read_json(settings);
    edited["model"] = json!("opus");
    write(settings, &serde_json::to_string(&edited).unwrap());
    w.uninstall(Harness::Claude).unwrap();
    let mut expected = before;
    expected["model"] = json!("opus");
    assert_eq!(read_json(settings), expected);
    assert!(install::installed(&w.home, Harness::Claude).is_none());
    assert_eq!(w.uninstall(Harness::Claude).unwrap(), Vec::<String>::new(), "nothing left to remove");
}

#[test]
fn claude_install_writes_through_a_symlinked_settings_file() {
    let w = World::new();
    let real = w.home.root().parent().unwrap().join("dotfiles/settings.json");
    write(&real, "{}");
    fs::create_dir_all(w.paths.claude_settings.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, &w.paths.claude_settings).unwrap();
    w.install(Harness::Claude).unwrap();
    assert!(fs::symlink_metadata(&w.paths.claude_settings).unwrap().file_type().is_symlink());
    assert!(read_json(&real)["hooks"]["Stop"].is_array());
}

#[test]
fn claude_install_refuses_a_settings_file_it_cannot_parse() {
    let w = World::new();
    write(&w.paths.claude_settings, "{ not json");
    let err = w.install(Harness::Claude).unwrap_err();
    assert!(err.contains("settings.json"), "{err}");
    assert_eq!(fs::read_to_string(&w.paths.claude_settings).unwrap(), "{ not json");
}

const USER_CONFIG: &str = r#"model = "gpt-6.1-sol" # mine

[hooks.state."HOOKS:stop:0:0"]
trusted_hash = "sha256:userA"
"#;

#[test]
fn codex_install_persists_trust_for_ours_only_and_uninstall_rekeys_user_hooks() {
    let w = World::new();
    let hooks = &w.paths.codex_hooks;
    let file = hooks.to_string_lossy().into_owned();
    write(hooks, &json!({"hooks": {"Stop": [user_hook("userA")]}}).to_string());
    write(&w.paths.codex_config, &USER_CONFIG.replace("HOOKS", &file));

    w.install(Harness::Codex).unwrap();
    let doc = read_json(hooks);
    assert_eq!(doc["hooks"]["Stop"][0], user_hook("userA"));
    let config = fs::read_to_string(&w.paths.codex_config).unwrap();
    assert!(config.starts_with("model = \"gpt-6.1-sol\" # mine\n"), "user formatting kept:\n{config}");
    let trust = install::trust_of(&config.parse::<DocumentMut>().unwrap());
    assert_eq!(trust.len(), CODEX_EVENTS.len() + 1);
    assert_eq!(trust[&format!("{file}:stop:0:0")], "sha256:userA", "user trust untouched");
    let ours_stop = &trust[&format!("{file}:stop:1:0")];
    assert!(w.ours(ours_stop.trim_start_matches("sha256:")), "V59: our hash from Codex: {ours_stop}");
    assert_eq!(install::installed(&w.home, Harness::Codex).unwrap().trust.len(), CODEX_EVENTS.len());

    let once = (fs::read(hooks).unwrap(), fs::read(&w.paths.codex_config).unwrap());
    w.install(Harness::Codex).unwrap();
    assert_eq!((fs::read(hooks).unwrap(), fs::read(&w.paths.codex_config).unwrap()), once, "V59: idempotent");

    // The user adds and trusts a hook after ours: it sits at stop:2:0.
    let mut doc = read_json(hooks);
    doc["hooks"]["Stop"].as_array_mut().unwrap().push(user_hook("userB"));
    write(hooks, &doc.to_string());
    let mut config: DocumentMut = fs::read_to_string(&w.paths.codex_config).unwrap().parse().unwrap();
    let mut t = BTreeMap::new();
    t.insert(format!("{file}:stop:2:0"), "sha256:userB".to_owned());
    install::set_trust(&mut config, &t).unwrap();
    fs::write(&w.paths.codex_config, config.to_string()).unwrap();

    let lines = w.uninstall(Harness::Codex).unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(read_json(hooks), json!({"hooks": {"Stop": [user_hook("userA"), user_hook("userB")]}}));
    let config = fs::read_to_string(&w.paths.codex_config).unwrap();
    let trust = install::trust_of(&config.parse::<DocumentMut>().unwrap());
    let expected: BTreeMap<String, String> = [
        (format!("{file}:stop:0:0"), "sha256:userA".to_owned()),
        (format!("{file}:stop:1:0"), "sha256:userB".to_owned()),
    ]
    .into();
    assert_eq!(trust, expected, "V59: shifted user hook keeps its hash under its new key; ours gone");
    assert!(config.starts_with("model = \"gpt-6.1-sol\" # mine\n"));
}

#[test]
fn codex_install_rolls_back_when_trust_cannot_be_read() {
    let w = World::new();
    let original = json!({"hooks": {"Stop": [user_hook("userA")]}}).to_string();
    write(&w.paths.codex_hooks, &original);
    let err = w.install_with(Harness::Codex, &|| Err(io::Error::other("codex not found"))).unwrap_err();
    assert!(err.contains("codex not found"), "{err}");
    assert_eq!(fs::read_to_string(&w.paths.codex_hooks).unwrap(), original, "V59: atomic");
    assert!(!w.paths.codex_config.exists());
    assert!(install::installed(&w.home, Harness::Codex).is_none());

    let err = w.install_with(Harness::Codex, &|| Ok(Vec::new())).unwrap_err();
    assert!(err.contains("listed 0 of our"), "{err}");
    assert_eq!(fs::read_to_string(&w.paths.codex_hooks).unwrap(), original);
}

#[test]
fn codex_install_creates_missing_files() {
    let w = World::new();
    w.install(Harness::Codex).unwrap();
    let doc = read_json(&w.paths.codex_hooks);
    assert_eq!(doc["hooks"].as_object().unwrap().len(), CODEX_EVENTS.len());
    let config = fs::read_to_string(&w.paths.codex_config).unwrap();
    assert!(!config.contains("[hooks]\n"), "no empty parent header:\n{config}");
    w.uninstall(Harness::Codex).unwrap();
    assert_eq!(read_json(&w.paths.codex_hooks), json!({}));
    assert!(install::trust_of(&fs::read_to_string(&w.paths.codex_config).unwrap().parse().unwrap()).is_empty());
}

#[test]
fn pi_install_writes_the_extension_and_uninstall_removes_it() {
    let w = World::new();
    w.install(Harness::Pi).unwrap();
    assert_eq!(fs::read_to_string(&w.paths.pi_extension).unwrap(), adapters::PI_SOURCE);
    w.uninstall(Harness::Pi).unwrap();
    assert!(!w.paths.pi_extension.exists());
    assert!(w.paths.pi_extension.parent().unwrap().is_dir(), "the user's extensions dir stays");
}
