# Baiji Harness Engineering 优化方案

基于 2026 年 AI Agent Harness Engineering 范式，对 baiji 项目进行系统性优化。Harness Engineering 的核心理念是：**可靠的 AI Agent 不靠更好的 prompt，而靠更好的"护栏"**——即围绕 LLM 构建约束、反馈循环和上下文管理系统。

## User Review Required

> [!IMPORTANT]
> 本方案涉及 **6 个模块** 的新增/修改，工作量较大。建议按优先级分批实施，每个阶段独立可用。请确认：
> 1. 是否同意分 3 个阶段实施？
> 2. 是否有特别想优先实现的部分？

## Open Questions

> [!IMPORTANT]
> **工具权限策略**：内置工具 `builtin__write` 当前可写入任意路径。是否需要配置白名单限制可写目录？（如仅限 `./` 当前项目目录）

> [!IMPORTANT]
> **Token 预算**：当前 `max_tokens` 固定 4096。是否需要支持根据上下文动态调整（如工具结果很长时自动增大）？

---

## 现状差距分析

| Harness Engineering 支柱 | 当前 baiji 现状 | 差距 |
|---|---|---|
| **架构约束** | 仅有 `max_iterations=5` 限制 | 缺少工具权限控制、输入验证、安全边界 |
| **反馈循环** | 无自动校验 | 工具结果未验证，无 retry，无自我纠正 |
| **上下文管理** | 简单截断（>30 条保留 10 条） | 缺少摘要压缩、分层记忆、token 计数 |
| **可观测性** | 仅文件日志 | 缺少结构化 trace、性能指标、调用链追踪 |
| **容错设计** | 工具失败直接返回错误字符串 | 缺少重试、降级、熔断机制 |
| **工具契约** | 工具输入输出均为 `String`/`Value` | 缺少类型化验证、输出截断保护 |

---

## Proposed Changes

### 阶段一：架构约束 + 工具契约（核心安全层）

优先级最高，保障 Agent 行为边界。

---

#### [NEW] [tool_policy.rs](file:///Users/liming/Code/Github/MyCode/baiji/src/agent/tool_policy.rs)

**工具策略引擎**——定义 Agent "能做什么"和"不能做什么"：

```rust
// 核心结构
pub struct ToolPolicy {
    allowed_paths: Vec<PathBuf>,       // 文件操作白名单目录
    blocked_tools: HashSet<String>,     // 禁用的工具
    max_output_bytes: usize,           // 单次工具输出最大字节（防止 token 爆炸）
    require_confirmation: Vec<String>,  // 需要用户确认的高危工具（如 write）
}

pub enum PolicyDecision {
    Allow,
    Deny(String),           // 拒绝原因
    Truncate(usize),        // 允许但截断输出
    RequireConfirmation,    // 需要用户确认（HITL）
}
```

- 文件操作工具在执行前检查路径是否在白名单内
- 工具输出超过 `max_output_bytes`（默认 8KB）自动截断，附加 `[truncated]` 标记
- 支持在 `config.json` 中配置策略

#### [MODIFY] [agent.rs](file:///Users/liming/Code/Github/MyCode/baiji/src/agent/agent.rs)

在 `execute_tool()` 方法前插入策略检查：

```diff
 async fn execute_tool(&self, tool_name: &str, arguments: Value) -> (String, bool) {
+    // 策略检查
+    match self.policy.check(tool_name, &arguments) {
+        PolicyDecision::Deny(reason) => return (format!("[Blocked] {}", reason), true),
+        PolicyDecision::RequireConfirmation => {
+            // 通过 AgentEvent 请求用户确认
+            // ...
+        }
+        _ => {}
+    }
+
     if tool_name.starts_with("builtin__") {
```

#### [MODIFY] [builtin_tools.rs](file:///Users/liming/Code/Github/MyCode/baiji/src/agent/builtin_tools.rs)

增加输入验证和输出保护：

