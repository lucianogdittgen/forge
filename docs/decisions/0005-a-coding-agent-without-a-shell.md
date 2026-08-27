# ADR-0005 — A coding agent without a shell

- **Status:** Accepted
- **Date:** 2026-08-27
- **Supersedes:** ADR-0004

## Context

ADR-0004 gave the agent seven process tools and nothing else: `--tools ""`
removed every built-in, and a runtime assertion refused any tool not prefixed
`mcp__forge__`. The reasoning was sound — every action becomes a process, so
every action is visible by construction — and the consequence was recorded
honestly at the time: *"the agent cannot read or edit files. For a coding
workbench this is a real limitation."*

Using it made the size of that limitation clear. Forge was a workbench whose
agent could not do the work. Asking it to change a line meant asking it to run
`sed` through `proc_start`, which is a worse editor and no safer. The
limitation was not a deferred nicety; it was the product not working.

The second problem was quieter and mattered more. ADR-0004 assumed the token
cost of a build was solved by the architecture. It was not: `proc_output`
returned up to **1000 rendered lines** — roughly 40 KB, ~10k tokens — straight
into the model's context. Reading a build *was* the way context got burned. The
visibility property was real; the cost property was assumed.

The requirement, restated by the developer: a TUI that behaves like Claude Code,
where long-running command output goes **to your eyes, not the model's
context**.

## Decision

**The agent gets Claude's normal coding tools, minus every tool that can run a
command.** The surface is:

```
Read, Edit, Write, NotebookEdit, WebFetch, WebSearch     (built-in)
proc_start, proc_list, proc_status, proc_output,
proc_wait, proc_input, proc_signal                       (Forge, over MCP)
```

`--tools` takes a *subset* of the built-ins — verified against `claude`
2.1.246 — so this is expressible directly rather than by policy.

### The token property comes from removing `Bash`, not from removing everything

This is the whole mechanism, and it is worth stating plainly because it is the
one thing that must not be undone:

- `Bash` runs a command and **returns its output into the context**. Every byte
  a build prints is billed and re-sent on every subsequent turn.
- `proc_start` runs a command and **returns an id**. The bytes go to the PTY,
  the pane, and the developer's eyes. They enter the model's context only if it
  explicitly calls `proc_output`, and then only under a hard cap.

So the property is not "the agent has few tools". It is "the agent has no path
by which command output arrives unasked". `Task`, `Workflow`, and `Skill` are
excluded for the same reason — each can reach a shell indirectly.

`assert_argv_safe` refuses to start if any of `Bash`, `Task`, `Workflow`, or
`Skill` appears in the tools list, in the same shape as the existing
`--dangerously-skip-permissions` refusal. This invariant carries the product's
promise; it fails loudly at startup rather than degrading silently.

The runtime assertion inverts accordingly: ADR-0004 warned when a tool was
*not* `mcp__forge__*`; Forge now warns only when a **command-runner** appears in
the `system/init` surface. That is the case where the pane would stop showing
what the agent is doing.

### Reading output is capped, and says what it dropped

`proc_output` defaults to 40 rows, allows at most 200, and truncates at ~8000
characters. When it trims, it says so — `showing the last 40 of 3,812 lines` —
so the model narrows its request rather than blindly retrying. The tool
description states that reading output costs context and that the developer can
already see it. `proc_wait`'s exit code, duration, and byte count are the cheap
and normal way to learn how a build went.

### Edits are auto-approved inside the workspace, and are visible

Auto-approving edits removes the prompt that was the developer's only signal
that a file changed. Silent edits break the thesis for files exactly as hidden
output breaks it for processes. So two things move together:

- `Policy::decide` auto-approves `Write`-class calls **only** when the call's
  `file_path` resolves inside the workspace. Anything outside asks.
- `Edit`/`Write`/`NotebookEdit` render as a distinct conversation line carrying
  path and lines changed, not the generic tool-call fallback.

Path containment is the security-sensitive part and is done deliberately: `..`
is collapsed **lexically**, because the target of a `Write` need not exist yet
and cannot be `canonicalize`d; the workspace root and the target's nearest
*existing* ancestor are both canonicalized, so a symlink cannot smuggle a path
out of the tree; and anything that fails to resolve asks rather than allows.

Unknown tool → `Destructive` → never auto-approvable stays exactly as ADR-0004
left it. That fail-closed default is why a forgotten tool is safe.

## What we verified

Everything above rests on `--permission-prompt-tool stdio` firing for built-in
tools and not only MCP ones, so the gate was probed against the real CLI
(`the_gate_sees_built_in_tools`, `#[ignore]`d because it costs a live turn).

**`Read`, `Edit`, and `Write` all reach `canUseTool`.** The gate is a real
boundary for every capability class, not only for writes; Forge can deny a read.

This one is easy to get backwards, and was gotten backwards once during this
work. An earlier probe ran with `Read` in `granted`, so *Forge* auto-approved it
and emitted no request — which looks exactly like the CLI never asking. The
conclusion drawn from it, that reads bypass the gate, was wrong. The probe now
grants nothing, which is the only configuration in which the question can be
answered at all. Anyone re-running it should keep `granted` empty for the same
reason.

There is also **no `Grep` or `Glob`** in this build's 24 built-ins. Searching
the tree therefore goes through `proc_start` with `grep`/`rg`/`find` — which
means you see it happen. That is a coincidence of the CLI's surface rather than
a design choice, but it is a convenient one.

## Consequences

### Positive
- Forge is usable as a coding workbench: the agent reads, edits, and reasons
  about the tree.
- A forty-minute build costs the same in tokens as a four-second one.
- The visibility promise for processes is unchanged; the promise now extends to
  files, through edit lines rather than through absence of edits.

### Negative
- Built-in tools are the CLI's, not Forge's, so their behaviour can change
  under us. The startup assertion and the `system/init` check are the guard.
- Auto-approved in-workspace edits mean the developer is *informed* rather than
  *asked*. That was chosen deliberately (a workbench that prompts per edit is a
  workbench nobody uses), and it is why the edit line is not optional.
- Whether the gate fires is the CLI's behaviour, not Forge's, so it is asserted
  against the real binary rather than assumed.

### Risks and mitigations
- *A future CLI version adds a command-running tool we do not know to exclude.*
  Mitigated by the allow-list shape: Forge names the tools it wants; it does not
  subtract from a set it does not control.
- *The model reaches for `proc_output` to follow a build.* Mitigated by the cap,
  by the trim note, and by the system prompt saying the developer is already
  watching.

## Revisit when

The CLI stops routing a capability class through `canUseTool`, or grows a tool
that runs commands without returning their output — which would make it safe to
admit.
