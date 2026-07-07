//! ConPTY win32-input-mode (DECSET 9001): detection + key-event encoding.
//!
//! conhost requests this mode at session start (`ESC[?9001h` is at byte 4 of
//! every journal) and re-asserts it throughout. When the terminal honors it,
//! keys are shipped as full Win32 KEY_EVENT records —
//! `ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _` (microsoft/terminal spec #4999) —
//! so conhost receives real key events instead of lossy VT bytes, and every
//! Windows chord (Ctrl+Backspace word-delete, Ctrl+Space, Shift+Enter,
//! Alt+word motions, Ctrl+Shift+F-keys, …) reaches PSReadLine exactly as it
//! would from Windows Terminal. conhost re-encodes them for whatever the
//! foreground app's console mode wants, so VT-input TUIs and ssh keep working.
//!
//! Plain typed text still flows as UTF-8 (the same mixed-stream path Windows
//! Terminal uses for paste), which keeps IME, dead keys, and non-US layouts
//! exact without any layout math on our side.
//!
//! Sequence format and per-chord character quirks follow microsoft/terminal
//! (MIT): doc/specs/#4999 and src/terminal/input/terminalInput.cpp.

use egui::{Key, Modifiers};
use std::io::Write;

// dwControlKeyState bits (wincon.h).
const SHIFT_PRESSED: u32 = 0x0010;
const LEFT_CTRL_PRESSED: u32 = 0x0008;
const LEFT_ALT_PRESSED: u32 = 0x0002;
const ENHANCED_KEY: u32 = 0x0100;

/// Streaming detector for `ESC[?9001h` / `ESC[?9001l` in raw PTY output.
/// Carries its match position across chunk boundaries.
#[derive(Default, Clone)]
pub struct ModeScanner {
    pos: u8,
}

const PATTERN: &[u8] = b"\x1b[?9001";

impl ModeScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan a raw output chunk; `Some(enabled)` if it contains a
    /// win32-input-mode set/reset (the last one in the chunk wins).
    pub fn feed(&mut self, bytes: &[u8]) -> Option<bool> {
        let mut result = None;
        let mut i = 0;
        while i < bytes.len() {
            if self.pos == 0 {
                // Ground state: nothing before the next ESC can advance the
                // match — SIMD-skip it. This runs on every output chunk in
                // both the daemon reader and the GUI, so plain text (the
                // overwhelming majority of a flood) must not pay a per-byte
                // DFA step.
                let Some(off) = memchr::memchr(0x1b, &bytes[i..]) else {
                    return result;
                };
                i += off;
            }
            let b = bytes[i];
            i += 1;
            if (self.pos as usize) < PATTERN.len() {
                if b == PATTERN[self.pos as usize] {
                    self.pos += 1;
                    continue;
                }
            } else {
                match b {
                    b'h' => result = Some(true),
                    b'l' => result = Some(false),
                    _ => {}
                }
            }
            // Mismatch or final byte: restart (ESC can begin a new match).
            self.pos = u8::from(b == 0x1b);
        }
        result
    }
}

/// Static facts about an egui key: console virtual-key code, the US-layout
/// characters it types (used for the `Uc` field and the VT fallback's
/// Alt/Ctrl character math), and whether it is an "enhanced" (gray nav) key.
pub struct KeyInfo {
    pub vk: u16,
    pub ch: Option<char>,
    pub shifted: Option<char>,
    pub enhanced: bool,
}

/// 0-based index of a letter key, if `key` is one.
pub fn letter_ord(key: Key) -> Option<u8> {
    let i = key as i32 - Key::A as i32;
    (0..26).contains(&i).then_some(i as u8)
}

