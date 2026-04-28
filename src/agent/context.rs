//! 分层上下文管理器 — Harness Engineering 上下文工程层
//!
//! 替代简单的消息截断，实现智能上下文压缩：
//! - 保留 system prompt 完整
//! - 保留最近 N 轮完整对话
//! - 中间对话压缩为摘要
//! - 基于 token 估算控制总量

use crate::llm::{Message, Role};
use tracing::{debug, info};

/// 分层上下文管理器
pub struct ContextManager {
    /// 估算的 token 上限（保守值，实际由 API max_tokens 控制）
    max_estimated_tokens: usize,
    /// 保留的最近完整轮次数
    keep_recent_turns: usize,
    /// 已积累的上下文摘要
    summaries: Vec<String>,
}

impl Default for ContextManager {
    fn default() -> Self {
        Self {
            max_estimated_tokens: 16000,
            keep_recent_turns: 6,
            summaries: Vec::new(),
        }
    }
}

impl ContextManager {
    pub fn new(max_estimated_tokens: usize, keep_recent_turns: usize) -> Self {
        Self {
            max_estimated_tokens,
            keep_recent_turns,
            summaries: Vec::new(),
        }
    }

    /// 智能裁剪上下文
    ///
    /// 策略：
    /// 1. 保留 system prompt（第一条 System 消息）
    /// 2. 估算当前 token 总量
    /// 3. 如果超限，将最早的对话轮次压缩为摘要
    /// 4. 摘要作为 System 消息注入到 system prompt 之后
    pub fn trim(&mut self, messages: &mut Vec<Message>) {
        let total_tokens = self.estimate_tokens(messages);

        if total_tokens <= self.max_estimated_tokens {
            return;
        }

        info!(
            "Context trim triggered: ~{} estimated tokens (limit: {}), {} messages",
            total_tokens,
            self.max_estimated_tokens,
            messages.len()
        );

        // 分离 system prompt（第一条）和剩余消息
        if messages.is_empty() {
            return;
        }

        let system_msg = messages.remove(0);

        // 将消息分为对话"轮次"（User + Assistant + 可选 Tool）
        let turns = Self::group_into_turns(messages);

        if turns.len() <= self.keep_recent_turns {
            // 轮次数不够多，无法压缩，直接回填
            messages.insert(0, system_msg);
            return;
        }

        // 压缩早期轮次为摘要
        let split = turns.len() - self.keep_recent_turns;
        let old_turns = &turns[..split];
        let recent_turns = &turns[split..];

        let summary = Self::summarize_turns(old_turns);
        self.summaries.push(summary.clone());

        debug!(
            "Compressed {} old turns into summary ({} chars), keeping {} recent turns",
            old_turns.len(),
            summary.len(),
            recent_turns.len()
        );

        // 重建消息列表
        messages.clear();
        messages.push(system_msg);

        // 注入摘要（所有历史摘要合并为一条 System 消息）
        if !self.summaries.is_empty() {
            let combined_summary = self.summaries.join("\n---\n");
            messages.push(Message {
                role: Role::System,
                content: format!(
                    "[Conversation Summary]\nPrevious conversation context:\n{}",
                    combined_summary
                ),
                tool_calls: None,
                tool_results: None,
            });
        }

        // 回填最近的完整轮次
        for turn in recent_turns {
            messages.extend(turn.iter().cloned());
        }

        info!(
            "Context trimmed: {} messages, ~{} estimated tokens",
            messages.len(),
            self.estimate_tokens(messages)
        );
    }

    /// 估算消息列表的 token 数
    ///
    /// 启发式规则：
    /// - 英文：~0.25 tokens/char (4 chars per token)
    /// - 中文：~0.5 tokens/char (2 chars per token)
    /// - 取中间值约 0.35 tokens/char
    /// - 额外开销：每条消息 +4 tokens (role, formatting)
    pub fn estimate_tokens(&self, messages: &[Message]) -> usize {
        messages
            .iter()
            .map(|m| {
                let content_tokens = estimate_string_tokens(&m.content);
                let tool_tokens = m
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|c| {
                                estimate_string_tokens(&c.name)
                                    + estimate_string_tokens(&c.arguments.to_string())
                            })
                            .sum::<usize>()
                    })
                    .unwrap_or(0);
                let result_tokens = m
                    .tool_results
                    .as_ref()
                    .map(|results| {
                        results
                            .iter()
                            .map(|r| estimate_string_tokens(&r.content))
                            .sum::<usize>()
                    })
                    .unwrap_or(0);

