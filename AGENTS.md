# baiji

A terminal AI coding agent built on a multi-crate Rust workspace: async streaming runtime, multi-vendor providers with API-key-only setup, built-in coding tools, sessions with JSONL persistence and compaction.

## Common Commands

```bash
cargo build                # Build the whole workspace
cargo run                  # Run the TUI app (default)
cargo test --workspace     # Run all tests (325 total)
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

Dependency direction: telemetry ← ai ← agent ← tools ← harness ← {tui, bin}; extensions ← agent. (harness depends on tools only for the shared CCR `spill_to_store` primitive — handle format must stay single-sourced so `expand` round-trips.)

## Configuration (two-scope, pi-style layered settings)

**Global**: `~/.baiji/config.json` (auto-generated template on first run). **Project** (optional): `./.baiji/config.json` — deep-merged over global (objects merge recursively, arrays/scalars replace, project wins), then env-var overrides apply. Project scope is whitelist-limited for security: only `model` / `max_tokens` / `max_turns` / `llm_compaction` / `compaction` / `retry` / `ui` / `policy.{max_tool_output_bytes, bash_timeout_secs, compression_enabled, verbosity_steer}` are honored; `api_key` / `vendor` / `endpoint` / `base_url` / `protocol` (a hostile repo could redirect the global key to an arbitrary server) and `policy.{allowed_paths, require_confirmation_tools}` (a hostile repo could open the sandbox / drop HITL) are global-only — violations are dropped with a warning. Unknown fields warn and are ignored.

```json
{
  "vendor": "glm",
  "api_key": "$ZHIPU_API_KEY",
  "model": null,
  "protocol": null,
  "base_url": null,
  "max_tokens": 4096,
  "llm_compaction": false,
  "max_turns": 24,
  "compaction": {
    "enabled": true,
    "max_estimated_tokens": null,
    "keep_recent_turns": 6
  },
  "retry": {
    "max_retries": 2,
    "base_delay_ms": 500,
    "max_delay_ms": 30000
  },
  "policy": {
    "allowed_paths": [],
    "require_confirmation_tools": ["bash", "write", "edit"],
    "max_tool_output_bytes": 32768,
    "bash_timeout_secs": 30,
    "compression_enabled": true,
    "verbosity_steer": false
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
- `max_turns` caps LLM turns per run (default 24).
- `compaction`: `enabled: false` disables both context-reduction tiers (tool-result stubbing + summary; the in-run anti-overflow trim stays); `max_estimated_tokens` overrides the window-derived budget (70% of the context window minus the output reserve, floor 16k); `keep_recent_turns` (default 6) is the intact-turn window. The in-run anti-overflow trim (`elide_old_tool_results`, triggered at 85% of the window) is reversible too when a spill closure is injected: elided content goes to the ctx store with a `ctx:<handle>` marker (main.rs wires `baiji_tools::spill_to_store` via `AgentRuntime::with_spill` — a closure, because a direct agent→tools dependency would cycle).
- `auto_continue: true` enables auto-continue (T4): when a run finishes normally with open todos and the turn budget is not exhausted, the next run starts automatically with the fixed `baiji_harness::AUTO_CONTINUE_PROMPT` input (visible in history). The budget (`auto_continue_max_turns`, default 96) counts per user-initiated chain (reset on manual submit); Esc interrupts the chain. headless opt-in: `baiji -e "..." --continue-until-done`.
- `retry`: transient-error retries with exponential backoff (`base_delay_ms × 2^attempt`, capped at `max_delay_ms`; server `Retry-After` honored but also capped). Defaults 2 / 500ms / 30s.
- `ui.theme`: `"dark"` (default) or `"light"` palettes.
- `hooks` (global-only): user command hooks fired on lifecycle events — `run_start` / `turn_start` / `tool_call` / `tool_result` / `run_end`, each a list of `{ "command": "...", "timeout_secs": 10 }` (`command_hook.rs` in baiji-extensions). Context is JSON on the hook's stdin (`{"event", ...}` with tool/args/input/turn/output-preview) plus the `BAIJI_HOOK_EVENT` env var. Convention: **exit code 2 on `tool_call` blocks the call** (stderr becomes the deny reason the LLM sees); any other failure or timeout is logged and fails open. `tool_result` is observe-only. Security: hooks run arbitrary shell — the section is NOT project-whitelisted (project-level `hooks` is dropped with a warning) and `BAIJI_HOOKS=off` is the global kill switch; the /status summary shows the count.
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
  │    ├─ no tool calls → final answer, break (a `max_tokens`-truncated answer carries a visible continuation marker)
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

- `AgentHarness::run`: persist user message → **stub old tool results** (if over budget) → compact (if still over budget) → `AgentRuntime::run` → persist new messages. Checkpoint is taken **after** compaction (compaction shrinks the list).
- Two-tier context reduction over one budget (`max_estimated_tokens`): **tier 1, reversible tool-result stubbing** (`stub.rs`, runs first when the ctx store is configured via `set_ctx_store`): tool results ≥512B outside the `keep_recent_turns` window are replaced oldest-turn-first with a `[ctx stub: … ctx:<handle>]` marker (original spilled via `baiji_tools::spill_to_store`, recoverable with `expand`); stops as soon as the estimate fits, in-memory view only — the JSONL keeps raw originals and each run re-derives (content-addressed spill is idempotent); `branch` forks inherit the current in-memory (possibly stubbed) view. **tier 2, summary compaction**: token estimate (ASCII ~0.25/char, CJK ~0.5/char, +4/message) **anchored on provider usage** (pi-style `estimateContextTokens`): the harness records the last reported usage + message count as a `UsageAnchor` after each run; the trigger estimate becomes `heuristic + offset` where `offset = usage − heuristic-of-anchored-prefix` (system prompt, tool definitions and tokenizer drift all live in the real number). The budget is tightened by the offset so stub/compact internal stop conditions stay equivalent; forcing (compact despite few turns) only trusts real values — anchored total, or observed tokens when no anchor exists (first run / after invalidation). The anchor is invalidated (history version bump) whenever history is rewritten: compaction, stubbing, session switch, branch; keeps the last 6 turns intact, older turns folded into a summary that carries `Files read:` / `Files modified:` tail lines (tool-call path extraction, capped at 30 per kind, merged forward across compactions; bash paths are not parsed). Default budget 48k tokens. `compact_with_llm` (opt-in via `llm_compaction: true`) asks the provider to summarize and falls back to the deterministic summary on error/empty.
- Sessions: `~/.baiji/sessions/<id>.jsonl`, append-only records `Started/Message/Summary`; `load`/`switch_session` replays (Summary → injected as `[Conversation Summary]` System message; history stays raw and self-heals via the next compaction). `branch()` forks a session (parent link, history copied). `list_sessions()` returns all metas for the UI picker.
- Skills: `load_skills([dirs])` scans `*/SKILL.md` with `name:`/`description:` frontmatter; project `./.baiji/skills` overrides user `~/.baiji/skills` (same-name entries within one root resolve deterministically — lexically greater path wins); rendered into the system prompt. The `skills` config section (`enabled`, default true; `disabled` name list; env `BAIJI_SKILLS=off`; project-overridable) filters via `filter_skills` — disabled skills drop both the tool and the prompt section. The skill tool's bundled-file listing shows files only (directories are excluded).
- Templates: `render("... {{var}} ...", &vars)`.
- Project memory (cross-session, `memory.rs`): per-project JSONL at `~/.baiji/memory/<name-hash>.jsonl` (project key = dir name + path hash). Entries carry a kind (fact/decision/preference/gotcha), `learned_at`, optional `valid_until` TTL — expired entries are lazily evicted and never injected. Active entries (≤20, 200 chars each) render into the system prompt as `## Project memory`. The `memory` tool (add/list/forget) lets the LLM write entries; same-fact re-add refreshes instead of duplicating.
- Session todo list (`todo.rs`, long-horizon state externalization): the `todo` tool (add/update/list/clear) maintains `TodoItem { id, content, status, note }` in a `TodoStore` shared between the harness and the tool (Arc). The current list renders into the system prompt as `## Task list` — the system prompt never participates in compaction, so the plan survives context reduction and session resume. Persistence: a `Record::Todo { items }` snapshot is appended to the session JSONL at the end of any run that mutated the list (replay = last record wins; `branch` inherits, `switch_session` restores). `has_open_todos()` feeds the future auto-continue (T4).

### Tools (`baiji-tools`)

`read` (multi-mode JIT disclosure: `signatures` = symbol outline with line anchors → read exact ranges via offset/limit, `map` = compact directory tree, `full` = numbered lines with optional `density` (0.05-1.0) entropy-based line selection; over-budget full reads auto-degrade to `[auto-density …]` instead of hard truncation — disable via `ExecutionEnv::without_density_fallback`; **cached re-read**: in-process `(path, args)` cache keyed on `(mtime, size)` — an identical re-read of an unmodified file whose previous output was ≥2KB returns a short `[unchanged: … ctx:<handle>]` stub instead of resending the content, recoverable via `expand`; no ctx store / small outputs / `map` mode (dir mtime unreliable) never stub), `write` (creates parents), `edit` (exact-match replace; unique or `replace_all`), `bash` (`sh -c` in workdir, timeout + kill, exit/stdout/stderr, three-stage output compression: ANSI strip → generic rules (noise-line filtering + consecutive-duplicate folding `⟨… repeated N×⟩`) → content-aware domain compressors), `grep` (regex, depth ≤10, skips `.git`/`target`/`node_modules`/hidden, glob filter, ≤200 matches; consecutive same-file matches are grouped under a `File: path` header with `line: text` rows when shorter), `find` (name substring + kind filter), `ls` (dirs first), `expand` (retrieve truncated/compressed output by `ctx:` handle).
**Subagent task tool (T8)**: the `task` tool spawns a nested `AgentRuntime` (`subagent.rs`) with its own conversation, turn budget (default 12) and a read-only tool subset (`SUBAGENT_ALLOWED_TOOLS`: read/grep/find/ls/search/imports/expand — recursion depth 1, `task` is filtered out of the sub-registry). Only the final answer returns to the parent (capped at 16k chars with a truncation note); intermediate output never enters the parent history. Cancellation propagates for free — the parent runtime awaits tool futures in `select!`, so Esc drops the subagent's future. Subagent failure (max turns / LLM error) is an `is_error` tool result, not a run failure. v1 simplifications: subagent events are not forwarded to the parent stream, and the Provider is snapshotted at construction (hot swap does not propagate).

**Background jobs (T7)**: bash's `run_in_background: true` detaches a command (no timeout, process group, output tee'd to a temp file) and returns a job id immediately; the shared `JobRegistry` (`jobs.rs`) tracks state (Running/Finished/Stopped + exit code), a watchdog task reaps the child, and the `jobs` tool manages them (`list` / `output <id>` — delivered through the truncation+spill pipeline with a `ctx:` handle — / `stop <id>` — killpg). The registry's Drop kills all still-running jobs and removes output files, so process exit leaves no orphans. HITL applies to background bash exactly as to foreground.

**Domain compressors** (`compressors/`, inspired by lean-ctx / ANOLISA tokenless): after the generic shell rules and before byte truncation, output is classified and routed to a domain compressor — `json` (lossless: compact re-serialization + drop blacklisted diagnostic fields (debug/trace/stack/…) + drop null/empty values; lossy: arrays capped at 32 head + 8 tail, strings >4096 chars truncated, depth ≤8), `tabular` (CSV/TSV row reduction: >32 data rows → keep header + first/last 4 + diagnostic rows + even sampling to a 32-row budget; markdown pipe tables and ragged rows rejected), `build_log` (cargo/pytest/npm/go-style logs: only contiguous runs ≥9 of progress lines are reduced to first/last 2 with a counted elision marker — diagnostics, summaries, stack frames and rustc error blocks survive verbatim; >8 elision ranges aborts), plus `search_results` path sharing for grep. Three disciplines throughout: (1) lossless transforms need ≥15% savings to be adopted; (2) failed commands (exit≠0) get lossless cleanup only — diagnostic context is never lossy-compressed; (3) every lossy transform spills the full original to the ctx store first and carries `ctx:<handle>` in its marker — spill unavailable ⇒ no lossy compression (fail-open).

**tree-sitter index** (`signatures.rs` / `index.rs`): AST symbol extraction for Rust/Python/JS/TS/Go (`Symbol { name, kind, line_start, line_end, signature }`, ≤500/file; regex fallback for other languages or parse failures). `CodeIndex` scans on demand (≤2000 files, same skip rules as grep) building a symbol table + import edges per file. Tools built on it: `search` (BM25 over symbol+filename docs — identifiers split on camelCase/snake_case/abbreviations; results are `path:L start-end` anchors feeding read offset/limit) and `imports` (outgoing edges of a file / incoming importers = change impact).

`ExecutionEnv`: workdir, `allowed_roots` path whitelist with lexical `..` normalization (no fs canonicalization — works for not-yet-existing paths), byte-boundary-safe output truncation, `max_file_size` 1MB, bash timeout 30s default.

**CCR (reversible truncation)**: when output exceeds `max_output_bytes` or a lossy domain compression applies, and the ctx store is enabled (default: `~/.baiji/ctx-store/`), the full content is spilled to a SHA-256 content-addressed file and the marker carries `ctx:<handle16>`; the `expand` tool retrieves it (with offset/limit paging). Truncation is never information loss. Store lifecycle: handle files older than the TTL (default 7 days, `with_ctx_store_ttl`) are pruned on store init (once per process); only 16-hex handle-named files are ever removed.

**Context ledger**: tools that compress/truncate report `original_bytes` **and** `original_tokens` (heuristic: ASCII ~4 chars/token, CJK ~2 — canonical `baiji_agent::estimate_text_tokens`, shared with compaction) on `ToolOutput`; the runtime annotates `agent.tool` spans (`bytes_*` + `tokens_original`/`tokens_delivered`/`tokens_saved`) and `AgentEvent::ToolFinished`; the harness taps the event stream and appends a `Ledger { tool_calls, original_bytes, delivered_bytes, original_tokens, delivered_tokens }` record per run to the session JSONL (ignored on replay; token fields serde-default so old records parse). The TUI status bar shows cumulative bytes saved plus the token estimate. `policy.compression_enabled: false` (or env `BAIJI_COMPRESSION=off`) disables domain compression only — the A/B control arm keeps generic shell rules and truncation.

**Verbosity steer** (`policy.verbosity_steer`, env `BAIJI_VERBOSITY_STEER=on|off`): when enabled, the runtime appends a byte-constant conciseness note to the last user message of every request (lean-ctx measures ~1/3 output-token savings). Request-copy-only — session history and the JSONL never contain the injected text; byte-constancy keeps provider-side prefix caching intact.

### Extensions (`baiji-extensions`)

`Plugin::register(&mut PluginContext)` adds tools/hooks; `PluginManager::apply` merges into the registries. Built-ins: `ClockPlugin` (a `now` tool), `SafetyPlugin` (hook denying command-position `rm -rf` against `/`, `~`, `$HOME` — token-level detection, `echo rm -rf /` is not blocked).

MCP: `mcp` module ports the mcporter CLI bridge — `register_mcp_tools(&mut ToolRegistry, mcporter.json path)` discovers `server.tool` tools via `npx -y mcporter list --json` (30s timeout per server, per-server failures warn-and-skip) and wraps each as an `AgentTool` that shells out to `mcporter call`.

### Headless CLI & Telemetry backends

- `baiji -e "<msg>"` runs one turn without the TUI: text deltas stream to stdout, tool activity goes to stderr, session is persisted (resumable via `--session <id>` from `baiji --sessions`). Confirmation policy in headless mode: `--yes` auto-approves the configured confirmation list; without it, confirmation-gated tools are **denied** (`DenyAllApprover` — unattended-safe default). `--sessions` lists sessions directly from the JsonlStore (creates nothing, needs no API key).
- Telemetry: `BAIJI_TELEMETRY=file` (or `jsonl`) appends span/event records to `~/.baiji/traces/trace_<ts>.jsonl` via `JsonlTelemetry` (span_start with constructor attrs → span_end with runtime attrs/duration/error; zero-dependency unix-ms timestamps). Default remains `NoopTelemetry`; `RecordingTelemetry` serves tests.

### TUI (`baiji-tui`)

Chat / input / status layout. Streaming text renders into a partial line and lands as a full line on `RunCompleted`. Typing during a run pushes steering; Esc cancels via `CancellationToken`; PageUp/PageDown scroll with a `usize::MAX` stick-to-bottom sentinel clamped each frame.

- **Session picker** (`Ctrl+O`): centered overlay listing sessions (newest first, current marked `▸`, branch origin shown as `⎇parent`); `Enter` switches (chat rebuilt from replayed history), `b` branches the current session, `Esc` closes. Disabled while a run is active.
- **In-TUI configuration** (hot provider swap, no restart): `/config` opens a wizard overlay — vendor list → endpoint list (Coding Plan variants with notes) → API key input (typed into the input box; empty keeps existing/$ENV) → model picker (async discovery via the vendor's models API, with a manual-input fallback) → applied. `/model [name]` switches model directly (no arg opens the picker); `/status` shows the active settings plus a runtime summary (max_tokens/max_turns, compaction, retry, compression & verbosity switches, whether a project config is in effect). Apply = save config file (serde_json roundtrip preserving unknown fields) + rebuild provider (`settings::build_provider`, endpoint→protocol routing) + `AgentHarness::swap_provider` (RwLock-backed hot swap in `AgentRuntime`) + status-bar hint update. Blocked while a run is active.
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
