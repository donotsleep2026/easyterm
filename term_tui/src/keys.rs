//! The frontend's own keys, in one place.
//!
//! Inside a session **every** key belongs to the remote: a terminal that
//! swallows `q`, `Esc` or `Ctrl-C` breaks vim, less and the shell. So the set
//! of chords this frontend reserves is small, fixed and documented, and it
//! lives here so the help overlay, the status bar, the help text and the tests
//! all read the same table:
//!
//! | where | key | what |
//! |---|---|---|
//! | session | `Ctrl-]` (configurable) | leave to the session manager, session keeps running |
//! | session | `Shift-PgUp/PgDn`, `Shift-Up/Down`, `Shift-Home/End` | scroll the mirror's scrollback |
//! | manager | `j`/`k`/arrows, `Enter`, `n`, `x`, `r`, `q`, `?`, digits | see [`manager_command`] |
//! | list | `j`/`k`/arrows, `Enter`, `/` filter, `Esc` | see [`list_key`] |
//!
//! The detach chord is configurable because a terminal emulator's shortcut
//! table is the user's, not ours (Windows Terminal claims `Ctrl-+`/`Ctrl--`):
//! `EASYTERM_TUI_DETACH_KEY=ctrl-^`, `f12`, `alt-q`, … The old name
//! `EASYTERM_TUI_CLOSE_KEY` still works, so an existing environment keeps
//! working across this change.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::manager::Command as ManagerCommand;

/// A chord this frontend reserves for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chord {
    /// `Ctrl-` plus a character.
    Ctrl(char),
    /// `Alt-` plus a character.
    Alt(char),
    /// A function key by number.
    F(u8),
}

/// The default detach chord: telnet's escape, unclaimed by Windows Terminal and
/// by the usual interactive programs.
pub const DEFAULT_DETACH: Chord = Chord::Ctrl(']');

impl Chord {
    /// Parse a binding such as `ctrl-]`, `ctrl-^`, `alt-q`, `f12`.
    pub fn parse(spec: &str) -> Option<Self> {
        let lower = spec.trim().to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("ctrl-") {
            let c = match rest {
                "space" => ' ',
                "caret" | "6" => '^',
                other => other.chars().next()?,
            };
            if rest.chars().count() > 1 && !matches!(rest, "space" | "caret" | "6") {
                return None;
            }
            return Some(Chord::Ctrl(c));
        }
        if let Some(rest) = lower.strip_prefix("alt-") {
            let c = match rest {
                "space" => ' ',
                other => other.chars().next()?,
            };
            return Some(Chord::Alt(c));
        }
        if let Some(num) = lower.strip_prefix('f') {
            return num.parse::<u8>().ok().map(Chord::F);
        }
        None
    }

    /// The chord in force, from the environment or the default.
    ///
    /// `EASYTERM_TUI_DETACH_KEY` wins; `EASYTERM_TUI_CLOSE_KEY` is the name this
    /// chord had while it still closed the session, and is still honoured. An
    /// unparsable value is reported once and ignored rather than silently
    /// becoming some other key.
    pub fn from_env() -> Self {
        for var in ["EASYTERM_TUI_DETACH_KEY", "EASYTERM_TUI_CLOSE_KEY"] {
            let Ok(raw) = std::env::var(var) else { continue };
            if raw.trim().is_empty() {
                continue;
            }
            match Chord::parse(&raw) {
                Some(chord) => return chord,
                None => log::warn!("ignoring {var}={raw:?}; using {}", DEFAULT_DETACH.label()),
            }
        }
        DEFAULT_DETACH
    }

    /// Whether this key event is the chord.
    pub fn matches(&self, k: &KeyEvent) -> bool {
        match *self {
            Chord::Ctrl(ch) => {
                k.code == KeyCode::Char(ch) && k.modifiers.contains(KeyModifiers::CONTROL)
            }
            Chord::Alt(ch) => k.code == KeyCode::Char(ch) && k.modifiers.contains(KeyModifiers::ALT),
            Chord::F(n) => k.code == KeyCode::F(n),
        }
    }

