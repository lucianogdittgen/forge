//! Capability classification and decisions.
//!
//! Forge's tools each declare a capability, so classification is a lookup
//! rather than an attempt to parse arbitrary shell. That distinction matters:
//! statically classifying a shell command is unreliable in the general case
//! (`eval`, `$(...)`, pipes, aliases, `rm -rf $VAR` where `VAR` is empty), so it
//! is used only to *reduce prompting*, never as the security boundary. The
//! boundary is the gate itself, which asks.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    Read,
    Write,
    Execute,
    Network,
    /// Irreversible, or destroys work: killing a process, discarding changes.
    Destructive,
}

impl Capability {
    /// The capability a Forge tool requires.
    ///
    /// Unknown tools are `Destructive` on purpose: an unrecognised name means
    /// Forge's table and its tool surface have drifted apart, and the safe
    /// reading of "I don't know what this does" is the most restrictive one.
    pub fn of_tool(name: &str) -> Self {
        let bare = name.rsplit("__").next().unwrap_or(name);
        match bare {
            "proc_start" | "proc_input" => Self::Execute,
            "proc_list" | "proc_status" | "proc_output" | "proc_wait" => Self::Read,
            "fs_read" | "fs_search" | "fs_list" => Self::Read,
            "fs_write" | "fs_edit" => Self::Write,
            "git_status" | "git_diff" | "git_log" => Self::Read,
            "proc_signal" | "git_checkout" | "git_reset" => Self::Destructive,
            _ => Self::Destructive,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "READ",
            Self::Write => "WRITE",
            Self::Execute => "EXECUTE",
            Self::Network => "NETWORK",
            Self::Destructive => "DESTRUCTIVE",
        }
    }

    /// Whether a call may be auto-approved without asking, given a grant set.
    ///
    /// `Destructive` is never auto-approved, however it was granted.
    pub fn auto_approvable(self, granted: &[Capability]) -> bool {
        self != Self::Destructive && granted.contains(&self)
    }
}

/// The answer to an approval request.
#[derive(Debug, Clone)]
pub enum Decision {
    Allow,
    /// Allow, but with arguments Forge rewrote — the gate can clamp a call
    /// rather than only accept or refuse it.
    AllowWithInput(serde_json::Value),
    Deny { message: String },
}
