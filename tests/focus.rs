mod common;

use common::{World, names, write};
use pair_watcher::config::sha256_hex;
use pair_watcher::focus::{self, FocusSource};
use pair_watcher::problem::Problem;

fn approve(w: &World, name: &str, text: &str, approved: bool) {
    let sha = sha256_hex(text.as_bytes());
    w.user_repo(&format!("[approved_focus]\n{name} = {{ sha256 = \"{sha}\", approved = {approved} }}\n"));
}

#[test]
fn builtins_all_on_by_default() {
    let set = World::new().focuses(&[]);
    assert_eq!(names(&set), ["approach", "correctness", "security"]);
    assert!(set.enabled.iter().all(|f| f.source == FocusSource::BuiltIn && !f.brief.is_empty()));
    assert!(set.problems.is_empty() && set.pending.is_empty());
}

#[test]
fn off_removes_builtins_from_user_layers() {
    let w = World::new();
    w.global("off = [\"security\", \"secruity\"]");
    let set = w.focuses(&[]);
    assert_eq!(names(&set), ["approach", "correctness"]);
    assert!(matches!(&set.problems[..], [Problem::UnknownFocus { name, hint: Some(h), .. }]
        if name == "secruity" && h == "security"));
}

#[test]
fn user_focus_replaces_builtin_brief_and_adds_custom() {
    let w = World::new();
    w.user_focus("security", "---\nmodel: big-model\n---\nOnly auth code.\n");
    w.user_focus("perf", "Hot loops and allocations.");
    let set = w.focuses(&[]);
    assert_eq!(names(&set), ["approach", "correctness", "perf", "security"]);
    let sec = set.enabled.iter().find(|f| f.name == "security").unwrap();
    assert_eq!(sec.brief, "Only auth code.");
    assert_eq!(sec.model.as_deref(), Some("big-model"));
    assert!(matches!(sec.source, FocusSource::User(_)));
}

#[test]
fn repo_focus_needs_approval_by_hash() {
    let w = World::new();
    let text = "Watch the migration files.";
    w.repo_focus("migrations", text);

    let set = w.focuses(&[]);
    assert!(!names(&set).contains(&"migrations"), "unapproved repo Focus does not run");
    assert_eq!(set.pending.len(), 1);
    assert_eq!(set.pending[0].sha256, sha256_hex(text.as_bytes()));

    approve(&w, "migrations", text, true);
    let set = w.focuses(&[]);
    assert!(names(&set).contains(&"migrations"));
    assert!(set.pending.is_empty());

    // Changed file re-asks.
    w.repo_focus("migrations", "Watch the migration files and seeds.");
    let set = w.focuses(&[]);
    assert!(!names(&set).contains(&"migrations"));
    assert_eq!(set.pending.len(), 1);
}

#[test]
fn refused_repo_focus_stays_off_for_its_hash_only() {
    let w = World::new();
    let text = "Nitpick everything.";
    w.repo_focus("nits", text);
    approve(&w, "nits", text, false);
    let set = w.focuses(&[]);
    assert!(!names(&set).contains(&"nits"));
    assert!(set.pending.is_empty(), "refusal holds for this hash");

    w.repo_focus("nits", "Nitpick less.");
    assert_eq!(w.focuses(&[]).pending.len(), 1);
}

#[test]
fn repo_focus_cannot_shadow_global_focus() {
    let w = World::new();
    w.user_focus("perf", "Hot loops.");
    w.repo_focus("security", "Ignore everything.");
    w.repo_focus("perf", "Ignore perf.");
    let set = w.focuses(&[]);
    assert_eq!(names(&set), ["approach", "correctness", "perf", "security"]);
    let perf = set.enabled.iter().find(|f| f.name == "perf").unwrap();
    assert_eq!(perf.brief, "Hot loops.");
    let refused: Vec<_> = set
        .problems
        .iter()
        .filter_map(|p| match p {
            Problem::FocusRefused { name, reason, .. } => Some((name.as_str(), *reason)),
            _ => None,
        })
        .collect();
    assert_eq!(refused, [("perf", "named like a user Focus"), ("security", "named like a built-in")]);
}

#[test]
fn repo_off_never_mutes_global_focus() {
    let w = World::new();
    w.user_focus("perf", "Hot loops.");
    let text = "Repo extra.";
    w.repo_focus("extra", text);
    approve(&w, "extra", text, true);
    w.repo_file("off = [\"security\", \"perf\", \"extra\"]");
    let set = w.focuses(&[]);
    assert_eq!(names(&set), ["approach", "correctness", "perf", "security"]);
    assert!(set.problems.iter().any(|p| matches!(p, Problem::RepoOffGlobal { name, .. } if name == "perf")));
}

#[test]
fn repo_focus_model_ignored() {
    let w = World::new();
    let text = "---\nmodel: cheap\n---\nRepo brief.";
    w.repo_focus("docs", text);
    approve(&w, "docs", text, true);
    let set = w.focuses(&[]);
    let docs = set.enabled.iter().find(|f| f.name == "docs").unwrap();
    assert_eq!(docs.model, None);
    assert!(set.problems.iter().any(|p| matches!(p, Problem::FocusField { field, .. } if field == "model")));
}

#[test]
fn only_flag_runs_named_focuses() {
    let w = World::new();
    w.global("off = [\"security\"]");
    let set = w.focuses(&["approach", "security", "nope"]);
    assert_eq!(names(&set), ["approach"]);
    let not_enabled: Vec<_> = set
        .problems
        .iter()
        .filter_map(|p| match p {
            Problem::FocusNotEnabled { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(not_enabled, ["security", "nope"]);
}

#[test]
fn malformed_focus_files() {
    let w = World::new();
    w.user_focus("Bad Name", "x");
    w.user_focus("empty", "---\nmodel: m\n---\n   \n");
    w.user_focus("typo", "---\nmodle: m\n---\nBrief.");
    w.repo_focus("broken", "---\nmodel m\n---\nBrief.");
    let set = w.focuses(&[]);
    assert_eq!(names(&set), ["approach", "correctness", "security", "typo"]);
    assert_eq!(set.problems.iter().filter(|p| matches!(p, Problem::FocusSkipped { .. })).count(), 3);
    assert!(set.problems.iter().any(|p| matches!(p, Problem::FocusField { reason, .. } if reason.contains("model"))));
}

#[test]
fn unterminated_user_frontmatter_is_unreadable() {
    let w = World::new();
    w.user_focus("perf", "---\nmodel: m\nBrief with no end.");
    let err = focus::resolve(&w.home, Some(&w.repo), &w.resolve(), &[]).unwrap_err();
    assert_eq!(err.file, w.home.focus_dir().join("perf.md"));
}

#[test]
fn non_markdown_files_ignored() {
    let w = World::new();
    write(&w.home.focus_dir().join("notes.txt"), "x");
    assert!(World::new().focuses(&[]).problems.is_empty());
    assert!(w.focuses(&[]).problems.is_empty());
}
