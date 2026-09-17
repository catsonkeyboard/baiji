//! Hook 拦截点
//!
//! 在 run / turn / 工具调用前后插入自定义逻辑：
//! 审计、策略拦截（`HookDecision::Deny`）、参数改写（`HookDecision::Modify`）、
//! 结果改写（`on_tool_output`，如脱敏）、指标采集等。
//! 默认所有方法为空实现，实现方按需覆盖。

use crate::tool::ToolOutput;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// 工具调用前的裁决
#[derive(Debug, Clone, PartialEq)]
pub enum HookDecision {
    /// 放行
    Proceed,
    /// 放行，但用改写后的参数执行（后续 hook、人工确认与工具看到的都是新参数）
    Modify(Value),
    /// 拒绝执行，理由会作为工具结果回传给 LLM
    Deny(String),
}

/// Hook 契约
#[async_trait]
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;

    async fn on_run_start(&self, _user_input: &str) -> Result<()> {
        Ok(())
    }

    /// 每轮 LLM 调用前（turn 从 1 开始）
    async fn on_turn_start(&self, _turn: u32) -> Result<()> {
        Ok(())
    }

    /// 工具执行前 — 返回 Deny 可拦截
    async fn on_tool_call(&self, _name: &str, _args: &Value) -> Result<HookDecision> {
        Ok(HookDecision::Proceed)
    }

    /// 工具执行后（只读观察）
    async fn on_tool_result(&self, _name: &str, _output: &ToolOutput) -> Result<()> {
        Ok(())
    }

    /// 工具执行后（可改写结果：脱敏、追加提示、改判 is_error 等）。
    /// 默认实现调用只读的 `on_tool_result` 并原样返回，二者实现其一即可。
    async fn on_tool_output(&self, name: &str, output: ToolOutput) -> Result<ToolOutput> {
        self.on_tool_result(name, &output).await?;
        Ok(output)
    }

    async fn on_run_end(&self, _answer: &str) -> Result<()> {
        Ok(())
    }
}

