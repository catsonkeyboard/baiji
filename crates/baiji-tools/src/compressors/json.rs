//! JSON 域压缩器：两段式（规则参考 tokenless json.rs 的 R1–R8）
//!
//! **无损段**（采纳门槛 15%）：紧凑重排 + 删黑名单字段（debug/trace/stack
//! 等诊断载荷）+ 删 null / 空值。黑名单与空值不携带任务信息，属结构清理。
//! **有损段**（命令成功且 ctx store 可用，输入 ≥2KB）：超长数组头 32 +
//! 尾 8 截断、超长字符串截断、深度裁剪；原文先 spill，标记携带句柄。

use crate::compressors::adopted;
use crate::env::ExecutionEnv;
use serde_json::{Map, Value};

/// 黑名单字段（小写比较）：诊断载荷，对任务几乎无信息量
const BLACKLIST: &[&str] = &[
    "debug",
    "trace",
    "traces",
    "stack",
    "stacktrace",
    "logs",
    "logging",
];
/// 字符串值截断阈值（字符数）
const MAX_STRING_CHARS: usize = 4096;
/// 数组头/尾保留条数（超过 40 条的数组才截断）
const ARRAY_KEEP_HEAD: usize = 32;
const ARRAY_KEEP_TAIL: usize = 8;
/// 最大嵌套深度
const MAX_DEPTH: usize = 8;
/// 有损段参与的最小输入（比无损段高：spill 与标记有固定成本）
const LOSSY_MIN_BYTES: usize = 2048;

/// 内容是 JSON 文档则解析返回（只认 `{` / `[` 开头，解析失败返回 None）
pub(crate) fn parse(content: &str) -> Option<Value> {
    let t = content.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    serde_json::from_str(t).ok()
}

pub(crate) fn compress(
    original: &str,
    value: &Value,
    env: &ExecutionEnv,
    allow_lossy: bool,
) -> String {
    // 无损段：紧凑重排 + 结构清理
    let compact = serde_json::to_string(&lossless(value)).unwrap_or_default();
    if !compact.is_empty() && adopted(original, &compact) {
        return compact;
    }
    if !allow_lossy || original.len() < LOSSY_MIN_BYTES {
        return original.to_string();
    }
    // 有损段：spill 失败（无 ctx store）即放弃——绝不发出取不回的内容
    let Some(handle) = env.spill(original) else {
        return original.to_string();
    };
    let truncated = serde_json::to_string(&bounded(&lossless(value), 0)).unwrap_or_default();
    if truncated.is_empty() || !adopted(original, &truncated) {
        return original.to_string();
    }
    format!(
        "{truncated}\n[json compressed — arrays capped at {ARRAY_KEEP_HEAD} head + {ARRAY_KEEP_TAIL} tail entries, long strings/depth trimmed; full original: ctx:{handle} (expand tool)]"
    )
}

