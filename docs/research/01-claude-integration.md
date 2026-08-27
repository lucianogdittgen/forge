# Claude integration architecture for Forge

Research output for brief 01. All version numbers and observed behaviour below were
captured on **2026-08-27**, Linux x86-64, **Python 3.14.7**, in a clean venv at
`/tmp/forge-research-venv`.

Markers: **VERIFIED** = I ran it and observed the output quoted. **UNVERIFIED** = documented
but not executed here; reason given.

---

## 0. Versions observed (VERIFIED)

| Package | Version | Notes |
|---|---|---|
| `claude-agent-sdk` (PyPI) | **0.2.145** | `Requires-Python: >=3.10`. Deps: `anyio`, `jsonschema`, `mcp`, `sniffio`. Installed and ran clean on 3.14.7. |
| `@anthropic-ai/claude-agent-sdk` (npm) | **0.3.247** | `npm view` |
| `anthropic` (PyPI) | **1.1.0** | `Requires-Python: >=3.10`. Deps incl. `httpx2`, `pydantic`, `jiter`. |
| `@anthropic-ai/sdk` (npm) | **0.121.0** | `npm view` |
| system `claude` CLI | 2.1.246 | `/usr/bin/claude` |
| CLI version pinned by the SDK | 2.1.247 | `claude_agent_sdk/_cli_version.py: __cli_version__ = "2.1.247"` |

Python 3.14 is **not** a problem for either package.

---

## 1. The four options

| | (a) Claude Agent SDK | (b) API + Tool Runner | (c) API + hand-written loop | (d) Wrap `claude` CLI |
|---|---|---|---|---|
| **Package** | `claude-agent-sdk` 0.2.145 | `anthropic` 1.1.0 (`client.beta.messages.tool_runner`) | `anthropic` 1.1.0 | `claude` binary |
| **In-process?** | **No** — spawns the `claude` binary, talks stream-JSON over stdio | Yes | Yes | No |
| **Agent loop** | Free | Free (`until_done()`) | You write it | Free |
| **Context mgmt / compaction** | Free (Claude Code's) | Server-side via `context_management` / `compact` betas; you wire it | You build it | Free |
| **Session persist / resume** | Free (`resume=<session_id>`) | You build it | You build it | `--resume` |
| **Built-in tools** | Read/Write/Edit/Bash/Glob/Grep/Web* — **removable** (§2.2) | None (only yours) | None | Present, harder to strip |
| **Custom tools** | `@tool` + in-process MCP server | `@beta_tool` decorator | Raw JSON schema | MCP config only |
| **Permission hooks** | `can_use_tool` + `PreToolUse` hooks + modes | Per-turn hooks; you own the gate | You own the gate | External MCP permission tool |
| **Streaming to a TUI** | `include_partial_messages=True` → `StreamEvent` deltas | `.stream()` / streaming runner | Raw SSE | stream-json parsing |
| **Swappability (brief requirement)** | Weakest — Claude Code semantics leak in | Strong | Strongest | Weakest |
| **Packaging cost** | **~250 MB bundled binary** (§2.6) | pip only | pip only | user must install CLI |
| **Model cost** | Same tokens; adds Claude Code system prompt overhead | Lean | Leanest | Same as (a) |
| **Build cost to reach Forge's v1** | Lowest | Medium (~2-4 wks) | High (~4-8 wks) | Low but a dead end |

Option (d) is dismissed as the brief expects: it is (a) with worse ergonomics, no typed
messages, and the same binary dependency. Nothing below revisits it.

---

## 2. Claude Agent SDK — concrete findings

### 2.1 Custom tools: in-process MCP, `@tool` decorator (VERIFIED)

Tools are Python functions wrapped in an **in-process** MCP server — no subprocess, no IPC
per tool call. Name on the wire is `mcp__{server}__{tool}`.

```python
from claude_agent_sdk import tool, create_sdk_mcp_server

@tool("proc_start", "Start a long-running process, returns a pid immediately",
      {"type": "object",
       "properties": {"cmd": {"type": "string"}},
       "required": ["cmd"]})
async def proc_start(args):
    return {"content": [{"type": "text", "text": '{"pid": 1234, "state": "running"}'}]}

server = create_sdk_mcp_server(name="forge", version="0.1.0", tools=[proc_start])
```

Verified signatures:

```
tool(name, description, input_schema: type | dict, annotations: ToolAnnotations|None) -> Callable[..., SdkMcpTool]
create_sdk_mcp_server(name, version='1.0.0', tools: list[SdkMcpTool]|None) -> McpSdkServerConfig
```

The schema arg accepts either a shorthand dict (`{"cmd": str}`) or full JSON Schema. Use
full JSON Schema — the shorthand makes every key required and supports no enums.

Two Python-only limits worth knowing: the `@tool` decorator forwards only `content` and
`is_error` from the handler's return dict (`structuredContent` is dropped — that needs a
standalone MCP server), and binary `resource.blob` blocks are dropped with a warning.

