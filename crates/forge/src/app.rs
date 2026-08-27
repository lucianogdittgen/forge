//! Application shell: layout, focus, and the event loop.
//!
//! Two panes, one rule. The agent talks in the left pane; every process it
//! starts runs in the right one, live, under the developer's hands. Nothing
//! here relays output from one to the other — they are two subscribers to the
//! same process manager, which is why the developer's view is never a summary
//! of what the agent decided to show them.

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use forge_agent::{AgentEvent, ApprovalRequest, Capability, Decision};
use forge_process::{ProcessEvent, ProcessId, ProcessManager, ProcessSpec, ProcessState};
use forge_tui::{PaneAction, TerminalPane};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use tokio::sync::{broadcast, mpsc};

use crate::agent_session::AgentSession;

/// Redraw cadence. Rendering is decoupled from output arrival: a process
/// emitting megabytes costs one parse and one draw per frame, not per read.
const FRAME: std::time::Duration = std::time::Duration::from_millis(16);

/// How much of a tool result to show inline. The developer has the terminal for
/// detail; this is a receipt, not a transcript.
const TOOL_RESULT_LINES: usize = 3;

/// Tools that would let the agent run commands Forge cannot render. Mirrors the
/// startup check in `forge-agent`; this one catches a surface that changed
/// underneath us between releases of the CLI.
const SHELL_TOOLS: &[&str] = &["Bash", "Task", "Workflow", "Skill"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Conversation,
    Terminal,
}

/// A line in the conversation pane.
#[derive(Debug)]
pub enum Line {
    User(String),
    Agent(String),
    Thinking(String),
    Tool {
        name: String,
        summary: String,
        capability: Capability,
    },
    ToolResult {
        is_error: bool,
        text: String,
    },
    Note(String),
}

/// What one iteration of the loop woke up for.
///
/// The select produces this and then releases its borrows, so handlers are free
/// to touch the whole of `self`.
enum Wake {
    Input(Event),
    Process(Result<ProcessEvent, broadcast::error::RecvError>),
    Agent(Option<AgentEvent>),
    Frame,
}

pub struct App {
    pub pm: ProcessManager,
    pub pane: TerminalPane,
    pub focus: Focus,
    pub conversation: Vec<Line>,
    pub workspace: String,

    agent: Option<AgentSession>,
    /// A turn is in flight; Ctrl-C cancels it rather than quitting.
    busy: bool,
    /// Assistant text arriving token by token, not yet a finished message.
    streaming: String,
    /// Set while the agent is blocked waiting for a decision.
    pending: Option<ApprovalRequest>,

    input: String,
    /// Lines scrolled back in the conversation. Zero follows the newest.
    conv_scroll: usize,

