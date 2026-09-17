//! 分层上下文压缩
//!
//! 替代简单截断：保留最近 N 轮完整对话，更早的轮次折叠为
//! 确定性摘要（提取每轮 user 问题 + assistant 首句/工具概要），
//! 以 `[Conversation Summary]` System 消息注入。
//! token 估算采用混合语言启发式（ASCII ~0.25/字符，CJK ~0.5/字符）。

use baiji_ai::{ChatRequest, Message, Role};

/// 压缩策略
#[derive(Debug, Clone)]
pub struct CompactionPolicy {
    /// 估算 token 上限（超出触发压缩）
    pub max_estimated_tokens: usize,
    /// 保留的最近完整轮次数
    pub keep_recent_turns: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            max_estimated_tokens: 48_000,
            keep_recent_turns: 6,
        }
    }
}

impl CompactionPolicy {
    /// 按模型上下文窗口推导压缩阈值：窗口的 70%（token 估算本身有误差，且系统提示与
    /// 工具定义不在估算内）再扣除为输出预留的 `max_output` token。下限 16k。
    pub fn for_context(context_length: u64, max_output: u32) -> Self {
        let budget = (context_length as f64 * 0.7) as u64;
        let budget = budget.saturating_sub(max_output as u64).max(16_000);
        Self {
            max_estimated_tokens: budget as usize,
            ..Self::default()
        }
    }
}

/// 估算消息列表的 token 数
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            let content = estimate_string_tokens(&m.content);
            let tools = m
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
            let results = m
                .tool_results
                .as_ref()
                .map(|rs| rs.iter().map(|r| estimate_string_tokens(&r.content)).sum::<usize>())
                .unwrap_or(0);
            content + tools + results + 4
        })
        .sum()
}

/// 压缩消息列表。超限时把旧轮次折叠为摘要并替换原列表，
/// 返回 Some(摘要文本)；未触发返回 None。
pub fn compact(messages: &mut Vec<Message>, policy: &CompactionPolicy) -> Option<String> {
    let old_turns = split_for_compaction(messages, policy)?;
    let summary = summarize_turns(&old_turns);
    rebuild(messages, old_turns, summary)
}

