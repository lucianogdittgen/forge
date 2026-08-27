//! Application shell: layout, focus, and the event loop.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use forge_process::{ProcessEvent, ProcessId, ProcessManager, ProcessSpec, ProcessState};
use forge_tui::{PaneAction, TerminalPane};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::broadcast;

/// Redraw cadence. Rendering is decoupled from output arrival: a process
/// emitting megabytes costs one parse and one draw per frame, not per read.
const FRAME: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Conversation,
    Terminal,
}

/// A line in the conversation pane.
///
/// Placeholder shape until the agent driver lands; the point of the slice is
/// that the terminal pane is real, not that this one is.
pub enum Line {
    User(String),
    Agent(String),
    Note(String),
}

pub struct App {
    pub pm: ProcessManager,
    pub pane: TerminalPane,
    pub focus: Focus,
    pub conversation: Vec<Line>,
    pub workspace: String,
    should_quit: bool,
    proc_rx: Option<broadcast::Receiver<ProcessEvent>>,
    /// Set when the retained buffer has dropped bytes, so truncation is visible
    /// to the user rather than silent.
    dropped_notice: bool,
}

impl App {
    pub fn new(workspace: String) -> Self {
        let mut pane = TerminalPane::new(24, 80);
        // Start with the terminal focused, and keep the pane's own flag in step
        // with `focus` — they are two halves of one state and drifting apart
        // shows up as a pane that looks focused but swallows nothing.
        pane.set_focus(true);
        Self {
            pm: ProcessManager::new(),
            pane,
            focus: Focus::Terminal,
            conversation: vec![
                Line::Note("Forge — the terminal stays yours.".into()),
                Line::Note("Tab switches panes · Enter focuses the terminal".into()),
                Line::Note("In the terminal: tap Esc twice to leave · Ctrl-C goes to the process".into()),
            ],
            workspace,
            should_quit: false,
            proc_rx: None,
            dropped_notice: false,
        }
    }