- `builtin__write`：验证路径不包含 `..`、不在系统目录
- `builtin__grep`：限制搜索深度（最大递归 10 层）、限制单文件大小（跳过 >1MB 文件）
- 所有工具输出添加字节长度限制

#### [MODIFY] [config.rs](file:///Users/liming/Code/Github/MyCode/baiji/src/config.rs)

添加策略配置段：

```json
{
  "policy": {
    "allowed_paths": ["./"],
    "max_tool_output_bytes": 8192,
    "blocked_tools": [],
    "require_confirmation_tools": ["builtin__write"]
  }
}
```

---

### 阶段二：反馈循环 + 容错（可靠性层）

让 Agent 具备自我纠正和容错能力。

---

#### [NEW] [retry.rs](file:///Users/liming/Code/Github/MyCode/baiji/src/agent/retry.rs)

**重试与降级引擎**：

```rust
pub struct RetryPolicy {
    max_retries: u32,           // 最大重试次数（默认 2）
    backoff_ms: u64,            // 退避时间
    retryable_errors: Vec<String>, // 可重试的错误模式
}

pub enum RetryAction {
    Retry { delay: Duration },
    Fallback(String),    // 降级为固定响应
    Fail,                // 直接失败
}
```

- LLM API 调用失败（网络/限流/500）自动重试，指数退避
- MCP 工具调用超时自动重试 1 次
- 工具连续失败 2 次后跳过该工具，注入 `[Tool unavailable]` 告知 LLM

#### [NEW] [validator.rs](file:///Users/liming/Code/Github/MyCode/baiji/src/agent/validator.rs)

**工具结果验证器**：

```rust
pub struct ToolResultValidator;

impl ToolResultValidator {
    /// 验证工具输出是否合理
    pub fn validate(tool_name: &str, result: &str) -> ValidationResult {
        // 1. 空结果检查
        // 2. 错误模式检测（如 "permission denied"、"not found"）
        // 3. 输出格式基本校验
    }
}

pub enum ValidationResult {
    Valid,
    Empty,                      // 空结果，建议 LLM 换策略
    Error(String),              // 检测到错误，注入诊断信息
    Suspicious(String),         // 可疑输出，添加警告
}
```

- 验证结果会以 `[Observation]` 标签注入到 LLM 上下文，辅助自我纠正
- 比如 grep 返回空结果时自动注入："No matches found. Consider broadening the search pattern or checking the path."

#### [MODIFY] [agent.rs](file:///Users/liming/Code/Github/MyCode/baiji/src/agent/agent.rs)

在工具执行后添加验证 + 重试逻辑：

```diff
 let (result, is_error) = self
     .execute_tool(&tool_call.name, tool_call.arguments.clone())
     .await;
+
+// 验证工具结果
+let validation = ToolResultValidator::validate(&tool_call.name, &result);
+let result = match validation {
+    ValidationResult::Empty => format!("{}\n[Observation: empty result]", result),
+    ValidationResult::Error(hint) => format!("{}\n[Observation: {}]", result, hint),
+    _ => result,
+};
```

---

### 阶段三：上下文工程 + 可观测性（智能层）

让 Agent 更聪明地管理记忆，让开发者更好地调试。

---

#### [NEW] [context.rs](file:///Users/liming/Code/Github/MyCode/baiji/src/agent/context.rs)

**分层上下文管理器**，取代当前的简单截断：

```rust
pub struct ContextManager {
    /// 估算的 token 上限
    max_tokens: usize,
    /// 历史摘要（长期记忆）
    summaries: Vec<String>,
}

impl ContextManager {
    /// 智能裁剪上下文
    /// 1. 保留 system prompt（不裁剪）
    /// 2. 保留最近 N 轮完整对话
    /// 3. 中间对话压缩为摘要
    /// 4. 基于 token 估算（每个 char ≈ 0.3 token 中文 / 0.25 token 英文）
    pub fn trim(&mut self, messages: &mut Vec<Message>) {
        // ...
    }

    /// 将旧对话压缩为摘要
    fn summarize(messages: &[Message]) -> String {
        // 简单启发式：提取每轮的 user 问题 + assistant 首句
    }
}
```

