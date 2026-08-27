# 03 — PTY, Process Management & Terminal Emulation for Forge

Research only. All numbers below were measured on this machine unless marked
UNVERIFIED.

**Test environment:** Linux 7.1.9 x86_64, 12 cores, 31 GiB RAM, Python 3.14.7,
rustc 1.98.0. Library versions: pyte 0.8.2, ptyprocess 0.7.0, pexpect 4.9.0,
wcwidth 0.8.2 (Python); portable-pty 0.8.1 (latest 0.9.0), vt100 0.15.x (latest
0.16.2) (Rust). Scratch programs in `/tmp/pty_*.py`, `/tmp/forge-rust-pty/`.

---

## TL;DR / Recommendation

- **PTY layer:** either language reads a PTY master at ~70–90 MB/s — the PTY is
  kernel-bound, not language-bound. Python's stdlib `os`/`pty`/`termios`/`fcntl`
  is *fully sufficient* for the PTY layer. **Do not** use `pexpect` for Forge's
  core (it is an expect/automation layer, blocking-oriented). Use `os.openpty` +
  manual `fork`/`exec` (or `ptyprocess` for the spawn plumbing) with
  `loop.add_reader` for async.
- **Emulator layer:** this is where language choice is decisive. **pyte is
  0.8 MB/s** and does **not** implement a true alternate screen. **Rust `vt100`
  is 34.8 MB/s** (~43× faster) and correct. If Forge's UI is Rust/Ratatui, use
  `vt100` (or `alacritty_terminal` for max fidelity) + `tui-term`. If the UI is
  Python, **do not feed the full stream through pyte** — see the backpressure
  design; parse a bounded tail only, or push emulation to a Rust sidecar.
- **Ctrl-C:** forward a *human keystroke* by writing `0x03` to the master (lets
  the line discipline decide). Interrupt *programmatically* (Stop button / agent)
  with `killpg(pgid, SIGINT)`. Both require the child to be its own session
  leader (`setsid`). Details below — this distinction is load-bearing.
- **stdout vs stderr:** a single PTY merges them irrecoverably. Keeping them
  separate costs you stderr's tty-ness (isatty→false, colors/buffering change,
  interleave ordering lost). **Recommend: one PTY, merged**, matching real
  terminal semantics. Offer a separate opt-in "diagnostics pipe" mode only if a
  consumer truly needs split streams.

---

## 1. Python PTY stack

### Comparison

| Option | Role | Async-friendly | Verdict for Forge core |
|---|---|---|---|
| `os.openpty` + `fork`/`execvp` + `termios`/`fcntl` | Raw primitives; full control of ctty, winsize, session | Yes (`add_reader` on master) | **Recommended base.** Everything Forge needs, no magic. |
| `pty.fork()` / `pty.spawn()` | Convenience: fork + setsid + dup2 + ctty in one call | `pty.spawn` no; `pty.fork` yes | `pty.fork` good for spawning; `pty.spawn` copies its own loop — unusable. |
| `ptyprocess` 0.7.0 (2021) | Clean class over the above (spawn, setwinsize, sendcontrol, terminate) | Read is blocking; wrap fd yourself | Fine as spawn helper; you still drive I/O. |
| `pexpect` 4.9.0 (2023) | Expect/automation on top of ptyprocess | Blocking `expect`/`read`; `asyncio` shim exists but is expect-shaped | **Avoid for core.** Built for scripting interactions, not live streaming. |
| `pyte` 0.8.2 (2023) | Terminal *emulator* (screen model), not a PTY | N/A (pure parser) | See §2 — too slow for the live path. |
| pyxtermjs-style | Flask-SocketIO + `pty.fork` to a browser xterm.js | thread/greenlet per pty | Architecture reference only; xterm.js is the emulator, server just shovels bytes. |

### Async: reading the master without blocking the loop

The correct pattern is `loop.add_reader(master_fd, cb)` with the fd set
non-blocking. Verified working (`/tmp/pty_asyncio.py`): captured 3 lines and exit
code 7.

