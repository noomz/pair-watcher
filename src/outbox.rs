//! `outbox.jsonl`: Challenges and Rulings bound for the Worker, delivered once via two-phase
//! claim/ack, stale-checked at claim (V28).
//!
//! Lines: `queued` → `claimed` (route, token) → `acked` | `dropped`. A claim that is never acked
//! (deliverer killed) expires after [`LEASE_MS`] and the item is claimable again, so a crash
//! never loses an item.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::jsonl::{self, Record};

pub const LEASE_MS: u64 = 30_000;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ItemKind {
    Challenge,
    Ruling,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,
    pub kind: ItemKind,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "k", rename_all = "kebab-case")]
enum Entry {
    Queued(Item),
    Claimed { id: String, route: String, token: String },
    Acked { id: String, token: String },
    Dropped { id: String, reason: String },
}

/// Held by whoever claimed an item until it acks.
#[derive(Clone, Debug, PartialEq)]
pub struct Claim {
    pub item: Item,
    pub token: String,
}

#[derive(Clone, Debug)]
pub struct Outbox {
    path: PathBuf,
}

struct Slot {
    item: Item,
    /// Latest claim: token + claim time.
    claim: Option<(String, u64)>,
    done: bool,
    acked: bool,
}

impl Outbox {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Outbox { path: path.into() }
    }

    /// Queues an item; false if its id was queued before.
    pub fn queue(&self, item: Item, now: u64) -> io::Result<bool> {
        self.transact(now, |slots| {
            if slots.iter().any(|s| s.item.id == item.id) {
                return (vec![], false);
            }
            (vec![Entry::Queued(item)], true)
        })
    }

    /// Claims the oldest deliverable item for `route`. Items `stale` judges stale are dropped
    /// on the way; items under a live claim are skipped.
    pub fn claim(&self, route: &str, now: u64, mut stale: impl FnMut(&Item) -> bool) -> io::Result<Option<Claim>> {
        let token = format!("{}.{now}", std::process::id());
        self.transact(now, |slots| {
            let mut out = Vec::new();
            for slot in slots.iter().filter(|s| !s.done) {
                if slot.claim.as_ref().is_some_and(|(_, at)| now.saturating_sub(*at) < LEASE_MS) {
                    continue;
                }
                let id = slot.item.id.clone();
                if stale(&slot.item) {
                    out.push(Entry::Dropped { id, reason: "stale".into() });
                    continue;
                }
                out.push(Entry::Claimed { id, route: route.to_owned(), token: token.clone() });
                return (out, Some(Claim { item: slot.item.clone(), token }));
            }
            (out, None)
        })
    }

    /// Marks a claimed item delivered. False if it is already done or was reclaimed by another
    /// deliverer after the lease ran out.
    pub fn ack(&self, claim: &Claim, now: u64) -> io::Result<bool> {
        self.transact(now, |slots| match slots.iter().find(|s| s.item.id == claim.item.id) {
            Some(Slot { done: false, claim: Some((token, _)), .. }) if *token == claim.token => {
                (vec![Entry::Acked { id: claim.item.id.clone(), token: claim.token.clone() }], true)
            }
            _ => (vec![], false),
        })
    }

    /// Withdraws an undelivered item (e.g. `superseded`); false if unknown or already done.
    pub fn drop_item(&self, id: &str, reason: &str, now: u64) -> io::Result<bool> {
        self.transact(now, |slots| {
            if slots.iter().any(|s| s.item.id == id && !s.done) {
                return (vec![Entry::Dropped { id: id.to_owned(), reason: reason.to_owned() }], true);
            }
            (vec![], false)
        })
    }

    /// Items neither acked nor dropped, oldest first (claimed ones included).
    pub fn pending(&self) -> io::Result<Vec<Item>> {
        let records = jsonl::read_from::<Record<Entry>>(&self.path, 0)?.records;
        Ok(fold(records).into_iter().filter(|s| !s.done).map(|s| s.item).collect())
    }

    /// True once item `id` was delivered (acked).
    pub fn acked(&self, id: &str) -> io::Result<bool> {
        let records = jsonl::read_from::<Record<Entry>>(&self.path, 0)?.records;
        Ok(fold(records).iter().any(|s| s.item.id == id && s.acked))
    }

    /// Read the whole outbox and append `f`'s entries under one exclusive lock.
    fn transact<R>(&self, now: u64, f: impl FnOnce(&[Slot]) -> (Vec<Entry>, R)) -> io::Result<R> {
        let file = jsonl::open_append(&self.path)?;
        file.lock()?;
        let records = jsonl::read_from::<Record<Entry>>(&self.path, 0)?.records;
        let (entries, result) = f(&fold(records));
        for entry in entries {
            jsonl::write_line(&file, &Record::at(now, entry))?;
        }
        Ok(result)
    }
}

fn fold(records: Vec<Record<Entry>>) -> Vec<Slot> {
    let mut slots: Vec<Slot> = Vec::new();
    for r in records {
        let id = match &r.body {
            Entry::Queued(item) => {
                if !slots.iter().any(|s| s.item.id == item.id) {
                    slots.push(Slot { item: item.clone(), claim: None, done: false, acked: false });
                }
                continue;
            }
            Entry::Claimed { id, .. } | Entry::Acked { id, .. } | Entry::Dropped { id, .. } => id,
        };
        let Some(slot) = slots.iter_mut().find(|s| s.item.id == *id) else { continue };
        match r.body {
            Entry::Claimed { token, .. } => slot.claim = Some((token, r.t)),
            Entry::Acked { .. } => (slot.done, slot.acked) = (true, true),
            Entry::Dropped { .. } => slot.done = true,
            Entry::Queued(_) => {}
        }
    }
    slots
}
