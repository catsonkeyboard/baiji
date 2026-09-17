# baiji

A terminal AI coding agent built on a multi-crate Rust workspace: async streaming runtime, multi-vendor providers with API-key-only setup, built-in coding tools, sessions with JSONL persistence and compaction.

## Common Commands

```bash
cargo build                # Build the whole workspace
cargo run                  # Run the TUI app (default)
cargo test --workspace     # Run all tests (172 total)
baiji -e "msg" --yes      # Headless one-shot run (streams to stdout)
baiji --sessions          # List sessions (no API key needed)
cargo test -p baiji-agent  # Test a single crate
cargo build --release      # Optimized build
RUST_LOG=debug cargo run   # Debug logs (written to ./logs/)
```

## Workspace Layout

```
crates/
  baiji-telemetry/   Span/event contracts (noop default, in-memory recorder for tests)
  baiji-ai/          Provider layer: types, Provider trait, Anthropic Messages,
                     OpenAI Chat Completions + Responses, vendor registry, model discovery
  baiji-agent/       Agent runtime: AgentTool trait, ToolRegistry, AgentEvent,
                     HookRegistry (HookDecision::Deny), SteeringQueue, streaming ReAct loop
  baiji-tools/       Coding tools (read/write/edit/bash/grep/find/ls) + ExecutionEnv
                     (path whitelist, output truncation, timeouts)
  baiji-harness/     AgentHarness: session tree, JSONL persistence, compaction,
                     skills loading, prompt templates, run loop
  baiji-extensions/  Plugin layer (Plugin/PluginManager) + built-ins: clock, safety
  baiji-tui/         Ratatui terminal UI (chat / input / status, streaming, steering)
src/                 Root bin crate `baiji`: config resolution + composition
```

Dependency direction: telemetry ← ai ← agent ← {tools, harness} ← {tui, bin}; extensions ← agent.

## Configuration

**Path**: `~/.baiji/config.json` (auto-generated template on first run)

```json
{
  "vendor": "glm",
  "api_key": "$ZHIPU_API_KEY",
  "model": null,
  "protocol": null,
  "base_url": null,
  "max_tokens": 4096,
  "llm_compaction": false,
  "policy": {
    "allowed_paths": [],
    "require_confirmation_tools": ["bash", "write", "edit"],
    "max_tool_output_bytes": 32768,
    "bash_timeout_secs": 30
  },
  "ui": { "theme": "dark" }
}
```

- Only `vendor` + API key are required; `model` is auto-discovered from the vendor's models API when omitted (first entry is used; set it explicitly to pin).
- `api_key` supports `$ENV_VAR` / `${ENV_VAR}`; if omitted, the vendor's recommended env var is read (see below). Undefined vars are kept literally.
- `endpoint` selects a vendor endpoint variant (default `"api"` = pay-per-use). Coding Plan subscriptions must use their dedicated endpoints — GLM: `"anthropic"` (`/api/anthropic`, Anthropic protocol) / `"coding"` (`/api/coding/paas/v4`, OpenAI Chat) / `"responses"` (`/api/v1`, OpenAI Responses); deepseek/kimi/minimax/mimo/bailian: `"anthropic"` (verified Anthropic-compatible endpoints). Unknown endpoint names fail fast with the available list.
- `protocol` override: `"anthropic"` / `"chat"` / `"responses"`; `base_url` override accepts root, `/v1`-suffixed, versioned (`/v4`), or full endpoint paths. Explicit `base_url`/`protocol` take precedence over the selected endpoint variant.
- `policy.allowed_paths` extends the path whitelist (default: current directory only).
- `policy.require_confirmation_tools` (HITL): listed tools prompt a y/a/n confirmation dialog before executing; `a` (AllowAll) suppresses re-prompts for that tool for the rest of the run. Empty list = auto-approve everything.
- `llm_compaction: true` switches context compaction to provider-generated summaries (falls back to the deterministic summary on API error).
- `ui.theme`: `"dark"` (default) or `"light"` palettes.
- MCP: place a `mcporter.json` in the project root; tools are discovered via `npx -y mcporter` at startup (requires Node). Tool names use `server.tool`.

### Vendor presets (`baiji-ai::vendors`)

