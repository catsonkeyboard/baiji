//! 工具调用修复：模型输出不规范时尽量救回，而不是让一轮白跑。
//!
//! - [`recover_text_tool_calls`]：无结构化工具调用、但正文里泄漏了调用信封
//!   （DSML `invoke/parameter`、`to=functions.NAME {json}`）→ 还原为真正的调用
//! - [`resolve_tool_name`]：`functions.read` / `read_file` / `Bash` 等常见别名映射到
//!   已注册工具；未知名给出 "did you mean" 建议与可用工具列表
//! - [`coerce_args`]：按工具 JSON Schema 宽松纠正参数类型（`"true"` → true、
//!   `"10"` → 10、JSON 字符串形式的数组/对象 → 解析）——否则
//!   `args["replace_all"].as_bool()` 会把 `"true"` 静默当成 false

use super::markup::{MarkupScan, parse_text_wrapped_tool_call, scan_tool_call_markup};
use crate::tool::ToolRegistry;
use baiji_ai::ToolCall;
use serde_json::{Map, Value};

/// 正文工具调用恢复结果
#[derive(Debug, Clone)]
pub(super) enum TextToolRecovery {
    None,
    /// 已还原的调用 + 去掉信封后的正文
    Recovered {
        calls: Vec<ToolCall>,
        sanitized: String,
    },
    /// 明显是调用信封但解析不了：不执行，提醒模型用原生接口重发
    Unparseable(String),
}

/// 从助手正文恢复工具调用。仅在响应**没有**结构化调用时调用；
/// 只接受解析到已注册工具（含别名）的调用，避免把正文里的示例当真执行。
pub(super) fn recover_text_tool_calls(text: &str, tools: &ToolRegistry) -> TextToolRecovery {
    if text.trim().is_empty() {
        return TextToolRecovery::None;
    }
    let known = |name: &str| resolve_tool_name(name, tools).ok();

    if let Some((prefix, name, arguments, suffix)) = parse_text_wrapped_tool_call(text)
        && let Some(resolved) = known(&name)
    {
        let sanitized = join_prose(&prefix, &suffix);
        return TextToolRecovery::Recovered {
            calls: vec![fallback_call(0, resolved, arguments)],
            sanitized,
        };
    }

    match scan_tool_call_markup(text) {
        MarkupScan::None => TextToolRecovery::None,
        MarkupScan::Unparseable { reason } => TextToolRecovery::Unparseable(reason),
        MarkupScan::Parsed {
            calls,
            sanitized_text,
        } => {
            let mut out = Vec::with_capacity(calls.len());
            for (i, call) in calls.into_iter().enumerate() {
                match known(&call.name) {
                    Some(resolved) => out.push(fallback_call(i, resolved, call.arguments)),
                    None => {
                        return TextToolRecovery::Unparseable(format!(
                            "unknown tool '{}' in markup",
                            call.name
                        ));
                    }
                }
            }
            TextToolRecovery::Recovered {
                calls: out,
                sanitized: sanitized_text,
            }
        }
    }
}

fn join_prose(prefix: &str, suffix: &str) -> String {
    match (prefix.is_empty(), suffix.is_empty()) {
        (true, _) => suffix.to_string(),
        (false, true) => prefix.to_string(),
        (false, false) => format!("{prefix}\n{suffix}"),
    }
}

fn fallback_call(index: usize, name: String, arguments: Value) -> ToolCall {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    ToolCall {
        id: format!("call_text_{nanos:x}_{index}"),
        name,
        arguments,
    }
}

