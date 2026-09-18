//! 历史 tool_result 的 stub 化（上下文压缩的第一档，可逆）
//!
//! 全量摘要（见 [`crate::compaction`]）粒度太粗且不可逆。超预算时先做
//! 更便宜的一档：把保留窗口之外的**大体积** tool result 正文替换为
//! 短引用 + ctx 句柄（原文 spill 到内容寻址存储，模型可用 `expand`
//! 按需取回）。stub 化后仍超预算才走摘要压缩。
//!
//! - 纯内存视图：不改 JSONL（原文审计保留），每次运行从原文重新推导；
//!   spill 内容寻址天然幂等（同内容同句柄，重写是 no-op）。
//!   例外：`branch` 分叉会继承当前内存视图（含 stub）。
//! - 确定性：按轮次从旧到新整轮推进，预算够了就停（更近的历史优先保留
//!   原文；整轮边界保证同输入同输出，为 prompt cache 稳定性打底）。
//! - fail-open：无 ctx store 或 spill 失败时整档跳过，回到摘要路径。

use crate::compaction::{CompactionPolicy, estimate_tokens};
use baiji_agent::estimate_text_tokens;
use baiji_ai::{Message, Role};
use std::path::Path;

/// 低于此字节数的 tool result 不值得替换（stub 标记本身也有 token 成本）
const STUB_MIN_BYTES: usize = 512;

/// stub 标记前缀。幂等检测用：已替换的内容不再处理。
pub const STUB_PREFIX: &str = "[ctx stub:";

/// stub 化保留窗口之外的大体积 tool result。
/// 返回 `(替换条数, 节省的 token 估算)`；未触发返回 `(0, 0)`。
pub fn stub_tool_results(
    messages: &mut [Message],
    ctx_store: Option<&Path>,
    policy: &CompactionPolicy,
) -> (usize, usize) {
    let Some(store) = ctx_store else {
        return (0, 0);
    };
    if estimate_tokens(messages) <= policy.max_estimated_tokens {
        return (0, 0);
    }
    let Some(cutoff) = old_region_cutoff(messages, policy.keep_recent_turns) else {
        return (0, 0);
    };

    let mut boundaries = turn_start_indices(&messages[..cutoff]);
    boundaries.push(cutoff);
    let mut stubbed = 0usize;
    let mut saved_tokens = 0usize;
    for window in boundaries.windows(2) {
        let (start, end) = (window[0], window[1]);
        for message in &mut messages[start..end] {
            let Some(results) = message.tool_results.as_mut() else {
                continue;
            };
            for result in results.iter_mut() {
                let original = result.content.as_str();
                if original.len() < STUB_MIN_BYTES || original.starts_with(STUB_PREFIX) {
                    continue;
                }
                // spill 失败（磁盘错误）保持原文——宁可超预算也不发取不回的内容
                let Some(handle) = baiji_tools::spill_to_store(store, original) else {
                    continue;
                };
                let before = estimate_text_tokens(original);
                result.content = format!(
                    "{STUB_PREFIX} {bytes} bytes (~{before} tokens) of tool output elided; \
                     full content handle: ctx:{handle} — call the expand tool with this handle to retrieve it]",
                    bytes = original.len(),
                );
                saved_tokens += before.saturating_sub(estimate_text_tokens(&result.content));
                stubbed += 1;
            }
        }
        // 整轮处理完检查预算：够了就停，更近的旧轮次保留原文
        if estimate_tokens(messages) <= policy.max_estimated_tokens {
            break;
        }
    }
    (stubbed, saved_tokens)
}

/// 保留窗口之外区域的结束下标（None = 轮次不足，不动）。
/// 轮次语义与 [`crate::compaction::group_into_turns`] 一致：User 消息开启新轮。
fn old_region_cutoff(messages: &[Message], keep_recent_turns: usize) -> Option<usize> {
    let starts = turn_start_indices(messages);
    let total_turns = starts.len();
    if total_turns <= keep_recent_turns {
        return None;
    }
    // 第 split 轮（保留区第一条）的起始下标 = 旧区域边界
    Some(starts[total_turns - keep_recent_turns])
}

