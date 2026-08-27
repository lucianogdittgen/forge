# Research Brief 04 — Workspace, Git, config, persistence and security for Forge

You are a research agent. Investigate ONLY. Do not build the app.

## Context
Forge is a terminal AI development workbench. Around the agent and the process
engine it needs: workspace detection, a Git view, filesystem watching, config,
a permission model, and (later) session persistence. Forge must work well on
Yocto/BitBake trees but must NOT depend on Yocto — Yocto support should fall out
of a generic design.

## Questions to answer (evidence + versions + citations)
1. Workspace detection. How should Forge detect: git repo, branch, dirty state,
   build system (make/cmake/ninja/cargo/npm/meson), a Yocto/OE tree (what are the
   real reliable markers — `oe-init-build-env`, `setup-environment`, `conf/bblayers.conf`,
   `kas/*.yml`, `meta-*/conf/layer.conf`?), and kas-managed vs plain OE trees.
   Give a concrete detection algorithm with precedence rules. Test it against
   /home/luciano/meta-freescale, which is a real Yocto layer, and report what
   your algorithm would say about it.
2. Git integration. Compare: shelling out to `git`, `pygit2`/libgit2, `dulwich`,
   Rust `git2`/`gix`. Consider: performance on huge repos (poky is large),
   correctness, dependency weight, ability to get status/diff/log/branch cheaply.
   Recommend one. How do we get a fast, incremental `git status` on a repo with
   100k+ files without stalling the UI?
3. Filesystem watching. `watchdog` (Python) vs `notify` (Rust) vs raw inotify.
   The killer problem: a Yocto build directory contains MILLIONS of files and a
   build churns them constantly. Watching naively will melt. Give a concrete
   strategy — what to watch, what to exclude, debouncing, inotify watch limits
   (check the real `fs.inotify.max_user_watches` on this machine). This is a real
   trap; treat it seriously.
4. Configuration. Compare TOML/YAML/JSON for Forge's config. Layering
   (system/user/project). Schema validation. Where do files live on Linux (XDG)?
   How should a configurable pane layout be expressed? Sketch the config schema.
5. Security / permission model. Design a model distinguishing
   READ / WRITE / EXECUTE / NETWORK / DESTRUCTIVE.
   - How do we classify an arbitrary agent-proposed shell command into these?
     Be honest about the limits of static command analysis (pipes, subshells,
     `eval`, `$(...)`, aliases, `rm -rf $VAR` where VAR is empty).
   - Survey how existing tools do it: Claude Code's permission rules, aider,
     Cursor, OpenHands, Devin. What actually works in practice?
   - Recommend a design: allowlist/denylist, path scoping, per-session grants,
     "always allow this exact command", audit log.
   - Where does the boundary sit — do we sandbox, or do we ask? Justify.
6. Persistence. What must survive a restart: conversation, process list,
   terminal scrollback, workspace metadata. What is genuinely reattachable
   (a PTY child cannot outlive its parent unless re-parented — is that true?
   how do tmux/screen/herdr/dtach do it?). Recommend a minimal v1 that does not
   over-engineer but does not paint us into a corner. Sketch the storage format
   and on-disk layout.

## Method
- WebSearch/WebFetch docs and source. Cite URLs and versions.
- ACTUALLY TEST on this machine: inspect /home/luciano/meta-freescale, run
  `git status` timings, check inotify limits with sysctl, check XDG vars.
  Report real observed output.
- Mark unverified claims UNVERIFIED.

## Deliverable
Write `/home/luciano/forge/docs/research/04-workspace.md` covering all six areas
with recommendations, the detection algorithm, the config schema sketch, the
permission model design, and the persistence layout.
Concise and technical. No filler. Then stop.
