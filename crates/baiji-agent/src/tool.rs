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
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            original_bytes: None,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            original_bytes: None,
        }
    }

    /// 标注压缩信息（原始大小）
    pub fn with_original_bytes(mut self, bytes: u64) -> Self {
        self.original_bytes = Some(bytes);
        self
    }

    /// 本条结果节省的字节数（未压缩或原始更小时为 0）
    pub fn bytes_saved(&self) -> u64 {
        self.original_bytes
            .unwrap_or(self.content.len() as u64)
            .saturating_sub(self.content.len() as u64)
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
    fn test_bytes_saved_accounting() {
        let out = ToolOutput::ok("0123456789").with_original_bytes(100);
        assert_eq!(out.original_bytes, Some(100));
        assert_eq!(out.bytes_saved(), 90);

        // 原始更小（异常数据）不产生负数
        let out = ToolOutput::ok("0123456789").with_original_bytes(5);
        assert_eq!(out.bytes_saved(), 0);
    }
}
