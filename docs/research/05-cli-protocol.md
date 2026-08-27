# 05 — Driving the `claude` CLI directly as Forge's agent backend

De-risking spike. Every claim below was executed against `claude` **2.1.246**
(`/usr/bin/claude` → `/opt/claude-code/bin/claude`) on Linux, model
`claude-sonnet-5`, on 2026-08-27. Raw captures live in `/tmp/forge-spike/`
(30 `.jsonl` transcripts; total model spend for the spike: **$0.44**).

Anything marked UNVERIFIED could not be executed and says why.

## Verdict table

| # | Question | Verdict |
|---|---|---|
| Q1 | Strip **all** built-in tools from the CLI | **PROVEN** — `--tools ""` yields `"tools": []` in the `system/init` event; MCP tools survive it. |
| Q2 | Register Forge's own tools, served by a Forge process | **PROVEN** — `--mcp-config` (file *and* inline JSON), stdio *and* streamable-HTTP transports, wire name `mcp__forge__forge_proc_start`; `--strict-mcp-config` drops every foreign MCP server. |
| Q3 | Gate every tool call, unshadowably, async, routed to a TUI | **PROVEN, but only with the exact flag set below.** The permission-prompt surface on its own **is shadowable** by allow-rules (`--allowed-tools`, any settings file, `--permission-mode bypassPermissions`). A `PreToolUse` hook returning `"ask"` is **not** shadowable and forces the prompt through even against `--allowed-tools` + `bypassPermissions` combined. Async proven to 123 s with no timeout. |

The headline risk is inside Q3 and is fully mitigable: **Forge must pass
`--setting-sources ""`, must never pass `--allowed-tools`, and should install a
`PreToolUse` `ask` hook as an unshadowable backstop.** Details in §3.

---

## Q1 — Stripping the built-in tool surface

`--tools <tools...>` exists (it is *not* in the "commonly used" part of the help
but is real): *"Specify the list of available tools from the built-in set. Use
`""` to disable all tools, `default` to use all tools, or specify tool names."*

Baseline, no flags — 24 built-ins:

```
"tools":["Task","Bash","CronCreate","CronDelete","CronList","DesignSync","Edit",
"EnterWorktree","ExitWorktree","ListAgents","Monitor","NotebookEdit",
"PushNotification","Read","ReportFindings","ScheduleWakeup","SendMessage",
"Skill","TaskOutput","TaskStop","WebFetch","WebSearch","Workflow","Write"]
```

With `--tools ""`:

```
$ claude -p --tools "" --setting-sources "" --strict-mcp-config --disable-slash-commands ...
TOOLS: []
MCP: []
permissionMode: default
n_slash: 0 n_skills: 0 n_agents: 5
```

Verified facts:

* `--tools ""` removes **every** built-in, including `Bash`, `Read`, `Write`,
  `Edit`, `WebSearch`, `WebFetch`, and `Task`.
* MCP tools are a **separate** surface and are unaffected — with
  `--tools "" --mcp-config <forge>` the init event lists *only*
  `["mcp__forge__forge_proc_start"]`. This is exactly the Forge model: 0 %
  Anthropic tool surface, 100 % Forge tool surface.
* Skills/slash-commands are a third surface. `--setting-sources ""` alone drops
  them from 33 → 15 (user settings stop being read, plugin-supplied ones
  remain); `--disable-slash-commands` takes them to **0**.
* `agents: ["claude","Explore","general-purpose","Plan","statusline-setup"]`
  still appears in `init` even at full lockdown. These are unreachable in
  practice because the `Task` tool that spawns them is gone. Forge should still
  treat the field as informational only.

No finding here is negative. Q1 is clean.

---

## Q2 — Registering Forge's tools

A ~90-line Python stdio MCP server (`/tmp/forge-spike/mcp_forge.py`) exposing
`forge_proc_start` was used as the stand-in for Forge's in-process tool host.

### Registration

Both forms work:

```bash
--mcp-config /tmp/forge-spike/mcp.json
--mcp-config '{"mcpServers":{"forge":{"type":"stdio","command":"python3","args":["…/mcp_forge.py"]}}}'
```

`init` reports `"mcp_servers":[{"name":"forge","status":"connected"}]`. A server
that fails to spawn reports `"status":"failed"` and simply contributes no tools —
Forge can detect a broken tool host from the init event alone.

### Wire name and end-to-end call (stdio transport)

```
0.57 system/init tools=['mcp__forge__forge_proc_start'] mcp=[{'name':'forge','status':'connected'}]
4.31 assistant.tool_use: mcp__forge__forge_proc_start id=toolu_01H7PJRM698Bd2cVVc1rRfpr input={"cmd": "sleep 1"}
4.35 user.tool_result id=toolu_01H7PJRM698Bd2cVVc1rRfpr is_error=None
     content=[{"type":"text","text":"{\"pid\": 4242, \"state\": \"running\", \"cmd\": \"sleep 1\", \"note\": \"FORGE-PROC-OK-4242\"}"}]
5.97 assistant.text: The PID is **4242**.
```

