//! Worker harnesses: detection, version probe, compat table, hook canary (V60, C5).

use std::ffi::OsStr;
use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
    Pi,
}

impl Harness {
    /// By the program's file name, so a full path or a wrapper dir still resolves.
    pub fn from_program(program: &OsStr) -> Option<Harness> {
        match Path::new(program).file_name()?.to_str()? {
            "claude" => Some(Harness::Claude),
            "codex" => Some(Harness::Codex),
            "pi" => Some(Harness::Pi),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
        }
    }

    /// The vendor whose models the Worker runs; pi runs any.
    pub fn vendor(self) -> Option<crate::models::Vendor> {
        match self {
            Harness::Claude => Some(crate::models::Vendor::Anthropic),
            Harness::Codex => Some(crate::models::Vendor::Openai),
            Harness::Pi => None,
        }
    }

    pub const ALL: [Harness; 3] = [Harness::Claude, Harness::Codex, Harness::Pi];

    /// Minimum and last-tested version (C5). Minimum = first release with every surface the
    /// adapters use: Claude mods (2.1.287), `codex queue` (0.149.0), pi `agent_settled` (0.80.4).
    pub fn compat(self) -> (Version, Version) {
        match self {
            Harness::Claude => (Version(2, 1, 287), Version(2, 1, 289)),
            Harness::Codex => (Version(0, 149, 0), Version(0, 160, 0)),
            Harness::Pi => (Version(0, 80, 4), Version(1, 0, 2)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// First `x.y.z` token in `--version` output (`2.1.291 (Claude Code)`, `codex-cli 0.160.1`).
    pub fn find(text: &str) -> Option<Version> {
        text.split(|c: char| c.is_whitespace() || c == '(' || c == ')').find_map(|word| {
            let word = word.trim_start_matches('v');
            let mut parts = word.split('.').map(str::parse::<u32>);
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(Ok(a)), Some(Ok(b)), Some(Ok(c)), None) => Some(Version(a, b, c)),
                _ => None,
            }
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// `<program> --version`; `None` when it fails or prints no version.
pub fn probe(program: &OsStr) -> Option<Version> {
    let out = Command::new(program).arg("--version").stdin(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Version::find(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| Version::find(&String::from_utf8_lossy(&out.stderr)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Tested,
    /// Newer than last-tested, or the version could not be read: run with hooks, pane notes it.
    Untested(Option<Version>),
    /// Below minimum: observe-only, no hook flags.
    BelowMinimum(Version),
}

impl Support {
    pub fn of(harness: Harness, version: Option<Version>) -> Support {
        let (minimum, tested) = harness.compat();
        match version {
            None => Support::Untested(None),
            Some(v) if v < minimum => Support::BelowMinimum(v),
            Some(v) if v > tested => Support::Untested(Some(v)),
            Some(_) => Support::Tested,
        }
    }

    pub fn hooks(self) -> bool {
        !matches!(self, Support::BelowMinimum(_))
    }

    /// Pane note, if any.
    pub fn note(self, harness: Harness) -> Option<String> {
        let h = harness.name();
        match self {
            Support::Tested => None,
            Support::Untested(Some(v)) => Some(format!("untested {h} {v}")),
            Support::Untested(None) => Some(format!("untested {h} (version unknown)")),
            Support::BelowMinimum(v) => Some(format!("{h} {v} below minimum → observe-only")),
        }
    }
}

/// How long a tailer turn end may go without a turn-end hook, either side (V60).
pub const CANARY_WINDOW_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Canary {
    /// Tailer saw a turn end with no turn-end hook (`Stop`, pi `agent_settled`) within the window.
    MissingStop,
    /// A hook record failed to parse or carries an unknown schema major.
    BadRecord,
    /// A hook payload was not JSON.
    BadPayload,
}

impl Canary {
    pub const LABEL: &'static str = "hooks silent → observe-only";
}

/// What the Watcher has seen so far, arrival times in unix ms.
#[derive(Clone, Debug, Default)]
pub struct HookSignals<'a> {
    pub turn_ends: &'a [u64],
    pub turn_end_hooks: &'a [u64],
    pub bad_records: usize,
    pub bad_payloads: usize,
}

/// First canary failure, if any. A turn end is judged only once its window has passed.
pub fn canary(s: &HookSignals, now: u64) -> Option<Canary> {
    if s.bad_records > 0 {
        return Some(Canary::BadRecord);
    }
    if s.bad_payloads > 0 {
        return Some(Canary::BadPayload);
    }
    let missing = s
        .turn_ends
        .iter()
        .any(|&t| now >= t + CANARY_WINDOW_MS && !s.turn_end_hooks.iter().any(|&h| h.abs_diff(t) <= CANARY_WINDOW_MS));
    missing.then_some(Canary::MissingStop)
}
