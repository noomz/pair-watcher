//! §T15 fail-open rules: failure classes (V86), retries + wake clock (V87), poison guard (V88),
//! lagging / BLIND (V89), `fallback_model` (V51).

use std::time::Duration;

use pair_watcher::fail::{self, Effect, Fallback, Health, LAGGING_BLIND_MS, PROBE_PRIMARY_MS};
use pair_watcher::models::Vendor;
use pair_watcher::provider::ProviderError;
use pair_watcher::status::Class;

const MIN: u64 = 60_000;

fn http(status: u16, body: &str) -> ProviderError {
    ProviderError::Http { status, retry_after: None, body: body.into() }
}

/// V86: Service, Slice and Key by status and body.
#[test]
fn classes() {
    for s in [429, 500, 502, 503, 529, 408] {
        assert_eq!(fail::classify(&http(s, "overloaded")), Class::Service, "{s}");
    }
    assert_eq!(fail::classify(&ProviderError::Timeout(String::new())), Class::Service);
    assert_eq!(fail::classify(&ProviderError::Network("reset".into())), Class::Service);
    assert_eq!(fail::classify(&http(400, "prompt is too long: context window exceeded")), Class::Slice);
    assert_eq!(fail::classify(&http(413, "")), Class::Slice);
    for s in [401, 403] {
        assert_eq!(fail::classify(&http(s, "invalid x-api-key")), Class::Key);
    }
    assert_eq!(fail::classify(&http(429, r#"{"error":{"code":"insufficient_quota"}}"#)), Class::Key);
    assert_eq!(fail::classify(&http(400, "Your credit balance is too low")), Class::Key);
    assert_eq!(fail::classify(&ProviderError::NoKey(Vendor::Openai)), Class::Key);
}

/// V87: 2 s / 8 s / 30 s + jitter, then stop; Retry-After ≤60 s honoured, longer ends retries;
/// nothing but Service retries.
#[test]
fn retries() {
    let e = http(503, "");
    assert_eq!(fail::retry_delay(0, &e, 0.0), Some(Duration::from_secs(2)));
    assert_eq!(fail::retry_delay(1, &e, 0.0), Some(Duration::from_secs(8)));
    assert_eq!(fail::retry_delay(2, &e, 0.0), Some(Duration::from_secs(30)));
    assert_eq!(fail::retry_delay(3, &e, 0.0), None);
    let j = fail::retry_delay(0, &e, 0.99).unwrap();
    assert!(j > Duration::from_secs(2) && j < Duration::from_millis(2400), "{j:?}");
    let after = |s| ProviderError::Http { status: 429, retry_after: Some(Duration::from_secs(s)), body: String::new() };
    assert_eq!(fail::retry_delay(0, &after(45), 0.0), Some(Duration::from_secs(45)));
    assert_eq!(fail::retry_delay(0, &after(61), 0.0), None);
    assert_eq!(fail::retry_delay(0, &http(400, ""), 0.0), None);
    assert_eq!(fail::retry_delay(0, &http(401, ""), 0.0), None);
}

/// V86, V87, V89: a Service failure lags with re-wakes 1 → 2 → 4 → 5 min, BLIND at 10 min;
/// an accepted wake ends both.
#[test]
fn service_lags_then_blind() {
    let mut h = Health::default();
    assert_eq!(h.failed(Class::Service, "s", "503", 0), [Effect::Lagging]);
    assert_eq!(h.rewake_at, Some(MIN));
    assert!(!h.rewake_due(MIN - 1) && h.rewake_due(MIN));
    let mut t = MIN;
    for wait in [2, 4] {
        assert!(h.failed(Class::Service, "s", "503", t).is_empty());
        assert_eq!(h.rewake_at, Some(t + wait * MIN));
        t += wait * MIN;
    }
    assert_eq!(t, 7 * MIN);
    assert!(h.failed(Class::Service, "s", "503", t).is_empty());
    assert_eq!(h.rewake_at, Some(12 * MIN), "the 5 min cap");
    assert!(h.check(LAGGING_BLIND_MS - 1).is_empty());
    assert_eq!(h.check(LAGGING_BLIND_MS), [Effect::Blind("lagging ≥10 min".into())]);
    assert!(h.check(LAGGING_BLIND_MS + MIN).is_empty(), "BLIND starts once");
    assert_eq!(h.accepted(), [Effect::LaggingEnd, Effect::BlindEnd]);
    assert_eq!(h, Health::default());
}

/// V88: two Slice failures on one slice advance past it; two skipped back to back are BLIND. A
/// failure on another slice starts the count again; Service failures never count.
#[test]
fn poison_guard() {
    let mut h = Health::default();
    assert!(h.failed(Class::Slice, "a", "400", 0).is_empty());
    assert_eq!(h.rewake_at, Some(MIN), "the held slice is retried");
    assert_eq!(h.failed(Class::Service, "a", "503", 1), [Effect::Lagging]);
    assert_eq!(h.failed(Class::Slice, "a", "400", 2), [Effect::Advance]);
    assert!(h.failed(Class::Slice, "b", "400", 3).is_empty());
    assert!(h.failed(Class::Slice, "c", "400", 4).is_empty(), "new slice, count restarts");
    assert_eq!(
        h.failed(Class::Slice, "c", "400", 5),
        [Effect::Advance, Effect::Blind("2 slices skipped back to back".into())]
    );

    let mut h = Health::default();
    h.failed(Class::Slice, "a", "400", 0);
    h.failed(Class::Slice, "a", "400", 1);
    h.accepted();
    h.failed(Class::Slice, "b", "400", 2);
    assert_eq!(h.failed(Class::Slice, "b", "400", 3), [Effect::Advance], "an accepted slice between resets");
}

/// V86: Key is BLIND at once.
#[test]
fn key_is_blind_at_once() {
    let mut h = Health::default();
    assert_eq!(h.failed(Class::Key, "a", "HTTP 401", 0), [Effect::Blind("model key: HTTP 401".into())]);
}

/// V51: Key or Service switches once configured, Slice never; a Service fallback probes the
/// primary once per 15 min, a Key fallback never.
#[test]
fn fallback() {
    let mut f = Fallback::default();
    assert!(!f.failed(Class::Service, false, 0), "not configured");
    assert!(!f.failed(Class::Slice, true, 0));
    assert!(f.failed(Class::Service, true, 0));
    assert!(!f.failed(Class::Key, true, 1), "already switched");
    assert!(!f.probe(PROBE_PRIMARY_MS - 1));
    assert!(f.probe(PROBE_PRIMARY_MS));
    assert!(!f.probe(PROBE_PRIMARY_MS + 1), "≤1× per 15 min");
    f.recovered();
    assert_eq!(f.active, None);

    let mut f = Fallback::default();
    assert!(f.failed(Class::Key, true, 0));
    assert!(!f.probe(10 * PROBE_PRIMARY_MS), "a Key switch holds for the session");
}
