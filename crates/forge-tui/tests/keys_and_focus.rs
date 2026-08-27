//! Key encoding and the focus gesture.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use forge_process::{ProcessManager, ProcessSpec};
use forge_tui::{PaneAction, TerminalPane, keys};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// Ctrl-C must encode to 0x03 — the byte a real terminal sends.
#[test]
fn ctrl_c_encodes_to_etx() {
    assert_eq!(keys::encode(ctrl('c')), Some(vec![0x03]));
}

#[test]
fn control_letters_map_to_their_control_codes() {
    assert_eq!(keys::encode(ctrl('a')), Some(vec![0x01]));
    assert_eq!(keys::encode(ctrl('d')), Some(vec![0x04])); // EOF
    assert_eq!(keys::encode(ctrl('z')), Some(vec![0x1a])); // suspend
    // Case must not matter.
    assert_eq!(keys::encode(ctrl('C')), Some(vec![0x03]));
}

#[test]
fn ordinary_and_unicode_characters_pass_through_as_utf8() {
    assert_eq!(keys::encode(key(KeyCode::Char('x'))), Some(b"x".to_vec()));
    assert_eq!(keys::encode(key(KeyCode::Char('é'))), Some("é".as_bytes().to_vec()));
    assert_eq!(keys::encode(key(KeyCode::Char('你'))), Some("你".as_bytes().to_vec()));
}

#[test]
fn enter_sends_carriage_return_not_newline() {
    // Terminals send CR; the line discipline turns it into NL. Sending \n
    // directly breaks readline-based programs.
    assert_eq!(keys::encode(key(KeyCode::Enter)), Some(vec![b'\r']));
}

#[test]
fn backspace_sends_del() {
    assert_eq!(keys::encode(key(KeyCode::Backspace)), Some(vec![0x7f]));
}

#[test]
fn unmodified_arrows_use_plain_csi() {
    assert_eq!(keys::encode(key(KeyCode::Up)), Some(b"\x1b[A".to_vec()));
    assert_eq!(keys::encode(key(KeyCode::Left)), Some(b"\x1b[D".to_vec()));
}

#[test]
fn modified_arrows_carry_a_modifier_parameter() {
    let c_up = KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL);
    assert_eq!(keys::encode(c_up), Some(b"\x1b[1;5A".to_vec()));
    let s_right = KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT);
    assert_eq!(keys::encode(s_right), Some(b"\x1b[1;2C".to_vec()));
}

#[test]
fn alt_prefixes_with_escape() {
    let alt_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT);
    assert_eq!(keys::encode(alt_x), Some(vec![0x1b, b'x']));
}

#[test]
fn function_keys_encode() {
    assert_eq!(keys::encode(key(KeyCode::F(1))), Some(b"\x1bOP".to_vec()));
    assert_eq!(keys::encode(key(KeyCode::F(5))), Some(b"\x1b[15~".to_vec()));
}

/// An unfocused pane must not swallow keys.
#[test]
fn unfocused_pane_ignores_keys() {
    let pm = ProcessManager::new();
    let mut pane = TerminalPane::new(24, 80);
    assert_eq!(pane.handle_key(&pm, ctrl('c')), PaneAction::Ignored);
}

/// A focused pane consumes everything, including keys that would otherwise be
/// Forge shortcuts. Ctrl-C must reach the build, not quit Forge.
#[test]
fn focused_pane_consumes_every_key() {
    let pm = ProcessManager::new();
    let mut pane = TerminalPane::new(24, 80);
    pane.set_focus(true);

    for k in [ctrl('c'), ctrl('d'), key(KeyCode::Char('q')), key(KeyCode::Tab)] {
        assert_eq!(pane.handle_key(&pm, k), PaneAction::Consumed, "key {k:?} escaped the pane");
    }
}

/// A single Escape is forwarded to the child; two in quick succession release
/// focus. Without the gesture there is no key left to escape with.
#[test]
fn double_escape_releases_focus_single_escape_does_not() {
    let pm = ProcessManager::new();
    let mut pane = TerminalPane::new(24, 80);
    pane.set_focus(true);

    assert_eq!(
        pane.handle_key(&pm, key(KeyCode::Esc)),
        PaneAction::Consumed,
        "a single Escape must go to the child (vim needs it)"
    );
    assert_eq!(
        pane.handle_key(&pm, key(KeyCode::Esc)),
        PaneAction::ReleaseFocus,
        "a second Escape within the window must release focus"
    );
}

/// A slow second Escape is just another Escape.
#[test]
fn slow_second_escape_does_not_release_focus() {
    let pm = ProcessManager::new();
    let mut pane = TerminalPane::new(24, 80);
    pane.set_focus(true);

    assert_eq!(pane.handle_key(&pm, key(KeyCode::Esc)), PaneAction::Consumed);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(
        pane.handle_key(&pm, key(KeyCode::Esc)),
        PaneAction::Consumed,
        "Escapes outside the tap window must both be forwarded"
    );
}

/// An intervening key resets the gesture, so Esc-x-Esc does not release focus.
#[test]
fn intervening_key_resets_the_escape_gesture() {
    let pm = ProcessManager::new();
    let mut pane = TerminalPane::new(24, 80);
    pane.set_focus(true);

    pane.handle_key(&pm, key(KeyCode::Esc));
    pane.handle_key(&pm, key(KeyCode::Char('x')));
    assert_eq!(
        pane.handle_key(&pm, key(KeyCode::Esc)),
        PaneAction::Consumed,
        "the gesture must not survive an intervening key"
    );
}

/// Keys typed into a focused pane actually reach the process.
#[tokio::test]
async fn keys_reach_the_attached_process() {
    let pm = ProcessManager::new();
    let id = pm
        .start(ProcessSpec::new("sh").arg("-c").arg("read line; echo saw:$line"))
        .expect("start");

    let mut pane = TerminalPane::new(24, 80);
    pane.attach(id);
    pane.set_focus(true);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    for c in "hi".chars() {
        pane.handle_key(&pm, key(KeyCode::Char(c)));
    }
    pane.handle_key(&pm, key(KeyCode::Enter));

    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let (out, _) = pm.output_snapshot(id).expect("snapshot");
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("saw:hi"), "process saw: {text:?}");
}