pub fn key_info(key: Key) -> Option<KeyInfo> {
    use Key::*;
    if let Some(i) = letter_ord(key) {
        return Some(KeyInfo {
            vk: 0x41 + i as u16,
            ch: Some((b'a' + i) as char),
            shifted: Some((b'A' + i) as char),
            enhanced: false,
        });
    }
    let digit = key as i32 - Num0 as i32;
    if (0..10).contains(&digit) {
        const SHIFTED: &[u8; 10] = b")!@#$%^&*(";
        return Some(KeyInfo {
            vk: 0x30 + digit as u16,
            ch: Some((b'0' + digit as u8) as char),
            shifted: Some(SHIFTED[digit as usize] as char),
            enhanced: false,
        });
    }
    let f = key as i32 - F1 as i32;
    if (0..24).contains(&f) {
        return Some(KeyInfo {
            vk: 0x70 + f as u16,
            ch: None,
            shifted: None,
            enhanced: false,
        });
    }
    let (vk, ch, shifted, enhanced) = match key {
        ArrowUp => (0x26, None, None, true),
        ArrowDown => (0x28, None, None, true),
        ArrowLeft => (0x25, None, None, true),
        ArrowRight => (0x27, None, None, true),
        Escape => (0x1B, None, None, false),
        Tab => (0x09, None, None, false),
        Backspace => (0x08, None, None, false),
        Enter => (0x0D, None, None, false),
        Space => (0x20, Some(' '), Some(' '), false),
        Insert => (0x2D, None, None, true),
        Delete => (0x2E, None, None, true),
        Home => (0x24, None, None, true),
        End => (0x23, None, None, true),
        PageUp => (0x21, None, None, true),
        PageDown => (0x22, None, None, true),
        // OEM punctuation, US layout.
        Semicolon => (0xBA, Some(';'), Some(':'), false),
        Colon => (0xBA, Some(':'), Some(':'), false),
        Equals => (0xBB, Some('='), Some('+'), false),
        Plus => (0xBB, Some('+'), Some('+'), false),
        Comma => (0xBC, Some(','), Some('<'), false),
        Minus => (0xBD, Some('-'), Some('_'), false),
        Period => (0xBE, Some('.'), Some('>'), false),
        Slash => (0xBF, Some('/'), Some('?'), false),
        Questionmark => (0xBF, Some('?'), Some('?'), false),
        Backtick => (0xC0, Some('`'), Some('~'), false),
        OpenBracket => (0xDB, Some('['), Some('{'), false),
        OpenCurlyBracket => (0xDB, Some('{'), Some('{'), false),
        Backslash => (0xDC, Some('\\'), Some('|'), false),
        Pipe => (0xDC, Some('|'), Some('|'), false),
        CloseBracket => (0xDD, Some(']'), Some('}'), false),
        CloseCurlyBracket => (0xDD, Some('}'), Some('}'), false),
        Exclamationmark => (0x31, Some('!'), Some('!'), false),
        Quote => (0xDE, Some('\''), Some('"'), false),
        _ => return None, // F25+, Copy/Cut/Paste — no console VK
    };
    Some(KeyInfo {
        vk,
        ch,
        shifted,
        enhanced,
    })
}

/// The US-layout character `key` types with the given shift state.
pub fn key_char(key: Key, shift: bool) -> Option<char> {
    let info = key_info(key)?;
    if shift {
        info.shifted
    } else {
        info.ch
    }
}

