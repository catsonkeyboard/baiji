//! 工具 JSON Schema 按 provider 方言归一化（白名单裁剪 + 结构变换）
//!
//! 每次请求都会把全部工具（内置 + 各 MCP 服务器）的 schema 发给模型，任何一个
//! provider 不认的关键字都会让**每一轮**请求 400、整个厂商不可用。这里在发请求前
//! 统一裁剪：只保留该协议已验证接受的关键字，删除的只是给模型看的约束——工具 /
//! MCP 服务器执行时仍会校验真实参数，语义不丢。
//!
//! 方言按 baiji 的两条协议路径划分：OpenAI 兼容端点（Chat Completions / Responses，含 GLM、Kimi、
//! DeepSeek、百炼、OpenRouter 等转发）取 OpenAI 与 OpenRouter 规则的并集；Anthropic
//! Messages 用 Anthropic 方言。

mod dialect;
mod keyword;

pub use dialect::{DialectSpec, DialectTransforms, apply};
use serde_json::Value;

const DEFAULT_TRANSFORMS: DialectTransforms = DialectTransforms {
    flatten_top_level_combiners: false,
    flatten_all_combiners: false,
    require_properties_on_objects: false,
    prune_dangling_required: false,
    const_as_enum: false,
    one_of_as_any_of: false,
    merge_all_of_branches: false,
};

/// OpenAI 兼容协议（Chat Completions / Responses）：OpenAI function-parameters 子集，
/// 叠加 OpenRouter/LM Studio 的约束（顶层组合子展平、object 必须带 properties）
pub const OPENAI_COMPATIBLE: DialectSpec = DialectSpec {
    id: "openai-compatible",
    supported_keywords: &[
        "description",
        "default",
        "examples",
        "format",
        "anyOf",
        "oneOf",
        "allOf",
        "$defs",
        "definitions",
        "$ref",
        "$schema",
        "const",
        "additionalProperties",
        "patternProperties",
        "prefixItems",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "pattern",
        "minItems",
        "maxItems",
        "nullable",
    ],
    // 严格校验器不认 `uri` 等格式
    supported_string_formats: &[
        "date-time",
        "time",
        "date",
        "duration",
        "email",
        "hostname",
        "ipv4",
        "ipv6",
        "uuid",
    ],
    transforms: DialectTransforms {
        // 根上的 oneOf/anyOf/allOf 被拒（"schema must have type 'object'"）
        flatten_top_level_combiners: true,
        one_of_as_any_of: true,
        // 接受 allOf 语法却不求交：属性只在 allOf 分支里的工具会变成零参数
        merge_all_of_branches: true,
        // LM Studio 等要求 object schema 带 properties
        require_properties_on_objects: true,
        ..DEFAULT_TRANSFORMS
    },
};

/// Anthropic Messages：属性内接受完整 draft 2020-12，但拒绝根上的组合子，
/// 且 `input_schema` 必须是带 `properties` 的 object
pub const ANTHROPIC: DialectSpec = DialectSpec {
    id: "anthropic",
    supported_keywords: &[
        "description",
        "default",
        "examples",
        "format",
        "anyOf",
        "oneOf",
        "allOf",
        "not",
        "if",
        "then",
        "else",
        "$defs",
        "definitions",
        "$ref",
        "$schema",
        "$comment",
        "const",
        "additionalProperties",
        "patternProperties",
        "propertyNames",
        "prefixItems",
        "contains",
        "uniqueItems",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "pattern",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
        "dependentRequired",
        "dependentSchemas",
        "nullable",
    ],
    supported_string_formats: &[],
    transforms: DialectTransforms {
        flatten_top_level_combiners: true,
        require_properties_on_objects: true,
        ..DEFAULT_TRANSFORMS
    },
};

