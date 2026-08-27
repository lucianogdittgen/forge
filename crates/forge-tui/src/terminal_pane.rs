//! The terminal pane: a real terminal, not a log view.
//!
//! Owns the focus gesture. When the pane has focus, **every** key goes to the
//! child — including Ctrl-C, which must reach `bitbake` rather than quitting
//! Forge. That leaves no key free to release focus, so releasing it is a
//! gesture: a double-tap of Escape. A single Escape is forwarded, because
//! `vim` needs it.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use forge_process::{ProcessId, ProcessManager};
use forge_terminal::{Emulator, Vt100Terminal};
use ratatui::prelude::*;
use tui_term::widget::PseudoTerminal;

/// Two Escapes within this window release focus; a single one is forwarded.
const ESCAPE_TAP_WINDOW: Duration = Duration::from_millis(400);

pub struct TerminalPane {
    pub process: Option<ProcessId>,
    term: Vt100Terminal,
    focused: bool,
    last_escape: Option<Instant>,
    /// Lines scrolled back from the bottom. Zero means following live output.
    scroll_offset: usize,
}

/// What the pane wants the application to do after handling a key.
#[derive(Debug, PartialEq, Eq)]
pub enum PaneAction {
    /// Fully handled; the app should do nothing.
    Consumed,
    /// The focus-release gesture fired.
    ReleaseFocus,
    /// Not for the pane.
    Ignored,
}

impl TerminalPane {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            process: None,
            term: Vt100Terminal::new(rows, cols, 10_000),
            focused: false,
            last_escape: None,
            scroll_offset: 0,
        }
    }

    pub fn attach(&mut self, id: ProcessId) {
        self.process = Some(id);
        self.scroll_offset = 0;
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    pub fn set_focus(&mut self, on: bool) {
        self.focused = on;
        self.last_escape = None;
    }

    pub fn alternate_screen(&self) -> bool {
        self.term.alternate_screen()
    }

    /// Feed bytes from the process. Parsing is deferred to the frame cadence.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.term.queue(bytes);
    }

    /// Parse whatever is queued if a frame is due. Called once per render tick.
    pub fn tick(&mut self) {
        if self.term.frame_due() {
            self.term.flush();
        }
    }

    pub fn resize(&mut self, pm: &ProcessManager, rows: u16, cols: u16) {
        if self.term.size() == (rows, cols) {
            return;
        }
        self.term.resize(rows, cols);
        // Keep the child's idea of the window in step with ours, or full-screen
        // programs draw to the wrong geometry.
        if let Some(id) = self.process {
            let _ = pm.resize(id, rows, cols);
        }
    }

    /// Handle a key while the pane has focus.
    ///
    /// Everything is forwarded to the child. Nothing here is interpreted as a
    /// Forge shortcut — that is the point of the pane.
    pub fn handle_key(&mut self, pm: &ProcessManager, key: KeyEvent) -> PaneAction {
        if !self.focused {
            return PaneAction::Ignored;
        }

        if key.code == KeyCode::Esc {
            let now = Instant::now();
            if let Some(prev) = self.last_escape {
                if now.duration_since(prev) <= ESCAPE_TAP_WINDOW {
                    self.last_escape = None;
                    return PaneAction::ReleaseFocus;
                }
            }
            self.last_escape = Some(now);
            // Forward this one: a single Escape belongs to the child.
            if let Some(id) = self.process {
                let _ = pm.write_stdin(id, &[0x1b]);
            }
            return PaneAction::Consumed;
        }
        self.last_escape = None;

        // Any keystroke returns to following live output, like a real terminal.
        if self.scroll_offset != 0 {
            self.scroll_offset = 0;
            self.term.set_scrollback(0);
        }

        if let (Some(id), Some(bytes)) = (self.process, crate::keys::encode(key)) {
            let _ = pm.write_stdin(id, &bytes);
        }
        PaneAction::Consumed
    }

    /// Scroll back through history.
    ///
    /// Refused while a full-screen application owns the screen: there is no
    /// meaningful history to scroll through, and `vim` expects the wheel.
    pub fn scroll(&mut self, delta: isize) {
        if self.alternate_screen() {
            return;
        }
        let new = (self.scroll_offset as isize - delta).clamp(0, 10_000) as usize;
        self.scroll_offset = new;
        self.term.set_scrollback(new);
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, title: &str) {
        use ratatui::widgets::{Block, Borders};

        let border_style = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let subtitle = if self.focused {
            " tap esc twice to leave "
        } else {
            " enter to focus "
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(format!(" {title} "))
            .title_bottom(subtitle);

        let pseudo = PseudoTerminal::new(self.term.screen()).block(block);
        frame.render_widget(pseudo, area);
    }
}

impl TerminalPane {
    /// Point the pane at a different process, discarding the old screen.
    ///
    /// Reusing the emulator across processes would leave the previous child's
    /// cursor position, colours and alternate-screen flag in place, which shows
    /// up as a corrupt first frame. Cheap enough to do on every switch.
    pub fn switch_to(&mut self, id: ProcessId) {
        let (rows, cols) = self.term.size();
        self.term = Vt100Terminal::new(rows, cols, 10_000);
        self.scroll_offset = 0;
        self.last_escape = None;
        self.process = Some(id);
    }
}