    /// How to name it in the status bar and the help overlay.
    pub fn label(&self) -> String {
        match *self {
            Chord::Ctrl(ch) => format!("Ctrl-{ch}"),
            Chord::Alt(ch) => format!("Alt-{ch}"),
            Chord::F(n) => format!("F{n}"),
        }
    }
}

/// How to move through the mirror's scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    PageUp,
    PageDown,
    LineUp,
    LineDown,
    Top,
    Bottom,
}

/// What a key press means while a session is on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionKey {
    /// Leave to the manager; the session keeps running.
    Detach,
    /// Move through the local mirror's scrollback (the remote never sees this).
    Scroll(Scroll),
    /// Bytes for the remote.
    Input(Vec<u8>),
    /// A key with nothing to send (a modifier press, a dead key).
    Ignore,
}

/// Classify a key press inside a live session.
///
/// Order matters: the detach chord first (it may otherwise look like ordinary
/// input), then the scroll chords, then everything else as remote input.
pub fn session_key(k: &KeyEvent, detach: Chord) -> SessionKey {
    if detach.matches(k) {
        return SessionKey::Detach;
    }
    // Scrollback is on Shift because no full-screen program wants
    // Shift-PageUp (the terminal's own scrollback, when we do not consume it,
    // is the terminal emulator's — which only knows the frames we drew, not
    // the remote's history).
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    let scroll = match k.code {
        KeyCode::PageUp if shift => Some(Scroll::PageUp),
        KeyCode::PageDown if shift => Some(Scroll::PageDown),
        KeyCode::Up if shift => Some(Scroll::LineUp),
        KeyCode::Down if shift => Some(Scroll::LineDown),
        KeyCode::Home if shift => Some(Scroll::Top),
        KeyCode::End if shift => Some(Scroll::Bottom),
        _ => None,
    };
    if let Some(s) = scroll {
        return SessionKey::Scroll(s);
    }
    match encode_key(k) {
        Some(bytes) => SessionKey::Input(bytes),
        None => SessionKey::Ignore,
    }
}