                content_tokens + tool_tokens + result_tokens + 4 // 4 tokens overhead per message
            })
            .sum()
    }

    /// 将消息分组为对话轮次
    ///
    /// 一个"轮次"= User 消息 + 后续的 Assistant/Tool 消息
    fn group_into_turns(messages: &[Message]) -> Vec<Vec<Message>> {
        let mut turns: Vec<Vec<Message>> = Vec::new();
        let mut current_turn: Vec<Message> = Vec::new();

        for msg in messages {
            if msg.role == Role::User && !current_turn.is_empty() {
                turns.push(std::mem::take(&mut current_turn));
            }
            current_turn.push(msg.clone());
        }

        if !current_turn.is_empty() {
            turns.push(current_turn);
        }

        turns
    }

    /// 将多个对话轮次压缩为摘要文本
    ///
    /// 简单启发式：提取每轮的 user 问题 + assistant 首句/工具调用概要
    fn summarize_turns(turns: &[Vec<Message>]) -> String {
        let mut lines: Vec<String> = Vec::new();

        for (i, turn) in turns.iter().enumerate() {
            let user_content = turn
                .iter()
                .find(|m| m.role == Role::User)
                .map(|m| &m.content)
                .unwrap_or(&String::new())
                .clone();

            let assistant_summary = turn
                .iter()
                .find(|m| m.role == Role::Assistant)
                .map(|m| {
                    // 提取首句（或前 100 字符）
                    let first_line = m.content.lines().next().unwrap_or("");
                    let summary = if first_line.len() > 100 {
                        format!("{}...", &first_line[..100])
                    } else {
                        first_line.to_string()
                    };

                    // 如果有工具调用，附加概要
                    if let Some(ref calls) = m.tool_calls {
                        let tool_names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                        format!("{} [used tools: {}]", summary, tool_names.join(", "))
                    } else {
                        summary
                    }
                })
                .unwrap_or_default();

            // 截断 user 内容
            let user_short = if user_content.len() > 80 {
                format!("{}...", &user_content[..80])
            } else {
                user_content
            };

            lines.push(format!(
                "Turn {}: User asked: \"{}\" → {}",
                i + 1,
                user_short,
                if assistant_summary.is_empty() {
                    "(no response)".to_string()
                } else {
                    assistant_summary
                }
            ));
        }

        lines.join("\n")
    }
}

/// 估算字符串的 token 数
fn estimate_string_tokens(s: &str) -> usize {
    // 混合语言启发式：统计 ASCII 和非 ASCII 字符分别估算
    let ascii_chars = s.chars().filter(|c| c.is_ascii()).count();
    let non_ascii_chars = s.chars().filter(|c| !c.is_ascii()).count();

    // ASCII: ~0.25 tokens/char, Non-ASCII (中文等): ~0.5 tokens/char
    let ascii_tokens = ascii_chars / 4;
    let non_ascii_tokens = non_ascii_chars / 2;

    ascii_tokens + non_ascii_tokens + 1 // +1 避免为 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_messages(count: usize) -> Vec<Message> {
        let mut msgs = vec![Message {
            role: Role::System,
            content: "You are a helpful assistant.".to_string(),
            tool_calls: None,
            tool_results: None,
        }];

        for i in 0..count {
            msgs.push(Message::user(format!("Question {}", i + 1)));
            msgs.push(Message::assistant(format!(
                "Answer to question {}. Here is a detailed response.",
                i + 1
            )));
        }

        msgs
    }

    #[test]
    fn test_no_trim_when_under_limit() {
        let mut ctx = ContextManager::new(100000, 6);
        let mut messages = make_messages(3); // 1 system + 6 = 7 messages
        let original_len = messages.len();
        ctx.trim(&mut messages);
        assert_eq!(messages.len(), original_len);
    }

    #[test]
    fn test_trim_compresses_old_turns() {
        let mut ctx = ContextManager::new(100, 2); // Very low token limit
        let mut messages = make_messages(8); // 1 system + 16 = 17 messages

        ctx.trim(&mut messages);

        // Should have: system + summary + recent turns
        // Recent 2 turns = 4 messages
        // Total: 1 (system) + 1 (summary) + 4 (recent) = 6
        assert!(messages.len() < 17, "Messages should be compressed");
        assert!(messages.len() >= 6, "Should keep system + summary + recent turns");

        // First message should still be system
        assert_eq!(messages[0].role, Role::System);

        // Second message should be summary
        assert_eq!(messages[1].role, Role::System);
        assert!(messages[1].content.contains("[Conversation Summary]"));
    }

    #[test]
    fn test_summary_contains_turn_info() {
        let mut ctx = ContextManager::new(100, 1);
        let mut messages = make_messages(5); // Many turns to force compression

        ctx.trim(&mut messages);

        // Check that summaries are accumulated
        assert!(!ctx.summaries.is_empty());

        let summary = &ctx.summaries[0];
        assert!(summary.contains("Turn 1:"));
        assert!(summary.contains("User asked:"));
    }

    #[test]
    fn test_estimate_tokens() {
        let ctx = ContextManager::default();

        let messages = vec![Message::user("Hello, world!")];
        let tokens = ctx.estimate_tokens(&messages);
        assert!(tokens > 0);

        // 中文应该估算更多 tokens
        let chinese = vec![Message::user("你好世界，这是一个测试")];
        let cn_tokens = ctx.estimate_tokens(&chinese);
        assert!(cn_tokens > 0);
    }

    #[test]
    fn test_group_into_turns() {
        let messages = vec![
            Message::user("Q1"),
            Message::assistant("A1"),
            Message::user("Q2"),
            Message::assistant("A2"),
        ];

        let turns = ContextManager::group_into_turns(&messages);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].len(), 2); // User + Assistant
        assert_eq!(turns[1].len(), 2);
    }

    #[test]
    fn test_estimate_string_tokens() {
        assert!(estimate_string_tokens("Hello") > 0);
        assert!(estimate_string_tokens("你好世界") > 0);
        assert!(estimate_string_tokens("") > 0); // +1 minimum
    }
}
