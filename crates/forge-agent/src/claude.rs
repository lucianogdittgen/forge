//! Claude backend: spawns and drives the `claude` CLI.
//!
//! Everything Claude-specific stops here. See ADR-0002 for why this drives the
//! CLI directly rather than going through a language SDK — chiefly that the
//! SDK is itself a wrapper around this same binary and this same protocol.

use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::permission::{Capability, Decision};
use crate::protocol::{self, ContentBlock, Incoming};
use crate::{Agent, AgentEvent, ApprovalRequest, SessionId};

/// Flags that would silently disarm Forge's permission gate.
///
/// The spike proved each of these bypasses the gate entirely — including
/// allow-rules planted in the user's own `~/.claude/settings.json`. Because the
/// security property lives in argv rather than in the type system, the likely
/// regression is a future maintainer adding one of these for convenience. We
/// assert on our own argv before spawning so that fails loudly instead.
const FORBIDDEN_FLAGS: &[&str] = &[
    "--allowed-tools",
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
];

pub struct ClaudeAgentConfig {
    pub binary: String,
    pub model: String,
    pub cwd: std::path::PathBuf,
    /// Forge-owned config dir, so the user's `~/.claude` is never touched.
    pub config_dir: std::path::PathBuf,
    /// MCP endpoint Forge serves its own tools from.
    pub mcp_url: Option<String>,
    pub system_prompt: Option<String>,
    pub session_id: Option<String>,
    /// Capabilities granted for the session; `Destructive` always asks anyway.
    pub granted: Vec<Capability>,
}

impl ClaudeAgentConfig {
    pub fn new(cwd: impl Into<std::path::PathBuf>) -> Self {
        let cwd = cwd.into();
        Self {
            binary: "claude".into(),
            model: "claude-opus-5".into(),
            config_dir: cwd.join(".forge").join("claude"),
            cwd,
            mcp_url: None,
            system_prompt: None,
            session_id: None,
            granted: vec![Capability::Read],
        }
    }

    /// Build argv. See ADR-0002 — every flag here is load-bearing.
    pub fn argv(&self) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "-p".into(),
            "--model".into(),
            self.model.clone(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            // stream-json emits nothing useful without it.
            "--verbose".into(),
            // Required for token-level deltas.
            "--include-partial-messages".into(),
            // Q1: removes all 24 built-ins. Without this the agent could run a
            // build through Bash, and the terminal pane would never see it.
            "--tools".into(),
            String::new(),
            // Q2: refuse the user's own MCP servers.
            "--strict-mcp-config".into(),
            "--disable-slash-commands".into(),
            // Q3: MANDATORY. Blocks user/project/local settings, which can
            // otherwise plant allow-rules that void the gate entirely.
            "--setting-sources".into(),
            String::new(),
            // Q3: the SDK's canUseTool, over the stdio control protocol.
            "--permission-prompt-tool".into(),
            "stdio".into(),
        ];

        if let Some(url) = &self.mcp_url {
            a.push("--mcp-config".into());
            a.push(format!(
                r#"{{"mcpServers":{{"forge":{{"type":"http","url":"{url}"}}}}}}"#
            ));
        }
        if let Some(sp) = &self.system_prompt {
            a.push("--system-prompt".into());
            a.push(sp.clone());
        }
        if let Some(sid) = &self.session_id {
            a.push("--session-id".into());
            a.push(sid.clone());
        }
        a
    }

    /// Refuse to start if argv contains anything that would void the gate.
    pub fn assert_argv_safe(argv: &[String]) -> Result<()> {
        for f in FORBIDDEN_FLAGS {
            if argv.iter().any(|a| a == f || a.starts_with(&format!("{f}="))) {
                bail!(
                    "refusing to start: {f} bypasses Forge's permission gate \
                     (see ADR-0002); the agent would be able to act without approval"
                );
            }
        }
        // bypassPermissions is the one --permission-mode value that voids the gate.
        if let Some(i) = argv.iter().position(|a| a == "--permission-mode") {
            if argv.get(i + 1).map(|s| s.as_str()) == Some("bypassPermissions") {
                bail!(
                    "refusing to start: --permission-mode bypassPermissions bypasses \
                     Forge's permission gate (see ADR-0002)"
                );
            }
        }
        Ok(())
    }
}

pub struct ClaudeAgent {
    child: Child,
    /// User turns. Ownership of stdin belongs to the writer task, so that
    /// turns and permission decisions cannot interleave mid-line.
    turns: mpsc::UnboundedSender<String>,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    session: Option<SessionId>,
}

impl ClaudeAgent {
    pub async fn spawn(cfg: ClaudeAgentConfig) -> Result<Self> {
        let argv = cfg.argv();
        ClaudeAgentConfig::assert_argv_safe(&argv)?;

        std::fs::create_dir_all(&cfg.config_dir).ok();

        let mut cmd = Command::new(&cfg.binary);
        cmd.args(&argv)
            .current_dir(&cfg.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Explicit, minimal environment. Inheriting the user's would pick up
            // their proxy base URL and stored credentials.
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()))
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("CLAUDE_CONFIG_DIR", &cfg.config_dir)
            .env("CLAUDE_CODE_ENTRYPOINT", "sdk-rs");