### 2.2 Yes — the built-in tools can be removed entirely (VERIFIED)

This was the main open risk and it is resolved. `ClaudeAgentOptions.tools=[]` removes every
built-in from the model's context. Observed `system/init` message from a live run:

```
[SystemMessage] subtype=init tools=['mcp__forge__forge_proc_start']
```

That is the complete tool list the model saw. No Bash, Read, Write, Edit, Glob, Grep, Web*.
Forge can supply 100% of the tool surface.

Two distinct layers — do not confuse them:

| Option | Layer | Effect |
|---|---|---|
| `tools=[...]` / `tools=[]` | **Availability** | Which built-ins exist in context. `[]` = none. MCP tools unaffected. |
| `disallowed_tools=["Bash"]` (bare) | **Availability** | Removes the tool from context. |
| `disallowed_tools=["Bash(rm *)"]` (scoped) | **Permission** | Tool stays visible; matching calls denied in every mode incl. `bypassPermissions`. |
| `allowed_tools=[...]` | **Permission** | Auto-approves. **Does not restrict anything.** |

### 2.3 Permissions — and a trap that matters for Forge

Documented evaluation order (6 steps): **hooks → deny rules → ask rules → permission mode →
allow rules → `can_use_tool`**.

`can_use_tool` works exactly as Forge needs — it is `async`, so it can park on an
`asyncio.Future` while the TUI renders a prompt, and resume when the user decides. **VERIFIED**:

```python
async def can_use_tool(tool_name, tool_input, ctx):
    fut = asyncio.get_running_loop().create_future()
    await approval_q.put((tool_name, tool_input, fut))   # hand to Textual
    return await fut                                      # await the human
```

Observed, with a fake TUI adding a 0.4 s "user thinking" delay:

```
[TOOL_USE] mcp__forge__proc_start {'cmd': 'bitbake core-image-minimal'}
   [can_use_tool] tool_use_id=toolu_01DHxePuYLbgBDoHZCnxUJiz -> queued to TUI
   [TUI] prompt: mcp__forge__proc_start(...) -> ALLOW
[TOOL_USE] mcp__forge__proc_kill {'pid': 4242}
   [can_use_tool] tool_use_id=toolu_013mRjnJSKJzu7JCacvFRzeN -> queued to TUI
   [TUI] prompt: mcp__forge__proc_kill({'pid': 4242}) -> DENY
[TEXT] I started the process `bitbake core-image-minimal`, which got **pid 4242**.
       However, I was unable to kill it — the kill action requires "DESTRUCTIVE"
       permission, which was not gra...
```

`PermissionResultDeny(message=...)` text reaches the model verbatim and it reasons about it
correctly. `ctx.tool_use_id` is always populated, so Forge can correlate a prompt to a
specific call.

**The trap.** My first run listed the tools in `allowed_tools` and the callback was never
invoked. The SDK says so out loud:

```
CanUseToolShadowedWarning: can_use_tool will not be invoked for:
mcp__forge__proc_start, mcp__forge__proc_kill. An allowed_tools entry that allows a whole
tool auto-approves it before the callback is consulted. To gate every tool call, use a
PreToolUse hook; or narrow the entry so calls fall through to can_use_tool.
Allow rules from settings files can also shadow the callback but are not visible here.
```

Bare-name `allowed_tools` entries, `acceptEdits`, `bypassPermissions`, **and allow rules in
the user's `~/.claude/settings.json`** all silently bypass `can_use_tool`. That last one is
disqualifying on its own: a Forge user's personal Claude Code settings could punch a hole in
Forge's permission model. Set `setting_sources=[]` to stop settings files being read at all.

