//! §T14 Standing rulings store (V78): add / remove, ids, the rulings hash, the wake section, hand
//! edits seen at session start, an unreadable file, and the pane's `r` overlay with `x`.

use std::fs;
use std::path::PathBuf;

use pair_watcher::event::{Event, Kind};
use pair_watcher::home::Home;
use pair_watcher::human::Act;
use pair_watcher::layout::{Overlay, Ui, layout};
use pair_watcher::mailbox::Mailbox;
use pair_watcher::pane::{self, Board, Effect};
use pair_watcher::ruling::{Keep, Side, Via};
use pair_watcher::standing::{self, Standing};
use pair_watcher::term::Key;
use tempfile::TempDir;

fn store() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("repos").join("abc.rulings.toml");
    (dir, path)
}

fn keep(headline: &str, side: Side, reason: Option<&str>) -> Keep {
    Keep {
        concern: "c1".into(),
        focus: "approach".into(),
        headline: headline.into(),
        side,
        reason: reason.map(Into::into),
    }
}

fn ids(held: &[Standing]) -> Vec<&str> {
    held.iter().map(|r| r.id.as_str()).collect()
}

/// `x` / `rulings rm` removes one by id; an unknown id is refused; ids are never reused.
#[test]
fn remove_by_id_and_ids_never_reused() {
    let (_d, path) = store();
    for h in ["a", "b", "c"] {
        standing::remember(&path, &keep(h, Side::Worker, None), "2026-10-08", true).unwrap().unwrap();
    }
    let gone = standing::remove(&path, "r2", true).unwrap().unwrap();
    assert_eq!(gone.headline, "b");
    assert_eq!(ids(&standing::load(&path).unwrap()), ["r1", "r3"]);
    let why = standing::remove(&path, "r2", true).unwrap().unwrap_err();
    assert!(why.contains("no Standing ruling r2") && why.contains("r1 r3"), "{why}");
    // The highest removed (r3): the next is r4, not r3 again.
    standing::remove(&path, "r3", true).unwrap().unwrap();
    let id = standing::remember(&path, &keep("d", Side::Dismissed, None), "2026-10-08", true).unwrap().unwrap();
    assert_eq!(id, "r4");
}

/// V84: the rulings hash follows what the rulings say, not the file's layout.
#[test]
fn hash_tracks_content_not_layout() {
    let (_d, path) = store();
    standing::remember(&path, &keep("a", Side::Watcher, Some("why")), "2026-10-08", true).unwrap().unwrap();
    let before = standing::hash(&standing::load(&path).unwrap());
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("# my notes\n\n{}\n\n", text.replace(" = ", "="))).unwrap();
    assert_eq!(standing::hash(&standing::load(&path).unwrap()), before);
    fs::write(&path, text.replace("why", "because")).unwrap();
    assert_ne!(standing::hash(&standing::load(&path).unwrap()), before);
    assert_ne!(standing::hash(&[]), before);
}

/// V7, V78: every ruling enters wake input with id, Focus, headline, verdict, reason, date.
#[test]
fn wake_section_lists_every_ruling() {
    assert_eq!(standing::render(&[]), "");
    let (_d, path) = store();
    standing::remember(
        &path,
        &keep("5th review round", Side::Worker, Some("reviewbot gates merge")),
        "2026-10-08",
        true,
    )
    .unwrap()
    .unwrap();
    standing::remember(&path, &keep("secrets in .env", Side::Watcher, None), "2026-10-09", true).unwrap().unwrap();
    let section = standing::render(&standing::load(&path).unwrap());
    let lines: Vec<&str> = section.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("held back by ruling <id>") && lines[0].contains("never lowers the Challenge bar"));
    assert_eq!(
        lines[1],
        "r1 · 2026-10-08 · approach · sided with the Worker: 5th review round — reviewbot gates merge"
    );
    assert_eq!(lines[2], "r2 · 2026-10-09 · approach · sided with the Watcher: secrets in .env");
}

/// V78 "file edits seen at session start": the first sight is the baseline; logged tool changes
/// are not edits; hand edits are named once.
#[test]
fn session_start_names_hand_edits_once() {
    let (_d, path) = store();
    let first = standing::start(&path).unwrap();
    assert_eq!((first.held.len(), first.edited.clone()), (0, None));
    assert_eq!(first.hash, Some(standing::hash(&[])));

    for h in ["a", "b", "c"] {
        standing::remember(&path, &keep(h, Side::Worker, None), "2026-10-08", true).unwrap().unwrap();
    }
    standing::remove(&path, "r3", true).unwrap().unwrap();
    let logged = standing::start(&path).unwrap();
    assert_eq!((ids(&logged.held), logged.edited), (vec!["r1", "r2"], None), "logged changes are not edits");

    // By hand: drop r1, reword r2, add r7.
    let text = fs::read_to_string(&path).unwrap();
    let mut held: Vec<Standing> = standing::load(&path).unwrap();
    held.remove(0);
    held[0].headline = "b, reworded".into();
    held.push(Standing { id: "r7".into(), ..held[0].clone() });
    let edited =
        toml::to_string(&toml::Table::from_iter([("ruling".to_owned(), toml::Value::try_from(&held).unwrap())]))
            .unwrap();
    assert_ne!(edited, text);
    fs::write(&path, edited).unwrap();
    let start = standing::start(&path).unwrap();
    assert_eq!(
        start.edited,
        Some(Act::RulingsEdited { added: vec!["r7".into()], removed: vec!["r1".into()], changed: vec!["r2".into()] })
    );
    assert_eq!(start.hash, Some(standing::hash(&held)));
    assert_eq!(standing::start(&path).unwrap().edited, None, "named once");

    // A removal no session logged (`rulings rm` with none live) is seen at the next start.
    standing::remove(&path, "r2", false).unwrap().unwrap();
    assert_eq!(
        standing::start(&path).unwrap().edited,
        Some(Act::RulingsEdited { added: vec![], removed: vec!["r2".into()], changed: vec![] })
    );
}