/// Hook 注册表：按注册顺序依次调用
#[derive(Default)]
pub struct HookRegistry {
    hooks: Vec<std::sync::Arc<dyn Hook>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, hook: std::sync::Arc<dyn Hook>) {
        self.hooks.push(hook);
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub async fn run_start(&self, user_input: &str) -> Result<()> {
        for hook in &self.hooks {
            hook.on_run_start(user_input).await?;
        }
        Ok(())
    }

    pub async fn turn_start(&self, turn: u32) -> Result<()> {
        for hook in &self.hooks {
            hook.on_turn_start(turn).await?;
        }
        Ok(())
    }

    /// 依次征询所有 hook：任一 Deny 即短路；Modify 的新参数传给后续 hook。
    ///
    /// 发生过改写时，再用**最终参数**对全部 hook 复核一遍（只认 Deny）：
    /// 否则注册在改写 hook 之前的安全 hook 只检查过旧参数，可被改写绕过。
    ///
    /// 返回 `Proceed`（参数未变）/ `Modify(最终参数)` / `Deny`。
    pub async fn tool_call(&self, name: &str, args: &Value) -> Result<HookDecision> {
        let mut current: Option<Value> = None;
        for hook in &self.hooks {
            match hook.on_tool_call(name, current.as_ref().unwrap_or(args)).await? {
                HookDecision::Proceed => {}
                HookDecision::Modify(new_args) => current = Some(new_args),
                deny @ HookDecision::Deny(_) => return Ok(deny),
            }
        }
        let Some(final_args) = current else {
            return Ok(HookDecision::Proceed);
        };
        for hook in &self.hooks {
            if let deny @ HookDecision::Deny(_) = hook.on_tool_call(name, &final_args).await? {
                return Ok(deny);
            }
        }
        Ok(HookDecision::Modify(final_args))
    }

    /// 工具结果依次流过所有 hook（每个 hook 都可改写）
    pub async fn tool_result(&self, name: &str, output: ToolOutput) -> Result<ToolOutput> {
        let mut output = output;
        for hook in &self.hooks {
            output = hook.on_tool_output(name, output).await?;
        }
        Ok(output)
    }

    pub async fn run_end(&self, answer: &str) -> Result<()> {
        for hook in &self.hooks {
            hook.on_run_end(answer).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingHook {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Hook for CountingHook {
        fn name(&self) -> &str {
            "counting"
        }
        async fn on_run_start(&self, _: &str) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn on_tool_call(&self, name: &str, _: &Value) -> Result<HookDecision> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if name == "bash" {
                return Ok(HookDecision::Deny("bash disabled in test".to_string()));
            }
            Ok(HookDecision::Proceed)
        }
    }

    #[tokio::test]
    async fn test_hook_registry_dispatch_and_deny() {
        let mut registry = HookRegistry::new();
        let hook = std::sync::Arc::new(CountingHook {
            calls: AtomicUsize::new(0),
        });
        registry.register(hook.clone());

        registry.run_start("hello").await.unwrap();
        assert_eq!(hook.calls.load(Ordering::SeqCst), 1);

        assert_eq!(
            registry
                .tool_call("read", &serde_json::json!({}))
                .await
                .unwrap(),
            HookDecision::Proceed
        );
        assert_eq!(
            registry
                .tool_call("bash", &serde_json::json!({}))
                .await
                .unwrap(),
            HookDecision::Deny("bash disabled in test".to_string())
        );
        assert_eq!(hook.calls.load(Ordering::SeqCst), 3);
    }

    /// 把 bash 命令改写成危险命令的 hook（模拟有缺陷/恶意的改写）
    struct RewriteHook;

    #[async_trait]
    impl Hook for RewriteHook {
        fn name(&self) -> &str {
            "rewrite"
        }
        async fn on_tool_call(&self, _name: &str, args: &Value) -> Result<HookDecision> {
            if args["command"] == "safe" {
                return Ok(HookDecision::Modify(serde_json::json!({"command": "danger"})));
            }
            Ok(HookDecision::Proceed)
        }
        async fn on_tool_output(&self, _name: &str, output: ToolOutput) -> Result<ToolOutput> {
            Ok(ToolOutput::ok(output.content.replace("sk-secret", "[redacted]")))
        }
    }

    struct DenyDangerHook;

    #[async_trait]
    impl Hook for DenyDangerHook {
        fn name(&self) -> &str {
            "deny-danger"
        }
        async fn on_tool_call(&self, _name: &str, args: &Value) -> Result<HookDecision> {
            Ok(if args["command"] == "danger" {
                HookDecision::Deny("danger".into())
            } else {
                HookDecision::Proceed
            })
        }
    }

    #[tokio::test]
    async fn test_modify_is_chained_and_rechecked_by_earlier_hooks() {
        // 安全 hook 注册在改写 hook **之前**：第一遍只看到 "safe"，复核时必须拦下 "danger"
        let mut registry = HookRegistry::new();
        registry.register(std::sync::Arc::new(DenyDangerHook));
        registry.register(std::sync::Arc::new(RewriteHook));
        let decision = registry
            .tool_call("bash", &serde_json::json!({"command": "safe"}))
            .await
            .unwrap();
        assert_eq!(decision, HookDecision::Deny("danger".into()));

        // 无害的改写 → Modify(最终参数)
        let mut registry = HookRegistry::new();
        registry.register(std::sync::Arc::new(RewriteHook));
        let decision = registry
            .tool_call("bash", &serde_json::json!({"command": "safe"}))
            .await
            .unwrap();
        assert_eq!(
            decision,
            HookDecision::Modify(serde_json::json!({"command": "danger"}))
        );
        // 未改写 → Proceed
        let decision = registry
            .tool_call("bash", &serde_json::json!({"command": "other"}))
            .await
            .unwrap();
        assert_eq!(decision, HookDecision::Proceed);
    }

    #[tokio::test]
    async fn test_tool_output_can_be_rewritten() {
        let mut registry = HookRegistry::new();
        registry.register(std::sync::Arc::new(RewriteHook));
        let out = registry
            .tool_result("bash", ToolOutput::ok("key=sk-secret"))
            .await
            .unwrap();
        assert_eq!(out.content, "key=[redacted]");
    }
}
