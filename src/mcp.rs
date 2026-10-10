//! `pair-watcher mcp` (I15): the read tools over MCP (stdio, newline-delimited JSON-RPC) for the
//! subscription backends. One server per wake, over that wake's snapshot; it holds the wake's
//! allowance (V8) and writes it to a stats file after each call for the provider to read back.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::snapshot::Store;
use crate::tools::{self, Allowance, Snapshot};

pub const SERVER: &str = "pair-watcher";
const PROTOCOL: &str = "2025-06-18";

/// One wake's server: the snapshot it serves and the allowance spent so far.
pub struct Server {
    store: Store,
    tree: String,
    head: Option<String>,
    stats: Option<PathBuf>,
    pub allowance: Allowance,
}

impl Server {
    pub fn new(store: Store, tree: String, head: Option<String>, stats: Option<PathBuf>) -> Server {
        Server { store, tree, head, stats, allowance: Allowance::default() }
    }

    /// One request or notification in, its response out (`None` for a notification).
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").filter(|id| !id.is_null())?.clone();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": SERVER, "version": env!("CARGO_PKG_VERSION")}
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": tools::defs()
                    .into_iter()
                    .map(|d| json!({"name": d.name, "description": d.description, "inputSchema": d.schema}))
                    .collect::<Vec<_>>()
            })),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or_default();
                let args = msg["params"].get("arguments").cloned().unwrap_or(Value::Null);
                let snap = Snapshot { store: &self.store, tree: &self.tree, head: self.head.as_deref() };
                let out = self.allowance.run(snap, name, &args);
                if let Some(path) = &self.stats {
                    let _ = write_stats(path, &self.allowance);
                }
                Ok(json!({"content": [{"type": "text", "text": out}], "isError": false}))
            }
            other => Err(json!({"code": -32601, "message": format!("method not found: {other}")})),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        })
    }

    /// Serves until stdin closes. A line that is not JSON gets a parse error.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) => self.handle(&msg),
                Err(e) => {
                    Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}}))
                }
            };
            if let Some(reply) = reply {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}

fn write_stats(path: &Path, a: &Allowance) -> io::Result<()> {
    std::fs::write(path, json!({"calls": a.calls, "read": a.read}).to_string())
}

/// The allowance a server wrote; nothing written = no calls.
pub fn read_stats(path: &Path) -> Allowance {
    let v: Value = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
    Allowance { calls: n("calls"), read: n("read") }
}

/// `pair-watcher mcp`: opens the wake's snapshot store and serves stdio.
pub fn run(mailbox: &Path, repo: &Path, tree: String, head: Option<String>, stats: Option<PathBuf>) -> io::Result<()> {
    let repo = crate::repo::discover(repo)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{} is not a git repo", repo.display())))?;
    let store = Store::open(mailbox, &repo)?;
    Server::new(store, tree, head, stats).serve(io::stdin().lock(), io::stdout().lock())
}
