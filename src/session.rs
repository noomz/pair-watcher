//! `claude-session` backend (I15, V106–V108): Watcher completions run through the Claude Worker
//! session's own client and credentials, by the Claude mod's `$.model.complete` (I8). No key, no
//! subprocess. The Watcher writes a request to the session mailbox; the mod claims it, runs it and
//! writes the result. `$.model.complete` is text only, so the read-tool loop is code-owned here:
//! the model asks for tools in its reply, this module serves them from the snapshot and the next
//! completion carries the results (V8). A blocked wake is one completion, no tools (V11).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use std::{fmt::Write as _, io};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::jsonl::{self, Record, now_ms};
use crate::models::Usage;
use crate::provider::{Api, Call, Failure, ProviderError, Reply};
use crate::tools::{self, Allowance, MAX_CALLS, MAX_READ};

pub const FILE: &str = "completions.jsonl";
/// A request no mod claimed this long: the mod is not loaded or the session closed (V108).
pub const UNCLAIMED: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(50);
const MAX_TOKENS: u64 = 8192;

const SYSTEM: &str = "You are a read-only reviewer. Follow the user message exactly and answer in \
the format it asks for.";

/// One block of a completion's text (`ModelTextBlock`): `cache` marks it for the prompt cache.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Block {
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache: bool,
}

/// What the mod passes to `$.model.complete` (V107).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub id: String,
    pub model: String,
    pub system: String,
    pub prompt: Vec<Block>,
    pub max_tokens: u64,
    /// What is left of the wake clock: the mod's call ends with it (V87).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `$.model.complete`'s token counts (`ModelUsage`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

impl From<ModelUsage> for Usage {
    fn from(u: ModelUsage) -> Usage {
        Usage {
            input: u.input_tokens,
            cached: u.cache_read_input_tokens,
            cache_write: u.cache_creation_input_tokens,
            output: u.output_tokens,
        }
    }
}

