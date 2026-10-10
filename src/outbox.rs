//! `outbox.jsonl`: Challenges and Rulings bound for the Worker, delivered once via two-phase
//! claim/ack, stale-checked at claim (V28).
//!
//! Lines: `queued` → `claimed` (route, token) → `acked` | `dropped`. A claim that is never acked
//! (deliverer killed) expires after [`LEASE_MS`] and the item is claimable again, so a crash
//! never loses an item. `held` records why a deliverer is holding an item back (V26 pane
//! `pending: human draft in prompt`); it never changes what is delivered.

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
    Claimed {
        id: String,
        route: String,
        token: String,
    },
    Acked {
        id: String,
        token: String,
    },
    Dropped {
        id: String,
        reason: String,
    },
    Held {
        id: String,
        route: String,
        reason: Option<String>,
    },
    /// The claimer handed it back undelivered: claimable again at once.
    Released {
        id: String,
        token: String,
    },
}

/// A deliverer's verdict on one pending item at claim time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Deliver,
    /// Not now: it stays pending (e.g. the deliverer's view of the log may be behind).
    Skip,
    /// Stale: dropped, never delivered.
    Drop,
}

/// What [`Outbox::withdraw`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Withdraw {
    Withdrawn,
    /// Dropped before (e.g. by a claim's stale check).
    AlreadyDropped,
    /// Delivered: it stays.
    Acked,
    /// A deliverer holds a live claim: it may be on its way.
    Claimed,
    /// Never queued.
    Missing,
}

/// An item delivered by `route` (V28).
#[derive(Clone, Debug, PartialEq)]
pub struct Delivered {
    pub item: Item,
    pub route: String,
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
    /// Route of the latest claim.
    route: Option<String>,
    held: Option<String>,
    /// When each hold with a reason began.
    waits: Vec<u64>,
    done: bool,
    acked: bool,
}

