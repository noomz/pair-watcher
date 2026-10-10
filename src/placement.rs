//! Where the pane opens (V61): a Herdr split, else a tmux split, else `run` prints the command.
//! Detected from the launch env, which the supervisor and its Watcher inherit; the Watcher opens
//! the pane on the first Concern, Escalation or BLIND (V67).

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

use crate::hook::shell_line;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    Herdr { bin: PathBuf, pane: String },
    Tmux { bin: PathBuf, pane: Option<String> },
    Print,
}

impl Placement {
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Placement {
        if let Some(pane) = env("HERDR_PANE_ID") {
            let bin = env("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
            return Placement::Herdr { bin: bin.into(), pane };
        }
        if env("TMUX").is_some() {
            return Placement::Tmux { bin: "tmux".into(), pane: env("TMUX_PANE") };
        }
        Placement::Print
    }

    /// Opens `command` in a split beside the Worker without taking focus.
    pub fn open(&self, command: &[String]) -> io::Result<()> {
        let line = shell_line(command);
        match self {
            Placement::Herdr { bin, pane } => {
                let split = Command::new(bin)
                    .args(["pane", "split", "--pane", pane, "--direction", "right", "--no-focus"])
                    .output()?;
                let out = checked(bin, split)?;
                let json: Value = serde_json::from_slice(&out.stdout).map_err(io::Error::other)?;
                let id = json["result"]["pane"]["pane_id"]
                    .as_str()
                    .ok_or_else(|| io::Error::other("herdr pane split: no result.pane.pane_id"))?;
                checked(bin, Command::new(bin).args(["pane", "run", id, &line]).output()?)?;
            }
            Placement::Tmux { bin, pane } => {
                let mut cmd = Command::new(bin);
                cmd.args(["split-window", "-h", "-d"]);
                if let Some(p) = pane {
                    cmd.args(["-t", p]);
                }
                checked(bin, cmd.arg(&line).output()?)?;
            }
            Placement::Print => return Err(io::Error::new(io::ErrorKind::Unsupported, "no Herdr or tmux")),
        }
        Ok(())
    }
}

/// `pair-watcher pane --session <id>`, run by the split or printed by `run`.
pub fn pane_command(exe: &Path, session: &str) -> Vec<String> {
    vec![exe.to_string_lossy().into_owned(), "pane".into(), "--session".into(), session.into()]
}

fn checked(bin: &Path, out: Output) -> io::Result<Output> {
    if out.status.success() {
        return Ok(out);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(io::Error::other(format!("{}: {} {}", bin.display(), out.status, err.trim())))
}
