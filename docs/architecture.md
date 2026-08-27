# Forge — Architecture

> Status: **draft**. Technology selection (§6) is pending the research in
> `docs/research/` and the ADRs in `docs/decisions/`. Everything above §6 is
> language-neutral and is expected to survive the technology choice.

## 1. What Forge is

Forge is a terminal development workbench that puts an AI coding agent and a
**real terminal** side by side. Its defining constraint:

> **Claude can work for you without taking your terminal away from you.**

Concretely: when the agent runs `bitbake core-image-minimal`, the developer
watches the actual BitBake process, live, exactly as if they had typed it
themselves — including progress bars, colour, and cursor rewriting. The agent
does not stand between the developer and the process output.

### 1.1 Non-goals

- Forge is **not** a Claude Code reimplementation and does not scrape or imitate
  its terminal UI.
- Forge is **not** a Yocto tool. BitBake support must fall out of a generic
  process model. No BitBake parsing exists in the core.
- Forge does **not** replace the terminal. The AI and the terminal coexist.

## 2. Layering

Forge is layered so that each layer depends only on those below it. The rule
that matters most: **the Process Manager does not know the agent exists.**

```
┌─────────────────────────────────────────────────────────────┐
│  UI                     panes, layout, input routing        │
├─────────────────────────────────────────────────────────────┤
│  Application            session, event bus, orchestration   │
├───────────────────────────┬─────────────────────────────────┤
│  Agent                    │  Workspace                      │
│    ports: Agent, Tool     │    Files · Git · Config         │
├───────────────────────────┴─────────────────────────────────┤
│  Tools                  the agent's capability surface      │
├─────────────────────────────────────────────────────────────┤
│  Permissions            classify · decide · audit           │
├─────────────────────────────────────────────────────────────┤
│  Process Manager        lifecycle, registry, state machine  │
├─────────────────────────────────────────────────────────────┤
│  Terminal               VT emulation, screen, scrollback    │
├─────────────────────────────────────────────────────────────┤
│  PTY                    fd, spawn, signals, resize, reap    │
└─────────────────────────────────────────────────────────────┘
```

Dependency direction is strictly downward. The Agent layer reaches the Process
Manager **only** through Tools, and Tools go through Permissions. There is no
path by which the agent can spawn a process that the UI does not see.

### 2.1 Why the Process Manager is independent

If the process engine were owned by the agent, then process output would be
something the agent *reports*, and the developer would see a summary rather
than the truth. Making the Process Manager a peer of the agent — both are
clients of the same registry — means the terminal pane renders the authoritative
byte stream and the agent merely subscribes to it like anyone else.

```
      Agent ──requests──►  Tools ──►  Permissions ──►  Process Manager
                                                            │
                                          ┌─────────────────┴────────────────┐
                                          ▼                                  ▼
                                    Terminal pane                      Agent's view
                                   (authoritative,                 (sampled window,
                                    every byte)                     state + exit code)
```

## 3. The long-running command model

A blocking `execute() -> complete output` API is **rejected**. It cannot express
a build that runs for forty minutes, and it forces the agent to hold the UI
hostage while it waits.

Instead, starting a process is non-blocking and returns a handle immediately:

```
start_process(cmd, cwd, env)
        │
        └──► process_id                      (returns immediately)
                 │
                 ├── live output ─────────►  terminal pane   (continuous)
                 ├── state updates ───────►  UI              (on transition)
                 └── exit event ──────────►  agent           (once)
```

This lets all three participants proceed concurrently:

```
Claude:     "BitBake is still running. I'll wait for the build result."
Developer:  [keeps inspecting files, scrolls the terminal, types into it]
Process:    [BitBake keeps running, output keeps streaming]
```

The agent observes a running process through `poll` / `read_output` / `wait`,
and is notified once on exit. It never blocks the UI to do so.

## 4. Process state machine

```
                 ┌──────────┐
                 │ STARTING │
                 └────┬─────┘
             spawn ok │ spawn failed
                 ┌────▼─────┐            ┌────────┐
                 │ RUNNING  │───────────►│ FAILED │
                 └────┬─────┘            └────────┘
       terminate req. │        │ exits on its own
                 ┌────▼─────┐  │
                 │ STOPPING │  │
                 └────┬─────┘  │
        SIGKILL after │        │
        grace period  │        │
             ┌────────▼───┐ ┌──▼─────┐
             │ INTERRUPTED│ │ EXITED │
             └────────────┘ └────────┘
```

| State | Meaning |
|---|---|
| `STARTING` | PTY allocated, `fork`/`exec` in flight, no pid confirmed yet |
| `RUNNING` | Child is alive and reaped-on-exit is armed |
| `STOPPING` | Termination requested; grace period before escalation |
| `EXITED` | Child exited on its own; exit code captured |
| `FAILED` | Never started (exec failure, missing binary, permission denied) |
| `INTERRUPTED` | Terminated by Forge or by an operator signal |

