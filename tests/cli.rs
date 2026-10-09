use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use pair_watcher::repo;
use tempfile::TempDir;

fn pw(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pair-watcher"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("PAIR_WATCHER_CEILING_USD")
        .output()
        .unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn home_layout_is_0700_and_tightens_existing() {
    let t = TempDir::new().unwrap();
    let root = t.path().join(".pair-watcher");
    fs::create_dir_all(root.join("sessions")).unwrap();
    fs::set_permissions(root.join("sessions"), fs::Permissions::from_mode(0o755)).unwrap();

    let out = pw(t.path(), t.path(), &["config", "show"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    for dir in ["", "adapters", "bin", "focus", "prefilter", "repos", "sessions"] {
        let mode = fs::metadata(root.join(dir)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{dir}");
    }
}

#[test]
fn config_show_origin_in_a_git_repo() {
    let t = TempDir::new().unwrap();
    let home = t.path().join("home");
    let work = t.path().join("work");
    fs::create_dir_all(work.join(".pair-watcher")).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["commit", "-q", "--allow-empty", "-m", "root"]);
    let rc = repo::discover(&work).unwrap().root_commit.unwrap();

    fs::create_dir_all(home.join(".pair-watcher/repos")).unwrap();
    fs::write(home.join(".pair-watcher/config.toml"), "ceiling_usd = 4\n").unwrap();
    fs::write(work.join(".pair-watcher/config.toml"), "telemetry = true\nmode = \"swarm\"\n").unwrap();
    fs::write(home.join(format!(".pair-watcher/repos/{rc}.toml")), "model = \"m1\"\n").unwrap();

    let out = pw(&home, &work, &["config", "show", "--origin"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stderr}");
    assert!(stdout.contains("ceiling_usd = 4.0  # ~/.pair-watcher/config.toml"), "{stdout}");
    assert!(stdout.contains("mode = \"swarm\"  # "), "{stdout}");
    assert!(stdout.contains(&format!("model = \"m1\"  # ~/.pair-watcher/repos/{rc}.toml")), "{stdout}");
    assert!(stdout.contains("telemetry = false  # default"), "{stdout}");
    assert!(stdout.contains("# config hash: "), "{stdout}");
    assert!(stderr.contains("ignored `telemetry = true` (repo may only set false)"), "{stderr}");
}

#[test]
fn config_show_unreadable_user_file_fails_with_location() {
    let t = TempDir::new().unwrap();
    fs::create_dir_all(t.path().join(".pair-watcher")).unwrap();
    fs::write(t.path().join(".pair-watcher/config.toml"), "ok = 1\n[[bad\n").unwrap();
    let out = pw(t.path(), t.path(), &["config", "show"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("config unreadable: ~/.pair-watcher/config.toml:2"), "{stderr}");
}

#[test]
fn hook_is_silent_and_bare_mcp_refuses() {
    let t = TempDir::new().unwrap();
    let out = pw(t.path(), t.path(), &["hook", "Stop", "--anything"]);
    assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty());

    let out = pw(t.path(), t.path(), &["mcp"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("started per wake by a subscription backend"));

    let out = pw(t.path(), t.path(), &["report"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("pair-watcher report · all sessions · 0 sessions"));
    let out = pw(t.path(), t.path(), &["report", "--since", "yesterday"]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("expected YYYY-MM-DD"));

    // `attach` needs a live session or a Worker transcript.
    let out = pw(t.path(), t.path(), &["attach", "s1"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no live session"));
}

#[test]
fn cli_surface_parses_every_i1_command() {
    use clap::Parser;
    use pair_watcher::cli::Cli;
    for args in [
        "run -c ceiling_usd=3 -c mode=swarm --ceiling 2 --focus approach --focus security -- codex exec",
        "attach s1",
        "pane",
        "pane --session s1",
        "mcp",
        "install all",
        "uninstall pi",
        "auth",
        "models update --yes",
        "models rollback",
        "doctor",
        "config show --origin",
        "focus approve docs --session s1",
        "mute security --session s1",
        "nochallenge",
        "pause --session s1",
        "rule E1 watcher --session s1",
        "rule E1 dismiss",
        "undo",
        "keep",
        "rulings list",
        "rulings rm R1",
        "report --export --explicit-only",
        "prefilter fit --target 0.9",
        "replay --variant model wakes-1",
        "audit --yes --max-usd 2 --sample 5 --model m --resume",
        "telemetry preview",
        "prune",
    ] {
        let argv = std::iter::once("pair-watcher").chain(args.split(' '));
        if let Err(e) = Cli::try_parse_from(argv) {
            panic!("`{args}`: {e}");
        }
    }
}

#[test]
fn run_flag_layer_orders_shortcuts_last() {
    use clap::Parser;
    use pair_watcher::cli::{Cli, Command};
    let cli = Cli::try_parse_from("pair-watcher run -c mode=companion --mode swarm --ceiling 2.5 -- claude".split(' '))
        .unwrap();
    let Command::Run(run) = cli.command else { panic!() };
    assert_eq!(run.flag_layer(), ["mode=companion", "mode=swarm", "ceiling_usd=2.5"]);
    assert_eq!(run.worker, ["claude"]);
}

fn pw_stdin(home: &Path, args: &[&str], stdin: &str) -> Output {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_pair-watcher"))
        .args(args)
        .env("HOME", home)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

/// I18, C8: `auth` writes keys 0600, keeps the rest, and an empty answer keeps a key.
#[test]
fn auth_stores_keys_0600() {
    let t = TempDir::new().unwrap();
    let creds = t.path().join(".pair-watcher/credentials");
    let out = pw_stdin(t.path(), &["auth"], "sk-ant\n\n");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::metadata(&creds).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::read_to_string(&creds).unwrap(), "ANTHROPIC_API_KEY = \"sk-ant\"\n");

    fs::set_permissions(&creds, fs::Permissions::from_mode(0o644)).unwrap();
    let out = pw_stdin(t.path(), &["auth"], "\nsk-oai\n");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::metadata(&creds).unwrap().permissions().mode() & 0o777, 0o600, "exposed file fixed");
    assert_eq!(fs::read_to_string(&creds).unwrap(), "ANTHROPIC_API_KEY = \"sk-ant\"\nOPENAI_API_KEY = \"sk-oai\"\n");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("sk-"), "keys never printed");

    let out = pw_stdin(t.path(), &["auth"], "\n\n");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "no keys changed");
}

/// V51: `primary` appends the human fact for the session.
#[test]
fn primary_is_a_session_fact() {
    let t = TempDir::new().unwrap();
    let home = pair_watcher::home::Home::new(t.path().join(".pair-watcher"));
    home.ensure_layout().unwrap();
    let mb = pair_watcher::mailbox::Mailbox::register(&home, "s1").unwrap();
    let out = pw(t.path(), t.path(), &["primary", "--session", "s1"]);
    assert!(out.status.success(), "{out:?}");
    let records = mb.log().read_from(0).unwrap().records;
    assert_eq!(
        records.last().unwrap().body.parse::<pair_watcher::human::Act>(),
        Some(pair_watcher::human::Act::Primary)
    );
}