/// `$.model.complete`'s result as the mod writes it (`ModelCompleteResult`): the reply, or the
/// reason there is none (`api-error` with `status` and `error`, `empty-reply`, `aborted`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Completion {
    pub is_answered: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default)]
    pub usage: ModelUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Completion {
    /// V108: `api-error` by status per V86 (no status = no response: network), `empty-reply` →
    /// Slice, `aborted` → Service.
    pub fn into_result(self) -> Result<(String, Usage), (ProviderError, Usage)> {
        let usage = Usage::from(self.usage);
        if self.is_answered {
            return Ok((self.text, usage));
        }
        let error = self.error.unwrap_or_default();
        let e = match (self.reason.as_deref(), self.status) {
            (Some("api-error"), Some(status)) => ProviderError::Http { status, retry_after: None, body: error },
            (Some("api-error"), None) => ProviderError::Network(format!("claude-session: {error}")),
            (Some("empty-reply"), _) => ProviderError::EmptyReply,
            (Some("aborted"), _) => ProviderError::Network("claude-session: aborted".into()),
            (reason, _) => ProviderError::Protocol(format!("claude-session reason {reason:?}")),
        };
        Err((e, usage))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum Entry {
    Request(Request),
    Claimed {
        id: String,
    },
    Result {
        id: String,
        result: Completion,
    },
    /// The Watcher gave up on it (unclaimed, or past the wake clock): no result is taken.
    Abandoned {
        id: String,
        reason: String,
    },
}

/// Where one request stands.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Missing,
    Open,
    Claimed,
    Done(Completion),
    Abandoned,
}

/// The session mailbox's completion requests: one locked append-only file, like the outbox.
#[derive(Clone, Debug)]
pub struct Completions {
    path: PathBuf,
}

impl Completions {
    pub fn new(mailbox: &Path) -> Completions {
        Completions { path: mailbox.join(FILE) }
    }

    pub fn request(&self, r: Request, now: u64) -> io::Result<()> {
        self.transact(now, |_| (vec![Entry::Request(r)], ()))
    }

    /// The mod's claim: every open request, oldest first. Only these may run (V107).
    pub fn claim(&self, now: u64) -> io::Result<Vec<Request>> {
        self.transact(now, |slots| {
            let open: Vec<Request> = slots.iter().filter(|(_, s)| *s == State::Open).map(|(r, _)| r.clone()).collect();
            (open.iter().map(|r| Entry::Claimed { id: r.id.clone() }).collect(), open)
        })
    }

    /// The mod's result for a claimed request; false if it is not claimed (abandoned, unknown,
    /// already answered).
    pub fn result(&self, id: &str, result: Completion, now: u64) -> io::Result<bool> {
        self.transact(now, |slots| match find(slots, id) {
            State::Claimed => (vec![Entry::Result { id: id.to_owned(), result }], true),
            _ => (vec![], false),
        })
    }

    /// Gives up on a request not yet answered; with `unclaimed_only`, only if no mod claimed it.
    /// True if abandoned.
    pub fn abandon(&self, id: &str, reason: &str, unclaimed_only: bool, now: u64) -> io::Result<bool> {
        self.transact(now, |slots| match find(slots, id) {
            State::Open => (vec![Entry::Abandoned { id: id.to_owned(), reason: reason.to_owned() }], true),
            State::Claimed if !unclaimed_only => {
                (vec![Entry::Abandoned { id: id.to_owned(), reason: reason.to_owned() }], true)
            }
            _ => (vec![], false),
        })
    }

    pub fn state(&self, id: &str) -> io::Result<State> {
        let records = jsonl::read_from::<Record<Entry>>(&self.path, 0)?.records;
        Ok(find(&fold(records), id))
    }

    fn transact<R>(&self, now: u64, f: impl FnOnce(&[(Request, State)]) -> (Vec<Entry>, R)) -> io::Result<R> {
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

fn fold(records: Vec<Record<Entry>>) -> Vec<(Request, State)> {
    let mut slots: Vec<(Request, State)> = Vec::new();
    for r in records {
        let (id, next) = match r.body {
            Entry::Request(req) => {
                if !slots.iter().any(|(q, _)| q.id == req.id) {
                    slots.push((req, State::Open));
                }
                continue;
            }
            Entry::Claimed { id } => (id, State::Claimed),
            Entry::Result { id, result } => (id, State::Done(result)),
            Entry::Abandoned { id, .. } => (id, State::Abandoned),
        };
        if let Some((_, s)) = slots.iter_mut().find(|(q, _)| q.id == id)
            && matches!(s, State::Open | State::Claimed)
        {
            *s = next;
        }
    }
    slots
}

fn find(slots: &[(Request, State)], id: &str) -> State {
    slots.iter().find(|(r, _)| r.id == id).map_or(State::Missing, |(_, s)| s.clone())
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The backend for one session. A `fallback_model` wake runs on the direct API (V51, V108).
#[derive(Clone)]
pub struct Session {
    pub completions: Completions,
    pub api: Api,
    pub unclaimed: Duration,
}

impl Session {
    pub fn call(&self, call: &Call) -> Result<Reply, Failure> {
        if call.fallback {
            return self.api.call(call);
        }
        let mut usage = Usage::default();
        let mut allowance = Allowance::default();
        let mut exchange = String::new();
        let mut requests = 0;
        loop {
            let forced = call.tools.is_none() || allowance.spent();
            let req = self.request(call, &exchange, forced);
            requests += 1;
            let (text, u) = self.complete(req, call.deadline).map_err(|(error, u)| {
                usage += u;
                Failure { error, usage }
            })?;
            usage += u;
            let (asks, rest) = split(&text);
            let snap = match call.tools {
                Some(snap) if !forced && !asks.is_empty() => snap,
                _ => return Ok(Reply { text: rest, usage, tools: allowance, requests }),
            };
            exchange.push_str("\n\n## Your tool requests\n");
            for (line, _, _) in &asks {
                let _ = writeln!(exchange, "{line}");
            }
            exchange.push_str("\n## Tool results\n");
            for (line, name, args) in &asks {
                let out = allowance.run(snap, name, args);
                let _ = write!(exchange, "\n### {line}\n{out}\n");
            }
        }
    }

    fn request(&self, call: &Call, exchange: &str, forced: bool) -> Request {
        let mut system = SYSTEM.to_owned();
        if call.tools.is_some() {
            system.push_str(&tool_protocol());
        }
        let mut prompt = vec![Block { text: call.input.to_owned(), cache: true }];
        let mut tail = exchange.to_owned();
        if call.tools.is_some() && forced {
            tail.push_str("\n\nThe tool allowance is spent: answer now with output lines only, no tool requests.");
        }
        if !tail.is_empty() {
            prompt.push(Block { text: tail, cache: false });
        }
        let id = format!("{}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed), now_ms());
        let timeout_ms = call.deadline.map(|d| d.saturating_duration_since(Instant::now()).as_millis() as u64);
        Request { id, model: call.model.to_owned(), system, prompt, max_tokens: MAX_TOKENS, timeout_ms }
    }

    /// Writes the request and waits for its result: unclaimed after [`Session::unclaimed`] is a
    /// Key failure, past the deadline a timeout; either way it is abandoned (V108).
    fn complete(&self, req: Request, deadline: Option<Instant>) -> Result<(String, Usage), (ProviderError, Usage)> {
        let io = |e: io::Error| (ProviderError::Network(format!("claude-session mailbox: {e}")), Usage::default());
        let id = req.id.clone();
        self.completions.request(req, now_ms()).map_err(io)?;
        let start = Instant::now();
        loop {
            match self.completions.state(&id).map_err(io)? {
                State::Done(c) => return c.into_result(),
                State::Open
                    if start.elapsed() >= self.unclaimed
                        && self.completions.abandon(&id, "unclaimed", true, now_ms()).map_err(io)? =>
                {
                    return Err((ProviderError::Unanswered, Usage::default()));
                }
                State::Missing | State::Abandoned => {
                    return Err((
                        ProviderError::Protocol(format!("claude-session request {id} lost")),
                        Usage::default(),
                    ));
                }
                _ => {}
            }
            if deadline.is_some_and(|d| Instant::now() >= d)
                && self.completions.abandon(&id, "timeout", false, now_ms()).map_err(io)?
            {
                return Err((ProviderError::Timeout(String::new()), Usage::default()));
            }
            thread::sleep(POLL);
        }
    }
}

/// How the model asks for read tools when its client offers none (V8).
fn tool_protocol() -> String {
    let mut s = format!(
        "\n\nRead tools: to read before you answer, reply with tool request lines only, one JSON object \
         per line: {{\"tool\": \"<name>\", \"args\": {{...}}}}. The next message brings the results. At \
         most {MAX_CALLS} calls and {} KB per wake; then answer. Tools:\n",
        MAX_READ / 1024
    );
    for d in tools::defs() {
        let _ = writeln!(s, "- {}: {} args schema: {}", d.name, d.description, d.schema);
    }
    s
}

/// The reply's tool request lines (line, tool, args), and its text without them.
fn split(text: &str) -> (Vec<(String, String, Value)>, String) {
    let mut asks = Vec::new();
    let mut rest = Vec::new();
    for line in text.lines() {
        let ask = serde_json::from_str::<Value>(line.trim()).ok().and_then(|v| {
            let name = v.get("tool")?.as_str()?.to_owned();
            Some((line.trim().to_owned(), name, v.get("args").cloned().unwrap_or(Value::Null)))
        });
        match ask {
            Some(a) => asks.push(a),
            None => rest.push(line),
        }
    }
    (asks, rest.join("\n"))
}
