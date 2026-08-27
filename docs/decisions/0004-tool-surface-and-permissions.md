# ADR-0004 — The agent's tool surface and the permission gate

- **Status:** Superseded by [ADR-0005](0005-a-coding-agent-without-a-shell.md)
- **Date:** 2026-08-27

## Context

ADR-0002 settled *how* Forge talks to Claude: the CLI over stream-JSON, with
`--tools ""` removing every built-in and an MCP endpoint supplying the whole
tool surface. It did not settle *what* that surface is, or where the security
requirement — "treat agent-generated commands as potentially dangerous;
distinguish READ, WRITE, EXECUTE, NETWORK, DESTRUCTIVE; do not blindly execute
arbitrary destructive commands" — is actually enforced.

Two forces pull against each other:

- The product promise is that the developer sees everything the agent runs.
  Any tool that produces work the terminal pane cannot render breaks it.
- A model that cannot get its job done will route around the tools it has. A
  surface that is too thin produces an agent that begs for a shell.

## Options considered

| Option | Pros | Cons |
|---|---|---|
| One `run_command` tool returning output | Trivial for the model | Reintroduces `execute() → wait → output`; a forty-minute build blocks the turn; the pane becomes decorative |
| A file/edit surface alongside the process tools | Agent can do more unaided | Edits are invisible in a terminal pane; needs a diff UI Forge does not have yet, and the promise silently stops holding |
| Process tools only, mirroring `ProcessManager` | Every action is a process, so every action is visible by construction | The agent cannot read a file without running a command — which is fine, because running a command is the thing the developer can see |

## Decision

**The agent's entire tool surface is seven process tools**, one per meaningful
`ProcessManager` operation, served from an in-process MCP endpoint in
`forge-mcp`:

| Tool | Capability |
|---|---|
| `proc_list`, `proc_status`, `proc_output`, `proc_wait` | `Read` |
| `proc_start` | `Execute` |
| `proc_input` | `Write` |
| `proc_signal` | `Destructive` |

There is no `run_and_return_output`. `proc_start` returns a process id
immediately; the outcome is obtained by `proc_wait`, which may time out without
that being an error.

Three properties are load-bearing:

1. **The agent gets a handle on the *same* `ProcessManager` that feeds the
   terminal pane** — not a private one. There is no code path by which it can
   start something the pane does not show.
2. **`proc_output` renders through the same VT emulator as the pane**
   (`forge_terminal::render_transcript`). The agent sees a build's progress bar
   collapsed to one line for the same reason the developer does. Feeding it raw
   bytes would give it thousands of CR-rewritten duplicates and a different
   picture of the build than the human has.
3. **The classification lives in `forge-agent`, keyed on the tool name, and an
   unknown tool is `Destructive`.** A new tool is therefore gated by default;
   forgetting to classify one fails closed. `Destructive` is never
   auto-approvable regardless of what is granted.

The MCP endpoint binds `127.0.0.1:0` with an unguessable token in the URL path.
The token is **not** the permission gate — it stops unrelated local processes
from finding the endpoint. The gate is the stdio `canUseTool` channel, which
the CLI cannot reach around.

Forge asserts on the `system/init` event that every tool the agent reports is
prefixed `mcp__forge__`, and says so loudly in the conversation if it is not.
That is the one invariant worth checking at runtime: a built-in appearing there
would mean the agent can act through a channel the pane does not render, and
the developer would stop being able to trust what they see.

## Consequences

### Positive
- Visibility is structural, not a matter of the agent's cooperation.
- The permission classes required by the brief map onto real, distinct
  operations rather than being an unused enum.
- Nothing in the tool layer knows what BitBake is.

### Negative
- The agent cannot read or edit files. For a coding workbench this is a real
  limitation, deferred deliberately: a file surface needs a diff UI before it
  can keep the visibility promise.
- Every read costs a process spawn.

### Risks and mitigations
- *The model treats `proc_start` as returning a result and stalls.* Mitigated in
  the system prompt, which states plainly that a forty-minute build is normal,
  and by `proc_start`'s own response text.
- *Approval fatigue leads to blanket allow.* `Read` is granted by default;
  `Destructive` cannot be granted in advance at all.

## Revisit when

The agent starts asking for a shell tool to work around the surface, or a
file-editing surface arrives with a diff view that preserves the visibility
property.
