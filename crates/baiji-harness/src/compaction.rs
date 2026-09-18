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
                .map(|rs| {
                    rs.iter()
                        .map(|r| estimate_string_tokens(&r.content))
                        .sum::<usize>()
                })
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
Carry over the 'Files read:' / 'Files modified:' lines from the earlier summary, extended with this round's files. Be concise (under 300 words), use bullet points, write in the conversation's language.";

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

/// 摘要尾部的文件清单行前缀（我们的固定格式，跨压缩解析续传）
const FILES_READ_PREFIX: &str = "Files read: ";
const FILES_MODIFIED_PREFIX: &str = "Files modified: ";
/// 单类文件清单上限（超出折叠计数）
const MAX_FILES_PER_KIND: usize = 30;

/// 确定性摘要：每轮的 user 问题 + assistant 首句与工具概要 +
/// 文件操作清单尾节（resume 后模型能立刻知道动过哪些文件）。
/// 上一次压缩的摘要正文原样带入，其文件清单拆出与本次合并（否则二次压缩会丢掉更早的历史）。
fn summarize_turns(turns: &[Vec<Message>]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut files_read: Vec<String> = Vec::new();
    let mut files_modified: Vec<String> = Vec::new();

    for (i, turn) in turns.iter().enumerate() {
        if let Some(prev) = turn.first().and_then(previous_summary) {
            let (body, prev_read, prev_modified) = split_file_tail(prev);
            merge_unique(&mut files_read, prev_read);
            merge_unique(&mut files_modified, prev_modified);
            lines.push(body);
            continue;
        }
        // 收集本轮工具调用涉及的文件（bash 不解析：路径噪声大）
        for message in turn {
            for call in message.tool_calls.iter().flatten() {
                let Some(path) = call.arguments["path"].as_str() else {
                    continue;
                };
                let bucket = match call.name.as_str() {
                    "write" | "edit" => &mut files_modified,
                    "read" | "grep" | "find" | "ls" | "search" | "imports" => &mut files_read,
                    _ => continue,
                };
                merge_unique(bucket, [path.to_string()]);
            }
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

        lines.push(format!(
            "Turn {}: user asked \"{user}\" -> {assistant}",
            i + 1
        ));
    }

    if !files_read.is_empty() {
        lines.push(format!("{FILES_READ_PREFIX}{}", render_files(&files_read)));
    }
    if !files_modified.is_empty() {
        lines.push(format!(
            "{FILES_MODIFIED_PREFIX}{}",
            render_files(&files_modified)
        ));
    }
    lines.join("\n")
}

/// 把上一次摘要拆成（正文, 读文件清单, 改文件清单）。
/// 清单行在任何位置都识别（我们的固定格式，幂等）。
fn split_file_tail(summary: &str) -> (String, Vec<String>, Vec<String>) {
    let mut read = Vec::new();
    let mut modified = Vec::new();
    let mut body: Vec<&str> = Vec::new();
    for line in summary.lines() {
        if let Some(rest) = line.strip_prefix(FILES_READ_PREFIX) {
            read.extend(split_file_list(rest));
        } else if let Some(rest) = line.strip_prefix(FILES_MODIFIED_PREFIX) {
            modified.extend(split_file_list(rest));
        } else {
            body.push(line);
        }
    }
    while body.last().is_some_and(|l| l.trim().is_empty()) {
        body.pop();
    }
    (body.join("\n"), read, modified)
}

fn split_file_list(list: &str) -> Vec<String> {
    list.split(", ")
        .map(|f| f.trim().trim_start_matches("… +"))
        .filter(|f| !f.is_empty() && !f.ends_with(" more"))
        .map(str::to_string)
        .collect()
}

fn merge_unique(dst: &mut Vec<String>, src: impl IntoIterator<Item = String>) {
    for item in src {
        if !dst.contains(&item) {
            dst.push(item);
        }
    }
}

fn render_files(files: &[String]) -> String {
    if files.len() <= MAX_FILES_PER_KIND {
        files.join(", ")
    } else {
        format!(
            "{}, … +{} more",
            files[..MAX_FILES_PER_KIND].join(", "),
            files.len() - MAX_FILES_PER_KIND
        )
    }
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
    // 与工具层台账共用同一估算器（baiji-agent 提供），保证口径一致
    baiji_agent::estimate_text_tokens(s)
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

    /// 构造一轮带工具调用的消息（assistant 携带 tool_calls，tool 角色带结果）
    fn tool_turn(user: &str, calls: &[(&str, &str)]) -> Vec<Message> {
        let mut turn = vec![Message::user(user)];
        turn.push(Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: Some(
                calls
                    .iter()
                    .map(|(name, path)| baiji_ai::ToolCall {
                        id: format!("t-{name}-{path}"),
                        name: name.to_string(),
                        arguments: serde_json::json!({ "path": path }),
                    })
                    .collect(),
            ),
            tool_results: None,
            reasoning: None,
        });
        turn.push(Message {
            role: Role::Tool,
            content: String::new(),
            tool_calls: None,
            tool_results: Some(
                calls
                    .iter()
                    .map(|(name, path)| baiji_ai::ToolResult {
                        tool_call_id: format!("t-{name}-{path}"),
                        content: "ok".into(),
                    })
                    .collect(),
            ),
            reasoning: None,
        });
        turn
    }

    #[test]
    fn test_summary_includes_file_operations() {
        let policy = CompactionPolicy {
            max_estimated_tokens: 10,
            keep_recent_turns: 1,
        };
        let mut msgs: Vec<Message> = vec![];
        for turn in [
            tool_turn(
                "读一下结构",
                &[("read", "src/main.rs"), ("grep", "src/lib")],
            ),
            tool_turn(
                "改一下",
                &[("edit", "src/main.rs"), ("write", "docs/new.md")],
            ),
            tool_turn("再看看", &[("bash", "ignored.rs")]),
        ] {
            msgs.extend(turn);
        }
        msgs.push(Message::user("final"));
        msgs.push(Message::assistant("done"));

        let summary = compact(&mut msgs, &policy).expect("should compact");
        // 读/改清单各归其位；bash 不解析；去重
        assert!(
            summary.contains("Files read: src/main.rs, src/lib"),
            "{summary}"
        );
        assert!(
            summary.contains("Files modified: src/main.rs, docs/new.md"),
            "{summary}"
        );
        assert!(!summary.contains("ignored.rs"), "{summary}");
    }

    #[test]
    fn test_file_lists_merge_across_compactions() {
        let policy = CompactionPolicy {
            max_estimated_tokens: 10,
            keep_recent_turns: 1,
        };
        let mut msgs: Vec<Message> = tool_turn("first", &[("read", "a.rs")])
            .into_iter()
            .collect::<Vec<_>>();
        // 顶满轮次让首轮可折叠
        msgs.push(Message::user("pad"));
        msgs.push(Message::assistant("pad"));
        let first = compact(&mut msgs, &policy).expect("first compaction");
        assert!(first.contains("Files read: a.rs"), "{first}");

        // 新增一轮读了 b.rs，再次压缩：旧清单保留且合并新文件，不重复出现两行
        msgs.extend(tool_turn("second", &[("read", "b.rs")]));
        msgs.push(Message::user("pad2"));
        msgs.push(Message::assistant("pad2"));
        let second = compact(&mut msgs, &policy).expect("second compaction");
        assert!(second.contains("a.rs"), "{second}");
        assert!(second.contains("b.rs"), "{second}");
        assert_eq!(second.matches("Files read:").count(), 1, "{second}");
    }

    #[test]
    fn test_file_list_cap_folds_with_count() {
        let turns: Vec<Vec<Message>> = (0..40)
            .map(|i| tool_turn("q", &[("read", &format!("src/file{i:02}.rs"))]))
            .collect();
        let summary = summarize_turns(&turns);
        assert!(summary.contains("… +10 more"), "{summary}");
        assert!(summary.contains("src/file00.rs"), "{summary}");
        assert!(summary.contains("src/file29.rs"), "{summary}");
        assert!(!summary.contains("src/file30.rs"), "{summary}");
    }

    #[test]
    fn test_summary_without_files_has_no_file_lines() {
        let summary = summarize_turns(&[make_one_turn_no_tools()]);
        assert!(!summary.contains("Files read:"), "{summary}");
        // 占位实现：单轮无工具
        fn make_one_turn_no_tools() -> Vec<Message> {
            vec![Message::user("q"), Message::assistant("a")]
        }
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
        let summaries = msgs
            .iter()
            .filter(|m| previous_summary(m).is_some())
            .count();
        assert_eq!(summaries, 1);
    }
}
