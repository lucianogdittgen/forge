//! Terminal emulation behaviour.
//!
//! These assert the properties that disqualified the Python stack: correct
//! alternate-screen save/restore, and carriage-return rewriting that collapses
//! rather than accumulating.

use forge_terminal::{Emulator, Vt100Terminal};

/// The test that rules out `pyte`.
///
/// Entering the alternate screen, drawing, and leaving it must restore the
/// original screen exactly. `pyte` has no `1049`/`47` handling at all, so this
/// clobbers the main screen permanently there — which is why `vim` and `htop`
/// corrupt a pyte-backed pane.
#[test]
fn alternate_screen_saves_and_restores_the_main_screen() {
    let mut t = Vt100Terminal::new(10, 40, 1000);

    t.feed(b"MAIN-SCREEN\r\n");
    assert!(t.visible_text()[0].starts_with("MAIN-SCREEN"));
    assert!(!t.alternate_screen());

    // Enter alt screen, draw something different.
    t.feed(b"\x1b[?1049h");
    assert!(t.alternate_screen(), "should be on the alternate screen");
    t.feed(b"\x1b[HALT-CONTENT");
    assert!(t.visible_text()[0].starts_with("ALT-CONTENT"));

    // Leave it: the main screen must come back untouched.
    t.feed(b"\x1b[?1049l");
    assert!(!t.alternate_screen(), "should be back on the main screen");
    assert!(
        t.visible_text()[0].starts_with("MAIN-SCREEN"),
        "main screen was clobbered: {:?}",
        t.visible_text()[0]
    );
}

/// A CR progress bar must occupy exactly one line, however many times it writes.
///
/// This is the concrete failure the brief calls out: "progress output using
/// carriage returns must not produce thousands of duplicated lines."
#[test]
fn carriage_return_progress_collapses_to_one_line() {
    let mut t = Vt100Terminal::new(10, 40, 1000);

    for i in 0..=100 {
        t.feed(format!("progress: {i}%\r").as_bytes());
    }
    t.flush();

    let visible = t.visible_text();
    assert!(
        visible[0].starts_with("progress: 100%"),
        "row 0 was: {:?}",
        visible[0]
    );
    // Every other row must still be blank: 101 writes, one line used.
    for (i, row) in visible.iter().enumerate().skip(1) {
        assert!(
            row.trim().is_empty(),
            "row {i} should be empty but was {row:?} — CR output is accumulating"
        );
    }
}

#[test]
fn sgr_colour_and_attributes_are_parsed() {
    let mut t = Vt100Terminal::new(5, 20, 100);
    t.feed(b"\x1b[1;31mRED-BOLD\x1b[0m");
    t.flush();

    let cell = t.screen().cell(0, 0).expect("cell 0,0");
    assert_eq!(cell.contents(), "R");
    assert!(cell.bold(), "bold attribute lost");
    assert_eq!(
        cell.fgcolor(),
        vt100::Color::Idx(1),
        "foreground colour lost"
    );
}

/// Double-width characters must occupy two cells.
#[test]
fn wide_characters_occupy_two_cells() {
    let mut t = Vt100Terminal::new(5, 20, 100);
    t.feed("你好".as_bytes());
    t.flush();

    let c0 = t.screen().cell(0, 0).expect("cell 0");
    assert_eq!(c0.contents(), "你");
    assert!(c0.is_wide(), "CJK character should be wide");
    // Cell 1 is the continuation of the wide char; cell 2 holds the next one.
    let c2 = t.screen().cell(0, 2).expect("cell 2");
    assert_eq!(c2.contents(), "好");
}

#[test]
fn cursor_addressing_and_erase_work() {
    let mut t = Vt100Terminal::new(5, 20, 100);
    t.feed(b"line-one\r\nline-two\r\n");
    // Move to row 1 col 1 and erase to end of screen.
    t.feed(b"\x1b[1;1H\x1b[0J");
    t.flush();

    let v = t.visible_text();
    assert!(
        v[0].trim().is_empty(),
        "erase-to-end should have cleared row 0"
    );
    assert!(
        v[1].trim().is_empty(),
        "erase-to-end should have cleared row 1"
    );
}

#[test]
fn resize_updates_reported_size() {
    let mut t = Vt100Terminal::new(24, 80, 100);
    assert_eq!(t.size(), (24, 80));
    t.resize(40, 120);
    assert_eq!(t.size(), (40, 120));
}

/// Coalescing: queued bytes are not parsed until a frame is due.
///
/// This is what decouples byte rate from frame rate under heavy build output.
#[test]
fn queue_defers_parsing_until_flush() {
    let mut t = Vt100Terminal::new(10, 40, 100);
    t.queue(b"deferred");
    // Not parsed yet, so the screen is still blank.
    assert!(
        t.visible_text()[0].trim().is_empty(),
        "queue must not parse immediately"
    );
    t.flush();
    assert!(
        t.visible_text()[0].starts_with("deferred"),
        "flush must parse queued bytes"
    );
}

/// A large burst forces a flush rather than waiting for the frame clock.
#[test]
fn large_burst_signals_a_frame_is_due() {
    let mut t = Vt100Terminal::new(10, 40, 100);
    let big = vec![b'x'; 70 * 1024];
    assert!(t.queue(&big), "a 70 KiB burst should force a frame");
}

/// Screen state stays bounded no matter how much is written.
#[test]
fn screen_state_is_bounded_under_heavy_output() {
    let mut t = Vt100Terminal::new(24, 80, 500);
    for i in 0..20_000 {
        t.feed(format!("line {i}\r\n").as_bytes());
    }
    t.flush();
    // 20k lines written, 24 rows visible. The emulator collapsed the rest into
    // bounded scrollback rather than retaining every line.
    assert_eq!(t.visible_text().len(), 24);
    assert!(
        t.visible_text()[23].contains("19999") || t.visible_text()[22].contains("19999"),
        "should be showing the most recent output, got {:?}",
        t.visible_text()
    );
}

#[test]
fn transcript_collapses_carriage_return_progress() {
    // The point of rendering through the emulator: 400 progress writes are one
    // line to the agent, exactly as they are one line to the developer.
    let mut b = Vec::new();
    for i in 1..=400 {
        b.extend_from_slice(format!("\rProgress: {i}/400").as_bytes());
    }
    let text = forge_terminal::render_transcript(&b, 200, 80);
    assert_eq!(text, "Progress: 400/400");
}

#[test]
fn transcript_keeps_ordinary_lines_and_strips_colour() {
    let b = b"\x1b[32mbuilding\x1b[0m\r\nlinking\r\ndone\r\n";
    let text = forge_terminal::render_transcript(b, 200, 80);
    assert_eq!(text, "building\nlinking\ndone");
}

#[test]
fn transcript_is_bounded_to_a_tail() {
    let mut b = Vec::new();
    for i in 0..500 {
        b.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    let text = forge_terminal::render_transcript(&b, 10, 80);
    let lines: Vec<&str> = text.lines().collect();
    // Nine, not ten: the final "\r\n" left the cursor on a blank row, and a
    // trailing blank row is padding rather than output.
    assert_eq!(lines.len(), 9, "must return only the tail");
    assert_eq!(*lines.last().unwrap(), "line 499");
    assert_eq!(lines[0], "line 491");
}
