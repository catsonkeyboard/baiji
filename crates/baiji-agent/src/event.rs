//! Agent 运行时事件
//!
//! Runtime 在运行过程中通过无界 mpsc 通道推送这些事件，
//! UI / 持久化 / 扩展层消费它们以获得实时可观测性。

use baiji_ai::Message;
use serde_json::Value;

/// 运行时事件
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 一次 LLM 轮次开始
    TurnStarted { turn: u32 },
    /// 流式文本增量
    TextDelta { text: String },
    /// 模型思考内容增量（思考模型在给出答案/调用工具前的推理；不计入最终答案）
    ReasoningDelta { text: String },
    /// 工具调用开始
    ToolStarted {
        id: String,
        name: String,
        args: Value,
    },
    /// 工具调用结束
    ToolFinished {
        id: String,
        name: String,
        output: String,
        is_error: bool,
        duration_ms: u64,
        /// 压缩前的原始字节数（上下文节省台账）
        original_bytes: Option<u64>,
        /// 压缩前的原始 token 估算（台账 token 口径）
        original_tokens: Option<u64>,
    },
    /// 一条新消息已进入对话历史（steering/assistant/tool）。
    /// 持久化层据此增量落盘，崩溃时不丢已完成的轮次
    MessageCommitted { message: Message },
    /// 本轮 LLM 调用因瞬时错误重试：此前收到的 TextDelta 作废，UI 应清空流式缓冲
    StreamRestarted,
    /// 厂商上报的真实 token 用量（一次 LLM 调用）
    UsageReported {
        input_tokens: u32,
        output_tokens: u32,
    },
    /// 一次 LLM 轮次结束
    TurnFinished { turn: u32 },
    /// 整个运行完成
    RunCompleted { answer: String },
    /// 运行失败
    RunFailed { error: String },
    /// 用户取消（Escape）
    Interrupted,
}

impl AgentEvent {
    /// 事件的简短名称（用于日志/遥测）
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn_started",
            Self::TextDelta { .. } => "text_delta",
            Self::ReasoningDelta { .. } => "reasoning_delta",
            Self::ToolStarted { .. } => "tool_started",
            Self::ToolFinished { .. } => "tool_finished",
            Self::MessageCommitted { .. } => "message_committed",
            Self::StreamRestarted => "stream_restarted",
            Self::UsageReported { .. } => "usage_reported",
            Self::TurnFinished { .. } => "turn_finished",
            Self::RunCompleted { .. } => "run_completed",
            Self::RunFailed { .. } => "run_failed",
            Self::Interrupted => "interrupted",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_kind() {
        assert_eq!(AgentEvent::Interrupted.kind(), "interrupted");
        assert_eq!(
            AgentEvent::TextDelta { text: "x".into() }.kind(),
            "text_delta"
        );
        assert_eq!(
            AgentEvent::RunCompleted {
                answer: String::new()
            }
            .kind(),
            "run_completed"
        );
    }
}
