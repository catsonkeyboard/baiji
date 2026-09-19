//! 子代理 task 工具（T8）——探索型子任务的上下文隔离 + 可定义角色
//!
//! 子任务（"找出所有调用点并总结"）的中间输出会污染主上下文；
//! `task` 工具 spawn 一个嵌套 [`AgentRuntime`]（独立对话、独立轮次预算、
//! 默认只读工具集）干脏活，只回传最终答案——中间输出全部丢弃。
//! 对主上下文的保护与 P3 的可逆 stub 是同一哲学的放大。
//!
//! - 可定义角色（agent 文件）：`~/.baiji/agents/*.md`（用户级）+
//!   `./.baiji/agents/*.md`（项目级，同名覆盖）。frontmatter 声明
//!   name/description/tools/model/thinking/max_turns，正文即子代理系统提示。
//!   调用方在 task 的 `agent` 参数里按名分发（[`AgentRole`] / [`SubagentRegistry`]）
//! - 并行编排：`task` 声明 [`AgentTool::parallel`]——一轮里的多个 task 调用
//!   由父 runtime 并发执行（join_all），结果按原顺序配对回传
//! - 递归深度上限 1：子工具集构造时剔除 `task` 自身
//! - 取消传播：父 runtime 在 `select!` 中 await 工具 future，Esc 取消时
//!   future 被 drop，子代理的内部流随之中止（无额外机制）
//! - 子代理失败（超轮次/LLM 错误）以 `is_error` 工具结果返回，不炸父 run
//! - 已知简化：子代理事件不向父流转发（上下文隔离的代价）；Provider 按
//!   角色构建后缓存，父级热切换不传播（重载角色可刷新）

use crate::AgentRuntime;
use crate::event::AgentEvent;
use crate::queue::SteeringQueue;
use crate::tool::{AgentTool, ToolOutput, ToolRegistry};
use baiji_ai::{Message, Provider, ThinkingLevel};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 子代理默认可用的工具（只读集；写入类工具由装配方显式追加）
pub const SUBAGENT_ALLOWED_TOOLS: &[&str] =
    &["read", "grep", "find", "ls", "search", "imports", "expand"];

/// 子代理系统提示
const SUB_SYSTEM_PROMPT: &str = "\
You are a focused subagent executing a single research task inside a parent agent's session. \
Complete the task with the tools available, then return a concise, self-contained result — \
the parent ONLY sees your final answer, all intermediate output is discarded. \
Locate with search/grep first, read only the ranges you need, and cite file:line in the result.";

/// 子代理单次运行的轮次上限（防失控；父级/角色可在构造时覆盖）。
/// 研究型任务常需 10+ 次检索——12 轮太紧（跑满即失败），20 + 运行时的
/// 倒计时收尾提醒让子代理几乎总能带回结果
const DEFAULT_MAX_TURNS: u32 = 20;
/// 回传答案的字符上限——超长说明任务应拆小，截断并提示
const MAX_ANSWER_CHARS: usize = 16 * 1024;

// ===== 可定义角色（agent 文件）=====

/// 一个可定义子代理角色（`agents/<name>.md`：frontmatter + 系统提示正文）
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRole {
    pub name: String,
    pub description: String,
    /// frontmatter 之后的正文 = 子代理系统提示（空 = 沿用默认子代理提示）
    pub system_prompt: String,
    /// 工具子集（角色限定；None = 全部只读工具）
    pub tools: Option<Vec<String>>,
    /// 指定模型（构建独立 provider；None = 继承父级）
    pub model: Option<String>,
    /// 思考级别（None = 不启用）
    pub thinking: Option<ThinkingLevel>,
    /// 轮次上限（None = 工具默认）
    pub max_turns: Option<u32>,
}

/// 从多个角色目录加载（后者覆盖同名前者；同目录内同名文件后读覆盖——
/// 目录序由装配方保证确定性）。`*.md` 之外的文件忽略，解析失败跳过
pub fn load_agent_roles(dirs: &[PathBuf]) -> Vec<AgentRole> {
    let mut roles: Vec<AgentRole> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        files.sort(); // 目录内确定性
        for file in files {
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue;
            };
            let fallback_name = file
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Some(role) = parse_agent_role(&content, &fallback_name) {
                if let Some(existing) = roles.iter_mut().find(|r| r.name == role.name) {
                    *existing = role;
                } else {
                    roles.push(role);
                }
            }
        }
    }
    roles.sort_by(|a, b| a.name.cmp(&b.name));
    roles
}