| id | provider | base_url | api key env |
|----|----------|----------|-------------|
| `openai` | OpenAI | `https://api.openai.com` | `OPENAI_API_KEY` |
| `anthropic` (`claude`) | Anthropic | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| `openrouter` (`router`) | OpenRouter | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| `bailian` (`dashscope`, `qwen`) | 阿里云百炼 | `https://dashscope.aliyuncs.com/compatible-mode/v1` · `anthropic`=`/apps/anthropic` | `DASHSCOPE_API_KEY` |
| `tencent` (`tokenhub`, `hunyuan`) | 腾讯 TokenHub | `https://tokenhub-intl.tencentmaas.com/v1` | `TOKENHUB_API_KEY` |
| `glm` (`zhipu`, `bigmodel`) | 智谱 | `https://open.bigmodel.cn/api/paas/v4` · Coding Plan: `anthropic`=`/api/anthropic`, `coding`=`/api/coding/paas/v4`, `responses`=`/api/v1` | `ZHIPU_API_KEY` |
| `kimi` (`moonshot`) | Kimi | `https://api.moonshot.cn/v1` · Coding Plan: `anthropic`=`/anthropic` | `MOONSHOT_API_KEY` |
| `deepseek` | DeepSeek | `https://api.deepseek.com` · `anthropic`=`/anthropic` | `DEEPSEEK_API_KEY` |
| `minimax` | MiniMax | `https://api.minimaxi.com/v1` · Coding Plan: `anthropic`=`/anthropic` | `MINIMAX_API_KEY` |
| `mimo` (`xiaomi`) | 小米 MiMo | `https://api.xiaomimimo.com/v1` · Token Plan: `anthropic`=`/anthropic` | `MIMO_API_KEY` |
| `opencode` (`zen`) | OpenCode Zen | `https://opencode.ai/zen/v1` | `OPENCODE_API_KEY` |
| `xai` (`grok`) | xAI | `https://api.x.ai` | `XAI_API_KEY` |

TokenHub has regional endpoints (default here is the international one) — override `base_url` per the TokenHub docs. OpenCode Zen does not expose a models API: set `model` explicitly.

## Architecture

### Provider layer (`baiji-ai`)

- Unified types: `Message { role, content, tool_calls, tool_results }`, `ChatRequest/Response`, `StreamChunk { Content, ToolCallStart, ToolCallArguments, Done, Error }`.
- `Provider` trait: `chat` / `chat_stream` (+ `protocol`, `model`).
- Anthropic `/v1/messages`: multiple System messages joined into `system`; tool args streamed as `ToolCallArguments` (index→id mapping).
- OpenAI-compatible: Chat Completions + Responses. Streaming tool calls are **buffered per index/item_id and flushed at stream end** — parallel tool-call fragments may interleave, and the agent accumulator is sequential. A stream-end sentinel flushes even when the server omits `[DONE]` / `response.completed`.
- Endpoint joining (`openai::endpoint`): full path kept as-is; versioned base (`/v1`, `/v4`, `/plan/v3`) appends the path; bare root appends `/v1<path>`.
- Model discovery `list_models`: OpenAI-style `GET /models` (Bearer) or Anthropic `/v1/models` (x-api-key + anthropic-version); both return `{"data":[{"id",...}]}`.

### Agent runtime (`baiji-agent`)

```
run()
  ├─ inject steering messages each turn
  ├─ loop (≤ max_turns, default 24):
  │    ├─ chat_stream with retry (transient: rate/timeout/5xx; backoff 500ms×2^n, ≤2 retries)
  │    ├─ accumulate Content→text, ToolCallStart/Arguments→tool calls
  │    ├─ no tool calls → final answer, break
  │    └─ tool calls → per tool: hooks.on_tool_call (Deny→error result)
  │         → ConfirmationGate (HITL: Deny→error result, AllowAll remembered per run)
  │         → tool.execute → hooks.on_tool_result → events → telemetry span
  │         after each tool, steering check (skip remaining)
  └─ new messages (steering/assistant/tool) appended back to the session history
```

- `AgentTool` trait: `name/description/parameters` (JSON Schema) + async `execute(Value) -> ToolOutput { content, is_error }`. `ToolRegistry` (same-name re-register replaces).
- `Hook` trait: `on_run_start/end`, `on_turn_start`, `on_tool_call` (→ `HookDecision`), `on_tool_result`. Default no-ops.
- HITL: `Approver` trait (`confirm(request, cancel) -> {Allow, AllowAll, Deny}`); `AutoApprover` is the default. `ConfirmationGate` wraps an approver with a required-tool list and per-run AllowAll memory. The TUI's `InteractiveApprover` bridges requests into dialogs; cancel/exit path always resolves (never hangs).
- `AgentEvent` over an unbounded mpsc: `TurnStarted/TextDelta/ToolStarted/ToolFinished/TurnFinished/RunCompleted/RunFailed/Interrupted`.
- `SteeringQueue`: messages typed while the agent runs; drained between turns and after each tool (skips remaining tools).
- Telemetry spans: `agent.run`, `agent.turn` (turn=N), `agent.tool` (name, duration_ms, is_error).

