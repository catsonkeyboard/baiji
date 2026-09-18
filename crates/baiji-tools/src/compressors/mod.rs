//! 内容感知压缩管道（借鉴 lean-ctx / ANOLISA tokenless 的域压缩器思想）
//!
//! 在通用 shell 规则（噪声行过滤 + 重复折叠）之后、字节截断之前，
//! 按内容类型派发到域压缩器：JSON / 表格 / 构建日志。
//!
//! 三条纪律（与 tokenless 的 PostTool 仲裁一致）：
//! 1. **无损变换节省不足 15% 不采纳**——标记与重排本身也有 token 成本；
//! 2. **命令失败（exit≠0 / 超时）只做无损清理**——诊断上下文宁可多给；
//! 3. **任何有损变换必须先把完整原文 spill 到 ctx store**（可经 expand
//!    凭句柄取回），spill 不可用则放弃有损（fail-open：原样返回）。

mod build_log;
mod json;
pub(crate) mod search_results;
mod tabular;

use crate::env::ExecutionEnv;

/// 参与域压缩的最小输入：更小的输出本来就不贵，标记本身有成本
const MIN_INPUT_BYTES: usize = 512;
/// 采纳门槛：候选长度必须降到原文的 85% 以下（即节省 ≥15%）
const ADOPT_RATIO: f64 = 0.15;

/// 门槛仲裁：候选是否值得采纳
pub(crate) fn adopted(original: &str, candidate: &str) -> bool {
    candidate.len() as f64 <= (original.len() as f64) * (1.0 - ADOPT_RATIO)
}

/// 剥离 ANSI CSI/SGR 转义序列（无损：颜色/样式不携带信息）。
/// 只删 CSI 序列（`ESC[...m` 等），不动 `\r` 重绘与 OSC——那些可能承载进度语义。
pub(crate) fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // CSI：ESC [ <参数字节> <终结字节>
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                // 参数 0x30–0x3F，中间 0x20–0x2F，终结 0x40–0x7E
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        } else {
            // 非 CSI 的 ESC（OSC 等）：原样保留，避免误删
            out.push('\x1b');
        }
    }
    out
}

/// 内容感知压缩入口：分类 → 域压缩器 → 门槛仲裁。
/// `allow_lossy` 仅当命令成功（exit=0）时为 true。
pub(crate) fn compress(content: &str, env: &ExecutionEnv, allow_lossy: bool) -> String {
    // 域压缩关闭（A/B 对照臂）：只保留通用 shell 规则与截断
    if content.len() < MIN_INPUT_BYTES || !env.compression_enabled {
        return content.to_string();
    }
    if let Some(value) = json::parse(content) {
        return json::compress(content, &value, env, allow_lossy);
    }
    if tabular::detect(content) {
        return tabular::compress(content, env, allow_lossy);
    }
    if build_log::detect(content) {
        return build_log::compress(content, env, allow_lossy);
    }
    content.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> ExecutionEnv {
        ExecutionEnv::new(".")
    }

    #[test]
    fn test_small_input_untouched() {
        let tiny = "{\"a\": 1}";
        assert_eq!(compress(tiny, &env(), true), tiny);
    }

    #[test]
    fn test_json_routed() {
        // 撑过 512B 管道门槛的 pretty JSON
        let mut obj = serde_json::Map::new();
        for i in 0..24 {
            obj.insert(
                format!("key_{i}"),
                serde_json::Value::String(format!("value-{i}")),
            );
        }
        obj.insert(
            "nested".into(),
            serde_json::json!({"a": 1, "b": 2, "c": [1, 2, 3]}),
        );
        let pretty = format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::Value::Object(obj)).unwrap()
        );
        assert!(pretty.len() >= MIN_INPUT_BYTES);
        let out = compress(&pretty, &env(), false);
        // 无损紧凑被采纳：单行、无换行缩进
        assert!(!out.contains('\n'), "{out}");
        assert!(out.contains("\"key_1\":\"value-1\""));
    }

    #[test]
    fn test_compression_disabled_returns_original() {
        // A/B 对照臂：域压缩关闭，pretty JSON 原样返回（不走无损紧凑）
        let mut obj = serde_json::Map::new();
        for i in 0..24 {
            obj.insert(
                format!("key_{i}"),
                serde_json::Value::String(format!("value-{i}")),
            );
        }
        let pretty = format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::Value::Object(obj)).unwrap()
        );
        assert!(pretty.len() >= MIN_INPUT_BYTES);
        let mut env = env();
        env.compression_enabled = false;
        assert_eq!(compress(&pretty, &env, true), pretty);
        // 开关打开时同一输入被压缩
        assert_ne!(compress(&pretty, &env_with_compression(), true), pretty);
    }

    fn env_with_compression() -> ExecutionEnv {
        let mut env = ExecutionEnv::new(".");
        env.compression_enabled = true;
        env
    }

    #[test]
    fn test_unknown_content_untouched() {
        let prose = "some long prose output\n".repeat(60);
        assert_eq!(compress(&prose, &env(), true), prose);
    }

    #[test]
    fn test_strip_ansi_removes_sgr_only() {
        assert_eq!(strip_ansi("\x1b[32mok\x1b[0m"), "ok");
        assert_eq!(strip_ansi("\x1b[1;31merror\x1b[0m: boom"), "error: boom");
        // 非 CSI 的 ESC 原样保留
        assert_eq!(strip_ansi("a\x1b]0;title\x07b"), "a\x1b]0;title\x07b");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
