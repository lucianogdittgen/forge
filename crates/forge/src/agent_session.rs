//! Owns the agent and the tool endpoint it talks to.
//!
//! The UI never holds an [`Agent`] directly. It holds channels, because the
//! agent must keep making progress while the UI is busy drawing frames and
//! pumping a PTY — and because a turn that blocks on a permission decision must
//! not block the terminal pane that the developer is watching.
//!
//! Startup order matters: the MCP endpoint must be listening before the agent
//! process is spawned, or its first `tools/list` finds nothing and the session
//! runs with an empty tool surface.

use std::path::PathBuf;

use anyhow::{Context, Result};
use forge_agent::claude::{ClaudeAgent, ClaudeAgentConfig};
use forge_agent::{Agent, AgentEvent, Capability};
use forge_mcp::McpServer;
use forge_process::ProcessManager;
use tokio::sync::mpsc;

/// What the UI asks of the agent.
pub enum AgentCommand {
    Send(String),
    Interrupt,
}

pub struct AgentSession {
    commands: mpsc::UnboundedSender<AgentCommand>,
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    /// Held for its lifetime: dropping it stops serving tools.
    _mcp: McpServer,
}

impl AgentSession {
    pub async fn start(pm: ProcessManager, cwd: PathBuf) -> Result<Self> {
        let mcp = McpServer::start(pm, cwd.clone())
            .await
            .context("could not start Forge's tool server")?;

        let mut cfg = ClaudeAgentConfig::new(cwd.clone());
        cfg.mcp_url = Some(mcp.url.clone());
        cfg.system_prompt = Some(system_prompt(&cwd));
        // Read-only calls go through without interrupting the developer.
        // Everything that starts, feeds or kills a process is asked about, and
        // `Destructive` is never auto-approved however this list is set.
        cfg.granted = vec![Capability::Read];

        let mut agent = ClaudeAgent::spawn(cfg).await?;

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AgentCommand>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => match cmd {
                        Some(AgentCommand::Send(text)) => {
                            if let Err(e) = agent.send(&text).await {
                                let _ = ev_tx.send(AgentEvent::Warning(e.to_string()));
                            }
                        }
                        Some(AgentCommand::Interrupt) => {
                            let _ = agent.interrupt().await;
                        }
                        // UI gone; nothing left to serve.
                        None => break,
                    },
                    ev = agent.next_event() => match ev {
                        Some(ev) => { let _ = ev_tx.send(ev); }
                        None => break,
                    },
                }
            }
        });

        Ok(Self {
            commands: cmd_tx,
            events: ev_rx,
            _mcp: mcp,
        })
    }

    pub fn send(&self, text: impl Into<String>) {
        let _ = self.commands.send(AgentCommand::Send(text.into()));
    }

    pub fn interrupt(&self) {
        let _ = self.commands.send(AgentCommand::Interrupt);
    }
}