**Therefore: Forge's capability gate belongs in a `PreToolUse` hook, not in `can_use_tool`.**
Hooks run first, on every call, and cannot be shadowed. **VERIFIED** — this run deliberately
auto-approved both tools via `allowed_tools` and the hook still fired and still denied:

```python
CAPS = {"mcp__forge__proc_start": "EXECUTE", "mcp__forge__proc_kill": "DESTRUCTIVE"}
GRANTED = {"EXECUTE"}

async def pre_tool_use(input_data, tool_use_id, context):
    cap = CAPS.get(input_data.get("tool_name", ""), "UNKNOWN")
    if cap not in GRANTED:
        return {"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": f"Forge: {cap} capability not granted"}}
    return {}

options = ClaudeAgentOptions(
    hooks={"PreToolUse": [HookMatcher(matcher=None, hooks=[pre_tool_use])]}, ...)
```

Output:

```
[TOOL_USE] mcp__forge__proc_start {'cmd': 'bitbake core-image-minimal'}
   [PreToolUse hook] mcp__forge__proc_start cap=EXECUTE
[TOOL_USE] mcp__forge__proc_kill {'pid': 777}
   [PreToolUse hook] mcp__forge__proc_kill cap=DESTRUCTIVE
[TEXT] I wasn't able to kill PID 777 — the environment blocked it with
       "Forge: DESTRUCTIVE capability not granted"...
```

Recommended shape: **`PreToolUse` hook = policy** (is this capability granted at all),
**`can_use_tool` = interactive consent** (ask the human for this specific call).

Permission modes available: `default`, `acceptEdits`, `plan`, `bypassPermissions`, `dontAsk`,
`auto`. `set_permission_mode()` changes it mid-session. For Forge, `default` + hook is right;
`dontAsk` is the headless/CI mode (converts every prompt to a denial, never calls back).

### 2.4 Streaming (VERIFIED)

`query()` returns an `AsyncIterator` of typed messages. With
`include_partial_messages=True` you additionally get raw `StreamEvent`s suitable for
token-by-token TUI rendering. Observed 18-22 deltas per short run:

```
[StreamEvent delta] {'type': 'thinking_delta', 'thinking': '', 'estimated_tokens': 50}
[StreamEvent delta] {'type': 'signature_delta', 'signature': 'ErQCCqgBCBEYAipA5XX5+bCS3sZ...'}
[StreamEvent delta] {'type': 'input_json_delta', 'partial_json': ''}
```

Message types to switch on: `SystemMessage` (`subtype="init"` carries the tool list),
`AssistantMessage` (`.content` → `TextBlock` / `ThinkingBlock` / `ToolUseBlock`),
`UserMessage`, `StreamEvent`, `ResultMessage` (`.is_error`, `.num_turns`, `.session_id`,
`.total_cost_usd`), `RateLimitEvent`.

Note `thinking_delta` arrives with `thinking: ''` — thinking display defaults to omitted.
Set `thinking={"type": "adaptive", "display": "summarized"}` if Forge wants to show reasoning.

### 2.5 Sessions (VERIFIED)

```python
async for m in query(prompt="Remember the codeword: PLATYPUS-9.", options=o):
    if isinstance(m, ResultMessage): sid = m.session_id
# -> turn1 session: b300ec66-8982-4de6-9820-a0393fdcb7bf

o2 = ClaudeAgentOptions(resume=sid, ...)
async for m in query(prompt="What was the codeword?", options=o2): ...
# -> RESUMED ANSWER: PLATYPUS-9
# -> turn2 session: b300ec66-8982-4de6-9820-a0393fdcb7bf   (same id)
```

Also available: `fork_session`, `session_store` (pluggable `SessionStore` ABC, so Forge can
persist sessions in its own DB instead of `~/.claude/`), `list_sessions`, `get_session_messages`,
`rename_session`, `delete_session`.

### 2.6 **It IS a subprocess wrapper around the `claude` CLI** — confirmed

This is the critical answer and it is unambiguous. `claude_agent_sdk/_internal/client.py`:

```python
from .transport.subprocess_cli import SubprocessCLITransport
```

`_internal/transport/subprocess_cli.py` resolves the binary in this order:

1. `_find_bundled_cli()` — `claude_agent_sdk/_bundled/claude`
2. `shutil.which("claude")`
3. `~/.npm-global/bin/claude`, `/usr/local/bin/claude`, `~/.local/bin/claude`,
   `~/node_modules/.bin/claude`, `~/.yarn/bin/claude`, `~/.claude/local/claude`
4. else raise `CLINotFoundError`

It sets `CLAUDE_CODE_ENTRYPOINT=sdk-py` and speaks newline-delimited JSON over stdin/stdout.
`ClaudeAgentOptions.cli_path` overrides discovery.

**The wheel ships the binary.** Measured in the venv:

```
-rwxr-xr-x 250162696 claude_agent_sdk/_bundled/claude
claude: ELF 64-bit LSB executable, x86-64, dynamically linked, not stripped
$ ./claude --version
2.1.247 (Claude Code)
```

**250,162,696 bytes — ~239 MiB — of Node-bundled ELF inside a Python wheel**, per platform.

Consequences for Forge packaging:

- A `pip install forge` drags ~250 MB and is platform-specific. A PyInstaller/`uv`-built
  Forge binary inherits it.
- Forge would ship a **second, version-pinned copy** of Claude Code alongside whatever the
  user already has. SDK 0.2.145 pins CLI 2.1.247; this machine has 2.1.246 on `PATH`. Skew is
  normal and the SDK resolves the bundled copy first, so behaviour won't match the user's CLI.
- Every agent turn crosses a process boundary and a JSON serialisation. Fine for Forge's
  interaction rate; it is not a hot loop.
- Debugging spans two runtimes (Python + a not-stripped Node binary). `stderr` callback and
  `debug_stderr` are the only windows in.
- The `.gitignore` next to the binary implies it is fetched at build time — an SDK upgrade can
  change the bundled CLI, i.e. change agent behaviour, without any Forge code change.

### 2.7 Authentication

Resolution order is the CLI's, not the Python SDK's. In **this** environment there is no
`ANTHROPIC_API_KEY` and no `~/.claude/.credentials.json`; auth resolved through the `env`
block of `~/.claude/settings.json`, which sets `ANTHROPIC_BASE_URL` (a self-hosted proxy) and
`ANTHROPIC_AUTH_TOKEN`. All live runs above went through that path — **VERIFIED** that the SDK
inherits ambient CLI credentials with zero explicit configuration.

That convenience is also a liability: **the SDK silently picks up whatever the machine's
Claude Code is configured with**, including a corporate proxy base URL. Forge should pass
`env=` explicitly rather than inherit, or users will get surprising routing.

**Licensing constraint — read this before planning distribution.** From the official overview:

> Unless previously approved, Anthropic does not allow third party developers to offer
> claude.ai login or rate limits for their products, including agents built on the Claude
> Agent SDK. Use the API key authentication methods described in the Quickstart instead.

So Forge **cannot** ship "log in with your Claude subscription" without prior approval from
Anthropic. Distributed Forge must use API keys. Branding rules also forbid calling it
"Claude Code" or mimicking its visuals.

---

## 3. What the Tool Runner / raw API route costs

`client.beta.messages.tool_runner` exists in `anthropic` 1.1.0. Surface **VERIFIED**
statically; the live loop is **UNVERIFIED** — every request through the proxy configured on
this machine returned `429 rate_limit_error` (`req_011CeTQwxaMsctJNotwTJ9BW`). The request
shape was accepted by the server, so this is a quota block, not an API-shape error.

```python
from anthropic.lib.tools import beta_tool

@beta_tool
def proc_start(cmd: str) -> str:
    """Start a long-running process. Returns immediately with a pid."""
    return json.dumps({"pid": 4242, "state": "running"})
```

The decorator returns `BetaFunctionTool` and derives the schema from the signature and
docstring — **VERIFIED**:

```json
{"name": "proc_start",
 "description": "Start a long-running process. Returns immediately with a pid.",
 "input_schema": {"type": "object", "additionalProperties": false,
                  "properties": {"cmd": {"title": "Cmd", "type": "string"}},
                  "required": ["cmd"]}}
```

