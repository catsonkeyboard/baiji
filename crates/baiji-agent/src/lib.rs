//! baiji-agent — Agent runtime
//!
//! - [`AgentTool`] trait 与工具注册表
//! - [`AgentEvent`] 运行时事件（流式文本、工具执行、生命周期）
//! - [`Hook`] 拦截点（run/turn/tool 前后）
//! - [`SteeringQueue`] 运行中用户引导消息队列
//! - [`AgentRuntime`] 流式工具调用循环（ReAct）

pub mod confirmation;
pub mod event;
pub mod hooks;
pub mod queue;
pub mod runtime;
pub mod subagent;
pub mod tool;

pub use confirmation::{
    Approver, AutoApprover, ConfirmationDecision, ConfirmationGate, ConfirmationRequest,
    DenyAllApprover,
};
pub use event::AgentEvent;
pub use hooks::{Hook, HookDecision, HookRegistry};
pub use queue::SteeringQueue;
pub use runtime::{AgentRuntime, PLAN_MODE_ALLOWED_TOOLS};
pub use subagent::{SubagentTool, SUBAGENT_ALLOWED_TOOLS};
pub use tool::{AgentTool, ToolOutput, ToolRegistry, estimate_text_tokens};

pub use baiji_ai::{
    ChatRequest, ChatResponse, Message, Role, StreamChunk, ToolCall, ToolDefinition, ToolResult,
};