/// 常见别名（其他 agent / 训练数据里的工具名）→ baiji 工具名
const TOOL_ALIASES: &[(&str, &str)] = &[
    ("shell", "bash"),
    ("sh", "bash"),
    ("run_shell", "bash"),
    ("run_command", "bash"),
    ("execute_command", "bash"),
    ("exec", "bash"),
    ("terminal", "bash"),
    ("read_file", "read"),
    ("view", "read"),
    ("view_file", "read"),
    ("cat", "read"),
    ("open_file", "read"),
    ("write_file", "write"),
    ("create_file", "write"),
    ("edit_file", "edit"),
    ("replace", "edit"),
    ("str_replace", "edit"),
    ("search_files", "grep"),
    ("ripgrep", "grep"),
    ("rg", "grep"),
    ("grep_search", "grep"),
    ("list_dir", "ls"),
    ("list_directory", "ls"),
    ("list_files", "ls"),
    ("glob", "find"),
    ("find_files", "find"),
    ("file_search", "find"),
    ("todo_write", "todo"),
    ("todowrite", "todo"),
    ("web_fetch", "webfetch"),
    ("fetch", "webfetch"),
    ("web_search", "websearch"),
];

/// 把模型给的工具名解析为已注册工具名；失败时返回给模型看的说明（含建议）
pub(crate) fn resolve_tool_name(name: &str, tools: &ToolRegistry) -> Result<String, String> {
    if tools.get(name).is_some() {
        return Ok(name.to_string());
    }
    let trimmed = name.trim();
    let stripped = ["functions.", "functions/", "functions:", "tools.", "tool."]
        .iter()
        .find_map(|p| trimmed.strip_prefix(p))
        .unwrap_or(trimmed);
    let names = tools.names();
    // 去前缀、大小写不敏感
    if let Some(found) = names
        .iter()
        .find(|n| n.eq_ignore_ascii_case(stripped))
    {
        return Ok(found.to_string());
    }
    let lowered = stripped.to_ascii_lowercase().replace('-', "_");
    if let Some((_, target)) = TOOL_ALIASES.iter().find(|(alias, _)| *alias == lowered)
        && tools.get(target).is_some()
    {
        return Ok(target.to_string());
    }

    let mut suggestions: Vec<(usize, &str)> = names
        .iter()
        .map(|n| (edit_distance(&lowered, &n.to_ascii_lowercase()), *n))
        .filter(|(d, n)| *d <= 2.max(n.len() / 3))
        .collect();
    suggestions.sort();
    let mut message = format!("[Unknown tool: {name}]");
    if let Some((_, best)) = suggestions.first() {
        message.push_str(&format!(" Did you mean '{best}'?"));
    }
    message.push_str(&format!(" Available tools: {}", names.join(", ")));
    Err(message)
}

/// Levenshtein 距离（工具名很短，O(n·m) 足够）
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// 按 JSON Schema 宽松纠正参数类型（只做无歧义的转换，其余原样保留）。
///
/// - 整个参数是 JSON 字符串 / null（部分模型这么发）→ 解析 / `{}`
/// - `boolean` ← `"true"/"false"/"yes"/"no"/"1"/"0"`
/// - `integer` / `number` ← 数字字符串
/// - `array` / `object` ← 能解析为对应类型的 JSON 字符串
/// - 递归进入 `properties` 与 `items`
pub(crate) fn coerce_args(schema: &Value, args: Value) -> Value {
    let args = match args {
        Value::Null => Value::Object(Map::new()),
        Value::String(s) if s.trim_start().starts_with('{') => {
            serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s))
        }
        other => other,
    };
    coerce_value(schema, args)
}

