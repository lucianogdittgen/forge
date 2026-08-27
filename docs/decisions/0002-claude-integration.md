# ADR-0002 — Claude integration architecture

- **Status:** Accepted
- **Date:** 2026-08-27

## Context

The brief asks for "the cleanest supported architecture for integrating Claude
without simply wrapping the `claude` executable unless there is a compelling
technical reason to do so", and for the rest of Forge not to depend on
Claude-specific detail.

The investigation dissolved the premise of that question. **The Claude Agent SDK
is itself a subprocess wrapper around the `claude` binary** — verified in its own
source: `_internal/transport/subprocess_cli.py` resolves a `claude` executable,
sets `CLAUDE_CODE_ENTRYPOINT=sdk-py`, and speaks newline-delimited JSON over
stdin/stdout. The Python wheel bundles that binary: a **250,162,696-byte** Node
ELF, one per platform.

So there is no wrap/don't-wrap choice. Every option wraps it. The only real
question is whether a typed Python adapter sits in front of the pipe — and,
given ADR-0001 selected Rust, whether we add a Python runtime purely to obtain
that adapter.

## Decision

**Forge drives the `claude` CLI directly over its stream-JSON stdio protocol,
from Rust, with no Python in the stack.** Forge serves its own tools from an
in-process MCP endpoint and owns 100% of the tool surface.

This was gated on a verification spike, because the alternative — assuming the
CLI could be locked down as tightly as the SDK — would have been discovered as
false only after the driver was written. All three questions came back proven.

### Q1 — Strip every built-in tool: **PROVEN**

`--tools ""` removes all 24 built-ins (`Bash`, `Read`, `Write`, `Edit`,
`WebSearch`, `Task`, …). MCP tools are a separate surface and survive it. With
`--tools "" --mcp-config <forge>` the `system/init` event listed exactly
`["mcp__forge__forge_proc_start"]` — 0% Anthropic tool surface, 100% Forge.

This matters beyond tidiness: a built-in `Bash` would let the agent run a build
through a channel Forge does not own, and the terminal pane would never see it.
Removing it is what makes the visibility guarantee structural.

### Q2 — Serve Forge's own tools: **PROVEN**

`--mcp-config` accepts a file or inline JSON, over stdio or streamable HTTP.
Wire names are `mcp__forge__<tool>`. `--strict-mcp-config` refuses the user's own
MCP servers. Forge will serve tools over HTTP from the Rust binary itself.

### Q3 — An unshadowable gate, async, routed to the TUI: **PROVEN, conditionally**

Two surfaces exist and they are **not** equivalent.

`--permission-prompt-tool stdio` gives us the SDK's `canUseTool` from the plain
CLI: the process emits a `control_request` on stdout and blocks for a
`control_response` on stdin. No MCP server needed, and the decision can be taken
as slowly as a human needs — verified blocking for **123 s** with no timeout.
`updatedInput` is honoured, so Forge can *rewrite or clamp* a tool's arguments at
the gate rather than only allow/deny.

But on its own that surface **is shadowable**, and the spike proved it by trying:

| Attempt (gate hard-wired to DENY) | Gate consulted? | Outcome |
|---|---|---|
| baseline | yes | DENIED |
| `--allowed-tools <tool>` | **no** | **BYPASSED** |
| `--settings` file with an allow-rule | **no** | **BYPASSED** |
| `--permission-mode bypassPermissions` | **no** | **BYPASSED** |
| project `.claude/settings.json` allow-rule | **no** | **BYPASSED** |
| **user `~/.claude/settings.json`** allow-rule | **no** | **BYPASSED** |
| user settings allow-rule **+ `--setting-sources ""`** | yes | DENIED |
| `PreToolUse` hook + `--allowed-tools` | yes | DENIED |
| `PreToolUse` hook + `bypassPermissions` | yes | DENIED |
| hook `ask` + `--allowed-tools` + `bypassPermissions` | yes | DENIED |

The fifth row is the one that mattered. **A user's personal Claude Code settings
could otherwise punch a hole straight through Forge's permission model** — a
developer who once allowed a tool for their own convenience would silently
disarm Forge. `--setting-sources ""` closes it, proven at both project and user
scope.

The last four rows give the backstop: **a `PreToolUse` hook cannot be shadowed by
anything.** A hook returning `"ask"` forces the prompt through even with
`--allowed-tools` and `bypassPermissions` both set.

## Consequences

### The flag set is load-bearing, not incidental

Forge must pass `--tools ""`, `--strict-mcp-config`, `--disable-slash-commands`,
`--setting-sources ""`, `--permission-prompt-tool stdio`, and a `PreToolUse`
`ask` hook. It must **never** pass `--allowed-tools`, `--permission-mode
bypassPermissions`, or `--dangerously-skip-permissions`.

Because the security property lives in argv rather than in Forge's own types,
the obvious regression is a future maintainer adding `--allowed-tools` for
convenience. **Forge asserts on its own argv before spawn and refuses to start**
if a forbidden flag is present, so the failure is loud instead of silent.

### Positive

- No Python runtime, no 250 MB bundled ELF inside Forge's own distribution.
- `CLAUDE_CONFIG_DIR` points at a Forge-owned directory, so the user's
  `~/.claude` is never read *or written*.
- `--bare` guarantees OAuth and keychain are never consulted, and skips
  CLAUDE.md discovery, LSP, and plugin sync — none of which Forge wants.
- Session resume (`--session-id`, `--resume`, `--fork-session`) comes free, and
  `--fork-session` is the natural primitive for "explore an alternative here".

### Negative and risks

| Risk | Mitigation |
|---|---|
| **Enterprise policy settings.** `--safe-mode` help says admin-managed settings still apply. No policy file exists on this machine, so whether a managed allow-rule can override `--setting-sources ""` is **UNVERIFIED**. On a corporate machine this is a real hole. | Detect `/etc/claude-code/` at startup and warn loudly. The `ask` hook is the mitigation if it proves exploitable. |
| The hook spawns a subprocess per tool call (~10 ms). | Treat the hook as a *policy* channel that always answers `ask`; keep the real decision on the stdio control channel, which is already in-process. |
| `system/init` repeats every turn. | Do not treat it as a one-time handshake; it will mis-frame turn boundaries. |
| Non-JSON lines appear on stdout (observed with `--resume <unknown>`). | The line reader tolerates garbage rather than failing. |
| Unknown event types (`rate_limit_event`, `compact_boundary`, …). | Skip-and-log, never fatal. |
| Still a subprocess dependency on a Node binary Forge does not build. | Unavoidable in every option; at least Forge does not also ship a second copy of it. |

### Licensing

Forge **cannot** offer "log in with your Claude subscription" without prior
Anthropic approval; distributed Forge must use API keys. Branding rules also
forbid presenting Forge as Claude Code.

## Revisit when

A first-party Rust SDK appears, the enterprise-policy hole is proven
exploitable, or Forge needs multiple model providers — at which point the
`Agent` port (ADR-0001 §7) is what makes the swap contained.