    /// Start a process and attach the terminal pane to it.
    pub fn start_process(&mut self, spec: ProcessSpec) -> Result<ProcessId> {
        let label = format!("{} {}", spec.command, spec.args.join(" "));
        let id = self.pm.start(spec)?;

        // Attach atomically so no output is missed between start and subscribe.
        let (backlog, rx) = self.pm.attach(id)?;
        self.pane.attach(id);
        self.pane.feed(&backlog);
        self.proc_rx = Some(rx);

        self.conversation.push(Line::Note(format!("started {label} ({id})")));
        Ok(id)
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// One iteration: wait for input, process output, or the frame deadline.
    pub async fn tick(&mut self, events: &mut crossterm::event::EventStream) -> Result<()> {
        use futures::StreamExt;

        let deadline = tokio::time::sleep(FRAME);
        tokio::pin!(deadline);

        tokio::select! {
            // Input.
            maybe = events.next() => {
                if let Some(Ok(ev)) = maybe {
                    self.handle_event(ev)?;
                }
            }

            // Process output. Feeding is cheap; parsing happens on the frame.
            res = async {
                match self.proc_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    // No process: park forever and let the other branches win.
                    None => std::future::pending().await,
                }
            } => {
                match res {
                    Ok(ProcessEvent::Output(bytes)) => self.pane.feed(&bytes),
                    Ok(ProcessEvent::StateChanged(st)) => self.on_state(st),
                    Ok(ProcessEvent::Exited { code, signal }) => self.on_exit(code, signal),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // The UI fell behind a burst. Say so rather than
                        // pretending the output was complete.
                        if !self.dropped_notice {
                            self.dropped_notice = true;
                            self.conversation.push(Line::Note(
                                format!("terminal fell behind; {n} output chunks dropped")));
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => { self.proc_rx = None; }
                }
            }

            _ = &mut deadline => {}
        }

        self.pane.tick();
        Ok(())
    }

    fn on_state(&mut self, st: ProcessState) {
        if st.is_terminal() {
            self.conversation.push(Line::Note(format!("process {st:?}")));
        }
    }

    fn on_exit(&mut self, code: Option<i32>, signal: Option<i32>) {
        let msg = match (code, signal) {
            (Some(0), _) => "process exited cleanly (0)".to_string(),
            (Some(c), _) => format!("process exited with code {c}"),
            (None, Some(s)) => format!("process terminated by signal {s}"),
            _ => "process ended".to_string(),
        };
        self.conversation.push(Line::Note(msg));
    }

    fn handle_event(&mut self, ev: Event) -> Result<()> {
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => self.handle_key(k),
            Event::Resize(cols, rows) => {
                // Give the terminal pane its share and tell the child.
                let (prows, pcols) = pane_size(rows, cols);
                self.pane.resize(&self.pm, prows, pcols);
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => self.pane.scroll(3),
                MouseEventKind::ScrollDown => self.pane.scroll(-3),
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // The terminal pane gets first refusal. When focused it consumes
        // everything, so no Forge shortcut can steal a key from the child.
        if self.focus == Focus::Terminal {
            match self.pane.handle_key(&self.pm, key) {
                PaneAction::ReleaseFocus => {
                    self.focus = Focus::Conversation;
                    self.pane.set_focus(false);
                    return;
                }
                PaneAction::Consumed => return,
                PaneAction::Ignored => {}
            }
        }

        // Application shortcuts, only reachable when the terminal is unfocused.
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), KeyModifiers::NONE) => self.should_quit = true,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.should_quit = true,
            (KeyCode::Tab, _) | (KeyCode::Enter, _) => {
                self.focus = Focus::Terminal;
                self.pane.set_focus(true);
            }
            _ => {}
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        let outer = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

        self.render_header(frame, outer[0]);

        let panes = Layout::horizontal([
            Constraint::Percentage(38),
            Constraint::Percentage(62),
        ])
        .split(outer[1]);

        self.render_conversation(frame, panes[0]);

        // Keep the child's window size in step with the pane's inner area.
        let inner_rows = panes[1].height.saturating_sub(2).max(1);
        let inner_cols = panes[1].width.saturating_sub(2).max(1);
        self.pane.resize(&self.pm, inner_rows, inner_cols);

        let title = match self.pane.process.and_then(|id| self.pm.get(id).ok()) {
            Some(r) => {
                let state = match r.state {
                    ProcessState::Running => "running".to_string(),
                    ProcessState::Exited => format!("exited {}", r.exit_code.unwrap_or(-1)),
                    other => format!("{other:?}").to_lowercase(),
                };
                format!("TERMINAL — {} [{}]", r.command, state)
            }
            None => "TERMINAL".to_string(),
        };
        self.pane.render(frame, panes[1], &title);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let running = self
            .pm
            .list()
            .iter()
            .filter(|r| !r.state.is_terminal())
            .count();

        let left = Span::styled(" Forge ", Style::default().fg(Color::Black).bg(Color::Cyan).bold());
        let mid = Span::raw(format!("  {}  ", self.workspace));
        let right = Span::styled(
            format!("{running} running "),
            Style::default().fg(Color::DarkGray),
        );

        let line = Line_::from(vec![left, mid, right]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_conversation(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Conversation;
        let border = if focused { Color::Cyan } else { Color::DarkGray };

        let lines: Vec<Line_> = self
            .conversation
            .iter()
            .flat_map(|l| match l {
                Line::User(t) => vec![
                    Line_::from(Span::styled("you", Style::default().fg(Color::Green).bold())),
                    Line_::from(t.clone()),
                    Line_::from(""),
                ],
                Line::Agent(t) => vec![
                    Line_::from(Span::styled("claude", Style::default().fg(Color::Magenta).bold())),
                    Line_::from(t.clone()),
                    Line_::from(""),
                ],
                Line::Note(t) => vec![Line_::from(Span::styled(
                    format!("· {t}"),
                    Style::default().fg(Color::DarkGray),
                ))],
            })
            .collect();

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(" AI ");

        frame.render_widget(
            Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
            area,
        );
    }
}

/// ratatui's `Line` collides with our conversation `Line`.
use ratatui::text::Line as Line_;

/// Inner size of the terminal pane for a given screen size.
fn pane_size(rows: u16, cols: u16) -> (u16, u16) {
    let pane_cols = (cols as f32 * 0.62) as u16;
    (rows.saturating_sub(3).max(1), pane_cols.saturating_sub(2).max(1))
}