/// Encode a key press the way a terminal would, or `None` for a key that
/// produces no bytes.
///
/// The previous hand-written `match` forwarded a dozen keys and **silently
/// dropped every other one** — which is why `Ctrl-D` did nothing: it fell
/// through to `_ => {}`. A terminal may not decide which control characters a
/// user is allowed to send, so this covers the whole `KeyCode` space and the
/// Ctrl/Alt encodings (`Ctrl-A`..`Ctrl-Z` → `0x01`..`0x1a`, `Ctrl-]`/`Ctrl-4` →
/// `0x1d`, `Ctrl-6` → `0x1e`, `Ctrl--` → `0x1f`, `Ctrl-?` → `0x7f`).
pub fn encode_key(k: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);

    let mut out: Vec<u8> = Vec::new();
    match k.code {
        KeyCode::Char(ch) => {
            if ctrl {
                match control_byte(ch) {
                    Some(byte) => out.push(byte),
                    None => {
                        // Anything else (`Ctrl-ä`, a CJK char) has no control
                        // code; send the character itself rather than nothing.
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        if alt {
                            out.insert(0, 0x1b);
                        }
                        return Some(out);
                    }
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        // Shift-Tab is its own sequence, not a modified Tab.
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::F(n) => out.extend_from_slice(function_key(n)?.as_bytes()),
        // Modifier-only presses and anything without an encoding send nothing.
        KeyCode::Null | KeyCode::CapsLock | KeyCode::ScrollLock | KeyCode::NumLock
        | KeyCode::PrintScreen | KeyCode::Pause | KeyCode::Menu | KeyCode::KeypadBegin
        | KeyCode::Media(_) | KeyCode::Modifier(_) => return None,
    }
    if alt && k.code != KeyCode::Esc {
        out.insert(0, 0x1b);
    }
    Some(out)
}

/// The byte `Ctrl-<ch>` sends, when the pair has a control code.
///
/// The alphabet is the rule (`Ctrl-A`..`Ctrl-Z` → `0x01`..`0x1a`, because a
/// control character is the letter with its top bits masked off), and the
/// punctuation/digit chords are the traditional aliases the same rule does not
/// reach: `Ctrl-Space`/`Ctrl-2`/`Ctrl-@` are NUL, `Ctrl-3`/`Ctrl-[` are ESC,
/// `Ctrl-4`/`Ctrl-\` are FS, `Ctrl-5`/`Ctrl-]` are GS, `Ctrl-6`/`Ctrl-^` are RS
/// (crossterm reports `Ctrl-6` as the character `6`, so it must be listed),
/// `Ctrl-7`/`Ctrl--`/`Ctrl-/` are US, `Ctrl-8`/`Ctrl-?` are DEL.
fn control_byte(ch: char) -> Option<u8> {
    let c = ch.to_ascii_uppercase() as u32;
    Some(match c {
        0x40..=0x5f => (c as u8) & 0x1f, // @ A-Z [ \ ] ^ _
        0x20 => 0x00,                    // Ctrl-Space
        0x3f => 0x7f,                    // Ctrl-?
        0x2f => 0x1f,                    // Ctrl-/
        0x2d => 0x1f,                    // Ctrl--
        0x32 => 0x00,                    // Ctrl-2
        0x33 => 0x1b,                    // Ctrl-3
        0x34 => 0x1c,                    // Ctrl-4
        0x35 => 0x1d,                    // Ctrl-5
        0x36 => 0x1e,                    // Ctrl-6
        0x37 => 0x1f,                    // Ctrl-7
        0x38 => 0x7f,                    // Ctrl-8
        _ => return None,
    })
}

/// The `xterm` sequence for a function key, or `None` past F12 (which has no
/// portable encoding).
fn function_key(n: u8) -> Option<&'static str> {
    Some(match n {
        1 => "\x1bOP",
        2 => "\x1bOQ",
        3 => "\x1bOR",
        4 => "\x1bOS",
        5 => "\x1b[15~",
        6 => "\x1b[17~",
        7 => "\x1b[18~",
        8 => "\x1b[19~",
        9 => "\x1b[20~",
        10 => "\x1b[21~",
        11 => "\x1b[23~",
        12 => "\x1b[24~",
        _ => return None,
    })
}

/// Manager screen: the key → [`ManagerCommand`] table.
pub fn manager_command(k: &KeyEvent) -> ManagerCommand {
    match k.code {
        KeyCode::Char('j') | KeyCode::Down => ManagerCommand::Move(1),
        KeyCode::Char('k') | KeyCode::Up => ManagerCommand::Move(-1),
        KeyCode::PageDown => ManagerCommand::Move(10),
        KeyCode::PageUp => ManagerCommand::Move(-10),
        KeyCode::Char('g') | KeyCode::Home => ManagerCommand::First,
        KeyCode::Char('G') | KeyCode::End => ManagerCommand::Last,
        KeyCode::Tab => ManagerCommand::Attention(1),
        KeyCode::BackTab => ManagerCommand::Attention(-1),
        KeyCode::Enter | KeyCode::Char('l') => ManagerCommand::Select,
        KeyCode::Char(ch @ '1'..='9') => {
            ManagerCommand::SelectNumber((ch as u8 - b'0') as usize)
        }
        // `Esc` goes back to the session the manager was entered from; `d` is
        // the same thing for people used to tmux's detach.
        KeyCode::Esc | KeyCode::Char('d') => ManagerCommand::Back,
        KeyCode::Char('n') | KeyCode::Char('c') => ManagerCommand::NewSession,
        KeyCode::Char('x') | KeyCode::Delete => ManagerCommand::Close,
        KeyCode::Char('r') => ManagerCommand::Rename,
        KeyCode::Char('a') => ManagerCommand::Agent,
        KeyCode::Char('?') | KeyCode::Char('h') => ManagerCommand::Help,
        KeyCode::Char('q') => ManagerCommand::Quit,
        _ => ManagerCommand::None,
    }
}

/// Help is a local screen: scrolling never sends input to a terminal session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpKey {
    Move(i32),
    Page(i32),
    First,
    Last,
    Back,
    None,
}