### Harness (`baiji-harness`)

- `AgentHarness::run`: persist user message → compact (if over budget) → `AgentRuntime::run` → persist new messages. Checkpoint is taken **after** compaction (compaction shrinks the list).
- Compaction: token estimate (ASCII ~0.25/char, CJK ~0.5/char, +4/message); keeps the last 6 turns intact, older turns folded into a summary. Default budget 48k tokens. `compact_with_llm` (opt-in via `llm_compaction: true`) asks the provider to summarize and falls back to the deterministic summary on error/empty.
- Sessions: `~/.baiji/sessions/<id>.jsonl`, append-only records `Started/Message/Summary`; `load`/`switch_session` replays (Summary → injected as `[Conversation Summary]` System message; history stays raw and self-heals via the next compaction). `branch()` forks a session (parent link, history copied). `list_sessions()` returns all metas for the UI picker.
- Skills: `load_skills([dirs])` scans `*/SKILL.md` with `name:`/`description:` frontmatter; project `./.baiji/skills` overrides user `~/.baiji/skills`; rendered into the system prompt.
- Templates: `render("... {{var}} ...", &vars)`.
- Project memory (cross-session, `memory.rs`): per-project JSONL at `~/.baiji/memory/<name-hash>.jsonl` (project key = dir name + path hash). Entries carry a kind (fact/decision/preference/gotcha), `learned_at`, optional `valid_until` TTL — expired entries are lazily evicted and never injected. Active entries (≤20, 200 chars each) render into the system prompt as `## Project memory`. The `memory` tool (add/list/forget) lets the LLM write entries; same-fact re-add refreshes instead of duplicating.

### Tools (`baiji-tools`)

`read` (multi-mode JIT disclosure: `signatures` = symbol outline with line anchors → read exact ranges via offset/limit, `map` = compact directory tree, `full` = numbered lines with optional `density` (0.05-1.0) entropy-based line selection; over-budget full reads auto-degrade to `[auto-density …]` instead of hard truncation — disable via `ExecutionEnv::without_density_fallback`), `write` (creates parents), `edit` (exact-match replace; unique or `replace_all`), `bash` (`sh -c` in workdir, timeout + kill, exit/stdout/stderr, output compression: noise-line filtering + consecutive-duplicate folding `⟨… repeated N×⟩`), `grep` (regex, depth ≤10, skips `.git`/`target`/`node_modules`/hidden, glob filter, ≤200 matches), `find` (name substring + kind filter), `ls` (dirs first), `expand` (retrieve truncated output by `ctx:` handle).

**tree-sitter index** (`signatures.rs` / `index.rs`): AST symbol extraction for Rust/Python/JS/TS/Go (`Symbol { name, kind, line_start, line_end, signature }`, ≤500/file; regex fallback for other languages or parse failures). `CodeIndex` scans on demand (≤2000 files, same skip rules as grep) building a symbol table + import edges per file. Tools built on it: `search` (BM25 over symbol+filename docs — identifiers split on camelCase/snake_case/abbreviations; results are `path:L start-end` anchors feeding read offset/limit) and `imports` (outgoing edges of a file / incoming importers = change impact).

`ExecutionEnv`: workdir, `allowed_roots` path whitelist with lexical `..` normalization (no fs canonicalization — works for not-yet-existing paths), byte-boundary-safe output truncation, `max_file_size` 1MB, bash timeout 30s default.

**CCR (reversible truncation)**: when output exceeds `max_output_bytes` and the ctx store is enabled (default: `~/.baiji/ctx-store/`), the full content is spilled to a SHA-256 content-addressed file and the marker carries `ctx:<handle16>`; the `expand` tool retrieves it (with offset/limit paging). Truncation is never information loss.

**Context ledger**: tools that compress/truncate report `original_bytes` on `ToolOutput`; the runtime annotates `agent.tool` spans (`bytes_original`/`bytes_delivered`/`bytes_saved`) and `AgentEvent::ToolFinished`; the harness taps the event stream and appends a `Ledger { tool_calls, original_bytes, delivered_bytes }` record per run to the session JSONL (ignored on replay). The TUI status bar shows cumulative bytes saved.

