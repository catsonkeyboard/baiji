//! AgentTool trait 与工具注册表

use anyhow::Result;
use async_trait::async_trait;
use baiji_ai::ToolDefinition;
use serde_json::Value;
use std::sync::Arc;

/// 工具执行结果
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// 返回给 LLM 的文本内容
    pub content: String,
    /// 是否为错误结果（用于事件与遥测标注）
    pub is_error: bool,
    /// 压缩前的原始字节数（None = 未压缩）。
    /// 工具在截断/压缩输出时填写，用于上下文节省台账。
    pub original_bytes: Option<u64>,
    /// 压缩前的原始 token 估算（启发式：ASCII ~4 字符/token，CJK ~2 字符/token）。
    /// 与 `original_bytes` 同源同条件填写，供台账以 token 口径统计。
    pub original_tokens: Option<u64>,
}

/// 文本 token 估算（启发式，与 harness compaction 同一口径）：
/// ASCII ~0.25/字符，非 ASCII（CJK 等）~0.5/字符，+1 常数。
pub fn estimate_text_tokens(s: &str) -> usize {
    let ascii = s.chars().filter(|c| c.is_ascii()).count();
    let non_ascii = s.chars().filter(|c| !c.is_ascii()).count();
    ascii / 4 + non_ascii / 2 + 1
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            original_bytes: None,
            original_tokens: None,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            original_bytes: None,
            original_tokens: None,
        }
    }

    /// 标注压缩信息（原始大小）
    pub fn with_original_bytes(mut self, bytes: u64) -> Self {
        self.original_bytes = Some(bytes);
        self
    }

    /// 标注压缩信息（原始 token 估算）
    pub fn with_original_tokens(mut self, tokens: u64) -> Self {
        self.original_tokens = Some(tokens);
        self
    }

    /// 本条结果节省的字节数（未压缩或原始更小时为 0）
    pub fn bytes_saved(&self) -> u64 {
        self.original_bytes
            .unwrap_or(self.content.len() as u64)
            .saturating_sub(self.content.len() as u64)
    }

    /// 本条结果节省的 token 估算（未压缩时为 0）
    pub fn tokens_saved(&self) -> u64 {
        self.original_tokens
            .unwrap_or(estimate_text_tokens(&self.content) as u64)
            .saturating_sub(estimate_text_tokens(&self.content) as u64)
    }
}

/// Agent 工具契约
///
/// 实现 `name` / `description` / `parameters`（JSON Schema）后，
/// 工具会自动暴露给 LLM；`execute` 的返回值作为工具结果回传。
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema（object 类型）
    fn parameters(&self) -> Value;

    async fn execute(&self, args: Value) -> Result<ToolOutput>;

    /// 转为 LLM 工具定义
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
        }
    }
}

/// 工具注册表
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn AgentTool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册工具（同名后注册覆盖先注册：替换旧条目）
    pub fn register(&mut self, tool: Arc<dyn AgentTool>) {
        if let Some(existing) = self.tools.iter_mut().find(|t| t.name() == tool.name()) {
            *existing = tool;
        } else {
            self.tools.push(tool);
        }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn AgentTool>> {
        self.tools.iter().find(|t| t.name() == name)
    }

    /// 导出为 LLM 工具定义列表（按注册顺序）
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait]
    impl AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo the input"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: Value) -> Result<ToolOutput> {
            Ok(ToolOutput::ok("echo!"))
        }
    }

    #[tokio::test]
    async fn test_registry_register_lookup_and_override() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));

        assert_eq!(registry.len(), 1);
        assert!(registry.get("echo").is_some());
        assert!(registry.get("missing").is_none());
        assert_eq!(registry.names(), vec!["echo"]);

        let defs = registry.definitions();
        assert_eq!(defs[0].name, "echo");
        assert_eq!(defs[0].description, "Echo the input");

        // 同名注册覆盖而非追加
        registry.register(Arc::new(EchoTool));
        assert_eq!(registry.len(), 1);

        let out = registry
            .get("echo")
            .unwrap()
            .execute(Value::Null)
            .await
            .unwrap();
        assert_eq!(out.content, "echo!");
        assert!(!out.is_error);
        assert_eq!(out.bytes_saved(), 0);
    }

    #[test]
    fn test_estimate_text_tokens_mixed() {
        // ASCII ~4 字符/token，非 ASCII ~2 字符/token，+1 常数
        assert_eq!(estimate_text_tokens("abcd"), 2);
        assert_eq!(estimate_text_tokens("你好"), 2);
        assert_eq!(estimate_text_tokens("abcd你好"), 3);
        assert_eq!(estimate_text_tokens(""), 1);
    }

    #[test]
    fn test_tokens_saved_accounting() {
        // ASCII 400 字符 ≈ 101 token；压缩后 40 字符 ≈ 11 token
        let out = ToolOutput::ok("x".repeat(40)).with_original_tokens(101);
        assert_eq!(out.original_tokens, Some(101));
        assert_eq!(out.tokens_saved(), 90);

        // 未标注 = 未压缩 → 0
        let out = ToolOutput::ok("hello");
        assert_eq!(out.original_tokens, None);
        assert_eq!(out.tokens_saved(), 0);

        // 原始更小（异常数据）不产生负数
        let out = ToolOutput::ok("x".repeat(400)).with_original_tokens(5);
        assert_eq!(out.tokens_saved(), 0);
    }

    #[test]
    fn test_bytes_saved_accounting() {
        let out = ToolOutput::ok("0123456789").with_original_bytes(100);
        assert_eq!(out.original_bytes, Some(100));
        assert_eq!(out.bytes_saved(), 90);

        // 原始更小（异常数据）不产生负数
        let out = ToolOutput::ok("0123456789").with_original_bytes(5);
        assert_eq!(out.bytes_saved(), 0);
    }
}