与现有 `trim_context` 的区别：
- **有摘要而非丢弃**：被裁剪的对话会被压缩为 1-2 句摘要保留在上下文中
- **基于 token 估算**：而非固定消息条数
- **保留工具调用记录的摘要**：如 "Called grep on src/, found 5 matches"

#### [NEW] [trace.rs](file:///Users/liming/Code/Github/MyCode/baiji/src/agent/trace.rs)

**结构化调用链追踪**：

```rust
#[derive(Debug, Serialize)]
pub struct AgentTrace {
    pub run_id: String,
    pub started_at: DateTime<Local>,
    pub turns: Vec<TurnTrace>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
}

#[derive(Debug, Serialize)]
pub struct TurnTrace {
    pub turn_number: usize,
    pub llm_latency_ms: u64,
    pub tool_calls: Vec<ToolCallTrace>,
    pub context_messages: usize,
    pub estimated_tokens: usize,
}

#[derive(Debug, Serialize)]
pub struct ToolCallTrace {
    pub tool_name: String,
    pub latency_ms: u64,
    pub input_size: usize,
    pub output_size: usize,
    pub success: bool,
    pub retried: bool,
}
```

- 每次 Agent 运行生成一个 JSON trace 文件，保存到 `logs/traces/`
- UI 状态栏展示实时指标：当前 turn 数、累计 token、工具调用数
- 方便事后分析 Agent 行为和性能瓶颈

#### [MODIFY] [ui/mod.rs](file:///Users/liming/Code/Github/MyCode/baiji/src/ui/mod.rs)

状态栏增加实时指标显示：

```diff
 let status_text = if app.is_streaming {
     Span::styled(
-        "⏳ Streaming...",
+        format!("⏳ Turn {} | {} tokens", app.current_turn, app.total_tokens),
         Style::default().fg(Color::Yellow),
     )
 }
```

---

## 文件变更总览

| 阶段 | 操作 | 文件 | 说明 |
|------|------|------|------|
| 1 | NEW | `src/agent/tool_policy.rs` | 工具策略引擎 |
| 1 | MODIFY | `src/agent/agent.rs` | 策略检查集成 |
| 1 | MODIFY | `src/agent/builtin_tools.rs` | 输入验证 + 输出保护 |
| 1 | MODIFY | `src/agent/mod.rs` | 注册新模块 |
| 1 | MODIFY | `src/config.rs` | 策略配置 |
| 2 | NEW | `src/agent/retry.rs` | 重试与降级 |
| 2 | NEW | `src/agent/validator.rs` | 工具结果验证 |
| 2 | MODIFY | `src/agent/agent.rs` | 验证 + 重试集成 |
| 3 | NEW | `src/agent/context.rs` | 分层上下文管理 |
| 3 | NEW | `src/agent/trace.rs` | 调用链追踪 |
| 3 | MODIFY | `src/agent/agent.rs` | 上下文 + trace 集成 |
| 3 | MODIFY | `src/ui/mod.rs` | 状态栏指标展示 |

## Verification Plan

### Automated Tests

每个新模块都附带单元测试：

```bash
cargo test                    # 全量测试
cargo test tool_policy        # 策略引擎测试
cargo test retry              # 重试逻辑测试
cargo test validator          # 验证器测试
cargo test context            # 上下文管理测试
```

关键测试用例：
- 路径穿越攻击 (`../../etc/passwd`) 被策略阻止
- 工具输出超过 8KB 被正确截断
- LLM 调用失败后成功重试
- 上下文超过 token 上限后正确压缩保留摘要
- trace JSON 文件格式正确且包含完整调用链

### Manual Verification

1. 运行 `cargo run`，尝试让 Agent 写入 `/etc/` 目录，验证策略阻止
2. 断开网络后发送消息，验证重试和错误提示
3. 连续对话 20 轮以上，观察上下文压缩行为
4. 检查 `logs/traces/` 下生成的 trace 文件
