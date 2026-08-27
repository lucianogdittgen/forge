# Research Brief 01 — Claude integration architecture for Forge

You are a research agent. Investigate ONLY. Do not build the app.

## Context
Forge is a terminal-based AI development workbench (TUI) for developers working on
large builds (Yocto/BitBake). It must embed Claude as a coding agent, BUT:
- Forge owns its own TUI. It must NOT scrape or wrap Claude Code's terminal UI.
- Forge owns its own PTY/process manager. Long-running processes (bitbake) are
  started by Forge and stream live to a terminal pane the user watches.
- The agent must be able to START a process, get a process id back IMMEDIATELY,
  keep reasoning while it runs, observe state, read output, and terminate it.
  A blocking `execute() -> full output` model is explicitly rejected.
- Forge needs its own permission model (READ/WRITE/EXECUTE/NETWORK/DESTRUCTIVE).
- Claude must sit behind a clean `Agent` interface so it can be swapped later.

## Questions to answer (with evidence + citations + version numbers)
1. Enumerate the integration options precisely:
   a. Claude Agent SDK (`claude-agent-sdk` Python / `@anthropic-ai/claude-agent-sdk`)
   b. Claude API + SDK Tool Runner (`client.beta.messages.tool_runner`)
   c. Claude API + hand-written agent loop
   d. Subprocess-wrapping the `claude` CLI (we consider this a last resort)
   For each: what does it give us, what does it take away, what does it cost.
2. For the Claude Agent SDK specifically, determine CONCRETELY:
   - Current package name + version on PyPI and npm. Python version support.
   - How custom tools are defined (in-process MCP servers? `@tool` decorator?).
     Show real, verified code.
   - Can we DISABLE / restrict the built-in tools (Bash, Read, Write, Edit)?
     We need Forge's own Bash-equivalent that returns a process handle, not output.
   - The permission/approval hook API (`can_use_tool`, `PreToolUse` hooks,
     permission modes). Can we route approvals into our own TUI and await a
     user decision asynchronously?
   - Streaming: how do we get incremental assistant text + tool-call events for
     live rendering in a TUI? Is it an async iterator? Does it work with asyncio?
   - Session persistence / resume.
   - Does it require the `claude` CLI binary to be installed and does it shell out
     to it? THIS IS CRITICAL — if the SDK is just a subprocess wrapper around the
     CLI, say so explicitly and explain the consequences for packaging.
   - Authentication: API key vs an existing Claude subscription login.
3. For the Tool Runner / raw API route: what would we have to build ourselves
   (context management, compaction, retries, tool loop, streaming)? Estimate it.
4. Async-tool problem: with each option, how do we model a tool that returns
   "started, pid 1234" and later delivers an exit event? Is there a supported
   pattern, or do we implement start/poll/wait as separate tools? Recommend a
   concrete tool surface (list tool names + signatures).
5. Concurrency: can the agent loop run inside an asyncio event loop shared with
   a TUI (Textual) without blocking the UI? Any known pitfalls.

## Method
- Use WebSearch/WebFetch against official docs. Prefer code.claude.com/docs/en/agent-sdk
  and the anthropics GitHub repos. Cite URLs.
- ACTUALLY INSTALL AND TEST where you can: `pipx`/`pip install --user` or a venv in
  /tmp. Verify the import surface with `python -c`, `pip show`, `dir()`. Report
  real observed output, not assumptions. Note: Python here is 3.14.7.
- If something is unverifiable, say "UNVERIFIED" explicitly. Do not invent APIs.

## Deliverable
Write `/home/luciano/forge/docs/research/01-claude-integration.md`:
- A comparison table of the 4 options.
- A clear RECOMMENDATION with reasoning and the main risk of that choice.
- The concrete proposed agent tool surface for Forge.
- Verified code snippets (marked VERIFIED / UNVERIFIED).
- A "what would make us change our mind" section.
Be concise and technical. No filler. Then stop.
