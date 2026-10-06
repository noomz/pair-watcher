//! UTF-8-safe byte clipping for wake input sections (V3, V7). Sizes are bytes.

/// Largest char boundary ≤ `n`.
fn floor(s: &str, n: usize) -> usize {
    if n >= s.len() {
        return s.len();
    }
    (0..=n).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0)
}

/// Smallest char boundary ≥ `n`.
fn ceil(s: &str, n: usize) -> usize {
    (n..=s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len())
}

/// The longest prefix of at most `n` bytes.
pub fn prefix(s: &str, n: usize) -> &str {
    &s[..floor(s, n)]
}

/// At most `max` bytes; a cut ends in `…`.
pub fn head(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    format!("{}…", &s[..floor(s, max.saturating_sub('…'.len_utf8()))])
}

/// The first line, at most `max` bytes.
pub fn first_line(s: &str, max: usize) -> String {
    head(s.trim_start().lines().next().unwrap_or_default().trim_end(), max)
}

/// First `h` and last `t` bytes around `[clipped N B]`; whole when the cut would save nothing.
pub fn head_tail(s: &str, h: usize, t: usize) -> String {
    let a = floor(s, h);
    let b = ceil(s, s.len().saturating_sub(t));
    if b <= a {
        return s.to_owned();
    }
    format!("{}\n[clipped {} B]\n{}", &s[..a], b - a, &s[b..])
}

/// At most about `max` bytes, cut from the middle (head 3/5, tail 2/5).
pub fn middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let room = max.saturating_sub(24);
    head_tail(s, room * 3 / 5, room - room * 3 / 5)
}
