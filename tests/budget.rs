use pair_watcher::budget::{Budget, Need, Stage, Status, usd};

fn at(spent: f64) -> Budget {
    let mut b = Budget::new(5.0);
    b.record(spent);
    b
}

/// V48: 60% checkpoints off, 75% Swarm collapses, 90% normal wakes stop, 100% all calls stop.
#[test]
fn ladder_steps_at_fixed_shares() {
    let cases = [
        (0.0, Stage::Watching),
        (2.99, Stage::Watching),
        (3.0, Stage::Thinned),
        (3.75, Stage::Collapsed),
        (4.5, Stage::Reserve),
        (5.0, Stage::Paused),
        (7.0, Stage::Paused),
    ];
    for (spent, stage) in cases {
        assert_eq!(at(spent).stage(), stage, "spent {spent}");
    }
}

#[test]
fn admission_per_need() {
    let admits = |spent, need| at(spent).admits(need);
    assert!(admits(2.9, Need::Checkpoint) && !admits(3.0, Need::Checkpoint));
    assert!(admits(4.49, Need::Normal) && !admits(4.5, Need::Normal));
    // The reserve: blocked wakes and an open Debate run to the ceiling, never past it.
    assert!(admits(4.99, Need::Reserve) && !admits(5.0, Need::Reserve));
    assert!(!at(3.7).swarm_collapsed() && at(3.75).swarm_collapsed());
}

/// V49 words; V50 `+$5` resumes.
#[test]
fn status_and_raise() {
    assert_eq!(at(1.0).status().to_string(), "watching");
    assert_eq!(at(3.2).status().to_string(), "thinned");
    assert_eq!(at(4.7).status().to_string(), "thinned");
    let mut b = at(5.0);
    b.record(-1.0);
    assert_eq!(b.status().to_string(), "paused $5/$5", "negative spend ignored");
    let mut b = at(4.52);
    b.record(0.5);
    assert_eq!(b.status().to_string(), "paused $5.02/$5");
    b.raise();
    assert_eq!(b.ceiling(), 10.0);
    assert!(b.admits(Need::Checkpoint) && b.status() == Status::Watching);
    assert!((b.left() - 4.98).abs() < 1e-9);

    assert_eq!(Status::PausedByYou.to_string(), "PAUSED (you)");
    assert_eq!(Status::Blind.to_string(), "BLIND");
    assert_eq!(Status::Lagging.to_string(), "lagging");
    assert_eq!(usd(4.5), "$4.50");
}

#[test]
fn zero_ceiling_stops_everything() {
    let b = Budget::new(0.0);
    assert_eq!(b.stage(), Stage::Paused);
    assert!(!b.admits(Need::Reserve));
}