`BetaToolRunner` methods (VERIFIED): `until_done`, `append_messages`, `set_messages_params`,
`generate_tool_call_response`. Async and streaming variants exist
(`BetaAsyncToolRunner`, `BetaAsyncStreamingToolRunner`) — the async streaming one is what
Forge would use.

What Forge must build itself on this route:

| Need | Effort | Notes |
|---|---|---|
| Tool loop | **free** | `until_done()` |
| Streaming to TUI | small | `BetaAsyncStreamingToolRunner` |
| Retries / backoff | free | SDK, `max_retries=2` default |
| Approval gate | small | per-turn hook, or just gate inside your own handlers — **simpler and unshadowable vs. option (a)** |
| Context compaction | medium | server-side `compact-2026-01-12` beta or `context_management` edits; you decide policy and must preserve compaction blocks in history |
| Session persistence / resume | **medium-large** | entirely yours: serialise message history, thinking blocks, tool results |
| System prompt / harness quality | **large, open-ended** | Claude Code's harness is a real asset; matching it is not a two-week job |
| Token accounting, cost display | small | `usage` on each response |
| Subagents, skills, plugins | large | only if Forge wants them |

Realistic estimate to parity with what Forge actually needs (not full Claude Code parity):
**2-4 weeks** for one engineer on Tool Runner; **4-8 weeks** hand-rolling the loop. The
delta between (b) and (c) is small and (b) dominates (c) — there is no good reason to pick (c).

---

## 4. The async-tool problem

**There is no push channel from Forge to a mid-turn model.** All four options are turn-based:
the model emits `tool_use`, you return exactly one `tool_result`, the turn continues. Nothing
in the Agent SDK, Tool Runner, or the raw API lets a tool deliver a *second, later* result for
the same `tool_use_id`. Do not design for one.

So: **start/poll/wait as separate tools is the correct pattern, not a workaround.** Two
delivery mechanisms compose:

1. **Model-driven (always works):** `proc_start` returns instantly; the model calls
   `proc_status` / `proc_wait` when it wants to know more.
2. **Forge-driven (option (a) only, UNVERIFIED here):** with `ClaudeSDKClient` in streaming
   mode, Forge can call `client.query(...)` on process exit to inject a new user turn —
   "process 3 exited, code 1, last 40 lines: ..." — waking an idle agent. `ClaudeSDKClient`
   exposes `query`, `receive_messages`, `interrupt`, `set_permission_mode`, `stop_task`
   (**VERIFIED** these methods exist; the injection pattern itself I did not run). This is the
   only real advantage option (a) has on this specific problem.

`proc_wait` must take a **bounded** timeout and return `state: "running"` on expiry. An
unbounded wait re-introduces the blocking `execute()` the brief rejects, and burns the turn.

### Proposed Forge tool surface

Capability in brackets is what the `PreToolUse` hook enforces.

**Processes**

```
proc_start(cmd: str, cwd?: str, env?: object, label?: str, pty?: bool=true)
    -> {proc_id, pid, state:"running", label, started_at}                     [EXECUTE]
proc_list()
    -> [{proc_id, label, cmd, state, pid, exit_code?, runtime_s, out_lines}]  [READ]
proc_status(proc_id: str)
    -> {proc_id, state:"running"|"exited"|"signaled", exit_code?, signal?,
        runtime_s, out_lines, last_line}                                      [READ]
proc_output(proc_id: str, from_line?: int, max_lines?: int=200,
            stream?: "stdout"|"stderr"|"both", grep?: str)
    -> {lines:[...], next_line, truncated, total_lines}                       [READ]
proc_wait(proc_id: str, timeout_s: int)          # BOUNDED. never blocks forever
    -> {proc_id, state, exit_code?, timed_out: bool}                          [READ]
proc_signal(proc_id: str, signal: "TERM"|"INT"|"KILL"|"HUP")
    -> {proc_id, state, delivered: bool}                                      [DESTRUCTIVE]
proc_input(proc_id: str, data: str)              # for interactive prompts
    -> {written: int}                                                          [EXECUTE]
```