Wire name confirmed: **`mcp__<serverName>__<toolName>`** →
`mcp__forge__forge_proc_start`.

The JSON-RPC the CLI actually sends (captured server-side):

```json
{"method":"tools/call","params":{"name":"forge_proc_start",
 "arguments":{"cmd":"sleep 1"},
 "_meta":{"claudecode/toolUseId":"toolu_01H7PJRM698Bd2cVVc1rRfpr","progressToken":2}},
 "jsonrpc":"2.0","id":2}
```

Note `_meta["claudecode/toolUseId"]` — Forge gets the `tool_use_id` **inside the
tool call itself**, so a Rust tool host can correlate a running process with the
transcript entry without any side channel.

### Transports

`claude mcp add -t <transport>` accepts **stdio, sse, http**. Both stdio and
streamable-HTTP were proven end to end. HTTP:

```
0.59 system/init tools=['mcp__forgehttp__forge_proc_start'] mcp=[{'name':'forgehttp','status':'connected'}]
3.20 assistant.tool_use: mcp__forgehttp__forge_proc_start input={"cmd": "true"}
3.23 user.tool_result content=[{"type":"text","text":"{\"pid\": 7777, \"cmd\": \"true\", \"note\": \"FORGE-HTTP-OK-7777\"}"}]
4.50 assistant.text: The note field is: `FORGE-HTTP-OK-7777`
```

The HTTP server was a 40-line `http.server` returning the JSON-RPC reply as a
single SSE frame (`Content-Type: text/event-stream`, `event: message\ndata: {…}`)
with an `Mcp-Session-Id` header. **Recommendation: Forge serves its tools over
streamable HTTP on `127.0.0.1`.** It avoids a second process, keeps the tool host
inside the Rust binary, and survives `claude` restarts (resume) without
re-plumbing pipes.

### `--strict-mcp-config`

A foreign server was planted as a project-scope `.mcp.json` in the cwd:

```
without --strict-mcp-config:
  TOOLS: [mcp__forge__forge_proc_start, mcp__userland__forge_proc_start]
  MCP:   [{"name":"userland","status":"connected"},{"name":"forge","status":"connected"}]

with --strict-mcp-config:
  TOOLS: [mcp__forge__forge_proc_start]
  MCP:   [{"name":"forge","status":"connected"}]
```

`--strict-mcp-config` is **mandatory** for Forge. Without it the user's own MCP
servers are silently merged into Forge's tool surface.

---

## Q3 — The permission gate (security-critical)

### Two surfaces exist, and they are not equivalent

**(a) The permission-prompt surface.** `--permission-prompt-tool` takes either an
MCP tool name *or* the literal string **`stdio`**.

* `--permission-prompt-tool mcp__forge__forge_approve` — the CLI calls that MCP
  tool for a decision. Note it is **removed from the model's own tool list**
  (init shows only `forge_proc_start`), so the model cannot self-approve.
* `--permission-prompt-tool stdio` — **this is the SDK's `canUseTool`, reachable
  from the plain CLI.** The CLI emits a `control_request` on stdout and blocks
  for a `control_response` on stdin. No MCP server needed for permissions.

**(b) The `PreToolUse` hook surface**, configured through `--settings`.

### Proof the gate fires and denies (stdio flavour)

```
 3.18 <<< tool_use mcp__forge__forge_proc_start {"cmd": "sleep 1"}
 3.19 <<< CONTROL_REQUEST: {"type":"control_request","request_id":"bb023cad-445b-4632-8ed4-fd94bd83bce2",
          "request":{"subtype":"can_use_tool","tool_name":"mcp__forge__forge_proc_start",
          "display_name":"Forge Proc Start","input":{"cmd":"sleep 1"},
          "permission_suggestions":[{"type":"addRules","rules":[{"toolName":"mcp__forge__forge_proc_start"}],
                                     "behavior":"allow","destination":"localSettings"}],
          "tool_use_id":"toolu_01SYPjVvyaZobE83nw9vo9tC"}}
 3.19 >>> {"type":"control_response","response":{"subtype":"success",
          "request_id":"bb023cad-445b-4632-8ed4-fd94bd83bce2",
          "response":{"behavior":"deny","message":"Forge TUI (stdio gate): operator DENIED."}}}
 3.19 <<< tool_result err=True Forge TUI (stdio gate): operator DENIED.
 4.68 <<< text: The call failed because the operator explicitly denied permission via
          the Forge TUI's stdio gate—not due to a technical error with the command itself.
 4.69 <<< result/success denials=[{"tool_name":"mcp__forge__forge_proc_start",
          "tool_use_id":"toolu_01SYPjVvyaZobE83nw9vo9tC","tool_input":{"cmd":"sleep 1"}}]
```

The model receives the denial as an errored `tool_result` **carrying Forge's own
message**, and reasons about it correctly. Denials are also summarised in the
`result` event's `permission_denials[]`.

