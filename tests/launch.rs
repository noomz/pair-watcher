mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use common::fake_codex;
use pair_watcher::harness::Harness;
use pair_watcher::launch::{self, CODEX_BYPASS_FLAG, CODEX_EVENTS, ListedHook, Plan};
use tempfile::TempDir;

const SHIM: &str = "/home/u/.pair-watcher/bin/pw-hook";

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// Parses a `-c key=value` override the way Codex does: the value is one TOML value.
fn toml_value(flag: &str) -> (String, toml::Value) {
    let (key, value) = flag.split_once('=').unwrap();
    let doc: toml::Table = toml::from_str(&format!("v = {value}")).unwrap();
    (key.to_owned(), doc["v"].clone())
}

#[test]
fn claude_settings_route_wake_hooks_through_the_shim() {
    let settings = launch::claude_settings(Path::new(SHIM));
    let hooks = settings["hooks"].as_object().unwrap();
    let mut events: Vec<&str> = hooks.keys().map(String::as_str).collect();
    events.sort();
    assert_eq!(
        events,
        ["PermissionRequest", "SessionStart", "Stop"],
        "V56: blocked = PermissionRequest, not Notification"
    );
    for (event, groups) in hooks {
        let hook = &groups[0]["hooks"][0];
        assert_eq!(hook["type"], "command");
        assert_eq!(hook["command"], format!("{SHIM} {event}"), "V58: stable shim command, no version path");
    }
}

#[test]
fn hook_command_quotes_only_paths_that_need_it() {
    assert_eq!(launch::hook_command(Path::new(SHIM), "Stop"), format!("{SHIM} Stop"));
    assert_eq!(
        launch::hook_command(Path::new("/Users/a b/it's/pw-hook"), "Stop"),
        r"'/Users/a b/it'\''s/pw-hook' Stop"
    );
}

#[test]
fn codex_hook_values_are_valid_toml_per_event() {
    let values = launch::codex_hook_values(Path::new(SHIM));
    assert_eq!(values.len(), CODEX_EVENTS.len());
    for (value, event) in values.iter().zip(CODEX_EVENTS) {
        let (key, v) = toml_value(value);
        assert_eq!(key, format!("hooks.{event}"));
        let hook = &v[0]["hooks"][0];
        assert_eq!(hook["command"].as_str(), Some(format!("{SHIM} {event}").as_str()));
        assert_eq!(hook["type"].as_str(), Some("command"));
    }
}

#[test]
fn codex_trust_is_one_inline_table_covering_every_key() {
    let trust = vec![
        ("/<session-flags>/config.toml:stop:0:0".to_owned(), "sha256:aa".to_owned()),
        ("/<session-flags>/config.toml:pre_tool_use:0:0".to_owned(), "sha256:bb".to_owned()),
    ];
    let value = launch::codex_trust_value(&trust);
    let (key, table) = toml_value(&value);
    assert_eq!(key, "hooks.state", "V57: never the dotted hooks.state.\"<key>\" form");
    for (k, hash) in &trust {
        assert_eq!(table[k.as_str()]["trusted_hash"].as_str(), Some(hash.as_str()));
    }

    let values = launch::codex_hook_values(Path::new(SHIM));
    let args = launch::codex_args(&values, &value);
    assert_eq!(
        args.iter().filter(|a| a.starts_with("hooks.state=")).count(),
        1,
        "a second hooks.state replaces the first"
    );
    assert!(args.chunks(2).all(|pair| pair[0] == "-c"));
    assert!(!args.iter().any(|a| a == CODEX_BYPASS_FLAG), "C10");
}

#[test]
fn codex_trust_takes_session_flag_hooks_only() {
    let hook = |key: &str, source: &str| ListedHook {
        key: key.into(),
        source: source.into(),
        trust_status: "untrusted".into(),
        current_hash: format!("sha256:{key}"),
        command: String::new(),
    };
    let listed = [hook("s1", "sessionFlags"), hook("u1", "user"), hook("s2", "sessionFlags"), hook("p1", "project")];
    let trust = launch::codex_trust(&listed);
    assert_eq!(trust, [("s1".into(), "sha256:s1".into()), ("s2".into(), "sha256:s2".into())]);
}

#[test]
fn bypass_flag_is_refused_for_codex() {
    assert!(launch::refusal(Harness::Codex, &os(&["codex", CODEX_BYPASS_FLAG])).is_some(), "C10");
    assert!(launch::refusal(Harness::Codex, &os(&["codex", "-m", "gpt"])).is_none());
    assert!(launch::refusal(Harness::Claude, &os(&["claude", "-p", "hi"])).is_none());
}

#[test]
fn plan_puts_our_flags_before_the_users() {
    let plan = Plan::new(&os(&["/bin/pi", "--model", "x"]), launch::pi_args(Path::new("/a/pi/pair-watcher.ts")));
    assert_eq!(plan.program, "/bin/pi");
    assert_eq!(plan.args, os(&["-e", "/a/pi/pair-watcher.ts", "--model", "x"]));
    let plan = Plan::new(&os(&["claude", "--resume"]), launch::claude_args(Path::new("/s/claude-settings.json")));
    assert_eq!(plan.args, os(&["--settings", "/s/claude-settings.json", "--resume"]));
}

#[test]
fn hooks_list_reads_current_hash_from_app_server() {
    let dir = TempDir::new().unwrap();
    let codex = fake_codex(dir.path(), None);
    let values = launch::codex_hook_values(Path::new(SHIM));
    let listed = launch::codex_hooks_list(codex.as_os_str(), &values, dir.path(), Duration::from_secs(5)).unwrap();
    let trust = launch::codex_trust(&listed);
    assert_eq!(trust.len(), CODEX_EVENTS.len());
    assert!(trust.iter().any(|(k, h)| k == "/<session-flags>/config.toml:Stop:0:0" && h == "sha256:Stop"));
    let argv = fs::read_to_string(dir.path().join("codex.argv")).unwrap();
    assert!(argv.starts_with("app-server -c hooks.SessionStart="), "{argv}");
}

#[test]
fn hooks_list_surfaces_errors_and_times_out() {
    let dir = TempDir::new().unwrap();
    let codex = fake_codex(dir.path(), Some(r#"{"id":2,"error":{"code":-32601,"message":"no such method"}}"#));
    let err = launch::codex_hooks_list(codex.as_os_str(), &[], dir.path(), Duration::from_secs(5)).unwrap_err();
    assert!(err.to_string().contains("no such method"), "{err}");

    let hang = dir.path().join("hang/codex");
    fs::create_dir_all(hang.parent().unwrap()).unwrap();
    // Reads requests, never answers, keeps stdout open.
    fs::write(&hang, "#!/bin/sh\nwhile read -r line; do :; done\n").unwrap();
    fs::set_permissions(&hang, fs::Permissions::from_mode(0o755)).unwrap();
    let err = launch::codex_hooks_list(hang.as_os_str(), &[], dir.path(), Duration::from_millis(300)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
}
