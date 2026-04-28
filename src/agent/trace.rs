//! 结构化调用链追踪 — Harness Engineering 可观测性层
//!
//! 记录每次 Agent 运行的完整调用链：LLM 延迟、工具调用、token 用量等。
//! 支持导出 JSON trace 文件用于事后分析和调试。

use chrono::{DateTime, Local};
use serde::Serialize;
use std::time::Instant;
use tracing::{debug, info, warn};

/// 一次完整 Agent 运行的追踪记录
#[derive(Debug, Clone, Serialize)]
pub struct AgentTrace {
    /// 运行唯一 ID
    pub run_id: String,
    /// 开始时间
    pub started_at: DateTime<Local>,
    /// 各轮次追踪
    pub turns: Vec<TurnTrace>,
    /// 总输入 token 数
    pub total_input_tokens: u32,
    /// 总输出 token 数
    pub total_output_tokens: u32,
    /// 总耗时（毫秒）
    pub total_duration_ms: u64,
}

/// 单轮次追踪
#[derive(Debug, Clone, Serialize)]
pub struct TurnTrace {
    /// 轮次序号
    pub turn_number: usize,
    /// LLM 调用延迟（毫秒）
    pub llm_latency_ms: u64,
    /// 工具调用追踪
    pub tool_calls: Vec<ToolCallTrace>,
    /// 上下文消息数
    pub context_messages: usize,
    /// 估算的上下文 token 数
    pub estimated_tokens: usize,
}

/// 工具调用追踪
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallTrace {
    /// 工具名称
    pub tool_name: String,
    /// 执行延迟（毫秒）
    pub latency_ms: u64,
    /// 输入大小（字节）
    pub input_size: usize,
    /// 输出大小（字节）
    pub output_size: usize,
    /// 是否成功
    pub success: bool,
    /// 是否重试过
    pub retried: bool,
}

/// 追踪记录器（Agent 运行期间使用）
pub struct TraceRecorder {
    trace: AgentTrace,
    current_turn: Option<TurnTrace>,
    run_start: Instant,
    turn_start: Option<Instant>,
}

impl TraceRecorder {
    /// 创建新的追踪记录器
    pub fn new() -> Self {
        let run_id = format!("{}", Local::now().format("%Y%m%d_%H%M%S_%3f"));

        Self {
            trace: AgentTrace {
                run_id,
                started_at: Local::now(),
                turns: Vec::new(),
                total_input_tokens: 0,
                total_output_tokens: 0,
                total_duration_ms: 0,
            },
            current_turn: None,
            run_start: Instant::now(),
            turn_start: None,
        }
    }

    /// 开始新轮次
    pub fn start_turn(&mut self, turn_number: usize, context_messages: usize, estimated_tokens: usize) {
        self.turn_start = Some(Instant::now());
        self.current_turn = Some(TurnTrace {
            turn_number,
            llm_latency_ms: 0,
            tool_calls: Vec::new(),
            context_messages,
            estimated_tokens,
        });
    }

    /// 记录 LLM 调用完成
    pub fn record_llm_complete(&mut self) {
        if let (Some(turn), Some(start)) = (&mut self.current_turn, self.turn_start) {
            turn.llm_latency_ms = start.elapsed().as_millis() as u64;
        }
    }

    /// 记录工具调用
    pub fn record_tool_call(&mut self, trace: ToolCallTrace) {
        if let Some(ref mut turn) = self.current_turn {
            turn.tool_calls.push(trace);
        }
    }

    /// 结束当前轮次
    pub fn end_turn(&mut self) {
        if let Some(turn) = self.current_turn.take() {
            debug!(
                "Turn {} complete: LLM {}ms, {} tool calls",
                turn.turn_number,
                turn.llm_latency_ms,
                turn.tool_calls.len()
            );
            self.trace.turns.push(turn);
        }
        self.turn_start = None;
    }

    /// 记录 token 用量
    pub fn record_usage(&mut self, input_tokens: u32, output_tokens: u32) {
        self.trace.total_input_tokens += input_tokens;
        self.trace.total_output_tokens += output_tokens;
    }

