//! Key events back into terminal escape sequences.
//!
//! Unavoidable tax: the TUI framework parses raw input into structured key
//! events, and the child process wants the original bytes. This table turns
//! them back. Toad's equivalent is 251 lines; the shape of the problem is the
//! same in every framework.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode a key event as the bytes a terminal would send.
///
/// Returns `None` for keys with no terminal representation.
pub fn encode(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let base: Vec<u8> = match key.code {
        KeyCode::Char(c) if ctrl => {
            // Control characters: Ctrl-A..Ctrl-Z -> 0x01..0x1a, plus the
            // punctuation forms. Ctrl-C becomes 0x03 here, which is exactly
            // what a real terminal sends and what the line discipline expects.
            let b = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => (c as u8) - b'a' + 1,
                ' ' | '@' => 0x00,
                '[' => 0x1b,
                '\\' => 0x1c,
                ']' => 0x1d,
                '^' => 0x1e,
                '_' | '?' => 0x1f,
                _ => return None,
            };
            vec![b]
        }
        KeyCode::Char(c) => {
            let mut s = [0u8; 4];
            c.encode_utf8(&mut s).as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Up => encode_arrow(b'A', shift, ctrl, alt),
        KeyCode::Down => encode_arrow(b'B', shift, ctrl, alt),
        KeyCode::Right => encode_arrow(b'C', shift, ctrl, alt),
        KeyCode::Left => encode_arrow(b'D', shift, ctrl, alt),
        KeyCode::F(n) => encode_function(n)?,
        _ => return None,
    };

    // Alt is a leading ESC, except where the sequence already encodes it.
    if alt && !matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right) {
        let mut v = vec![0x1b];
        v.extend(base);
        return Some(v);
    }
    Some(base)
}

/// Arrows use a modifier parameter when modified: `CSI 1 ; <m> <final>`.
fn encode_arrow(final_byte: u8, shift: bool, ctrl: bool, alt: bool) -> Vec<u8> {
    let m = 1 + (shift as u8) + 2 * (alt as u8) + 4 * (ctrl as u8);
    if m == 1 {
        vec![0x1b, b'[', final_byte]
    } else {
        format!("\x1b[1;{m}").into_bytes().into_iter().chain([final_byte]).collect()
    }
}

fn encode_function(n: u8) -> Option<Vec<u8>> {
    let s = match n {
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
    };
    Some(s.as_bytes().to_vec())
}