```python
fl = fcntl.fcntl(master, fcntl.F_GETFL)
fcntl.fcntl(master, fcntl.F_SETFL, fl | os.O_NONBLOCK)
loop.add_reader(master, on_readable)   # cb runs when epoll says readable

def on_readable():
    try:
        data = os.read(master, 65536)
    except BlockingIOError:
        return
    except OSError:          # Linux: EIO on master once the slave side closes
        loop.remove_reader(master); mark_eof(); return
    if not data:            # rare EOF path
        loop.remove_reader(master); mark_eof(); return
    handle(data)
```

Key gotchas, all observed:
- **EOF on Linux is `OSError(EIO)`, not `b""`.** You must catch EIO and treat it
  as end-of-stream, then `waitpid` for the exit code.
- Do **not** use asyncio's `connect_read_pipe`/`StreamReader` on a PTY master —
  those assume pipe EOF semantics and mishandle the EIO. `add_reader` on the raw
  fd is the robust choice.
- A background reader **thread** doing blocking `os.read` into an
  `asyncio.Queue` (via `call_soon_threadsafe`) is the equally-valid alternative
  and can be simpler for coupling to a synchronous emulator; it costs one thread
  per process. For dozens of processes, `add_reader` scales better.

### Ctrl-C — process groups, sessions, controlling terminals

Setup (what `pty.fork()` / a correct manual spawn must do): the child calls
`setsid()` → becomes **session leader** and **process-group leader** of a new
group (pgid == pid), with **no** controlling terminal; then opening the slave
(or `ioctl(TIOCSCTTY)`) makes the slave its **controlling terminal**. The
terminal tracks a **foreground process group**; the line discipline sends
signals there.

Two ways to deliver an interrupt — both measured (`/tmp/pty_ctrlc.py`,
`/tmp/pty_rawmode.py`):

1. **Write `0x03` to the master.** The *line discipline* interprets it. In
   canonical/`ISIG` mode it echoes `^C` and raises `SIGINT` on the terminal's
   foreground group. Observed: bash's `INT` trap fired.
   **But** if the child put the tty in raw mode (`-isig`, as `vim`/`less`/a TUI
   do), `0x03` is delivered as a **literal data byte** and interrupts nothing.
   Observed: `sleep 5` kept running; `xxd` showed byte `03` reaching the app.
2. **`os.killpg(pgid, SIGINT)`.** Sends the signal directly to the process group,
   **independent of terminal mode**. Observed: killed the raw-mode child that
   `0x03` could not touch (reaped with signal 2).

**Rule for Forge:**
- The terminal pane forwards a user's actual Ctrl-C keystroke → **write `0x03`**
  (plus the other control chars). This is exactly what a real terminal does and
  is correct whether the app is cooked or raw.
- A programmatic "interrupt/stop" (UI Stop button, agent-issued) → **`killpg(
  os.getpgid(pid), SIGINT)`**, because you cannot assume the child is in ISIG
  mode. Escalate `SIGINT → SIGTERM → SIGKILL` with timeouts.
- **Always signal the group, never the bare pid**, so grandchildren (bash → the
  real build tool) also receive it. This requires the child to be its own
  session/group leader (setsid), which also guarantees you never signal Forge
  itself. Verified the child's pgid differs from ours.

### Reaping, zombies, orphans, cleanup on crash

- After EIO/EOF, `os.waitpid(pid, 0)` to collect status; decode with
  `os.WIFEXITED/WEXITSTATUS/WIFSIGNALED/WTERMSIG`. Verified exit code 7 and
  signal-2 termination read back correctly.
- With many children, install a `SIGCHLD` handler or poll `waitpid(-1, WNOHANG)`
  so a process that dies *before* its master hits EIO is still reaped promptly.
  asyncio: `loop.add_signal_handler(SIGCHLD, reap_all)`.
- **Crash/cleanup:** on Forge shutdown, iterate live processes and
  `killpg(pgid, SIGTERM)` then `SIGKILL`. Because each child is a session leader
  in its own group, a Forge crash leaves orphans reparented to PID 1 — they do
  **not** die automatically. Options: (a) a supervisor/pidfile that sweeps on
  restart; (b) put children in a cgroup and kill the cgroup; (c)
  `PR_SET_PDEATHSIG` (Linux `prctl`) so the child dies with Forge — note
  PDEATHSIG is keyed to the parent *thread*, so set it in the child right after
  fork and re-check parent pid.