/// 解析单个 agent 文件：`---` frontmatter（name/description/tools/model/
/// thinking/max_turns）+ 正文系统提示。name 缺省取文件名；frontmatter 缺失
/// 或 name 解析为空 → None（跳过该文件）
pub fn parse_agent_role(content: &str, fallback_name: &str) -> Option<AgentRole> {
    let (frontmatter, body) = split_frontmatter(content);
    let mut name = None;
    let mut description = String::new();
    let mut tools = None;
    let mut model = None;
    let mut thinking = None;
    let mut max_turns = None;
    if let Some(fm) = &frontmatter {
        for line in fm.lines() {
            if let Some(v) = parse_field(line, "name") {
                name = Some(v);
            } else if let Some(v) = parse_field(line, "description") {
                description = v;
            } else if let Some(v) = parse_field(line, "model") {
                model = Some(v);
            } else if let Some(v) = parse_field(line, "thinking") {
                thinking = ThinkingLevel::parse(&v);
            } else if let Some(v) = parse_field(line, "max_turns") {
                max_turns = v.parse::<u32>().ok();
            } else if let Some(v) = parse_field(line, "tools") {
                // `[a, b]` 或逗号分隔
                let list = v
                    .trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>();
                if !list.is_empty() {
                    tools = Some(list);
                }
            }
        }
    }
    let name = match name {
        // parse_field 过滤空值：能到这里的都是非空显式名
        Some(n) => n,
        None if !fallback_name.is_empty() => fallback_name.to_string(),
        None => return None,
    };
    Some(AgentRole {
        name,
        description,
        system_prompt: body.trim().to_string(),
        tools,
        model,
        thinking,
        max_turns,
    })
}

/// 分离 `---` 包裹的 frontmatter 与正文
fn split_frontmatter(content: &str) -> (Option<String>, String) {
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let frontmatter = rest[..end].trim().to_string();
            let body = rest[end + 4..]
                .trim_start_matches('-')
                .trim_start()
                .to_string();
            return (Some(frontmatter), body);
        }
    }
    (None, content.to_string())
}

/// `key: value` 行解析（引号去除；空值返回 None）
fn parse_field(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    line.trim()
        .strip_prefix(&prefix)
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

/// 角色注册表：`SubagentTool`（分发）与 harness（系统提示段）共享同一 Arc；
/// RwLock 支持 TUI `/subagents r` 运行中热重载
pub struct SubagentRegistry {
    dirs: Vec<PathBuf>,
    roles: std::sync::RwLock<Vec<AgentRole>>,
}

impl SubagentRegistry {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self {
            dirs,
            roles: std::sync::RwLock::new(Vec::new()),
        }
    }

    /// 从目录（重新）加载，返回角色数
    pub fn load(&self) -> usize {
        let roles = load_agent_roles(&self.dirs);
        let count = roles.len();
        *self.roles.write().unwrap() = roles;
        count
    }

    /// 当前角色快照
    pub fn roles(&self) -> Vec<AgentRole> {
        self.roles.read().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.roles.read().unwrap().len()
    }

    /// 按名查找
    pub fn find(&self, name: &str) -> Option<AgentRole> {
        self.roles
            .read()
            .unwrap()
            .iter()
            .find(|r| r.name == name)
            .cloned()
    }

    /// 角色目录（管理界面展示）
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }
}

/// `task` 工具：独立上下文跑子任务，回传最终答案。
/// 指定 `agent` 参数时按 [`AgentRole`] 分发（专属系统提示/工具集/模型/预算）
pub struct SubagentTool {
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn AgentTool>>,
    max_turns: u32,
    registry: Option<Arc<SubagentRegistry>>,
    /// 角色指定 model 时构建独立 provider 的原料（None = 角色只能继承父 provider）
    provider_config: Option<baiji_ai::ProviderConfig>,
    /// 已构建的角色 provider 缓存（model → provider）
    role_providers: std::sync::Mutex<std::collections::HashMap<String, Arc<dyn Provider>>>,
}