/// LLM 压缩变体：用 provider 对旧轮次生成摘要，失败时回退确定性摘要。
pub async fn compact_with_llm(
    provider: &dyn baiji_ai::Provider,
    messages: &mut Vec<Message>,
    policy: &CompactionPolicy,
) -> Option<String> {
    let old_turns = split_for_compaction(messages, policy)?;

    // 旧轮次转录为纯 user/assistant 对话（截断防超长）
    let transcript = old_turns
        .iter()
        .flat_map(|turn| turn.iter())
        .filter_map(|m| match m.role {
            Role::User => Some(format!("User: {}", truncate_chars(&m.content, 400))),
            Role::Assistant => Some(format!("Assistant: {}", truncate_chars(&m.content, 400))),
            // 上一次压缩的摘要必须带入，否则更早的历史会被彻底遗忘
            Role::System => previous_summary(m)
                .map(|prev| format!("Summary of even earlier conversation:\n{prev}")),
            Role::Tool => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let request = ChatRequest::new(vec![
        Message::system(SUMMARIZE_PROMPT),
        Message::user(transcript),
    ])
    .with_max_tokens(1024);

    let summary = match provider.chat(request).await {
        Ok(response) if !response.content.trim().is_empty() => response.content,
        Ok(_) => {
            tracing::warn!("LLM compaction returned empty summary, falling back");
            summarize_turns(&old_turns)
        }
        Err(e) => {
            tracing::warn!("LLM compaction failed ({e}), falling back to deterministic summary");
            summarize_turns(&old_turns)
        }
    };

    rebuild(messages, old_turns, summary)
}

const SUMMARIZE_PROMPT: &str = "\
Summarize the following conversation turns for an AI coding agent's memory. \
Preserve: user goals, decisions made, files/paths touched, tool actions taken, and unresolved questions. \
Be concise (under 300 words), use bullet points, write in the conversation's language.";

/// 触发判定 + 切分：返回需要压缩的旧轮次（None = 不触发）
fn split_for_compaction(
    messages: &[Message],
    policy: &CompactionPolicy,
) -> Option<Vec<Vec<Message>>> {
    if estimate_tokens(messages) <= policy.max_estimated_tokens {
        return None;
    }
    let turns = group_into_turns(messages);
    if turns.len() <= policy.keep_recent_turns {
        return None;
    }
    let split = turns.len() - policy.keep_recent_turns;
    Some(turns[..split].to_vec())
}

/// 用 [摘要 System 消息 + 保留轮次] 替换原列表，返回摘要
fn rebuild(
    messages: &mut Vec<Message>,
    old_turns: Vec<Vec<Message>>,
    summary: String,
) -> Option<String> {
    let keep = old_turns.len();
    let all_turns = group_into_turns(messages);
    let mut rebuilt = vec![Message {
        role: Role::System,
        content: format!("{SUMMARY_PREFIX}{summary}"),
        tool_calls: None,
        tool_results: None,
        reasoning: None,
    }];
    for turn in all_turns.iter().skip(keep) {
        rebuilt.extend(turn.iter().cloned());
    }
    *messages = rebuilt;
    Some(summary)
}

/// 按对话轮次分组：一条 User 消息开启新轮次
pub fn group_into_turns(messages: &[Message]) -> Vec<Vec<Message>> {
    let mut turns: Vec<Vec<Message>> = Vec::new();
    let mut current: Vec<Message> = Vec::new();

    for msg in messages {
        if msg.role == Role::User && !current.is_empty() {
            turns.push(std::mem::take(&mut current));
        }
        current.push(msg.clone());
    }
    if !current.is_empty() {
        turns.push(current);
    }
    turns
}

const SUMMARY_PREFIX: &str = "[Conversation Summary]\n";

/// 若该消息是上一次压缩产生的摘要，返回摘要正文
fn previous_summary(message: &Message) -> Option<&str> {
    (message.role == Role::System)
        .then(|| message.content.strip_prefix(SUMMARY_PREFIX))
        .flatten()
}

/// 确定性摘要：每轮的 user 问题 + assistant 首句与工具概要。
/// 上一次压缩的摘要原样带入（否则二次压缩会丢掉更早的全部历史）。
fn summarize_turns(turns: &[Vec<Message>]) -> String {
    turns
        .iter()
        .enumerate()
        .map(|(i, turn)| {
            if let Some(prev) = turn.first().and_then(previous_summary) {
                return prev.to_string();
            }
            let user = turn
                .iter()
                .find(|m| m.role == Role::User)
                .map(|m| truncate_chars(&m.content, 80))
                .unwrap_or_default();

            let assistant = turn
                .iter()
                .find(|m| m.role == Role::Assistant)
                .map(|m| {
                    let first_line = truncate_chars(m.content.lines().next().unwrap_or(""), 100);
                    match &m.tool_calls {
                        Some(calls) if !calls.is_empty() => {
                            let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                            format!("{first_line} [used tools: {}]", names.join(", "))
                        }
                        _ => first_line,
                    }
                })
                .unwrap_or_else(|| "(no response)".to_string());

            format!("Turn {}: user asked \"{user}\" -> {assistant}", i + 1)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 按字符数截断（避免 panic 于多字节边界）
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn estimate_string_tokens(s: &str) -> usize {
    let ascii = s.chars().filter(|c| c.is_ascii()).count();
    let non_ascii = s.chars().filter(|c| !c.is_ascii()).count();
    ascii / 4 + non_ascii / 2 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_messages(turns: usize) -> Vec<Message> {
        let mut msgs = Vec::new();
        for i in 0..turns {
            msgs.push(Message::user(format!(
                "Question number {} about rust async programming",
                i + 1
            )));
            msgs.push(Message::assistant(format!(
                "Answer to question {} with a fairly detailed response covering several points.",
                i + 1
            )));
        }
        msgs
    }

    #[test]
    fn test_no_compaction_under_limit() {
        let policy = CompactionPolicy {
            max_estimated_tokens: 100_000,
            keep_recent_turns: 6,
        };
        let mut messages = make_messages(3);
        let before = messages.len();
        assert!(compact(&mut messages, &policy).is_none());
        assert_eq!(messages.len(), before);
    }

    #[test]
    fn test_compaction_folds_old_turns() {
        let policy = CompactionPolicy {
            max_estimated_tokens: 100,
            keep_recent_turns: 2,
        };
        let mut messages = make_messages(8);
        let summary = compact(&mut messages, &policy).expect("should compact");

        // [summary system] + 2 轮（4 条）
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].role, Role::System);
        assert!(messages[0].content.contains("[Conversation Summary]"));
        assert!(summary.contains("Turn 1:"));
        assert!(summary.contains("user asked"));
        // 最近 2 轮保持原文
        assert!(messages.last().unwrap().content.contains("question 8"));
    }

    #[test]
    fn test_compaction_keeps_too_few_turns() {
        // 超限但轮次不足 keep_recent_turns：不压缩
        let policy = CompactionPolicy {
            max_estimated_tokens: 10,
            keep_recent_turns: 10,
        };
        let mut messages = make_messages(3);
        assert!(compact(&mut messages, &policy).is_none());
    }

    #[test]
    fn test_group_into_turns_with_tool_messages() {
        let messages = vec![
            Message::user("q1"),
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: Some(vec![baiji_ai::ToolCall {
                    id: "t1".into(),
                    name: "grep".into(),
                    arguments: serde_json::json!({}),
                }]),
                tool_results: None,
                reasoning: None,
            },
            Message {
                role: Role::Tool,
                content: String::new(),
                tool_calls: None,
                tool_results: Some(vec![baiji_ai::ToolResult {
                    tool_call_id: "t1".into(),
                    content: "match".into(),
                }]),
                reasoning: None,
            },
            Message::assistant("done"),
            Message::user("q2"),
            Message::assistant("a2"),
        ];

        let turns = group_into_turns(&messages);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].len(), 4); // user + assistant + tool + assistant
        assert_eq!(turns[1].len(), 2);
    }

    #[test]
    fn test_estimate_tokens_mixed_language() {
        let english = vec![Message::user("Hello world this is english text")];
        let chinese = vec![Message::user("你好世界这是一段中文文本")];
        // 同等字符数下中文估算更高
        assert!(estimate_tokens(&chinese) >= estimate_tokens(&english) / 2);
        assert!(estimate_tokens(&english) > 0);
    }

    #[test]
    fn test_policy_for_context() {
        // 200k 窗口、8k 输出预留 → 132k
        assert_eq!(
            CompactionPolicy::for_context(200_000, 8_000).max_estimated_tokens,
            132_000
        );
        // 极小窗口不低于下限
        assert_eq!(
            CompactionPolicy::for_context(8_000, 8_000).max_estimated_tokens,
            16_000
        );
    }

    #[test]
    fn test_second_compaction_keeps_first_summary() {
        let policy = CompactionPolicy {
            max_estimated_tokens: 1,
            keep_recent_turns: 1,
        };
        let mut msgs = vec![
            Message::user("first question about alpha"),
            Message::assistant("alpha answer"),
            Message::user("second question"),
            Message::assistant("second answer"),
        ];
        let first = compact(&mut msgs, &policy).unwrap();
        assert!(first.contains("alpha"));

        msgs.push(Message::user("third question"));
        msgs.push(Message::assistant("third answer"));
        let second = compact(&mut msgs, &policy).unwrap();

        // 第一次摘要的内容仍在，且没有被渲染成空的 "user asked \"\""
        assert!(second.contains("alpha"), "{second}");
        assert!(second.contains("second question"), "{second}");
        assert!(!second.contains("user asked \"\""), "{second}");
        // 只保留一条摘要消息
        let summaries = msgs.iter().filter(|m| previous_summary(m).is_some()).count();
        assert_eq!(summaries, 1);
    }
}