### Extensions (`baiji-extensions`)

`Plugin::register(&mut PluginContext)` adds tools/hooks; `PluginManager::apply` merges into the registries. Built-ins: `ClockPlugin` (a `now` tool), `SafetyPlugin` (hook denying command-position `rm -rf` against `/`, `~`, `$HOME` — token-level detection, `echo rm -rf /` is not blocked).

MCP: `mcp` module ports the mcporter CLI bridge — `register_mcp_tools(&mut ToolRegistry, mcporter.json path)` discovers `server.tool` tools via `npx -y mcporter list --json` (30s timeout per server, per-server failures warn-and-skip) and wraps each as an `AgentTool` that shells out to `mcporter call`.

### Headless CLI & Telemetry backends

- `baiji -e "<msg>"` runs one turn without the TUI: text deltas stream to stdout, tool activity goes to stderr, session is persisted (resumable via `--session <id>` from `baiji --sessions`). Confirmation policy in headless mode: `--yes` auto-approves the configured confirmation list; without it, confirmation-gated tools are **denied** (`DenyAllApprover` — unattended-safe default). `--sessions` lists sessions directly from the JsonlStore (creates nothing, needs no API key).
- Telemetry: `BAIJI_TELEMETRY=file` (or `jsonl`) appends span/event records to `~/.baiji/traces/trace_<ts>.jsonl` via `JsonlTelemetry` (span_start with constructor attrs → span_end with runtime attrs/duration/error; zero-dependency unix-ms timestamps). Default remains `NoopTelemetry`; `RecordingTelemetry` serves tests.

### TUI (`baiji-tui`)

Chat / input / status layout. Streaming text renders into a partial line and lands as a full line on `RunCompleted`. Typing during a run pushes steering; Esc cancels via `CancellationToken`; PageUp/PageDown scroll with a `usize::MAX` stick-to-bottom sentinel clamped each frame.

- **Session picker** (`Ctrl+O`): centered overlay listing sessions (newest first, current marked `▸`, branch origin shown as `⎇parent`); `Enter` switches (chat rebuilt from replayed history), `b` branches the current session, `Esc` closes. Disabled while a run is active.
- **In-TUI configuration** (hot provider swap, no restart): `/config` opens a wizard overlay — vendor list → endpoint list (Coding Plan variants with notes) → API key input (typed into the input box; empty keeps existing/$ENV) → model picker (async discovery via the vendor's models API, with a manual-input fallback) → applied. `/model [name]` switches model directly (no arg opens the picker); `/status` shows the active settings. Apply = save config file (serde_json roundtrip preserving unknown fields) + rebuild provider (`settings::build_provider`, endpoint→protocol routing) + `AgentHarness::swap_provider` (RwLock-backed hot swap in `AgentRuntime`) + status-bar hint update. Blocked while a run is active.
- **HITL dialogs**: `InteractiveApprover` forwards confirmation requests to the UI loop; the dialog replaces the input box (`y` allow / `a` allow-all-this-run / `n` or Esc deny). A superseded unanswered request is auto-denied; app exit and run cancel always resolve pending requests.
- **Themes**: `Theme::dark()` (default) / `light()` palettes from `ui.theme`.
- **Paste**: bracketed paste is enabled for the TUI lifetime (`EnableBracketedPaste` on init, disabled on exit); `Event::Paste` is forwarded as `UiEvent::Paste` and folded into the input box single-line (`sanitize_paste`: newlines → spaces).

## Key Data Paths

```
Enter → App::spawn_run → harness.run
  → JsonlStore append(user)
  → compact (maybe)
  → AgentRuntime::run(system + history)
       provider.chat_stream ─ mpsc AgentEvent ─→ TUI forward task → UiEvent::Agent
  → JsonlStore append(new messages)
```

## Important Conventions

- Tool names are plain (`read`, `bash`, …) — no prefixes.
- `StreamChunk::Content` always means assistant text; tool arguments flow only via `ToolCallArguments` (providers must not smuggle args through Content).
- Tool failures are reported as `ToolOutput { is_error: true }` (returned to the LLM), not as `Err` (which aborts the run).
- Session history excludes the system prompt; the runtime prepends it per run. Multiple System messages may appear after compaction.
- Anthropic tool results must use `tool_use_id`; OpenAI function outputs go through `function_call_output` (Responses) or `role:"tool"` + `tool_call_id` (Chat Completions).
- Logging goes to `./logs/` (never stdout — the TUI owns the terminal).
