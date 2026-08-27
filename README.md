# Forge

An AI development workbench for the terminal.

> **Claude can work for you without taking your terminal away from you.**

Forge puts an AI coding agent and a **real terminal** side by side. When the
agent runs `bitbake core-image-minimal`, you watch the actual BitBake process —
live, with its colours, its progress bars, its cursor rewriting — exactly as if
you had typed it yourself. The agent doesn't stand between you and your build.

```
┌──────────────────────────────┬──────────────────────────────────────┐
│ AI                           │ TERMINAL — bitbake [running]         │
│                              │                                      │
│ you                          │ $ bitbake core-image-minimal         │
│ investigate this failure     │                                      │
│                              │ Loading cache: 100%                  │
│ claude                       │ Parsing recipes: 100%                │
│ I'll inspect the build log.  │ NOTE: Executing Tasks                │
│                              │                                      │
│ ✓ inspected recipe           │ [23%] do_compile                     │
│ ✓ inspected git history      │   CC drivers/foo/bar.o               │
│ ▶ running bitbake            │   LD vmlinux                         │
└──────────────────────────────┴──────────────────────────────────────┘
```

## Status

**Early, but the loop closes.** You can type a request on the left, watch Claude
read and edit your tree, start a build, and see that build run live on the
right.

| Component | State |
|---|---|
| PTY process engine | working, 12 tests |
| Terminal emulator | working, 12 tests |
| Terminal pane + key handling | working, 17 tests |
| Two-pane TUI shell | working, 21 tests |
| Claude agent integration | working, 32 tests |
| Tool server + permission gate | working, 14 tests |
| Git integration / persistence | not started |

Requires the `claude` CLI on your `PATH`. Without it Forge still runs — it is a
terminal with a note explaining why nobody is home on the left.

## Try it

```bash
cargo run --release              # runs your $SHELL in the terminal pane
cargo run --release -- htop      # or any command
```

Then type on the left and press `Enter`.

| Key | |
|---|---|
| `Tab` | focus the terminal pane |
| **Tap `Esc` twice** | leave the terminal pane |
| `Enter` | send what you typed to the agent |
| `y` / `n` | answer a pending approval |
| `Ctrl-C` | cancel the turn in flight, or quit when there is none |
| `Ctrl-]` | point the terminal at another process |
| `PgUp` / `PgDn` | scroll the conversation |

While the terminal pane has focus, **every** key goes to the child process,
including `Ctrl-C`. That is deliberate: an interrupt must reach your build, not
your editor. It also means no key is left over to escape with, which is why
leaving is a double-tap gesture rather than a keystroke.

When the agent starts something, the pane switches to it on its own — you don't
opt in to seeing your own build. If you have deliberately switched away with
`Ctrl-]`, it stays where you put it.

## What makes it different

Most AI coding tools run your commands and hand you a summary. Forge runs them
in a real PTY and shows you the bytes.

The design property that makes this structural rather than a matter of
discipline:

```
ProcessManager.start()  ->  process_id       (not output)
                             │
        ┌────────────────────┼────────────────────┐
        ▼                    ▼                    ▼
  terminal pane          the agent            the UI
  (every byte)      (a sampled window)    (state changes)
```

`start()` returns a **handle**, and output is obtained by *subscribing*. The
terminal pane and the agent subscribe independently, and the agent has no
privileged access to the registry. There is no code path by which it can run
something you cannot see.

The agent reads and edits your tree like any coding agent — `Read`, `Edit`,
`Write`, `NotebookEdit`, `WebFetch`, `WebSearch` — and on top of that gets seven
process operations over MCP: `proc_start`, `proc_list`, `proc_status`,
`proc_output`, `proc_wait`, `proc_input`, `proc_signal`.

What it does **not** get is a shell. `Bash` runs a command and returns its output
into the model's context, so a forty-minute build is billed by the line and
re-sent every turn. `proc_start` runs a command and returns an *id*: the bytes go
to the PTY and to your eyes. That single subtraction is what makes a long build
cost the same as a short one. Forge refuses to start if `Bash`, `Task`,
`Workflow`, or `Skill` is in the tool list, and says so loudly if one shows up at
runtime.

Output enters the model's context only when it asks, and then under a hard cap —
40 lines by default, 200 at most — and the reply says what it trimmed, so it
narrows its question instead of re-reading the build. Usually it doesn't need to:
`proc_wait` gives it the exit code for free.

Each tool carries a capability (`READ`/`WRITE`/`EXECUTE`/`NETWORK`/
`DESTRUCTIVE`). Reads are silent; edits inside the workspace go through and show
up in the conversation as a line naming the file and the lines changed; edits
*outside* it ask; starting a process asks; signalling one can never be
pre-approved. An unclassified tool counts as destructive, so forgetting to
classify one fails closed.

Even the agent's own view of output goes through the same VT emulator as your
pane, so a progress bar that rewrites itself ten thousand times is one line to
both of you.

Long-running commands are never `execute() → wait → return output`. The agent
starts a process, gets an id back immediately, and keeps reasoning while it
runs — so it can say "the build is still going, I'll check the log meanwhile"
instead of freezing until a forty-minute build finishes.

## Design notes

Three details that are easy to get wrong, and are enforced by tests:

- **Ctrl-C needs two different mechanisms.** A human keystroke writes `0x03` and
  lets the line discipline decide. A programmatic stop uses `killpg(SIGINT)`.
  These are not interchangeable: a child in raw mode — `vim`, `less`, any
  full-screen program — receives `0x03` as ordinary data and ignores it. A Stop
  button built on `0x03` works in `bash` and silently fails everywhere else.
- **Signals go to the process group**, never the bare pid, because a real build
  is `sh` → `bitbake` → compilers and the pid is only the shell.
- **A non-zero exit is not a failure.** `EXITED` (the build failed) and `FAILED`
  (Forge could not run the command) are different states, so the agent treats a
  failing build as something to reason about rather than a broken tool.

Forge is not a Yocto tool. BitBake is just a process; the core contains no
BitBake-specific parsing.

## Architecture

See [`docs/architecture.md`](docs/architecture.md), the decision records in
[`docs/decisions/`](docs/decisions/), and the investigation that produced them in
[`docs/research/`](docs/research/).

```
crates/
├── forge-process    PTY lifecycle, signals, state machine, output retention
├── forge-terminal   VT emulation behind a swappable trait
├── forge-tui        panes, key encoding, focus
├── forge-agent      the Agent port, and the Claude CLI behind it
├── forge-mcp        the tool server the agent talks to
└── forge            the application
```

## Requirements

Linux, Rust 1.85+.

## Licence

MIT