/// A broken hand edit loads no rulings, says so, and is never overwritten.
#[test]
fn unreadable_store_is_reported_and_kept() {
    let (_d, path) = store();
    standing::remember(&path, &keep("a", Side::Worker, None), "2026-10-08", true).unwrap().unwrap();
    fs::write(&path, "[[ruling]\nid = ").unwrap();
    let start = standing::start(&path).unwrap();
    assert!(start.held.is_empty() && start.hash.is_none() && start.edited.is_none());
    assert!(start.notes[0].starts_with("rulings unreadable:"), "{:?}", start.notes);
    assert!(standing::remember(&path, &keep("b", Side::Worker, None), "2026-10-08", true).is_err());
    assert!(standing::remove(&path, "r1", true).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "[[ruling]\nid = ");
}

/// I17, V78: `r` overlays the rulings, `x` removes the selected one, item keys wait; the header
/// count follows removes, and a hand edit gets one ticker line.
#[test]
fn pane_r_overlay_and_x_remove() {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path().join(".pair-watcher"));
    home.ensure_layout().unwrap();
    let mb = Mailbox::register(&home, "s1").unwrap();
    let log = |e: Event| mb.log().append(e).unwrap();
    log(Event::new(Kind::Session).with("phase", "start").with("rulings", 2));
    log(Act::RulingsEdited { added: vec![], removed: vec!["r3".into()], changed: vec![] }.event());
    let board = || Board::rebuild(0, &mb.log().read_from(0).unwrap().records);

    let path = standing::path(&home, "abc");
    standing::start(&path).unwrap();
    for h in ["first headline", "second headline"] {
        standing::remember(&path, &keep(h, Side::Worker, Some("a reason")), "2026-10-08", true).unwrap().unwrap();
    }
    let mut b = board();
    assert_eq!(b.rulings_held, 2, "a hand edit is already in the start count");
    assert!(b.ticker.last().unwrap().text.contains("edited by hand: −r3"));
    b.held = standing::load(&path).unwrap();

    let mut ui = Ui::default();
    let frame = layout(&b, &ui, 64, 30);
    assert!(frame.plain().iter().any(|l| l.contains("rulings 2")));
    let frame = layout(&b, &ui, 64, 30);
    assert_eq!(pane::key(&b, &mut ui, &frame, Key::Char('x'), 0), Effect::None);
    assert!(ui.note.as_deref().unwrap().contains("open them with r"));

    assert_eq!(pane::key(&b, &mut ui, &frame, Key::Char('r'), 0), Effect::None);
    assert_eq!(ui.overlay, Overlay::Rulings);
    for width in [48, 64, 96] {
        let text = layout(&b, &ui, width, 30).plain();
        assert!(text.iter().any(|l| l.starts_with("RULINGS 2")), "{width}: {text:#?}");
        assert!(text.iter().any(|l| l.contains("›r1 2026-10-08 approach · sided with the Worker")), "{text:#?}");
        assert!(text.iter().any(|l| l.contains("second headline")) && text.iter().any(|l| l.contains("— a reason")));
    }
    let frame = layout(&b, &ui, 64, 30);
    assert_eq!(pane::key(&b, &mut ui, &frame, Key::Char('+'), 0), Effect::None, "no item keys over rulings");
    pane::key(&b, &mut ui, &frame, Key::Char('j'), 0);
    let frame = layout(&b, &ui, 64, 30);
    assert_eq!(pane::key(&b, &mut ui, &frame, Key::Char('x'), 0), Effect::Remove("r2".into()));

    log(pane::remove(&path, "r2", Via::Key).unwrap());
    assert!(pane::remove(&path, "r2", Via::Key).unwrap_err().contains("no Standing ruling r2"));
    let b = board();
    assert_eq!(b.rulings_held, 1);
    assert_eq!(b.ticker.last().unwrap().text, "Standing ruling r2 removed");
    let removed = mb.log().read_from(0).unwrap().records.last().unwrap().body.clone();
    assert_eq!(removed.parse::<Act>(), Some(Act::RulingRemoved { ruling: "r2".into(), via: Via::Key }));
    assert_eq!(ids(&standing::load(&path).unwrap()), ["r1"]);
    // Removed through the pane (logged): the next session start sees no edit.
    assert_eq!(standing::start(&path).unwrap().edited, None);
}