    /// 完成追踪，返回最终记录
    pub fn finish(mut self) -> AgentTrace {
        // 如果还有未结束的轮次，先结束
        self.end_turn();
        self.trace.total_duration_ms = self.run_start.elapsed().as_millis() as u64;
        self.trace
    }

    /// 获取当前累计统计（用于 UI 实时显示）
    pub fn current_stats(&self) -> TraceStats {
        let tool_calls: usize = self.trace.turns.iter().map(|t| t.tool_calls.len()).sum();
        let current_tool_calls = self
            .current_turn
            .as_ref()
            .map(|t| t.tool_calls.len())
            .unwrap_or(0);

        TraceStats {
            turn_count: self.trace.turns.len()
                + if self.current_turn.is_some() { 1 } else { 0 },
            total_tool_calls: tool_calls + current_tool_calls,
            total_input_tokens: self.trace.total_input_tokens,
            total_output_tokens: self.trace.total_output_tokens,
            elapsed_ms: self.run_start.elapsed().as_millis() as u64,
        }
    }
}

/// 实时统计摘要
#[derive(Debug, Clone)]
pub struct TraceStats {
    pub turn_count: usize,
    pub total_tool_calls: usize,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub elapsed_ms: u64,
}

/// 保存追踪记录到文件
pub async fn save_trace(trace: &AgentTrace) {
    let dir = std::path::Path::new("logs/traces");
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        warn!("Failed to create traces directory: {}", e);
        return;
    }

    let filename = format!("trace_{}.json", trace.run_id);
    let path = dir.join(filename);

    match serde_json::to_string_pretty(trace) {
        Ok(json) => {
            if let Err(e) = tokio::fs::write(&path, json).await {
                warn!("Failed to save trace: {}", e);
            } else {
                info!("Trace saved to {}", path.display());
            }
        }
        Err(e) => warn!("Failed to serialize trace: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trace_recorder_basic() {
        let mut recorder = TraceRecorder::new();

        recorder.start_turn(1, 5, 1000);
        recorder.record_llm_complete();
        recorder.record_tool_call(ToolCallTrace {
            tool_name: "builtin__grep".to_string(),
            latency_ms: 50,
            input_size: 100,
            output_size: 500,
            success: true,
            retried: false,
        });
        recorder.end_turn();

        recorder.start_turn(2, 8, 2000);
        recorder.record_llm_complete();
        recorder.end_turn();

        let trace = recorder.finish();

        assert_eq!(trace.turns.len(), 2);
        assert_eq!(trace.turns[0].tool_calls.len(), 1);
        assert_eq!(trace.turns[0].tool_calls[0].tool_name, "builtin__grep");
        assert_eq!(trace.turns[1].tool_calls.len(), 0);
        // total_duration_ms is a u64; just verify finish() doesn't panic
        // (test runs too fast for ms precision to be > 0)
        let _ = trace.total_duration_ms;
    }

    #[test]
    fn test_current_stats() {
        let mut recorder = TraceRecorder::new();

        let stats = recorder.current_stats();
        assert_eq!(stats.turn_count, 0);
        assert_eq!(stats.total_tool_calls, 0);

        recorder.start_turn(1, 5, 1000);
        recorder.record_tool_call(ToolCallTrace {
            tool_name: "test".to_string(),
            latency_ms: 10,
            input_size: 10,
            output_size: 20,
            success: true,
            retried: false,
        });

        let stats = recorder.current_stats();
        assert_eq!(stats.turn_count, 1);
        assert_eq!(stats.total_tool_calls, 1);
    }

    #[test]
    fn test_record_usage() {
        let mut recorder = TraceRecorder::new();
        recorder.record_usage(100, 50);
        recorder.record_usage(200, 75);

        let trace = recorder.finish();
        assert_eq!(trace.total_input_tokens, 300);
        assert_eq!(trace.total_output_tokens, 125);
    }

    #[test]
    fn test_run_id_format() {
        let recorder = TraceRecorder::new();
        assert!(!recorder.trace.run_id.is_empty());
        // Should be in format: YYYYMMDD_HHMMSS_mmm
        assert!(recorder.trace.run_id.len() >= 15);
    }
}