`EXITED` with a non-zero code is **not** `FAILED`. `FAILED` means Forge could not
run the command at all. This distinction matters to the agent: a failing build
is a normal, informative result; a failed spawn is a Forge-level problem.

### 4.1 Per-process record

| Field | Notes |
|---|---|
| `id` | Forge-assigned, stable across the process lifetime |
| `pid`, `pgid` | OS identity; `pgid` is what signals are sent to |
| `command`, `argv` | As requested and as executed |
| `cwd`, `env` | Captured at spawn |
| `started_at`, `ended_at` | Monotonic + wall clock |
| `state` | Per §4 |
| `exit_code`, `signal` | Populated on termination |
| `pty` | Master fd, child slave name, controlling-terminal status |
| `size` | `(rows, cols)`, kept in sync with the pane |
| `output` | Bounded ring buffer, see §5.2 |

## 5. Terminal

### 5.1 It is an emulator, not a log widget

The terminal pane must run a VT emulator over the byte stream. A scrolling text
widget is not acceptable: BitBake and `ninja` rewrite a status line with
carriage returns, and a naive widget turns one progress line into thousands of
duplicate lines. The pane must correctly handle ANSI colour, cursor addressing,
carriage return, line rewriting, erase, the alternate screen, Unicode and
double-width characters, scrolling regions, and resize.

Acceptance is behavioural: `bash`, `htop`, `vim`, `less`, and `bitbake` must all
look and behave the way they do in the user's own terminal.

### 5.2 Backpressure

A kernel build emits output far faster than any UI can render it. The design
must be explicit about this or Forge will stall or exhaust memory.

- The PTY reader drains the master fd **as fast as the kernel delivers it** and
  never blocks on the UI. Blocking the reader applies backpressure to the child,
  which changes the build's timing and can deadlock it.
- Bytes are fed to the emulator, which collapses them into screen state. This is
  where a carriage-return progress bar stops being unbounded.
- The UI redraws on a **fixed cadence**, rendering current screen state rather
  than every intermediate frame. Rendering is decoupled from arrival.
- Scrollback is a **bounded** ring buffer. Oldest lines are dropped, and the
  drop is visible to the user rather than silent.

The agent reads a *window* of scrollback, never the whole stream, so that a
long build cannot blow up the agent's context.

## 6. Technology selection

**Pending.** To be filled from `docs/research/` and fixed in ADRs:

- ADR-0001 — Implementation language and TUI framework
- ADR-0002 — Claude integration architecture
- ADR-0003 — PTY and terminal-emulation stack
- ADR-0004 — Permission model
- ADR-0005 — Persistence

## 7. Ports (language-neutral)

These are the interfaces the rest of Forge is written against. Claude-specific
detail must not leak past `Agent`.

```
Agent
  send(message)                     -> stream of AgentEvent
  interrupt()
  conversation()                    -> Conversation

AgentEvent  = TextDelta | ToolCallStarted | ToolCallFinished
            | ApprovalRequested | TurnFinished | Error

Tool
  name, description, schema
  permission: Permission
  invoke(ToolCall)                  -> ToolResult

Permission  = READ | WRITE | EXECUTE | NETWORK | DESTRUCTIVE

ProcessManager
  start(command, cwd, env, size)    -> ProcessId        # non-blocking
  get(ProcessId)                    -> ProcessRecord
  list()                            -> [ProcessRecord]
  write_stdin(ProcessId, bytes)
  resize(ProcessId, rows, cols)
  signal(ProcessId, sig)
  terminate(ProcessId, grace)
  subscribe(ProcessId)              -> stream of ProcessEvent

ProcessEvent = Output(bytes) | StateChanged(state) | Exited(code, signal)

Workspace
  root, git: GitInfo | None
  detect()                          -> WorkspaceInfo
  files()                           -> FileTree

Conversation
  messages, id, created_at
  append(message), persist(), restore(id)
```

Note the asymmetry that makes the whole design work: `ProcessManager.start`
returns an **id**, not output. Output is obtained by *subscribing*, and both the
terminal pane and the agent subscribe independently.

## 8. Security posture

Agent-proposed commands are treated as untrusted input. Every tool declares a
permission class, and `EXECUTE`/`DESTRUCTIVE` operations pass through an explicit
decision point before reaching the Process Manager. The user always sees what
the agent is doing — visibility is a security property here, not just a UX one.
Detail is deferred to ADR-0004.

## 9. Development-time note

Forge is developed with the help of an external orchestration tool. **That tool
is not part of Forge.** Forge has no dependency on it, does not import it, does
not require it to build, install, or run, and the shipped product contains no
trace of it. Anyone cloning this repository on a clean machine can build and use
Forge without knowing it existed.
