//! Keyboard/mouse → VT sequence bindings (the fallback path when the session
//! has not negotiated win32-input-mode — see `crate::win32_input`).
//! Table adapted from egui_term (MIT, Ilya Shvyryalkin); anything the table
//! doesn't pin explicitly is computed by `vt_fallback` with the standard
//! xterm rules, so every modifier combination of every key encodes. Chord
//! byte values cross-checked against microsoft/terminal's terminalInput.cpp
//! (MIT).

use alacritty_terminal::term::TermMode;
use egui::{Key, Modifiers, PointerButton};

use crate::win32_input::{key_char, key_info, letter_ord};

pub type TerminalMode = TermMode;

#[derive(Clone, Hash, Debug, PartialEq, Eq)]
pub enum BindingAction {
    Copy,
    Paste,
    Char(char),
    Esc(String),
    LinkOpen,
    Ignore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputKind {
    KeyCode(Key),
    Mouse(PointerButton),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding<T> {
    pub target: T,
    pub modifiers: Modifiers,
    pub terminal_mode_include: TerminalMode,
    pub terminal_mode_exclude: TerminalMode,
}

pub type KeyboardBinding = Binding<InputKind>;
pub type MouseBinding = Binding<InputKind>;

macro_rules! generate_bindings {
    (
        $binding_type:ident;
        $(
            $input_kind:tt$(::$button:ident)?
            $(,$input_modifiers:expr)*
            $(,+$terminal_mode_include:expr)*
            $(,~$terminal_mode_exclude:expr)*
            ;$action:expr
        );*
        $(;)*
    ) => {{
        macro_rules! input_kind_match {
            (KeyboardBinding, $key:ident) => {{
                InputKind::KeyCode(Key::$key)
            }};
            (MouseBinding, $key:ident) => {{
                InputKind::Mouse(PointerButton::$key)
            }};
        }

        let mut v = Vec::new();

        $(
            let mut _input_modifiers = Modifiers::default();
            $(_input_modifiers = $input_modifiers;)*
            let mut _terminal_mode_include = TerminalMode::empty();
            $(_terminal_mode_include.insert($terminal_mode_include);)*
            let mut _terminal_mode_exclude = TerminalMode::empty();
            $(_terminal_mode_exclude.insert($terminal_mode_exclude);)*

            let binding = $binding_type {
                target: input_kind_match!($binding_type, $input_kind),
                modifiers: _input_modifiers,
                terminal_mode_include: _terminal_mode_include,
                terminal_mode_exclude: _terminal_mode_exclude,
            };

            v.push((binding, $action.into()));
        )*

        v
    }};
}

#[derive(Clone, Debug)]
pub struct BindingsLayout {
    layout: Vec<(Binding<InputKind>, BindingAction)>,
}

impl Default for BindingsLayout {
    fn default() -> Self {
        BindingsLayout::new()
    }
}

impl BindingsLayout {
    pub fn new() -> Self {
        let mut layout = Self {
            layout: default_keyboard_bindings(),
        };
        layout.add_bindings(platform_keyboard_bindings());
        layout.add_bindings(mouse_default_bindings());
        layout
    }

    pub fn add_bindings(&mut self, bindings: Vec<(Binding<InputKind>, BindingAction)>) {
        for (binding, action) in bindings {
            match self
                .layout
                .iter()
                .position(|(layout_binding, _)| layout_binding == &binding)
            {
                Some(position) => self.layout[position] = (binding, action),
                None => self.layout.push((binding, action)),
            }
        }
    }

    pub fn get_action(
        &self,
        input: InputKind,
        modifiers: Modifiers,
        terminal_mode: TerminalMode,
    ) -> BindingAction {
        for (binding, action) in &self.layout {
            let is_triggered = binding.target == input
                && modifiers.matches_exact(binding.modifiers)
                && terminal_mode.contains(binding.terminal_mode_include)
                && !terminal_mode.intersects(binding.terminal_mode_exclude);

            if is_triggered {
                return action.clone();
            }
        }

        BindingAction::Ignore
    }
}

fn default_keyboard_bindings() -> Vec<(Binding<InputKind>, BindingAction)> {
    generate_bindings!(
        KeyboardBinding;
        // NONE MODIFIERS
        Enter;     BindingAction::Char('\x0d');
        Backspace; BindingAction::Char('\x7f');
        Escape;    BindingAction::Char('\x1b');
        Tab;       BindingAction::Char('\x09');
        Insert;    BindingAction::Esc("\x1b[2~".into());
        Delete;    BindingAction::Esc("\x1b[3~".into());
        PageUp;    BindingAction::Esc("\x1b[5~".into());
        PageDown;  BindingAction::Esc("\x1b[6~".into());
        F1;        BindingAction::Esc("\x1bOP".into());
        F2;        BindingAction::Esc("\x1bOQ".into());
        F3;        BindingAction::Esc("\x1bOR".into());
        F4;        BindingAction::Esc("\x1bOS".into());
        F5;        BindingAction::Esc("\x1b[15~".into());
        F6;        BindingAction::Esc("\x1b[17~".into());
        F7;        BindingAction::Esc("\x1b[18~".into());
        F8;        BindingAction::Esc("\x1b[19~".into());
        F9;        BindingAction::Esc("\x1b[20~".into());
        F10;       BindingAction::Esc("\x1b[21~".into());
        F11;       BindingAction::Esc("\x1b[23~".into());
        F12;       BindingAction::Esc("\x1b[24~".into());
        // APP_CURSOR Excluding
        End,        ~TerminalMode::APP_CURSOR; BindingAction::Esc("\x1b[F".into());
        Home,       ~TerminalMode::APP_CURSOR; BindingAction::Esc("\x1b[H".into());
        ArrowUp,    ~TerminalMode::APP_CURSOR; BindingAction::Esc("\x1b[A".into());
        ArrowDown,  ~TerminalMode::APP_CURSOR; BindingAction::Esc("\x1b[B".into());
        ArrowLeft,  ~TerminalMode::APP_CURSOR; BindingAction::Esc("\x1b[D".into());
        ArrowRight, ~TerminalMode::APP_CURSOR; BindingAction::Esc("\x1b[C".into());
        // APP_CURSOR Including
        End,        +TerminalMode::APP_CURSOR; BindingAction::Esc("\x1bOF".into());
        Home,       +TerminalMode::APP_CURSOR; BindingAction::Esc("\x1bOH".into());
        ArrowUp,    +TerminalMode::APP_CURSOR; BindingAction::Esc("\x1bOA".into());
        ArrowDown,  +TerminalMode::APP_CURSOR; BindingAction::Esc("\x1bOB".into());
        ArrowLeft,  +TerminalMode::APP_CURSOR; BindingAction::Esc("\x1bOD".into());
        ArrowRight, +TerminalMode::APP_CURSOR; BindingAction::Esc("\x1bOC".into());
        // Chords with non-formulaic encodings (everything formulaic — modified
        // nav/F-keys, Ctrl/Alt character math — is computed by vt_fallback).
        Enter,      Modifiers::SHIFT; BindingAction::Char('\x0d');
        Escape,     Modifiers::SHIFT; BindingAction::Char('\x1b');
        Backspace,  Modifiers::SHIFT; BindingAction::Char('\x7f');
        Backspace,  Modifiers::CTRL; BindingAction::Char('\x08');
        Backspace,  Modifiers::ALT; BindingAction::Esc("\x1b\x7f".into());
        Tab,        Modifiers::SHIFT; BindingAction::Esc("\x1b[Z".into());
    )
}

fn platform_keyboard_bindings() -> Vec<(Binding<InputKind>, BindingAction)> {
    generate_bindings!(
        KeyboardBinding;
        C, Modifiers::SHIFT | Modifiers::COMMAND; BindingAction::Copy;
        V, Modifiers::SHIFT | Modifiers::COMMAND; BindingAction::Paste;
    )
}

fn mouse_default_bindings() -> Vec<(Binding<InputKind>, BindingAction)> {
    generate_bindings!(
        MouseBinding;
        Primary, Modifiers::COMMAND; BindingAction::LinkOpen;
    )
}

/// Keys that take the xterm modifier formula: `CSI 1;m X` for the letter
/// form, `CSI n;m ~` for the tilde form, with m = 1+Shift(1)+Alt(2)+Ctrl(4).
enum CsiKey {
    Letter(u8),
    Tilde(u8),
}

fn csi_key(key: Key) -> Option<CsiKey> {
    use Key::*;
    Some(match key {
        ArrowUp => CsiKey::Letter(b'A'),
        ArrowDown => CsiKey::Letter(b'B'),
        ArrowRight => CsiKey::Letter(b'C'),
        ArrowLeft => CsiKey::Letter(b'D'),
        Home => CsiKey::Letter(b'H'),
        End => CsiKey::Letter(b'F'),
        F1 => CsiKey::Letter(b'P'),
        F2 => CsiKey::Letter(b'Q'),
        F3 => CsiKey::Letter(b'R'),
        F4 => CsiKey::Letter(b'S'),
        Insert => CsiKey::Tilde(2),
        Delete => CsiKey::Tilde(3),
        PageUp => CsiKey::Tilde(5),
        PageDown => CsiKey::Tilde(6),
        F5 => CsiKey::Tilde(15),
        F6 => CsiKey::Tilde(17),
        F7 => CsiKey::Tilde(18),
        F8 => CsiKey::Tilde(19),
        F9 => CsiKey::Tilde(20),
        F10 => CsiKey::Tilde(21),
        F11 => CsiKey::Tilde(23),
        F12 => CsiKey::Tilde(24),
        F13 => CsiKey::Tilde(25),
        F14 => CsiKey::Tilde(26),
        F15 => CsiKey::Tilde(28),
        F16 => CsiKey::Tilde(29),
        F17 => CsiKey::Tilde(31),
        F18 => CsiKey::Tilde(32),
        F19 => CsiKey::Tilde(33),
        F20 => CsiKey::Tilde(34),
        _ => return None,
    })
}

/// The byte a Ctrl chord produces, per xterm: letters fold to 0x01–0x1A,
/// the `@[\]^_` column to 0x00/0x1B–0x1F, `/` and `?` to 0x1F/0x7F, and
/// anything else passes through unchanged (Ctrl+1 types '1').
fn vt_ctrl_byte(key: Key, shift: bool) -> Option<u8> {
    use Key::*;
    Some(match key {
        Space | Num2 | Backtick => 0x00,
        Num3 | OpenBracket | OpenCurlyBracket => 0x1b,
        Num4 | Backslash | Pipe => 0x1c,
        Num5 | CloseBracket | CloseCurlyBracket => 0x1d,
        Num6 => 0x1e,
        Num7 | Slash | Minus => 0x1f,
        Num8 | Questionmark => 0x7f,
        Enter => b'\n',
        Tab => b'\t',
        Backspace => 0x08,
        Escape => 0x1b,
        _ => {
            if let Some(i) = letter_ord(key) {
                1 + i
            } else {
                let c = key_char(key, shift)?;
                if c.is_ascii() {
                    c as u8
                } else {
                    return None;
                }
            }
        }
    })
}

/// Encode any (key, modifiers) the binding table doesn't pin, with standard
/// xterm rules: the CSI modifier formula for nav/F-keys, ESC-prefixing for
/// Alt (word motions, Alt+. history-arg, …), and Ctrl character math.
/// Returns None when the combination genuinely has no VT encoding.
pub fn vt_fallback(key: Key, mods: Modifiers) -> Option<Vec<u8>> {
    let ctrl = mods.ctrl || mods.command;
    let alt = mods.alt;
    let shift = mods.shift;
    // AltGr: Windows reports it as Ctrl+Alt, and egui delivers BOTH the Key
    // event and a Text event with the layout's real character. The char wins
    // (Windows Terminal's rule) — encoding the chord too would ship a meta
    // sequence (e.g. `ESC DC1` for AltGr+Q) ahead of every `@ { } [ ] \ ~ |`
    // an ssh user on a German/Nordic/French layout types. Nav/F-keys carry
    // no printable, so their Ctrl+Alt chords keep the CSI formula below.
    if ctrl && alt && key_info(key).is_some_and(|i| i.ch.is_some()) {
        return None;
    }
    if let Some(spec) = csi_key(key) {
        let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
        return Some(
            match (spec, m) {
                (CsiKey::Letter(c), 1) => format!("\x1b[{}", c as char),
                (CsiKey::Letter(c), m) => format!("\x1b[1;{m}{}", c as char),
                (CsiKey::Tilde(n), 1) => format!("\x1b[{n}~"),
                (CsiKey::Tilde(n), m) => format!("\x1b[{n};{m}~"),
            }
            .into_bytes(),
        );
    }
    if alt {
        let base: Option<Vec<u8>> = if ctrl {
            vt_ctrl_byte(key, shift).map(|b| vec![b])
        } else {
            match key {
                Key::Enter => Some(b"\r".to_vec()),
                Key::Backspace => Some(b"\x7f".to_vec()),
                Key::Tab => Some(b"\t".to_vec()),
                Key::Escape => Some(b"\x1b".to_vec()),
                Key::Space => Some(b" ".to_vec()),
                _ => key_char(key, shift).map(|c| c.to_string().into_bytes()),
            }
        };
        return base.map(|b| {
            let mut v = vec![0x1b];
            v.extend(b);
            v
        });
    }
    if ctrl {
        return vt_ctrl_byte(key, shift).map(|b| vec![b]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fb(key: Key, mods: Modifiers) -> Vec<u8> {
        vt_fallback(key, mods).unwrap_or_else(|| panic!("no encoding for {key:?}+{mods:?}"))
    }

    const CTRL_ALT: Modifiers = Modifiers {
        alt: true,
        ctrl: true,
        shift: false,
        mac_cmd: false,
        command: false,
    };

    #[test]
    fn table_pins_the_backspace_family() {
        let b = BindingsLayout::new();
        let m = TermMode::empty();
        let act = |mods| b.get_action(InputKind::KeyCode(Key::Backspace), mods, m);
        assert_eq!(act(Modifiers::NONE), BindingAction::Char('\x7f'));
        assert_eq!(act(Modifiers::CTRL), BindingAction::Char('\x08'));
        assert_eq!(act(Modifiers::ALT), BindingAction::Esc("\x1b\x7f".into()));
    }

    #[test]
    fn ctrl_character_math() {
        assert_eq!(fb(Key::A, Modifiers::CTRL), vec![0x01]);
        assert_eq!(fb(Key::Z, Modifiers::CTRL), vec![0x1a]);
        assert_eq!(fb(Key::Space, Modifiers::CTRL), vec![0x00]);
        assert_eq!(fb(Key::Num2, Modifiers::CTRL), vec![0x00]);
        assert_eq!(fb(Key::Num7, Modifiers::CTRL), vec![0x1f]);
        assert_eq!(fb(Key::Num8, Modifiers::CTRL), vec![0x7f]);
        assert_eq!(fb(Key::Slash, Modifiers::CTRL), vec![0x1f]);
        assert_eq!(fb(Key::Minus, Modifiers::CTRL), vec![0x1f]);
        assert_eq!(fb(Key::OpenBracket, Modifiers::CTRL), vec![0x1b]);
        assert_eq!(fb(Key::Backslash, Modifiers::CTRL), vec![0x1c]);
        assert_eq!(fb(Key::CloseBracket, Modifiers::CTRL), vec![0x1d]);
        assert_eq!(fb(Key::Enter, Modifiers::CTRL), b"\n".to_vec());
        // Ctrl+Shift+letter folds like Ctrl+letter (xterm behavior).
        assert_eq!(fb(Key::A, Modifiers::CTRL | Modifiers::SHIFT), vec![0x01]);
        // Unfoldable chars pass through.
        assert_eq!(fb(Key::Num1, Modifiers::CTRL), b"1".to_vec());
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(fb(Key::A, Modifiers::ALT), b"\x1ba".to_vec());
        assert_eq!(fb(Key::A, Modifiers::ALT | Modifiers::SHIFT), b"\x1bA".to_vec());
        // Ctrl+Alt+Enter has no printable — the meta chord still encodes.
        assert_eq!(fb(Key::Enter, CTRL_ALT), vec![0x1b, b'\n']);
        assert_eq!(fb(Key::Enter, Modifiers::ALT), b"\x1b\r".to_vec());
        assert_eq!(fb(Key::Period, Modifiers::ALT), b"\x1b.".to_vec());
        assert_eq!(
            fb(Key::Period, Modifiers::ALT | Modifiers::SHIFT),
            b"\x1b>".to_vec()
        );
        assert_eq!(fb(Key::Num3, Modifiers::ALT), b"\x1b3".to_vec());
    }

    #[test]
    fn csi_modifier_formula() {
        assert_eq!(fb(Key::ArrowLeft, Modifiers::CTRL), b"\x1b[1;5D".to_vec());
        assert_eq!(fb(Key::ArrowUp, Modifiers::SHIFT), b"\x1b[1;2A".to_vec());
        assert_eq!(fb(Key::Home, Modifiers::SHIFT), b"\x1b[1;2H".to_vec());
        assert_eq!(fb(Key::End, Modifiers::CTRL), b"\x1b[1;5F".to_vec());
        assert_eq!(
            fb(Key::Delete, Modifiers::CTRL | Modifiers::SHIFT),
            b"\x1b[3;6~".to_vec()
        );
        // The old table sent Shift+Delete's code for Alt+Insert; the formula
        // fixes it.
        assert_eq!(fb(Key::Insert, Modifiers::ALT), b"\x1b[2;3~".to_vec());
        assert_eq!(fb(Key::F1, Modifiers::CTRL), b"\x1b[1;5P".to_vec());
        assert_eq!(fb(Key::F5, Modifiers::SHIFT), b"\x1b[15;2~".to_vec());
        assert_eq!(
            fb(
                Key::ArrowRight,
                Modifiers::CTRL | Modifiers::ALT | Modifiers::SHIFT
            ),
            b"\x1b[1;8C".to_vec()
        );
        // F13–F20 exist even unmodified.
        assert_eq!(fb(Key::F13, Modifiers::NONE), b"\x1b[25~".to_vec());
        assert_eq!(fb(Key::F20, Modifiers::NONE), b"\x1b[34~".to_vec());
    }

    #[test]
    fn plain_printables_have_no_fallback() {
        // They arrive as Text events; a fallback here would double-type.
        assert!(vt_fallback(Key::A, Modifiers::NONE).is_none());
        assert!(vt_fallback(Key::Slash, Modifiers::SHIFT).is_none());
        assert!(vt_fallback(Key::Space, Modifiers::NONE).is_none());
    }

    #[test]
    fn altgr_printables_have_no_fallback() {
        // German-layout simulation over ssh (the VT path): AltGr+Q = @,
        // AltGr+7 = {, AltGr+8 = [, AltGr+ß = \ … each arrives as a
        // Ctrl+Alt Key event PLUS a Text event carrying the character. The
        // Key half must encode nothing, or readline receives a meta chord
        // (`ESC DC1` for AltGr+Q) before every such character.
        assert!(vt_fallback(Key::Q, CTRL_ALT).is_none(), "AltGr+Q (@)");
        assert!(vt_fallback(Key::B, CTRL_ALT).is_none());
        assert!(vt_fallback(Key::Num7, CTRL_ALT).is_none(), "AltGr+7 ({{)");
        assert!(vt_fallback(Key::Num8, CTRL_ALT).is_none(), "AltGr+8 ([)");
        assert!(vt_fallback(Key::OpenBracket, CTRL_ALT).is_none());
        assert!(vt_fallback(Key::Q, CTRL_ALT | Modifiers::SHIFT).is_none());
        // Nav keys carry no printable: the CSI modifier formula survives.
        assert_eq!(fb(Key::ArrowRight, CTRL_ALT), b"\x1b[1;7C".to_vec());
        assert_eq!(fb(Key::Delete, CTRL_ALT), b"\x1b[3;7~".to_vec());
    }
}
