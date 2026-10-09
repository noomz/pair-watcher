mod common;

use common::World;
use pair_watcher::config::{config_hash, env_var};
use pair_watcher::problem::{Origin, Problem, ticker_line};
use toml::Value;

/// Resolves `repo_line` in the repo file over `global` and returns the value plus whether the
/// repo layer won (V80).
fn repo_over(global: &str, repo_line: &str, key: &str) -> (Value, bool) {
    let w = World::new();
    w.global(global).repo_file(repo_line);
    let r = w.resolve();
    let e = r.entry(key);
    let ignored = r.problems.iter().any(|p| matches!(p, Problem::RepoIgnored { key: k, .. } if *k == key));
    let kept = matches!(e.origin, Origin::Repo(_));
    assert_eq!(kept, !ignored, "{key}: kept xor reported");
    (e.value.clone().unwrap(), kept)
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

// ── V80: one test per key direction ─────────────────────────────────────────

#[test]
fn repo_ignored_keys() {
    for (line, key) in [
        ("model = \"gpt-x\"", "model"),
        ("fallback_model = \"m\"", "fallback_model"),
        ("backend = \"claude-p\"", "backend"),
        ("checkpoints = false", "checkpoints"),
        ("infer_rulings = false", "infer_rulings"),
        ("band_digits = false", "band_digits"),
        ("prefilter.url = \"https://hosted.example\"", "prefilter.url"),
        ("prefilter.model = \"other\"", "prefilter.model"),
    ] {
        let w = World::new();
        w.repo_file(line);
        let r = w.resolve();
        assert_eq!(r.entry(key).origin, Origin::Default, "{key}");
        assert!(r.problems.iter().any(|p| matches!(p, Problem::RepoIgnored { key: k, .. } if *k == key)), "{key}");
    }
}

#[test]
fn repo_ceiling_lower_only() {
    assert_eq!(repo_over("ceiling_usd = 4", "ceiling_usd = 3", "ceiling_usd"), (Value::Float(3.0), true));
    assert_eq!(repo_over("ceiling_usd = 4", "ceiling_usd = 4", "ceiling_usd"), (Value::Float(4.0), true));
    assert_eq!(repo_over("ceiling_usd = 4", "ceiling_usd = 50", "ceiling_usd"), (Value::Float(4.0), false));
    assert_eq!(repo_over("", "ceiling_usd = 6", "ceiling_usd"), (Value::Float(5.0), false));
}

#[test]
fn repo_store_lower_only() {
    let line = "[store]\nretention_days = 7";
    assert_eq!(repo_over("", line, "store.retention_days"), (Value::Integer(7), true));
    let line = "[store]\nretention_days = 90";
    assert_eq!(repo_over("", line, "store.retention_days"), (Value::Integer(30), false));
    assert_eq!(repo_over("", "store.cap_gb = 1", "store.cap_gb"), (Value::Float(1.0), true));
    assert_eq!(repo_over("", "store.cap_gb = 9", "store.cap_gb"), (Value::Float(2.0), false));
}

#[test]
fn repo_mode_free() {
    assert_eq!(repo_over("", "mode = \"swarm\"", "mode"), (s("swarm"), true));
    assert_eq!(repo_over("mode = \"swarm\"", "mode = \"companion\"", "mode"), (s("companion"), true));
}

#[test]
fn repo_auto_challenge_true_only() {
    assert_eq!(repo_over("", "auto_challenge = false", "auto_challenge"), (Value::Boolean(true), false));
    let g = "auto_challenge = false";
    assert_eq!(repo_over(g, "auto_challenge = true", "auto_challenge"), (Value::Boolean(true), true));
}

#[test]
fn repo_escalate_toward_note_only() {
    let k = "escalate_unresolved";
    assert_eq!(repo_over("", "escalate_unresolved = \"note\"", k), (s("note"), true));
    assert_eq!(repo_over("", "escalate_unresolved = \"off\"", k), (s("warn"), false));
    let g = "escalate_unresolved = \"off\"";
    assert_eq!(repo_over(g, "escalate_unresolved = \"note\"", k), (s("note"), true));
    assert_eq!(repo_over(g, "escalate_unresolved = \"warn\"", k), (s("off"), false));
}

#[test]
fn repo_prefilter_off_only() {
    let k = "prefilter.mode";
    assert_eq!(repo_over("", "[prefilter]\nmode = \"off\"", k), (s("off"), true));
    assert_eq!(repo_over("", "[prefilter]\nmode = \"skip\"", k), (s("auto"), false));
    assert_eq!(repo_over("prefilter.mode = \"off\"", "prefilter.mode = \"shadow\"", k), (s("off"), false));
}

#[test]
fn repo_telemetry_false_only() {
    assert_eq!(repo_over("", "telemetry = true", "telemetry"), (Value::Boolean(false), false));
    assert_eq!(repo_over("telemetry = true", "telemetry = false", "telemetry"), (Value::Boolean(false), true));
}

#[test]
fn repo_off_ignored_for_builtins() {
    let w = World::new();
    w.repo_file("off = [\"security\", \"repo-thing\"]");
    let r = w.resolve();
    assert_eq!(r.off(), vec!["repo-thing"]);
    assert!(r.problems.iter().any(|p| matches!(p, Problem::RepoIgnored { key: "off", .. })));

    let w = World::new();
    w.global("off = [\"approach\"]").repo_file("off = [\"security\"]");
    let r = w.resolve();
    assert_eq!(r.off(), vec!["approach"], "all-builtin repo off leaves lower layers alone");
}

#[test]
fn repo_bad_value_cannot_loosen() {
    // Bad value → key default (V83), but the default still passes the repo filter.
    let (v, kept) = repo_over("ceiling_usd = 2", "ceiling_usd = \"lots\"", "ceiling_usd");
    assert_eq!((v, kept), (Value::Float(2.0), false));
}

// ── V79: precedence ─────────────────────────────────────────────────────────

#[test]
fn precedence_flags_env_userrepo_repo_global_default() {
    let mut w = World::new();
    w.global("ceiling_usd = 4\nmode = \"swarm\"\nmodel = \"g\"");
    w.repo_file("ceiling_usd = 3");
    w.user_repo("ceiling_usd = 2.5\nmodel = \"ur\"");
    w.env.insert(env_var("ceiling_usd"), "2".into());
    w.flags.push("ceiling_usd=1".into());

    let r = w.resolve();
    assert_eq!(r.get("ceiling_usd"), Some(&Value::Float(1.0)));
    assert_eq!(r.entry("ceiling_usd").origin, Origin::Flag);
    assert_eq!(r.entry("model").origin, Origin::UserRepo(w.home.repo_config(common::ROOT_COMMIT)));
    assert_eq!(r.entry("mode").origin, Origin::Global(w.home.config_file()));
    assert_eq!(r.entry("backend").origin, Origin::Default);

    w.flags.clear();
    assert_eq!(w.resolve().entry("ceiling_usd").origin, Origin::Env("PAIR_WATCHER_CEILING_USD".into()));
    w.env.clear();
    assert_eq!(w.resolve().get("ceiling_usd"), Some(&Value::Float(2.5)));
}

#[test]
fn flags_and_env_are_unfiltered() {
    let mut w = World::new();
    w.env.insert("PAIR_WATCHER_TELEMETRY".into(), "true".into());
    w.flags.push("ceiling_usd=50".into());
    w.flags.push("model=gpt-x".into());
    let r = w.resolve();
    assert_eq!(r.get("telemetry"), Some(&Value::Boolean(true)));
    assert_eq!(r.get("ceiling_usd"), Some(&Value::Float(50.0)));
    assert_eq!(r.get("model"), Some(&Value::String("gpt-x".into())));
    assert!(r.problems.is_empty());
}

#[test]
fn user_repo_file_is_trusted_and_skips_approved_focus() {
    let w = World::new();
    w.user_repo("ceiling_usd = 40\n[approved_focus]\nx = { sha256 = \"00\", approved = true }");
    let r = w.resolve();
    assert_eq!(r.get("ceiling_usd"), Some(&Value::Float(40.0)));
    assert!(r.problems.is_empty(), "{:?}", r.problems);
}

#[test]
fn env_names_and_values() {
    assert_eq!(env_var("prefilter.mode"), "PAIR_WATCHER_PREFILTER__MODE");
    assert_eq!(env_var("store.cap_gb"), "PAIR_WATCHER_STORE__CAP_GB");
    let mut w = World::new();
    w.env.insert("PAIR_WATCHER_PREFILTER__MODE".into(), "shadow".into());
    w.env.insert("PAIR_WATCHER_OFF".into(), "security, approach".into());
    w.env.insert("PAIR_WATCHER_STORE__RETENTION_DAYS".into(), "7".into());
    let r = w.resolve();
    assert_eq!(r.get("prefilter.mode"), Some(&Value::String("shadow".into())));
    assert_eq!(r.off(), vec!["security", "approach"]);
    assert_eq!(r.get("store.retention_days"), Some(&Value::Integer(7)));
}

// ── V83: forgiving validation ───────────────────────────────────────────────

#[test]
fn unknown_keys_get_nearest_hint() {
    let mut w = World::new();
    w.global("telemtry = true\nzzz = 1\n[store]\nretention = 3\n[weird]\na = 1");
    w.flags.push("celing_usd=3".into());
    let r = w.resolve();
    let unknown: Vec<(String, Option<&str>)> = r
        .problems
        .iter()
        .filter_map(|p| match p {
            Problem::UnknownKey { key, hint, .. } => Some((key.clone(), *hint)),
            _ => None,
        })
        .collect();
    assert!(unknown.contains(&("telemtry".into(), Some("telemetry"))));
    assert!(unknown.contains(&("zzz".into(), None)));
    assert!(unknown.contains(&("store.retention".into(), Some("store.retention_days"))));
    assert!(unknown.contains(&("weird".into(), None)), "unknown table reported whole");
    assert!(unknown.contains(&("celing_usd".into(), Some("ceiling_usd"))));
}

#[test]
fn bad_values_fall_back_to_default() {
    let mut w = World::new();
    w.global("ceiling_usd = 9\nmode = \"crowd\"\noff = [1]");
    w.flags.push("ceiling_usd=-1".into());
    w.flags.push("nokey".into());
    let r = w.resolve();
    assert_eq!(r.get("ceiling_usd"), Some(&Value::Float(5.0)));
    assert_eq!(r.get("mode"), Some(&Value::String("companion".into())));
    assert_eq!(r.off(), Vec::<&str>::new());
    let bad = r.problems.iter().filter(|p| matches!(p, Problem::BadValue { .. })).count();
    assert_eq!(bad, 3);
    assert!(r.problems.contains(&Problem::BadFlag { arg: "nokey".into() }));
    assert_eq!(ticker_line(&r.problems).as_deref(), Some("config: 4 problems — pair-watcher doctor"));
}

#[test]
fn unparseable_user_file_is_unreadable_with_line() {
    let w = World::new();
    w.global("mode = \"swarm\"\nceiling_usd = = 3\n");
    let err = w.try_resolve().unwrap_err();
    assert_eq!(err.file, w.home.config_file());
    assert_eq!(err.line, Some(2));
    assert!(err.to_string().starts_with("config unreadable: "));
    assert!(err.to_string().ends_with("config.toml:2"));

    let w = World::new();
    w.user_repo("[[[");
    assert_eq!(w.try_resolve().unwrap_err().file, w.home.repo_config(common::ROOT_COMMIT));
}

#[test]
fn unparseable_repo_file_is_only_a_problem() {
    let w = World::new();
    w.repo_file("ceiling_usd = \n");
    let r = w.resolve();
    assert!(matches!(&r.problems[..], [Problem::RepoUnparseable { line: Some(1), .. }]));
    assert!(r.layer_files.is_empty());
}

#[test]
fn layer_files_lists_present_files() {
    let w = World::new();
    w.global("").repo_file("");
    let r = w.resolve();
    assert_eq!(r.layer_files, vec![w.home.config_file(), w.repo.config_file()]);
}

// ── V84: config hash ────────────────────────────────────────────────────────

#[test]
fn config_hash_tracks_resolved_values_and_briefs() {
    let w = World::new();
    let base = config_hash(&w.resolve(), &w.focuses(&[]).enabled);
    assert_eq!(base.len(), 64);

    // Same resolved value from another layer or spelling → same hash.
    w.global("ceiling_usd = 5");
    assert_eq!(config_hash(&w.resolve(), &w.focuses(&[]).enabled), base);

    // A filtered repo loosening doesn't change the resolved config.
    w.repo_file("telemetry = true");
    assert_eq!(config_hash(&w.resolve(), &w.focuses(&[]).enabled), base);

    w.global("ceiling_usd = 4");
    let lowered = config_hash(&w.resolve(), &w.focuses(&[]).enabled);
    assert_ne!(lowered, base);

    w.user_focus("security", "Only look at auth code.");
    assert_ne!(config_hash(&w.resolve(), &w.focuses(&[]).enabled), lowered);
}

// ── V109: per-harness table ─────────────────────────────────────────────────

#[test]
fn harness_table_beats_top_level_within_a_layer() {
    let w = World::new();
    w.global("model = \"g\"\nbackend = \"api\"\n[harness.claude]\nmodel = \"gc\"\nbackend = \"claude-p\"");
    let r = w.resolve();
    let global = Origin::Global(w.home.config_file());
    assert_eq!(r.get_for("model", "claude"), Some(&s("gc")));
    assert_eq!(r.for_harness("model", "claude").origin, global);
    assert_eq!(r.get_for("backend", "claude"), Some(&s("claude-p")));
    assert_eq!(r.get_for("model", "codex"), Some(&s("g")), "another harness keeps the top-level key");
    assert_eq!(r.get_for("fallback_model", "claude"), None);
    assert_eq!(r.get("model"), Some(&s("g")), "the top-level key itself is unchanged");
}

#[test]
fn harness_table_layer_order_unchanged() {
    let mut w = World::new();
    w.global("[harness.claude]\nmodel = \"gc\"");
    w.user_repo("model = \"ur\"");
    assert_eq!(w.resolve().get_for("model", "claude"), Some(&s("ur")), "higher layer's top-level key wins");

    w.user_repo("[harness.claude]\nmodel = \"urc\"");
    w.env.insert(env_var("model"), "e".into());
    assert_eq!(w.resolve().get_for("model", "claude"), Some(&s("e")), "top-level env beats a file's table");

    w.env.insert(env_var("harness.claude.model"), "ec".into());
    let r = w.resolve();
    assert_eq!(r.get_for("model", "claude"), Some(&s("ec")), "same layer: the table wins");
    assert_eq!(r.for_harness("model", "claude").origin, Origin::Env("PAIR_WATCHER_HARNESS__CLAUDE__MODEL".into()));

    w.flags.push("model=f".into());
    assert_eq!(w.resolve().get_for("model", "claude"), Some(&s("f")));
    w.flags.push("harness.claude.model=fc".into());
    let r = w.resolve();
    assert_eq!(
        (r.get_for("model", "claude"), &r.for_harness("model", "claude").origin),
        (Some(&s("fc")), &Origin::Flag)
    );
    assert_eq!(r.get_for("model", "pi"), Some(&s("f")));
}

#[test]
fn harness_env_names() {
    assert_eq!(env_var("harness.codex.backend"), "PAIR_WATCHER_HARNESS__CODEX__BACKEND");
    assert_eq!(env_var("harness.pi.fallback_model"), "PAIR_WATCHER_HARNESS__PI__FALLBACK_MODEL");
    let mut w = World::new();
    w.env.insert("PAIR_WATCHER_HARNESS__CODEX__BACKEND".into(), "codex-exec".into());
    assert_eq!(w.resolve().get_for("backend", "codex"), Some(&s("codex-exec")));
}

#[test]
fn repo_file_cannot_set_harness_table() {
    let w = World::new();
    w.global("[harness.claude]\nmodel = \"gc\"").repo_file("[harness.claude]\nmodel = \"x\"\nbackend = \"claude-p\"");
    let r = w.resolve();
    assert_eq!(r.get_for("model", "claude"), Some(&s("gc")));
    assert_eq!(r.get_for("backend", "claude"), Some(&s("api")));
    for key in ["harness.claude.model", "harness.claude.backend"] {
        let ignored = r.problems.iter().any(|p| matches!(p, Problem::RepoIgnored { key: k, .. } if *k == key));
        assert!(ignored, "{key} reported");
    }
}

#[test]
fn harness_table_problems() {
    let w = World::new();
    w.global(
        "backend = \"claude-p\"\n[harness.codex]\nbackend = \"pi\"\nmodl = \"x\"\n[harness.cursor]\nmodel = \"y\"",
    );
    let r = w.resolve();
    assert_eq!(r.get_for("backend", "codex"), Some(&s("claude-p")), "bad value falls back to the top-level key");
    assert!(r.problems.iter().any(|p| matches!(p, Problem::BadValue { key: "harness.codex.backend", .. })));
    let unknown: Vec<(String, Option<&str>)> = r
        .problems
        .iter()
        .filter_map(|p| match p {
            Problem::UnknownKey { key, hint, .. } => Some((key.clone(), *hint)),
            _ => None,
        })
        .collect();
    assert!(unknown.contains(&("harness.codex.modl".into(), Some("harness.codex.model"))));
    assert!(unknown.iter().any(|(k, _)| k == "harness.cursor"));
}

#[test]
fn harness_table_in_show_and_hash() {
    let w = World::new();
    let base = config_hash(&w.resolve(), &w.focuses(&[]).enabled);
    w.global("[harness.pi]\nmodel = \"gpt-6.1-sol\"");
    let r = w.resolve();
    let shown: Vec<_> = r.entries().filter(|(_, e)| e.value.is_some()).map(|(k, e)| (k, e.origin.clone())).collect();
    assert!(shown.contains(&("harness.pi.model", Origin::Global(w.home.config_file()))));
    assert!(r.canonical().contains("harness.pi.model = \"gpt-6.1-sol\"\n"));
    assert_ne!(config_hash(&r, &w.focuses(&[]).enabled), base);
}