**Filesystem** (Forge's own, replacing the built-ins)

```
fs_read(path, start_line?, end_line?)  -> {content, total_lines, truncated}   [READ]
fs_write(path, content, create_dirs?)  -> {path, bytes, created}              [WRITE]
fs_edit(path, old, new, replace_all?)  -> {path, replacements}                [WRITE]
fs_search(pattern, path?, glob?, max_results?) -> {matches:[{file,line,text}]} [READ]
fs_list(path, depth?)                  -> {entries:[{path,kind,size}]}         [READ]
```

**Build-domain** (the reason Forge exists — thin, typed wrappers, not raw bash)

```
bb_recipes(filter?)                    -> [{recipe, version, layer}]           [READ]
bb_task_log(recipe, task)              -> {path, lines}                        [READ]
bb_depends(recipe, kind?)              -> {graph}                              [READ]
```

**Network** is deliberately absent from the tool list: it is a *capability* attached to
`proc_start`, gated by the hook on inspection of `cmd`, not a separate tool.

Design notes:

- `proc_id` is a Forge-issued opaque handle, not the OS pid. Pids get reused; Forge's handle
  survives exit so `proc_status` still answers afterwards.
- Every result is JSON in a single `text` block (`structuredContent` is dropped by the Python
  `@tool` decorator — §2.1).
- `proc_output` must be paginated with `next_line`. A bitbake log is millions of lines; an
  unbounded tail will blow the context window in one call. Cap hard and set `truncated`.
- Mark the read-only tools `ToolAnnotations(readOnlyHint=True)` so the model batches
  `proc_status` + `proc_output` into one parallel turn.
- Return failures as `{"is_error": True, ...}` with an actionable message rather than raising.

---

## 5. Concurrency with Textual (VERIFIED)

No blocking, and no anyio/asyncio conflict. The SDK is built on `anyio`, which runs happily on
the asyncio backend — I drove the whole thing from plain `asyncio.run()` (what Textual uses),
with a 0.2 s heartbeat task running alongside:

```
STREAM DELTAS RECEIVED: 18
EVENT-LOOP HEARTBEAT TICKS DURING RUN: 39 (loop stayed responsive)
PYTHON DRIVER: asyncio.run  (SDK is anyio-based, ran fine on asyncio)
```

39 ticks across a run containing two tool calls and two 0.4 s human-approval pauses — the loop
was never blocked, including while `can_use_tool` was parked on a Future.

Pitfalls to design around:

- **Tool handlers run on the event loop.** Any blocking call inside a `@tool` handler freezes
  the TUI. Forge's handlers must be genuinely async, or use `asyncio.to_thread`. This is the
  most likely way to break the UI.
- Run `query()` in a Textual worker (`@work`), not inline in a message handler.
- `include_partial_messages=True` produces a high event rate — batch or throttle before
  touching widgets, or rendering will dominate.
- The subprocess is killed when the parent exits; the SDK tracks live children. Forge's own
  PTY children are separate and Forge must reap them itself.
- `max_buffer_size` guards against a single huge stdio line; a tool returning a giant blob can
  trip it. Another reason to paginate `proc_output`.

---

## 6. Recommendation

**Use the Claude Agent SDK (option a) for v1, behind Forge's `Agent` interface, configured as
a bare harness:**

```python
ClaudeAgentOptions(
    tools=[],                                  # zero built-ins — Forge owns the surface
    mcp_servers={"forge": forge_server},       # in-process, Forge's tools only
    hooks={"PreToolUse": [HookMatcher(None, [capability_gate])]},   # unshadowable policy
    can_use_tool=interactive_consent,          # TUI prompt, awaits a Future
    setting_sources=[],                        # ignore the user's ~/.claude settings
    permission_mode="default",
    include_partial_messages=True,
    env={...},                                 # explicit, do not inherit ambient creds
)
```

**Reasoning.** The brief's hard requirements are all satisfiable, and the two that looked
fatal turned out not to be: built-in tools are fully removable (§2.2), and the permission gate
can be made unshadowable via `PreToolUse` (§2.3). What remains is a free, production-grade
agent loop, context compaction, and session resume — the three most expensive things to build
and the three least differentiating for Forge. Forge's value is the TUI, the PTY manager, and
the Yocto domain knowledge; none of that is what you'd be writing on route (b) for the first
month. Taking (a) buys roughly a month.

**Main risk: the 250 MB CLI subprocess dependency (§2.6).** It is not a technical blocker —
it works — it is a *product* and *supply-chain* risk. Forge's distribution becomes
platform-specific and heavy; agent behaviour is pinned to a Node binary Forge doesn't build,
can't inspect, and whose upgrade can silently change how the agent acts; and the claude.ai
login restriction means Forge must require API keys from every user regardless.

**Mitigation, and it is the whole point of the `Agent` interface:** keep the seam narrow and
Claude-Code-shaped concepts out of it. If the interface is

```python
class Agent(Protocol):
    async def run(self, prompt: str, session: SessionId | None) -> AsyncIterator[AgentEvent]: ...
    async def interrupt(self) -> None: ...
```

with `AgentEvent` a Forge-owned union (`TextDelta`, `ToolCall`, `ToolResult`, `Approval`,
`Done`), then swapping to Tool Runner later is a contained job — mostly re-implementing
session persistence, which is the one thing option (a) is giving away for free. Write the
tool handlers as **plain async functions** and register them with `@tool` in a thin adapter
layer; they then port to `@beta_tool` unchanged. Do not let `session_id`, MCP tool naming
(`mcp__forge__*`), or `ClaudeAgentOptions` leak past the adapter.

---

## 7. What would make us change our mind

Switch to **Tool Runner (b)** if any of these turn out true:

- **Packaging blocks distribution.** If Forge must ship as a single modest binary, or support
  a platform with no `claude-agent-sdk` wheel, the 250 MB bundled CLI is disqualifying.
- **The permission model leaks.** If any path is found where a tool executes without the
  `PreToolUse` hook firing — the whole capability model rests on §2.3 holding. Write a test
  that asserts the hook fires for every tool call and run it against each SDK upgrade.
- **An SDK upgrade silently changes agent behaviour.** The bundled CLI is fetched at build
  time. If Forge gets a behaviour regression it cannot pin or diagnose, in-process wins.
- **Claude Code's system prompt fights Forge.** The harness carries opinions about
  software-engineering workflow. If those conflict with a Yocto build workflow and
  `system_prompt` overrides prove insufficient, owning the prompt outright becomes worth the
  rebuild.
- **Latency or the process boundary becomes a real cost** — e.g. Forge wants many concurrent
  agents per session, each spawning a ~250 MB-image process.
- **Anthropic declines subscription-login approval and API-key-only proves a bad UX**, removing
  option (a)'s remaining onboarding advantage.
- **Forge needs multi-provider support** (the brief hints at swappability). Tool Runner is
  still Anthropic-only; at that point the real answer is (c), a hand-written loop against a
  provider abstraction — and the `Agent` interface above is what makes that reachable.

Conversely, **stay on (a) harder** (adopt subagents, skills, plugins) if Forge's roadmap turns
toward multi-agent build triage, since re-implementing subagent orchestration on (b) is a
large project of its own.

---

## Appendix: reproduction

```bash
python3 -m venv /tmp/forge-research-venv
/tmp/forge-research-venv/bin/pip install claude-agent-sdk anthropic
/tmp/forge-research-venv/bin/python /tmp/t_sdk.py      # tools=[] + custom tool
/tmp/forge-research-venv/bin/python /tmp/t_perm.py     # can_use_tool + streaming + asyncio
/tmp/forge-research-venv/bin/python /tmp/t_hook.py     # PreToolUse capability gate
/tmp/forge-research-venv/bin/python /tmp/t_resume.py   # session resume
```

Live runs used `model="claude-sonnet-5"` to keep cost down; observed `total_cost_usd` ≈ 0.0067
per short run.

## Sources

- Agent SDK overview — https://code.claude.com/docs/en/agent-sdk/overview
- Configure permissions — https://code.claude.com/docs/en/agent-sdk/permissions
- Give Claude custom tools — https://code.claude.com/docs/en/agent-sdk/custom-tools
- Python SDK repo — https://github.com/anthropics/claude-agent-sdk-python
- Anthropic Python SDK repo — https://github.com/anthropics/anthropic-sdk-python
- Local inspection of `claude-agent-sdk` 0.2.145 and `anthropic` 1.1.0 in `/tmp/forge-research-venv`
