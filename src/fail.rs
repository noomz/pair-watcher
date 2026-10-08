//! Fail-open rules (V86–V89, V51): failure classes, retries and wake clocks, lagging re-wakes, the
//! poison guard, and the BLIND triggers a Watcher's own wakes trip. Pure: the Watcher loop feeds
//! outcomes and the clock in and logs what comes out.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::provider::ProviderError;
use crate::status::Class;

/// Wake wall clock: retries included (V87).
pub const WAKE_CLOCK: Duration = Duration::from_secs(180);
pub const BLOCKED_CLOCK: Duration = Duration::from_secs(60);
/// Retries after the first attempt fails with a Service error (V87).
pub const RETRY_DELAYS: [Duration; 3] = [Duration::from_secs(2), Duration::from_secs(8), Duration::from_secs(30)];
/// A longer `Retry-After` ends the retries: the wake fails and the Watcher lags.
pub const RETRY_AFTER_MAX: Duration = Duration::from_secs(60);
/// Lagging re-wakes, minutes after each failed wake; the last repeats (V87).
pub const REWAKE_MIN: [u64; 4] = [1, 2, 4, 5];
/// Lagging this long is BLIND (V89).
pub const LAGGING_BLIND_MS: u64 = 10 * 60_000;
/// On a Service fallback, the primary is tried again at most this often (V51).
pub const PROBE_PRIMARY_MS: u64 = 15 * 60_000;

const MIN_MS: u64 = 60_000;

/// V86. Billing and quota exhaustion is a Key failure whatever its status.
pub fn classify(e: &ProviderError) -> Class {
    match e {
        ProviderError::NoKey(_) => Class::Key,
        ProviderError::Http { status, body, .. } => {
            let body = body.to_ascii_lowercase();
            let exhausted =
                ["insufficient_quota", "billing", "credit balance", "quota exceeded"].iter().any(|w| body.contains(w));
            match status {
                401..=403 => Class::Key,
                _ if exhausted => Class::Key,
                408 | 429 | 500..=599 => Class::Service,
                _ => Class::Slice,
            }
        }
        ProviderError::Timeout | ProviderError::Network(_) | ProviderError::Protocol(_) => Class::Service,
    }
}

/// The wait before retry `n` (0-based) after `e`, or `None` = stop: not a Service error, retries
/// spent, or a `Retry-After` past [`RETRY_AFTER_MAX`]. `jitter` in `[0, 1)` adds up to 20%.
pub fn retry_delay(n: usize, e: &ProviderError, jitter: f64) -> Option<Duration> {
    if classify(e) != Class::Service {
        return None;
    }
    let base = *RETRY_DELAYS.get(n)?;
    let wait = match e {
        ProviderError::Http { retry_after: Some(after), .. } if *after > RETRY_AFTER_MAX => return None,
        ProviderError::Http { retry_after: Some(after), .. } => base.max(*after),
        _ => base,
    };
    Some(wait.mul_f64(1.0 + 0.2 * jitter.clamp(0.0, 1.0)))
}

/// A cheap jitter source: no randomness crate for one number per retry.
pub fn jitter() -> f64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    f64::from(nanos % 1000) / 1000.0
}

/// What a [`Health`] change asks the loop to log or do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Lagging,
    LaggingEnd,
    Blind(String),
    BlindEnd,
    /// Poison guard: commit the cursor past the held slice and log it as lost coverage (V88).
    Advance,
}

/// One Watcher's failure state (Swarm: per Focus, V89).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Health {
    pub lagging_since: Option<u64>,
    /// Next lagging or held-slice re-wake; a real trigger wakes sooner (V87).
    pub rewake_at: Option<u64>,
    rewakes: usize,
    /// The held slice and its Slice failures (V88).
    slice: Option<(String, u8)>,
    /// Slices the poison guard skipped back to back.
    skipped: u8,
    pub blind: Option<(u64, String)>,
}

impl Health {
    /// An accepted wake: lagging and BLIND end (V89), the guard resets.
    pub fn accepted(&mut self) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.lagging_since.take().is_some() {
            out.push(Effect::LaggingEnd);
        }
        if self.blind.take().is_some() {
            out.push(Effect::BlindEnd);
        }
        *self = Health::default();
        out
    }

    /// A failed wake on `slice` (the key of the held slice's first entry).
    pub fn failed(&mut self, class: Class, slice: &str, error: &str, now: u64) -> Vec<Effect> {
        let mut out = Vec::new();
        match class {
            Class::Service => {
                if self.lagging_since.is_none() {
                    self.lagging_since = Some(now);
                    out.push(Effect::Lagging);
                }
                let wait = REWAKE_MIN[self.rewakes.min(REWAKE_MIN.len() - 1)];
                self.rewakes += 1;
                self.rewake_at = Some(now + wait * MIN_MS);
                out.extend(self.check(now));
            }
            Class::Slice => {
                let fails = match &self.slice {
                    Some((key, n)) if key == slice => n + 1,
                    _ => 1,
                };
                if fails >= 2 {
                    self.slice = None;
                    self.skipped += 1;
                    self.rewake_at = None;
                    out.push(Effect::Advance);
                    if self.skipped >= 2 {
                        out.extend(self.go_blind(now, "2 slices skipped back to back"));
                    }
                } else {
                    self.slice = Some((slice.to_owned(), fails));
                    self.rewake_at = Some(now + MIN_MS);
                }
            }
            Class::Key => out.extend(self.go_blind(now, &format!("model key: {error}"))),
        }
        out
    }

    /// Time-driven: lagging ≥10 min is BLIND (V89).
    pub fn check(&mut self, now: u64) -> Vec<Effect> {
        match self.lagging_since {
            Some(since) if now.saturating_sub(since) >= LAGGING_BLIND_MS => self.go_blind(now, "lagging ≥10 min"),
            _ => Vec::new(),
        }
    }

    pub fn rewake_due(&self, now: u64) -> bool {
        self.rewake_at.is_some_and(|at| now >= at)
    }

    pub fn go_blind(&mut self, now: u64, reason: &str) -> Vec<Effect> {
        if self.blind.is_some() {
            return Vec::new();
        }
        self.blind = Some((now, reason.to_owned()));
        vec![Effect::Blind(reason.to_owned())]
    }
}

/// `fallback_model` (V51): which model the next wake uses.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Fallback {
    /// Why the fallback runs; `None` = on the primary.
    pub active: Option<Class>,
    probed: u64,
}

impl Fallback {
    /// A failed wake on the primary: Key or Service switches, from the next wake; Slice never.
    /// Returns whether it switched now.
    pub fn failed(&mut self, class: Class, configured: bool, now: u64) -> bool {
        if !configured || self.active.is_some() || class == Class::Slice {
            return false;
        }
        self.active = Some(class);
        self.probed = now;
        true
    }

    /// On a Service fallback, the primary gets one attempt (no retries) once per
    /// [`PROBE_PRIMARY_MS`]. A Key fallback holds for the session.
    pub fn probe(&mut self, now: u64) -> bool {
        if self.active == Some(Class::Service) && now.saturating_sub(self.probed) >= PROBE_PRIMARY_MS {
            self.probed = now;
            return true;
        }
        false
    }

    /// The probe succeeded: back on the primary.
    pub fn recovered(&mut self) {
        self.active = None;
    }
}