    should_quit: bool,
    proc_rx: Option<broadcast::Receiver<ProcessEvent>>,
    /// Highest process id the pane has ever followed, so a process the agent
    /// starts is followed automatically without stealing the pane back from a
    /// developer who deliberately switched away.
    followed: u64,
    /// Set when the retained buffer has dropped bytes, so truncation is visible
    /// to the user rather than silent.
    dropped_notice: bool,
    tool_surface_checked: bool,
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
                Line::Note("Tab moves between the conversation and the terminal.".into()),
                Line::Note("In the terminal: Esc Esc leaves · Ctrl-C goes to the process".into()),
            ],
            workspace,
            agent: None,
            busy: false,
            streaming: String::new(),
            pending: None,
            input: String::new(),
            conv_scroll: 0,
            should_quit: false,
            proc_rx: None,
            followed: 0,
            dropped_notice: false,
            tool_surface_checked: false,
        }
    }

    /// Attach the agent, once its tool endpoint is listening.
    pub fn attach_agent(&mut self, session: AgentSession) {
        self.agent = Some(session);
        self.conversation
            .push(Line::Note("agent ready — Tab to talk to it".into()));
    }

    /// Explain, once, why there is no agent, without pretending there is one.
    pub fn note_no_agent(&mut self, why: &str) {
        self.conversation
            .push(Line::Note(format!("no agent: {why}")));
        self.conversation
            .push(Line::Note("the terminal works regardless.".into()));
    }

    /// Start a process and point the terminal pane at it.
    pub fn start_process(&mut self, spec: ProcessSpec) -> Result<ProcessId> {
        let label = format!("{} {}", spec.command, spec.args.join(" "))
            .trim()
            .to_string();
        let id = self.pm.start(spec)?;
        self.follow(id)?;
        self.conversation
            .push(Line::Note(format!("started {label} ({id})")));
        Ok(id)
    }

    /// Point the pane at `id`, replaying what it has already written.
    fn follow(&mut self, id: ProcessId) -> Result<()> {
        // Attach atomically so no output is missed between start and subscribe.
        let (backlog, rx) = self.pm.attach(id)?;
        self.pane.switch_to(id);
        self.pane.feed(&backlog);
        self.proc_rx = Some(rx);
        self.followed = self.followed.max(id.0);
        Ok(())
    }

    /// Follow anything newly started — by the agent, or by anyone else.
    ///
    /// This is the moment the product's promise is kept: the agent asks for a
    /// build, and the build appears in front of the developer without the agent
    /// having any say in whether they see it.
    fn follow_new_processes(&mut self) {
        let newest = self.pm.list().iter().map(|r| r.id.0).max().unwrap_or(0);
        if newest > self.followed {
            let id = ProcessId(newest);
            if let Ok(r) = self.pm.get(id) {
                let label = r.label.clone().unwrap_or_else(|| r.command.clone());
                if self.follow(id).is_ok() {
                    self.conversation
                        .push(Line::Note(format!("terminal now showing {label} ({id})")));
                }
            }
        }
    }

    /// Cycle the pane to the next process, newest-first order.
    fn cycle_process(&mut self) {
        let mut ids: Vec<ProcessId> = self.pm.list().into_iter().map(|r| r.id).collect();
        ids.sort();
        if ids.len() < 2 {
            return;
        }
        let next = match self
            .pane
            .process
            .and_then(|c| ids.iter().position(|i| *i == c))
        {
            Some(i) => ids[(i + 1) % ids.len()],
            None => ids[0],
        };
        let _ = self.follow(next);
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// One iteration: wait for input, process output, an agent event, or the
    /// frame deadline.
    pub async fn tick(&mut self, events: &mut crossterm::event::EventStream) -> Result<()> {
        use futures::StreamExt;

        let deadline = tokio::time::sleep(FRAME);
        tokio::pin!(deadline);

        // Borrow the two receivers as disjoint fields, run the select, and let
        // the borrows end before anything touches `self`.
        let wake = {
            let proc_rx = self.proc_rx.as_mut();
            let agent_rx: Option<&mut mpsc::UnboundedReceiver<AgentEvent>> =
                self.agent.as_mut().map(|a| &mut a.events);

            tokio::select! {
                maybe = events.next() => match maybe {
                    Some(Ok(ev)) => Wake::Input(ev),
                    _ => Wake::Frame,
                },
                res = async {
                    match proc_rx {
                        Some(rx) => rx.recv().await,
                        // No process: park forever and let the other branches win.
                        None => std::future::pending().await,
                    }
                } => Wake::Process(res),
                ev = async {
                    match agent_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => Wake::Agent(ev),
                _ = &mut deadline => Wake::Frame,
            }
        };

        match wake {
            Wake::Input(ev) => self.handle_event(ev)?,
            Wake::Process(res) => match res {
                Ok(ProcessEvent::Output(bytes)) => self.pane.feed(&bytes),
                Ok(ProcessEvent::StateChanged(st)) => self.on_state(st),
                Ok(ProcessEvent::Exited { code, signal }) => self.on_exit(code, signal),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // The UI fell behind a burst. Say so rather than pretending
                    // the output was complete.
                    if !self.dropped_notice {
                        self.dropped_notice = true;
                        self.conversation.push(Line::Note(format!(
                            "terminal fell behind; {n} output chunks dropped"
                        )));
                    }
                }
                Err(broadcast::error::RecvError::Closed) => self.proc_rx = None,
            },
            Wake::Agent(Some(ev)) => self.on_agent_event(ev),
            Wake::Agent(None) => {
                self.agent = None;
                self.busy = false;
                self.conversation
                    .push(Line::Note("the agent process ended".into()));
            }
            Wake::Frame => {}
        }

        self.follow_new_processes();
        self.pane.tick();
        Ok(())
    }

    fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Ready { tools } => {
                if self.tool_surface_checked {
                    return;
                }
                self.tool_surface_checked = true;
                // The one invariant worth asserting at runtime. Built-in
                // editing tools are expected now — what must never appear is a
                // way to run commands outside Forge's process manager, because
                // that output would land in the model's context instead of on
                // the developer's screen.
                let shells: Vec<&String> = tools
                    .iter()
                    .filter(|t| SHELL_TOOLS.contains(&t.as_str()))
                    .collect();
                if shells.is_empty() {
                    let forge = tools
                        .iter()
                        .filter(|t| t.starts_with("mcp__forge__"))
                        .count();
                    self.conversation.push(Line::Note(format!(
                        "{} built-in + {forge} Forge tools · no shell",
                        tools.len() - forge
                    )));
                } else {
                    self.conversation.push(Line::Note(format!(
                        "WARNING: the agent has a shell ({shells:?}). It can run commands                          this pane will not show, and their output will cost you context."
                    )));
                }
            }

            AgentEvent::TextDelta(t) => self.streaming.push_str(&t),
            AgentEvent::Text(t) => {
                // The complete message supersedes what was streamed.
                self.streaming.clear();
                if !t.trim().is_empty() {
                    self.conversation.push(Line::Agent(t));
                }
            }
            AgentEvent::Thinking(t) => {
                if !t.trim().is_empty() {
                    self.conversation.push(Line::Thinking(t));
                }
            }

            AgentEvent::ToolCall { name, input, .. } => {
                self.conversation.push(Line::Tool {
                    summary: summarise_call(&name, &input),
                    capability: Capability::of_tool(&name),
                    name: bare_tool_name(&name),
                });
            }
            AgentEvent::ToolResult {
                is_error, content, ..
            } => {
                self.conversation.push(Line::ToolResult {
                    is_error,
                    text: content,
                });
            }

            AgentEvent::ApprovalRequested(req) => {
                // Pull focus to the conversation: the agent is blocked, and a
                // developer typing into the terminal would never notice.
                self.focus = Focus::Conversation;
                self.pane.set_focus(false);
                self.conv_scroll = 0;
                self.pending = Some(req);
            }

            AgentEvent::TurnFinished {
                cost_usd, is_error, ..
            } => {
                self.busy = false;
                self.streaming.clear();
                if is_error {
                    self.conversation
                        .push(Line::Note("the turn ended with an error".into()));
                } else if let Some(c) = cost_usd {
                    self.conversation
                        .push(Line::Note(format!("turn complete · ${c:.4}")));
                }
            }

            AgentEvent::Warning(w) => self.conversation.push(Line::Note(w)),
        }
    }

    fn on_state(&mut self, st: ProcessState) {
        if st.is_terminal() {
            self.conversation
                .push(Line::Note(format!("process {st:?}")));
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
        // A blocked agent outranks everything: while a decision is pending the
        // only keys that mean anything are the two that answer it.
        if self.pending.is_some() {
            self.handle_approval_key(key);
            return;
        }

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

        match (key.code, key.modifiers) {
            (KeyCode::Tab, _) => {
                self.focus = Focus::Terminal;
                self.pane.set_focus(true);
            }
            // Ctrl-C cancels the turn when there is one to cancel, and only
            // quits when there is not.
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if self.busy {
                    if let Some(a) = &self.agent {
                        a.interrupt();
                    }
                    self.busy = false;
                    self.conversation
                        .push(Line::Note("turn interrupted".into()));
                } else {
                    self.should_quit = true;
                }
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) if self.input.is_empty() => {
                self.should_quit = true;
            }
            (KeyCode::Char(']'), KeyModifiers::CONTROL) => self.cycle_process(),

            (KeyCode::Enter, _) => self.submit(),
            (KeyCode::Backspace, _) => {
                self.input.pop();
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => self.input.clear(),
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                let t = self.input.trim_end();
                let cut = t.rfind(' ').map(|i| i + 1).unwrap_or(0);
                self.input.truncate(cut);
            }
            (KeyCode::PageUp, _) => self.conv_scroll = self.conv_scroll.saturating_add(5),
            (KeyCode::PageDown, _) => self.conv_scroll = self.conv_scroll.saturating_sub(5),
            (KeyCode::Char(c), m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
                self.conv_scroll = 0;
                self.input.push(c);
            }
            _ => {}
        }
    }

    fn handle_approval_key(&mut self, key: KeyEvent) {
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(Decision::Allow),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Decision::Deny {
                message: "The developer declined this action.".into(),
            }),
            _ => None,
        };
        let Some(decision) = decision else { return };

        if let Some(req) = self.pending.take() {
            let allowed = matches!(decision, Decision::Allow | Decision::AllowWithInput(_));
            req.respond(decision);
            self.conversation.push(Line::Note(format!(
                "{} {}",
                if allowed { "allowed" } else { "denied" },
                bare_tool_name(&req.tool_name)
            )));
        }
    }

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.conv_scroll = 0;
        self.conversation.push(Line::User(text.clone()));

        match &self.agent {
            Some(a) => {
                a.send(text);
                self.busy = true;
            }
            None => self.conversation.push(Line::Note(
                "no agent is running, so this went nowhere.".into(),
            )),
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        let outer = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
        self.render_header(frame, outer[0]);

        let panes = Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(outer[1]);

        // The prompt box grows when there is a decision to make, so an approval
        // cannot be missed by a developer watching the terminal.
        let prompt_height = if self.pending.is_some() { 7 } else { 3 };
        let left = Layout::vertical([Constraint::Min(3), Constraint::Length(prompt_height)])
            .split(panes[0]);

        self.render_conversation(frame, left[0]);
        self.render_prompt(frame, left[1]);

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
                let name = r
                    .label
                    .unwrap_or_else(|| short_command(&r.command, &r.args));
                format!("{} — {name} [{state}]", r.id)
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

        let mut spans = vec![
            Span::styled(
                " Forge ",
                Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
            ),
            Span::raw(format!("  {}  ", self.workspace)),
            Span::styled(
                format!("{running} running  "),
                Style::default().fg(Color::DarkGray),
            ),
        ];

        if self.pending.is_some() {
            spans.push(Span::styled(
                " approval needed ",
                Style::default().fg(Color::Black).bg(Color::Yellow).bold(),
            ));
        } else if self.busy {
            spans.push(Span::styled(
                "working…",
                Style::default().fg(Color::Magenta),
            ));
        }

        frame.render_widget(Paragraph::new(Line_::from(spans)), area);
    }

    fn render_conversation(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Conversation && self.pending.is_none();
        let width = area.width.saturating_sub(2).max(8) as usize;
        let height = area.height.saturating_sub(2).max(1) as usize;

        let mut lines: Vec<Line_> = Vec::new();
        for entry in &self.conversation {
            render_entry(entry, width, &mut lines);
        }
        // Text still arriving is shown as it arrives, not held back until the
        // message is complete.
        if !self.streaming.is_empty() {
            lines.push(Line_::from(Span::styled(
                "claude",
                Style::default().fg(Color::Magenta).bold(),
            )));
            for l in wrap(&self.streaming, width) {
                lines.push(Line_::from(l));
            }
        }

        // Wrapping is done here rather than by the widget so that scrolling can
        // be exact: one unit of scroll is one line the developer can see.
        let max_start = lines.len().saturating_sub(height);
        let start = max_start.saturating_sub(self.conv_scroll);
        let view: Vec<Line_> = lines.into_iter().skip(start).take(height).collect();

        let mut title = " AI ".to_string();
        if self.conv_scroll > 0 {
            title = format!(" AI — scrolled back {} ", self.conv_scroll);
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if focused {
                Color::Cyan
            } else {
                Color::DarkGray
            }))
            .title(title);

        frame.render_widget(Paragraph::new(view).block(block), area);
    }

    fn render_prompt(&self, frame: &mut Frame, area: Rect) {
        if let Some(req) = &self.pending {
            let cap = req.capability;
            let colour = match cap {
                Capability::Destructive => Color::Red,
                Capability::Write | Capability::Execute => Color::Yellow,
                _ => Color::Cyan,
            };
            let width = area.width.saturating_sub(2).max(8) as usize;

            let mut lines = vec![Line_::from(vec![
                Span::styled(
                    format!(" {} ", cap.label()),
                    Style::default().fg(Color::Black).bg(colour).bold(),
                ),
                Span::raw(format!(" {}", bare_tool_name(&req.tool_name))),
            ])];
            for l in wrap(&summarise_call(&req.tool_name, &req.input), width) {
                lines.push(Line_::from(Span::styled(
                    l,
                    Style::default().fg(Color::White),
                )));
            }
            if let Some(reason) = &req.reason {
                for l in wrap(reason, width) {
                    lines.push(Line_::from(Span::styled(
                        l,
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            lines.push(Line_::from(vec![
                Span::styled("y", Style::default().fg(Color::Green).bold()),
                Span::raw(" allow   "),
                Span::styled("n", Style::default().fg(Color::Red).bold()),
                Span::raw(" deny"),
            ]));

            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(colour))
                .title(" the agent is waiting for you ");
            frame.render_widget(Paragraph::new(lines).block(block), area);
            return;
        }

        let focused = self.focus == Focus::Conversation;
        let (hint, style) = if self.agent.is_none() {
            (" no agent ", Style::default().fg(Color::DarkGray))
        } else if focused {
            (" enter to send ", Style::default().fg(Color::Cyan))
        } else {
            (" tab to type ", Style::default().fg(Color::DarkGray))
        };

        let cursor = if focused { "▏" } else { "" };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(style)
            .title_bottom(hint);

        frame.render_widget(
            Paragraph::new(Line_::from(vec![
                Span::styled("> ", Style::default().fg(Color::Green)),
                Span::raw(tail(&self.input, area.width.saturating_sub(5) as usize)),
                Span::raw(cursor),
            ]))
            .block(block),
            area,
        );
    }
}

fn render_entry(entry: &Line, width: usize, out: &mut Vec<Line_<'static>>) {
    match entry {
        Line::User(t) => {
            out.push(Line_::from(Span::styled(
                "you",
                Style::default().fg(Color::Green).bold(),
            )));
            out.extend(wrap(t, width).into_iter().map(Line_::from));
            out.push(Line_::from(""));
        }
        Line::Agent(t) => {
            out.push(Line_::from(Span::styled(
                "claude",
                Style::default().fg(Color::Magenta).bold(),
            )));
            out.extend(wrap(t, width).into_iter().map(Line_::from));
            out.push(Line_::from(""));
        }
        Line::Thinking(t) => {
            for l in wrap(t, width) {
                out.push(Line_::from(Span::styled(
                    l,
                    Style::default().fg(Color::DarkGray).italic(),
                )));
            }
        }
        Line::Tool {
            name,
            summary,
            capability,
        } => {
            out.push(Line_::from(vec![
                Span::styled("→ ", Style::default().fg(Color::Blue)),
                Span::styled(name.clone(), Style::default().fg(Color::Blue).bold()),
                Span::styled(
                    format!("  {}", capability.label()),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            for l in wrap(summary, width.saturating_sub(2)) {
                out.push(Line_::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(Color::Gray),
                )));
            }
        }
        Line::ToolResult { is_error, text } => {
            let colour = if *is_error {
                Color::Red
            } else {
                Color::DarkGray
            };
            let mut shown: Vec<String> = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(TOOL_RESULT_LINES)
                .map(|l| tail_prefix(l, width.saturating_sub(2)))
                .collect();
            let total = text.lines().filter(|l| !l.trim().is_empty()).count();
            if total > TOOL_RESULT_LINES {
                shown.push(format!("… {} more lines", total - TOOL_RESULT_LINES));
            }
            if shown.is_empty() {
                shown.push("(no output)".into());
            }
            for l in shown {
                out.push(Line_::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(colour),
                )));
            }
            out.push(Line_::from(""));
        }
        Line::Note(t) => {
            for l in wrap(&format!("· {t}"), width) {
                out.push(Line_::from(Span::styled(
                    l,
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }
}

/// Wrap on word boundaries, breaking words only when they do not fit at all.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        for word in raw.split(' ') {
            if word.chars().count() > width {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                let mut chunk = String::new();
                for c in word.chars() {
                    if chunk.chars().count() == width {
                        out.push(std::mem::take(&mut chunk));
                    }
                    chunk.push(c);
                }
                line = chunk;
                continue;
            }
            let extra = if line.is_empty() { 0 } else { 1 };
            if line.chars().count() + extra + word.chars().count() > width {
                out.push(std::mem::take(&mut line));
            } else if extra == 1 {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

/// The trailing `width` characters — what an input field should show.
fn tail(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        return s.to_string();
    }
    s.chars().skip(n - width).collect()
}

/// The leading `width` characters, marked when truncated.
fn tail_prefix(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut t: String = s.chars().take(width.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// `mcp__forge__proc_start` reads as noise in a conversation.
pub fn bare_tool_name(name: &str) -> String {
    name.rsplit("__").next().unwrap_or(name).to_string()
}

/// A one-line description of a tool call, in the terms the developer thinks in.
pub fn summarise_call(name: &str, input: &serde_json::Value) -> String {
    let get = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("");

    match bare_tool_name(name).as_str() {
        "proc_start" => {
            let args: Vec<String> = input
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let mut s = short_command(get("command"), &args);
            let cwd = get("cwd");
            if !cwd.is_empty() {
                s.push_str(&format!("   in {cwd}"));
            }
            s
        }
        "proc_signal" => format!("{} to {}", get("signal"), get("process_id")),
        "proc_input" => format!("type {:?} into {}", get("data"), get("process_id")),
        "proc_list" => "list processes".into(),

        // Edits are auto-approved inside the workspace, so this line is the
        // developer's only notice that a file changed. It has to say which
        // file, and how much of it — the same reason the terminal pane exists.
        "Edit" | "Write" | "NotebookEdit" => {
            let path = short_path(get("file_path"));
            match edit_size(input) {
                Some(n) => format!("{path}  {n}"),
                None => path,
            }
        }
        "Read" => short_path(get("file_path")),
        "WebFetch" => get("url").to_string(),
        "WebSearch" => format!("search {:?}", get("query")),

        _ => {
            let id = get("process_id");
            if id.is_empty() {
                compact(input)
            } else {
                id.to_string()
            }
        }
    }
}

/// The tail of a path, which is what identifies a file in a narrow pane.
fn short_path(p: &str) -> String {
    if p.is_empty() {
        return "(no path)".into();
    }
    let parts: Vec<&str> = p.rsplit('/').take(3).collect();
    let short: String = parts.into_iter().rev().collect::<Vec<_>>().join("/");
    if short.len() < p.len() {
        format!("…/{short}")
    } else {
        short
    }
}

/// How much an edit changes, in the terms the tool's own arguments give us.
fn edit_size(input: &serde_json::Value) -> Option<String> {
    let lines = |k: &str| {
        input
            .get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.lines().count())
    };
    match (lines("old_string"), lines("new_string")) {
        (Some(old), Some(new)) => Some(format!("-{old} +{new}")),
        // `Write` replaces the file outright; the count is the whole thing.
        _ => lines("content").map(|n| format!("{n} lines")),
    }
}

fn short_command(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        command.to_string()
    } else {
        format!("{command} {}", args.join(" "))
    }
}

fn compact(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// ratatui's `Line` collides with our conversation `Line`.
use ratatui::text::Line as Line_;

/// Inner size of the terminal pane for a given screen size.
pub fn pane_size(rows: u16, cols: u16) -> (u16, u16) {
    let pane_cols = (cols as f32 * 0.62) as u16;
    (
        rows.saturating_sub(3).max(1),
        pane_cols.saturating_sub(2).max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use forge_agent::{ApprovalRequest, Capability, Decision};
    use serde_json::json;

    fn app() -> App {
        App::new("test".into())
    }

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn notes(app: &App) -> Vec<String> {
        app.conversation
            .iter()
            .filter_map(|l| match l {
                Line::Note(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn wrapping_breaks_on_words_and_preserves_every_character() {
        let text = "start the yocto build and tell me when it fails";
        let lines = wrap(text, 12);
        assert!(lines.iter().all(|l| l.chars().count() <= 12), "{lines:?}");
        assert_eq!(lines.join(" "), text);
    }

    #[test]
    fn a_word_longer_than_the_pane_is_broken_rather_than_lost() {
        // Paths and build-directory names routinely exceed a narrow pane.
        let long = "/var/lib/yocto/build/tmp/work/core2-64-poky-linux/busybox";
        let lines = wrap(long, 20);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| l.chars().count() <= 20), "{lines:?}");
        assert_eq!(lines.concat(), long);
    }

    #[test]
    fn wrapping_keeps_explicit_line_breaks() {
        assert_eq!(
            wrap("one\ntwo", 40),
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn tool_names_are_shown_without_their_transport_prefix() {
        assert_eq!(bare_tool_name("mcp__forge__proc_start"), "proc_start");
        assert_eq!(bare_tool_name("proc_wait"), "proc_wait");
    }

    #[test]
    fn a_start_call_reads_as_the_command_it_will_run() {
        let s = summarise_call(
            "mcp__forge__proc_start",
            &json!({"command": "bitbake", "args": ["core-image-minimal"], "cwd": "/build"}),
        );
        assert_eq!(s, "bitbake core-image-minimal   in /build");
    }

    #[test]
    fn a_signal_call_names_the_signal_and_its_target() {
        let s = summarise_call(
            "mcp__forge__proc_signal",
            &json!({"process_id": "proc-2", "signal": "KILL"}),
        );
        assert_eq!(s, "KILL to proc-2");
    }

    #[test]
    fn other_calls_fall_back_to_the_process_they_name() {
        assert_eq!(
            summarise_call("mcp__forge__proc_output", &json!({"process_id": "proc-7"})),
            "proc-7"
        );
    }

    #[test]
    fn typing_goes_to_the_input_line_and_editing_keys_work() {
        let mut a = app();
        a.focus = Focus::Conversation;
        a.pane.set_focus(false);
        for c in "build it now".chars() {
            a.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(a.input, "build it now");
        a.handle_key(ctrl('w'));
        assert_eq!(a.input, "build it ");
        a.handle_key(ctrl('u'));
        assert_eq!(a.input, "");
    }

    #[test]
    fn submitting_without_an_agent_says_so_instead_of_swallowing_the_message() {
        let mut a = app();
        a.focus = Focus::Conversation;
        a.pane.set_focus(false);
        a.input = "start the build".into();
        a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(a.conversation.last_mut(), Some(Line::Note(_))));
        assert!(notes(&a).iter().any(|n| n.contains("no agent")));
        // The message is still recorded, so nothing the developer typed is lost.
        assert!(a
            .conversation
            .iter()
            .any(|l| matches!(l, Line::User(t) if t == "start the build")));
        assert!(!a.busy, "there is nothing to be busy with");
    }

    #[test]
    fn ctrl_c_quits_only_when_there_is_no_turn_to_cancel() {
        let mut a = app();
        a.focus = Focus::Conversation;
        a.pane.set_focus(false);

        a.busy = true;
        a.handle_key(ctrl('c'));
        assert!(!a.should_quit(), "Ctrl-C during a turn cancels the turn");
        assert!(!a.busy);

        a.handle_key(ctrl('c'));
        assert!(a.should_quit());
    }

    #[test]
    fn a_pending_decision_takes_every_key_until_it_is_answered() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut a = app();
        a.on_agent_event(AgentEvent::ApprovalRequested(ApprovalRequest::new(
            "r1".into(),
            "mcp__forge__proc_signal".into(),
            json!({"process_id": "proc-1", "signal": "KILL"}),
            Capability::Destructive,
            None,
            tx,
        )));

        // Focus is pulled off the terminal so the prompt cannot be missed.
        assert_eq!(a.focus, Focus::Conversation);
        assert!(!a.pane.focused());

        // Keys that are not a decision do nothing at all — no stray typing
        // lands in the input line while the agent is blocked.
        a.handle_key(press('x'));
        a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a.input, "");
        assert!(a.pending.is_some());
        assert!(rx.try_recv().is_err());

        a.handle_key(press('n'));
        assert!(a.pending.is_none());
        assert!(matches!(rx.try_recv(), Ok(Decision::Deny { .. })));
        assert!(notes(&a).iter().any(|n| n == "denied proc_signal"));
    }

    #[test]
    fn approval_can_be_granted() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut a = app();
        a.pending = Some(ApprovalRequest::new(
            "r1".into(),
            "mcp__forge__proc_start".into(),
            json!({"command": "bitbake"}),
            Capability::Execute,
            None,
            tx,
        ));
        a.handle_key(press('y'));
        assert!(matches!(rx.try_recv(), Ok(Decision::Allow)));
        assert!(notes(&a).iter().any(|n| n == "allowed proc_start"));
    }

    #[test]
    fn a_tool_the_agent_calls_is_shown_with_its_capability() {
        let mut a = app();
        a.on_agent_event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "mcp__forge__proc_start".into(),
            input: json!({"command": "bitbake", "args": ["core-image-minimal"]}),
        });
        match a.conversation.last() {
            Some(Line::Tool {
                name,
                summary,
                capability,
            }) => {
                assert_eq!(name, "proc_start");
                assert_eq!(summary, "bitbake core-image-minimal");
                assert_eq!(*capability, Capability::Execute);
            }
            other => panic!("expected a tool line, got {other:?}"),
        }
    }

    #[test]
    fn a_complete_message_replaces_what_was_streamed() {
        let mut a = app();
        a.on_agent_event(AgentEvent::TextDelta("star".into()));
        a.on_agent_event(AgentEvent::TextDelta("ting".into()));
        assert_eq!(a.streaming, "starting");
        a.on_agent_event(AgentEvent::Text("starting the build".into()));
        assert!(
            a.streaming.is_empty(),
            "no duplicated text after the message lands"
        );
        assert!(matches!(a.conversation.last(), Some(Line::Agent(t)) if t == "starting the build"));
    }

    #[test]
    fn a_clean_tool_surface_is_reported_quietly() {
        let mut a = app();
        a.on_agent_event(AgentEvent::Ready {
            tools: vec![
                "Read".into(),
                "Edit".into(),
                "mcp__forge__proc_start".into(),
                "mcp__forge__proc_wait".into(),
            ],
        });
        assert!(
            notes(&a)
                .iter()
                .any(|n| n == "2 built-in + 2 Forge tools · no shell"),
            "{:?}",
            notes(&a)
        );
    }

    /// Editing tools are expected now. A *shell* is the thing that must never
    /// appear: its output would go to the model's context, not to the pane.
    #[test]
    fn editing_tools_are_not_treated_as_foreign() {
        let mut a = app();
        a.on_agent_event(AgentEvent::Ready {
            tools: vec!["Read".into(), "Edit".into(), "Write".into()],
        });
        assert!(
            !notes(&a).iter().any(|n| n.contains("WARNING")),
            "{:?}",
            notes(&a)
        );
    }

    #[test]
    fn every_shell_tool_is_reported_loudly() {
        for shell in ["Bash", "Task", "Workflow", "Skill"] {
            let mut a = app();
            a.on_agent_event(AgentEvent::Ready {
                tools: vec!["Read".into(), shell.to_string()],
            });
            let n = notes(&a);
            assert!(
                n.iter().any(|x| x.contains("WARNING") && x.contains(shell)),
                "{shell} passed unnoticed: {n:?}"
            );
        }
    }

    /// Edits are auto-approved inside the workspace, so the conversation line
    /// is the developer's only notice that a file changed. It must name the
    /// file and say how much moved.
    #[test]
    fn an_edit_says_which_file_and_how_much_changed() {
        let mut a = app();
        a.on_agent_event(AgentEvent::ToolCall {
            id: "t1".into(),
            name: "Edit".into(),
            input: json!({
                "file_path": "/home/dev/src/meta-foo/recipes-core/busybox_1.36.bb",
                "old_string": "one\ntwo",
                "new_string": "one\ntwo\nthree",
            }),
        });
        match a.conversation.last() {
            Some(Line::Tool {
                name,
                summary,
                capability,
            }) => {
                assert_eq!(name, "Edit");
                assert_eq!(*capability, Capability::Write);
                assert!(summary.contains("busybox_1.36.bb"), "{summary}");
                assert!(summary.contains("-2 +3"), "{summary}");
            }
            other => panic!("expected a tool line, got {other:?}"),
        }
    }

    #[test]
    fn a_whole_file_write_reports_its_size() {
        let s = summarise_call(
            "Write",
            &json!({"file_path": "/tmp/a/b/new.rs", "content": "x\ny\nz"}),
        );
        assert!(s.contains("new.rs") && s.contains("3 lines"), "{s}");
    }

    #[test]
    fn a_long_path_keeps_the_end_that_identifies_it() {
        assert_eq!(short_path("/a/b/c/d/e/f.rs"), "…/d/e/f.rs");
        assert_eq!(short_path("short.rs"), "short.rs");
        assert_eq!(short_path(""), "(no path)");
    }

    #[tokio::test]
    async fn a_process_the_agent_starts_appears_in_the_pane_unasked() {
        let mut a = app();
        a.start_process(ProcessSpec::new("cat").cwd("/")).unwrap();
        let first = a.pane.process.unwrap();

        // Started behind the UI's back, exactly as the MCP tool does it.
        let second = a.pm.start(ProcessSpec::new("cat").cwd("/")).unwrap();
        assert_ne!(first, second);

        a.follow_new_processes();
        assert_eq!(
            a.pane.process,
            Some(second),
            "the developer sees it without asking"
        );
        assert!(notes(&a).iter().any(|n| n.contains(&format!("{second}"))));

        // Idempotent: the same process is not followed twice, so a developer
        // who switches away with Ctrl-] is not dragged back on every frame.
        a.follow(first).unwrap();
        a.follow_new_processes();
        assert_eq!(a.pane.process, Some(first));
    }
}