/// Encode one egui key press as a win32-input-mode key-down + key-up pair.
///
/// Returns `None` for unchorded printables: those arrive as `Event::Text`
/// (kept as plain UTF-8 passthrough for layout/IME fidelity — conhost
/// synthesizes key events from chars, exactly like Windows Terminal's paste
/// path), so encoding the Key event too would double-type them. Down+up
/// pairs per press mirror what conhost itself synthesizes from VT input;
/// egui's key-release events are ignored (their modifiers are unreliable —
/// the chord's modifier is often released first).
pub fn encode_key(key: Key, mods: Modifiers) -> Option<Vec<u8>> {
    let info = key_info(key)?;
    let ctrl = mods.ctrl || mods.command;
    let alt = mods.alt;
    if info.ch.is_some() && !ctrl && !alt {
        return None;
    }
    // AltGr: Windows reports it as Ctrl+Alt, and egui then delivers BOTH this
    // Key event and a Text event carrying the layout's real character
    // (`@ { } [ ] \ ~ |` on German/Nordic/French layouts). The char wins —
    // Windows Terminal's rule — so a Ctrl+Alt chord on a printable key must
    // encode nothing here, or every AltGr keystroke ships a spurious chord
    // record alongside the character. Deliberate Ctrl+Alt+printable chords
    // are the standard casualty every Windows terminal accepts.
    if ctrl && alt && info.ch.is_some() {
        return None;
    }
    // Best-effort UnicodeChar matching a real KEY_EVENT_RECORD; Vk+Cs are
    // what conhost/PSReadLine dispatch on, so 0 is acceptable elsewhere.
    let uc: u32 = match key {
        Key::Enter => {
            if ctrl && !alt {
                10
            } else {
                13
            }
        }
        Key::Backspace => {
            if ctrl && !alt {
                127 // the chord's real console char — inverted vs plain 8
            } else {
                8
            }
        }
        Key::Tab => 9,
        Key::Escape => 27,
        _ if ctrl && !alt => match letter_ord(key) {
            Some(i) => i as u32 + 1,
            None if key == Key::Space => 32,
            None => 0,
        },
        _ if ctrl && alt => 0,
        _ => key_char(key, mods.shift).map_or(0, |c| c as u32),
    };
    let mut cs = 0u32;
    if mods.shift {
        cs |= SHIFT_PRESSED;
    }
    if ctrl {
        cs |= LEFT_CTRL_PRESSED;
    }
    if alt {
        cs |= LEFT_ALT_PRESSED;
    }
    if info.enhanced {
        cs |= ENHANCED_KEY;
    }
    let sc = scan_code(info.vk);
    let mut out = Vec::with_capacity(48);
    for kd in [1u8, 0] {
        let _ = write!(out, "\x1b[{};{};{};{};{};1_", info.vk, sc, uc, kd, cs);
    }
    Some(out)
}