impl Outbox {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Outbox { path: path.into() }
    }

    /// Queues an item; false if its id is queued, claimed or delivered. A dropped id never
    /// reached anyone and may be queued again.
    pub fn queue(&self, item: Item, now: u64) -> io::Result<bool> {
        self.transact(now, |slots| {
            if slots.iter().any(|s| s.item.id == item.id && (!s.done || s.acked)) {
                return (vec![], false);
            }
            (vec![Entry::Queued(item)], true)
        })
    }

    /// Claims the oldest item `gate` lets through for `route`. Items it judges stale are dropped
    /// on the way; items it holds back, or under a live claim, are skipped.
    pub fn claim(&self, route: &str, now: u64, mut gate: impl FnMut(&Item) -> Gate) -> io::Result<Option<Claim>> {
        let token = format!("{}.{now}", std::process::id());
        self.transact(now, |slots| {
            let mut out = Vec::new();
            for slot in slots.iter().filter(|s| !s.done) {
                if slot.claim.as_ref().is_some_and(|(_, at)| now.saturating_sub(*at) < LEASE_MS) {
                    continue;
                }
                let id = slot.item.id.clone();
                match gate(&slot.item) {
                    Gate::Deliver => {}
                    Gate::Skip => continue,
                    Gate::Drop => {
                        out.push(Entry::Dropped { id, reason: "stale".into() });
                        continue;
                    }
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

    /// Hands a claimed item back undelivered (the Worker turned busy between claim and send), so
    /// the next deliverer need not wait out the lease. False if the claim is no longer this one.
    pub fn release(&self, claim: &Claim, now: u64) -> io::Result<bool> {
        self.transact(now, |slots| match slots.iter().find(|s| s.item.id == claim.item.id) {
            Some(Slot { done: false, claim: Some((token, _)), .. }) if *token == claim.token => {
                (vec![Entry::Released { id: claim.item.id.clone(), token: claim.token.clone() }], true)
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

    /// Withdraws item `id` unless it is delivered or under a live claim, in one locked step.
    pub fn withdraw(&self, id: &str, reason: &str, now: u64) -> io::Result<Withdraw> {
        self.transact(now, |slots| match slots.iter().find(|s| s.item.id == id) {
            None => (vec![], Withdraw::Missing),
            Some(s) if s.acked => (vec![], Withdraw::Acked),
            Some(s) if s.done => (vec![], Withdraw::AlreadyDropped),
            Some(Slot { claim: Some((_, at)), .. }) if now.saturating_sub(*at) < LEASE_MS => {
                (vec![], Withdraw::Claimed)
            }
            Some(_) => (vec![Entry::Dropped { id: id.to_owned(), reason: reason.to_owned() }], Withdraw::Withdrawn),
        })
    }

    /// Records why `route` holds pending item `id` back (`None` clears it); written only when the
    /// reason changes, so a polling deliverer adds a line per change, not per poll.
    pub fn hold(&self, id: &str, route: &str, reason: Option<&str>, now: u64) -> io::Result<()> {
        // Unlocked look first: an unchanged reason, the common poll, never takes the lock.
        if self.held(id)?.as_deref() == reason {
            return Ok(());
        }
        self.transact(now, |slots| match slots.iter().find(|s| s.item.id == id && !s.done) {
            Some(s) if s.held.as_deref() != reason => {
                let held =
                    Entry::Held { id: id.to_owned(), route: route.to_owned(), reason: reason.map(str::to_owned) };
                (vec![held], ())
            }
            _ => (vec![], ()),
        })
    }

    /// Runs `f` under the outbox lock with whether item `id` is still out (pending or
    /// delivered): no claim can drop or deliver it until `f` returns.
    pub fn while_out<R>(&self, id: &str, f: impl FnOnce(bool) -> R) -> io::Result<R> {
        self.transact(0, |slots| {
            let out = slots.iter().any(|s| s.item.id == id && (!s.done || s.acked));
            (vec![], f(out))
        })
    }

    /// Why a deliverer holds pending item `id` back, if it says.
    pub fn held(&self, id: &str) -> io::Result<Option<String>> {
        let records = jsonl::read_from::<Record<Entry>>(&self.path, 0)?.records;
        Ok(fold(records).into_iter().find(|s| s.item.id == id && !s.done).and_then(|s| s.held))
    }

    /// When each hold of pending item `id` began, oldest first: the pane merges them into one
    /// `pending` line with a wait count (V65).
    pub fn waits(&self, id: &str) -> io::Result<Vec<u64>> {
        let records = jsonl::read_from::<Record<Entry>>(&self.path, 0)?.records;
        Ok(fold(records).into_iter().find(|s| s.item.id == id && !s.done).map(|s| s.waits).unwrap_or_default())
    }

    /// Acked items with the route that delivered them, oldest first.
    pub fn delivered(&self) -> io::Result<Vec<Delivered>> {
        let records = jsonl::read_from::<Record<Entry>>(&self.path, 0)?.records;
        Ok(fold(records)
            .into_iter()
            .filter(|s| s.acked)
            .map(|s| Delivered { item: s.item, route: s.route.unwrap_or_default() })
            .collect())
    }

    /// Items neither acked nor dropped, oldest first (claimed ones included).
    pub fn pending(&self) -> io::Result<Vec<Item>> {
        let records = jsonl::read_from::<Record<Entry>>(&self.path, 0)?.records;
        Ok(fold(records).into_iter().filter(|s| !s.done).map(|s| s.item).collect())
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
                let fresh = Slot {
                    item: item.clone(),
                    claim: None,
                    route: None,
                    held: None,
                    waits: Vec::new(),
                    done: false,
                    acked: false,
                };
                // A re-queued (dropped) id is new: it goes to the back, oldest-first order kept.
                match slots.iter().position(|s| s.item.id == item.id) {
                    None => slots.push(fresh),
                    Some(i) if slots[i].done && !slots[i].acked => {
                        slots.remove(i);
                        slots.push(fresh);
                    }
                    Some(_) => {}
                }
                continue;
            }
            Entry::Claimed { id, .. }
            | Entry::Acked { id, .. }
            | Entry::Dropped { id, .. }
            | Entry::Held { id, .. }
            | Entry::Released { id, .. } => id,
        };
        let Some(slot) = slots.iter_mut().find(|s| s.item.id == *id) else { continue };
        match r.body {
            Entry::Claimed { token, route, .. } => {
                slot.claim = Some((token, r.t));
                slot.route = Some(route);
                slot.held = None;
            }
            Entry::Held { reason, .. } => {
                if reason.is_some() {
                    slot.waits.push(r.t);
                }
                slot.held = reason;
            }
            Entry::Released { token, .. } => {
                if slot.claim.as_ref().is_some_and(|(t, _)| *t == token) {
                    slot.claim = None;
                }
            }
            Entry::Acked { .. } => (slot.done, slot.acked) = (true, true),
            Entry::Dropped { .. } => slot.done = true,
            Entry::Queued(_) => {}
        }
    }
    slots
}
