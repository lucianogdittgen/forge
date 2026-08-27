# ADR-0001 — Implementation language and TUI framework

- **Status:** Accepted
- **Date:** 2026-08-27

## Context

Forge's defining feature is a terminal pane that behaves like a real terminal
while a build runs in it. The acceptance criteria are behavioural: `bash`,
`htop`, `vim`, `less` and `bitbake` must look and behave as they do in the
user's own terminal. That makes VT emulation quality and throughput the
load-bearing technical property of the whole product, not an implementation
detail.

## Options considered

| Option | Pros | Cons |
|---|---|---|
| Python + Textual | TCSS gives configurable layout free; Claude Agent SDK is first-party Python; fastest for ordinary UI work | No working terminal widget exists; must hand-write ~4k LOC emulator; 10% of raw PTY throughput |
| Rust + Ratatui | `tui-term`+`vt100` render a correct terminal today; 92% of raw throughput; single static binary | Layout config is imperative code; no first-party Claude SDK; thin bus factors on the VT crates |
| Go + Bubble Tea | Good single-binary story | Weakest embedded-terminal ecosystem of the three |

## Decision

**Rust, with Ratatui + `tui-term` + `vt100` + `portable-pty`.**

## Consequences

### The evidence that decided it

Measured on this host, same workload (200k coloured lines, 7.29 MB, through a
real PTY):

| Stage | Python | Rust |
|---|---|---|
| PTY read only | 8.31 MB/s | 8.59 MB/s |
| PTY read + VT parse | **0.86 MB/s** | **7.92 MB/s** |
| retained | **10%** | **92%** |

Both languages read the PTY at the same speed — the kernel does that work. The
entire difference is the parser. In Rust the parser is ~5× faster than the
producer, so the workload is producer-bound with headroom. In Python the parser
is ~10× *slower* than the producer, so it becomes the bottleneck.

Crucially, **nothing is dropped in either language** — a PTY applies
backpressure. The Python symptom is therefore not tearing but *the build running
slower because the UI is throttling it*, with one core pegged. For a tool whose
purpose is watching a kernel build, that is the wrong failure mode.

Correctness was the harder blocker. **`pyte` implements no `1049`/`47` handling
at all**: entering and leaving the alternate screen permanently clobbers the main
screen. `vim` and `htop` — two named acceptance criteria — corrupt the pane. The
only Textual terminal widget, `textual-terminal`, does not import against current
Textual and was last touched in 2024.

The decisive signal: **Textual's own author, building the closest prior art
(Toad), did not use `pyte` or any off-the-shelf widget — he wrote ~4,000 LOC of
terminal emulator.** His purpose-built parser still benchmarks at ~18k lines/s,
the same order as `pyte`. Three independent Python implementations land at
0.7–0.9 MB/s. This is a language-level ceiling, not a library defect.

The Rust path was verified end-to-end before deciding: a real PTY, colour, CR
progress, wide chars, and alt-screen enter/exit rendered correctly through
ratatui, including `post-alt: active=false row0="MAIN-SCREEN"` — correct restore.

### Negative

- **Configurable layout gets harder.** This is the one requirement where Textual
  wins outright: TCSS is a real stylesheet users can edit with no recompile. In
  Ratatui, layout is imperative constraint-solving, so "configurable layout"
  means designing a format, writing its parser, and mapping it onto `Layout`.
  Accepted because the requirement is explicitly "must *eventually* be
  configurable" while the terminal pane is not deferrable. Zellij's answer to
  the same problem was a WASM plugin runtime, which shows how large this can get
  — we will ship a fixed layout first and design the format only once pane types
  are stable.
- **No first-party Claude SDK for Rust.** See ADR-0002; this turns out not to
  cost what it appears to.
- `tui-term` is render-only (1,574 LOC): PTY lifecycle, key encoding, resize and
  selection are ours to write.
- `vt100` does **not reflow on resize** (verified: widening 10→20 cols leaves
  wrapped lines wrapped). Every soft-wrap emulator has this; tmux accepts it.

### Risks and mitigations

| Risk | Mitigation |
|---|---|
| `vt100` bus factor ~1, no reflow | Keep the emulator behind a trait from day one. `termwiz` and `alacritty_terminal` are production-grade escape hatches, and ratatui already ships a termwiz backend. |
| Layout config cost | Fixed layout in v1; format designed once pane types stabilise. |
| Key re-encoding tax | Expect a hand-written key table (Toad's is 251 lines). |

## Revisit when

Configurable layout moves to the front of the roadmap, or `vt100` proves too
thin — at which point swap the emulator behind the trait rather than the
language.
