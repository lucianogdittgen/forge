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
            // Forge's own process tools.
            "proc_start" | "proc_input" => Self::Execute,
            "proc_list" | "proc_status" | "proc_output" | "proc_wait" => Self::Read,
            "proc_signal" => Self::Destructive,

            // The built-ins Forge hands the agent (`DEFAULT_TOOLS`).
            "Read" => Self::Read,
            "Edit" | "Write" | "NotebookEdit" => Self::Write,
            "WebFetch" | "WebSearch" => Self::Network,

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
    Deny {
        message: String,
    },
}

/// What the gate should do with one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Goes through without interrupting the developer.
    Allow,
    /// Stop and ask.
    Ask,
}

/// The rule the gate applies, given where the developer is working.
///
/// Split out as a plain value with a pure [`decide`](Policy::decide) so the
/// policy can be tested exhaustively without spawning an agent. The gate in
/// `claude.rs` does nothing but call it.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Edits at or below this path go through; anything else asks.
    pub workspace: std::path::PathBuf,
    pub granted: Vec<Capability>,
}

impl Policy {
    pub fn new(workspace: impl Into<std::path::PathBuf>, granted: Vec<Capability>) -> Self {
        Self {
            workspace: workspace.into(),
            granted,
        }
    }

    pub fn decide(&self, tool: &str, input: &serde_json::Value) -> Verdict {
        let cap = Capability::of_tool(tool);

        // Editing inside the tree the developer is working in is the normal
        // case, and prompting for each one would make a refactor unusable.
        // Outside it, an edit is a surprise and must be asked about — a stray
        // path is exactly the mistake worth catching.
        if cap == Capability::Write {
            return match input.get("file_path").and_then(|v| v.as_str()) {
                Some(p) if self.contains(p) => Verdict::Allow,
                // No path at all means Forge cannot tell what would be touched.
                _ => Verdict::Ask,
            };
        }

        if cap.auto_approvable(&self.granted) {
            Verdict::Allow
        } else {
            Verdict::Ask
        }
    }

    /// Whether `path` lands inside the workspace.
    ///
    /// Deliberately conservative: anything that cannot be resolved confidently
    /// returns `false`, which asks rather than allows.
    ///
    /// `canonicalize` cannot be used on the target itself — a `Write` names a
    /// file that does not exist yet — so `..` is removed lexically and the
    /// nearest *existing* ancestor is canonicalised. That closes the symlink
    /// route: a path under a symlinked directory resolves to where the symlink
    /// actually points before it is compared.
    fn contains(&self, path: &str) -> bool {
        if path.is_empty() {
            return false;
        }
        let Ok(root) = self.workspace.canonicalize() else {
            return false;
        };
        let joined = {
            let p = std::path::Path::new(path);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            }
        };
        let Some(resolved) = resolve_existing_prefix(&joined) else {
            return false;
        };
        resolved.starts_with(&root)
    }
}

/// Canonicalise the longest existing prefix of `path`, then re-apply the rest
/// lexically.
///
/// Returns `None` if the path escapes upward past what can be resolved, so a
/// caller that treats `None` as "ask" cannot be walked out of the tree.
fn resolve_existing_prefix(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::path::Component;

    // Strip `.` and collapse `..` lexically first: `..` cannot be resolved
    // against a component that does not exist yet.
    let mut lexical = std::path::PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Refuse to climb above the root of the path we were given.
                if !lexical.pop() {
                    return None;
                }
            }
            other => lexical.push(other.as_os_str()),
        }
    }

    // Now canonicalise as much of it as exists, so symlinks anywhere in the
    // existing prefix are followed before the comparison.
    let mut existing = lexical.as_path();
    let mut tail = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return Some(out);
        }
        let parent = existing.parent()?;
        tail.push(existing.file_name()?.to_os_string());
        existing = parent;
    }
}