pub fn help_key(k: &KeyEvent) -> HelpKey {
    match k.code {
        KeyCode::Char('j') | KeyCode::Down => HelpKey::Move(1),
        KeyCode::Char('k') | KeyCode::Up => HelpKey::Move(-1),
        KeyCode::PageDown | KeyCode::Char(' ') => HelpKey::Page(1),
        KeyCode::PageUp => HelpKey::Page(-1),
        KeyCode::Home | KeyCode::Char('g') => HelpKey::First,
        KeyCode::End | KeyCode::Char('G') => HelpKey::Last,
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => HelpKey::Back,
        _ => HelpKey::None,
    }
}

/// What a key press means on a session list (the saved-host picker, or the DSH
/// session list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListKey {
    Move(i32),
    Page(i32),
    First,
    Last,
    Open,
    /// Start filtering (`/`).
    Filter,
    /// Keep typing the filter (only while filtering).
    FilterChar(char),
    FilterBackspace,
    /// Leave the list.
    Back,
    None,
}

/// Classify a key press on a list. `filtering` is true while the `/` filter
/// has the keyboard: then every printable key extends the filter instead of
/// moving the selection.
pub fn list_key(k: &KeyEvent, filtering: bool) -> ListKey {
    if filtering {
        return match k.code {
            KeyCode::Esc => ListKey::Filter,
            KeyCode::Enter => ListKey::Open,
            KeyCode::Backspace => ListKey::FilterBackspace,
            KeyCode::Down => ListKey::Move(1),
            KeyCode::Up => ListKey::Move(-1),
            KeyCode::Char(ch) => ListKey::FilterChar(ch),
            _ => ListKey::None,
        };
    }
    match k.code {
        KeyCode::Char('j') | KeyCode::Down => ListKey::Move(1),
        KeyCode::Char('k') | KeyCode::Up => ListKey::Move(-1),
        KeyCode::PageDown => ListKey::Page(10),
        KeyCode::PageUp => ListKey::Page(-10),
        KeyCode::Char('g') | KeyCode::Home => ListKey::First,
        KeyCode::Char('G') | KeyCode::End => ListKey::Last,
        KeyCode::Enter | KeyCode::Char('l') => ListKey::Open,
        KeyCode::Char('/') => ListKey::Filter,
        KeyCode::Char('q') | KeyCode::Esc => ListKey::Back,
        _ => ListKey::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// Everything a terminal user can type must reach the remote. The bug this
    /// pins down: the old hand-written match forwarded a dozen keys and dropped
    /// the rest, so `Ctrl-D` (EOF) did nothing at all.
    #[test]
    fn control_characters_are_encoded() {
        let ctrl = KeyModifiers::CONTROL;
        assert_eq!(encode_key(&key(KeyCode::Char('d'), ctrl)), Some(vec![0x04])); // EOF
        assert_eq!(encode_key(&key(KeyCode::Char('c'), ctrl)), Some(vec![0x03])); // SIGINT
        assert_eq!(encode_key(&key(KeyCode::Char('a'), ctrl)), Some(vec![0x01]));
        assert_eq!(encode_key(&key(KeyCode::Char('z'), ctrl)), Some(vec![0x1a]));
        assert_eq!(encode_key(&key(KeyCode::Char(']'), ctrl)), Some(vec![0x1d]));
        assert_eq!(encode_key(&key(KeyCode::Char('\\'), ctrl)), Some(vec![0x1c]));
        assert_eq!(encode_key(&key(KeyCode::Char(' '), ctrl)), Some(vec![0x00]));
        assert_eq!(encode_key(&key(KeyCode::Char('?'), ctrl)), Some(vec![0x7f]));
        // crossterm reports the digit/punctuation aliases as characters, so the
        // traditional codes must be listed, not derived.
        assert_eq!(encode_key(&key(KeyCode::Char('6'), ctrl)), Some(vec![0x1e]));
        assert_eq!(encode_key(&key(KeyCode::Char('2'), ctrl)), Some(vec![0x00]));
        assert_eq!(encode_key(&key(KeyCode::Char('-'), ctrl)), Some(vec![0x1f]));
        assert_eq!(encode_key(&key(KeyCode::Char('/'), ctrl)), Some(vec![0x1f]));
        // Upper case behaves like lower case (a terminal has one control code).
        assert_eq!(encode_key(&key(KeyCode::Char('D'), ctrl)), Some(vec![0x04]));

        // Plain keys, and the keys the old match forgot.
        assert_eq!(
            encode_key(&key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(vec![b'q'])
        );
        assert_eq!(encode_key(&key(KeyCode::Enter, KeyModifiers::NONE)), Some(vec![b'\r']));
        assert_eq!(encode_key(&key(KeyCode::Tab, KeyModifiers::NONE)), Some(vec![b'\t']));
        assert_eq!(encode_key(&key(KeyCode::Esc, KeyModifiers::NONE)), Some(vec![0x1b]));
        assert_eq!(
            encode_key(&key(KeyCode::Delete, KeyModifiers::NONE)),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::F(5), KeyModifiers::NONE)),
            Some(b"\x1b[15~".to_vec())
        );

        // Alt is ESC-prefixed; a non-ASCII char keeps its UTF-8 bytes.
        assert_eq!(
            encode_key(&key(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(vec![0x1b, b'x'])
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('中'), KeyModifiers::NONE)),
            Some("中".as_bytes().to_vec())
        );

        // Nothing to send is reported, not silently dropped as a wrong byte.
        assert_eq!(encode_key(&key(KeyCode::Null, KeyModifiers::NONE)), None);
        assert_eq!(encode_key(&key(KeyCode::F(24), KeyModifiers::NONE)), None);
    }

    #[test]
    fn the_detach_chord_is_configurable_and_parsed_strictly() {
        assert_eq!(Chord::parse("ctrl-]"), Some(Chord::Ctrl(']')));
        assert_eq!(Chord::parse("CTRL-^"), Some(Chord::Ctrl('^')));
        assert_eq!(Chord::parse("ctrl-space"), Some(Chord::Ctrl(' ')));
        assert_eq!(Chord::parse("alt-q"), Some(Chord::Alt('q')));
        assert_eq!(Chord::parse("f12"), Some(Chord::F(12)));
        assert_eq!(Chord::parse("ctrl-"), None);
        assert_eq!(Chord::parse("nonsense"), None);
        assert_eq!(Chord::parse("ctrl-ab"), None, "not a single key");

        assert_eq!(DEFAULT_DETACH.label(), "Ctrl-]");
        assert!(DEFAULT_DETACH.matches(&key(KeyCode::Char(']'), KeyModifiers::CONTROL)));
        assert!(!DEFAULT_DETACH.matches(&key(KeyCode::Char(']'), KeyModifiers::NONE)));
        assert!(Chord::Alt('q').matches(&key(KeyCode::Char('q'), KeyModifiers::ALT)));
        assert!(Chord::F(12).matches(&key(KeyCode::F(12), KeyModifiers::NONE)));
    }

    #[test]
    fn the_detach_chord_wins_over_remote_input() {
        // Ctrl-] is also a control character (0x1d); the reserved chord must be
        // read first, or the user could never leave a session.
        assert_eq!(
            session_key(&key(KeyCode::Char(']'), KeyModifiers::CONTROL), DEFAULT_DETACH),
            SessionKey::Detach
        );
        assert_eq!(
            session_key(&key(KeyCode::Char(']'), KeyModifiers::CONTROL), Chord::Ctrl('^')),
            SessionKey::Input(vec![0x1d]),
            "with another chord, Ctrl-] is ordinary remote input again"
        );
    }

    #[test]
    fn shift_scroll_keys_never_reach_the_remote() {
        let shift = KeyModifiers::SHIFT;
        for (code, want) in [
            (KeyCode::PageUp, Scroll::PageUp),
            (KeyCode::PageDown, Scroll::PageDown),
            (KeyCode::Up, Scroll::LineUp),
            (KeyCode::Down, Scroll::LineDown),
            (KeyCode::Home, Scroll::Top),
            (KeyCode::End, Scroll::Bottom),
        ] {
            assert_eq!(session_key(&key(code, shift), DEFAULT_DETACH), SessionKey::Scroll(want));
        }
        // Without Shift they are the program's keys, unchanged.
        assert_eq!(
            session_key(&key(KeyCode::PageUp, KeyModifiers::NONE), DEFAULT_DETACH),
            SessionKey::Input(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            session_key(&key(KeyCode::Up, KeyModifiers::NONE), DEFAULT_DETACH),
            SessionKey::Input(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn the_manager_has_no_chord_except_its_own_letters() {
        // Every key is a plain key here: nothing else is listening, so the
        // usual vocabulary is free.
        assert_eq!(manager_command(&key(KeyCode::Char('j'), KeyModifiers::NONE)), ManagerCommand::Move(1));
        assert_eq!(manager_command(&key(KeyCode::Down, KeyModifiers::NONE)), ManagerCommand::Move(1));
        assert_eq!(manager_command(&key(KeyCode::Char('k'), KeyModifiers::NONE)), ManagerCommand::Move(-1));
        assert_eq!(manager_command(&key(KeyCode::Enter, KeyModifiers::NONE)), ManagerCommand::Select);
        assert_eq!(manager_command(&key(KeyCode::Char('3'), KeyModifiers::NONE)), ManagerCommand::SelectNumber(3));
        assert_eq!(manager_command(&key(KeyCode::Esc, KeyModifiers::NONE)), ManagerCommand::Back);
        assert_eq!(manager_command(&key(KeyCode::Char('n'), KeyModifiers::NONE)), ManagerCommand::NewSession);
        assert_eq!(manager_command(&key(KeyCode::Char('x'), KeyModifiers::NONE)), ManagerCommand::Close);
        assert_eq!(manager_command(&key(KeyCode::Char('r'), KeyModifiers::NONE)), ManagerCommand::Rename);
        assert_eq!(manager_command(&key(KeyCode::Char('a'), KeyModifiers::NONE)), ManagerCommand::Agent);
        assert_eq!(manager_command(&key(KeyCode::Char('?'), KeyModifiers::NONE)), ManagerCommand::Help);
        assert_eq!(manager_command(&key(KeyCode::Char('q'), KeyModifiers::NONE)), ManagerCommand::Quit);
        assert_eq!(manager_command(&key(KeyCode::Char('z'), KeyModifiers::NONE)), ManagerCommand::None);
        assert_eq!(manager_command(&key(KeyCode::Tab, KeyModifiers::NONE)), ManagerCommand::Attention(1));
        assert_eq!(manager_command(&key(KeyCode::BackTab, KeyModifiers::SHIFT)), ManagerCommand::Attention(-1));
    }

    #[test]
    fn help_has_its_own_scroll_and_close_keys() {
        let plain = KeyModifiers::NONE;
        assert_eq!(help_key(&key(KeyCode::Down, plain)), HelpKey::Move(1));
        assert_eq!(help_key(&key(KeyCode::PageUp, plain)), HelpKey::Page(-1));
        assert_eq!(help_key(&key(KeyCode::End, plain)), HelpKey::Last);
        assert_eq!(help_key(&key(KeyCode::Esc, plain)), HelpKey::Back);
        assert_eq!(help_key(&key(KeyCode::Char('x'), plain)), HelpKey::None);
        assert_eq!(session_key(&key(KeyCode::Char('?'), plain), DEFAULT_DETACH), SessionKey::Input(vec![b'?']));
    }

    #[test]
    fn the_list_filter_owns_the_keyboard_while_it_is_open() {
        // While filtering, `j` is a letter to search for, not a cursor move.
        assert_eq!(list_key(&key(KeyCode::Char('j'), KeyModifiers::NONE), true), ListKey::FilterChar('j'));
        assert_eq!(list_key(&key(KeyCode::Char('j'), KeyModifiers::NONE), false), ListKey::Move(1));
        assert_eq!(list_key(&key(KeyCode::Esc, KeyModifiers::NONE), true), ListKey::Filter);
        assert_eq!(list_key(&key(KeyCode::Enter, KeyModifiers::NONE), true), ListKey::Open);
        assert_eq!(list_key(&key(KeyCode::Backspace, KeyModifiers::NONE), true), ListKey::FilterBackspace);
        assert_eq!(list_key(&key(KeyCode::Char('/'), KeyModifiers::NONE), false), ListKey::Filter);
        assert_eq!(list_key(&key(KeyCode::Char('q'), KeyModifiers::NONE), false), ListKey::Back);
    }
}