/// 按方言归一化一个工具参数 schema
pub fn normalize(schema: &Value, spec: &DialectSpec) -> Value {
    apply(schema, spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_dialects_are_coherent() {
        OPENAI_COMPATIBLE.validate().unwrap();
        ANTHROPIC.validate().unwrap();
    }

    #[test]
    fn test_builtin_style_schema_is_preserved() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path"},
                "limit": {"type": "integer", "minimum": 1},
                "mode": {"type": "string", "enum": ["full", "map"]},
                "paths": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["path"]
        });
        assert_eq!(normalize(&schema, &OPENAI_COMPATIBLE), schema);
        assert_eq!(normalize(&schema, &ANTHROPIC), schema);
    }

    #[test]
    fn test_bare_object_gains_properties() {
        let bare = json!({"type": "object"});
        let want = json!({"type": "object", "properties": {}});
        assert_eq!(normalize(&bare, &OPENAI_COMPATIBLE), want);
        assert_eq!(normalize(&bare, &ANTHROPIC), want);
        assert_eq!(normalize(&json!({}), &ANTHROPIC), want);
    }

    #[test]
    fn test_unsupported_keywords_and_formats_dropped_for_openai() {
        let schema = json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "format": "uri"},
                "when": {"type": "string", "format": "date-time"},
                "tags": {"type": "array", "items": {"type": "string"}, "uniqueItems": true},
                "meta": {"type": "object", "properties": {}, "propertyNames": {"pattern": "^x"}},
                // 属性名恰好叫关键字：不能被误删
                "uniqueItems": {"type": "boolean"}
            },
            "x-vendor": 1
        });
        let out = normalize(&schema, &OPENAI_COMPATIBLE);
        assert_eq!(out["properties"]["url"], json!({"type": "string"}));
        assert_eq!(out["properties"]["when"]["format"], "date-time");
        assert!(out["properties"]["tags"].get("uniqueItems").is_none());
        assert!(out["properties"]["meta"].get("propertyNames").is_none());
        assert_eq!(out["properties"]["uniqueItems"], json!({"type": "boolean"}));
        assert!(out.get("x-vendor").is_none());
        // Anthropic 接受这些关键字
        let out = normalize(&schema, &ANTHROPIC);
        assert_eq!(out["properties"]["tags"]["uniqueItems"], true);
        assert_eq!(out["properties"]["url"]["format"], "uri");
    }

    #[test]
    fn test_top_level_combiners_are_flattened() {
        let schema = json!({
            "oneOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
                {"type": "object", "properties": {"b": {"type": "integer"}}}
            ]
        });
        for spec in [&OPENAI_COMPATIBLE, &ANTHROPIC] {
            let out = normalize(&schema, spec);
            assert_eq!(out["type"], "object", "{}", spec.id);
            assert!(out.get("oneOf").is_none() && out.get("anyOf").is_none());
            assert_eq!(out["properties"]["a"]["type"], "string");
            assert_eq!(out["properties"]["b"]["type"], "integer");
        }
    }

    #[test]
    fn test_all_of_branches_merge_for_openai() {
        let schema = json!({
            "type": "object",
            "properties": {"shared": {"type": "string"}},
            "allOf": [
                {"properties": {"x": {"type": "number"}}, "required": ["x"]}
            ]
        });
        let out = normalize(&schema, &OPENAI_COMPATIBLE);
        assert!(out.get("allOf").is_none());
        assert_eq!(out["properties"]["x"]["type"], "number");
        assert_eq!(out["required"], json!(["x"]));
    }

    #[test]
    fn test_all_of_ref_is_hoisted_not_dropped() {
        let schema = json!({
            "type": "object",
            "$defs": {"Color": {"type": "string", "enum": ["red", "blue"]}},
            "properties": {
                "color": {"allOf": [{"$ref": "#/$defs/Color"}], "description": "Pick one"}
            }
        });
        let out = normalize(&schema, &OPENAI_COMPATIBLE);
        let color = &out["properties"]["color"];
        assert_eq!(color["$ref"], "#/$defs/Color", "{out:#}");
        assert_eq!(color["description"], "Pick one");
        assert!(color.get("allOf").is_none());
        assert_eq!(out["$defs"]["Color"]["enum"], json!(["red", "blue"]));
    }

    #[test]
    fn test_data_keywords_are_not_walked() {
        // enum / default 里的值是数据：即使长得像 schema 也不能被改写
        let schema = json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["format", "uniqueItems"], "default": "format"},
                "cfg": {"type": "object", "properties": {}, "default": {"format": "uri"}}
            }
        });
        let out = normalize(&schema, &OPENAI_COMPATIBLE);
        assert_eq!(out["properties"]["kind"]["enum"], json!(["format", "uniqueItems"]));
        assert_eq!(out["properties"]["cfg"]["default"], json!({"format": "uri"}));
    }
}
