# Notes: Rust Agent 项目架构研究

## 技术选型

### LLM Provider 抽象
```rust
// 核心 trait 设计
#[async_trait]
pub trait LLMProvider: Send + Sync {
    async fn chat(&self, messages: Vec<Message>) -> Result<String>;
    async fn chat_stream(&self, messages: Vec<Message>) -> Result<Stream>;
    fn supports_tools(&self) -> bool;
}
```

### ReAct Agent 流程
```
User Input
    ↓
System Prompt + History + User Input → LLM
    ↓
LLM Response (Thought + Action)
    ↓
Parse Action → Execute Tool / Final Answer
    ↓
Observation → Append to History
    ↓
Continue loop until Final Answer
```

### MCP 工具转换
MCP Tool 格式需要转换为 LLM Function Call 格式：
```rust
// MCP Tool
{
    "name": "search",
    "description": "Search the web",
    "inputSchema": { ... }
}

// Convert to OpenAI Function
{
    "name": "mcp_server_name__tool_name",
    "description": "Search the web",
    "parameters": { ... }
}
```

## 目录结构规划

```
baiji/
├── Cargo.toml
├── config.example.toml
├── src/
│   ├── main.rs           # 程序入口
│   ├── app.rs            # 应用主循环
│   ├── config.rs         # 配置管理
│   ├── event.rs          # 事件处理（键盘/输入）
│   ├── ui/
│   │   ├── mod.rs        # UI 模块入口
│   │   ├── layout.rs     # 布局定义
│   │   ├── chat.rs       # 对话渲染
│   │   └── input.rs      # 输入框渲染
│   ├── llm/
│   │   ├── mod.rs        # LLM 模块入口
│   │   ├── provider.rs   # Provider trait
│   │   ├── anthropic.rs  # Claude 实现
│   │   ├── openai.rs     # OpenAI 实现（预留）
│   │   └── types.rs      # 通用类型定义
│   ├── agent/
│   │   ├── mod.rs        # Agent 模块入口
│   │   ├── react.rs      # ReAct 实现
│   │   └── prompt.rs     # 提示词模板
│   ├── mcp/
│   │   ├── mod.rs        # MCP 模块入口
│   │   ├── client.rs     # MCP 客户端管理
│   │   └── tools.rs      # 工具转换
│   └── tools/
│       ├── mod.rs        # 工具执行
│       └── executor.rs   # 执行器
```

## 依赖列表

```toml
[dependencies]
# 异步运行时
tokio = { version = "1", features = ["full"] }

# 终端 UI
ratatui = "0.29"
crossterm = "0.28"

# HTTP 和 SSE
reqwest = { version = "0.12", features = ["json", "stream"] }
eventsource-stream = "0.2"
futures = "0.3"

# MCP SDK
rmcp = { version = "0.1", features = ["client"] }

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# 错误处理
thiserror = "2"
anyhow = "1"

# 工具
dirs = "6"          # 获取配置目录
async-trait = "0.1"
tokio-stream = "0.1"
```

## ReAct 提示词模板

```markdown
You are an AI assistant that can use tools to help answer questions.

When you need to use a tool, respond in this format:
Thought: <your reasoning about what to do next>
Action: <tool_name>
Action Input: <json object with tool parameters>

When you have the final answer, respond in this format:
Thought: <your reasoning>
Final Answer: <your answer>

Available tools:
{tools_description}

Begin!

Question: {question}
```