        for k in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"] {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }

        let mut child = cmd.spawn().with_context(|| {
            format!("could not spawn `{}` — is the Claude CLI installed?", cfg.binary)
        })?;

        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;

        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (dec_tx, dec_rx) = mpsc::unbounded_channel();
        let (turn_tx, turn_rx) = mpsc::unbounded_channel();

        Self::spawn_reader(stdout, ev_tx.clone(), dec_tx, cfg.granted.clone());
        Self::spawn_stderr(stderr, ev_tx);
        Self::spawn_writer(stdin, dec_rx, turn_rx);

        Ok(Self { child, turns: turn_tx, events: ev_rx, session: None })
    }

    /// Read stdout, turn protocol events into Forge events.
    fn spawn_reader(
        stdout: tokio::process::ChildStdout,
        events: mpsc::UnboundedSender<AgentEvent>,
        decisions: mpsc::UnboundedSender<(String, Decision)>,
        granted: Vec<Capability>,
    ) {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match protocol::parse_line(&line) {
                    // Repeats every turn; not a handshake.
                    Incoming::Init { .. } => {}

                    Incoming::Assistant { content } | Incoming::User { content } => {
                        for b in content {
                            let ev = match b {
                                ContentBlock::Text(t) if !t.is_empty() => AgentEvent::Text(t),
                                ContentBlock::Thinking(t) if !t.is_empty() => {
                                    AgentEvent::Thinking(t)
                                }
                                ContentBlock::ToolUse { id, name, input } => {
                                    AgentEvent::ToolCall { id, name, input }
                                }
                                ContentBlock::ToolResult { id, is_error, content } => {
                                    AgentEvent::ToolResult { id, is_error, content }
                                }
                                _ => continue,
                            };
                            let _ = events.send(ev);
                        }
                    }

                    Incoming::StreamDelta { kind, text } if kind == "text_delta" => {
                        let _ = events.send(AgentEvent::TextDelta(text));
                    }
                    Incoming::StreamDelta { .. } => {}

                    Incoming::ControlRequest { request_id, tool_name, input, reason } => {
                        let cap = Capability::of_tool(&tool_name);

                        // Auto-approve only read-ish capabilities the session
                        // granted. Destructive never qualifies, however granted.
                        if cap.auto_approvable(&granted) {
                            let _ = decisions.send((request_id, Decision::Allow));
                            continue;
                        }

                        let (tx, mut rx) = mpsc::unbounded_channel();
                        let req = ApprovalRequest::new(
                            request_id.clone(),
                            tool_name,
                            input,
                            cap,
                            reason,
                            tx,
                        );
                        let _ = events.send(AgentEvent::ApprovalRequested(req));

                        // Wait for the UI. The agent process blocks meanwhile,
                        // which is intended: the spike verified a 123 s wait
                        // with no timeout.
                        let decisions = decisions.clone();
                        tokio::spawn(async move {
                            let d = rx.recv().await.unwrap_or(Decision::Deny {
                                message: "Forge: no decision was made.".into(),
                            });
                            let _ = decisions.send((request_id, d));
                        });
                    }

                    Incoming::Result { session_id, is_error, cost_usd } => {
                        let _ = events.send(AgentEvent::TurnFinished {
                            session: SessionId(session_id),
                            cost_usd,
                            is_error,
                        });
                    }

                    // Unknown types and non-JSON lines are never fatal.
                    Incoming::Other { kind } => {
                        tracing::debug!(kind, "unhandled agent event");
                    }
                    Incoming::Unparsed(l) => {
                        tracing::debug!(line = %l, "non-JSON line from agent");
                    }
                }
            }
        });
    }

    fn spawn_stderr(
        stderr: tokio::process::ChildStderr,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    let _ = events.send(AgentEvent::Warning(line));
                }
            }
        });
    }

    /// Sole owner of the agent's stdin.
    ///
    /// One task writes both user turns and permission decisions so that two
    /// writers can never interleave halfway through a JSON line.
    fn spawn_writer(
        mut stdin: ChildStdin,
        mut decisions: mpsc::UnboundedReceiver<(String, Decision)>,
        mut turns: mpsc::UnboundedReceiver<String>,
    ) {
        tokio::spawn(async move {
            loop {
                let line = tokio::select! {
                    Some((id, d)) = decisions.recv() => protocol::control_response(&id, &d),
                    Some(turn) = turns.recv() => turn,
                    else => break,
                };
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });
    }
}

impl Agent for ClaudeAgent {
    async fn send(&mut self, message: &str) -> Result<()> {
        let turn = serde_json::to_string(&protocol::UserTurn::new(message))?;
        self.turns
            .send(turn)
            .map_err(|_| anyhow::anyhow!("agent process has stopped"))?;
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<()> {
        // The CLI has no in-band interrupt on this transport; signalling the
        // process is the supported way to cancel a turn in flight.
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGINT);
        }
        Ok(())
    }

    async fn next_event(&mut self) -> Option<AgentEvent> {
        let ev = self.events.recv().await?;
        if let AgentEvent::TurnFinished { session, .. } = &ev {
            self.session = Some(session.clone());
        }
        Some(ev)
    }

    fn session(&self) -> Option<&SessionId> {
        self.session.as_ref()
    }
}
