# Rust Agent 架构设计文档

## 系统架构图

```
┌─────────────────────────────────────────────────────────────────┐
│                         Terminal UI (Ratatui)                    │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐   │
│  │ Chat History │  │ Input Box    │  │ Status Bar           │   │
│  └──────────────┘  └──────────────┘  └──────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                         App Controller                           │
│         (状态管理、事件循环、协调各模块)                           │
└─────────────────────────────────────────────────────────────────┘
                              │
          ┌───────────────────┼───────────────────┐
          ▼                   ▼                   ▼
┌─────────────────┐  ┌─────────────────┐  ┌─────────────────────┐
│   Config Manager │  │  ReAct Agent    │  │   LLM Provider      │
│   (配置管理)     │  │  (核心逻辑)      │  │   (API 客户端)       │
└─────────────────┘  └─────────────────┘  └─────────────────────┘
                              │                   │
                              ▼                   ▼
                    ┌─────────────────┐  ┌──────────────┐
                    │  Tool Executor  │  │ Anthropic   │
                    │  (工具执行器)    │  │ Claude API  │
                    └─────────────────┘  └──────────────┘
                              │
                              ▼
                    ┌─────────────────┐
                    │  MCP Client     │
                    │  (MCP 客户端)    │
                    └─────────────────┘
                              │
                              ▼
                    ┌─────────────────┐
                    │  MCP Servers    │
                    │  (外部工具服务)  │
                    └─────────────────┘
```

## 模块详细设计

### 1. Config 模块 (`src/config.rs`)

**职责**: 管理应用程序配置

**配置文件路径**:
- Linux: `~/.config/baiji/config.json`
- macOS: `~/Library/Application Support/baiji/config.json`
- Windows: `%APPDATA%/baiji/config.json`

**配置格式**: JSON

**结构**:
```rust
#[derive(Debug, Deserialize)]
pub struct Config {
    pub llm: LLMConfig,
    pub mcp_servers: HashMap<String, MCPServerConfig>,
    pub ui: Option<UIConfig>,
}

#[derive(Debug, Deserialize)]
pub struct LLMConfig {
    pub provider: String,      // "anthropic" | "openai"
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
pub struct MCPServerConfig {
    pub command: String,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
}
```

**配置文件示例** (`config.json`):
```json
{
  "llm": {
    "provider": "anthropic",
    "base_url": "https://api.anthropic.com",
    "api_key": "$ANTHROPIC_API_KEY",
    "model": "claude-3-5-sonnet-20241022",
    "max_tokens": 4096,
    "temperature": 0.7
  },
  "mcp_servers": {
    "tavily": {
      "command": "npx",
      "args": ["-y", "@tavily/mcp"],
      "env": {
        "TAVILY_API_KEY": "your-key"
      }
    },
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/Users/liming/Code"]
    }
  },
  "ui": {
    "theme": "dark"
  }
}
```

### 2. LLM 模块 (`src/llm/`)

**职责**: 提供统一的 LLM 调用接口

**核心 Trait**:
```rust
#[async_trait]
pub trait LLMProvider: Send + Sync {
    /// 非流式对话
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// 流式对话
    async fn chat_stream(&self, request: ChatRequest) -> Result<BoxStream<'static, Result<StreamChunk>>>;

    /// 是否支持工具调用
    fn supports_tools(&self) -> bool;
}

pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolDefinition>>,
}

pub struct Message {
    pub role: Role,
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_results: Option<Vec<ToolResult>>,
}

pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}
```

**Anthropic 实现要点**:
- 使用 Messages API
- 支持 `stream: true` 获取 SSE 流
- 工具调用通过 `tool_use` / `tool_result` 内容块实现

### 3. Agent 模块 (`src/agent/`)

**职责**: 实现 ReAct 推理循环

**核心结构**:
```rust
pub struct ReActAgent {
    llm: Arc<dyn LLMProvider>,
    tool_executor: Arc<ToolExecutor>,
    max_iterations: u32,
}

pub struct AgentStep {
    pub thought: String,
    pub action: Option<Action>,
    pub observation: Option<String>,
    pub is_final: bool,
}

pub enum Action {
    ToolCall { name: String, input: Value },
    FinalAnswer { answer: String },
}
```

**ReAct 循环流程**:
```rust
impl ReActAgent {
    pub async fn run(&self, question: &str) -> Result<String> {
        let mut context = vec![Message::user(question)];

        for i in 0..self.max_iterations {
            // 1. 调用 LLM
            let response = self.llm.chat(ChatRequest {
                messages: context.clone(),
                tools: self.get_available_tools(),
            }).await?;

            // 2. 解析响应
            let step = self.parse_response(&response)?;

            // 3. 如果是最终答案，返回
            if step.is_final {
                return Ok(step.action.unwrap().answer());
            }

            // 4. 执行工具
            if let Some(Action::ToolCall { name, input }) = step.action {
                let observation = self.tool_executor.execute(&name, input).await?;

                // 5. 将 observation 加入上下文
                context.push(Message::assistant(&response.content));
                context.push(Message::tool(&observation));
            }
        }

        Err(anyhow!("Max iterations reached"))
    }
}
```

