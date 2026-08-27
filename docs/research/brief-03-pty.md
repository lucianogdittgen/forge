# Research Brief 03 — PTY, process management and terminal emulation for Forge

You are a research agent. Investigate ONLY. Do not build the app.

## Context
Forge's process manager is the heart of the product and must be INDEPENDENT of
the AI agent. It starts real processes on a PTY, streams their output live to a
terminal pane, and lets both the human and the agent interact with them.

Per-process state to maintain: pid, command, cwd, env, start time, state, exit
code, output stream, PTY state, terminal dimensions.
States: STARTING, RUNNING, STOPPING, EXITED, FAILED, INTERRUPTED.
Must support: Ctrl-C, Ctrl-D, resize (SIGWINCH), interactive stdin, stdout and
stderr streaming, termination, cleanup, exit status.
Explicitly NOT in scope for the core: any BitBake-specific parsing. BitBake is
just a process.

## Questions to answer (evidence + versions + citations)
1. Python PTY stack — evaluate and compare:
   - stdlib `pty`, `os.openpty`, `fcntl`/`termios`/`struct` for TIOCSWINSZ
   - `ptyprocess`, `pexpect`, `pyte`, `pyxtermjs`-style approaches
   - asyncio integration: how do you read a PTY master fd without blocking the
     event loop? `loop.add_reader`? A thread? Show the real, correct pattern.
   - How do you deliver Ctrl-C correctly? Writing 0x03 to the PTY vs
     `killpg(SIGINT)`. Explain process groups, sessions, and controlling
     terminals. Which is right and why. This detail matters a lot — get it right.
   - Reaping children, avoiding zombies, cleanup on crash, orphan handling.
2. Terminal emulation in Python:
   - `pyte` — what VT features does it actually implement? Alternate screen?
     Scrollback? Wide/Unicode chars? Performance under heavy output? Find real
     numbers or benchmark it yourself.
   - Alternatives to pyte. Anything faster or more complete.
3. Rust PTY/terminal stack — evaluate:
   - `portable-pty` (wezterm), `pty-process`, `rustix-openpty`
   - `vt100`, `wezterm-term`, `alacritty_terminal`, `termwiz` as emulators
   - `tui-term` as the Ratatui widget
   - Maturity, VT completeness, performance, last commit.
4. Cross-cutting hard problems — give a concrete answer for EACH:
   - stdout vs stderr on a PTY: they are merged by definition. If we need them
     separated we cannot use a single PTY. What are the options and what is the
     cost? Recommend one.
   - Resize propagation to the child and redraw correctness.
   - Backpressure: a build emits output faster than the UI can render. How do we
     avoid unbounded memory and UI stalls? Ring buffer? Coalescing? Give a design.
   - Scrollback: how much to keep, and how to let the agent read a window of it.
   - Detach/reattach and persistence of terminal state.
5. Benchmark reality-check: what is the throughput of the Python approach vs the
   Rust approach when a process dumps, say, 200MB of build log? Test it if you can.

## Method
- WebSearch/WebFetch docs, GitHub, and real source. Cite URLs and versions.
- ACTUALLY TEST. Write scratch programs in /tmp. Spawn `bash`, `top`, `vim`,
  `yes | head -c 100M`, and a carriage-return progress loop. Measure. Report real
  observed behaviour and numbers.
  Environment: Linux, Python 3.14.7, rustc 1.98.0.
- Mark unverified claims UNVERIFIED. Do not invent API names.

## Deliverable
Write `/home/luciano/forge/docs/research/03-pty.md`:
- Comparison tables (Python stack, Rust stack).
- A RECOMMENDATION for the PTY layer and the emulator layer.
- Concrete, correct answers to the Ctrl-C / process-group question and the
  stdout-vs-stderr question.
- A proposed ProcessManager interface (language-agnostic pseudo-signatures).
- A backpressure design.
- Measured numbers where you have them.
Concise and technical. No filler. Then stop.
