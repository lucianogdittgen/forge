//! The agent port, and a Claude implementation behind it.
//!
//! Nothing outside this crate may know that Claude is involved. The rest of
//! Forge speaks [`Agent`] and [`AgentEvent`], which are Forge-owned types; the
//! CLI's session ids, MCP wire names (`mcp__forge__*`) and control protocol all
//! stop at this boundary. That seam is what makes ADR-0002 reversible.

pub mod claude;
pub mod protocol;
pub mod permission;

pub use permission::{Capability, Decision};

use std::pin::Pin;

use tokio::sync::mpsc;

/// A Forge-owned identifier for a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionId(pub String);

/// Everything the UI needs to render agent activity.
///
/// Deliberately narrow: no provider-specific fields, so a future backend can
/// produce the same events without the UI changing.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Incremental assistant text for live rendering.
    TextDelta(String),
    /// A complete assistant message.
    Text(String),
    /// Summarised reasoning, when the model is configured to emit it.
    Thinking(String),
    /// The agent wants to run a tool.
    ToolCall { id: String, name: String, input: serde_json::Value },
    /// A tool finished.
    ToolResult { id: String, is_error: bool, content: String },
    /// Forge must decide whether this call may proceed.
    ///
    /// The UI answers by resolving the paired responder; the agent process
    /// blocks meanwhile, which is expected and safe.
    ApprovalRequested(ApprovalRequest),
    /// The turn ended.
    TurnFinished { session: SessionId, cost_usd: Option<f64>, is_error: bool },
    /// Non-fatal problem worth surfacing.
    Warning(String),
}

/// A pending permission decision.
#[derive(Debug)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub capability: Capability,
    /// Why the gate fired, when the agent process told us.
    pub reason: Option<String>,
    responder: mpsc::UnboundedSender<Decision>,
}

impl Clone for ApprovalRequest {
    fn clone(&self) -> Self {
        Self {
            request_id: self.request_id.clone(),
            tool_name: self.tool_name.clone(),
            input: self.input.clone(),
            capability: self.capability,
            reason: self.reason.clone(),
            responder: self.responder.clone(),
        }
    }
}

impl ApprovalRequest {
    pub fn new(
        request_id: String,
        tool_name: String,
        input: serde_json::Value,
        capability: Capability,
        reason: Option<String>,
        responder: mpsc::UnboundedSender<Decision>,
    ) -> Self {
        Self { request_id, tool_name, input, capability, reason, responder }
    }

    /// Answer the request. Safe to call once; later calls are ignored.
    pub fn respond(&self, decision: Decision) {
        let _ = self.responder.send(decision);
    }
}

pub type EventStream = Pin<Box<dyn futures::Stream<Item = AgentEvent> + Send>>;

/// What Forge requires of an agent backend.
#[allow(async_fn_in_trait)]
pub trait Agent {
    /// Send a user turn and stream what happens.
    async fn send(&mut self, message: &str) -> anyhow::Result<()>;
    /// Cancel the turn in flight.
    async fn interrupt(&mut self) -> anyhow::Result<()>;
    /// Receive the next event, or `None` when the agent has stopped.
    async fn next_event(&mut self) -> Option<AgentEvent>;
    fn session(&self) -> Option<&SessionId>;
}