### 4. MCP 模块 (`src/mcp/`)

**职责**: 管理 MCP 客户端连接和工具转换

**核心结构**:
```rust
pub struct MCPClientManager {
    clients: HashMap<String, Client<StdioTransport>>,
    tool_registry: ToolRegistry,
}

pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
}

pub struct RegisteredTool {
    pub server_name: String,
    pub tool_name: String,
    pub schema: ToolSchema,
}
```

**工具转换逻辑**:
```rust
impl MCPClientManager {
    /// 将 MCP tools 转换为 LLM 可用的工具定义
    pub fn to_llm_tools(&self) -> Vec<ToolDefinition> {
        self.tool_registry
            .tools
            .values()
            .map(|t| ToolDefinition {
                name: format!("{}__{}", t.server_name, t.tool_name),
                description: t.schema.description.clone(),
                parameters: t.schema.parameters.clone(),
            })
            .collect()
    }

    /// 执行工具调用
    pub async fn execute(&self, full_name: &str, args: Value) -> Result<String> {
        let (server, tool) = parse_tool_name(full_name)?;
        let client = self.clients.get(server).ok_or(...)?;

        let result = client.call_tool(tool, args).await?;
        Ok(serialize_result(result))
    }
}
```

### 5. UI 模块 (`src/ui/`)

**职责**: 渲染终端界面

**布局设计**:
```
┌────────────────────────────────────────────────────────┐
│ Chat History (flex: 1)                                 │
│                                                        │
│ User: Hello!                                           │
│ Assistant: Hi there! How can I help?                   │
│ ...                                                    │
│                                                        │
├────────────────────────────────────────────────────────┤
│ Input Box (fixed: 3 lines)                             │
│ > Type here...                                         │
├────────────────────────────────────────────────────────┤
│ Status Bar (fixed: 1 line)                             │
│ Model: claude-3-5-sonnet | Status: Ready | [?] Help   │
└────────────────────────────────────────────────────────┘
```

**主要组件**:
- `ChatPanel`: 显示对话历史，支持滚动
- `InputPanel`: 多行输入框，支持编辑
- `StatusBar`: 显示当前状态信息

### 6. App 模块 (`src/app.rs`)

**职责**: 应用主循环，协调各模块

```rust
pub struct App {
    config: Config,
    llm: Arc<dyn LLMProvider>,
    agent: ReActAgent,
    ui_state: UIState,
    event_rx: Receiver<Event>,
}

pub enum Event {
    Key(KeyEvent),
    Tick,
    LLMResponse(StreamChunk),
    ToolResult(String),
}

impl App {
    pub async fn run(&mut self) -> Result<()> {
        // 初始化终端
        let mut terminal = setup_terminal()?;

        // 主循环
        loop {
            // 渲染 UI
            self.draw(&mut terminal)?;

            // 处理事件
            match self.event_rx.recv().await? {
                Event::Key(key) => self.handle_key(key).await?,
                Event::LLMResponse(chunk) => self.handle_llm_chunk(chunk),
                Event::ToolResult(result) => self.handle_tool_result(result),
                Event::Tick => {},
            }

            // 退出条件
            if self.ui_state.should_quit {
                break;
            }
        }

        restore_terminal()?;
        Ok(())
    }
}
```

## 数据流图

### 普通对话流程
```
User Input
    ↓
App::handle_key(Enter)
    ↓
添加消息到 UI 历史
    ↓
调用 LLM::chat_stream()
    ↓
SSE 流 → Event::LLMResponse(chunk)
    ↓
实时更新 UI
    ↓
流结束，完整消息保存到历史
```

### Agent 工具调用流程
```
User Input (with /agent prefix)
    ↓
启动 ReAct Agent
    ↓
Agent::run()
    │
    ├── LLM 调用
    │       ↓
    ├── 解析 Thought/Action
    │       ↓
    ├── 如果是 ToolCall
    │       ↓
    ├── ToolExecutor::execute()
    │       ↓
    ├── MCPClientManager::execute()
    │       ↓
    ├── MCP Server 执行
    │       ↓
    ├── 返回 Observation
    │       ↓
    └── 循环继续...
    ↓
返回 Final Answer
    ↓
显示结果
```

## 错误处理策略

1. **配置错误**: 启动时检查，提供友好的错误提示
2. **网络错误**: 重试机制 + 用户提示
3. **MCP 连接错误**: 记录警告，继续运行（降级为无工具模式）
4. **LLM API 错误**: 显示在状态栏，允许用户重试

## 扩展性设计

### 添加新的 LLM Provider
1. 实现 `LLMProvider` trait
2. 在配置中添加 provider 类型
3. 在工厂方法中注册新 provider

### 添加新的 MCP Server
只需在配置中添加新的 server 配置，无需修改代码。

### 自定义工具执行器
可以实现 `ToolExecutor` trait，支持除 MCP 外的其他工具来源。