/// 无损清理：删黑名单字段、null、空值（"" / [] / {}），其余原样保留
fn lossless(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, val) in map {
                let key = k.to_lowercase();
                if BLACKLIST.contains(&key.as_str()) || val.is_null() || is_empty_value(val) {
                    continue;
                }
                out.insert(k.clone(), lossless(val));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(lossless).collect()),
        other => other.clone(),
    }
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// 有界截断：数组头 32 尾 8、字符串 4096 字符、深度 8
fn bounded(v: &Value, depth: usize) -> Value {
    match v {
        Value::Array(items) if items.len() > ARRAY_KEEP_HEAD + ARRAY_KEEP_TAIL => {
            let mut out: Vec<Value> = items[..ARRAY_KEEP_HEAD]
                .iter()
                .map(|x| bounded(x, depth + 1))
                .collect();
            let omitted = items.len() - ARRAY_KEEP_HEAD - ARRAY_KEEP_TAIL;
            out.push(Value::String(format!(
                "… {omitted} more item(s) elided (recover via ctx handle)"
            )));
            out.extend(
                items[items.len() - ARRAY_KEEP_TAIL..]
                    .iter()
                    .map(|x| bounded(x, depth + 1)),
            );
            Value::Array(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|x| bounded(x, depth + 1)).collect()),
        Value::Object(map) if depth >= MAX_DEPTH => Value::String(format!(
            "… object with {} key(s) beyond depth {MAX_DEPTH} elided (recover via ctx handle)",
            map.len()
        )),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, x)| (k.clone(), bounded(x, depth + 1)))
                .collect(),
        ),
        Value::String(s) if s.chars().count() > MAX_STRING_CHARS => {
            let prefix: String = s.chars().take(MAX_STRING_CHARS).collect();
            Value::String(format!(
                "{prefix}…[string truncated: {MAX_STRING_CHARS}/{} chars]",
                s.chars().count()
            ))
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with_store() -> (tempfile::TempDir, ExecutionEnv) {
        let dir = tempfile::tempdir().unwrap();
        let env = ExecutionEnv::new(".").with_ctx_store(dir.path());
        (dir, env)
    }

    fn pretty(v: &Value) -> String {
        serde_json::to_string_pretty(v).unwrap()
    }

    #[test]
    fn test_lossless_cleanup_adopted() {
        let mut obj = serde_json::json!({
            "name": "baiji",
            "version": "1.2.3",
            "debug": {"steps": ["a", "b", "c"], "timing": [1, 2, 3]},
            "stackTrace": "at foo (a.rs:1:1)\n at bar (b.rs:2:2)",
            "message": "debug info as a VALUE must survive",
            "empty_list": [],
            "empty_str": "",
            "nulled": null,
            "count": 42,
        });
        // 撑过 512B 门槛：批量填充真实字段
        if let Value::Object(map) = &mut obj {
            for i in 0..20 {
                map.insert(format!("field_{i}"), Value::String(format!("value-{i}")));
            }
        }
        let original = pretty(&obj);
        let out = compress(&original, &obj, &ExecutionEnv::new("."), false);

        // 紧凑单行 + 采纳门槛
        assert!(!out.contains("\n"), "{out}");
        assert!(out.len() < original.len() * 85 / 100, "savings < 15%");
        // 任务事实逐字存活
        assert!(out.contains("\"name\":\"baiji\""));
        assert!(out.contains("\"version\":\"1.2.3\""));
        assert!(out.contains("\"count\":42"));
        assert!(out.contains("debug info as a VALUE must survive"));
        // 黑名单字段 / null / 空值被删
        assert!(!out.contains("stackTrace"));
        assert!(!out.contains("empty_list"));
        assert!(!out.contains("nulled"));
        assert!(!out.contains("null"));
        // 黑名单 key 是字段名，不是值文本：message 的值仍在
        assert!(!out.contains("\"debug\":"));
    }

    #[test]
    fn test_compact_input_without_targets_untouched() {
        // 已经紧凑、无黑名单/空值：无损不达门槛，无失真目标 → 原样返回
        let items: Vec<Value> = (0..30)
            .map(|i| serde_json::json!({"id": i, "n": format!("item-{i}")}))
            .collect();
        let compact = serde_json::to_string(&serde_json::json!({"items": items})).unwrap();
        assert!(compact.len() >= 512);
        let value: Value = serde_json::from_str(&compact).unwrap();
        let out = compress(&compact, &value, &ExecutionEnv::new("."), false);
        assert_eq!(out, compact);
    }

    #[test]
    fn test_lossy_denied_for_failed_commands() {
        // 紧凑输入 + 100 元素数组：无损不采纳；exit≠0（allow_lossy=false）→ 数组原样
        let items: Vec<Value> = (0..100)
            .map(|i| serde_json::json!({"id": i, "payload": format!("payload-number-{i}")}))
            .collect();
        let compact = serde_json::to_string(&serde_json::json!({"items": items})).unwrap();
        let value: Value = serde_json::from_str(&compact).unwrap();
        let out = compress(&compact, &value, &ExecutionEnv::new("."), false);
        assert_eq!(out, compact, "failed command must not lose content");
        assert!(out.matches("payload-number-").count() == 100);
    }

    #[test]
    fn test_lossy_array_cap_with_spill_and_recover() {
        let items: Vec<Value> = (0..100)
            .map(|i| serde_json::json!({"id": i, "payload": format!("payload-number-{i}")}))
            .collect();
        let compact = serde_json::to_string(&serde_json::json!({"items": items})).unwrap();
        assert!(compact.len() >= LOSSY_MIN_BYTES);
        let value: Value = serde_json::from_str(&compact).unwrap();

        let (_dir, env) = env_with_store();
        let out = compress(&compact, &value, &env, true);
        assert!(out.contains("60 more item(s) elided"), "{out}");
        assert!(out.contains("ctx:"), "must carry recovery handle");
        // 头 32 + 尾 8 存活
        assert!(out.contains("payload-number-0\""));
        assert!(out.contains("payload-number-31\""));
        assert!(out.contains("payload-number-99\""));
        assert!(!out.contains("payload-number-50"));
        // 原文可完整取回（expand 的底层路径）
        let handle = &out[out.find("ctx:").unwrap() + 4..][..16];
        assert_eq!(env.retrieve(handle).unwrap(), compact);
    }

    #[test]
    fn test_lossy_requires_ctx_store() {
        let items: Vec<Value> = (0..100)
            .map(|i| serde_json::json!({"id": i, "payload": format!("payload-number-{i}")}))
            .collect();
        let compact = serde_json::to_string(&serde_json::json!({"items": items})).unwrap();
        let value: Value = serde_json::from_str(&compact).unwrap();
        // 无 ctx store：spill 失败 → 放弃有损
        let out = compress(&compact, &value, &ExecutionEnv::new("."), true);
        assert_eq!(out, compact);
    }

    #[test]
    fn test_long_string_truncated_in_lossy_stage() {
        let long = "x".repeat(8000);
        let original =
            serde_json::to_string(&serde_json::json!({"blob": long, "pad": "y".repeat(300)}))
                .unwrap();
        let value: Value = serde_json::from_str(&original).unwrap();
        let (_dir, env) = env_with_store();
        let out = compress(&original, &value, &env, true);
        assert!(out.contains("string truncated"), "{out}");
        assert!(out.len() < original.len() * 85 / 100);
        assert!(out.contains("ctx:"));
    }

    #[test]
    fn test_depth_pruned_in_lossy_stage() {
        // 12 层嵌套对象：深度 8 以下裁剪后节省远超门槛
        let mut leaf = serde_json::json!({"data": "deep"});
        for _ in 0..12 {
            leaf = serde_json::json!({"child": leaf, "pad": "z".repeat(400)});
        }
        let original = serde_json::to_string(&leaf).unwrap();
        assert!(original.len() >= LOSSY_MIN_BYTES);
        let value: Value = serde_json::from_str(&original).unwrap();
        let (_dir, env) = env_with_store();
        let out = compress(&original, &value, &env, true);
        assert!(out.contains("beyond depth 8 elided"), "{out}");
    }

    #[test]
    fn test_parse_rejects_non_json() {
        assert!(parse("plain text log line").is_none());
        assert!(parse("{\"broken\": ").is_none());
        assert!(parse("{\"ok\": true}\nextra").is_none());
        assert!(parse("  {\"ok\": true}").is_some());
        assert!(parse("[1,2,3]").is_some());
    }
}
