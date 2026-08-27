# ADR-0003 — PTY handling, signals, and stream semantics

- **Status:** Accepted
- **Date:** 2026-08-27

## Context

The process manager is the heart of Forge and must behave exactly like a real
terminal. Several details here are easy to get subtly wrong in ways that only
show up under real workloads — `vim` ignoring Ctrl-C, grandchild build processes
surviving a stop, or a build silently slowing down because the UI throttles it.
Each was tested rather than assumed.

## Decision

### 1. One PTY, stdout and stderr merged

A PTY has one slave; `dup2(slave,1)` and `dup2(slave,2)` point at the same
device, so bytes are merged in write order **with no marker**. Verified:
`OUT1|ERR1|OUT2|ERR2` came back as a single indistinguishable stream.

Separating them is possible (stdout→PTY, stderr→pipe) but costs stderr its
tty-ness: `isatty()` becomes false, so programs switch to block buffering and
drop colour on stderr, and cross-stream ordering is lost.

**We merge.** It is what a real terminal does, it keeps colour and tty-detection
correct, and BitBake specifically is happiest on a single merged tty. A split
"diagnostics" mode may be offered later behind an explicit flag, documented as
making stderr a pipe rather than a tty.

### 2. Ctrl-C has two mechanisms, and using one for both is a bug

| Origin | Mechanism | Why |
|---|---|---|
| Human keystroke in the pane | write `0x03` to the master | Exactly what a real terminal does; the line discipline decides |
| Programmatic stop (UI button, agent tool) | `killpg(pgid, SIGINT)` | Works regardless of terminal mode |

This distinction is load-bearing and was verified. Writing `0x03` to a child that
put the tty in raw mode (`-isig`, as `vim`, `less`, and any full-screen TUI do)
**does nothing** — `sleep 5` kept running and `xxd` showed byte `03` arriving as
literal application data. `killpg` killed the same child, reaped with signal 2.

A "Stop" button implemented by writing `0x03` would therefore appear to work in
`bash` and silently fail in `vim`.

### 3. Always signal the process group; always `setsid()` the child

The child calls `setsid()` (becoming session and process-group leader, pgid==pid)
and acquires the slave as its controlling terminal. Signals go to the **group**,
never the bare pid, so that `bash → bitbake → gcc` grandchildren receive them.
Verified the child's pgid differs from Forge's, which also guarantees Forge can
never signal itself. Escalation is `SIGINT → SIGTERM → SIGKILL` with timeouts.

### 4. Resize

Set the initial winsize at spawn — an unset size reports `0 0` (observed).
On pane resize: update stored dims → `TIOCSWINSZ` on the master → resize the
emulator, so the model and the child agree. Verified `TIOCSWINSZ` immediately
delivers `SIGWINCH` to the foreground group and the child read back `stty size`
= `40 120`.

### 5. Backpressure

The PTY reader drains the master **as fast as the kernel delivers**, never
blocking on the UI — blocking the reader applies backpressure to the child and
changes the build's timing. Bytes go to the emulator, which collapses them into
screen state (this is where a CR progress bar stops being unbounded). The UI
redraws on a **fixed cadence** from current screen state, decoupled from arrival.

Reads are coalesced into one frame (~16 ms or 64 KiB) before parsing — the single
most important technique for high-volume output, taken from Toad. Refresh dirty
lines, not the pane.

### 6. Reattachment: hold the master fd

The common belief that a PTY child dies unless re-parented is **wrong**, and the
correction determines the persistence design. Verified: parent exits with nothing
holding the master → child **dead**. Identical test with a detached holder
process keeping the master open → child **alive**, `PPID=1`, still on `pts/7`.

When the last fd referring to the master closes, the kernel sends `SIGHUP` to the
terminal's foreground group. Re-parenting to `init` happens either way and is
irrelevant. This is exactly how tmux, screen and dtach work.

Corollaries: a process is reattachable only if a **Forge-owned process outlives
the TUI**; reattaching to a process from a previous, now-dead Forge is
impossible; and scrollback for a reattached process must come from Forge's own
on-disk log, because tmux and screen hold history in memory and lose it too.

v1 does **not** reattach live processes. The API is shaped so that splitting a
supervisor out later is a transport change, not a redesign.

## Consequences

`EXITED` (non-zero exit) stays distinct from `FAILED` (could not spawn), so the
agent treats a failing build as an informative result rather than a tooling
error. Scrollback is a bounded ring buffer whose drops are visible rather than
silent, and the agent reads a *window* of it so a long build cannot blow up its
context.

## Revisit when

A consumer genuinely needs split stdout/stderr, or persistence requirements grow
to demand the supervisor split.