/// What the agent needs to know about the room it is in.
///
/// Most of this exists to stop the model doing the helpful thing that ruins the
/// product: summarising output the developer is already watching, or waiting
/// out a forty-minute build in silence because it thinks a tool call must
/// return a finished result.
fn system_prompt(cwd: &std::path::Path) -> String {
    format!(
        "You are the agent inside Forge, a terminal workbench. The developer is working at \
         {cwd} and is looking at a live terminal pane next to this conversation.\n\
         \n\
         You can read and edit files normally. What you do not have is a shell: there is no \
         Bash tool, and running a program is always proc_start.\n\
         \n\
         Everything you start with proc_start appears in that pane immediately and streams \
         there in real time, exactly as if the developer had typed the command. They can watch \
         it, scroll it, type into it and press Ctrl-C on it while you work. Searching the tree \
         is a command too — use proc_start with grep, rg or find.\n\
         \n\
         Because they can already see it:\n\
         - Do not relay or summarise output they are watching. Say what you concluded from it, \
           not what it said.\n\
         - proc_start returns an id, not a result. A build that takes forty minutes is normal. \
           Use proc_wait when you need the outcome, and keep working when you do not.\n\
         - proc_wait's exit code is the cheap way to learn whether something worked. Prefer it.\n\
         - proc_output costs the developer context and is hard-capped. Read it to diagnose a \
           specific failure, never to follow a build along. If you need more than the cap, run \
           a narrower command rather than asking for more lines.\n\
         \n\
         Edits inside {cwd} go through without interrupting them. Anything outside it, and \
         anything that could destroy work such as signalling a process, needs their approval \
         and they may refuse; treat a refusal as a decision, not an obstacle.",
        cwd = cwd.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The whole loop, against the real `claude` binary.
    ///
    /// Ignored by default: it needs the CLI installed, working credentials and
    /// a network round trip, none of which belong in `cargo test`. Run it by
    /// hand with `cargo test -p forge -- --ignored --nocapture` when changing
    /// anything about how the agent is launched.
    #[tokio::test]
    #[ignore = "spawns the real claude CLI"]
    async fn the_agent_can_start_a_process_the_developer_would_see() {
        let dir = std::env::temp_dir().join("forge-e2e-proc");
        std::fs::create_dir_all(&dir).unwrap();
        let pm = ProcessManager::new();
        let mut session = AgentSession::start(pm.clone(), dir.clone()).await.unwrap();

        session.send("Run the command `echo forge-e2e` and tell me nothing else.");

        let mut tools: Vec<String> = Vec::new();
        let mut started = false;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let ev = tokio::time::timeout_at(deadline, session.events.recv())
                .await
                .expect("timed out waiting for the agent");
            let Some(ev) = ev else { break };
            eprintln!("{ev:?}");
            match ev {
                AgentEvent::Ready { tools: t } => tools = t,
                AgentEvent::ApprovalRequested(req) => {
                    // Standing in for the developer pressing `y`.
                    req.respond(forge_agent::Decision::Allow);
                }
                AgentEvent::ToolCall { name, .. } if name.ends_with("proc_start") => {
                    started = true;
                }
                AgentEvent::TurnFinished { .. } => break,
                _ => {}
            }
        }

        // The surface is a coding agent's, minus any way to run a command that
        // the pane could not show.
        assert!(
            tools.contains(&"Edit".to_string()),
            "no edit tool: {tools:?}"
        );
        assert!(
            tools.contains(&"Read".to_string()),
            "no read tool: {tools:?}"
        );
        for shell in ["Bash", "Task", "Workflow", "Skill"] {
            assert!(
                !tools.contains(&shell.to_string()),
                "{shell} leaked in: {tools:?}"
            );
        }
        assert!(started, "the agent never called proc_start");

        // The point of the product: the process exists in the manager the
        // terminal pane reads from, not somewhere only the agent can see.
        let seen = pm.list();
        assert!(
            seen.iter()
                .any(|r| r.command.contains("echo")
                    || r.args.iter().any(|a| a.contains("forge-e2e"))),
            "nothing reached the process manager: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The edit gate, end to end: inside the workspace it must not interrupt,
    /// outside it must. Auto-approval is only safe if "outside" really asks.
    #[tokio::test]
    #[ignore = "spawns the real claude CLI"]
    async fn edits_inside_the_workspace_do_not_interrupt_but_outside_ones_do() {
        let dir = std::env::temp_dir().join("forge-e2e-edit");
        std::fs::create_dir_all(&dir).unwrap();
        let inside = dir.join("inside.txt");
        std::fs::write(&inside, "before\n").unwrap();

        let outside = std::env::temp_dir().join("forge-e2e-outside.txt");
        std::fs::write(&outside, "before\n").unwrap();

        let pm = ProcessManager::new();
        let mut session = AgentSession::start(pm, dir.clone()).await.unwrap();
        session.send(format!(
            "Use Edit to change `before` to `after` in {}, then use Edit to do exactly \
             the same in {}. No commentary.",
            inside.display(),
            outside.display()
        ));

        let mut gated: Vec<String> = Vec::new();
        let mut edited: Vec<String> = Vec::new();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        loop {
            let ev = tokio::time::timeout_at(deadline, session.events.recv())
                .await
                .expect("timed out");
            let Some(ev) = ev else { break };
            match ev {
                AgentEvent::ToolCall { name, input, .. } => {
                    if name == "Edit" {
                        edited.push(
                            input
                                .get("file_path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        );
                    }
                }
                AgentEvent::ApprovalRequested(req) => {
                    gated.push(
                        req.input
                            .get("file_path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    );
                    req.respond(forge_agent::Decision::Allow);
                }
                AgentEvent::TurnFinished { .. } => break,
                _ => {}
            }
        }

        eprintln!("edited: {edited:?}\ngated:  {gated:?}");
        let inside_s = inside.to_string_lossy().to_string();
        let outside_s = outside.to_string_lossy().to_string();

        assert!(edited.contains(&inside_s), "never edited inside");
        assert!(
            !gated.contains(&inside_s),
            "an edit inside the workspace interrupted the developer: {gated:?}"
        );
        assert!(
            gated.contains(&outside_s),
            "an edit OUTSIDE the workspace went through unasked: gated={gated:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
    }
}
