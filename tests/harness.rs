use std::ffi::OsStr;

use pair_watcher::harness::{CANARY_WINDOW_MS, Canary, Harness, HookSignals, Support, Version, canary};

#[test]
fn harness_is_detected_by_file_name() {
    assert_eq!(Harness::from_program(OsStr::new("claude")), Some(Harness::Claude));
    assert_eq!(Harness::from_program(OsStr::new("/opt/homebrew/bin/codex")), Some(Harness::Codex));
    assert_eq!(Harness::from_program(OsStr::new("./bin/pi")), Some(Harness::Pi));
    assert_eq!(Harness::from_program(OsStr::new("aider")), None);
    assert_eq!(Harness::from_program(OsStr::new("claude-code")), None);
}

#[test]
fn version_is_read_from_each_harness_output() {
    assert_eq!(Version::find("2.1.291 (Claude Code)"), Some(Version(2, 1, 291)));
    assert_eq!(Version::find("codex-cli 0.160.1"), Some(Version(0, 160, 1)));
    assert_eq!(Version::find("1.0.4\n"), Some(Version(1, 0, 4)));
    assert_eq!(Version::find("v3.2.1"), Some(Version(3, 2, 1)));
    assert_eq!(Version::find("no version here 1.2"), None);
}

#[test]
fn compat_table_marks_newer_versions_untested() {
    assert_eq!(Support::of(Harness::Claude, Some(Version(2, 1, 289))), Support::Tested);
    assert_eq!(Support::of(Harness::Claude, Some(Version(2, 1, 100))), Support::Tested, "no minimum until T16");
    let newer = Support::of(Harness::Codex, Some(Version(0, 160, 1)));
    assert_eq!(newer, Support::Untested(Some(Version(0, 160, 1))));
    assert!(newer.hooks(), "V60: newer → run with hooks");
    assert_eq!(newer.note(Harness::Codex).as_deref(), Some("untested codex 0.160.1"));
    assert_eq!(Support::of(Harness::Pi, None).note(Harness::Pi).as_deref(), Some("untested pi (version unknown)"));
    assert_eq!(Support::Tested.note(Harness::Pi), None);
    let below = Support::BelowMinimum(Version(0, 1, 0));
    assert!(!below.hooks(), "V60: below minimum → observe-only");
    assert_eq!(below.note(Harness::Pi).as_deref(), Some("pi 0.1.0 below minimum → observe-only"));
}

#[test]
fn canary_flags_turn_end_without_stop_hook_after_window() {
    let w = CANARY_WINDOW_MS;
    fn s<'a>(turn_ends: &'a [u64], turn_end_hooks: &'a [u64]) -> HookSignals<'a> {
        HookSignals { turn_ends, turn_end_hooks, ..Default::default() }
    }
    assert_eq!(canary(&s(&[1_000], &[1_200]), 100_000), None, "Stop hook near the turn end");
    assert_eq!(canary(&s(&[20_000], &[20_000 - w]), 100_000), None, "hook may land first, inside the window");
    assert_eq!(canary(&s(&[1_000], &[]), 1_000 + w - 1), None, "window still open");
    assert_eq!(canary(&s(&[1_000], &[]), 1_000 + w), Some(Canary::MissingStop));
    assert_eq!(canary(&s(&[1_000, 50_000], &[1_000]), 100_000), Some(Canary::MissingStop), "every turn end needs one");
    assert_eq!(canary(&s(&[1_000], &[1_000 + w + 1]), 100_000), Some(Canary::MissingStop));
}

#[test]
fn canary_flags_bad_records_and_payloads() {
    let bad = HookSignals { bad_records: 1, ..Default::default() };
    assert_eq!(canary(&bad, 0), Some(Canary::BadRecord));
    let bad = HookSignals { bad_payloads: 2, ..Default::default() };
    assert_eq!(canary(&bad, 0), Some(Canary::BadPayload));
    assert_eq!(Canary::LABEL, "hooks silent → observe-only");
}
