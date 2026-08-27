# Research Brief 05 — VERIFY the `claude` CLI as a directly-driven agent backend

You are a verification agent. This is a DE-RISKING SPIKE, not a survey.
Forge has committed to driving the `claude` binary directly from Rust over its
stream-json stdio protocol, with NO Python SDK in between. Three assumptions are
currently unverified and the architecture depends on them. Your job is to prove
or disprove each one by RUNNING IT. Documentation alone is not an answer.

Background you can rely on (already verified by a previous agent):
- `claude-agent-sdk` (Python) is itself just a subprocess wrapper: it resolves a
  `claude` binary, sets `CLAUDE_CODE_ENTRYPOINT=sdk-py`, and speaks
  newline-delimited JSON over stdin/stdout.
- Under the Python SDK these all worked: `tools=[]` removed every built-in tool;
  a `PreToolUse` hook could not be shadowed; `can_use_tool` was async and could
  await a human decision; `setting_sources=[]` stopped `~/.claude/settings.json`
  from being read; `resume=<session_id>` restored a session.
The question is whether the SAME control is reachable from the CLI directly.

System `claude` is 2.1.246 at /usr/bin/claude. Read `claude --help` in full first.

## The three questions that decide the architecture

### Q1 — Can we strip ALL built-in tools from the CLI?
Forge must own 100% of the tool surface: no Bash, Read, Write, Edit, Glob, Grep,
WebSearch, WebFetch. Under the SDK this was `tools=[]`.
- Find the CLI equivalent. Investigate `--allowed-tools`, `--disallowed-tools`,
  `--tools`, `--settings`, and any agent/config file mechanism.
- PROVE it: start a session, and capture the `system`/`init` event from the
  stream-json output, which lists the exact tools the model was given. Paste it.
- If built-ins CANNOT be fully removed, say so plainly and report the minimum
  achievable tool set. This would be a serious finding — report it loudly.

### Q2 — Can Forge's own tools be registered, and can a Rust process serve them?
- Verify `--mcp-config` (inline JSON and/or file). Write a TRIVIAL MCP server
  (any language is fine for the test — the point is the protocol, not the impl)
  exposing one tool, e.g. `forge_proc_start`, register it, and confirm the model
  can call it and receive the result. Paste the transcript.
- Confirm the wire name (`mcp__forge__forge_proc_start` or similar).
- Check `--strict-mcp-config`: does it stop the user's own MCP servers from
  being loaded? Forge must not inherit the user's MCP config.
- Note whether stdio and/or HTTP/SSE MCP transports are supported, since a Rust
  Forge would likely serve tools in-process over one of them.

### Q3 — Can Forge gate EVERY tool call, unshadowably, and route approval to a TUI?
This is the security-critical one.
- Investigate `--permission-prompt-tool` (an MCP tool the CLI calls to ask for
  permission), `--permission-mode`, hooks configured via `--settings`, and any
  `PreToolUse` equivalent reachable from the CLI.
- PROVE the gate fires: make a tool call get DENIED by Forge's own decision
  point, and show the model reasoning about the denial.
- PROVE it cannot be bypassed: deliberately try to shadow it the way the Python
  SDK could be shadowed — bare-name `--allowed-tools` entries, `acceptEdits`,
  `bypassPermissions`, and an allow rule planted in a settings file. Does the
  gate still fire in every case? THIS IS THE KEY TEST.
- Determine how to stop the user's `~/.claude/settings.json` from being read at
  all (the SDK's `setting_sources=[]`). Try `--settings` with an explicit file,
  and look for a CLI equivalent. A Forge user's personal Claude Code allow-rules
  MUST NOT be able to punch a hole in Forge's permission model.
- Can the permission decision be made ASYNCHRONOUSLY (i.e. Forge takes seconds
  to ask a human in a TUI and then answers)? Does the CLI block gracefully?
  Test with a deliberate multi-second delay before answering.

## Also determine (secondary, but needed to write the driver)
4. The exact stream-json protocol: with `--output-format stream-json
   --input-format stream-json --verbose`, document EVERY event type observed and
   its JSON shape. We need: assistant text deltas, thinking, tool_use,
   tool_result, permission requests, result/usage, errors, rate limits.
   Paste real captured JSON lines. This becomes Forge's protocol spec.
5. How to send a follow-up user turn into a running session on stdin (the shape
   of the input message), and how to INTERRUPT a turn in flight.
6. Session resume from the CLI (`--resume`, `--session-id`, `--fork-session`).
7. Auth: how the CLI resolves credentials, and how to pass them EXPLICITLY via
   env rather than inheriting the machine's config (incl. a proxy base URL).
8. Whether `--include-partial-messages` (or equivalent) is needed for deltas.

## Method
- RUN THINGS. Use `--print`/`-p` non-interactive mode with stream-json so it is
  scriptable. Capture raw stdout to files in /tmp and paste real lines.
- Keep model cost low: use `--model claude-sonnet-5` and short prompts.
- Test tool calls with trivial fake tools; do NOT run real destructive commands.
- If a flag does not exist, say so — do not invent it. Mark anything you could
  not execute as UNVERIFIED and explain why.

## Deliverable
Write `/home/luciano/forge/docs/research/05-cli-protocol.md`:
- A verdict table: Q1 / Q2 / Q3 → PROVEN / PARTIAL / DISPROVEN, one line each.
- Real captured evidence for each.
- The full stream-json event catalogue with example lines.
- The exact `claude` argv Forge should spawn, with every flag justified.
- A "gaps and risks" section: anything the Python SDK gives that the CLI does
  not, and what Forge must build itself as a result.
- If Q1 or Q3 is DISPROVEN, say clearly what that means for the architecture.
Concise and technical. No filler. Then stop.