/// 每轮的起始下标（含第 0 轮的 0）。
/// 与 `group_into_turns` 同语义：User 消息且前面已有消息时开启新轮。
fn turn_start_indices(messages: &[Message]) -> Vec<usize> {
    let mut starts = vec![0usize];
    let mut seen_any = false;
    for (i, m) in messages.iter().enumerate() {
        if seen_any && m.role == Role::User {
            starts.push(i);
        }
        seen_any = true;
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;
    use baiji_ai::ToolResult;

    fn tool_message(id: &str, content: &str) -> Message {
        Message {
            role: Role::Tool,
            content: String::new(),
            tool_calls: None,
            tool_results: Some(vec![ToolResult {
                tool_call_id: id.into(),
                content: content.into(),
            }]),
            reasoning: None,
        }
    }

    /// 每轮：user + assistant + tool result（result_size 字节）
    fn history(turns: usize, result_size: usize) -> Vec<Message> {
        let mut msgs = Vec::new();
        for i in 0..turns {
            msgs.push(Message::user(format!("question number {i}")));
            msgs.push(Message::assistant(format!("answer number {i}")));
            msgs.push(tool_message(&format!("t{i}"), &"x".repeat(result_size)));
        }
        msgs
    }

    fn tool_contents(msgs: &[Message]) -> Vec<String> {
        msgs.iter()
            .filter_map(|m| m.tool_results.as_ref())
            .flat_map(|rs| rs.iter().map(|r| r.content.clone()))
            .collect()
    }

    fn handle_of(stub_content: &str) -> &str {
        &stub_content[stub_content.find("ctx:").unwrap() + 4..][..16]
    }

    #[test]
    fn test_stub_replaces_oldest_first_and_stops_at_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("ctx");
        // 每轮 ≈ 305 token（301 工具结果 + 文本），10 轮 ≈ 3050 > 3000
        let policy = CompactionPolicy {
            max_estimated_tokens: 3_000,
            keep_recent_turns: 2,
        };
        let mut msgs = history(10, 1200);
        assert!(estimate_tokens(&msgs) > 3_000);

        let (stubbed, saved) = stub_tool_results(&mut msgs, Some(store.as_path()), &policy);
        // 第一轮 stub 后即达预算：只替换最老的一轮
        assert_eq!(stubbed, 1, "expected exactly the oldest turn stubbed");
        assert!(saved > 0);
        assert!(
            estimate_tokens(&msgs) <= 3_000,
            "stub must bring estimate under budget"
        );

        let contents = tool_contents(&msgs);
        assert!(contents[0].starts_with(STUB_PREFIX), "{}", contents[0]);
        assert!(contents[0].contains("1200 bytes"));
        // 其余（含保留窗口）原样
        for c in &contents[1..] {
            assert_eq!(c, &"x".repeat(1200));
        }
    }

    #[test]
    fn test_stub_multiple_turns_until_budget() {
        let dir = tempfile::tempdir().unwrap();
        let policy = CompactionPolicy {
            max_estimated_tokens: 2_400,
            keep_recent_turns: 2,
        };
        let mut msgs = history(10, 1200);
        let (stubbed, _) = stub_tool_results(&mut msgs, Some(dir.path()), &policy);
        // ~3050 → stub 到 ≤2400：需 4 轮（每轮省 ~276）
        assert_eq!(stubbed, 4, "expected 4 oldest turns stubbed");
        let contents = tool_contents(&msgs);
        for c in &contents[..4] {
            assert!(c.starts_with(STUB_PREFIX));
        }
        for c in &contents[4..] {
            assert_eq!(c, &"x".repeat(1200));
        }
    }

    #[test]
    fn test_stub_is_reversible_via_expand_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("ctx");
        let policy = CompactionPolicy {
            max_estimated_tokens: 200,
            keep_recent_turns: 1,
        };
        let original_content = "meaningful tool output\n".repeat(60); // 1.3KB
        let mut msgs = vec![
            Message::user("q1"),
            Message::assistant("a1"),
            tool_message("t1", &original_content),
            Message::user("q2"),
            Message::assistant("a2"),
        ];
        let (stubbed, _) = stub_tool_results(&mut msgs, Some(store.as_path()), &policy);
        assert_eq!(stubbed, 1);

        // stub 内容带句柄；原文可经 expand 的底层路径取回
        let stub_content = &tool_contents(&msgs)[0];
        let handle = handle_of(stub_content);
        let env = baiji_tools::ExecutionEnv::new(".").with_ctx_store(&store);
        assert_eq!(env.retrieve(handle).unwrap(), original_content);
    }

    #[test]
    fn test_stub_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let policy = CompactionPolicy {
            max_estimated_tokens: 2_000,
            keep_recent_turns: 2,
        };
        let mut msgs = history(8, 1200);
        let first = stub_tool_results(&mut msgs, Some(dir.path()), &policy);
        assert!(first.0 > 0);
        let snapshot = tool_contents(&msgs);

        // 第二次：已 stub 的内容被识别，不再处理（返回 0）
        let second = stub_tool_results(&mut msgs, Some(dir.path()), &policy);
        assert_eq!(second, (0, 0));
        assert_eq!(tool_contents(&msgs), snapshot);
    }

    #[test]
    fn test_stub_without_store_is_noop() {
        let policy = CompactionPolicy {
            max_estimated_tokens: 500,
            keep_recent_turns: 1,
        };
        let mut msgs = history(5, 1200);
        let before = tool_contents(&msgs);
        assert_eq!(stub_tool_results(&mut msgs, None, &policy), (0, 0));
        assert_eq!(tool_contents(&msgs), before);
    }

    #[test]
    fn test_stub_under_budget_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let policy = CompactionPolicy {
            max_estimated_tokens: 100_000,
            keep_recent_turns: 2,
        };
        let mut msgs = history(5, 1200);
        assert_eq!(
            stub_tool_results(&mut msgs, Some(dir.path()), &policy),
            (0, 0)
        );
    }

    #[test]
    fn test_stub_skips_small_results() {
        let dir = tempfile::tempdir().unwrap();
        let policy = CompactionPolicy {
            max_estimated_tokens: 100,
            keep_recent_turns: 1,
        };
        // 结果只有 100B（< STUB_MIN_BYTES）：无可替换，宁可留给摘要
        let mut msgs = history(5, 100);
        assert_eq!(
            stub_tool_results(&mut msgs, Some(dir.path()), &policy),
            (0, 0)
        );
    }

    #[test]
    fn test_stub_then_compact_not_needed() {
        let dir = tempfile::tempdir().unwrap();
        let policy = CompactionPolicy {
            max_estimated_tokens: 3_000,
            keep_recent_turns: 2,
        };
        let mut msgs = history(10, 1200);
        stub_tool_results(&mut msgs, Some(dir.path()), &policy);
        // stub 化已达预算：不需要全量摘要（不产生 Summary 记录）
        assert!(crate::compaction::compact(&mut msgs, &policy).is_none());
    }

    #[test]
    fn test_stub_deterministic() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let policy = CompactionPolicy {
            max_estimated_tokens: 2_400,
            keep_recent_turns: 2,
        };
        let mut a = history(10, 1200);
        let mut b = history(10, 1200);
        stub_tool_results(&mut a, Some(dir_a.path()), &policy);
        stub_tool_results(&mut b, Some(dir_b.path()), &policy);
        assert_eq!(tool_contents(&a), tool_contents(&b));
    }

    #[test]
    fn test_old_region_cutoff_matches_turn_grouping() {
        // 与 compaction::group_into_turns 的轮次语义一致：
        // [System(摘要), User, Tool, Assistant, User, Assistant] = 2 轮，keep 1 → cutoff 在第二个 User
        let msgs = vec![
            Message::system("[Conversation Summary]\nold"),
            Message::user("q1"),
            tool_message("t1", "r"),
            Message::assistant("a1"),
            Message::user("q2"),
            Message::assistant("a2"),
        ];
        // 带头 System（上一次摘要）按 group_into_turns 语义算独立一轮：共 3 轮
        assert_eq!(old_region_cutoff(&msgs, 1), Some(4));
        assert_eq!(old_region_cutoff(&msgs, 2), Some(1));
        assert_eq!(old_region_cutoff(&msgs, 3), None);
        assert_eq!(
            crate::compaction::group_into_turns(&msgs).len(),
            turn_start_indices(&msgs).len()
        );
    }
}
