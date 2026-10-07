//! The pane's terminal: raw mode on the alternate screen, size, keys. Restored on drop, so a
//! panic unwinding through the pane leaves the shell usable.

use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Enter,
    Esc,
    Backspace,
    CtrlC,
}

/// Raw mode + alternate screen until dropped.
pub struct Raw {
    orig: libc::termios,
}

impl Raw {
    pub fn enter() -> io::Result<Raw> {
        let fd = io::stdin().as_raw_fd();
        let mut t = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr fills `t` on success; checked before use.
        if unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: initialised by the successful tcgetattr above.
        let orig = unsafe { t.assume_init() };
        let mut raw = orig;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `raw` is a valid termios copied from the terminal's own.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = io::stdout();
        out.write_all(b"\x1b[?1049h\x1b[?25l")?;
        out.flush()?;
        Ok(Raw { orig })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
        // SAFETY: restores the attributes read in `enter`.
        unsafe { libc::tcsetattr(io::stdin().as_raw_fd(), libc::TCSANOW, &self.orig) };
    }
}

/// Columns × rows of the terminal on stdout; 80 × 24 when unknown.
pub fn size() -> (usize, usize) {
    let mut ws = MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: TIOCGWINSZ writes a winsize into `ws`; zeroed if it fails.
    let ok = unsafe { libc::ioctl(io::stdout().as_raw_fd(), libc::TIOCGWINSZ, ws.as_mut_ptr()) } == 0;
    // SAFETY: zero-initialised, and filled on success.
    let ws = unsafe { ws.assume_init() };
    if ok && ws.ws_col > 0 && ws.ws_row > 0 { (ws.ws_col as usize, ws.ws_row as usize) } else { (80, 24) }
}

/// Keys typed within `wait`; empty when none.
pub fn keys(wait: Duration) -> io::Result<Vec<Key>> {
    let mut pfd = libc::pollfd { fd: io::stdin().as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd.
    let n = unsafe { libc::poll(&mut pfd, 1, wait.as_millis() as libc::c_int) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(Vec::new()) } else { Err(e) };
    }
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut buf = [0u8; 256];
    let len = io::stdin().read(&mut buf)?;
    Ok(parse(&buf[..len]))
}

/// Bytes from a raw terminal → keys; unknown escape sequences are skipped.
pub fn parse(bytes: &[u8]) -> Vec<Key> {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars().peekable();
    let mut out = Vec::new();
    while let Some(c) = chars.next() {
        out.push(match c {
            '\x1b' if chars.peek() == Some(&'[') || chars.peek() == Some(&'O') => {
                chars.next();
                // Parameters, then the final byte.
                let mut last = None;
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() || c == '~' {
                        last = Some(c);
                        break;
                    }
                }
                match last {
                    Some('A') => Key::Up,
                    Some('B') => Key::Down,
                    _ => continue,
                }
            }
            '\x1b' => Key::Esc,
            '\r' | '\n' => Key::Enter,
            '\x7f' | '\x08' => Key::Backspace,
            '\x03' => Key::CtrlC,
            c if c.is_control() => continue,
            c => Key::Char(c),
        });
    }
    out
}

/// Seconds east of UTC now (DST changes mid-session are ignored).
pub fn local_offset() -> i64 {
    // SAFETY: time + localtime_r with valid pointers; tm read only on success.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm = MaybeUninit::<libc::tm>::zeroed();
        if libc::localtime_r(&now, tm.as_mut_ptr()).is_null() {
            return 0;
        }
        tm.assume_init().tm_gmtoff
    }
}