---

## 2. Terminal emulation in Python (pyte)

**Measured throughput (`/tmp/pty_pyte_bench.py`):** `pyte.Stream.feed` processed
**53.9 MB in 63.8 s = 0.84 MB/s**. Extrapolated, a 200 MB build log would take
**~4 minutes of pure CPU** just to model the screen. This is the single most
important Python finding.

Feature coverage, tested directly (`/tmp/*` pyte snippets):

| Feature | pyte 0.8.2 | Observed |
|---|---|---|
| Core VT100/VT220, SGR colors, cursor, CR/LF | Yes | Works |
| Scrollback | `HistoryScreen` (configurable `history=`, `ratio=`) | Works; deque-backed |
| Wide / Unicode (CJK) | Yes, via `wcwidth` | `你` occupies 2 cells, 2nd cell empty — correct |
| **Alternate screen (`?1049h/l`, smcup/rmcup)** | **Broken/no-op** | Entering did not clear; leaving did **not** restore primary buffer. `vim`/`top` will render wrong. |
| Performance under heavy output | Poor | 0.84 MB/s (above) |

Maintenance: last release **Nov 2023**; classifiers stop at Python 3.12.

**Alternatives in pure Python:** effectively none that are both faster and more
complete. `pyte` is the de-facto library. Faster paths mean *not* emulating in
Python: (a) forward raw bytes to a JS `xterm.js` front-end (pyxtermjs model) and
never emulate server-side; (b) call a Rust emulator via PyO3/subprocess. If you
must emulate in Python, only feed a **bounded tail** (see §4).

---

## 3. Rust PTY / terminal stack

| Crate | Role | Latest (as of 2026-08) | Notes |
|---|---|---|---|
| `portable-pty` (wezterm) | PTY spawn + resize + reader/writer, cross-platform | **0.9.0** (2025-02) | Mature, powers wezterm. Clean `openpty`/`CommandBuilder`. **Benchmarked here (0.8.1).** |
| `pty-process` | PTY + `std`/`tokio`/`async-std` process spawning | **0.5.3** (2025-07) | Good native-async ergonomics; Unix-focused. |
| `rustix-openpty` | Thin `openpty`/`login_tty` over rustix | **0.2.0** (2025-03) | Low-level primitive (used by alacritty). Build your own spawn on top. |
| `vt100` | VT emulator (screen + scrollback) | **0.16.2** (2025-07) | Simple API, pairs with tui-term. **Benchmarked here.** |
| `alacritty_terminal` | Alacritty's emulator core | **0.26.0** (2026-04) | Most battle-tested/complete VT (alt screen, etc.); heavier API. Actively developed. |
| `termwiz` (wezterm) | Terminal toolkit incl. emulator/surface | **0.23.3** (2025-03) | Full-featured; part of wezterm stack. |
| `wezterm-term` | wezterm's emulator crate | not on crates.io | Consumed as a git/workspace dep from the wezterm repo (UNVERIFIED as standalone-published). |
| `tui-term` | Ratatui **widget** wrapping a `vt100::Screen` | **0.3.4** (2026-04) | The drop-in pane widget if Forge UI is Ratatui. |

**Measured (`/tmp/forge-rust-pty`, `--release`):**
- `portable-pty` raw read: **215.5 MB in 2.47 s = 87.1 MB/s**.
- `vt100::Parser::process` full emulation (50×200, 10k scrollback): **215.5 MB in
  6.19 s = 34.8 MB/s**.

Maturity/perf summary: `portable-pty` and `alacritty_terminal` are the most
mature and are under active 2026 development; `vt100`+`tui-term` is the smallest
correct path to a working Ratatui pane. `vt100` at 34.8 MB/s is ~43× pyte and is
comfortably ahead of real build output rates.

---

## 4. Cross-cutting hard problems

### stdout vs stderr on a PTY
A PTY has one slave; `dup2(slave, 1)` and `dup2(slave, 2)` both point at the same
device, so bytes are **merged in write order with no marker** — unrecoverable.
Verified (`/tmp/pty_stderr.py`): `OUT1|ERR1|OUT2|ERR2` came back as one stream.

