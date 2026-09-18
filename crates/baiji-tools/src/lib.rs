//! baiji-tools — coding 工具集 + 执行环境
//!
//! 内置 read / write / edit / bash / grep / find / ls / expand 八个工具，
//! 全部实现 [`baiji_agent::AgentTool`]，受 [`ExecutionEnv`] 的
//! 路径白名单与输出限制约束。输出超限截断时完整内容 spill 到
//! 内容寻址存储（CCR），LLM 可用 `expand` 凭句柄取回。

pub mod compressors;
pub mod env;
pub mod index;
pub mod signatures;
pub mod tools;
pub mod walk;

pub use env::ExecutionEnv;

use baiji_agent::AgentTool;
use std::sync::Arc;

/// 注册全部内置工具（共享同一个执行环境）
pub fn builtin_tools(env: ExecutionEnv) -> Vec<Arc<dyn AgentTool>> {
    let env = Arc::new(env);
    vec![
        Arc::new(tools::ReadTool::new(env.clone())),
        Arc::new(tools::WriteTool::new(env.clone())),
        Arc::new(tools::EditTool::new(env.clone())),
        Arc::new(tools::BashTool::new(env.clone())),
        Arc::new(tools::GrepTool::new(env.clone())),
        Arc::new(tools::FindTool::new(env.clone())),
        Arc::new(tools::LsTool::new(env.clone())),
        Arc::new(tools::SearchTool::new(env.clone())),
        Arc::new(tools::ImportsTool::new(env.clone())),
        Arc::new(tools::ExpandTool::new(env)),
    ]
}
