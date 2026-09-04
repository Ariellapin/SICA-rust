//! Output retention: head/tail windows over text with honest omission
//! notices.
//!
//! Three places used to cut oversized text their own way — the spill digest,
//! the per-stream cap in `run-cli`, and the compaction excerpt — each with its
//! own wording. They now share this module so the model reads one vocabulary
//! ("… N bytes omitted …") wherever a cut happened, and so every cut is
//! UTF-8-boundary safe by construction.
//!
//! The library never decides *how to recover* what was omitted; that sentence
//! belongs to the caller (a spill names its file, a stream cap says nothing
//! more is available). [`notice`] takes it as a parameter.

/// What a window left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Omitted {
    /// Nothing — the text fit.
    None,
    /// Exactly this many bytes were dropped from the middle (or the end).
    Bytes(usize),
}

/// The retained parts of a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextWindow<'a> {
    pub head: &'a str,
    pub tail: &'a str,
    pub omitted: Omitted,
}

impl<'a> TextWindow<'a> {
    /// Join the parts, inserting `marker` where the omission happened.
    /// With nothing omitted the head is returned verbatim.
    pub fn render(&self, marker: &str) -> String {
        match self.omitted {
            Omitted::None => self.head.to_string(),
            Omitted::Bytes(_) => {
                let mut out = String::with_capacity(self.head.len() + marker.len() + self.tail.len() + 2);
                out.push_str(self.head);
                if !self.head.is_empty() && !self.head.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(marker);
                if !self.tail.is_empty() {
                    out.push('\n');
                    out.push_str(self.tail);
                }
                out
            }
        }
    }
}

/// Longest prefix of `s` that is at most `max` bytes and ends on a char
/// boundary.
pub fn utf8_head(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Longest suffix of `s` that is at most `max` bytes and starts on a char
/// boundary.
pub fn utf8_tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Keep up to `head` bytes from the front and `tail` bytes from the back.
/// When the text fits inside `head + tail` nothing is omitted and the whole
/// text is the head.
pub fn head_tail(s: &str, head: usize, tail: usize) -> TextWindow<'_> {
    if s.len() <= head + tail {
        return TextWindow { head: s, tail: "", omitted: Omitted::None };
    }
    let h = utf8_head(s, head);
    let t = utf8_tail(s, tail);
    let omitted = s.len().saturating_sub(h.len() + t.len());
    TextWindow { head: h, tail: t, omitted: Omitted::Bytes(omitted) }
}

/// Keep only the first `max` bytes.
pub fn head_only(s: &str, max: usize) -> TextWindow<'_> {
    if s.len() <= max {
        return TextWindow { head: s, tail: "", omitted: Omitted::None };
    }
    let h = utf8_head(s, max);
    TextWindow { head: h, tail: "", omitted: Omitted::Bytes(s.len() - h.len()) }
}

/// The standard omission sentence. `recovery` says how to get at the rest
/// (empty when there is no way); it is appended after a semicolon.
///
/// ```text
/// [… 12345 bytes omitted; full output saved to spill/7/run-cli-1-2.txt …]
/// [… 12345 bytes omitted …]
/// ```
pub fn notice(omitted: Omitted, recovery: &str) -> String {
    match omitted {
        Omitted::None => String::new(),
        Omitted::Bytes(n) if recovery.is_empty() => format!("[… {n} bytes omitted …]"),
        Omitted::Bytes(n) => format!("[… {n} bytes omitted; {recovery} …]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_and_tail_respect_char_boundaries() {
        // 'é' is two bytes; cutting at 3 must not split the second one.
        let s = "aéé";
        assert_eq!(utf8_head(s, 2), "a");
        assert_eq!(utf8_head(s, 3), "aé");
        assert_eq!(utf8_tail(s, 3), "é");
        assert_eq!(utf8_tail(s, 1), "");
        assert_eq!(utf8_head(s, 100), s);
        assert_eq!(utf8_tail(s, 100), s);
    }

    #[test]
    fn fitting_text_is_not_windowed() {
        let w = head_tail("short", 10, 10);
        assert_eq!(w.omitted, Omitted::None);
        assert_eq!(w.head, "short");
        assert_eq!(w.render("X"), "short");
    }

    #[test]
    fn window_counts_exact_bytes() {
        let s = "a".repeat(100);
        let w = head_tail(&s, 30, 20);
        assert_eq!(w.head.len(), 30);
        assert_eq!(w.tail.len(), 20);
        assert_eq!(w.omitted, Omitted::Bytes(50));
        let r = w.render(&notice(w.omitted, "see the file"));
        assert!(r.contains("[… 50 bytes omitted; see the file …]"), "{r}");
        assert!(r.starts_with(&"a".repeat(30)));
        assert!(r.ends_with(&"a".repeat(20)));
    }

    #[test]
    fn head_only_marks_the_end() {
        let s = "line1\nline2\nline3";
        let w = head_only(s, 6);
        assert_eq!(w.head, "line1\n");
        assert_eq!(w.tail, "");
        assert_eq!(w.omitted, Omitted::Bytes(11));
        assert_eq!(w.render(&notice(w.omitted, "")), "line1\n[… 11 bytes omitted …]");
    }

    #[test]
    fn notice_without_recovery_is_terse() {
        assert_eq!(notice(Omitted::None, "x"), "");
        assert_eq!(notice(Omitted::Bytes(3), ""), "[… 3 bytes omitted …]");
    }
}
