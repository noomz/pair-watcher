//! Direct API backend, the default model provider (I15): Anthropic Messages and OpenAI Responses,
//! with the code-owned read-only tool loop (V8). Each wake is one fresh, stateless conversation
//! (V2). A blocked wake passes no snapshot: no tools, one call (V11). Retries, timeouts per wake and
//! failure classes belong to the supervisor (V86, V87).

use std::fmt;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::credentials::Keys;
use crate::models::{Usage, Vendor};
use crate::tools::{self, Allowance, Snapshot};

const MAX_OUTPUT: u64 = 8192;
const ANTHROPIC_URL: &str = "https://api.anthropic.com";
const OPENAI_URL: &str = "https://api.openai.com/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ERROR_BODY_MAX: usize = 2048;

/// One Watcher's model call for one wake.
pub struct Call<'a> {
    pub model: &'a str,
    pub vendor: Vendor,
    /// Rendered wake input (`wake::Input::render`).
    pub input: &'a str,
    /// `None` = no tools (blocked wake).
    pub tools: Option<Snapshot<'a>>,
    /// The wake clock (V87): no request runs past it.
    pub deadline: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    /// Text of the final response.
    pub text: String,
    /// Summed over every request of the loop.
    pub usage: Usage,
    pub tools: Allowance,
    pub requests: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProviderError {
    NoKey(Vendor),
    Http {
        status: u16,
        retry_after: Option<Duration>,
        body: String,
    },
    Timeout,
    Network(String),
    /// Response not in the vendor's shape.
    Protocol(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::NoKey(v) => write!(f, "no {} API key", v.name()),
            ProviderError::Http { status, body, .. } => write!(f, "HTTP {status}: {body}"),
            ProviderError::Timeout => f.write_str("timed out"),
            ProviderError::Network(e) => write!(f, "network: {e}"),
            ProviderError::Protocol(e) => write!(f, "unexpected response: {e}"),
        }
    }
}

/// A failed call still spent what its earlier requests billed (V47).
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub error: ProviderError,
    pub usage: Usage,
}

pub struct Api {
    agent: ureq::Agent,
    keys: Keys,
    anthropic_url: String,
    openai_url: String,
}

impl Api {
    /// Base URLs from the standard `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL`; `timeout` bounds each
    /// request.
    pub fn new(keys: Keys, env: &dyn Fn(&str) -> Option<String>, timeout: Duration) -> Api {
        let agent =
            ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(timeout)).build().into();
        let base = |var: &str, default: &str| {
            env(var)
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| default.to_owned())
                .trim_end_matches('/')
                .to_owned()
        };
        Api {
            agent,
            keys,
            anthropic_url: base("ANTHROPIC_BASE_URL", ANTHROPIC_URL),
            openai_url: base("OPENAI_BASE_URL", OPENAI_URL),
        }
    }

    pub fn call(&self, call: &Call) -> Result<Reply, Failure> {
        let mut usage = Usage::default();
        let fail = |error, usage| Failure { error, usage };
        let key = self.keys.get(call.vendor).ok_or_else(|| fail(ProviderError::NoKey(call.vendor), usage))?;
        let mut convo = Convo::new(call.vendor, call.input);
        let mut allowance = Allowance::default();
        let mut requests = 0;
        loop {
            let forced = call.tools.is_none() || allowance.spent();
            let body = convo.request(call.model, call.tools.is_some(), forced);
            requests += 1;
            let resp = self.post(call.vendor, key, &body, call.deadline).map_err(|e| fail(e, usage))?;
            let turn = convo.absorb(resp).map_err(|e| fail(e, usage))?;
            usage += turn.usage;
            let snap = match call.tools {
                Some(snap) if !forced && !turn.calls.is_empty() => snap,
                _ => return Ok(Reply { text: turn.text, usage, tools: allowance, requests }),
            };
            let results = turn.calls.into_iter().map(|c| (c.id, allowance.run(snap, &c.name, &c.args))).collect();
            convo.answer(results);
        }
    }

    fn post(&self, vendor: Vendor, key: &str, body: &Value, deadline: Option<Instant>) -> Result<Value, ProviderError> {
        let left = match deadline {
            Some(d) => {
                Some(d.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()).ok_or(ProviderError::Timeout)?)
            }
            None => None,
        };
        let req = match vendor {
            Vendor::Anthropic => self
                .agent
                .post(format!("{}/v1/messages", self.anthropic_url))
                .header("x-api-key", key)
                .header("anthropic-version", ANTHROPIC_VERSION),
            Vendor::Openai => self
                .agent
                .post(format!("{}/responses", self.openai_url))
                .header("authorization", format!("Bearer {key}")),
        };
        let req = match left {
            Some(left) => req.config().timeout_global(Some(left)).build(),
            None => req,
        };
        let mut resp = req.send_json(body).map_err(transport)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(ProviderError::Http { status, retry_after, body: crate::clip::head(&body, ERROR_BODY_MAX) });
        }
        resp.body_mut().read_json::<Value>().map_err(transport)
    }
}

fn transport(e: ureq::Error) -> ProviderError {
    match e {
        ureq::Error::Timeout(_) => ProviderError::Timeout,
        ureq::Error::Json(e) => ProviderError::Protocol(e.to_string()),
        e => ProviderError::Network(e.to_string()),
    }
}

