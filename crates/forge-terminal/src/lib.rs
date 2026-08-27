//! VT emulation over a process's byte stream.
//!
//! The terminal pane must be a real emulator, not a scrolling text widget.
//! BitBake, ninja and gcc rewrite a status line with carriage returns; a widget
//! that appends lines turns one progress bar into thousands of duplicates. Here
//! the byte stream is collapsed into *screen state*, which is bounded no matter
//! how much the child writes.
//!
//! The emulator sits behind [`Emulator`] rather than being used directly. The
//! current backend is `vt100`, which is fast and correct but has a thin bus
//! factor and does not reflow on resize; keeping the seam lets us swap in
//! `termwiz` or `alacritty_terminal` without touching the UI.

use std::time::{Duration, Instant};

/// A terminal emulator backend.
pub trait Emulator {
    fn feed(&mut self, bytes: &[u8]);
    fn resize(&mut self, rows: u16, cols: u16);
    fn size(&self) -> (u16, u16);
    /// Whether the child is currently on the alternate screen (`vim`, `htop`).
    ///
    /// The UI needs this: text selection and scrollback are meaningless while a
    /// full-screen application owns the screen.
    fn alternate_screen(&self) -> bool;
    /// Plain-text rendering of the visible screen, one entry per row.
    fn visible_text(&self) -> Vec<String>;
}

/// `vt100`-backed terminal.
pub struct Vt100Terminal {
    parser: vt100::Parser,
    /// Bytes accumulated since the last frame was parsed.
    pending: Vec<u8>,
    last_flush: Instant,
    coalesce: Duration,
    scrollback: usize,
}

impl Vt100Terminal {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, scrollback),
            pending: Vec::with_capacity(64 * 1024),
            last_flush: Instant::now(),
            // One frame at 60fps. Coalescing reads before parsing decouples byte
            // rate from frame rate and is the single most important technique
            // for surviving high-volume build output.
            coalesce: Duration::from_millis(16),
            scrollback,
        }
    }

    /// Queue bytes without parsing them yet.
    ///
    /// Returns `true` if a frame is due. The caller drains on that signal, so a
    /// process emitting megabytes costs one parse per frame rather than one per
    /// `read`.
    pub fn queue(&mut self, bytes: &[u8]) -> bool {
        self.pending.extend_from_slice(bytes);
        self.frame_due()
    }

    pub fn frame_due(&self) -> bool {
        // 64 KiB also forces a flush, so a burst is not held back waiting for
        // the clock.
        self.pending.len() >= 64 * 1024 || self.last_flush.elapsed() >= self.coalesce
    }

    /// Parse everything queued. Cheap when nothing is pending.
    pub fn flush(&mut self) {
        if self.pending.is_empty() {
            self.last_flush = Instant::now();
            return;
        }
        self.parser.process(&self.pending);
        self.pending.clear();
        self.last_flush = Instant::now();
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback
    }

    /// Scroll the viewport back by `n` lines from the bottom.
    pub fn set_scrollback(&mut self, n: usize) {
        self.parser.screen_mut().set_scrollback(n);
    }
}

impl Emulator for Vt100Terminal {
    fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        self.flush();
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        // Flush first: parsing queued bytes against the new geometry would place
        // them differently than the child intended when it wrote them.
        self.flush();
        self.parser.screen_mut().set_size(rows, cols);
    }

    fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    fn visible_text(&self) -> Vec<String> {
        let (rows, _) = self.parser.screen().size();
        (0..rows)
            .map(|r| self.parser.screen().contents_between(r, 0, r, u16::MAX))
            .collect()
    }
}

/// Render a raw byte stream as the text a developer would see on screen.
///
/// The agent must be given the *same* collapsed view the terminal pane shows,
/// not the raw stream. BitBake, ninja and cargo rewrite one status line with
/// carriage returns; handing the agent those bytes would spend its context on
/// thousands of near-identical lines that never existed on screen. Running the
/// bytes through the emulator first makes "what the agent read" and "what the
/// developer saw" the same thing.
///
/// `rows` bounds the result to a tail: the last `rows` lines of the transcript,
/// which is what a question like "how is the build going?" actually needs.
/// Trailing blank rows are trimmed, so a short-lived process does not return a
/// screen of padding.
pub fn render_transcript(bytes: &[u8], rows: u16, cols: u16) -> String {
    let rows = rows.clamp(1, 1000);
    let cols = cols.clamp(20, 500);
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    let screen = parser.screen();

    let mut lines: Vec<String> = (0..rows)
        .map(|r| {
            screen
                .contents_between(r, 0, r, u16::MAX)
                .trim_end()
                .to_string()
        })
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}
