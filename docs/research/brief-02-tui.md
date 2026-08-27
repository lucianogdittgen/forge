# Research Brief 02 — TUI framework selection for Forge

You are a research agent. Investigate ONLY. Do not build the app.

## Context
Forge is a terminal AI development workbench. Its defining feature: a real,
live terminal pane (running bitbake, htop, vim) sits SIDE BY SIDE with an AI
conversation pane, plus file tree and git diff panes. Layout must eventually be
configurable. It targets Linux. Single-binary/easy-install distribution matters.

The hardest requirement: one pane must host a REAL terminal emulator — ANSI
colours, cursor addressing, carriage-return progress rewriting, alternate
screen, resize, Unicode/wide chars — not a scrolling log widget.

## Candidates to evaluate (add others if strong)
- Python + Textual
- Rust + Ratatui
- Go + Bubble Tea (only as a comparison point)
- Any framework with a genuinely mature embedded-terminal widget

## Questions to answer (evidence + versions + citations)
1. For EACH candidate:
   - Current version, maintenance health, release cadence.
   - Async model. Can it host an asyncio/tokio task that streams PTY bytes at
     high volume (a kernel build emits megabytes) without dropping frames or
     blocking input?
   - Rendering performance ceiling. What happens at 10k+ lines/sec of output?
     Find real benchmarks or reported limits.
   - Does a maintained embedded terminal widget exist? Name it, give its repo,
     version, last commit, and how complete its VT emulation is. Be skeptical —
     check whether it actually handles alternate screen and resize.
   - Focus/keyboard routing: can keystrokes be routed exclusively to the terminal
     pane (so Ctrl-C reaches bitbake, not the TUI) and then released?
   - Mouse, scrollback, text selection/copy support.
   - Distribution story: how does an end user install it on a clean machine?
2. Cross-cutting: which option gets a working vertical slice fastest, and which
   is the better 3-year bet? Say if they differ.
3. Check what the Claude integration story looks like in each language —
   note the Agent SDK exists for Python and TypeScript, and whether a Rust app
   would have to shell out or use raw HTTP. Flag this as a coupling risk.
4. Find and study PRIOR ART. Real projects that put a live terminal next to an
   AI pane or inside a TUI. Examples to look at: Textual's own terminal
   experiments, `textual-terminal`, Zellij, Wave Terminal, Warp, aider, elia,
   Rust `ratatui` + `tui-term`, `wezterm`'s crates. What did they learn? What
   did they give up? This is the most valuable part of this brief.

## Method
- WebSearch/WebFetch official docs and GitHub. Cite URLs, check last-commit dates.
- ACTUALLY TEST: create a venv in /tmp, `pip install textual`, run
  `python -m textual` demo if possible; `cargo add ratatui tui-term` in a scratch
  crate in /tmp and check it compiles. Report real observed results.
  Environment: Python 3.14.7, rustc 1.98.0, headless-friendly terminal.
- Mark anything unverified as UNVERIFIED.

## Deliverable
Write `/home/luciano/forge/docs/research/02-tui.md`:
- Comparison table across the criteria above.
- A RECOMMENDATION with reasoning, plus the strongest argument AGAINST it.
- A prior-art section with concrete lessons.
- Risk list.
Concise and technical. No filler. Then stop.