Options if you need them separate:
1. **stdout→PTY, stderr→pipe** (verified `/tmp/pty_split2.py`): you regain
   separation but stderr's `isatty()` becomes **false** — programs switch to
   block buffering and drop color on stderr, and cross-stream ordering is lost.
2. **Two PTYs** (one per stream): both stay ttys, but you now have two
   controlling-terminal-ish fds, double the emulation, and *still* no guaranteed
   interleave ordering.
3. **One PTY, merged** (what real terminals do).

**Recommendation: option 3 (single merged PTY) as the default.** It matches how
the process would behave in any real terminal, keeps colors/tty-detection
correct, and preserves ordering. If a specific consumer (e.g. structured error
capture) needs stderr alone, offer option 1 behind an explicit flag and document
that stderr is then a pipe, not a tty. BitBake specifically is happiest on a
single merged tty.

### Resize propagation
Verified (`/tmp/pty_resize.py`): `ioctl(master, TIOCSWINSZ, struct.pack("HHHH",
rows, cols, 0, 0))` immediately delivers `SIGWINCH` to the foreground group; the
child saw `stty size` = `40 120` and `TIOCGWINSZ` read back the same. **Set the
initial winsize at spawn** (an unset size reports `0 0` — observed). On every UI
pane resize: update stored dims → `TIOCSWINSZ` → the emulator's `resize(rows,
cols)` so the model and the child agree; the app redraws itself on SIGWINCH
(TUIs) or reflows on next output (line programs).

### Backpressure (build faster than UI can render)
Reality from the numbers: the PTY delivers ~70–90 MB/s; a Python emulator digests
~0.8 MB/s; even Rust vt100 is ~35 MB/s. The reader must **never** block the PTY
(a full slave write buffer stalls the *build*), and must decouple ingestion from
rendering. Design:

1. **Drain aggressively, always.** The `add_reader`/reader-thread loop does
   `os.read` into memory as fast as the kernel yields, so the child never blocks
   on a full pty buffer.
2. **Two-tier storage per process:**
   - **Raw ring buffer (bytes)** — bounded, e.g. 8–32 MB, `collections.deque`
     of chunks with a byte cap (or a Rust `VecDeque<u8>`). Oldest bytes evicted.
     This is the source of truth for "recent output" and for the agent.
   - **Emulator screen + bounded scrollback** — the live visible grid plus N
     lines of history (see below). Fed from the ring on a **render tick**, not
     per-read.
3. **Coalesce rendering.** Don't emulate/redraw per chunk. On a timer
   (~30–60 Hz) feed everything accumulated since the last tick in one batch, then
   render once. This is what makes `\r`-heavy progress bars cheap: a hundred
   carriage-return updates between ticks collapse to the final frame.
4. **Shed load on overload.** If ingest rate outruns the emulator for a sustained
   burst (e.g. `yes`), keep the raw ring intact but **skip intermediate emulator
   frames** — process only the tail needed to reconstruct the current screen
   (feed the last screenful worth of bytes, or fast-forward the parser). The user
   sees a live-ish screen; nothing blocks; memory stays bounded.
5. **Persist full logs to disk** out-of-band if a complete transcript is needed;
   the in-memory ring is for interactivity, not archival.

Net: bounded memory (ring cap + fixed scrollback), no build stalls (never block
the reader), no UI stalls (coalesced ticks + frame-skipping under load).

### Scrollback
Keep a fixed budget: **raw ring** (bytes, ~8–32 MB) + **emulator scrollback**
(~10k lines, matching what a real terminal keeps). `vt100::Parser::new(rows,
cols, scrollback)` and pyte `HistoryScreen(..., history=N)` both take an explicit
cap. **Agent read API:** expose a windowed view — `read_screen()` (current grid
as text), `read_scrollback(start_line, count)`, and `read_tail(nbytes)` off the
raw ring. Give the agent line-addressed windows, not the whole buffer, so its
context stays bounded regardless of build size.

### Detach / reattach & persistence
The PTY master fd lives in the Forge process; if a client (TUI/web) disconnects,
the process and its master keep running — reattach = re-subscribe to the stream
and **replay the emulator's current screen + scrollback** (the model already
holds it; no child cooperation needed). For persistence across a Forge restart,
the fd cannot survive, so either (a) accept that processes are Forge-lifetime, or
(b) run each child under a `tmux`/`abduco`/`dtach`-style detachable server, or a
small setsid'd shim that owns the PTY and speaks to Forge over a unix socket —
then Forge can die and reattach. Recommend starting with (a) and designing the
ProcessManager interface so a detachable backend can slot in later.

---

## 5. Benchmark reality-check (200 MB build log)

| Stage | Python | Rust |
|---|---:|---:|
| Raw PTY read (`os.read` / `portable-pty`) | **69.3 MB/s** (215 MB / 3.11 s) | **87.1 MB/s** (215 MB / 2.47 s) |
| Full VT emulation (pyte / vt100) | **0.84 MB/s** (53.9 MB / 63.8 s) | **34.8 MB/s** (215 MB / 6.19 s) |

Notes: the workload is `yes 'buildlog line …' | head -c 200M`. Both raw reads
return **215.5 MB** for a 200 MB (209,715,200-byte) payload because the PTY line
discipline's **ONLCR** expands each `\n`→`\r\n` across ~6.0 M lines — itself a
useful confirmation the stream is going through a real tty, not a pipe.

**Interpretation:** the *PTY* is not the bottleneck in either language
(kernel-bound, ~parity). The *emulator* is the whole story: Rust `vt100` handles
a 200 MB dump in ~6 s; pyte would need ~4 min. Forge's live-render path must
either be Rust, or (if Python) must never push the firehose through pyte —
enforce the §4 coalescing + tail-only emulation.

---

## Proposed ProcessManager interface (language-agnostic)

```
# Independent of any AI agent. Emits events; both human UI and agent consume.