fn schema_types(schema: &Value) -> Vec<&str> {
    match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn coerce_value(schema: &Value, value: Value) -> Value {
    let types = schema_types(schema);
    let accepts = |t: &str| types.contains(&t);
    match value {
        Value::Object(map) if accepts("object") || schema.get("properties").is_some() => {
            let properties = schema.get("properties").and_then(Value::as_object);
            Value::Object(
                map.into_iter()
                    .map(|(k, v)| {
                        let v = match properties.and_then(|p| p.get(&k)) {
                            Some(prop) => coerce_value(prop, v),
                            None => v,
                        };
                        (k, v)
                    })
                    .collect(),
            )
        }
        Value::Array(items) if accepts("array") => {
            let item_schema = schema.get("items").cloned().unwrap_or(Value::Null);
            Value::Array(
                items
                    .into_iter()
                    .map(|v| coerce_value(&item_schema, v))
                    .collect(),
            )
        }
        Value::String(s) if !types.is_empty() && !accepts("string") => {
            coerce_string(&types, s, schema)
        }
        // 数字 → 布尔（0/1）这种模型也常犯
        Value::Number(n) if accepts("boolean") && !accepts("number") && !accepts("integer") => {
            match n.as_i64() {
                Some(0) => Value::Bool(false),
                Some(1) => Value::Bool(true),
                _ => Value::Number(n),
            }
        }
        other => other,
    }
}

fn coerce_string(types: &[&str], s: String, schema: &Value) -> Value {
    let trimmed = s.trim();
    for t in types {
        match *t {
            "boolean" => match trimmed.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => return Value::Bool(true),
                "false" | "no" | "0" => return Value::Bool(false),
                _ => {}
            },
            "integer" => {
                if let Ok(i) = trimmed.parse::<i64>() {
                    return Value::from(i);
                }
            }
            "number" => {
                if let Ok(i) = trimmed.parse::<i64>() {
                    return Value::from(i);
                }
                if let Ok(f) = trimmed.parse::<f64>()
                    && let Some(n) = serde_json::Number::from_f64(f)
                {
                    return Value::Number(n);
                }
            }
            "array" if trimmed.starts_with('[') => {
                if let Ok(v @ Value::Array(_)) = serde_json::from_str::<Value>(trimmed) {
                    return coerce_value(schema, v);
                }
            }
            "object" if trimmed.starts_with('{') => {
                if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(trimmed) {
                    return coerce_value(schema, v);
                }
            }
            "null" if trimmed.is_empty() || trimmed == "null" => return Value::Null,
            _ => {}
        }
    }
    Value::String(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{AgentTool, ToolOutput};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Arc;

    struct Named(&'static str);

    #[async_trait]
    impl AgentTool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            ""
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput::ok(""))
        }
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        for name in ["read", "write", "edit", "bash", "grep", "find", "ls", "mcp__gh__search"] {
            r.register(Arc::new(Named(name)));
        }
        r
    }

    #[test]
    fn test_resolve_tool_name_aliases_and_prefixes() {
        let r = registry();
        for (input, want) in [
            ("read", "read"),
            ("functions.read", "read"),
            ("Bash", "bash"),
            ("read_file", "read"),
            ("shell", "bash"),
            ("execute_command", "bash"),
            ("list_dir", "ls"),
            ("glob", "find"),
            ("str_replace", "edit"),
            ("functions.write_file", "write"),
            ("mcp__gh__search", "mcp__gh__search"),
        ] {
            assert_eq!(resolve_tool_name(input, &r).as_deref(), Ok(want), "{input}");
        }
    }

    #[test]
    fn test_unknown_tool_suggests_and_lists() {
        let r = registry();
        let err = resolve_tool_name("gerp", &r).unwrap_err();
        assert!(err.contains("Did you mean 'grep'"), "{err}");
        assert!(err.contains("Available tools: read"), "{err}");
        let err = resolve_tool_name("deploy_to_prod", &r).unwrap_err();
        assert!(!err.contains("Did you mean"), "{err}");
        // 别名目标未注册时不映射
        assert!(resolve_tool_name("web_fetch", &r).is_err());
    }

    #[test]
    fn test_coerce_args_by_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "replace_all": {"type": "boolean"},
                "limit": {"type": "integer"},
                "density": {"type": "number"},
                "paths": {"type": "array", "items": {"type": "string"}},
                "opts": {"type": "object", "properties": {"deep": {"type": "boolean"}}},
                "edits": {"type": "array", "items": {
                    "type": "object",
                    "properties": {"replace_all": {"type": "boolean"}}
                }},
                "text": {"type": "string"},
                "maybe": {"type": ["integer", "null"]}
            }
        });
        let out = coerce_args(
            &schema,
            json!({
                "replace_all": "true",
                "limit": "10",
                "density": "0.5",
                "paths": "[\"a\", \"b\"]",
                "opts": "{\"deep\": \"yes\"}",
                "edits": [{"replace_all": "false"}],
                "text": "123",
                "maybe": "7",
                "extra": "keep"
            }),
        );
        assert_eq!(
            out,
            json!({
                "replace_all": true,
                "limit": 10,
                "density": 0.5,
                "paths": ["a", "b"],
                "opts": {"deep": true},
                "edits": [{"replace_all": false}],
                "text": "123",
                "maybe": 7,
                "extra": "keep"
            })
        );
    }

    #[test]
    fn test_coerce_args_leaves_ambiguous_values() {
        let schema = json!({"type": "object", "properties": {
            "flag": {"type": "boolean"}, "n": {"type": "integer"}, "b2": {"type": "boolean"}
        }});
        let out = coerce_args(&schema, json!({"flag": "maybe", "n": "ten", "b2": 1}));
        assert_eq!(out, json!({"flag": "maybe", "n": "ten", "b2": true}));
        // 整体是 JSON 字符串 / null
        assert_eq!(
            coerce_args(&schema, json!("{\"n\": \"3\"}")),
            json!({"n": 3})
        );
        assert_eq!(coerce_args(&schema, Value::Null), json!({}));
        // 无 schema 信息时不动
        assert_eq!(coerce_args(&json!({}), json!({"x": "1"})), json!({"x": "1"}));
    }

    #[test]
    fn test_recover_dsml_markup_into_calls() {
        let r = registry();
        let text = "Running it.\n<｜DSML｜function_calls>\n<｜DSML｜invoke name=\"bash\">\n<｜DSML｜parameter name=\"command\" string=\"true\">ls</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜function_calls>";
        match recover_text_tool_calls(text, &r) {
            TextToolRecovery::Recovered { calls, sanitized } => {
                assert_eq!(sanitized, "Running it.");
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "bash");
                assert_eq!(calls[0].arguments, json!({"command": "ls"}));
                assert!(calls[0].id.starts_with("call_text_"));
            }
            other => panic!("expected recovery, got {other:?}"),
        }
    }

    #[test]
    fn test_recover_harmony_style_call() {
        let r = registry();
        let text = "Let me look.\nto=functions.read_file {\"path\": \"src/main.rs\"}";
        match recover_text_tool_calls(text, &r) {
            TextToolRecovery::Recovered { calls, sanitized } => {
                assert_eq!(sanitized, "Let me look.");
                assert_eq!(calls[0].name, "read");
                assert_eq!(calls[0].arguments, json!({"path": "src/main.rs"}));
            }
            other => panic!("expected recovery, got {other:?}"),
        }
    }

    #[test]
    fn test_recover_ignores_prose_and_unknown_tools() {
        let r = registry();
        assert!(matches!(
            recover_text_tool_calls("The build passed; I will invoke the tests next.", &r),
            TextToolRecovery::None
        ));
        // 代码块里的示例不算
        let fenced = "Format:\n```xml\n<invoke name=\"bash\">\n<parameter name=\"command\">ls</parameter>\n</invoke>\n```";
        assert!(matches!(recover_text_tool_calls(fenced, &r), TextToolRecovery::None));
        // 未注册的工具：不执行，按不可解析处理
        let unknown = "<invoke name=\"deploy\">\n<parameter name=\"env\">prod</parameter>\n</invoke>";
        assert!(matches!(
            recover_text_tool_calls(unknown, &r),
            TextToolRecovery::Unparseable(_)
        ));
        // to=functions 指向未知工具：当普通文本
        assert!(matches!(
            recover_text_tool_calls("to=functions.deploy {\"x\": 1}", &r),
            TextToolRecovery::None
        ));
    }
}
