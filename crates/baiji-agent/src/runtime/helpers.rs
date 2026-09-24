//! runtime 辅助函数：上下文精简（elide）、消息提交、重试退避、verbosity steer。
//! 从 mod.rs 拆出的自由函数与常量。

use crate::event::AgentEvent;
use baiji_ai::{Message, Role, StopReason};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// spill 闭包：内容 → ctx 句柄（main.rs 用 baiji_tools::spill_to_store 构造）
pub(crate) type SpillFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// 最近这么多条工具结果消息保持原样（模型正在用）
const KEEP_RECENT_TOOL_MESSAGES: usize = 3;
/// 小于此字节数的工具结果不值得精简
const ELIDE_MIN_BYTES: usize = 1024;
pub(super) const ELIDED_NOTE: &str = "[elided to save context — this older tool output was removed; re-run the tool if you need it again]";

/// 粗略 token 估算（厂商未上报 usage 时的兜底）：ASCII ≈ 4 字符/token，其它 ≈ 2
pub(super) fn rough_token_estimate(messages: &[Message]) -> usize {
    let estimate = |s: &str| {
        let ascii = s.bytes().filter(u8::is_ascii).count();
        let other = s.chars().count().saturating_sub(ascii);
        ascii / 4 + other / 2
    };
    messages
        .iter()
        .map(|m| {
            estimate(&m.content)
                + m.tool_calls
                    .iter()
                    .flatten()
                    .map(|c| estimate(&c.arguments.to_string()))
                    .sum::<usize>()
                + m.tool_results
                    .iter()
                    .flatten()
                    .map(|r| estimate(&r.content))
                    .sum::<usize>()
        })
        .sum()
}

/// 把较早的大块工具结果替换为占位说明，返回释放的字节数。
/// 消息条数与 tool_call ↔ tool_result 的配对保持不变（API 要求严格配对）。
/// spill 可用时占位携带 ctx 句柄（可逆，expand 取回）；否则回退不可逆占位。
pub(super) fn elide_old_tool_results(convo: &mut [Message], spill: Option<&SpillFn>) -> usize {
    let tool_positions: Vec<usize> = convo
        .iter()
        .enumerate()
        .filter(|(_, m)| m.tool_results.is_some())
        .map(|(i, _)| i)
        .collect();
    let cutoff = tool_positions
        .len()
        .saturating_sub(KEEP_RECENT_TOOL_MESSAGES);
    let mut freed = 0;
    for &position in &tool_positions[..cutoff] {
        for result in convo[position].tool_results.iter_mut().flatten() {
            if result.content.len() >= ELIDE_MIN_BYTES {
                let bytes = result.content.len();
                let replacement = match spill.and_then(|f| f(&result.content)) {
                    Some(handle) => format!(
                        "[ctx stub: {bytes} bytes of older tool output elided; \
                         full content handle: ctx:{handle} — call the expand tool with this handle to retrieve it]"
                    ),
                    None => ELIDED_NOTE.to_string(),
                };
                freed += bytes.saturating_sub(replacement.len());
                result.content = replacement;
            }
        }
    }
    freed
}

/// 新消息进入工作上下文，并通知持久化层增量落盘
pub(super) fn commit(convo: &mut Vec<Message>, events: &UnboundedSender<AgentEvent>, message: Message) {
    events
        .send(AgentEvent::MessageCommitted {
            message: message.clone(),
        })
        .ok();
    convo.push(message);
}

/// 重试退避延迟：优先服务端 Retry-After，否则 base×2^attempt；
/// 一律不超过上限（服务端要求过长等待也截断，避免悬挂）
pub(super) fn retry_delay(
    base: Duration,
    attempt: u32,
    cap: Duration,
    server_retry_after: Option<Duration>,
) -> Duration {
    let raw = server_retry_after.unwrap_or_else(|| base.saturating_mul(1u32 << attempt.min(16)));
    raw.min(cap)
}

/// verbosity steer 的恒定指令文本。逐字节恒定：同一会话内每轮请求的追加
/// 内容相同，此前的前缀在 provider 侧的自动前缀缓存中仍然命中。
pub(super) const STEER_SUFFIX: &str = "\n\n[System note: Be concise. Answer directly without restating the \
question or adding pleasantries; skip filler and long summaries unless asked for detail. Keep \
code, commands and facts complete.]";

/// 向最后一条 user 消息追加 verbosity 指令（只作用于请求副本；无 user 消息不注入）
pub(super) fn steer_last_user(messages: &mut [Message]) {
    for m in messages.iter_mut().rev() {
        if m.role == Role::User {
            m.content.push_str(STEER_SUFFIX);
            return;
        }
    }
}

/// steering 打断后，未执行的工具调用的占位结果
pub(super) const SKIPPED_BY_STEERING: &str = "[Skipped] 用户在运行中发来了新指令，此工具调用未执行";

/// 用户取消后，未执行/被中断的工具调用的结果
pub(super) const CANCELLED_BY_USER: &str = "[Cancelled] 用户取消了本次运行，此工具调用未完成";

pub(super) fn invalid_args_message(tool: &str, stop: Option<&StopReason>, max_tokens: u32) -> String {
    if stop == Some(&StopReason::MaxTokens) {
        format!(
            "[Error] tool '{tool}' was NOT executed: the response hit the output limit \
             (max_tokens={max_tokens}) and the arguments JSON was cut off. Retry with smaller \
             arguments — e.g. write the file in several smaller write/edit calls."
        )
    } else {
        format!(
            "[Error] tool '{tool}' was NOT executed: its arguments were not a valid JSON object \
             (the response was probably truncated). Send the call again with complete arguments."
        )
    }
}

pub(super) fn user_input_of(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// 把 convo 中新增的消息（index > 1 + history_len）追加到持久化历史
pub(super) fn sync_new_messages(convo: &[Message], history_len: usize, messages: &mut Vec<Message>) {
    let new_start = 1 + history_len; // 跳过 system + 原有历史
    messages.truncate(history_len); // 幂等：重复调用不会产生重复消息
    if convo.len() > new_start {
        messages.extend(convo[new_start..].iter().cloned());
    }
}