type ProcState = STARTING | RUNNING | STOPPING | EXITED | FAILED | INTERRUPTED

record ProcInfo {
  id; pid; pgid; command[]; cwd; env{}; start_time;
  state: ProcState; exit_code?; term_signal?;
  rows; cols;
}

interface ProcessManager {
  # lifecycle
  spawn(command[], cwd, env{}, rows, cols) -> proc_id
      # forks, setsid, sets ctty on slave, applies initial winsize, state=STARTING->RUNNING
  wait(proc_id) -> (exit_code | signal)            # async; reaps, no zombies
  info(proc_id) -> ProcInfo

  # interaction (human OR agent)
  write_stdin(proc_id, bytes)                       # raw bytes to master
  send_key(proc_id, keystroke)                      # convenience: Ctrl-C -> 0x03, Ctrl-D -> 0x04
  interrupt(proc_id)                                # killpg(SIGINT)  [programmatic Ctrl-C]
  terminate(proc_id, escalate=true)                 # SIGTERM -> timeout -> SIGKILL, group-wide
  resize(proc_id, rows, cols)                       # TIOCSWINSZ (+auto SIGWINCH) + emulator.resize

  # output
  subscribe(proc_id) -> stream<bytes>               # live merged stream (fan-out to N clients)
  read_screen(proc_id) -> text_grid                 # current emulator screen
  read_scrollback(proc_id, start_line, count) -> lines
  read_tail(proc_id, nbytes) -> bytes               # from raw ring buffer

  # teardown
  cleanup(proc_id)                                  # close master, ensure group killed
  shutdown_all()                                    # crash/exit: killpg TERM->KILL every group
}

# Events (pushed to subscribers): state_changed, output(bytes),
# exited(code|signal), resized(rows,cols).
```

Design rules baked in: (1) single merged PTY per process; (2) child is always a
session/group leader so `killpg` is safe and complete; (3) `send_key` writes
control bytes (human path) while `interrupt`/`terminate` use `killpg` (program
path) — the §1 distinction, made explicit in the API; (4) output path is
raw-ring + coalesced-emulator per §4; (5) zero BitBake knowledge — BitBake is
just a `spawn(["bitbake", ...])`.
