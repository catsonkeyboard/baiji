# baiji

> A terminal-based AI agent client built in Rust — featuring a ReAct reasoning loop, MCP tool integration, and a production-grade Harness Engineering guardrail system.

![Rust](https://img.shields.io/badge/Rust-2024_Edition-orange?logo=rust)
![License](https://img.shields.io/badge/License-MIT-blue)
![Build](https://img.shields.io/badge/build-cargo-green)

---

## Features

- 🤖 **ReAct Agent** — Reasoning + Acting loop with up to 5 iterations per turn
- 🖥️ **Terminal UI** — Fully interactive TUI powered by [Ratatui](https://ratatui.rs)
- 🔌 **MCP Integration** — Discovers and executes tools via the [mcporter](https://github.com/catsonkeyboard/mcporter) CLI bridge
- 🛡️ **Harness Engineering** — Production-grade guardrails: path whitelisting, tool blocking, output truncation, retry & backoff
- 🧠 **Context Management** — Token-estimation-based smart compression; older turns are summarized, not discarded
- 📊 **Observability** — Per-run trace files with LLM latency, tool timing, and I/O sizes
- ⏎ **Steering** — Send new instructions mid-run; the agent re-orients without restarting
- ❌ **Cancellation** — Press `Esc` to instantly cancel the current agent run

---

## Quick Start

### Prerequisites

- Rust toolchain (`rustup` recommended)
- An API key for Anthropic or OpenAI

### Installation

```bash
git clone https://github.com/catsonkeyboard/baiji.git
cd baiji
cargo build --release
```

### Configuration

Copy the sample config and fill in your credentials:

```bash
cp config.sample.json ~/.baiji/config.json
```

Edit `~/.baiji/config.json`:

```json
{
  "llm": {
    "provider": "anthropic",
    "base_url": "https://api.anthropic.com",
    "api_key": "$ANTHROPIC_API_KEY",
    "model": "claude-3-5-sonnet-20241022",
    "max_tokens": 4096
  },
  "policy": {
    "allowed_paths": ["./"],
    "max_tool_output_bytes": 8192,
    "blocked_tools": [],
    "require_confirmation_tools": ["builtin__write"],
    "max_search_depth": 10,
    "max_file_size": 1048576
  },
  "ui": {
    "theme": "dark",
    "show_thoughts": false
  }
}
```

> `api_key` supports `$ENV_VAR` and `${ENV_VAR}` syntax for automatic environment variable expansion.

### Run

```bash
cargo run
# or with debug logging:
RUST_LOG=debug cargo run
```

---

## Architecture

```
main.rs
  ├── Config::load()            Load ~/.baiji/config.json
  ├── ProviderFactory::create   Create LLM Provider (Anthropic / OpenAI)
  ├── McporterBridge::new()     Initialize MCP bridge (async tool discovery)
  ├── ToolPolicy::from_config() Load guardrail policy
  ├── ReActAgent::new()         Assemble Agent (LLM + tools + policy + MCP)
  └── App::run()                Enter Ratatui main loop
```

### Agent Run Loop

```
ReActAgent::run()
  ContextManager::trim()              ← Smart context compression
  TraceRecorder::start_turn()         ← Begin trace recording
  loop (max 5 iterations):
    stream_llm_response() + Retry     ← LLM call with automatic retry
    if tool_calls:
      ToolPolicy::check_tool()        ← Policy check (block/confirm/allow)
      ToolPolicy::check_path()        ← Path whitelist enforcement
      execute_tool()                  ← Execute the tool
      ToolPolicy::truncate_output()   ← Output truncation protection
      ToolResultValidator::validate() ← Inject [Observation] hints for LLM
      TraceRecorder::record_tool()    ← Track tool latency & I/O
    else:
      TraceRecorder::finish()         ← Save JSON trace to logs/traces/
      tx.send(Completed(answer))      ← Return answer to UI
```

---

## Module Reference

| Module | File | Responsibility |
|---|---|---|
| `app` | `src/app.rs` | App state, main loop, keyboard/Agent event handling |
| `ui` | `src/ui/mod.rs` | Ratatui rendering — Chat, Input, StatusBar |
| `event` | `src/event.rs` | Async keyboard event listener |
| `agent` | `src/agent/agent.rs` | ReAct Agent core loop |
| `builtin_tools` | `src/agent/builtin_tools.rs` | Built-in tools: grep / read / write |
| `tool_policy` | `src/agent/tool_policy.rs` | 🛡️ Path whitelist, tool blocking, output truncation |
| `retry` | `src/agent/retry.rs` | 🔄 Error classification, exponential backoff |
| `validator` | `src/agent/validator.rs` | 🔍 Tool result validation, observation injection |
| `context` | `src/agent/context.rs` | 🧠 Token estimation, summary compression |
| `trace` | `src/agent/trace.rs` | 📊 Per-turn latency & tool timing traces |
| `memory` | `src/agent/memory.rs` | System prompt construction, AGENTS.md loading |
| `llm` | `src/llm/` | LLM abstraction (`LLMProvider` trait + Anthropic + streaming) |
| `mcp` | `src/mcp/` | MCP bridge — tool discovery & execution via mcporter |
| `config` | `src/config.rs` | JSON config loading, env var expansion, validation |

---

## Harness Engineering Guardrail System

### Architectural Constraints — ToolPolicy

| Config Key | Default | Description |
|---|---|---|
| `allowed_paths` | `["./"]` | Restrict file operations to whitelisted directories |
| `blocked_tools` | `[]` | Prevent the agent from calling specified tools |
| `require_confirmation_tools` | `["builtin__write"]` | Tools that require user confirmation before execution |
| `max_tool_output_bytes` | `8192` | Auto-truncate tool output with `[truncated]` marker |
| `max_search_depth` | `10` | Maximum grep recursion depth |
| `max_file_size` | `1048576` (1MB) | Skip files larger than this |

### Feedback Loop — Retry & Validation

- Transient errors (network, rate-limit, 5xx) → exponential backoff: `500ms → 1s → 2s`, max 2 retries
- Permanent errors (401, 404) → fail immediately, no retry
- Empty results, error strings, and truncated output are detected and flagged
- Automatic `[Observation: ...]` injection guides LLM self-correction

### Context Engineering — ContextManager

- Token estimation: ASCII ≈ 0.25 tok/char, CJK ≈ 0.5 tok/char
- The most recent **6 turns** are always preserved intact
- Older turns are **compressed into summaries** (not dropped), injected as `[Conversation Summary]`

### Observability — TraceRecorder

- Each agent run writes a trace to `logs/traces/trace_YYYYMMDD_HHMMSS.json`
- Captures: per-turn LLM latency, tool call details (name / duration / I/O size / success)
- Status bar shows real-time: `⏳ Turn N | M tools`

---

## MCP Tool Integration

MCP tools are configured via `mcporter.json` in the project root (excluded from version control — see `mcporter.sample.json` for a template).

```json
{
  "mcpServers": {
    "your-server": {
      "command": "npx",
      "args": ["-y", "your-mcp-package"],
      "env": {
        "YOUR_API_KEY": "$YOUR_API_KEY"
      }
    }
  }
}
```

### Tool Naming Convention

| Type | Format | Example |
|---|---|---|
| Built-in | `builtin__<name>` | `builtin__read`, `builtin__write`, `builtin__grep` |
| MCP | `<server>__<tool>` | `tavily__search` |

---

## Keyboard Shortcuts

| Key | Action |
|---|---|
| `Enter` | Send message |
| `Esc` | Cancel current agent run |
| `↑ / ↓` | Scroll chat history |
| `Ctrl+C` | Quit |

---

## Development

```bash
cargo build          # Build
cargo run            # Run
cargo test           # Run all tests (62 total)
RUST_LOG=debug cargo run  # Run with debug logging
```

---

## License

[MIT](./LICENSE) © [catsonkeyboard](https://github.com/catsonkeyboard)