`updatedInput` is honoured — answering `{"behavior":"allow","updatedInput":
{"cmd":"REWRITTEN-BY-FORGE"}}` made the MCP server actually receive
`{"arguments":{"cmd":"REWRITTEN-BY-FORGE"}}`. Forge can rewrite or clamp
arguments at the gate, not merely allow/deny.

### THE KEY TEST — shadowing

Every row below ran with Forge's gate hard-wired to **deny**. "Gate fired" means
Forge's decision point was actually consulted.

| Attempt | Gate consulted? | Outcome |
|---|---|---|
| baseline (`--permission-prompt-tool`, nothing else) | yes | **DENIED** ✅ |
| `--allowed-tools "mcp__forge__forge_proc_start"` | **no** | **ALLOWED — BYPASSED** ❌ |
| `--allowed-tools "mcp__forge"` (bare/prefix form) | **no** | **ALLOWED — BYPASSED** ❌ |
| `--settings` file with `permissions.allow:[…]` only | **no** | **ALLOWED — BYPASSED** ❌ |
| `--permission-mode acceptEdits` | yes | **DENIED** ✅ |
| `--permission-mode bypassPermissions` | **no** | **ALLOWED — BYPASSED** ❌ |
| `--permission-mode dontAsk` | no | denied by the CLI itself (fail-closed, but not Forge's decision) |
| project `.claude/settings.json` allow-rule, default sources | **no** | **ALLOWED — BYPASSED** ❌ |
| project `.claude/settings.json` allow-rule + `--setting-sources ""` | yes | **DENIED** ✅ |
| **user `~/.claude/settings.json`** allow-rule, default sources | **no** | **ALLOWED — BYPASSED** ❌ |
| **user `~/.claude/settings.json`** allow-rule + `--setting-sources ""` | yes | **DENIED** ✅ |
| `PreToolUse` hook deny + `--allowed-tools <tool>` | yes (hook) | **DENIED** ✅ |
| `PreToolUse` hook deny + `--permission-mode bypassPermissions` | yes (hook) | **DENIED** ✅ |
| `PreToolUse` hook deny + settings allow-rule | yes (hook) | **DENIED** ✅ |
| **`PreToolUse` hook `ask` + `--allowed-tools <tool>` + `bypassPermissions`** | **yes (hook → stdio gate)** | **DENIED** ✅ |

The user-settings test was run by temporarily planting
`{"permissions":{"allow":["mcp__forge__forge_proc_start"],"defaultMode":"bypassPermissions"}}`
into the real `~/.claude/settings.json` and restoring it afterwards (`diff`
verified clean).

Three conclusions:

1. **Allow-rules from any source shadow the permission-prompt surface.** This is
   the same weakness the Python SDK had. It is not a CLI regression, but it is
   real and it must be designed around.
2. **`--setting-sources ""` closes the user/project/local settings hole
   completely.** Proven twice, at project and user scope. This is the CLI
   equivalent of the SDK's `setting_sources=[]`. Note that `--settings <file>`
   is *not* blocked by it — but that file is supplied by Forge itself, so that is
   correct behaviour, not a hole.
3. **A `PreToolUse` hook cannot be shadowed.** A hook returning
   `permissionDecision: "deny"` beats `--allowed-tools`, beats settings
   allow-rules, and beats `--permission-mode bypassPermissions`. A hook returning
   `permissionDecision: "ask"` **forces the `can_use_tool` prompt through even
   when `--allowed-tools` and `bypassPermissions` are both set**:

```
 3.29 <<< CONTROL_REQUEST: {"type":"control_request","request_id":"1588b3dc-…",
          "request":{"subtype":"can_use_tool","tool_name":"mcp__forge__forge_proc_start",
          "input":{"cmd":"sleep 1"},
          "decision_reason":"Forge policy: every tool call must be confirmed by the operator.",
          "decision_reason_type":"hook","tool_use_id":"toolu_01THD26kycpn1aZWiRqxssTw"}}
 3.30 <<< tool_result err=True Forge TUI (stdio gate): operator DENIED.
```

That combination — hook forces `ask`, stdio gate answers — is the unshadowable,
async, TUI-routable gate Forge needs.

Hook stdin payload (what Forge's hook binary receives):

```json
{"session_id":"bc7a5015-…","transcript_path":"/home/…/…jsonl","cwd":"/tmp/forge-spike",
 "prompt_id":"2dc4673c-…","permission_mode":"bypassPermissions","effort":{"level":"high"},
 "hook_event_name":"PreToolUse","tool_name":"mcp__forge__forge_proc_start",
 "tool_input":{"cmd":"sleep 1"},"tool_use_id":"toolu_01QkLbkK3ZrFgVaVDbeEnkSk"}
```

Hook stdout contract:

```json
{"hookSpecificOutput":{"hookEventName":"PreToolUse",
 "permissionDecision":"allow|deny|ask",
 "permissionDecisionReason":"…"}}
```

### Asynchronous decisions

Proven three times, no timeout, no error, no degradation:

| Gate | Delay before answering | Result |
|---|---|---|
| MCP permission tool | 8.0 s | tool_use at t=3.28 → tool_result at t=11.30, allowed |
| MCP permission tool | **150 s** | tool_result at t=153.05, `result/success` |
| stdio `can_use_tool` | **120 s** | `control_response` at t=123.11 → tool_result at t=123.12, `result/success` |
| `PreToolUse` hook (no explicit `timeout`) | **90 s** | hook answered at +90 s, deny honoured |

The CLI blocks the turn gracefully for at least 150 s. A human answering in a TUI
is well inside that. The exact ceiling is UNVERIFIED (not probed past 150 s);
hooks accept an explicit `"timeout": <seconds>` field and Forge should set it
generously (e.g. 3600) rather than rely on the default.

### Interrupt while a permission is pending

```
 3.19 <<< can_use_tool pending id=43e79344-635f-489c-be75-70a155f74a7f
 7.19 >>> {"type":"control_request","request_id":"forge-int-2","request":{"subtype":"interrupt"}}
 7.19 <<< control_cancel_request {"type":"control_cancel_request","request_id":"43e79344-…"}
 7.19 <<< control_response {"type":"control_response","response":{"subtype":"success",
          "request_id":"forge-int-2","response":{"still_queued":[]}}}
 7.19 <<< result/error_during_execution terminal=aborted_tools
```

The CLI emits `control_cancel_request` naming the pending permission request, so
Forge's TUI knows to dismiss the dialog. Forge must handle this — otherwise a
stale approval dialog outlives its turn.

### Fail-closed default

With no `--permission-prompt-tool` and no allow-rules, an MCP tool call is
refused rather than run:

```
3.13 system/permission_denied
3.13 user.tool_result is_error=True
     "Claude requested permissions to use mcp__forge__forge_proc_start, but you haven't granted it yet."
```

Good default. Forge is never one missing flag away from an ungated tool call —
but it *is* one stray `--allowed-tools` away, hence the hook backstop.

---

## 4. stream-json protocol catalogue

Captured with `--output-format stream-json --input-format stream-json --verbose
--include-partial-messages --include-hook-events --replay-user-messages`.
Census from one tool-calling turn:

```
   1  system/init                        1  system/hook_started
   2  system/status                      1  system/hook_response
   2  system/thinking_tokens             2  user  (replay + tool_result)
   2  stream_event/message_start         3  assistant (thinking, tool_use, text)
   1  content_block_start/thinking       3  content_block_stop
   2  content_block_delta/thinking_delta 1  content_block_start/tool_use
   1  content_block_delta/signature_delta 4 content_block_delta/input_json_delta
   1  content_block_start/text           2  stream_event/message_delta
   2  content_block_delta/text_delta     2  stream_event/message_stop
   1  result/success
```

Every line is a complete JSON object terminated by `\n`. Every event carries
`session_id`; most carry a `uuid`; assistant events carry `parent_tool_use_id`
(non-null only for subagent output, and only with `--forward-subagent-text`).

### `system/init` — **emitted at the start of EVERY turn, not once per process**

Confirmed in the multi-turn test: three user turns produced three `init` events
on one process. Forge must treat it as a turn marker, not a handshake.

```json
{"type":"system","subtype":"init","cwd":"/tmp/forge-spike",
 "session_id":"ef8cfed2-ba7f-48d4-8a68-22b614bff346",
 "tools":["mcp__forge__forge_proc_start"],
 "mcp_servers":[{"name":"forge","status":"connected"}],
 "model":"claude-sonnet-5","permissionMode":"default","slash_commands":[],
 "apiKeySource":"/login managed key","claude_code_version":"2.1.246",
 "output_style":"default","agents":["claude","Explore","general-purpose","Plan","statusline-setup"],
 "skills":[],"plugins":[],
 "capabilities":["interrupt_receipt_v1","interrupt_cancel_queued_v1","msg_lifecycle_v1"],
 "analytics_disabled":false,"product_feedback_disabled":false,
 "uuid":"44339f19-…","memory_paths":{"auto":"/home/…/memory/"},
 "messaging_socket_path":"/run/user/1000/cc-socks/2171326.sock",
 "fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required"}
```

### `system/status`

```json
{"type":"system","subtype":"status","status":"requesting","uuid":"791f57a9-…","session_id":"ef8cfed2-…"}
```

### `system/thinking_tokens`

```json
{"type":"system","subtype":"thinking_tokens","estimated_tokens":50,
 "estimated_tokens_delta":50,"uuid":"a88d9c6c-…","session_id":"ef8cfed2-…"}
```

### `system/permission_denied`

Emitted on the CLI's own short-circuit deny (no gate configured, or `dontAsk`).

### `system/hook_started` / `system/hook_response` (needs `--include-hook-events`)

```json
{"type":"system","subtype":"hook_started","hook_id":"9ff6116b-…",
 "hook_name":"PreToolUse:mcp__forge__forge_proc_start","hook_event":"PreToolUse",
 "uuid":"24516d5d-…","session_id":"ef8cfed2-…"}
```
```json
{"type":"system","subtype":"hook_response","hook_id":"9ff6116b-…",
 "hook_name":"PreToolUse:mcp__forge__forge_proc_start","hook_event":"PreToolUse",
 "output":"{\"hookSpecificOutput\":{…\"permissionDecision\":\"allow\"…}}\n",
 "stdout":"…","stderr":"","exit_code":0,"outcome":"success",
 "uuid":"62d4468e-…","session_id":"ef8cfed2-…"}
```

### `assistant` — thinking

```json
{"type":"assistant","message":{"model":"claude-sonnet-5","id":"msg_011CeTSqGjJ7h5NF1q5cKMAh",
 "type":"message","role":"assistant",
 "content":[{"type":"thinking","thinking":"","signature":"EqYCCqgBCBEYAipAt2Mbsh…GAE="}],
 "stop_reason":null,"stop_sequence":null,"stop_details":null,
 "usage":{"input_tokens":2,"cache_creation_input_tokens":199,"cache_read_input_tokens":8776,
  "cache_creation":{"ephemeral_5m_input_tokens":199,"ephemeral_1h_input_tokens":0},
  "output_tokens":2,"service_tier":"standard","inference_geo":"not_available"},
 "context_management":null},
 "parent_tool_use_id":null,"session_id":"ef8cfed2-…","uuid":"1eec5a17-…",
 "timestamp":"2026-08-27T14:55:11.680Z","request_id":"req_011CeTSqFNwKqzV4zXE8uhsC"}
```

### `assistant` — tool_use

```json
{"type":"assistant","message":{…,"content":[{"type":"tool_use",
  "id":"toolu_01BZ2898aEic7Lb8xWStkcqQ","name":"mcp__forge__forge_proc_start",
  "input":{"cmd":"echo hi"},"caller":{"type":"direct"}}],…},
 "tool_use_meta":[{"id":"toolu_01BZ2898aEic7Lb8xWStkcqQ",
   "display_name":"Forge Proc Start","server_display_name":"forge"}]}
```

`tool_use_meta` gives Forge the human-facing label without a lookup table.

### `assistant` — text

```json
{"type":"assistant","message":{…,"content":[{"type":"text","text":"PID: 4242"}],…},
 "request_id":"req_011CeTSqRV9jeW7chu7Ldye5"}
```

### `user` — tool_result (the CLI echoes it back)

```json
{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01BZ2898aEic7Lb8xWStkcqQ",
 "type":"tool_result","content":[{"type":"text","text":"{\"pid\": 4242, …}"}]}]},
 "parent_tool_use_id":null,"session_id":"ef8cfed2-…","uuid":"e23238a8-…",
 "timestamp":"2026-08-27T14:55:11.727Z",
 "tool_use_result":[{"type":"text","text":"{\"pid\": 4242, …}"}]}
```

Denials arrive as the same shape with `"is_error":true` and Forge's message as
the content.

### `user` — replayed input (`--replay-user-messages`)

```json
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Call forge_proc_start…"}]},
 "session_id":"ef8cfed2-…","parent_tool_use_id":null,"uuid":"d7e088e3-…",
 "timestamp":"2026-08-27T14:55:08.927Z","isReplay":true}
```

`isReplay: true` is the acknowledgement that stdin was consumed. Useful for
backpressure; skip these when rendering.

### `stream_event` (needs `--include-partial-messages`)

Without the flag, **no `stream_event` lines are produced at all** — the baseline
run emitted exactly 3 lines (`init`, `assistant`, `result`). **Q8 answer: yes,
`--include-partial-messages` is required for deltas.**

```json
{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-sonnet-5",
 "id":"msg_011CeTSqGjJ7h5NF1q5cKMAh","type":"message","role":"assistant","content":[],
 "stop_reason":null,"usage":{…}}},"session_id":"ef8cfed2-…","parent_tool_use_id":null,
 "uuid":"3a39a805-…","ttft_ms":1525}
```
```json
{"type":"stream_event","event":{"type":"content_block_start","index":0,
 "content_block":{"type":"thinking","thinking":"","signature":""}},…}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,
 "delta":{"type":"thinking_delta","thinking":"","estimated_tokens":50}},…}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,
 "delta":{"type":"signature_delta","signature":"EqYCCqgB…"}},…}
{"type":"stream_event","event":{"type":"content_block_start","index":1,
 "content_block":{"type":"tool_use","id":"toolu_01BZ…","name":"mcp__forge__forge_proc_start",
                  "input":{},"caller":{"type":"direct"}}},…}
{"type":"stream_event","event":{"type":"content_block_delta","index":1,
 "delta":{"type":"input_json_delta","partial_json":""}},…}
{"type":"stream_event","event":{"type":"content_block_start","index":0,
 "content_block":{"type":"text","text":""}},…}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,
 "delta":{"type":"text_delta","text":"P"}},…}
{"type":"stream_event","event":{"type":"content_block_stop","index":0},…}
{"type":"stream_event","event":{"type":"message_delta",
 "delta":{"stop_reason":"tool_use","stop_sequence":null,"stop_details":null},
 "usage":{"input_tokens":2,"output_tokens":76,"output_tokens_details":{"thinking_tokens":12},…},
 "context_management":{"applied_edits":[]}},…}
{"type":"stream_event","event":{"type":"message_stop"},…}
```

The `stream_event` frames are raw Anthropic Messages API SSE events wrapped with
`session_id`/`uuid`. The non-`stream_event` `assistant` lines are the *coalesced*
version of the same content — Forge must render one or the other, not both.

### `control_request` / `control_response` / `control_cancel_request`

See §Q3. Subtypes proven live: `can_use_tool` (CLI → Forge) and `interrupt`
(Forge → CLI). The binary also contains `set_permission_mode`, `set_model`,
`hook_callback`, `mcp_message`, and `control_cancel_request` — UNVERIFIED, not
exercised.

### `result` — success

```json
{"type":"result","subtype":"success","is_error":false,
 "duration_ms":6069,"duration_api_ms":6069,"num_turns":2,"stop_reason":"end_turn",
 "session_id":"ef8cfed2-…","total_cost_usd":0.0077492,
 "usage":{"input_tokens":4,"cache_creation_input_tokens":326,"cache_read_input_tokens":17751,
  "output_tokens":84,"output_tokens_details":{"thinking_tokens":12},
  "server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},
  "service_tier":"standard","cache_creation":{…},"inference_geo":"not_available",
  "iterations":[{…}],"speed":"standard"},
 "modelUsage":{"claude-sonnet-5":{"inputTokens":1192,"outputTokens":100,
  "cacheReadInputTokens":17751,"cacheCreationInputTokens":326,"costUSD":0.0077492,
  "contextWindow":200000,"maxOutputTokens":64000,"canonicalModel":"claude-sonnet-5",
  "provider":"firstParty","costBasis":"list"}},
 "permission_denials":[],"terminal_reason":"completed",
 "result":"PID: 4242","ttft_ms":…,"queued_turn_count":0,
 "subagent_stats":{…},"api_error_status":null}
```

### `result` — error variants (all captured live)

| subtype | `terminal_reason` | trigger |
|---|---|---|
| `error_during_execution` | `aborted_streaming` | interrupt during text streaming |
| `error_during_execution` | `aborted_tools` | interrupt while a permission was pending |
| `error_during_execution` | — | `--resume <unknown-uuid>` (also prints `No conversation found with session ID: …` to stdout **before** the JSON — Forge must tolerate non-JSON lines) |
| `error_max_budget_usd` | `budget_exhausted` | `--max-budget-usd 0.0001` |

`error_max_turns` exists in the binary but was not triggered — UNVERIFIED.

### Not observed — declare as TODO in the driver

* `{"type":"rate_limit_event","rate_limit_info":{…,"status":"allowed|allowed_warning|rejected",
  "resets_at":…},"uuid":…,"session_id":…}` — present in the binary's schema, could
  not be triggered (would require actually hitting a rate limit). **UNVERIFIED.**
  Forge's driver must not crash on an unknown top-level `type`.
* `system/compact_boundary` (with `compact_metadata`) — auto-compaction. **UNVERIFIED.**
* `prompt_suggestion` (`--prompt-suggestions`), `system/session_end`,
  `tool_progress` — present in the binary, not exercised. **UNVERIFIED.**

**Driver rule: unknown `type` values must be logged and skipped, never fatal.**

---

## 5. Sending follow-up turns, and interrupting

Follow-up user turn — just write another line to stdin on the *same* process:

```json
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"…"}]}}
```

Proven to carry conversation state within one process:

```
  0.00 >>> user "Reply with exactly the word BANANA and nothing else."
  2.54 <<< text: BANANA
  2.55 <<< result/success stop=end_turn turns=1 sid=11111111-2222-3333-4444-555555555555
  2.55 >>> user "What word did you just reply with? One word answer."
  2.56 <<< system/init                      ← init repeats per turn
  4.23 <<< text: BANANA
  4.26 <<< result/success stop=end_turn turns=1 sid=11111111-…
```

Interrupt a turn in flight:

```json
{"type":"control_request","request_id":"forge-int-1","request":{"subtype":"interrupt"}}
```
```
  8.59 <<< control_response {"type":"control_response","response":{"subtype":"success",
          "request_id":"forge-int-1","response":{"still_queued":[]}}}
  8.59 <<< result/error_during_execution   terminal_reason="aborted_streaming"
```

`still_queued` lists user turns that were queued and cancelled
(`interrupt_cancel_queued_v1` in `init.capabilities`).

---

## 6. Session resume

`--session-id <uuid>` is honoured verbatim — the whole three-turn session above
ran under `11111111-2222-3333-4444-555555555555`.

```
resume  : claude … --resume 11111111-2222-3333-4444-555555555555
          → init session_id = 11111111-2222-3333-4444-555555555555 ; answered "BANANA"
fork    : claude … --resume 11111111-… --fork-session
          → init session_id = 38632649-57e7-4df4-849a-808d87658222 ; answered "BANANA"
```

Resume restores full conversation state across a **new process**. `--fork-session`
branches to a fresh id while keeping the history — the natural primitive for
Forge's "explore an alternative from here".

`--no-session-persistence` disables on-disk sessions entirely (and therefore
resume). Sessions are stored under `$CLAUDE_CONFIG_DIR/projects/<slug>/<uuid>.jsonl`.

---

## 7. Auth

`init.apiKeySource` reports which path was taken. Verified with a completely
scrubbed environment (`env -i`, only `PATH`, `HOME`, and two Anthropic vars):

```
$ env -i PATH=/usr/bin:/bin HOME=$HOME \
    ANTHROPIC_BASE_URL=https://<proxy-host> ANTHROPIC_API_KEY=<token> \
    claude -p "Say PONG" --model claude-sonnet-5 --output-format stream-json --verbose \
      --bare --tools "" --setting-sources "" --strict-mcp-config

init apiKeySource= ANTHROPIC_API_KEY tools= []
text: ['PONG']
result: success PONG
```

* **A proxy base URL works** — the whole spike ran through one
  (`ANTHROPIC_BASE_URL` pointed at an internal gateway).
* `--bare` is the strongest guarantee: *"Anthropic auth is strictly
  `ANTHROPIC_API_KEY` or `apiKeyHelper` via `--settings` (OAuth and keychain are
  never read)"*. Without it, `apiKeySource` reported `"/login managed key"`,
  i.e. the machine's stored credentials were used.
* `CLAUDE_CONFIG_DIR` relocates the entire config/session tree. Setting it to a
  Forge-owned directory means the user's `~/.claude` is never read *or written*.
  Verified: a fresh dir got `projects/`, `sessions/`, `backups/`, `.claude.json`
  and `apiKeySource` became `none` (no inherited credentials).
* Other auth-relevant env vars present in the binary:
  `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_CUSTOM_HEADERS`, `ANTHROPIC_ORGANIZATION_ID`,
  `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`,
  `AWS_BEARER_TOKEN_BEDROCK`, `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU,FABLE}_MODEL`,
  `ANTHROPIC_MODEL`, `ANTHROPIC_UNIX_SOCKET`.

**Forge should spawn `claude` with an explicit, minimal env — never the user's.**

---

## The argv Forge should spawn

```
claude -p
  --model <model>                       # explicit; never inherit the user's default
  --input-format  stream-json           # Forge writes turns + control_responses on stdin
  --output-format stream-json           # newline-delimited JSON on stdout
  --verbose                             # required for stream-json to emit anything useful
  --include-partial-messages            # REQUIRED for token deltas (§4)
  --replay-user-messages                # stdin acknowledgement (isReplay:true) → backpressure
  --include-hook-events                 # observability of Forge's own gate hook

  --tools ""                            # Q1: removes ALL 24 built-ins
  --mcp-config '{"mcpServers":{"forge":{"type":"http","url":"http://127.0.0.1:<port>/mcp"}}}'
                                        # Q2: Forge's tools, served in-process by the Rust binary
  --strict-mcp-config                   # Q2: refuse the user's MCP servers
  --disable-slash-commands              # kills the skills surface (33 → 0)

  --setting-sources ""                  # Q3: MANDATORY. blocks user/project/local settings,
                                        #     which can otherwise plant allow-rules or
                                        #     defaultMode:bypassPermissions and void the gate
  --permission-prompt-tool stdio        # Q3: SDK canUseTool over the stdio control protocol
  --settings '{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[
       {"type":"command","command":"<forge-hook-binary>","timeout":3600}]}]}}'
                                        # Q3: unshadowable backstop — returns "ask", which
                                        #     forces the stdio gate even against allow-rules

  --session-id <uuid>                   # Forge owns session identity
  --system-prompt <…>                   # or --append-system-prompt to keep CC's default
```

Never pass, under any circumstance:

| Flag | Why |
|---|---|
| `--allowed-tools` | shadows the permission gate (proven) |
| `--permission-mode bypassPermissions` | shadows the permission gate (proven) |
| `--permission-mode acceptEdits` | harmless today (gate still fired) but meaningless with no built-in edit tools |
| `--dangerously-skip-permissions`, `--allow-dangerously-skip-permissions` | obvious |

Environment (explicit, minimal — do **not** inherit the user's):

```
PATH=<minimal>
HOME=<user home>                       # still needed for temp/XDG resolution
CLAUDE_CONFIG_DIR=<forge-owned dir>    # user's ~/.claude never touched
CLAUDE_CODE_ENTRYPOINT=sdk-rs          # telemetry hygiene (mirrors the SDK's sdk-py)
ANTHROPIC_BASE_URL=<proxy or api.anthropic.com>
ANTHROPIC_API_KEY=<token>
```

Add `--bare` if Forge wants a hard guarantee that OAuth/keychain are never
consulted. It also skips CLAUDE.md auto-discovery, LSP, plugin sync and
background prefetches — all things Forge does not want. The only cost is that
Forge must then supply context explicitly via `--system-prompt` / `--add-dir`,
which it should be doing anyway.

### Proof the whole stack runs together

Full recommended argv, scrubbed env, Forge-owned config dir, HTTP tool host,
hook forcing `ask`, stdio gate answering after a 2 s "human" delay:

```
 0.88 init tools=['mcp__forge__forge_proc_start'] mcp=[{'name':'forge','status':'connected'}]
      mode=default skills=0 auth=ANTHROPIC_API_KEY sid=66acf812-ecce-4d2e-803e-2199869e91e4
 2.93 GATE ask tool=mcp__forge__forge_proc_start reason_type=hook input={"cmd": "echo hi"}
 4.93 GATE allow sent
 4.96 tool_result err=None [{'type':'text','text':'{"pid": 7777, "cmd": "echo hi", "note": "FORGE-HTTP-OK-7777"}'}]
 6.71 text: The process started successfully. …**note**: `FORGE-HTTP-OK-7777`
 6.74 result/success cost=$0.0067 stream_events=21
```

Zero built-in tools. Zero skills. Zero user config. One Forge tool. Every call
gated. Streaming deltas intact.

---

## Gaps and risks

**Security**

1. **Allow-rules shadow the permission-prompt surface.** Mitigated by
   `--setting-sources ""` + never passing `--allowed-tools` + the `ask` hook.
   The residual risk is a *code* regression in Forge (someone adds
   `--allowed-tools` for convenience). **Forge should assert on its own argv
   before spawn** and refuse to start if a forbidden flag is present.
2. **Enterprise managed/policy settings.** `--safe-mode`'s help text states
   *"Admin-managed (policy) settings still apply"*. No policy file exists on this
   machine (`/etc/claude-code/` absent), so whether a managed
   `permissions.allow` can override `--setting-sources ""` and shadow the gate is
   **UNVERIFIED**. On a corporate machine this is a real hole. Forge should
   detect the policy file's presence at startup and warn loudly; the `ask` hook
   is the mitigation if it turns out to be exploitable.
3. **The hook is a subprocess.** It spawns per tool call, so Forge must ship a
   tiny hook binary that round-trips the decision to the Forge process over a
   unix socket. Cost: one `fork`/`exec` per tool call plus IPC. Measured
   end-to-end hook overhead in this spike was ~10 ms for a shell script. Consider
   the hook a *policy* channel (always `ask`) and keep the real decision on the
   stdio control channel, which is already in-process.
4. `--permission-prompt-tool` as an *MCP tool* hides that tool from the model
   (verified). If Forge ever exposes a permission tool by another route, the model
   could call it. Prefer `stdio`.

**Protocol**

5. **`system/init` repeats every turn.** A driver that treats it as a one-time
   handshake will mis-frame turn boundaries.
6. **Non-JSON lines on stdout.** `--resume <unknown>` printed a bare English
   sentence before the JSON `result`. The line reader must tolerate garbage.
7. **Unknown event types.** `rate_limit_event`, `compact_boundary`,
   `prompt_suggestion`, `tool_progress`, `session_end` were not observed live.
   Skip-and-log, never fatal.
8. **Coalesced vs delta duplication.** `assistant` and `stream_event` carry the
   same content. Pick one rendering path.
9. **Async gate ceiling unknown.** Proven to 150 s; not probed further. Hooks take
   an explicit `timeout`; the `can_use_tool` control request appears to have none,
   but that is inference, not proof.

**What Forge must build itself**

10. A newline-delimited JSON codec with the control-protocol request/response
    correlation (`request_id`), including handling `control_cancel_request`.
11. A permission-decision router: `can_use_tool` → TUI dialog → answer, with
    cancellation, and `updatedInput` support for argument rewriting.
12. The hook binary + socket, and argv self-assertion (risk 1).
13. Backpressure/turn-queue management on stdin (the CLI queues turns and reports
    `still_queued` on interrupt).
14. Cost/usage accounting from `result.modelUsage` — the SDK gave no more than
    this, so no gap.

**What the Python SDK gives that the CLI does not**

Nothing material. Every SDK capability the previous investigation verified has a
proven CLI equivalent:

| SDK | CLI |
|---|---|
| `tools=[]` | `--tools ""` |
| `can_use_tool` (async) | `--permission-prompt-tool stdio` + `control_request/can_use_tool` |
| unshadowable `PreToolUse` hook | `--settings '{"hooks":{"PreToolUse":…}}'` — and it beats `--allowed-tools` *and* `bypassPermissions` |
| `setting_sources=[]` | `--setting-sources ""` |
| `resume=<session_id>` | `--resume` / `--fork-session` / `--session-id` |
| MCP servers | `--mcp-config` + `--strict-mcp-config` |

The SDK's only remaining advantage is ergonomic: typed message classes and
automatic control-protocol plumbing. Both are a few hundred lines of Rust.

**Decision: the no-SDK, drive-the-binary architecture holds. Proceed.**