fn scan_code(vk: u16) -> u16 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC};
    unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `ESC[Vk;Sc;Uc;Kd;Cs;Rc_` frames out of an encoded buffer.
    fn parse(buf: &[u8]) -> Vec<Vec<u32>> {
        String::from_utf8_lossy(buf)
            .split('\x1b')
            .filter(|s| !s.is_empty())
            .map(|s| {
                assert!(s.starts_with('[') && s.ends_with('_'), "bad frame {s:?}");
                s[1..s.len() - 1]
                    .split(';')
                    .map(|p| p.parse().unwrap())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn ctrl_backspace_is_a_key_event_pair_with_the_real_console_char() {
        let ev = parse(&encode_key(Key::Backspace, Modifiers::CTRL).unwrap());
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0][0], 0x08, "VK_BACK");
        assert_eq!(ev[0][2], 127, "Ctrl+Backspace carries DEL, not BS");
        assert_eq!(ev[0][3], 1, "down first");
        assert_eq!(ev[1][3], 0, "then up");
        assert_eq!(ev[0][4], 0x0008, "LEFT_CTRL_PRESSED");
        assert_eq!(ev[0][5], 1, "repeat count");
    }

    #[test]
    fn unchorded_printables_are_left_to_text_events() {
        assert!(encode_key(Key::A, Modifiers::NONE).is_none());
        assert!(encode_key(Key::A, Modifiers::SHIFT).is_none());
        assert!(encode_key(Key::Space, Modifiers::NONE).is_none());
        assert!(encode_key(Key::Slash, Modifiers::NONE).is_none());
    }

    #[test]
    fn ctrl_letter_carries_the_control_code() {
        let ev = parse(&encode_key(Key::A, Modifiers::CTRL).unwrap());
        assert_eq!(ev[0][0], 0x41);
        assert_eq!(ev[0][2], 1);
        assert_eq!(ev[0][4], 0x0008);
    }

    #[test]
    fn alt_letter_carries_the_typed_char() {
        let ev = parse(&encode_key(Key::A, Modifiers::ALT).unwrap());
        assert_eq!(ev[0][2], 'a' as u32);
        assert_eq!(ev[0][4], 0x0002, "LEFT_ALT_PRESSED");
        let ev = parse(&encode_key(Key::A, Modifiers::ALT | Modifiers::SHIFT).unwrap());
        assert_eq!(ev[0][2], 'A' as u32);
        assert_eq!(ev[0][4], 0x0012);
    }

    #[test]
    fn altgr_printables_are_left_to_text_events() {
        // German-layout simulation: AltGr+Q = @, AltGr+7 = {, AltGr+8 = [,
        // AltGr+ß = \ … all arrive as Ctrl+Alt Key events PLUS a Text event
        // with the real character. The Key half must encode nothing.
        const CTRL_ALT: Modifiers = Modifiers {
            alt: true,
            ctrl: true,
            shift: false,
            mac_cmd: false,
            command: false,
        };
        assert!(encode_key(Key::Q, CTRL_ALT).is_none(), "AltGr+Q (@)");
        assert!(encode_key(Key::Num7, CTRL_ALT).is_none(), "AltGr+7 ({{)");
        assert!(encode_key(Key::Num8, CTRL_ALT).is_none(), "AltGr+8 ([)");
        assert!(encode_key(Key::OpenBracket, CTRL_ALT).is_none());
        assert!(encode_key(Key::Space, CTRL_ALT).is_none());
        // Shifted AltGr combos exist too (e.g. AltGr+Shift on some layouts).
        assert!(encode_key(Key::Q, CTRL_ALT | Modifiers::SHIFT).is_none());
        // Non-printable keys carry no AltGr character — their Ctrl+Alt
        // chords still encode (nothing to double-deliver).
        assert!(encode_key(Key::Delete, CTRL_ALT).is_some());
        assert!(encode_key(Key::Enter, CTRL_ALT).is_some());
        assert!(encode_key(Key::F5, CTRL_ALT).is_some());
        // And the US-layout goldens are untouched: single-modifier chords
        // on printables still encode.
        assert!(encode_key(Key::A, Modifiers::CTRL).is_some());
        assert!(encode_key(Key::A, Modifiers::ALT).is_some());
    }

    #[test]
    fn nav_cluster_sets_enhanced_key() {
        let ev = parse(&encode_key(Key::ArrowUp, Modifiers::NONE).unwrap());
        assert_eq!(ev[0][0], 0x26);
        assert_eq!(ev[0][4], 0x0100, "ENHANCED_KEY");
    }

    #[test]
    fn shift_enter_is_distinguishable() {
        let ev = parse(&encode_key(Key::Enter, Modifiers::SHIFT).unwrap());
        assert_eq!(ev[0][0], 0x0D);
        assert_eq!(ev[0][2], 13);
        assert_eq!(ev[0][4], 0x0010, "SHIFT_PRESSED");
    }

    #[test]
    fn ctrl_space_reaches_the_console() {
        let ev = parse(&encode_key(Key::Space, Modifiers::CTRL).unwrap());
        assert_eq!(ev[0][0], 0x20);
        assert_eq!(ev[0][2], 32);
        assert_eq!(ev[0][4], 0x0008);
    }

    #[test]
    fn high_function_keys_map_to_vk() {
        let ev = parse(&encode_key(Key::F24, Modifiers::CTRL).unwrap());
        assert_eq!(ev[0][0], 0x87);
        assert!(encode_key(Key::F25, Modifiers::CTRL).is_none());
    }

    #[test]
    fn scanner_detects_across_chunk_boundaries() {
        let mut s = ModeScanner::new();
        assert_eq!(s.feed(b"noise \x1b[?90"), None);
        assert_eq!(s.feed(b"01h more"), Some(true));
        assert_eq!(s.feed(b"\x1b[?9001l"), Some(false));
        assert_eq!(s.feed(b"\x1b[?9001x\x1b[?9001h"), Some(true));
        assert_eq!(s.feed(b"\x1b[?25h\x1b]9;9;C:\\\x07"), None);
    }

    #[test]
    fn scanner_restarts_on_embedded_escape() {
        let mut s = ModeScanner::new();
        // A truncated match followed immediately by a real one.
        assert_eq!(s.feed(b"\x1b[?9\x1b[?9001h"), Some(true));
    }
}
