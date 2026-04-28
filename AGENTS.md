# baiji

A terminal AI Agent client built on Ratatui, featuring a ReAct Agent, MCP tool integration, and a Harness Engineering guardrail system.

## Common Commands

```bash
cargo build                # Build
cargo run                  # Run
cargo test                 # Run all tests (62 total)
RUST_LOG=debug cargo run   # Run with debug logging
```

## Architecture Overview

```
main.rs
  ├── Config::load()            Load ~/.baiji/config.json
  ├── ProviderFactory::create   Create LLM Provider (Anthropic / OpenAI)
  ├── McporterBridge::new()     Initialize MCP bridge (async tool discovery in background)
  ├── ToolPolicy::from_config() Load tool policy (path whitelist / output truncation etc.)
  ├── ReActAgent::new()         Assemble Agent (LLM + tools + policy + MCP)
  └── App::run()                Enter Ratatui main loop
```

## Module Reference

| Module | File | Responsibility |
|--------|------|----------------|
| `app` | `src/app.rs` | App state, main loop, keyboard/Agent event handling, real-time stats (turn/tool counts) |
| `ui` | `src/ui/mod.rs` | Ratatui rendering (Chat / Input / StatusBar), status bar shows `Turn N | M tools` |
| `event` | `src/event.rs` | Async keyboard event listener (`spawn_blocking` + mpsc) |
| `agent` | `src/agent/agent.rs` | ReAct Agent core loop: streaming LLM calls, tool execution, Steering interrupts |
| `builtin_tools` | `src/agent/builtin_tools.rs` | Built-in tools: grep / read / write (policy-constrained) |
| `tool_policy` | `src/agent/tool_policy.rs` | 🛡️ Tool policy engine: path whitelist, tool blocking, output truncation, confirmation |
| `retry` | `src/agent/retry.rs` | 🔄 Retry & degradation: error classification, exponential backoff, tool circuit breaker |
| `validator` | `src/agent/validator.rs` | 🔍 Tool result validator: empty result detection, error recognition, LLM correction hint injection |
| `context` | `src/agent/context.rs` | 🧠 Layered context management: token estimation, summary compression, turn grouping |
| `trace` | `src/agent/trace.rs` | 📊 Call chain tracing: per-turn latency, tool timing, JSON output to `logs/traces/` |
| `memory` | `src/agent/memory.rs` | System prompt construction, AGENTS.md loading |
| `prompt` | `src/agent/prompt.rs` | Agent system prompt template |
| `llm` | `src/llm/` | LLM abstraction layer (`LLMProvider` trait + Anthropic implementation + streaming) |
| `mcp` | `src/mcp/` | MCP bridge (discovers and executes tools via the mcporter CLI) |
| `config` | `src/config.rs` | JSON config loading, environment variable expansion, validation, policy config |

## Key Data Flows

### User Sends a Message
```
User presses Enter
  → App::send_message()
    → Append User / Assistant (empty placeholder) messages
    → scroll_to_bottom()
    → tokio::spawn(agent.run(history, question, tx, cancel, steering))
```

### Agent Run Loop (with Harness Engineering)
```
ReActAgent::run()
  ContextManager::trim()         ← Smart context compression (summaries instead of truncation)
  TraceRecorder::start_turn()    ← Trace recording
  loop (max 5 iterations):
    stream_llm_response() + RetryPolicy   ← LLM call + automatic retry
    if tool_calls:
      for each tool_call:
        ToolPolicy::check_tool()           ← Policy check (block/confirm/allow)
        ToolPolicy::check_path()           ← Path whitelist enforcement
        execute_tool()                     ← Execute the tool
        ToolPolicy::truncate_output()      ← Output truncation protection
        ToolResultValidator::validate()    ← Result validation + observation injection
        TraceRecorder::record_tool_call()  ← Track tool latency
      messages.push(assistant + tool_results)
    else:
      TraceRecorder::finish() → save JSON trace
      tx.send(Completed(answer))  →  return
```

