//! Making text from other machines safe to draw.
//!
//! A process name, a model name or a hostname is chosen by whoever controls
//! that machine. Drawn as it is, an escape sequence in one could move the
//! cursor, rewrite the screen, change the window title or, in some
//! terminals, run commands. Bidirectional overrides could make one name
//! look like another. So every string read from the wire passes through
//! `clean` before anything else sees it.

/// `s` without control characters (C0, DEL and C1, which covers ESC and
/// every escape sequence's introducer) and without bidirectional overrides
/// and isolates. Tabs and newlines become spaces, so a field stays on its
/// line.
pub fn clean(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\t' | '\n' | '\r' => Some(' '),
            c if c.is_control() => None,
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => None,
            c => Some(c),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn escape_sequences_lose_their_introducer() {
        assert_eq!(clean("a\x1b[2Jb"), "a[2Jb");
        assert_eq!(clean("\x1b]52;c;ZXZpbA==\x07"), "]52;c;ZXZpbA==");
        // C1 CSI, which some terminals honor like ESC [.
        assert_eq!(clean("x\u{9b}31my"), "x31my");
    }

    #[test]
    fn bidi_overrides_are_removed() {
        assert_eq!(clean("llama\u{202e}revres"), "llamarevres");
    }

    #[test]
    fn ordinary_text_is_kept() {
        assert_eq!(clean("NVIDIA RTX PRO 4500 Blackwell · 65°C"), "NVIDIA RTX PRO 4500 Blackwell · 65°C");
        assert_eq!(clean("a\tb\nc"), "a b c");
    }
}