impl SubagentTool {
    pub fn new(provider: Arc<dyn Provider>, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        Self {
            provider,
            // 递归防护：深度上限 1——子工具集里绝不含 task 自身
            tools: tools
                .into_iter()
                .filter(|t| t.name() != Self::name_static())
                .collect(),
            max_turns: DEFAULT_MAX_TURNS,
            registry: None,
            provider_config: None,
            role_providers: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns.max(1);
        self
    }

    /// 启用可定义角色（`agent` 参数分发）
    pub fn with_roles(mut self, registry: Arc<SubagentRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// 提供构建角色专属 provider 的原料（角色 `model` 字段生效的前提）
    pub fn with_provider_config(mut self, config: baiji_ai::ProviderConfig) -> Self {
        self.provider_config = Some(config);
        self
    }

    fn name_static() -> &'static str {
        "task"
    }

    /// 子代理实际可用的工具名（测试/诊断用）
    pub fn sub_tool_names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// 角色专属 provider：按 model 构建并缓存；无原料或构建失败回退父级
    fn provider_for_role(&self, role: &AgentRole) -> Arc<dyn Provider> {
        let Some(model) = &role.model else {
            return self.provider.clone();
        };
        if let Some(cached) = self.role_providers.lock().unwrap().get(model) {
            return cached.clone();
        }
        let built = self.provider_config.clone().and_then(|mut config| {
            config.model = model.clone();
            baiji_ai::build_provider(config).ok()
        });
        match built {
            Some(provider) => {
                self.role_providers
                    .lock()
                    .unwrap()
                    .insert(model.clone(), provider.clone());
                provider
            }
            None => {
                tracing::warn!(
                    "subagent role '{}' model '{model}' unavailable, falling back to the parent provider",
                    role.name
                );
                self.provider.clone()
            }
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for SubagentTool {
    fn name(&self) -> &str {
        Self::name_static()
    }

    fn description(&self) -> &str {
        "Run a subagent with its OWN context on a research subtask (e.g. 'find all callers \
         of X and summarize'). The subagent has read-only tools, its own turn budget, and \
         returns only its final answer — intermediate output never enters this conversation. \
         Pass 'agent' to dispatch a named role from the 'Subagents' system-prompt section \
         (its own prompt/tools/model). Issue MULTIPLE task calls in one turn to run \
         subagents in parallel. Use for broad exploration; do direct reads/greps for \
         simple lookups. The prompt must be self-contained (the subagent sees nothing \
         of this conversation)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "Self-contained subtask description"},
                "agent": {"type": "string", "description": "Optional role name from the 'Subagents' section — uses that role's prompt/tools/model/budget"},
                "tools": {"type": "array", "items": {"type": "string"}, "description": "Extra tool names to allow beyond the read-only default (optional; a role's own tool list takes precedence)"}
            },
            "required": ["prompt"]
        })
    }

    /// 子代理各自独立上下文：同轮多个 task 可并发执行
    fn parallel(&self) -> bool {
        true
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let Some(prompt) = args["prompt"]
            .as_str()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            return Ok(ToolOutput::err(
                "[Error] task requires a non-empty 'prompt'",
            ));
        };

        // 角色分发：按名取角色；未知角色列出可用项
        let role = match args["agent"]
            .as_str()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            Some(agent_name) => match self.registry.as_ref().and_then(|r| r.find(agent_name)) {
                Some(role) => Some(role),
                None => {
                    let available: Vec<String> = self
                        .registry
                        .as_ref()
                        .map(|r| r.roles().iter().map(|x| x.name.clone()).collect())
                        .unwrap_or_default();
                    return Ok(ToolOutput::err(format!(
                        "[Error] unknown agent '{agent_name}'. Available: {}",
                        if available.is_empty() {
                            "(none defined)".to_string()
                        } else {
                            available.join(", ")
                        }
                    )));
                }
            },
            None => None,
        };