### UI Event Handling
```
App::run() main loop:
  tokio::select! {
    event_handler.next()  →  handle_key_event()
    agent_rx.recv()       →  handle_agent_event()
                              TurnStart  → current_turn += 1
                              ToolEnd    → tool_call_count += 1
                              Completed  → reset counters
  }
  terminal.draw(UI::draw)  // Status bar: ⏳ Turn 3 | 5 tools
```

## Configuration

**Path**: `~/.baiji/config.json` (same across all platforms)

```json
{
  "llm": {
    "provider": "anthropic",
    "base_url": "https://api.anthropic.com",
    "api_key": "$ANTHROPIC_API_KEY",
    "model": "claude-3-5-sonnet-20241022"
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

- `api_key` supports `$ENV_VAR` or `${ENV_VAR}` syntax for automatic expansion
- `provider` supports `"anthropic"` and `"openai"`
- All `policy` fields are optional; safe defaults are used when omitted
- MCP tools are configured independently via `mcporter.json` (project root)

## Harness Engineering Guardrail System

### Architectural Constraints (ToolPolicy)
- `allowed_paths`: File operations (read/write/grep) are restricted to whitelisted directories
- `blocked_tools`: Prevents the Agent from using specified tools
- `require_confirmation_tools`: High-risk operations require user confirmation (TODO: HITL interaction)
- `max_tool_output_bytes`: Output exceeding the limit is automatically truncated with a `[truncated]` marker
- `max_search_depth`: Maximum grep recursion depth (default: 10)
- `max_file_size`: Files larger than this are skipped (default: 1MB)

### Feedback Loop (RetryPolicy + ToolResultValidator)
- LLM call failures are automatically classified: transient errors (network/rate-limit/5xx) trigger retries; permanent errors (401/404) fail immediately
- Exponential backoff: 500ms → 1s → 2s, max 5s, up to 2 retries
- Tool result validation: empty results, error patterns, and truncation are detected
- Automatic `[Observation: ...]` injection to assist LLM self-correction

### Context Engineering (ContextManager)
- Token-estimation-based trimming (mixed language: ASCII at 0.25 tok/char, CJK at 0.5 tok/char)
- Messages grouped into "turns" by User+Assistant pairs; the most recent 6 turns are kept intact
- Older turns are compressed into summaries (not discarded), injected as `[Conversation Summary]`

### Observability (TraceRecorder)
- Each Agent run generates `logs/traces/trace_YYYYMMDD_HHMMSS.json`
- Records: per-turn LLM latency, tool call details (name/duration/IO size/success)
- Status bar shows real-time metrics: current turn count, tool call count

## Important Conventions

### Tool Naming
- Built-in tools: `builtin__grep`, `builtin__read`, `builtin__write` (prefix `builtin__`)
- MCP tools: `server_name__tool_name` (double underscore), parsed with `splitn(2, "__")`

### Scroll Mechanism (`app.scroll`)
- `scroll_to_bottom()` sets `scroll = usize::MAX` as a sentinel value
- `render_chat_area` (takes `&mut App`) clamps after each frame: `app.scroll = scroll.min(max_scroll)`
- `ScrollbarState::new(max_scroll + 1)` — Ratatui scrollbar only shows the thumb at the bottom when `position == content_length - 1`

### Anthropic tool_result Format
Tool result blocks must use the `tool_use_id` field (not `id`), otherwise the API returns 400.

### Async Architecture
- The Agent runs in a separate `tokio::spawn` task, pushing events to the main loop via `UnboundedSender<AgentEvent>`
- Keyboard events are read with `tokio::task::spawn_blocking` (`crossterm::event::read` is blocking IO)
- Trace files are saved asynchronously in a separate `tokio::spawn`, avoiding blocking the Agent's return

### Steering Mechanism
- Messages sent by the user while the Agent is running are injected into the steering queue
- After each tool execution, the steering queue is checked; if messages are present, remaining tools are skipped and new instructions are injected
- The Escape key cancels the entire Agent run via `CancellationToken`
