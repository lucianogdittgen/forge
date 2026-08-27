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

**Early.** The process and terminal engine work and are tested. The agent
integration is in progress. Not yet usable as a daily driver.

| Component | State |
|---|---|
| PTY process engine | working, 11 tests |
| Terminal emulator | working, 9 tests |
| Terminal pane + key handling | working, 15 tests |
| Two-pane TUI shell | working |
| Claude agent integration | in progress |
| Files / Git / permissions / persistence | not started |

## Try it

```bash
cargo run --release              # runs your $SHELL in the terminal pane
cargo run --release -- htop      # or any command
```

- `Tab` / `Enter` — focus the terminal pane
- **Tap `Esc` twice** — leave the terminal pane
- `q` — quit (only when the terminal pane is not focused)

While the terminal pane has focus, **every** key goes to the child process,
including `Ctrl-C`. That is deliberate: an interrupt must reach your build, not
your editor. It also means no key is left over to escape with, which is why
leaving is a double-tap gesture rather than a keystroke.

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
└── forge            the application
```

## Requirements

Linux, Rust 1.85+.

## Licence

MIT
