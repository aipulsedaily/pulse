//! Streaming ANSI stripper shared by the probe (line predicates) and the
//! daemon (`C2D::BlockText` — journal ranges rendered as clean clipboard
//! text). Same grammar as the probe's original `strip_ansi` (CSI, OSC with
//! BEL/ST terminators, bare ESC-x) but carries its state across chunk
//! boundaries, so feeding a stream in arbitrary slices never leaves a
//! half-swallowed sequence's tail in the output. OSC bodies — including the
//! 7717 block hooks — are swallowed whole; stray BELs are dropped too.

#[derive(Default, Clone, Copy)]
enum StripState {
    #[default]
    Ground,
    Esc,
    /// ESC + intermediate byte(s) 0x20–0x2F (charset designations like
    /// `ESC ( B`): the FINAL byte belongs to the sequence and must be
    /// swallowed too (L-9 — it used to leak into stripped output).
    EscInter,
    Csi,
    Osc,
    OscEsc,
}

#[derive(Default)]
pub struct AnsiStripper {
    state: StripState,
}

impl AnsiStripper {
    /// Legacy char-per-byte output (Latin-1 expansion of the raw bytes) —
    /// kept for existing callers whose predicates are ASCII-only. New code
    /// that shows text to humans/agents should use `feed_bytes` and decode
    /// the result as UTF-8 once the stream is complete.
    pub fn feed(&mut self, bytes: &[u8], out: &mut String) {
        let mut raw = Vec::with_capacity(bytes.len());
        self.feed_bytes(bytes, &mut raw);
        out.extend(raw.iter().map(|&b| b as char));
    }

    /// Same DFA, raw-byte output: multi-byte UTF-8 text passes through
    /// intact (decode with `String::from_utf8_lossy` when done).
    pub fn feed_bytes(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for &b in bytes {
            self.state = match self.state {
                StripState::Ground => match b {
                    0x1b => StripState::Esc,
                    0x07 => StripState::Ground, // stray bell
                    _ => {
                        out.push(b);
                        StripState::Ground
                    }
                },
                StripState::Esc => match b {
                    b'[' => StripState::Csi,
                    b']' => StripState::Osc,
                    0x20..=0x2f => StripState::EscInter, // ESC ( … / ESC ) …
                    _ => StripState::Ground, // two-byte ESC x
                },
                StripState::EscInter => match b {
                    0x20..=0x2f => StripState::EscInter,
                    _ => StripState::Ground, // final byte, swallowed
                },
                StripState::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        StripState::Ground
                    } else {
                        StripState::Csi
                    }
                }
                StripState::Osc => match b {
                    0x07 => StripState::Ground,
                    0x1b => StripState::OscEsc,
                    _ => StripState::Osc,
                },
                StripState::OscEsc => {
                    if b == b'\\' {
                        StripState::Ground
                    } else {
                        StripState::Osc
                    }
                }
            };
        }
    }
}

/// Lowercase hex of a byte slice — the shared encoder (was hand-rolled in
/// codex_hooks, bootstrap, and two probe/test helpers). Two chars per byte,
/// no separators.
pub fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Paste/submission payload sanitization (security): strip control
/// characters — C0 except `\r` `\n` `\t`, DEL, and the C1 range — before the
/// payload is written to a PTY. A clipboard string containing a literal
/// `ESC[201~` would otherwise CLOSE a bracketed paste early and everything
/// after it (typically ending in `\r`) executes as live input; raw ESC can
/// likewise inject arbitrary VT / win32-input records on the non-bracketed
/// path. Keeping `\r\n\t` preserves multi-line pastes exactly.
/// xterm/alacritty/Windows Terminal precedent.
pub fn sanitize_paste(text: &str) -> std::borrow::Cow<'_, str> {
    // char::is_control = Unicode Cc: C0, DEL, and C1 (U+0080–U+009F, which
    // includes the single-char CSI U+009B).
    let bad = |c: char| c.is_control() && !matches!(c, '\r' | '\n' | '\t');
    if text.chars().any(bad) {
        std::borrow::Cow::Owned(text.chars().filter(|&c| !bad(c)).collect())
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F2 (round 2): the bracketed-paste injection class. The sanitized
    /// payload may not contain ESC (bracket close-early), other C0 controls
    /// (except \r\n\t), DEL, or C1 — while ordinary multi-line text passes
    /// through byte-identical (and borrowed).
    #[test]
    fn sanitize_paste_strips_escapes_keeps_text() {
        // The attack: close the bracket early, then a live command + Enter.
        let evil = "harmless\x1b[201~rm -rf ~\r\x1b[200~";
        let clean = sanitize_paste(evil);
        assert!(!clean.contains('\x1b'), "ESC must never survive a paste");
        assert_eq!(clean, "harmless[201~rm -rf ~\r[200~");
        // Ordinary multi-line paste: untouched, zero-copy.
        let normal = "line one\r\n\tline two\nline three";
        assert!(matches!(
            sanitize_paste(normal),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(sanitize_paste(normal), normal);
        // Full C0 sweep + DEL + C1 (U+009B is a one-char CSI).
        let controls = "a\x00b\x07c\x7fd\u{9b}e\u{85}f";
        assert_eq!(sanitize_paste(controls), "abcdef");
        // Unicode text is not text-mangled.
        let uni = "grüße 你好 🎉";
        assert_eq!(sanitize_paste(uni), uni);
    }

    #[test]
    fn strips_hooks_sgr_and_bel() {
        let mut s = AnsiStripper::default();
        let mut out = String::new();
        // A 7717 hook OSC (BEL-terminated), SGR color, and a stray BEL must
        // all vanish; the plain text must survive — including across an
        // arbitrary chunk split inside the OSC body.
        let data = b"pre\x1b]7717;0123456789abcdef;exec;7b7d\x07\x1b[31mred\x1b[0m\x07end";
        let (a, b) = data.split_at(11); // split mid-OSC
        s.feed(a, &mut out);
        s.feed(b, &mut out);
        assert_eq!(out, "preredend"); // "pre" + "red" + "end"
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}') && !out.contains("7717"));
    }

    /// L-9: DEC charset designations (`ESC ( B`, `ESC ) 0`) are three bytes —
    /// the final byte must not leak into stripped output, and a chunk split
    /// inside the sequence must not change that.
    #[test]
    fn charset_designations_are_swallowed_whole() {
        let mut s = AnsiStripper::default();
        let mut out = String::new();
        let data = b"a\x1b(Bb\x1b)0c\x1b(0d";
        let (x, y) = data.split_at(2); // split between ESC and '('
        s.feed(x, &mut out);
        s.feed(y, &mut out);
        assert_eq!(out, "abcd", "charset final bytes must not leak");
        // Two-byte ESC x still swallows exactly one byte.
        let mut s = AnsiStripper::default();
        let mut out = String::new();
        s.feed(b"x\x1b=y", &mut out); // DECKPAM
        assert_eq!(out, "xy");
    }

    #[test]
    fn feed_bytes_keeps_utf8_intact() {
        // The legacy char-per-byte path mangles multi-byte UTF-8; the bytes
        // path must pass it through so controller reads show real text.
        let mut s = AnsiStripper::default();
        let mut out = Vec::new();
        let data = "héllo \u{1b}[32m漢字\u{1b}[0m🎉".as_bytes();
        // Split inside the 漢 sequence to prove chunk boundaries are safe.
        let cut = data.iter().position(|&b| b == 0xe6).unwrap() + 1;
        s.feed_bytes(&data[..cut], &mut out);
        s.feed_bytes(&data[cut..], &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "héllo 漢字🎉");
    }
}
