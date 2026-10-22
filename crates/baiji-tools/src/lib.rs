//! baiji-tools — coding 工具集 + 执行环境
//!
//! 内置 read / write / edit / bash / grep / find / ls / expand 八个工具，
//! 全部实现 [`baiji_agent::AgentTool`]，受 [`ExecutionEnv`] 的
//! 路径白名单与输出限制约束。输出超限截断时完整内容 spill 到
//! 内容寻址存储（CCR），LLM 可用 `expand` 凭句柄取回。
//! web 工具（webfetch / websearch）经 [`web_tools`] 按配置单独注册。

pub mod compressors;
pub mod env;
pub mod index;
pub mod signatures;
pub mod tools;
pub mod walk;

pub use env::{ExecutionEnv, spill_to_store};
pub use tools::external_agent::{ExternalAgentSpec, register_external_agents};
pub use tools::web::WebConfig;

use baiji_agent::AgentTool;
use std::sync::Arc;

/// web 工具（webfetch / websearch）。`cfg.enabled == false` 时返回空列表。
/// 两个工具共享同一个 HTTP 客户端（连接池复用；带元数据地址防护）。
/// 与 [`builtin_tools`] 分开注册：web 配置段全局专用，由装配方按配置决定是否启用。
pub fn web_tools(env: Arc<ExecutionEnv>, cfg: &WebConfig) -> Vec<Arc<dyn AgentTool>> {
    if !cfg.enabled {
        return Vec::new();
    }
    let client = match tools::web::build_client() {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!("web tools disabled: HTTP client init failed: {e}");
            return Vec::new();
        }
    };
    vec![
        Arc::new(tools::WebFetchTool::new(env.clone(), client.clone())),
        Arc::new(tools::WebSearchTool::new(env, client, cfg)),
    ]
}

/// 注册全部内置工具（共享同一个执行环境）
pub fn builtin_tools(env: ExecutionEnv) -> Vec<Arc<dyn AgentTool>> {
    builtin_tools_with_jobs(env).0
}

/// 同 [`builtin_tools`]，并返回共享的后台任务注册表
/// （TUI 的 /tasks、/kill 用它查看与停止后台任务）
pub fn builtin_tools_with_jobs(
    env: ExecutionEnv,
) -> (Vec<Arc<dyn AgentTool>>, Arc<tools::jobs::JobRegistry>) {
    let env = Arc::new(env);
    // 后台任务注册表：bash(生产) 与 jobs(消费) 共享；
    // Drop 时击杀全部仍在运行的任务（进程退出不留孤儿）
    let job_registry = Arc::new(tools::jobs::JobRegistry::new());
    let tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(tools::ReadTool::new(env.clone())),
        Arc::new(tools::WriteTool::new(env.clone())),
        Arc::new(tools::EditTool::new(env.clone())),
        Arc::new(tools::BashTool::with_jobs(
            env.clone(),
            job_registry.clone(),
        )),
        Arc::new(tools::JobsTool::new(env.clone(), job_registry.clone())),
        Arc::new(tools::GrepTool::new(env.clone())),
        Arc::new(tools::FindTool::new(env.clone())),
        Arc::new(tools::LsTool::new(env.clone())),
        Arc::new(tools::SearchTool::new(env.clone())),
        Arc::new(tools::ImportsTool::new(env.clone())),
        Arc::new(tools::ExpandTool::new(env)),
    ];
    (tools, job_registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_tools_respect_enabled_flag() {
        let env = Arc::new(ExecutionEnv::new("."));
        let tools = web_tools(env.clone(), &WebConfig::default());
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["webfetch", "websearch"]);

        let disabled = WebConfig {
            enabled: false,
            ..WebConfig::default()
        };
        assert!(web_tools(env, &disabled).is_empty());
    }
}
