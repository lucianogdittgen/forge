# 04 — Workspace, Git, config, persistence and security for Forge

Research output for Brief 04. All timings, `sysctl` values, syscall behaviour and file
counts below were measured on the machine this ran on unless marked **UNVERIFIED**.

**Test host:** Arch Linux, kernel `7.1.9-arch1-2`, x86_64, btrfs, Python `3.14.7`,
git `2.55.0`, sqlite `3.53.4`, bubblewrap `0.12.0`, kas `5.4`, bitbake `2.19.0`.

---

## 0. Summary of recommendations

| Area | Recommendation |
|---|---|
| Workspace detection | Facet set, not a type enum. Pure filesystem probe + one `git rev-parse` batch. Never source `oe-init-build-env` or run `bitbake` to detect. |
| Git | **Shell out to `git`**, `--porcelain=v2 -z`, off the event loop. Not pygit2, not dulwich. Enable `feature.manyFiles` + `core.fsmonitor` on large worktrees. |
| FS watching | Watch a **hand-picked allowlist of paths**, never the workspace root, never a build dir. Debounce 150–300 ms. Treat `IN_Q_OVERFLOW` as "rescan", because you *will* hit it. |
| Config | **TOML**, four layers, pydantic-validated, project layer cannot widen permissions. |
| Permissions | 4 capabilities + 1 severity flag. `bashlex`/tree-sitter AST classification for *prompt reduction only*. Sandbox is opt-in per process profile — **ask, don't sandbox**, for the bitbake path. |
| Persistence | SQLite (WAL) for structured state + append-only raw byte logs for PTY output. v1 does **not** reattach live processes; the API is shaped so a supervisor split is a transport change later. |

---

## 1. Workspace detection

### 1.1 The single most important design decision

A workspace is **not** one of {git repo, cmake project, Yocto tree}. `/home/luciano/meta-qcom`
is simultaneously a git repo, an OE layer, and a kas project. `/tmp/oe-core` is a git repo,
an OE layer container (`meta/`, `meta-selftest/`, `meta-skeleton/`) *and* an oe-core root.

Model the result as a **set of facets** with independent evidence, plus one derived
`primary_build_system` for UI purposes. Anything modelled as an enum will be wrong on the
first real tree.

### 1.2 Marker reliability (measured against real trees on this host)

| Marker | What it actually proves | Reliability |
|---|---|---|
| `conf/layer.conf` containing `BBFILE_COLLECTIONS` | The containing directory **is** an OE/BitBake layer | **Definitive.** Cheapest, most reliable OE marker there is. |
| `conf/bblayers.conf` | The containing directory is a **configured build directory** | **Definitive.** Also enumerates the layers via `BBLAYERS`. |
| `conf/local.conf` | Build directory, user config present | Strong, but `conf/local.conf` also appears in template dirs — pair with `bblayers.conf` |
| `oe-init-build-env` | An oe-core (or legacy poky) checkout root | Strong. Present at `/tmp/oe-core/oe-init-build-env` |
| `setup-environment` + `setup-environment.d/` | A vendor/community wrapper tree (OSSystems, Freescale community BSP) | Strong but **non-standard** — it is a downstream convention, not upstream. `/home/luciano/yocto-platform` has `setup-environment.d/inobram.py` and no `setup-environment` (it is `copyfile`-d in by `repo`). Treat as a *hint*, not proof. |
| `meta-*/` directory naming | **Nothing** | **Do not use.** oe-core's own layer is `meta/`, not `meta-*`. `/home/luciano/meta-freescale` is named `meta-*` but contains **zero** `meta-*` subdirectories. Naming is convention only; `conf/layer.conf` is the fact. |
| `kas/*.yml` filename glob | **Nothing** | **Do not use.** `/home/luciano/meta-qcom` keeps 20+ kas configs in `ci/`, not `kas/`. |
| YAML with a top-level `header.version` integer | A kas config file | **Definitive**, and it is the marker kas itself uses. `header` and `header.version` are the only *required* keys in the format ([kas docs](https://kas.readthedocs.io/en/latest/userguide/project-configuration.html)). Confirmed against `/home/luciano/meta-qcom/ci/base.yml` (`version: 14`). |
| `*.lock.<ext>` sibling | kas lockfile (pins `overrides.repos.<id>.commit`) | Definitive, confirms kas |
| `.config.yaml` | Written by `kas menu` | Strong |
| `.repo/manifest.xml` | A `repo`-managed multi-git workspace | Definitive |
| `default.xml` with `<manifest><project .../>` | A repo manifest *source* repo (not a checkout) | Strong. `/home/luciano/yocto-platform/default.xml` is exactly this. |
| `conf/toolcfg.conf` + `init-build-env` | A **bitbake-setup** build directory | Definitive, and new — see 1.3 |

### 1.3 A finding that changes the Yocto detection landscape

**Poky master is dead.** Cloning `https://git.yoctoproject.org/poky` at master yields a repo
containing a single `README` (commit `5c2b3d1`, "README: switch to released documentation URLs"):

> The poky repository master branch is no longer being updated. […] a) switch to individual
> clones of bitbake, openembedded-core, meta-yocto and yocto-docs, b) use the new bitbake-setup

`bitbake-setup` was introduced in Yocto 5.3 "Whinlatter" and is shipped in the bitbake repo
itself — confirmed locally: `/home/luciano/yocto-wrynose/bitbake/bin/bitbake-setup` exists next
to `bitbake-config-build`, with `bb.__version__ = "2.19.0"`. It parses a JSON configuration
describing layers + config fragments, clones them, and creates a build dir whose tool-owned
config lives in `conf/toolcfg.conf` (deliberately separate from `conf/local.conf`).
LTS branches (`kirkstone`, `scarthgap`, …) still exist on the poky repo — `git ls-remote`
lists them — so poky-shaped trees will be in the wild for years.

