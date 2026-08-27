# 02 — TUI framework selection for Forge

Research date: 2026-08-27. Environment: Arch Linux, Python 3.14.7, rustc 1.98.0,
xterm-256color. All benchmarks run on this machine unless marked otherwise.

**Bottom line: Rust + Ratatui + `tui-term`/`vt100` + `portable-pty`.** The
deciding evidence is not language preference — it is that Python has *no working
embedded terminal widget at all*, and that the one person best placed to build
one (Textual's author) had to write ~4,000 lines from scratch to get it, and
still landed at ~0.7 MB/s of VT throughput. Details and the counter-argument
below.

---

## 1. Comparison table

| Criterion | Python + Textual | Rust + Ratatui | Go + Bubble Tea |
|---|---|---|---|
| Version / date | **8.2.8**, 2026-06-30 | **0.30.2**, 2026-06-19 | **v2.0.9**, 2026-08-19 |
| Repo health | 37.1k★, last push 2026-07-11, 352 open issues | 22.4k★, last push 2026-08-24, 217 open issues | 44.6k★, last push 2026-08-19, 203 open issues |
| Cadence | 254 PyPI releases; 15 in 2026 H1 | 0.30 line since 2025-12; split into `ratatui-core`/`ratatui-widgets` | v2 line active |
| Async model | asyncio; framework owns the loop. PTY reader is a normal task. | You own the loop; tokio or plain threads. `crossterm` has `event-stream`. | `tea.Cmd` goroutines returning msgs. |
| **Embedded terminal widget** | **None that works.** `textual-terminal` 0.3.0 is dead (below). Toad has one but it is AGPL and not published as a library. | **`tui-term` 0.3.4**, 2026-04-07, last commit 2026-08-01, 229★, MIT. Render-only. | **None first-party.** `charmbracelet/x/vt` + `x/xpty` are in the *experimental* monorepo, outside v2 stability guarantees. |
| VT parse throughput | **0.86 MB/s / 23.6k lines/s** (pyte) | **46.3 MB/s / 1.43M lines/s** (vt100, release) | UNVERIFIED |
| End-to-end PTY read+parse | **0.86 MB/s** — 10% of the 8.31 MB/s raw read rate | **7.92 MB/s** — 92% of the 8.59 MB/s raw read rate | UNVERIFIED |
| Widget render cost | 1.18 ms/frame (Strip build, 50×200) → ~848 fps ceiling | 0.344 ms/frame (full `PseudoTerminal` 200×50) → ~2,905 fps ceiling | UNVERIFIED |
| Frame pacing | 60 fps cap (`TEXTUAL_FPS`); emits CSI 2026 synchronized output | You pace it; ratatui diffs the back buffer to cell-level deltas | "Cursed Renderer" (ncurses-derived) in v2 |
| Keys → PTY | Possible but lossy: Textual parses keys into names, you must re-encode to bytes. Toad ships a 251-line hand-written map. | Raw `KeyEvent` from crossterm; you encode. Same work, but no lossy round-trip. | Similar to Rust. |
| Focus release | No built-in answer. Toad's solution: **double-tap Esc**, because every key must reach the child. | Same problem, same class of solution. Your choice of escape gesture. | Same. |
| Mouse | Textual mouse events; must re-encode to SGR for the child. Toad does this. | `vt100` exposes `mouse_protocol_mode()`/`_encoding()`; you encode. | Via `x/vt`. |
| Scrollback | pyte `HistoryScreen` works; Toad keeps its own buffer | `vt100::Parser::new(rows, cols, scrollback_len)`, `set_scrollback(n)` — verified | Via `x/vt` |
| Selection/copy | Textual has native selection. Toad **disables it while the alt screen is active**. | Not provided; you implement over the cell grid. `vt100` surfaces an OSC-52 clipboard callback. | Not provided. |
| Distribution | `uv tool install` / `pipx`, or `curl \| sh` shipping a pinned interpreter. Toad requires **Python ≥3.14 exactly** and pins `textual==8.2.7`. | Single static binary. My test binary (portable-pty + vt100) is **836K** release. `cargo install`, or ship the artifact. | Single static binary. |
| Layout configurability | **Best.** TCSS stylesheet — reloadable at runtime, user-editable without recompiling. | Imperative `Layout`/constraint solver. Runtime-configurable layout is code you write. | Lip Gloss; similar to Rust. |

---

## 2. The embedded terminal question — verified results

This is the requirement that decides the project, so I tested it rather than
reading about it.

### Python: `textual-terminal` is dead

```
$ pip install textual-terminal          # 0.3.0, last release 2023-01-29
$ python -c "from textual_terminal import Terminal"
ImportError: cannot import name 'DEFAULT_COLORS' from 'textual.app'
```

It does not import against Textual 8.2.8. Repo `mitosch/textual-terminal` last
pushed **2024-06-27** (141★). It pins `textual>=0.8.0`; Textual has since gone
0.8 → 8.2.8. This is not "needs a patch" — it is unmaintained against a
framework that has had 254 releases since.

### Python: `pyte` cannot do alternate screen

`textual-terminal` delegates VT emulation to `pyte` 0.8.2 (last release
2023-11-12). Direct test:

```python
st.feed("MAIN-SCREEN\r\n")
st.feed("\x1b[?1049h"); st.feed("\x1b[HALT"); st.feed("\x1b[?1049l")
# s.display[0] == 'ALTN-SCREEN'      <-- main screen clobbered
# 1049 in s.mode  == False
```

Expected `'MAIN-SCREEN'`. Got `'ALTN-SCREEN'`. `pyte` has no `1049`/`47` handling
anywhere in its source. **Running `vim` or `htop` in a pyte-backed pane corrupts
the pane permanently.** CR-rewrite and wide chars do work; scrollback works via
`HistoryScreen`. Alternate screen does not. That single gap disqualifies the
whole Python off-the-shelf path.

### Rust: `tui-term` + `vt100` works, verified end to end

`cargo add ratatui tui-term portable-pty crossterm vt100` resolves and compiles
clean on rustc 1.98.0 (193 crates). I spawned a real PTY, fed it colour, CR
progress, wide chars, and an alt-screen enter/exit, and rendered the result
through ratatui's `TestBackend`:

```
in alt:   active=true  row0="ALT-CONTENT"
post-alt: active=false row0="MAIN-SCREEN"     <-- correct restore
cr-rewrite: "prog: 99%"
sgr: fg=Idx(1) bold=true | bg=Idx(27)
wide: cell0="你" is_wide=true cell1_cont=true cell2="好"
scrollback@10 row0="line88" offset=10
```

`vt100::Screen` also exposes `alternate_screen()`, `mouse_protocol_mode()`,
`mouse_protocol_encoding()`, `bracketed_paste()`, `application_keypad()`,
`application_cursor()`, `hide_cursor()`, plus callbacks for window title, bell,
resize request, and OSC-52 clipboard. That is the full surface you need to host
a real terminal.

**Two honest caveats.** (a) `vt100` does **not reflow on resize** — verified:
widening 10→20 cols leaves already-wrapped lines wrapped. Every soft-wrap-based
emulator has this; you either accept it (tmux does) or keep your own logical-line
model. (b) `tui-term` is **render-only** — 1,574 LOC total, no key encoding, no
PTY spawning, no input path. It converts a `vt100::Screen` into a ratatui
widget and nothing else. PTY lifecycle, key encoding, resize, and selection are
yours to write. It is honest about its scope rather than broken, but it is not
a drop-in terminal pane.

---

## 3. Throughput — the numbers that matter

Same workload everywhere: 200,000 lines of `\x1b[3Nm...` coloured text, 7.29 MB,
emitted by `sh` through a real PTY.

| Stage | Python | Rust |
|---|---|---|
| PTY read only (no parse) | 8.31 MB/s, 228k lines/s | 8.59 MB/s, 236k lines/s |
| PTY read + VT parse | **0.86 MB/s, 23.6k lines/s** | **7.92 MB/s, 217k lines/s** |
| Parse in isolation | 0.86 MB/s (pyte) | 46.3 MB/s (vt100, release) |
| Fraction of raw rate retained | **10%** | **92%** |

Read the middle row carefully. Both languages read the PTY at the same speed —
the kernel does that work. The difference is entirely the parser. In Rust the
parser is ~5x faster than the producer, so the workload is producer-bound and
you have headroom. In Python the parser is ~10x *slower* than the producer, so
the parser is the bottleneck and it saturates a core.

**The Python ceiling is not a pyte problem.** I benchmarked Toad's own
purpose-built parser — written by Textual's author specifically to replace pyte
for exactly this use case:

```
Toad ANSI parser: 200,000 coloured lines (7.29 MB) in 11119 ms
                  -> 17,986 lines/s, 0.66 MB/s
```

Slower than pyte, because it also maintains buffers and computes per-line
deltas — not a like-for-like parser comparison. But the order of magnitude is
the point: three independent Python implementations all land at **0.7–0.9 MB/s,
~20k lines/s**. Rust is **~50–70x** faster on the same shape of work.

**What "10k+ lines/sec" actually means.** Nothing gets dropped in either
language — a PTY applies backpressure, so a slow reader just makes the writer
block in `write(2)`. The observable symptom in Python is not tearing, it is
*your build slowing down* because the UI is throttling it, plus one pegged core.
Evidence: in the Python read+parse run the chunk count collapsed from 25,603 to
1,781 as the kernel coalesced writes behind the busy parser. A kernel build
emitting a few MB/s of gcc output will be actively slowed by a pyte-class parser.

**Rendering is not the bottleneck in either language.** Textual's per-line Strip
construction costs 1.18 ms for a 50×200 pane (~848 fps ceiling); ratatui +
`tui-term` renders and diffs a full 200×50 pane in 0.344 ms (~2,905 fps). Both
are far above the 16.7 ms budget for 60 fps. Textual also emits CSI 2026
synchronized output (`\x1b[?2026h`) and caps at 60 fps by default — the same
anti-tearing trick Zellij credits for its full-screen-app performance. *I could
not get a reliable full-compositor frame time out of Textual's headless
`run_test()` harness — that specific number is UNVERIFIED.*

---

## 4. Prior art — the most valuable section

### Toad (Will McGugan / Batrachian AI) — the direct precedent

Python + Textual, AGPL-3.0 with commercial licensing, 3.4k★, 983 commits, PyPI
`batrachian-toad` 0.6.20 (2026-05-26). Toad is *the* prior art: a Textual AI
terminal front-end whose README claims "a fully working shell with full-color
output, interactive commands, and tab completion… At time of writing Toad is the
only terminal UI which does this."

I cloned it and read the implementation. Lessons, all sourced from the code:

1. **He did not use `textual-terminal` or `pyte`. He wrote his own.**
   `src/toad/ansi/` is ~2,240 LOC (`_ansi.py` alone is 1,626), plus
   `shell.py` (304), `widgets/terminal.py` (567), `widgets/terminal_tool.py`
   (351). ~4,000 LOC of terminal emulator. `pyte` appears nowhere in
   `pyproject.toml`. **The strongest available signal that no off-the-shelf
   Python option exists is that the framework's own author didn't use one.**

2. **Coalesce PTY reads to one frame before parsing.** `shell_read.py` batches
   reads for up to `max_buffer_duration = 1/60`s (or 64 KiB) before handing them
   to the parser. This decouples byte rate from frame rate and is the single
   most important technique for high-volume output. Steal this regardless of
   language.

3. **Refresh changed lines, not the pane.** `TerminalState.write()` returns
   `(scrollback_delta, alternate_delta)` — sets of dirty line numbers. The widget
   intersects them with the visible range and issues one 1-row `Region` refresh
   per changed line. A `None` delta means "full refresh". This is what makes
   partial updates possible and is why he can claim no flicker.

4. **Scrollback and alternate screen are separate buffers, concatenated
   virtually.** The alt screen is placed after the scrollback in the scroll
   space, so a full-screen app doesn't destroy history.

5. **Focus release is a UX invention, not a technical one.** `Terminal` is
   `can_focus=True`, and `on_key` calls `event.prevent_default(); event.stop()`
   on *everything* — no key can reach the app while the pane has focus, which is
   correct (Ctrl-C must reach bitbake). Escape is the only way out, and Escape is
   itself a key the child needs. His answer: **tap Esc twice** within
   `ESCAPE_TAP_DURATION`; a single Esc is forwarded to the child.
   `border_subtitle = "Tap esc twice to exit"`. There is no better answer
   available in any framework — plan for a gesture.

6. **Key re-encoding is a 251-line hand-written table.** `ansi/_keys.py` maps
   Textual key names back to escape sequences (`"ctrl+shift+f1": "\x1b[1;6P"`,
   …). This is pure tax caused by the framework parsing keys into names before
   you see them.

7. **Text selection is given up while a TUI is running.**
   `allow_select = is_finalized or not alternate_screen`. You cannot select text
   in a pane running `vim`. Accepted tradeoff.

8. **Interrupt is explicit**: `await self.write(b"\x03")`. Resize is
   `TIOCSWINSZ` via `fcntl.ioctl`, dispatched through `asyncio.to_thread` so it
   never blocks the loop. Same for `os.write` to the master fd.

9. **Frontend/backend split over JSON-on-stdio**, agents reached via the Agent
   Client Protocol. His stated reasons: backend can be any language; the two
   processes get separate cores so the UI never waits on the agent; the frontend
   is swappable (desktop/web); the transport can become remote. **This
   architecture makes the host language of the UI nearly irrelevant to the AI
   integration story.** See §5.

10. **Distribution is the pain point.** `curl -fsSL batrachian.ai/install | sh`,
    or `uv tool install -U batrachian-toad --python 3.14`. It requires Python
    **≥3.14** and pins `textual[syntax]==8.2.7` *exactly*, plus
    `textual-speedups==0.2.1` (a compiled accelerator Textual imports in
    `geometry.py`). Also pulls `psutil`, `setproctitle`, `bashlex`, `watchdog`.
    Linux clipboard needs `xclip` installed separately. That is what
    "easy install" costs in Python.

11. **On flicker, from the announcement post**, aimed at Claude Code and Gemini
    CLI: rewriting previous lines is "a surprisingly expensive operation in
    terminals" with "a high likelihood you will see a partial frame", and "you
    can only update a maximum of a few pages before the flicker gets
    intolerable." His fix is app-managed partial regions — "It is something the
    app has to manage itself." Applies identically in Rust.

**Licensing caveat: Toad is AGPL-3.0.** Read it for architecture; do not copy
code into Forge unless Forge is AGPL.

### Zellij — Rust, terminal multiplexer, 35.1k★, last push 2026-08-26

Client/server split; the multiplexer core is native Rust and stays cheap
"even when managing dozens of concurrent panes running heavy I/O tasks." Its
cited performance win for hosting full-screen apps is **CSI 2026 synchronized
output** — the same mechanism Textual already emits. Its extensibility answer is
a WASM plugin runtime with protobuf-over-stdout between host and guest;
acknowledged cost is memory (the standard advice is tmux for headless servers,
Zellij for local dev). Relevant lesson for Forge's configurable-layout
requirement: they went to a *sandboxed plugin runtime* rather than a config file,
which is a much larger commitment than TCSS.

### WezTerm / Alacritty — the VT crates worth knowing

`portable-pty` 0.9.0 (from the WezTerm repo, 6.3M recent downloads) is the PTY
layer I tested and is the obvious choice. `termwiz` 0.23.3 (2025-03) is WezTerm's
terminal library — much more complete than `vt100`, and ratatui ships a
`ratatui-termwiz` backend, so it is a viable upgrade path if `vt100` proves
thin. `alacritty_terminal` 0.26.0 (2026-04-06) is the other production-grade
grid+parser, extracted specifically for embedding. `vte` 0.15.0 (14.3M recent
downloads) is the shared state machine underneath `vt100`. **Rust has four
independent, production-proven VT implementations to fall back on. Python has
`pyte`, which cannot do alternate screen.**

### Wave Terminal and Warp — what happens when you give up

Wave (`wavetermdev/waveterm`) wanted precisely Forge's product — terminal blocks
beside an AI pane, plus editors and previews — and shipped **Electron + React +
a Go backend**, not a TUI. Warp did the same with a GPU-rendered native app.
Lesson: everyone who prioritised UI richness over terminal-nativeness left the
terminal. Forge's premise is the opposite bet — the TUI *is* the point — but
know that the two best-funded attempts concluded the TUI ceiling was too low for
their goals.

### aider and elia — the negative results

aider (48.5k★, last push 2026-05-22) is a top-tier terminal AI tool that
deliberately stayed on `prompt_toolkit` + Rich, streaming into normal scrollback
rather than taking the alternate screen. It has no split panes and no embedded
terminal. *I found no maintainer statement giving the reason — treat the
rationale as inference, UNVERIFIED.* elia (2.4k★, Textual-based LLM chat) has
not been pushed since **2024-10-10** — a Textual AI TUI that went stale.

### `tui-term` — health check

229★, MIT, `0.3.4` (2026-04-07), last commit **2026-08-01**. Recent commits are
mostly Dependabot, but it has already migrated to ratatui 0.30's
`ratatui-core`/`ratatui-widgets` split — it tracks upstream. README self-describes
as "active development… work in progress"; the `controller` lifecycle helper is
`unstable`-gated and "limited to oneshot commands." Treat it as a well-scoped
rendering primitive, not a product.

---

## 5. Claude integration and the coupling risk

The Agent SDK is first-party for **Python and TypeScript only**. Anthropic's
documented path for every other language is to run the CLI as a subprocess with
`-p --output-format json`. Community Rust ports exist and reportedly lag the
official SDKs by 1–2 releases.

**I think this is a weaker argument for Python than it first appears**, for one
concrete reason: Toad — a *Python* app, written by someone with every reason to
use the Python SDK in-process — runs the agent **out of process over JSON on
stdio anyway**, via ACP. His stated reasons were language freedom, separate
cores, and a swappable frontend. If the correct architecture puts the agent in a
subprocess regardless of UI language, then Rust's lack of a first-party SDK
costs you a subprocess wrapper you were going to write anyway.

It is still a real risk, so flag it honestly: a Rust Forge that wants
*in-process* SDK features — hooks, permission callbacks, MCP wiring, telemetry —
is either writing them against raw HTTP or trusting a community port. Brief 01
owns this decision; if it concludes Forge needs deep in-process SDK integration,
that materially weakens this brief's recommendation. **This is the single
cross-brief dependency that could flip the answer.**

---

## 6. Recommendation

**Rust + Ratatui 0.30 + `tui-term` + `vt100` + `portable-pty`, with the agent as
a subprocess speaking JSON over stdio.**

Reasoning, in order of weight:

1. **The hardest requirement has a working answer in exactly one language.**
   I compiled and ran it. The Python equivalent does not exist: the only widget
   doesn't import, and its emulator can't do alternate screen — meaning `vim` and
   `htop`, two of the three programs named in the brief, break the pane.

2. **It is also the fastest path to a vertical slice** — which is
   counterintuitive, so state it plainly. Python's normal advantage is
   neutralised here: to get a terminal pane in Textual you must first write
   Toad's ~4,000 lines (or relicense Forge as AGPL and vendor his). In Rust,
   `cargo add tui-term portable-pty` gets you a rendering terminal pane in an
   afternoon; the remaining work (key encoding, resize, focus gesture) is work
   you'd owe in either language. **The two questions in the brief — fastest slice
   and best 3-year bet — do not diverge here.** That is unusual and is the main
   reason I'm confident.

3. **Headroom where it matters.** 92% of raw PTY rate retained vs 10%. Forge's
   defining workload is watching a kernel build, and Python would actively slow
   that build down while pegging a core.

4. **Fallbacks exist.** If `vt100` proves too thin (no reflow, bus factor ~1),
   `termwiz` and `alacritty_terminal` are production-grade drop-in-ish
   replacements, and ratatui already ships a termwiz backend. Python has no
   second option.

5. **Distribution matches the brief.** An 836K static binary versus pinning an
   exact Python patch version, an exact Textual version, a compiled speedups
   wheel, and a system `xclip`.

### The strongest argument against

**Configurable layout is the one requirement where Textual wins outright, and
it's a stated Forge goal.** Textual's TCSS is a real stylesheet — users edit a
`.tcss` file and the layout changes, no recompile, no config schema, no DSL for
you to invent. In Ratatui, layout is imperative constraint-solving code, so
"layout must eventually be configurable" becomes: design a config format, write
its parser, map it onto `Layout`, handle invalid configs. That is weeks of work
that Textual gives you free, and it compounds — every new pane type needs to be
expressible in whatever format you invented. Zellij's answer to the same problem
was a WASM plugin runtime, which tells you how large this can get.

Secondary: **the Rust terminal stack has thin bus factors.** `tui-term` is
essentially one maintainer with recent commits that are mostly Dependabot;
`vt100` is one maintainer (`doy`), last pushed 2025-07-12; `portable-pty` last
released 2025-02. None is abandoned, none is a company-backed project. Textual
has a company behind it and 15 releases in 2026 alone.

I judge these outweighed because layout config is deferrable ("must *eventually*
be configurable") while the terminal pane is not, and because the Rust fallbacks
are real while the Python ones are not. But if Forge's roadmap moves
configurable layout forward, or if Brief 01 demands in-process SDK integration,
re-open this.

### If you pick Python anyway

Do not fight it — budget for Toad's architecture explicitly: your own ANSI
parser (assume 2–4k LOC), 16 ms read coalescing, per-line delta refresh, a
hand-written key table, double-Esc focus release, selection disabled in alt
screen. Accept ~20k lines/s and a pegged core under heavy build output. Do not
plan around `textual-terminal` or `pyte`.

---

## 7. Risks

| # | Risk | Severity | Evidence | Mitigation |
|---|---|---|---|---|
| 1 | Brief 01 concludes Forge needs in-process Agent SDK features → Rust is wrong | **High** | No first-party Rust SDK; community ports lag 1–2 releases | Resolve Brief 01 before writing code. Note Toad runs the agent out-of-process anyway. |
| 2 | Configurable layout costs weeks in Ratatui that TCSS gives free | **High** | Ratatui layout is imperative; Zellij needed a WASM runtime | Ship a fixed layout first. Design the config format only once pane types are stable. |
| 3 | `vt100` bus factor ~1; no resize reflow | Medium | Single maintainer, last push 2025-07-12; reflow absence verified | Keep the parser behind a trait from day one. `termwiz`/`alacritty_terminal` are the escape hatches. |
| 4 | `tui-term` is render-only — PTY, keys, resize, selection are all yours | Medium | 1,574 LOC, zero input handling in source | Scope it as one milestone. Toad's `shell.py` (304 LOC) is the size reference for the PTY half. |
| 5 | Focus routing has no good answer in any framework | Medium | Toad ships double-tap-Esc and says so in the border subtitle | Adopt the gesture, make it discoverable in the pane border, make it configurable. |
| 6 | Text selection unavailable while a full-screen TUI runs | Low–Med | Toad disables `allow_select` in alt screen | Match the behaviour; add "copy whole scrollback" as the fallback. |
| 7 | Terminal pane fights the AI pane for the same keys | Medium | Toad's `on_key` swallows *everything* when focused | Global chord (Ctrl-a style) reserved outside the pane; nothing reserved inside it. |
| 8 | Rust slows AI/agent iteration versus Python | Medium | No SDK; slower compile-edit loop | The subprocess boundary means the agent side can be Python if you want it to be. |
| 9 | Naive `Paragraph`+`Wrap` over a large scrollback is O(entire buffer) per frame | Low | Ratatui maintainers document this | Irrelevant if you render via `tui-term`/`PseudoTerminal`, which is O(viewport). Verified 0.344 ms/frame. |
| 10 | The whole TUI premise has a ceiling | Low (strategic) | Wave and Warp both left the terminal for Electron/GPU | Accepted — it's Forge's differentiator. Keep the core (PTY, agent, workspace) UI-agnostic so a future GUI is possible. |

---

## Sources

Framework and crate metadata (queried 2026-08-27 via PyPI, crates.io, GitHub APIs):
- [Textual](https://github.com/Textualize/textual) · [docs](https://textual.textualize.io/) · [PyPI](https://pypi.org/project/textual/)
- [Ratatui](https://github.com/ratatui/ratatui) · [rendering concepts](https://ratatui.rs/concepts/rendering/) · [Bencher benchmarks](https://bencher.dev/perf/ratatui-org)
- [tui-term](https://github.com/a-kenji/tui-term) · [vt100-rust](https://github.com/doy/vt100-rust) · [portable-pty (WezTerm)](https://github.com/wezterm/wezterm)
- [Bubble Tea](https://github.com/charmbracelet/bubbletea) · [charmbracelet/x](https://github.com/charmbracelet/x)
- [textual-terminal](https://github.com/mitosch/textual-terminal) · [pyte](https://github.com/selectel/pyte)

Prior art:
- [Toad](https://github.com/batrachianai/toad) — source read at commit `dd4f90e` (2026-05-26)
- [Announcing Toad](https://willmcgugan.github.io/announcing-toad/) · [Toad released](https://willmcgugan.github.io/toad-released/)
- [OpenHands × Toad](https://www.openhands.dev/blog/20251218-openhands-toad-collaboration) · [InfoQ on Toad](https://infoq.com/news/2025/12/llm-agent-cli/)
- [Zellij](https://github.com/zellij-org/zellij) · [Zellij plugin system](https://zellij.dev/news/new-plugin-system/)
- [Wave Terminal](https://github.com/wavetermdev/waveterm) · [Wave frontend architecture](https://deepwiki.com/wavetermdev/waveterm/3-block-system)
- [aider](https://github.com/Aider-AI/aider) · [elia](https://github.com/darrenburns/elia)

Claude integration:
- [Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview) · [Agent Client Protocol](https://agentclientprotocol.com/overview/introduction)

Benchmarks and capability tests were run locally; scripts are in
`/tmp/forge-tui-test/` (Python) and `/tmp/forge-rs-test/` (Rust) and are
reproducible from the code quoted above.