struct ToolCall {
    id: String,
    name: String,
    args: Value,
}

/// One response, normalized.
struct Turn {
    text: String,
    calls: Vec<ToolCall>,
    usage: Usage,
}

/// The wake's conversation in the vendor's wire shape.
enum Convo {
    Anthropic(Vec<Value>),
    Openai(Vec<Value>),
}

impl Convo {
    fn new(vendor: Vendor, input: &str) -> Convo {
        match vendor {
            // Cache the input so the loop's later requests read it at the cached rate.
            Vendor::Anthropic => Convo::Anthropic(vec![json!({
                "role": "user",
                "content": [{"type": "text", "text": input, "cache_control": {"type": "ephemeral"}}]
            })]),
            Vendor::Openai => Convo::Openai(vec![json!({"role": "user", "content": input})]),
        }
    }

    fn request(&self, model: &str, tools: bool, forced: bool) -> Value {
        let defs = tools::defs();
        match self {
            Convo::Anthropic(messages) => {
                let mut body = json!({"model": model, "max_tokens": MAX_OUTPUT, "messages": messages});
                if tools {
                    body["tools"] = defs
                        .iter()
                        .map(|d| json!({"name": d.name, "description": d.description, "input_schema": d.schema}))
                        .collect();
                    body["tool_choice"] = json!({"type": if forced { "none" } else { "auto" }});
                }
                body
            }
            Convo::Openai(input) => {
                let mut body = json!({
                    "model": model,
                    "input": input,
                    "max_output_tokens": MAX_OUTPUT,
                    "store": false,
                    "include": ["reasoning.encrypted_content"]
                });
                if tools {
                    body["tools"] = defs
                        .iter()
                        .map(|d| json!({"type": "function", "name": d.name, "description": d.description, "parameters": d.schema}))
                        .collect();
                    body["tool_choice"] = json!(if forced { "none" } else { "auto" });
                }
                body
            }
        }
    }

    /// Appends the response to the conversation and reads it.
    fn absorb(&mut self, resp: Value) -> Result<Turn, ProviderError> {
        let missing = |what: &str| ProviderError::Protocol(format!("no `{what}`"));
        let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        match self {
            Convo::Anthropic(messages) => {
                let content = resp.get("content").and_then(Value::as_array).ok_or_else(|| missing("content"))?;
                let u = resp.get("usage").ok_or_else(|| missing("usage"))?;
                let turn = Turn {
                    text: texts(content.iter().filter(|b| b["type"] == "text").map(|b| &b["text"])),
                    calls: content
                        .iter()
                        .filter(|b| b["type"] == "tool_use")
                        .map(|b| ToolCall { id: str_of(&b["id"]), name: str_of(&b["name"]), args: b["input"].clone() })
                        .collect(),
                    usage: Usage {
                        input: n(u, "input_tokens"),
                        cached: n(u, "cache_read_input_tokens"),
                        cache_write: n(u, "cache_creation_input_tokens"),
                        output: n(u, "output_tokens"),
                    },
                };
                messages.push(json!({"role": "assistant", "content": content}));
                Ok(turn)
            }
            Convo::Openai(input) => {
                let output = resp.get("output").and_then(Value::as_array).ok_or_else(|| missing("output"))?;
                let u = resp.get("usage").ok_or_else(|| missing("usage"))?;
                let cached = u.get("input_tokens_details").map_or(0, |d| n(d, "cached_tokens"));
                let turn = Turn {
                    text: texts(
                        output
                            .iter()
                            .filter(|i| i["type"] == "message")
                            .filter_map(|i| i["content"].as_array())
                            .flatten()
                            .filter(|c| c["type"] == "output_text")
                            .map(|c| &c["text"]),
                    ),
                    calls: output
                        .iter()
                        .filter(|i| i["type"] == "function_call")
                        .map(|i| ToolCall {
                            id: str_of(&i["call_id"]),
                            name: str_of(&i["name"]),
                            // Unparseable arguments reach the tool as none and come back as its error.
                            args: serde_json::from_str(i["arguments"].as_str().unwrap_or("{}")).unwrap_or(Value::Null),
                        })
                        .collect(),
                    usage: Usage {
                        input: n(u, "input_tokens").saturating_sub(cached),
                        cached,
                        cache_write: 0,
                        output: n(u, "output_tokens"),
                    },
                };
                input.extend(output.iter().cloned());
                Ok(turn)
            }
        }
    }

    fn answer(&mut self, results: Vec<(String, String)>) {
        match self {
            Convo::Anthropic(messages) => messages.push(json!({
                "role": "user",
                "content": results
                    .into_iter()
                    .map(|(id, out)| json!({"type": "tool_result", "tool_use_id": id, "content": out}))
                    .collect::<Vec<_>>()
            })),
            Convo::Openai(input) => input.extend(
                results
                    .into_iter()
                    .map(|(id, out)| json!({"type": "function_call_output", "call_id": id, "output": out})),
            ),
        }
    }
}

fn texts<'a>(parts: impl Iterator<Item = &'a Value>) -> String {
    parts.filter_map(Value::as_str).collect::<Vec<_>>().join("\n")
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}
