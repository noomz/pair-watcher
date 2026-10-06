use pair_watcher::harness::Harness;
use pair_watcher::trigger::{CHECKPOINT_FLOOR_MS, CODEX_BLOCKED_MS, Due, Signal, Trigger, Triggers, signal};
use serde_json::{Value, json};

const NOW: u64 = 10_000_000;

fn kinds(due: &[Due]) -> Vec<Trigger> {
    due.iter().map(|d| d.trigger).collect()
}

fn never() -> bool {
    panic!("repo_changed must not be asked")
}

#[test]
fn hook_events_map_per_harness() {
    assert_eq!(signal(Harness::Claude, "Stop"), Some(Signal::TurnEnd));
    assert_eq!(signal(Harness::Codex, "Stop"), Some(Signal::TurnEnd));
    assert_eq!(signal(Harness::Pi, "agent_settled"), Some(Signal::TurnEnd));
    assert_eq!(signal(Harness::Claude, "PermissionRequest"), Some(Signal::Blocked));
    assert_eq!(signal(Harness::Pi, "herdr:blocked"), Some(Signal::Blocked));
    assert_eq!(signal(Harness::Codex, "PermissionRequest"), Some(Signal::MaybeBlocked));
    assert_eq!(signal(Harness::Codex, "PreToolUse"), Some(Signal::Activity));
    assert_eq!(signal(Harness::Codex, "PostToolUse"), Some(Signal::Activity));
    assert_eq!(signal(Harness::Claude, "SessionStart"), None);
    assert_eq!(signal(Harness::Claude, "Notification"), None, "V56: not Notification");
}

/// V1: turn end wakes at once, under the floor, without snapshotting.
#[test]
fn turn_end_bypasses_floor() {
    let mut t = Triggers::default();
    t.observe(NOW, Signal::TurnEnd, &Value::Null);
    assert_eq!(kinds(&t.due(NOW, NOW - 1_000, true, never)), [Trigger::TurnEnd]);
}

/// V1: blocked bypasses the floor, carries its payload, and comes before the normal wake.
#[test]
fn blocked_bypasses_floor_and_goes_first() {
    let mut t = Triggers::default();
    t.observe(NOW, Signal::TurnEnd, &Value::Null);
    t.observe(NOW, Signal::Blocked, &json!({"tool_name": "AskUserQuestion"}));
    let due = t.due(NOW, NOW - 1_000, true, never);
    assert_eq!(kinds(&due), [Trigger::Blocked, Trigger::TurnEnd]);
    assert_eq!(due[0].payload, Some(json!({"tool_name": "AskUserQuestion"})));
}

/// V1: a checkpoint needs ≥3 min since the last wake AND a repo change.
#[test]
fn checkpoint_needs_floor_and_change() {
    let mut t = Triggers::default();
    assert!(t.due(NOW, NOW - CHECKPOINT_FLOOR_MS + 1, true, never).is_empty());
    assert!(t.due(NOW, NOW - CHECKPOINT_FLOOR_MS, true, || false).is_empty());
    assert_eq!(kinds(&t.due(NOW, NOW - CHECKPOINT_FLOOR_MS, true, || true)), [Trigger::Checkpoint]);
}

/// V1: `checkpoints = false` drops checkpoints only.
#[test]
fn checkpoints_off_keeps_turn_end_and_blocked() {
    let mut t = Triggers::default();
    assert!(t.due(NOW, 0, false, never).is_empty());
    t.observe(NOW, Signal::TurnEnd, &Value::Null);
    t.observe(NOW, Signal::Blocked, &Value::Null);
    assert_eq!(kinds(&t.due(NOW, 0, false, never)), [Trigger::Blocked, Trigger::TurnEnd]);
}

/// V1: a burst coalesces to one wake; the turn end subsumes a checkpoint.
#[test]
fn burst_coalesces() {
    let mut t = Triggers::default();
    for i in 0..5 {
        t.observe(NOW + i, Signal::TurnEnd, &Value::Null);
    }
    assert_eq!(kinds(&t.due(NOW, 0, true, never)), [Trigger::TurnEnd]);
    assert!(t.due(NOW, NOW, true, never).is_empty());
}

/// V56: Codex `PermissionRequest` is blocked only after 10 s with no next hook event.
#[test]
fn codex_permission_request_waits_for_quiet() {
    let mut t = Triggers::default();
    t.observe(NOW, Signal::MaybeBlocked, &json!({"q": 1}));
    assert!(t.due(NOW + CODEX_BLOCKED_MS - 1, NOW, true, || false).is_empty());
    let due = t.due(NOW + CODEX_BLOCKED_MS, NOW, true, || false);
    assert_eq!(kinds(&due), [Trigger::Blocked]);
    assert_eq!(due[0].payload, Some(json!({"q": 1})));
    assert!(t.due(NOW + 2 * CODEX_BLOCKED_MS, NOW, true, || false).is_empty(), "fires once");
}

/// V56: a tool hook or guardian review start cancels the timer; `Stop` cancels it and wakes.
#[test]
fn codex_activity_cancels_blocked_timer() {
    for (cancel, want) in [(Signal::Activity, vec![]), (Signal::TurnEnd, vec![Trigger::TurnEnd])] {
        let mut t = Triggers::default();
        t.observe(NOW, Signal::MaybeBlocked, &Value::Null);
        t.observe(NOW + 1_000, cancel, &Value::Null);
        assert_eq!(kinds(&t.due(NOW + CODEX_BLOCKED_MS * 2, NOW, true, || false)), want);
    }
}