**Consequence for Forge:** the detection algorithm must handle *four* Yocto tree topologies,
not one — poky-monorepo (legacy/LTS), separate-clones + `oe-init-build-env`, kas, and
bitbake-setup — and must not privilege any of them. Sources:
[bitbake-setup manual](https://docs.yoctoproject.org/bitbake/bitbake-user-manual/bitbake-user-manual-environment-setup.html),
[sigma-star: bitbake-setup vs KAS](https://sigma-star.at/blog/2025/10/the-evolving-landscape-of-yocto-project-setup-bitbake-setup-vs.-kas/).

### 1.4 The detection algorithm

```
detect(start_dir) -> Workspace
```

**Phase A — anchor walk (upward, O(depth), pure stat).**

Walk from `realpath(start_dir)` toward `/`. Stop at the first of: `$HOME`, a filesystem
boundary (`st_dev` change), or 40 levels. At each level record the presence of:

```
.git            .repo/manifest.xml    .jj    .hg    .svn
conf/layer.conf                       conf/bblayers.conf
oe-init-build-env                     setup-environment
init-build-env                        conf/toolcfg.conf
Cargo.toml  package.json  pyproject.toml  CMakeLists.txt  meson.build
Makefile  GNUmakefile  go.mod  build.ninja  CMakeCache.txt  meson-info/
```

**Phase B — root selection (precedence, highest wins).**

1. `.repo/manifest.xml`  → `workspace_root` = that dir. A repo tree contains N independent
   gits; the outer dir is the workspace, each project is a sub-repo.
2. kas work dir — the directory that is the common parent of the kas-managed repos
   (`KAS_WORK_DIR`, default = dir containing the config's repos). Only if Phase C confirms kas.
3. `git rev-parse --show-toplevel` → `workspace_root`.
4. Highest anchor from Phase A.
5. `start_dir` itself.

Note the deliberate ordering: `.repo` and kas outrank git, because in those trees the git
toplevel is a *component*, not the workspace.

**Phase C — facet probe (bounded, downward).**

Run only inside `workspace_root`, hard-bounded:

* max depth 3, max 2000 directory entries examined, 500 ms wall-clock budget;
* hard-skip: `.git`, `build*/tmp*`, `tmp`, `tmp-glibc`, `sstate-cache`, `downloads`,
  `node_modules`, `target`, `.venv`, `dist`, `__pycache__`;
* only read files < 256 KiB.

Emit facets:

| Facet | Test |
|---|---|
| `git` | one batched call (see §2.4) |
| `repo_manifest` | `.repo/manifest.xml` or a `default.xml` whose root element is `<manifest>` |
| `oe_layer` | any `conf/layer.conf` at depth ≤ 3 matching `^\s*BBFILE_COLLECTIONS` |
| `oe_core` | `oe-init-build-env` at root, **or** a layer whose `layer.conf` sets `LAYERSERIES_CORENAMES` |
| `oe_build_dir` | `conf/bblayers.conf` |
| `bitbake_setup` | `conf/toolcfg.conf`, or `bin/bitbake-setup` in a bitbake checkout |
| `kas` | any `*.yml`/`*.yaml` at depth ≤ 3 whose parsed top level is a mapping with `header.version` as an `int` |
| `vendor_setup_env` | `setup-environment` or `setup-environment.d/` |
| `cargo`, `npm`, `python`, `cmake`, `meson`, `make`, `go` | the corresponding manifest file |

**Phase D — kas vs plain OE.** These are not mutually exclusive; kas *produces* a plain OE
build dir. Decide as follows:

```
if facet.kas and facet.oe_build_dir:   managed = KAS_MANAGED   (build dir was produced by kas)
elif facet.kas:                        managed = KAS_ONLY      (configs present, not built yet)
elif facet.oe_build_dir:               managed = PLAIN_OE
elif facet.repo_manifest:              managed = REPO_MANAGED
elif facet.bitbake_setup:              managed = BITBAKE_SETUP
else:                                  managed = NONE
```

`KAS_MANAGED` is what changes Forge's behaviour: build commands must be wrapped
(`kas shell <cfg> -c "bitbake ..."`) because a kas tree has no `oe-init-build-env` at its root
to source. This matches the split already encoded in this machine's `ossystems-embedded`
skills.

**Phase E — `primary_build_system` ranking** (first match):
`oe_build_dir` → `kas` → `oe_core` → `oe_layer` → `cargo` → `meson` → `cmake` → `npm` →
`go` → `python` → `make`. `make` is last always: a `Makefile` at the root of a Yocto layer
means nothing.

**Hard rules.**
* Detection is **read-only and never executes project code.** No sourcing `setup-environment`
  (it is arbitrary shell), no `bitbake -e`, no `kas dump` (it clones repos). Those are
  *enrichment* actions, gated behind EXECUTE permission, run only on explicit request.
* Detection must be re-runnable in < 50 ms warm. Budget it and degrade (drop Phase C
  facets, keep git) rather than block the UI.
* Parse YAML with `yaml.safe_load` only. kas itself uses PyYAML/YAML 1.1; a kas config from an
  untrusted repo is attacker-controlled input.

### 1.5 Algorithm output for `/home/luciano/meta-freescale` (actually run)

Observed markers:

```
FOUND conf/layer.conf
absent: oe-init-build-env, setup-environment, conf/bblayers.conf, conf/local.conf,
        .repo, Makefile, CMakeLists.txt, Cargo.toml, package.json, meson.build,
        pyproject.toml
meta-* subdirectories: none
conf/layer.conf: BBFILE_COLLECTIONS += "freescale-layer"
                 LAYERSERIES_COMPAT_freescale-layer = "whinlatter wrynose"
                 (no LAYERSERIES_CORENAMES)
dynamic-layers/: aglprofilegraphical arm-toolchain filesystem-layer gnome-layer ivi …
git toplevel: /home/luciano/meta-freescale   branch: master   remote: github.com:…/meta-freescale
git status --porcelain=v2: clean (0 entries)
tracked files: 616   worktree files: 615   .git: 60 MB (67445 objects, 4 packs)
```

**Verdict the algorithm produces:**

```
workspace_root      = /home/luciano/meta-freescale        (Phase B rule 3: git toplevel)
facets              = {git, oe_layer}
managed             = NONE
primary_build_system= oe_layer
layers              = [ { path: ".", collection: "freescale-layer",
                          compat: ["whinlatter","wrynose"],
                          depends: ["core"], priority: 5 } ]
dynamic_layers      = 14 conditional layer bundles (from BBFILES_DYNAMIC)
buildable_here      = false        # no bblayers.conf, no kas config, no oe-init-build-env
git                 = { branch: "master", dirty: false, ahead/behind: unknown-until-fetch }
```

This is correct and is the interesting case: **it is a Yocto tree you cannot build in.**
Forge's UI must say "OE layer — no build directory configured" and must *not* offer a
`bitbake` action, because there is nothing to source. A naive `meta-*`-glob or
"is there a `.bb` file" detector would wrongly report a buildable Yocto workspace here.

Also note `LAYERSERIES_COMPAT = "whinlatter wrynose"` vs oe-core's
`LAYERSERIES_COMPAT_core = "blacksail"` in `/tmp/oe-core` — the layer is one release behind
master. That comparison is free (two file reads) and is a genuinely useful thing for Forge
to surface; it is the single most common cause of `bitbake` refusing to parse a layer.

---

## 2. Git integration

### 2.1 Candidates

| Option | Version | License | Verdict |
|---|---|---|---|
| shell out to `git` | 2.55.0 here | GPLv2 (separate process) | **Recommended** |
| [`pygit2`](https://pypi.org/project/pygit2/) (libgit2) | 1.20.0, requires Python ≥ 3.11 | GPLv2 **with linking exception** | Rejected — see below |
| [`dulwich`](https://pypi.org/project/dulwich/) (pure Python) | 1.2.13, Python ≥ 3.10 | Apache-2.0/GPLv2 dual | Rejected for status; useful as a fallback parser |
| [`GitPython`](https://pypi.org/project/GitPython/) | 3.1.60 | BSD-3 | Rejected — maintenance mode, and it shells out anyway with worse ergonomics |
| Rust [`git2`](https://crates.io/crates/git2) | 0.21.0 (`libgit2-sys 0.18.8+1.9.7`) | MIT/Apache-2.0 | Same libgit2 objection |
| Rust [`gix`](https://crates.io/crates/gix) | 0.87.1 | MIT/Apache-2.0 | Fastest pure-Rust option, but same objection |

### 2.2 Why shelling out wins, concretely

The decisive point is not language-binding overhead. It is that **`git status` performance on
a big worktree comes almost entirely from three index features that live in git, not in
libgit2 or gix**: the untracked cache, index v4, and fsmonitor. Measured on a synthetic
100 000-file / 2 115-directory repo on btrfs, warm cache, best of 3:

| Configuration | `git status --porcelain=v2 --branch` |
|---|---|
| default (`git init`, nothing tuned) | **110–125 ms** |
| `-uno` (skip untracked scan) | 72–92 ms |
| `git diff-files --name-only` only | 45–47 ms |
| `core.untrackedCache=true` | **22–28 ms** |
| `+ feature.manyFiles` (index v4, `index.skipHash`) | 24–26 ms |
| `+ core.fsmonitor=true` (built-in daemon) | **11–12 ms** |
| fsmonitor, immediately after 5 000-file churn | 24–37 ms, settling to 11 ms |

That is a **10× improvement from configuration alone**, on the same binary. A libgit2 or gix
backend does not get any of it for free. Real repos for scale: `/tmp/oe-core` (5 037 tracked
files) is 9 ms untuned; `/home/luciano/meta-freescale` (616 files, 60 MB `.git`) is 4–11 ms
untuned — neither needs any of this. The tuning matters at oe-core+meta-openembedded+
meta-freescale+meta-qcom scale and at build-directory scale.

Secondary reasons: `--porcelain=v2 -z` is a stability-guaranteed machine format; pygit2 pins
an exact libgit2 ABI (`libgit2-sys 0.18.8+1.9.7` for git2 0.21) which is a recurring packaging
problem for a single-binary-ish distribution; and pygit2's GPLv2-with-linking-exception is a
license question you do not need to have.

The counter-argument — process spawn cost — is ~1–3 ms, i.e. under 10 % of even the *tuned*
100k-file status. It is noise.

### 2.3 A verified correction to the git documentation

`git-config(1)` on this machine (git 2.55.0) states:

> The built-in file system monitor is currently available only on a limited set of supported
> platforms. Currently, this includes Windows and MacOS.

**This is stale. It works on Linux.** Verified:

```
$ git config core.fsmonitor true && git status >/dev/null
$ git fsmonitor--daemon status
fsmonitor-daemon is watching '/tmp/bigrepo'
$ ps -eo pid,cmd | grep fsmonitor
2101799 /usr/lib/git-core/git fsmonitor--daemon run --detach --ipc-threads=8
$ GIT_TRACE2_PERF=1 git status 2>&1 >/dev/null | grep fsm_client
… fsm_client | ..query/command:builtin:0.2101799.…
… ipc-client | ....try-connect/path:.git/fsmonitor--daemon.ipc
… fsm_client | ..query/response-length:90045
… fsmonitor  | ..apply_count:5000
```

The daemon really is answering the query over `.git/fsmonitor--daemon.ipc` (a Unix domain
socket), and status drops to 11 ms. **How it watches matters a great deal for §3:**

```
$ ls -l /proc/2101799/fd
3 -> anon_inode:inotify
$ cat /proc/2101799/fdinfo/* | grep -c '^inotify'
2115                      # exactly one inotify watch per directory in the worktree
$ cat /proc/2101799/fdinfo/* | grep -c fanotify
0
```

So git's own Linux fsmonitor is **recursive inotify, one watch per directory, one inotify
instance**. That is the same budget Forge would spend, and it is a strong argument for
letting git do this watching instead of duplicating it (§3.4).

Caveats from `git-fsmonitor--daemon(1)`: it does not understand submodules (reports
submodule changes against the superproject — correct results, worse performance), and it
refuses network filesystems unless `fsmonitor.allowRemote=true`.

### 2.4 The concrete design

**One batched metadata call, ~1 process:**

```
git rev-parse --show-toplevel --absolute-git-dir --is-inside-work-tree \
              --abbrev-ref HEAD --short HEAD
```

**Status, the hot path:**

```
git --no-optional-locks status --porcelain=v2 --branch --untracked-files=normal \
    --ignore-submodules=dirty -z
```

* `--no-optional-locks` is **mandatory**: without it, a background `git status` takes
  `index.lock` to refresh the index and will race the user's own `git` invocations in the
  terminal pane. This is the single most likely way a git panel corrupts someone's workflow.
* `-z` gives NUL-delimited paths — the only way to be correct with the paths that appear in
  Yocto trees (spaces, UTF-8, and worse).
* `--porcelain=v2 --branch` gives branch, upstream, and ahead/behind in the header lines
  without a second `rev-list`.
* `--ignore-submodules=dirty` avoids recursing into every layer when the workspace is a
  repo-manifest tree.

**Never stalling the UI, in four rules:**

1. Run every git call in a worker (thread for Python, blocking-pool task for Rust). Never on
   the event loop, never with an unbounded timeout — 2 s hard kill, show stale-with-spinner.
2. **Single-flight + trailing coalesce.** At most one `git status` in flight per repo; new
   requests set a dirty bit and re-fire once on completion. A build touching 50 000 files
   must produce one refresh, not 50 000.
3. **Two-speed status.** Cheap tier (`git diff-files`, ~45 ms at 100k) on every debounce tick;
   full tier (with untracked) at most every 2 s or on explicit request. Untracked-file
   discovery is what costs — 110 ms vs 72 ms with `-uno` above.
4. **Auto-tune the repo on first sight, with consent.** If `git ls-files | wc -l` (or
   `.git/index` size) exceeds ~20 000 entries, offer to set
   `feature.manyFiles=true` + `core.untrackedCache=true` + `core.fsmonitor=true` in the
   repo's *local* config. Do not do it silently: `index.skipHash=true` makes git < 2.13
   refuse the index and git < 2.40 report `fsck` corruption (`git-config(1)`), which is a real
   hazard on a build server with an older toolchain.

**Diff:** `git diff --no-color --no-ext-diff -U3 -- <path>` per file, on demand, for the
selected file only. Never diff a whole Yocto worktree eagerly.
**Log:** `git log --format=%H%x00%an%x00%at%x00%s -z -n 100` — 1 ms on oe-core.
**Branch list:** `git for-each-ref --format=... refs/heads` — 1 ms on oe-core.

**Where dulwich earns its place:** as a zero-subprocess reader for `.git/HEAD`,
`.git/refs/`, and packed-refs when you only need "what branch am I on" at 60 fps for a status
bar. That is a nice-to-have, not v1.

---

## 3. Filesystem watching

### 3.1 The trap, quantified on this machine

`fs.inotify` limits, as measured:

```
fs.inotify.max_user_watches   = 524288
fs.inotify.max_user_instances = 1024
fs.inotify.max_queued_events  = 16384
```

Cost of a watch (measured, 52 551-directory tree, 3 trials):

```
recursive os.walk to enumerate 52 551 dirs : 257 ms
inotify_add_watch × 52 551                 :  79 ms  (1.5 µs/watch)
kernel Slab delta                          : ~5 970–6 100 kB  →  116–119 bytes/watch
```

So memory and setup are **not** the problem people assume. 500 000 watches ≈ 58 MB of slab
and would take ~1 s of `inotify_add_watch` — the enumeration walk dominates.

**The actual killer is the event queue.** Measured directly: 5 000 directories watched,
100 000 create+delete events generated in 1.28 s, queue not drained during generation:

```
generated ~100000 events in 1.28s
drained 16385 events, IN_Q_OVERFLOW markers: 1
```

**83 615 events were silently discarded.** The kernel delivers exactly `max_queued_events`
(16 384) plus one `IN_Q_OVERFLOW` marker (`wd == -1`, `mask & 0x4000`) and drops the rest.

This is the whole problem in one number. A bitbake `do_compile` of the kernel or a
`do_rootfs` produces events at hundreds of thousands per second across `tmp/work`. **No
debouncing, no watcher library, and no amount of tuning fixes this** — you cannot drain
faster than a build can churn, and once you overflow, your view of the filesystem is
provably wrong. The only correct responses are (a) don't watch there, and (b) treat
overflow as "my state is invalid, rescan from scratch".

Secondary limits that bite in practice: `max_user_instances = 1024` is **per user, not per
process** — 22 inotify instances were already in use by other processes on this idle
machine. Forge sharing a box with VS Code, a language server, and a `git fsmonitor--daemon`
per repo can plausibly exhaust instances long before watches.

### 3.2 Library comparison

| Option | Version | Assessment |
|---|---|---|
| [`watchdog`](https://pypi.org/project/watchdog/) | 6.0.0, Python ≥ 3.9, Apache-2.0 | Mature, portable. `InotifyObserver` builds the recursive watch set itself, in Python, one `inotify_add_watch` per directory. Its recursive walk on a build tree is the failure mode. Fine when you hand it a *small*, explicit path set. |
| raw `inotify` via `ctypes`/[`inotify_simple`](https://pypi.org/project/inotify-simple/) 2.0.1 | — | What you need if you want `IN_Q_OVERFLOW` handling, per-watch budgets, and `IN_ONLYDIR`/`IN_EXCL_UNLINK` control. watchdog abstracts overflow away, which is precisely the thing you must not abstract away. |
| Rust [`notify`](https://crates.io/crates/notify) | 8.2.0 (+ [`notify-debouncer-full`](https://crates.io/crates/notify-debouncer-full) 0.7.0) | Best-in-class. `notify-debouncer-full` gives correct rename-pairing and dedup, which you would otherwise write yourself. Still one inotify watch per directory — **it does not solve the scale problem, only the ergonomics.** |
| `fanotify` | kernel supports it here (`/proc/sys/fs/fanotify` present) | The only mechanism that watches a whole *mount* with a single descriptor (`FAN_MARK_FILESYSTEM`) — no per-directory watches, no enumeration walk. But: needs `CAP_SYS_ADMIN` for filesystem/mount marks, so it is out for an unprivileged TUI. **UNVERIFIED** whether the unprivileged `FAN_REPORT_FID` subset (Linux 5.13+) is sufficient for Forge's needs; worth a spike, not a v1 dependency. |

`ignore` 0.4.33 (Rust) or a `pathspec`-based matcher is a separate, necessary component:
you need gitignore semantics to decide what *not* to report even among paths you do watch.

### 3.3 Concrete strategy

**Watch this (allowlist, explicit, bounded):**

| Path | Why | Est. watches |
|---|---|---|
| `<repo>/.git/HEAD`, `.git/refs/`, `.git/packed-refs`, `.git/index` | branch/dirty invalidation, ~10 watches | ~10/repo |
| the **currently open editor buffers' directories** | external-modification detection | ≤ 20 |
| the **file-tree pane's currently expanded directories only** | lazy: watch on expand, unwatch on collapse | ≤ 200 |
| `<build>/conf/*.conf`, `<build>/conf/bblayers.conf` | config changed → re-detect workspace | ~2 |
| `<build>/tmp/log/cooker/` *(opt-in)* | bitbake cooker logs, low churn | ~2 |
| kas configs + `*.lock.yml` | detection invalidation | ≤ 10 |

Total steady-state target: **< 500 watches, 1 inotify instance.** That is 0.1 % of this
machine's watch budget and cannot overflow the queue under normal editing.

**Never watch, at any depth (hard denylist, applied before `inotify_add_watch`):**

```
tmp  tmp-glibc  tmp-*  build/tmp*  sstate-cache  downloads  cache  buildhistory
.git/objects  node_modules  target  .venv  __pycache__  .mypy_cache  .pytest_cache
dist  .tox  .cargo  .rustup  work  work-shared  deploy  sysroots  sysroots-components
```

`tmp/work`, `sstate-cache` and `downloads` are the three that make Yocto special, and the
first is where the millions of files live.

**Then belt-and-braces:** even with the denylist, enforce a **global watch budget** (default
5 000). When enumeration would exceed it, stop adding watches, mark that subtree
`POLLED`, and fall back to a 5 s `stat()` poll of *just the few files you care about* in it.
Log the degradation visibly — a silently-degraded watcher is worse than none.

**Debouncing:** coalesce per logical target with a 150 ms leading-edge-suppressed,
300 ms max-wait trailing window. Editors write via `rename()` (`IN_MOVED_FROM`/`IN_MOVED_TO`
pairs) and many tools do write-truncate-write; without pairing you will fire 3–5 times per
save. `notify-debouncer-full` does this correctly out of the box; in Python you must write it.

**Overflow handling — the non-negotiable part:**

```python
if mask & IN_Q_OVERFLOW:            # wd == -1
    metrics.inotify_overflows += 1
    invalidate_all_cached_fs_state()
    schedule_full_rescan(backoff)   # not immediate: overflow means churn is ongoing
```

Read the inotify fd with a large buffer (≥ 1 MiB, as in the test above) from a dedicated
thread that does nothing but `read()` and push to a queue, so decoding and matching never
back-pressure the kernel queue.

### 3.4 Do not duplicate git's watcher

Since `git fsmonitor--daemon` already holds one inotify watch per worktree directory (§2.3),
running Forge's own recursive watcher over the same worktree **doubles** the watch cost for
zero benefit. Recommended division of labour:

* **git worktree contents** → let `core.fsmonitor` handle it; Forge polls `git status`
  cheaply (11 ms at 100k files) on a timer plus on explicit triggers.
* **specific non-git files** (build config, logs, open buffers) → Forge's own small watcher.
* **build output directories** → **nothing watches them.** Progress comes from the PTY
  stream, which is the correct source and costs zero watches.

That last line is the key architectural insight: Forge already has a live byte stream from
the build process. Watching the filesystem to learn about the build is solving a solved
problem the expensive way.

---

## 4. Configuration

### 4.1 Format: TOML

| | TOML | YAML | JSON |
|---|---|---|---|
| Human-writable, comments | yes | yes | **no comments** |
| Parser in Python stdlib | **`tomllib`, 3.11+** (verified on 3.14.7) | no (PyYAML 6.0.3) | yes |
| Ambiguity / footguns | few | Norway problem, YAML 1.1 vs 1.2, anchors, tag-based deserialization | none, but unusable as human config |
| Deep nesting ergonomics | mediocre | good | poor |
| Round-trip with comments | `tomlkit` 0.15.1 | `ruamel.yaml` 0.19.1 | n/a |

**Pick TOML.** Zero-dependency reading via stdlib `tomllib`; `tomlkit` 0.15.1 only in the
write path so `forge config set` preserves user comments. The one real cost is that deeply
nested pane layouts are awkward in TOML — solved in §4.4 by using arrays of tables rather
than deep inline maps.

Note Forge will still need a YAML parser regardless, for kas config *detection* (§1.4).
That's PyYAML/`safe_load` only, never for Forge's own config.

### 4.2 File locations (XDG Base Directory Specification 0.8, 2021-05-08)

Verified on this host: none of `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` /
`XDG_CACHE_HOME` are set, all four default directories already exist, and
`XDG_RUNTIME_DIR=/run/user/1000` exists with mode `0700`.

| Purpose | Path | Spec basis |
|---|---|---|
| System config | `/etc/xdg/forge/config.toml`, then each `$XDG_CONFIG_DIRS` entry | `XDG_CONFIG_DIRS` defaults to `/etc/xdg` |
| User config | `$XDG_CONFIG_HOME/forge/config.toml` → `~/.config/forge/config.toml` | |
| Project config | `<workspace_root>/.forge/config.toml` (committed) | not XDG — travels with the repo |
| Project local | `<workspace_root>/.forge/config.local.toml` (gitignored) | |
| Sessions, conversations, output logs | `$XDG_DATA_HOME/forge/` → `~/.local/share/forge/` | data that must survive |
| Audit log, crash logs, last layout | `$XDG_STATE_HOME/forge/` → `~/.local/state/forge/` | spec: "state… should survive restarts but is not important or portable enough for DATA_HOME… logs, history, recently used files… view, layout" |
| Workspace scan cache, git status cache | `$XDG_CACHE_HOME/forge/` → `~/.cache/forge/` | safe to delete |
| Unix sockets, pidfile, lockfile | `$XDG_RUNTIME_DIR/forge/` → `/run/user/1000/forge/` | spec: "runtime files and objects like sockets"; 0700, cleared at logout — exactly right for a supervisor socket |

Use [`platformdirs`](https://pypi.org/project/platformdirs/) 4.11.4 rather than
hand-rolling. Honour `FORGE_CONFIG_DIR` as an escape hatch (Claude Code's `CLAUDE_CONFIG_DIR`
precedent), which also makes tests trivial.

### 4.3 Layering and merge semantics

Precedence, lowest → highest:

```
1. built-in defaults (in code, always complete)
2. /etc/xdg/forge/config.toml, then $XDG_CONFIG_DIRS in order
3. $XDG_CONFIG_HOME/forge/config.toml
4. <workspace>/.forge/config.toml
5. <workspace>/.forge/config.local.toml
6. FORGE_* environment variables
7. CLI flags
```

Merge rules: **tables merge recursively; arrays and scalars replace wholesale.** Do not
concatenate arrays — "my `permissions.deny` list got silently extended by three layers I
didn't read" is exactly the failure mode to avoid. Provide explicit `deny_append` /
`allow_append` keys where accumulation is genuinely wanted.

**Security rule — borrow this from Claude Code and do not compromise on it.** Certain keys
are honoured *only* from layers the user controls (2, 3, 6, 7) and are **ignored with a
startup warning** when they appear in the project layers (4, 5), because those come from a
cloned repository:

```
permissions.allow            permissions.mode          sandbox.*
permissions.additional_dirs  secrets.*                 agent.model / agent.base_url
```

`permissions.deny` is the exception: a project may *tighten* but never widen. Claude Code
implements the same asymmetry (`mask`, `network.tlsTerminate`,
`credentials.allowPlaintextInject` are "ignored in a repository's `.claude/settings.json`"
— [sandboxing docs](https://code.claude.com/docs/en/sandboxing)). Without this rule,
`git clone` is remote code execution.

### 4.4 Schema validation

**pydantic 2.13.4.** Reasons: it is the validation layer the Python ecosystem has converged
on, it gives typed access objects rather than dicts (which matters when config is read from
40 call sites), and its error messages name the offending key path. `jsonschema` 4.26.0 is
the alternative if a language-agnostic published schema is a goal; that is a v2 concern.

Emit a JSON Schema from the pydantic models (`model_json_schema()`) and ship it at
`docs/schema/forge-config.schema.json` so editors can complete the file — same trick kas uses
(`# yaml-language-server: $schema=…` in `/home/luciano/meta-qcom/ci/base.yml`).

Validation policy: **unknown keys are a warning, not an error** (forward compatibility with
newer Forge versions writing config), **invalid values are a hard error** at that layer only —
drop the layer, keep the rest, and surface the problem in the UI rather than refusing to start.

### 4.5 Config schema sketch

```toml
# ~/.config/forge/config.toml
schema_version = 1

[ui]
theme            = "dark"          # dark | light | auto
mouse            = true
scrollback_lines = 20000

[agent]
provider   = "claude"
model      = "claude-opus-5"
max_tokens = 16384
# base_url = "…"                   # user-layer only

[workspace]
detect_depth       = 3
detect_budget_ms   = 500
extra_ignore       = ["*.wic.gz", "*.bmap"]
auto_tune_git      = "ask"         # ask | always | never  (see §2.4 rule 4)
git_large_threshold = 20000        # index entries

[watch]
enabled       = true
watch_budget  = 5000               # max inotify watches; then degrade to polling
debounce_ms   = 150
max_wait_ms   = 300
poll_interval_ms = 5000            # for POLLED subtrees
deny = ["tmp", "tmp-*", "sstate-cache", "downloads", "buildhistory",
        "work", "work-shared", "deploy", "sysroots", "sysroots-components",
        "node_modules", "target", ".venv", "__pycache__"]

[permissions]
mode = "ask"                       # ask | trusted | readonly | yolo
scope_roots = ["${workspace_root}"]
allow = [
  "read:${workspace_root}/**",
  "exec:git status*", "exec:git log*", "exec:git diff*",
  "exec:bitbake-layers show-layers",
]
deny = [
  "read:~/.ssh/**", "read:~/.gnupg/**", "read:**/.env", "read:**/*.pem",
  "write:${workspace_root}/.git/**",
  "exec:rm -rf /*", "exec:sudo *", "exec:curl *", "exec:wget *",
  "network:*",
]
session_grants_persist = false     # "always allow" survives restart?
audit_log = true

[permissions.destructive]
require_typed_confirmation = true  # type the target path to confirm
protected_paths = ["${HOME}", "/", "${workspace_root}/.git"]

[sandbox]
enabled = false                    # opt-in; see §5.5
backend  = "bubblewrap"
allow_write = ["${workspace_root}", "${TMPDIR}"]
allow_net   = false
excluded_commands = ["bitbake", "kas", "runqemu", "devtool", "wic"]

[processes]
default_shell   = "/bin/sh"
kill_timeout_ms = 5000             # SIGTERM → wait → SIGKILL
persist_output  = true
output_cap_mb   = 256              # per process, ring-truncated

[persistence]
enabled          = true
retain_sessions  = 20
retain_days      = 30

# ---- pane layout: arrays of tables, not deep inline maps ----
[layout]
name = "default"
root = "main"

[[layout.node]]
id        = "main"
type      = "split"
direction = "horizontal"           # horizontal | vertical
children  = ["left", "right"]
sizes     = [0.55, 0.45]           # fractions, must sum ≈ 1.0

[[layout.node]]
id       = "left"
type     = "split"
direction = "vertical"
children = ["terminal", "processes"]
sizes    = [0.7, 0.3]

[[layout.node]]
id   = "right"
type = "split"
direction = "vertical"
children = ["chat", "diff"]
sizes = [0.6, 0.4]

[[layout.node]]
id = "terminal"
type = "pane"
widget = "terminal"
[layout.node.options]
follow_process = "active"

[[layout.node]]
id = "processes"
type = "pane"
widget = "process_list"

[[layout.node]]
id = "chat"
type = "pane"
widget = "agent_chat"

[[layout.node]]
id = "diff"
type = "pane"
widget = "git_diff"

[keys]
"ctrl+b n" = "pane.next"
"ctrl+b x" = "pane.close"
"ctrl+c"   = "process.interrupt"
```

Why a flat node list with id references rather than nested tables: TOML's nested-table syntax
for a 4-deep tree is painful to write and worse to diff, whereas `[[layout.node]]` entries are
one screenful each and reorderable. Validate at load that the graph is a tree rooted at
`layout.root`, that `children` sizes match arity, and that no cycle exists.

---

## 5. Security / permission model

### 5.1 The capability model

Four capabilities plus one orthogonal severity flag — **not** five peers:

| Capability | Grants | Scoped by |
|---|---|---|
| `READ` | read file contents, list directories | path glob set |
| `WRITE` | create / modify / truncate | path glob set |
| `EXECUTE` | spawn a process | command pattern set |
| `NETWORK` | outbound connection | host/domain set |

`DESTRUCTIVE` is a **flag on a requested action**, not a capability, because destructiveness
is a property of *scope*, not of *kind*: `rm build/tmp/foo.o` and `rm -rf ~` are both WRITE.
An action is flagged DESTRUCTIVE when it is (a) irreversible, (b) recursive, and (c) targets
a path outside a git worktree or matches `permissions.destructive.protected_paths`. Flagged
actions always prompt, in every mode except `yolo`, and get typed confirmation.

This mirrors how the real systems behave: Claude Code's `bypassPermissions` still refuses a
short list of actions, and its `deny` rules override `allow` unconditionally
([permissions docs](https://code.claude.com/docs/en/permissions): "Rules are evaluated in
order: deny, then ask, then allow… a deny rule can't carry allowlist exceptions").

### 5.2 Classifying an arbitrary shell command — and why it cannot be trusted

**The approach.** Three stages, in order:

**Stage 1 — parse, don't regex.** Use a real shell grammar (`bashlex`, or `tree-sitter-bash`
if Rust). Split on the full separator set. Claude Code documents exactly the right list:
`&&`, `||`, `;`, `|`, `|&`, `&`, and newlines, with the rule that "a rule must match each
subcommand independently."

**Stage 2 — structural veto. If the AST contains any of the following, return
`UNCLASSIFIABLE` and prompt. No exceptions, no heuristics.**

| Construct | Why it defeats analysis |
|---|---|
| `eval`, `source`, `.` | the executed text does not exist until runtime |
| `$(...)`, backticks | ditto |
| `<(...)`, `>(...)` | process substitution executes |
| any word containing `$VAR` in an argument to a WRITE/DESTRUCTIVE command | `rm -rf "$D/build"` with `D=""` is `rm -rf /build`; with `D` unset under `set -u` it errors, without it, it does not |
| `sh -c`, `bash -c`, `env X=y cmd`, `xargs` *with flags*, `find … -exec`, `find … -delete`, `watch`, `setsid`, `flock`, `ionice` | indirection: the real command is an argument |
| `docker run`, `docker exec`, `kas shell -c`, `devbox run`, `mise exec`, `npx`, `nix run`, `ssh host cmd` | environment runners that execute their tail |
| `>`, `>>`, `2>` to a path outside scope | a read-only-looking command that writes |
| aliases / shell functions | resolved by the user's shell, not visible to you |
| any word that is not a literal after wildcard expansion | globs resolve at runtime |

**Stage 3 — table lookup on the surviving simple commands.** A curated table
`argv[0] (+ subcommand) → {caps}`:

```
ls cat head tail wc stat file find grep rg realpath du df      -> READ
git status|log|diff|show|branch|remote -v|rev-parse|for-each-ref -> READ
git add|commit|checkout|switch|restore|stash|merge|rebase       -> READ|WRITE
git push|fetch|pull|clone|ls-remote                             -> READ|WRITE|NETWORK
cp mv mkdir touch tee sed -i patch                              -> READ|WRITE
rm rmdir shred truncate mkfs dd                                 -> READ|WRITE|DESTRUCTIVE
curl wget nc ssh scp rsync pip npm cargo apt dnf                -> NETWORK|...
bitbake kas devtool wic runqemu make cmake ninja meson          -> READ|WRITE|NETWORK|EXECUTE
sudo doas su                                                    -> ALWAYS PROMPT (never allowlistable)
```

Note `bitbake` correctly requires all four — it fetches from the network, writes gigabytes,
and executes arbitrary recipe shell. That is honest, and it is why §5.5 concludes the way it
does.

**Being honest about the limits.** Static analysis of shell is not a security boundary, and
every serious implementation says so:

* Cursor's own documentation describes its classifier as **"best-effort convenience, not a
  security boundary"** ([Cursor terminal docs](https://cursor.com/docs/agent/tools/terminal)).
* **CVE-2026-22708** — Cursor Terminal Tool Allowlist Bypass via environment variables: before
  2.3, with Auto-Run + Allowlist enabled, shell built-ins could execute without appearing in
  the allowlist, letting prompt injection poison the environment that trusted commands read
  ([OSV](https://api.osv.dev/v1/vulns/CVE-2026-22708)).
* Independent research on Cursor's denylist found obfuscated commands executing automatically
  despite matching denylist intent
  ([Backslash](https://www.backslash.security/blog/cursor-ai-security-flaw-autorun-denylist)).
* Claude Code's own docs warn that **"Bash permission patterns that try to constrain command
  arguments are fragile"**, listing redirects, alternate URL forms and `URL=http://github.com
  && curl $URL` as bypasses, and recommending you deny `curl`/`wget` wholesale and route
  network access through a tool with a real domain check instead.
* Even the wrapper-stripping that makes prefix rules usable creates holes: Claude Code strips
  `timeout`, `time`, `nice`, `nohup`, `stdbuf`, `command`, `builtin`, `noglob`, and bare
  `xargs`, but explicitly **not** `devbox run`, `npx`, `docker exec` — and documents that
  `Bash(devbox run *)` therefore "matches whatever comes after `run`, including
  `devbox run rm -rf .`".

**Conclusion:** classification exists to make the *common, safe* 90 % of actions
prompt-free. It must never be the thing standing between a hostile string and the
filesystem. Design so that a classifier bug is an annoyance (an unnecessary prompt) or a
contained mistake, never a compromise.

### 5.3 What the field actually does

| System | Model | What works | What doesn't |
|---|---|---|---|
| **Claude Code** | `allow`/`ask`/`deny` rules, `Tool(specifier)`, deny-first precedence; gitignore-syntax path rules for Read/Edit; `WebFetch(domain:…)`; per-project persisted "don't ask again"; optional OS sandbox (Seatbelt / bubblewrap+socat) | Deny-first with no allow-exceptions is the right primitive. Compound commands split into subcommands, each matched independently, and approving `git status && npm test` saves a rule for `npm test` alone. Read-deny also blocks Edit/Write on the same path. Project settings can't enable credential masking. | Argument-constraining rules are fragile *by their own documentation*; wrapper list is fixed and incomplete; the docs' own guidance is to deny network tools rather than pattern-match their args |
| **Cursor** | 3-stage: allowlist → sandbox → LLM classifier subagent; `.cursor/permissions.json`, `sandbox.json` | Sandboxing measurably improves autonomy — Cursor reports sandboxed agents stop 40 % less often | Classifier is non-deterministic and explicitly not a boundary; allowlist silently ignored in some sandbox mode combinations; CVE-2026-22708 |
| **aider** | Binary: `--yes-always` (env `AIDER_YES_ALWAYS`), plus `--suggest-shell-commands` on/off | Simple, comprehensible | No granularity at all. All-or-nothing. Not a model to copy. |
| **OpenHands** | v1 (1.7.0, May 2026): optional sandboxing, `LocalWorkspace` default, LLM security analyzer scoring Low/Medium/High with a confirmation policy | The risk-score → confirmation-policy shape is good | Docker runtime mounts `/var/run/docker.sock`, which by their own admission gives full host Docker control if the agent escapes |
| **Devin** | Full per-session cloud VM from a clean snapshot; ephemeral filesystem | Strongest isolation of the five; no classification needed because the blast radius is a disposable VM | Not applicable to Forge — Forge runs on the developer's machine, on *their* source tree, and the whole point is that the artifacts persist. Anything unsaved is lost. |

**The pattern across all five:** everyone who tried to make static command analysis the
security boundary shipped a bypass. Everyone who made isolation the boundary and used
analysis only to reduce prompts is doing fine.

### 5.4 Recommended design for Forge

**Rule syntax** — one flat string form, `capability:specifier`:

```
read:${workspace_root}/**
write:${workspace_root}/**
exec:git status*
exec:bitbake *
network:*.yoctoproject.org
network:*.openembedded.org
```

**Evaluation order (deny → ask → allow, first match wins, specificity irrelevant):**

```
1. hard-coded never-allow list   (sudo, doas, su, chmod +s, writes to ~/.ssh, ~/.bashrc)
2. permissions.deny              from any layer (project may tighten)
3. DESTRUCTIVE flag              -> typed confirmation, always
4. UNCLASSIFIABLE (Stage-2 veto) -> prompt, always
5. permissions.ask
6. session grants                (in-memory, this run only, unless persisted)
7. permissions.allow
8. permissions.mode default      (ask | trusted | readonly | yolo)
```

Deny must be non-overridable by allow — this is the single most important structural
property, and it is why Claude Code's docs spell out that "`Bash(aws *)` blocks every
matching call, including calls that also match a narrower allow rule".

**Path scoping.** Every READ/WRITE decision resolves the path *and every symlink hop*, and
checks both the literal path and the resolved target. Allow rules require **both** to match
(fall back to prompting if only one does); deny rules fire if **either** matches. Without
this, `ln -s ~/.ssh/id_rsa ./key` defeats path scoping entirely — Claude Code documents
exactly this asymmetry and the `./project/key → ~/.ssh/id_rsa` example.

**"Always allow this exact command."** Store the **canonicalised AST digest**, not the raw
string: `argv` after wrapper-stripping, with the cwd it was approved in, hashed. This means
`git log --oneline` approved in workspace A does not auto-approve it in workspace B, and
whitespace/quoting variations don't create duplicate grants. Persist to
`.forge/config.local.toml` (gitignored) — never to the committed project config, or one
developer's grants become everyone's.

**Session grants** default to memory-only. `session_grants_persist = true` opts in. Grants are
keyed `(workspace_root, capability, digest)` and expire.

**Audit log** — append-only JSONL at `$XDG_STATE_HOME/forge/audit.jsonl`, `0600`, one record
per *decision*, not per action:

```json
{"ts":"2026-08-27T14:31:02.441Z","session":"01J…","seq":417,
 "capability":"EXECUTE","action":"spawn",
 "argv":["bitbake","core-image-minimal"],"cwd":"/home/luciano/builds/x",
 "classification":{"caps":["READ","WRITE","NETWORK","EXECUTE"],
                   "destructive":false,"reason":"table:bitbake"},
 "decision":"allow","source":"rule:exec:bitbake *","actor":"agent",
 "process_id":"p_01J…"}
```

`actor` distinguishes agent-proposed from human-typed — without it the log cannot answer the
only question anyone ever asks it.

### 5.5 Where the boundary sits: **ask, with sandbox as an opt-in profile**

**Recommendation: Forge asks. Forge does not sandbox by default.** Justification, specific
to this product:

1. **A sandbox that must be disabled for the main use case is worse than no sandbox**, because
   it trains users to disable it. Forge's headline workload is bitbake. bitbake:
   * writes to `DL_DIR` and `SSTATE_DIR`, which are conventionally **outside** the workspace
     and often shared between projects (`/home/luciano/builds/*` here are separate from the
     layer checkouts);
   * fetches from arbitrary upstreams over the network by design — a domain allowlist for a
     Yocto build is a list of every SCM and mirror any recipe references;
   * runs `pseudo`, an `LD_PRELOAD` fakeroot implementation with its own sqlite database, and
     uninative, and its own namespace usage. Nesting bubblewrap around that is at best
     fragile. (**UNVERIFIED** — I did not run a full bitbake under `bwrap` on this host; but
     the mechanism conflict is structural, not speculative.)

   Claude Code's own answer to this shape of problem is `sandbox.excludedCommands` — an
   explicit escape hatch for commands that can't be sandboxed. Forge should ship that list
   pre-populated with `bitbake`, `kas`, `devtool`, `wic`, `runqemu`.

2. **The sandbox available here is genuinely usable for everything else.** Verified on this
   machine: `bwrap 0.12.0` present, `kernel.unprivileged_userns_clone = 1`,
   `/proc/sys/user/max_user_namespaces = 127083`, and
   `bwrap --ro-bind / / --dev /dev --unshare-net echo BWRAP_OK` succeeds. (`socat` is **not**
   installed, which is what Claude Code uses to relay sandboxed traffic to its proxy.) So a
   `sandbox.enabled = true` profile for test suites, linters, `oelint-adv`, and
   agent-generated scripts is a real, low-cost win — just not for the build.

3. **Asking is cheap when classification is good.** The measured cost of a prompt is one
   keystroke; the measured cost of a wrong auto-allow is unbounded. With a decent allowlist
   (`git` read subcommands, `ls`/`cat`/`grep`, the project's own build command) the steady-state
   prompt rate for real work is low. Cursor's own number — 84 % fewer approval prompts from
   their three-stage filter — shows how much of the traffic is trivially allowlistable.

4. **Layer defence rather than choosing one.** Concretely:
   * **Always on:** deny-first rules, path scoping with symlink resolution, DESTRUCTIVE typed
     confirmation, audit log, never-allow list.
   * **Always on:** capability scoping at the *Forge tool* level — the agent's file-read and
     file-write tools are checked directly, without going through a shell, so the shell
     classifier is not on the critical path for the majority of agent actions. This is the
     highest-leverage single decision in the whole model: **give the agent structured tools
     so it rarely needs to propose shell at all.**
   * **Opt-in per profile:** bubblewrap for `EXECUTE` where the command is not in
     `excluded_commands`.
   * **Documented, never default:** `mode = "yolo"`.

   Note the limits even of the sandbox layer, from Claude Code's own security notes: a proxy
   that doesn't terminate TLS makes its decision from the client-supplied hostname, so
   domain fronting can reach hosts outside the allowlist; allowing `/var/run/docker.sock`
   through is equivalent to granting host root; and write access to any directory on `$PATH`
   or to `~/.bashrc` is privilege escalation. If Forge ships `allow_write` defaults, they must
   exclude those.

---

## 6. Persistence

### 6.1 The reattachment question, answered empirically

> "a PTY child cannot outlive its parent unless re-parented — is that true?"

**The premise is subtly wrong, and the correction determines the design.** Re-parenting is
not the mechanism. **Holding the master fd open is.** Two experiments on this host:

**Experiment 1 — parent exits, master fd closes with it:**

```python
mfd, sfd = os.openpty()
pid = os.fork()
if pid == 0:
    os.setsid(); fcntl.ioctl(sfd, termios.TIOCSCTTY, 0)
    os.dup2(sfd,0); os.dup2(sfd,1); os.dup2(sfd,2)
    os.execvp("sh", ["sh","-c","for i in $(seq 1 30); do echo tick $i; sleep 1; done"])
time.sleep(0.5); os._exit(0)      # parent dies; nothing holds mfd
```

Result after 2 s: **child is gone.** `ps -p <pid>` returns no rows.

**Experiment 2 — identical, except a `setsid()`-detached holder process keeps `mfd` open:**

```
== child 2119199 ==
    PID    PPID STAT TT      SID     PGID CMD
2119199       1 Ss+  pts/7   2119199 2119199 sh -c for i in $(seq 1 40); …
== holder 2119200 ==
    PID    PPID STAT TT      SID     CMD
2119200       1 Ss   ?       2119200 python3 ptytest2.py
```

**Child survives**, `PPID = 1`, still on `pts/7`, still running.

**The actual rule:** when the last file descriptor referring to the PTY *master* is closed,
the kernel sends `SIGHUP` to the foreground process group of that terminal. Re-parenting to
`init` happens automatically on parent death and is irrelevant — the child in experiment 1
was reparented too, and still died. What saved experiment 2 was that a *surviving process
held the master*.

This is precisely how tmux, screen and dtach work: a daemon that `setsid()`s away from the
controlling terminal (double-fork for the portable guarantee that it can never re-acquire
one), **owns the PTY masters**, and treats the attached client as a disposable proxy over a
Unix socket. dtach's man page is explicit: "when the detach character is pressed, dtach
detaches from the session and exits, and the process running in the session is unaffected."
The [herdr docs](https://herdr.dev/docs/persistence-remote/) describe the same split —
"Herdr keeps panes running in a background server. Your terminal client can detach and
reconnect later" — with a `--no-session` flag that collapses the split for debugging.

**Corollaries Forge must respect:**
* A process is only reattachable if a *Forge-owned* process outlives the TUI. Reparenting to
  `init` is not enough.
* Reattaching to a process started by a *previous, now-dead* Forge is impossible for the PTY
  — the master is gone, the child got SIGHUP. You can only *report* that it existed.
* Scrollback for a reattached process must come from Forge's own on-disk log, because
  tmux/screen/dtach hold history in memory only and lose it too.

### 6.2 What must survive a restart

| State | Survives? | Mechanism |
|---|---|---|
| Conversation history | **yes** | append-only JSONL + SQLite index |
| Process list & metadata (argv, cwd, env-delta, start/end, exit code, state) | **yes** | SQLite |
| Terminal scrollback | **yes** | raw byte log on disk, replayed through the emulator |
| Live processes | **not in v1** — see 6.3 | requires the supervisor split |
| Workspace metadata (facets, layers, branch) | **yes, as cache** | SQLite in `$XDG_CACHE_HOME`, revalidated on load |
| Permission session grants | **opt-in** (`session_grants_persist`) | `.forge/config.local.toml` |
| Audit log | **yes, always** | append-only JSONL in `$XDG_STATE_HOME` |
| Pane layout / focus | **yes** | `$XDG_STATE_HOME` (spec explicitly names "view, layout" as state) |

### 6.3 Minimal v1 that does not paint you into a corner

**v1 scope:** Forge owns PTYs in-process. When Forge exits, its children die (correctly and
predictably — no orphaned bitbakes silently eating a build server). **Everything else
persists.** On restart, the process list shows previous processes as `ORPHANED` or `EXITED`
with their full recorded output replayable, and offers "run again" with the exact argv+cwd.

This is honest, small, and *not* a dead end — provided three constraints are honoured from
day one:

1. **A process handle is an opaque string ID, never a Python object or an fd.** Every caller —
   the agent tool layer, the TUI, the persistence layer — refers to processes as
   `p_01J7…`. If any code path holds a `subprocess.Popen`, the supervisor split becomes a
   rewrite instead of a transport change.
2. **The process manager's public surface is already RPC-shaped**: `spawn(argv, cwd, env,
   size) -> id`, `write(id, bytes)`, `resize(id, rows, cols)`, `signal(id, sig)`,
   `subscribe(id, from_offset) -> stream`, `status(id)`, `list()`. Every argument is JSON-
   serialisable. Every response is JSON-serialisable. No callbacks, no shared memory.
   `subscribe(id, from_offset)` is the load-bearing one — resumable-by-offset is what makes
   reattach work later *and* what makes on-disk replay work now.
3. **Output is written to disk as it streams**, not buffered and flushed at exit. The on-disk
   log is the source of truth; the in-memory emulator is a view over it. This is what makes
   scrollback survive a crash, not just a clean exit, and it is what a future supervisor
   would serve `subscribe(from_offset)` from anyway.

**v2 (when reattach is actually wanted):** add `forge-supervisor`, a double-forked daemon
holding the masters, listening on `$XDG_RUNTIME_DIR/forge/supervisor.sock`. The TUI becomes a
client. Because of constraints 1–3, this is a transport swap behind an unchanged interface.
`$XDG_RUNTIME_DIR` is the correct home for the socket per the spec ("runtime files and
objects like sockets"), it's `0700`, and it is cleared at logout — which is the right
lifetime for a supervisor.

**Do not** build the supervisor in v1. It brings: daemon lifecycle, version-skew between
client and daemon, socket auth, stale-socket recovery, orphan reaping, and a second place
for bugs to live. None of that pays for itself until users are asking for reattach.

### 6.4 Storage format

**SQLite (WAL) for structured state; flat files for bulk bytes.** SQLite 3.53.4 is present in
Python's stdlib here. WAL mode is essential: it lets the TUI read session metadata while a
writer appends, without blocking. Rationale for the split: putting 256 MB of kernel-build
output in a `BLOB` column makes every query slow and every backup enormous; putting session
metadata in JSON files makes "list my last 20 sessions" an O(n) directory scan with no
transactions.

```sql
PRAGMA journal_mode=WAL;  PRAGMA synchronous=NORMAL;  PRAGMA foreign_keys=ON;

CREATE TABLE schema_meta (version INTEGER NOT NULL);

CREATE TABLE workspace (
  id TEXT PRIMARY KEY,             -- hash of realpath
  root TEXT NOT NULL UNIQUE,
  facets TEXT NOT NULL,            -- JSON array
  managed TEXT NOT NULL,
  primary_build_system TEXT,
  detected_at INTEGER NOT NULL,
  detect_fingerprint TEXT NOT NULL -- mtimes+sizes of marker files; cheap revalidation
);

CREATE TABLE session (
  id TEXT PRIMARY KEY,             -- ULID, sortable
  workspace_id TEXT REFERENCES workspace(id),
  started_at INTEGER NOT NULL,
  ended_at INTEGER,
  forge_version TEXT NOT NULL,
  title TEXT,
  layout TEXT                      -- JSON snapshot
);

CREATE TABLE message (
  session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  ts INTEGER NOT NULL,
  role TEXT NOT NULL,              -- user | assistant | tool_use | tool_result | system
  body TEXT NOT NULL,              -- JSON
  tokens_in INTEGER, tokens_out INTEGER,
  PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;

CREATE TABLE process (
  id TEXT PRIMARY KEY,             -- p_<ULID>
  session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
  argv TEXT NOT NULL,              -- JSON array
  cwd TEXT NOT NULL,
  env_delta TEXT,                  -- JSON, only vars differing from Forge's env
  pid INTEGER,
  state TEXT NOT NULL,             -- STARTING RUNNING STOPPING EXITED FAILED INTERRUPTED ORPHANED
  exit_code INTEGER,
  started_at INTEGER NOT NULL, ended_at INTEGER,
  rows INTEGER, cols INTEGER,
  log_path TEXT NOT NULL,          -- relative to session dir
  log_bytes INTEGER NOT NULL DEFAULT 0,
  actor TEXT NOT NULL              -- human | agent
);
CREATE INDEX process_by_session ON process(session_id, started_at);

CREATE TABLE grant (
  workspace_id TEXT NOT NULL, capability TEXT NOT NULL, digest TEXT NOT NULL,
  spec TEXT NOT NULL, granted_at INTEGER NOT NULL, expires_at INTEGER,
  PRIMARY KEY (workspace_id, capability, digest)
) WITHOUT ROWID;
```

**PTY output log format** — `stdout.raw` is the **raw byte stream exactly as read from the
master**, unmodified: no line splitting, no ANSI stripping, no decoding. This is the only
format that can be replayed through the terminal emulator to reproduce what the user saw,
including `\r` progress rewriting and alternate-screen use. Alongside it, `stdout.idx` is a
fixed-width index enabling seek-by-time and seek-by-line without scanning:

```
stdout.idx record (16 bytes, little-endian, one per ~64 KiB of output or per 100 ms):
  u64 byte_offset
  u32 ms_since_process_start
  u32 line_estimate
```

Truncation at `processes.output_cap_mb`: keep the **head** (first 1 MiB — configuration echo,
early errors) and the **tail** (remainder of the cap — the actual failure), and write an
explicit `<<< forge: N bytes elided >>>` marker between them. A pure ring buffer loses the
"which bitbake config produced this?" prologue, which is exactly the thing you need.

### 6.5 On-disk layout

```
$XDG_CONFIG_HOME/forge/                       # ~/.config/forge
  config.toml

$XDG_DATA_HOME/forge/                         # ~/.local/share/forge
  forge.db                                    # SQLite, WAL  (forge.db-wal, forge.db-shm)
  sessions/
    01J8Z9F3QK4M7X2N5P6R8T0V1W/               # ULID
      meta.json                               # redundant, human-readable, crash-recoverable
      conversation.jsonl                       # append-only mirror of `message`
      processes/
        p_01J8Z9G1.../
          meta.json
          stdout.raw                          # raw PTY bytes
          stdout.idx                          # 16-byte seek records

$XDG_STATE_HOME/forge/                        # ~/.local/state/forge
  audit.jsonl                                 # 0600, append-only, never rotated silently
  last_layout.toml
  crash/2026-08-27T14-31-02.log

$XDG_CACHE_HOME/forge/                        # ~/.cache/forge
  workspace/<workspace_id>.json               # detection cache + fingerprint
  git/<workspace_id>.status.json              # last known git status

$XDG_RUNTIME_DIR/forge/                       # /run/user/1000/forge  (0700, tmpfs)
  forge.lock                                  # flock, single instance per workspace
  supervisor.sock                             # v2 only

<workspace_root>/.forge/
  config.toml                                 # committed; cannot widen permissions
  config.local.toml                           # gitignored; session grants land here
```

Why `conversation.jsonl` duplicates the `message` table: a JSONL append is atomic enough to
survive `SIGKILL` mid-write with at most one truncated final line, whereas an uncommitted
SQLite transaction is simply lost. On startup, reconcile: if `conversation.jsonl` has records
beyond `MAX(seq)` in the DB, replay them in. Cheap insurance against losing the last few
turns of a long session to a crash, which is the failure users actually mind.

Retention: on startup, delete sessions older than `retain_days` beyond the newest
`retain_sessions`, oldest first, and `VACUUM` if more than 25 % of pages are free. Never
delete `audit.jsonl`.

---

## 7. Consolidated version table

| Component | Version | Source |
|---|---|---|
| git | 2.55.0 | measured on host |
| bitbake | 2.19.0 (`bitbake-setup` present in `bin/`) | `/home/luciano/yocto-wrynose/bitbake` |
| kas | 5.4 (config format version 23) | `kas --version` |
| bubblewrap | 0.12.0 | measured |
| SQLite | 3.53.4 | Python stdlib on host |
| Python | 3.14.7 (`tomllib` in stdlib since 3.11) | measured |
| pygit2 | 1.20.0 (Python ≥ 3.11, GPLv2+linking exception) | PyPI |
| dulwich | 1.2.13 (Python ≥ 3.10) | PyPI |
| GitPython | 3.1.60 (maintenance mode) | PyPI |
| watchdog | 6.0.0 (Python ≥ 3.9, Apache-2.0) | PyPI |
| inotify-simple | 2.0.1 | PyPI |
| platformdirs | 4.11.4 | PyPI |
| tomlkit | 0.15.1 | PyPI |
| pydantic | 2.13.4 | PyPI |
| jsonschema | 4.26.0 | PyPI |
| PyYAML | 6.0.3 | PyPI |
| Rust `git2` | 0.21.0 (`libgit2-sys` 0.18.8+1.9.7) | crates.io |
| Rust `gix` | 0.87.1 | crates.io |
| Rust `notify` | 8.2.0 | crates.io |
| Rust `notify-debouncer-full` | 0.7.0 | crates.io |
| Rust `ignore` | 0.4.33 | crates.io |
| XDG Base Directory Spec | 0.8 (2021-05-08) | freedesktop.org |
| TOML | 1.0.0 | toml.io |

## 8. Unverified claims, flagged

* **UNVERIFIED** — that a full `bitbake` build runs correctly inside `bwrap`. The conflict
  with `pseudo`'s `LD_PRELOAD` fakeroot and its own namespace use is structural, but I did not
  run a build to confirm it fails. Test before promising a sandbox mode that covers builds.
* **UNVERIFIED** — that a real Yocto `tmp/` contains "millions of files". No populated build
  tree existed on this host (`/home/luciano/builds/*` contain only `.wic.gz` artifacts). The
  §3 argument does not depend on the exact number: the overflow measurement (100 000 events →
  83 615 lost) reproduces at 5 000 directories, which any build exceeds within seconds.
* **UNVERIFIED** — whether unprivileged `fanotify` with `FAN_REPORT_FID` (no `CAP_SYS_ADMIN`)
  can cover Forge's watch set with a single descriptor. `/proc/sys/fs/fanotify` exists on this
  kernel; the privileged mount-wide mark is definitely unavailable to a TUI.
* **UNVERIFIED** — the exact current kas config-format version. kas 5.4 reports "configuration
  format version 23"; the docs' newest example is also 23, but the docs do not state a
  canonical "current" number and point at a changelog instead. Detect on the *presence* of
  `header.version`, never on its value.
* **UNVERIFIED** — Devin's precise isolation boundary and egress policy. Everything in §5.3 for
  Devin comes from third-party write-ups, not Cognition's own security documentation.
* **Documentation defect, verified as such** — `git-config(1)` in git 2.55.0 states the
  built-in fsmonitor is available only on Windows and macOS. It demonstrably works on Linux
  (§2.3). Do not gate the feature on the documented platform list; probe
  `git fsmonitor--daemon status` instead.
