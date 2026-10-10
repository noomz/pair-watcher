//! Repo Focus approval by file hash (V82): the `run` prompt, `focus approve`, and the
//! `[approved_focus]` table in the user per-repo file. Never notifies.

use std::fs;
use std::io::{self, BufRead, Write};

use toml_edit::{DocumentMut, InlineTable, value};

use crate::focus::PendingApproval;
use crate::home::{Home, write_atomic};
use crate::problem::tilde;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Approve,
    /// Not run this session; asked again next session.
    Skip,
    /// Refused for this exact file; asked again only when the file changes.
    Never,
}

/// Asks about one pending Focus before the Worker starts. `v` prints the file and asks again;
/// end of input skips.
pub fn ask(pending: &PendingApproval, input: &mut impl BufRead, out: &mut impl Write) -> io::Result<Answer> {
    loop {
        write!(
            out,
            "pair-watcher: repo Focus `{}` ({}) is not approved.\n[v]iew [a]pprove [s]kip this session [n]ever for this file: ",
            pending.name,
            tilde(&pending.file)
        )?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            writeln!(out)?;
            return Ok(Answer::Skip);
        }
        match line.trim() {
            "v" | "V" => match fs::read_to_string(&pending.file) {
                Ok(text) => writeln!(out, "{}", text.trim_end())?,
                Err(e) => writeln!(out, "pair-watcher: cannot read {}: {e}", tilde(&pending.file))?,
            },
            "a" | "A" => return Ok(Answer::Approve),
            "s" | "S" => return Ok(Answer::Skip),
            "n" | "N" => return Ok(Answer::Never),
            _ => {}
        }
    }
}

/// Late attach or no TTY: the Focus does not run until approved by command.
pub fn ticker_line(pending: &PendingApproval) -> String {
    format!("repo Focus `{0}` not approved — pair-watcher focus approve {0}", pending.name)
}

/// Stores the decision for exactly this file hash; the rest of the file (human overrides,
/// comments) is kept as written.
pub fn record(home: &Home, root_commit: &str, pending: &PendingApproval, approved: bool) -> io::Result<()> {
    let path = home.repo_config(root_commit);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let invalid = |msg: String| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {msg}", tilde(&path)));
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| invalid(e.to_string()))?;
    let table = doc
        .entry("approved_focus")
        .or_insert(toml_edit::table())
        .as_table_like_mut()
        .ok_or_else(|| invalid("`approved_focus` is not a table".into()))?;
    let mut entry = InlineTable::new();
    entry.insert("sha256", pending.sha256.as_str().into());
    entry.insert("approved", approved.into());
    table.insert(&pending.name, value(entry));
    write_atomic(&path, doc.to_string().as_bytes())
}
