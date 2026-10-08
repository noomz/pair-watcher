//! API keys: standard env names first, then `~/.pair-watcher/credentials` (I18), a TOML file of the
//! same names that is refused when group/world-readable (I2, C8).

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use toml::Table;

use crate::home::{Home, write_atomic};
use crate::models::Vendor;
use crate::problem::tilde;

pub fn env_name(vendor: Vendor) -> &'static str {
    match vendor {
        Vendor::Anthropic => "ANTHROPIC_API_KEY",
        Vendor::Openai => "OPENAI_API_KEY",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredError {
    /// Group/world-readable: not read at all.
    Exposed(PathBuf),
    Unreadable(PathBuf, String),
}

impl fmt::Display for CredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredError::Exposed(p) => write!(f, "{}: group/world-readable, refused (chmod 600 it)", tilde(p)),
            CredError::Unreadable(p, e) => write!(f, "{}: {e}", tilde(p)),
        }
    }
}

/// Keys found per vendor, env winning over the file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Keys {
    anthropic: Option<String>,
    openai: Option<String>,
    /// The file was refused or unreadable and counted as absent; env keys still apply.
    pub problem: Option<CredError>,
}

impl Keys {
    pub fn load(home: &Home, env: &dyn Fn(&str) -> Option<String>) -> Keys {
        let mut keys = Keys::default();
        let file = read_file(home).unwrap_or_else(|e| {
            keys.problem = Some(e);
            None
        });
        for vendor in [Vendor::Anthropic, Vendor::Openai] {
            let name = env_name(vendor);
            let key = env(name)
                .filter(|k| !k.trim().is_empty())
                .or_else(|| file.as_ref().and_then(|t| t.get(name)?.as_str().map(str::to_owned)))
                .map(|k| k.trim().to_owned())
                .filter(|k| !k.is_empty());
            *keys.slot(vendor) = key;
        }
        keys
    }

    pub fn get(&self, vendor: Vendor) -> Option<&str> {
        match vendor {
            Vendor::Anthropic => self.anthropic.as_deref(),
            Vendor::Openai => self.openai.as_deref(),
        }
    }

    /// Vendors with a key: the backends `model = auto` may pick from (V46).
    pub fn configured(&self) -> Vec<Vendor> {
        [Vendor::Anthropic, Vendor::Openai].into_iter().filter(|v| self.get(*v).is_some()).collect()
    }

    fn slot(&mut self, vendor: Vendor) -> &mut Option<String> {
        match vendor {
            Vendor::Anthropic => &mut self.anthropic,
            Vendor::Openai => &mut self.openai,
        }
    }
}

fn read_file(home: &Home) -> Result<Option<Table>, CredError> {
    let path = home.credentials_file();
    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CredError::Unreadable(path, e.to_string())),
    };
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(CredError::Exposed(path));
    }
    let text = fs::read_to_string(&path).map_err(|e| CredError::Unreadable(path.clone(), e.to_string()))?;
    // The parse error would quote the line, i.e. a key: report the file only.
    text.parse::<Table>().map(Some).map_err(|_| CredError::Unreadable(path, "unparseable".into()))
}

/// `auth`: sets each given key in `credentials`, keeping the rest of the file; written 0600, which
/// also fixes a file that was group/world-readable (C8).
pub fn store(home: &Home, keys: &[(Vendor, String)]) -> io::Result<()> {
    let path = home.credentials_file();
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    // The parse error would quote the line, i.e. a key: report the file only.
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{}: unparseable", tilde(&path))))?;
    for (vendor, key) in keys {
        doc.insert(env_name(*vendor), toml_edit::value(key.trim()));
    }
    write_atomic(&path, doc.to_string().as_bytes())
}