        // 子注册表：角色限定集 > 调用点指定集 > 全部（仍在只读白名单内）
        let extra: Vec<String> = args["tools"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let allow: Option<Vec<String>> = role
            .as_ref()
            .and_then(|r| r.tools.clone())
            .or((!extra.is_empty()).then_some(extra));
        let mut registry = ToolRegistry::new();
        for tool in &self.tools {
            let ok = allow
                .as_ref()
                .is_none_or(|names| names.iter().any(|name| name == tool.name()));
            if ok {
                registry.register(tool.clone());
            }
        }

        // 角色专属：系统提示 / provider / 预算 / 思考级别（缺省回落工具级配置）
        let system_prompt = role
            .as_ref()
            .map(|r| r.system_prompt.trim())
            .filter(|p| !p.is_empty())
            .unwrap_or(SUB_SYSTEM_PROMPT);
        let provider = role
            .as_ref()
            .map(|r| self.provider_for_role(r))
            .unwrap_or_else(|| self.provider.clone());
        let max_turns = role
            .as_ref()
            .and_then(|r| r.max_turns)
            .unwrap_or(self.max_turns)
            .max(1);
        let thinking = role.as_ref().and_then(|r| r.thinking);

        // 嵌套 runtime：独立对话与预算；事件排空（不向父流转发）
        let runtime = AgentRuntime::new(provider)
            .with_tools(registry)
            .with_limits(max_turns, 8192)
            .with_thinking(thinking);
        let mut messages = vec![Message::user(prompt)];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = runtime
            .run(
                system_prompt,
                &mut messages,
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await;
        drop(tx);
        let _ = drain.await;

        match result {
            Ok(answer) => {
                // 答案超长：截断并标注（子代理产物即摘要；要完整细节应缩小任务）
                let chars = answer.chars().count();
                if chars <= MAX_ANSWER_CHARS {
                    Ok(ToolOutput::ok(answer))
                } else {
                    let cut: String = answer.chars().take(MAX_ANSWER_CHARS).collect();
                    let original_bytes = answer.len() as u64;
                    Ok(ToolOutput::ok(format!(
                        "{cut}\n\n[subagent answer truncated at {MAX_ANSWER_CHARS} of {chars} chars — \
                         narrow the task prompt and re-run for full detail]"
                    ))
                    .with_original_bytes(original_bytes))
                }
            }
            // 子代理失败不炸父 run：以错误工具结果回传，父模型可重试或换法
            Err(e) => Ok(ToolOutput::err(format!("[Subagent failed] {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baiji_ai::{ChatRequest, ChatResponse, Protocol, StreamChunk};
    use futures::StreamExt as _;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 每一轮都返回工具调用；计数见底后回答（脚本化）
    struct ScriptProvider {
        /// 每轮都发工具调用（永不回答）→ 触发子代理 max_turns
        always_tool: bool,
        /// 最终答案（默认短）
        answer: &'static str,
        calls: AtomicU32,
    }

    impl ScriptProvider {
        fn answering(answer: &'static str) -> Self {
            Self {
                always_tool: false,
                answer,
                calls: AtomicU32::new(0),
            }
        }

        fn never_answering() -> Self {
            Self {
                always_tool: true,
                answer: "",
                calls: AtomicU32::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for ScriptProvider {
        async fn chat(&self, _: ChatRequest) -> anyhow::Result<ChatResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<StreamChunk>>>
        {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let chunks: Vec<anyhow::Result<StreamChunk>> = if self.always_tool || n == 1 {
                vec![
                    Ok(StreamChunk::ToolCallStart {
                        id: "t1".into(),
                        name: "lookup".into(),
                    }),
                    Ok(StreamChunk::ToolCallArguments {
                        id: "t1".into(),
                        arguments: "{}".into(),
                    }),
                    Ok(StreamChunk::Done),
                ]
            } else {
                vec![
                    Ok(StreamChunk::Content(self.answer.to_string())),
                    Ok(StreamChunk::Done),
                ]
            };
            Ok(futures::stream::iter(chunks).boxed())
        }
        fn protocol(&self) -> Protocol {
            Protocol::OpenAIChat
        }
        fn model(&self) -> &str {
            "mock"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    /// 返回大块标记文本的"检索"工具（子代理中间输出的替身）
    struct LookupTool;

    #[async_trait::async_trait]
    impl AgentTool for LookupTool {
        fn name(&self) -> &str {
            "lookup"
        }
        fn description(&self) -> &str {
            "bulk lookup"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput::ok("BULK-INTERMEDIATE-OUTPUT ".repeat(500)))
        }
    }

    fn task_tool(provider: Arc<ScriptProvider>) -> SubagentTool {
        SubagentTool::new(provider, vec![Arc::new(LookupTool)])
    }

    #[tokio::test]
    async fn test_task_returns_summary_not_intermediate_output() {
        let provider = Arc::new(ScriptProvider::answering(
            "SUMMARY: found 3 call sites (see file:line refs)",
        ));
        let tool = task_tool(provider);

        let out = tool
            .execute(serde_json::json!({"prompt": "find all callers of foo"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // 父上下文只见最终答案
        assert!(
            out.content.contains("SUMMARY: found 3 call sites"),
            "{}",
            out.content
        );
        assert!(
            !out.content.contains("BULK-INTERMEDIATE-OUTPUT"),
            "intermediate output must not leak to the parent context"
        );
    }

    #[tokio::test]
    async fn test_subagent_failure_is_error_result_not_panic() {
        // 每轮都调工具 → 触发子代理 max_turns(2)
        let provider = Arc::new(ScriptProvider::never_answering());
        let tool = SubagentTool::new(provider, vec![Arc::new(LookupTool)]).with_max_turns(2);

        let out = tool
            .execute(serde_json::json!({"prompt": "loop forever"}))
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("[Subagent failed]"), "{}", out.content);
        assert!(out.content.contains("最大迭代"), "{}", out.content);
    }

    #[tokio::test]
    async fn test_recursion_filtered_from_sub_tools() {
        let provider = Arc::new(ScriptProvider::answering("inner"));
        let nested = task_tool(Arc::new(ScriptProvider::answering("nested")));
        let tool = SubagentTool::new(provider, vec![Arc::new(LookupTool), Arc::new(nested)]);
        // 子工具集里没有 task（深度上限 1）
        assert!(!tool.sub_tool_names().contains(&"task"));
        assert!(tool.sub_tool_names().contains(&"lookup"));
    }

    #[tokio::test]
    async fn test_long_answer_truncated_with_note() {
        let long = "x".repeat(20 * 1024);
        let provider = Arc::new(ScriptProvider::answering("a"));
        // 直接构造长答案 provider
        struct LongAnswer;
        #[async_trait::async_trait]
        impl Provider for LongAnswer {
            async fn chat(&self, _: ChatRequest) -> anyhow::Result<ChatResponse> {
                unreachable!()
            }
            async fn chat_stream(
                &self,
                _: ChatRequest,
            ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<StreamChunk>>>
            {
                Ok(futures::stream::iter(vec![
                    Ok(StreamChunk::Content("y".repeat(20 * 1024))),
                    Ok(StreamChunk::Done),
                ])
                .boxed())
            }
            fn protocol(&self) -> Protocol {
                Protocol::OpenAIChat
            }
            fn model(&self) -> &str {
                "mock"
            }
            fn provider_name(&self) -> &str {
                "mock"
            }
        }
        let _ = long;
        let tool = SubagentTool::new(Arc::new(LongAnswer), vec![Arc::new(LookupTool)]);
        let out = tool
            .execute(serde_json::json!({"prompt": "p"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content.contains("truncated at"),
            "{}",
            out.content.chars().rev().take(200).collect::<String>()
        );
        assert!(out.original_bytes.is_some());
    }

    /// 父侧 provider：第一轮调 task 工具，第二轮给最终答案
    struct ParentProvider;

    #[async_trait::async_trait]
    impl Provider for ParentProvider {
        async fn chat(&self, _: ChatRequest) -> anyhow::Result<ChatResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<StreamChunk>>>
        {
            static CALLS: AtomicU32 = AtomicU32::new(0);
            let n = CALLS.fetch_add(1, Ordering::SeqCst) + 1;
            let chunks: Vec<anyhow::Result<StreamChunk>> = if n == 1 {
                vec![
                    Ok(StreamChunk::ToolCallStart {
                        id: "p1".into(),
                        name: "task".into(),
                    }),
                    Ok(StreamChunk::ToolCallArguments {
                        id: "p1".into(),
                        arguments: r#"{"prompt":"find all callers of foo"}"#.into(),
                    }),
                    Ok(StreamChunk::Done),
                ]
            } else {
                vec![
                    Ok(StreamChunk::Content(
                        "done based on subagent findings".into(),
                    )),
                    Ok(StreamChunk::Done),
                ]
            };
            Ok(futures::stream::iter(chunks).boxed())
        }
        fn protocol(&self) -> Protocol {
            Protocol::OpenAIChat
        }
        fn model(&self) -> &str {
            "mock"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[tokio::test]
    async fn test_parent_context_isolation_end_to_end() {
        let sub = Arc::new(ScriptProvider::answering(
            "SUMMARY: 3 call sites (a.rs:12, b.rs:7, c.rs:99)",
        ));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(SubagentTool::new(sub, vec![Arc::new(LookupTool)])));

        let runtime = AgentRuntime::new(Arc::new(ParentProvider)).with_tools(registry);
        let mut history = Vec::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let answer = runtime
            .run(
                "sys",
                &mut history,
                &tx,
                &CancellationToken::new(),
                &SteeringQueue::new(),
            )
            .await
            .unwrap();
        assert!(answer.contains("done based on subagent findings"));

        // 父历史:task 工具结果携带子代理摘要;子代理中间输出与其 lookup
        // 调用一律不出现
        let tool_results: String = history
            .iter()
            .filter_map(|m| m.tool_results.as_ref())
            .flat_map(|rs| rs.iter().map(|r| r.content.clone()))
            .collect();
        assert!(
            tool_results.contains("SUMMARY: 3 call sites"),
            "{tool_results}"
        );
        assert!(
            !tool_results.contains("BULK-INTERMEDIATE-OUTPUT"),
            "subagent intermediate output leaked into the parent history"
        );
        let parent_calls: Vec<String> = history
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .flat_map(|cs| cs.iter().map(|c| c.name.clone()))
            .collect();
        assert_eq!(
            parent_calls,
            vec!["task"],
            "only the task call in parent history"
        );
    }

    #[tokio::test]
    async fn test_prompt_required() {
        let provider = Arc::new(ScriptProvider::answering("a"));
        let tool = SubagentTool::new(provider, vec![Arc::new(LookupTool)]);
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("requires a non-empty 'prompt'"));
    }

    // ===== 可定义角色 =====

    #[test]
    fn test_parse_agent_role_frontmatter() {
        let role = parse_agent_role(
            "---\nname: reviewer\ndescription: finds issues\nmodel: glm-4.7\nthinking: high\nmax_turns: 20\ntools: [read, grep]\n---\nYou are a code reviewer.",
            "fallback",
        )
        .unwrap();
        assert_eq!(role.name, "reviewer");
        assert_eq!(role.description, "finds issues");
        assert_eq!(role.model.as_deref(), Some("glm-4.7"));
        assert_eq!(role.thinking, Some(ThinkingLevel::High));
        assert_eq!(role.max_turns, Some(20));
        assert_eq!(
            role.tools,
            Some(vec!["read".to_string(), "grep".to_string()])
        );
        assert_eq!(role.system_prompt, "You are a code reviewer.");

        // 无 frontmatter：文件名兜底，正文即提示
        let role = parse_agent_role("Just a body.", "generic-role").unwrap();
        assert_eq!(role.name, "generic-role");
        assert_eq!(role.system_prompt, "Just a body.");
        assert!(role.tools.is_none());

        // tools 逗号形式
        let role = parse_agent_role("---\nname: x\ntools: read, grep,ls\n---\nbody", "f").unwrap();
        assert_eq!(
            role.tools,
            Some(vec!["read".into(), "grep".into(), "ls".into()])
        );
    }

    #[test]
    fn test_load_agent_roles_project_overrides_user() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            user.path().join("reviewer.md"),
            "---\nname: reviewer\ndescription: old\n---\nold body",
        )
        .unwrap();
        std::fs::write(
            user.path().join("explorer.md"),
            "---\nname: explorer\ndescription: walks the tree\n---\nexplore body",
        )
        .unwrap();
        std::fs::write(
            project.path().join("reviewer.md"),
            "---\nname: reviewer\ndescription: new\n---\nnew body",
        )
        .unwrap();
        std::fs::write(user.path().join("not-markdown.txt"), "ignore me").unwrap();

        let roles = load_agent_roles(&[user.path().to_path_buf(), project.path().to_path_buf()]);
        assert_eq!(roles.len(), 2);
        // 项目同名覆盖用户级
        let reviewer = roles.iter().find(|r| r.name == "reviewer").unwrap();
        assert_eq!(reviewer.description, "new");
        assert_eq!(reviewer.system_prompt, "new body");
        // 字典序
        assert_eq!(roles[0].name, "explorer");
    }

    #[test]
    fn test_registry_load_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SubagentRegistry::new(vec![dir.path().to_path_buf()]);
        assert_eq!(registry.load(), 0);
        assert_eq!(registry.count(), 0);

        std::fs::write(
            dir.path().join("r1.md"),
            "---\nname: r1\ndescription: one\n---\nbody one",
        )
        .unwrap();
        assert_eq!(registry.load(), 1);
        assert!(registry.find("r1").is_some());
        assert!(registry.find("missing").is_none());

        // 热重载可见新增（TUI /subagents r 路径）
        std::fs::write(
            dir.path().join("r2.md"),
            "---\nname: r2\ndescription: two\n---\nbody two",
        )
        .unwrap();
        assert_eq!(registry.load(), 2);
        assert_eq!(registry.roles().len(), 2);
    }

    /// 捕获子代理请求（系统提示 / 工具定义 / 思考级别）的 Provider
    struct CapturingSubProvider {
        systems: std::sync::Mutex<Vec<String>>,
        tool_names: std::sync::Mutex<Vec<Vec<String>>>,
        thinking: std::sync::Mutex<Vec<Option<ThinkingLevel>>>,
    }

    #[async_trait::async_trait]
    impl Provider for CapturingSubProvider {
        async fn chat(&self, _: ChatRequest) -> anyhow::Result<ChatResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<StreamChunk>>>
        {
            let system = request
                .messages
                .first()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            self.systems.lock().unwrap().push(system);
            self.tool_names.lock().unwrap().push(
                request
                    .tools
                    .iter()
                    .flatten()
                    .map(|t| t.name.clone())
                    .collect(),
            );
            self.thinking.lock().unwrap().push(request.thinking);
            Ok(futures::stream::iter(vec![
                Ok(StreamChunk::Content("role answer".into())),
                Ok(StreamChunk::Done),
            ])
            .boxed())
        }
        fn protocol(&self) -> Protocol {
            Protocol::OpenAIChat
        }
        fn model(&self) -> &str {
            "mock"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[tokio::test]
    async fn test_role_dispatch_prompt_tools_thinking() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("reviewer.md"),
            "---\nname: reviewer\ndescription: finds issues\ntools: [lookup]\nthinking: high\n---\nYou are a senior code reviewer.",
        )
        .unwrap();
        let registry = Arc::new(SubagentRegistry::new(vec![dir.path().to_path_buf()]));
        registry.load();

        let provider = Arc::new(CapturingSubProvider {
            systems: std::sync::Mutex::new(Vec::new()),
            tool_names: std::sync::Mutex::new(Vec::new()),
            thinking: std::sync::Mutex::new(Vec::new()),
        });
        struct OtherTool;
        #[async_trait::async_trait]
        impl AgentTool for OtherTool {
            fn name(&self) -> &str {
                "other"
            }
            fn description(&self) -> &str {
                "other"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
                Ok(ToolOutput::ok("other"))
            }
        }
        let tool = SubagentTool::new(
            provider.clone(),
            vec![Arc::new(LookupTool), Arc::new(OtherTool)],
        )
        .with_roles(registry);

        let out = tool
            .execute(serde_json::json!({
                "prompt": "review this diff",
                "agent": "reviewer"
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("role answer"));

        // 角色的系统提示/工具集/思考级别全部生效
        assert!(
            provider.systems.lock().unwrap()[0].contains("You are a senior code reviewer."),
            "{}",
            provider.systems.lock().unwrap()[0]
        );
        assert_eq!(provider.tool_names.lock().unwrap()[0], vec!["lookup"]);
        assert_eq!(
            provider.thinking.lock().unwrap()[0],
            Some(ThinkingLevel::High)
        );
    }

    #[tokio::test]
    async fn test_unknown_agent_lists_available() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.md"),
            "---\nname: alpha\ndescription: d\n---\nbody",
        )
        .unwrap();
        let registry = Arc::new(SubagentRegistry::new(vec![dir.path().to_path_buf()]));
        registry.load();
        let tool = SubagentTool::new(
            Arc::new(ScriptProvider::answering("x")),
            vec![Arc::new(LookupTool)],
        )
        .with_roles(registry);

        let out = tool
            .execute(serde_json::json!({"prompt": "p", "agent": "nope"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("unknown agent 'nope'"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("alpha"),
            "lists available: {}",
            out.content
        );
    }
}
