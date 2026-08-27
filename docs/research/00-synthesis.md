# Research synthesis — the decision that has to be made

Four investigations (`01`–`04`) ran in parallel on 2026-08-27. Everything below
was **executed and observed**, not read from documentation. This file records
where they agree, where they collide, and what the collision forces us to choose.

## 1. Findings that are settled

These are not in tension and can be treated as decided.

| # | Finding | Evidence |
|---|---|---|
| 1 | **A single PTY merges stdout and stderr irrecoverably.** Separating them costs stderr its tty-ness (`isatty()`→false, block buffering, colour dropped) and loses interleave order. | `OUT1\|ERR1\|OUT2\|ERR2` came back as one stream |
| 2 | **Ctrl-C needs two different mechanisms.** A human keystroke → write `0x03` to the master and let the line discipline decide. A programmatic stop → `killpg(pgid, SIGINT)`. | Writing `0x03` to a raw-mode child (`-isig`) did **nothing**; `sleep 5` kept running and `xxd` showed byte `03` arriving as literal data. `killpg` killed the same child. |
| 3 | **Always signal the process group, never the pid**, and `setsid()` the child. Otherwise `bash → bitbake` grandchildren survive, and you risk signalling Forge itself. | child pgid ≠ Forge pgid, verified |
| 4 | **Reattachment is about holding the master fd, not re-parenting.** | Parent exits, nothing holds master → child **dead**. Identical test with a detached holder process → child **alive**, `PPID=1`, still on `pts/7`. |
| 5 | **A build tree cannot be watched naively.** 524,288 watch limit; a 52,551-dir tree costs ~6 MB kernel Slab. Watch a hand-picked allowlist, never the root, never a build dir. Treat `IN_Q_OVERFLOW` as "rescan" — you *will* hit it. | measured on this host |
| 6 | **Shell out to `git`** (`--porcelain=v2 -z`), off the event loop. Not pygit2, not dulwich. | perf on real trees |
| 7 | **There is no push channel to a mid-turn model.** One `tool_use` → exactly one `tool_result`. `start`/`poll`/`wait` as separate tools is the *correct* pattern, not a workaround. `proc_wait` must take a **bounded** timeout. | all four integration options are turn-based |
| 8 | **Poky master is dead.** Yocto now has *four* tree topologies (poky-monorepo/LTS, separate-clones, kas, `bitbake-setup`). Detection must privilege none. | cloned poky master: one README saying so |

Finding 7 validates the brief's `start_process → process_id` model from the
agent side too, and finding 4 tells us exactly what v1 persistence can and
cannot promise.

## 2. The collision

Briefs 02 and 03 independently reached the same conclusion about the terminal,
and it points away from the language brief 01's recommendation is native to.

**The terminal pane — Forge's defining feature — is decisively better in Rust.**

| | Python | Rust |
|---|---|---|
| PTY read (kernel-bound) | 8.31 MB/s | 8.59 MB/s |
| PTY read **+ VT parse** | **0.86 MB/s** | **7.92 MB/s** |
| fraction of raw rate retained | **10%** | **92%** |
| alternate screen (`vim`, `htop`) | **broken** | correct |

Two results decide this:

- **`pyte` has no `1049`/`47` handling at all.** Entering and leaving the
  alternate screen clobbers the main screen permanently. `vim` and `htop` —
  two of the three programs the brief names as acceptance criteria — corrupt
  the pane. `textual-terminal` (the only widget) doesn't even import against
  current Textual; it is four years stale.
- **The Python ceiling is not a `pyte` problem.** Toad's purpose-built parser,
  written by Textual's own author *specifically to replace pyte for this exact
  use case*, benchmarks at 17,986 lines/s — same order. Three independent Python
  implementations all land at 0.7–0.9 MB/s.

Nothing is dropped in either language — a PTY applies backpressure. The Python
symptom is worse than dropped frames: **the build itself slows down**, because
the parser throttles the writer, while one core sits pegged. For a tool whose
entire purpose is watching a kernel build, that is the wrong failure mode.

The strongest signal available: **Textual's author did not use an off-the-shelf
Python terminal. He wrote ~4,000 LOC of emulator.** That is the real cost of the
Python path, and it is paid before Forge's first feature.

## 3. What dissolves the collision

Brief 01's finding that looked like it forced Python turns out to do the
opposite. **The Claude Agent SDK is itself a subprocess wrapper around the
`claude` binary** — verified in its own source:

```
claude_agent_sdk/_internal/transport/subprocess_cli.py
  → resolves a `claude` binary, sets CLAUDE_CODE_ENTRYPOINT=sdk-py,
    speaks newline-delimited JSON over stdin/stdout
-rwxr-xr-x 250162696 claude_agent_sdk/_bundled/claude   # ~239 MiB ELF, Node
```

So "don't wrap the `claude` executable" is not a choice between wrapping and not
wrapping. **Every option wraps it.** The only question is whether a typed Python
adapter sits in front of the pipe.

Toad reached the same architecture from the other direction: a *Python* app,
written by someone with every reason to use the Python SDK in-process, runs its
agent **out-of-process over JSON on stdio** anyway — for language freedom,
separate cores, and a swappable frontend. Which means:

> If the agent belongs in a subprocess regardless of the UI's language, then
> Rust's lack of a first-party SDK costs a subprocess wrapper we were going to
> write anyway.

## 4. What is genuinely at stake in the choice

The Python SDK path is **empirically verified**; the Rust-drives-CLI path is
**plausible but unverified**. Specifically, these were observed working in
Python and would have to be re-established over the raw protocol:

- `tools=[]` removes **every** built-in. Observed `system/init`:
  `tools=['mcp__forge__forge_proc_start']` — Forge owns 100% of the surface.
- A `PreToolUse` hook is **unshadowable**. This matters more than it sounds:
  `allowed_tools` bare names, `acceptEdits`, `bypassPermissions`, **and allow
  rules in the user's own `~/.claude/settings.json`** all silently bypass
  `can_use_tool`. A Forge user's personal Claude Code settings could otherwise
  punch a hole straight through Forge's permission model. `setting_sources=[]`
  closes it.
- `can_use_tool` is `async`, so it can park on a Future while the TUI renders an
  approval prompt and resume on the user's decision. Verified with a fake TUI:
  the event loop ticked 39 times during a run containing two tool calls and two
  human-approval pauses.

The right split, then, is **policy in the hook** (is this capability granted at
all) and **consent in the callback** (ask the human about this specific call).

## 5. Constraints that bind regardless

- **~250 MB of Node ELF per platform**, bundled in the Python wheel. Forge would
  ship a *second, version-pinned* copy of Claude Code beside whatever the user
  already has. SDK 0.2.145 pins CLI 2.1.247; this host has 2.1.246.
- **The bundled CLI is fetched at build time**, so an SDK upgrade can change
  agent behaviour with no Forge code change.
- **Licensing: Forge cannot offer "log in with your Claude subscription"**
  without prior Anthropic approval. Distributed Forge must use API keys.
- **The SDK silently inherits the machine's Claude Code configuration**,
  including a corporate proxy `ANTHROPIC_BASE_URL`. Pass `env=` explicitly.

## 6. The decision to make

Not "which language is nicer" but: **where does the Forge↔agent process boundary
sit, and what speaks the protocol on the far side?** The terminal engine is Rust
in every option that takes the terminal seriously; the agent is a subprocess in
every option at all.

Deferred to ADR-0001/0002.
