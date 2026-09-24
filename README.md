# baiji

> A terminal AI coding agent built in Rust — a multi-crate workspace with a streaming ReAct runtime, multi-vendor providers (API-key-only setup), built-in coding tools, and sessions with JSONL persistence and context compaction.

![Rust](https://img.shields.io/badge/Rust-2024_Edition-orange?logo=rust)

![License](https://img.shields.io/badge/License-MIT-blue)

![Build](https://img.shields.io/badge/build-cargo-green)



![Tests](https://img.shields.io/badge/tests-380_passing-brightgreen)

---

## Features

- 🤖 **Streaming ReAct loop** — reasoning + native tool calling, with steering (inject new instructions mid-run) and instant cancellation (`Esc`)
- 🔌 **Multi-vendor providers** — Anthropic Messages + OpenAI Chat Completions + OpenAI Responses protocols; 12 vendor presets (OpenAI, Anthropic, DeepSeek, GLM, Kimi, MiniMax, MiMo, Bailian, OpenRouter, xAI Grok, OpenCode Zen, TokenHub) with model auto-discovery
- 🧠 **Thinking/reasoning support** — hot-swappable reasoning level (`/thinking high`), cross-protocol replay of thinking blocks within the current tool loop
- 🛠️ **Built-in coding tools** — read / write / edit / bash / grep / find / ls / search (BM25) / imports (tree-sitter index) / expand, plus external agent delegation (`/codex`, `/claude`, …)
- 🧵 **Context engineering** — reversible tool-output compression (content-addressed store + `expand` to recover), tiered compaction (deterministic or LLM-generated summaries), usage-anchored token estimation
- 💾 **Sessions** — tree-structured (fork/branch), JSONL persistence, `--session <id>` resume, cross-session project memory
- 📋 **Todo & goal & spec modes** — long-horizon task state externalized; auto-continue chains runs until todos complete; spec-driven drafting → approve → implement
- 👥 **Subagents** — `task` tool spawns isolated-context read-only subagents with role files (`agents/*.md`), optional per-role model/thinking config
- 🛡️ **Guardrails** — path whitelisting (lexical + realpath), HITL confirmation gates, plan mode (read-only tool gate), hook system (`exit 2` on `tool_call` blocks the call), safety plugin, output truncation
- 🖥️ **TUI** — Ratatui UI with streaming render, 19 slash commands, fish-style ghost completion, bilingual (en/zh) interface
- 📊 **Observability** — per-run JSONL telemetry (`BAIJI_TELEMETRY=file`), daily-rotated logs, compression savings accounting

---

## Quick Start

### Prerequisites

- Rust toolchain (`rustup` recommended, edition 2024)
- An API key for any supported vendor

### Build & Run

```bash
git clone https://github.com/catsonkeyboard/baiji.git
cd baiji
cargo build --release

baiji                 # interactive TUI (config wizard on first run)
baiji -e "msg" --yes  # headless one-shot run (streams to stdout)
baiji -e "msg" --plan # read-only planning run (plan mode)
baiji --sessions      # list sessions (no API key needed)
```

### Configuration

Global config lives at `~/.baiji/config.json` (a template is auto-generated on first run). A project may override a whitelisted subset via `./.baiji/config.json` (deep-merged; security-sensitive fields like `api_key` / `vendor` / `allowed_paths` are global-only).

```json
{
  "vendor": "glm",
  "api_key": "$ZHIPU_API_KEY",
  "model": null,
  "endpoint": null,
  "max_tokens": 4096,
  "thinking": null,
  "llm_compaction": false,
  "max_turns": 24,
  "compaction": { "enabled": true, "max_estimated_tokens": null, "keep_recent_turns": 6 },
  "retry": { "max_retries": 2, "base_delay_ms": 500, "max_delay_ms": 30000 },
  "external_agents": [
    { "name": "codex", "command": "codex exec {prompt}", "timeout_secs": 300 }
  ],
  "policy": {
    "allowed_paths": [],
    "require_confirmation_tools": ["bash", "write", "edit"],
    "max_tool_output_bytes": 32768,
    "bash_timeout_secs": 30,
    "compression_enabled": true
  },
  "ui": { "theme": "dark", "language": "en" }
}
```

- Only `vendor` + API key are required; `model` is auto-discovered from the vendor's models API when omitted (`/model` to change).
- `api_key` supports `$ENV_VAR` / `${ENV_VAR}` syntax; if omitted, the vendor's recommended env var is read.
- `endpoint` selects vendor endpoint variants — Coding Plan subscriptions use dedicated endpoints (e.g. GLM `"anthropic"` / `"coding"` / `"responses"`; deepseek/kimi/minimax/mimo/bailian expose verified `"anthropic"`-compatible endpoints).
- `thinking` sets the reasoning level: `"minimal"` / `"low"` / `"medium"` / `"high"` (null = off), mapped per protocol and hot-swappable via `/thinking`.
- `require_confirmation_tools` (HITL): listed tools prompt a y/a/n dialog before executing; empty list = auto-approve.
- `hooks` (global-only): user shell hooks on `run_start` / `turn_start` / `tool_call` / `tool_result` / `run_end`. Convention: **exit code 2 on `tool_call` blocks the call**; all other hook failures fail open. `BAIJI_HOOKS=off` disables.

### MCP Tools

Drop an `mcporter.json` in the project root; servers are spawned as resident stdio processes speaking native MCP (JSON-RPC), initialized once and reused for every call — millisecond round-trips instead of per-call CLI cold starts. Config format is identical to Claude Code's `.mcp.json` (`mcpServers: {command, args, env}`); tools are exposed to the model as `mcp__<server>__<tool>`. Crashed servers restart lazily on the next call.

---

## Architecture

```
crates/
  baiji-telemetry/   Span/event contracts (noop default, in-memory recorder, JSONL backend)
  baiji-ai/          Provider layer: types, Provider trait, Anthropic Messages,
                     OpenAI Chat Completions + Responses, 12-vendor registry, model discovery
  baiji-agent/       Agent runtime: AgentTool trait, ToolRegistry, streaming ReAct loop,
                     HookRegistry, SteeringQueue, ConfirmationGate, subagents
  baiji-tools/       Coding tools (read/write/edit/bash/grep/find/ls/search/imports/expand)
                     + ExecutionEnv (path whitelist, output truncation, timeouts)
                     + reversible output compressors
  baiji-harness/     AgentHarness: session tree, JSONL persistence, compaction,
                     skills, prompt templates, todo/goal/spec/memory, run loop
  baiji-extensions/  Plugin layer (Plugin/PluginManager) + built-ins: clock, safety,
                     command hooks, MCP stdio client
  baiji-tui/         Ratatui terminal UI (streaming, steering, ghost completion,
                     wizard, i18n en/zh)
src/                 Root bin crate `baiji`: config resolution + composition root
```

Dependency direction: `telemetry ← ai ← agent ← tools ← harness ← {tui, bin}`; `extensions ← agent`.

### Agent run loop

```
AgentRuntime::run()
  ├─ drain steering queue (mid-run user instructions)
  └─ loop (≤ max_turns):
       ├─ ChatRequest(system + history + tool definitions)   # plan mode filters to read-only tools
       ├─ chat_stream (transient errors retried w/ backoff)  # status-code-based classification
       ├─ no tool calls → final answer, done
       └─ tool calls → per call:
            hooks.on_tool_call (Deny/Modify/Proceed)
            ConfirmationGate (HITL y/a/n) → execute → hooks.on_tool_result
            # all-parallel batches run concurrently
       └─ context near window → elide old tool results (reversible, ctx-store handles)
```

Between runs, `AgentHarness` handles compaction (keep recent N turns intact, fold older turns into a summary), session persistence, and system-prompt assembly (skills, memory, todos, subagent roles, spec state).

---

## Keyboard & Commands

| Key         | Action                                        |
| ----------- | --------------------------------------------- |
| `Enter`     | Send message (mid-run = steering)             |
| `Esc`       | Cancel current run / stop auto-continue chain |
| `Ctrl+O`    | Session picker (switch / fork)                |
| `Ctrl+C`    | Quit                                          |
| `PgUp/PgDn` | Scroll chat history                           |

19 slash commands: `/help` `/config` `/model` `/thinking` `/plan` `/goal` `/spec` `/experts` `/subagents` `/todos` `/tasks` `/kill` `/compact` `/usage` `/status` `/session` `/fork` `/new` `/resume` — plus dynamic `/codex`-style commands for configured external agents. Ghost completion suggests as you type; `Tab` accepts.

---

## Development

```bash
cargo build                # Build the whole workspace
cargo test --workspace     # Run all tests (380 total, zero warnings)
cargo test -p baiji-agent  # Test a single crate
RUST_LOG=debug cargo run   # Debug logs → ~/.baiji/logs/ (never pollutes the project dir)
cargo build --release      # Optimized build
```

Tests live inline (`#[cfg(test)]` modules): stream state machines are pure and exhaustively unit-tested against raw SSE frames; the TUI has a full-layout `TestBackend` render smoke test; bash tool tests verify process-group kill of grandchildren.

---

## License

[MIT](./LICENSE) © [catsonkeyboard](https://github.com/catsonkeyboard)
