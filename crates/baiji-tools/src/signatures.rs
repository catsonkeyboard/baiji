//! 符号大纲提取（tree-sitter AST 优先，正则回退）
//!
//! 五种语言（Rust / Python / JavaScript / TypeScript / Go）用 AST
//! 提取精确符号与**行区间**（lean-ctx signatures 的 span 语义）：
//! `fn parse  L120-180`——LLM 可直接按区间 `read offset=120 limit=61`。
//! 其它语言回退到正则声明行匹配（仅行号，无区间）。

use serde_json::Value;

/// 一个符号
#[derive(Debug, Clone, PartialEq)]
pub struct Symbol {
    pub name: String,
    /// fn / struct / enum / trait / impl / mod / class / interface / type / method …
    pub kind: String,
    /// 1-based 起始行
    pub line_start: usize,
    /// 1-based 结束行（含）
    pub line_end: usize,
    /// 展示行（声明首行，截断到 120 字符）
    pub signature: String,
}

/// 支持的语言
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    TypeScriptTsx,
    Go,
}

impl Lang {
    /// 按扩展名探测
    pub fn detect(path: &std::path::Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::TypeScriptTsx),
            "go" => Some(Self::Go),
            _ => None,
        }
    }
}

/// 提取符号大纲：AST 语言走 tree-sitter，产出为空时回退正则。
pub fn outline(content: &str, lang: Option<Lang>) -> Vec<Symbol> {
    match lang {
        Some(lang) => {
            let symbols = ast_outline(content, lang);
            if symbols.is_empty() {
                regex_outline(content)
            } else {
                symbols
            }
        }
        None => regex_outline(content),
    }
}

// ========== tree-sitter AST 提取 ==========

/// 单文件符号数上限（防超大文件爆炸）
const MAX_SYMBOLS: usize = 500;

fn ast_outline(content: &str, lang: Lang) -> Vec<Symbol> {
    let mut parser = tree_sitter::Parser::new();
    let language: tree_sitter::Language = match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::TypeScriptTsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
    };
    if parser.set_language(&language).is_err() {
        return regex_outline(content);
    }
    let Some(tree) = parser.parse(content, None) else {
        return regex_outline(content);
    };

    let mut symbols = Vec::new();
    walk_symbols(&mut tree.root_node().walk(), content, lang, &mut symbols);
    symbols
}

/// 深度优先遍历，收集命中 kind 表的节点（含 impl/class 内部成员）
fn walk_symbols(
    cursor: &mut tree_sitter::TreeCursor,
    content: &str,
    lang: Lang,
    out: &mut Vec<Symbol>,
) {
    loop {
        if out.len() >= MAX_SYMBOLS {
            return;
        }
        let node = cursor.node();
        if let Some((kind, name)) = symbol_of(node, lang, content) {
            let line_start = node.start_position().row + 1; // 0-based → 1-based
            let line_end = node.end_position().row + 1;
            let signature = content[node.start_byte()..node.end_byte()]
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(120)
                .collect();
            out.push(Symbol {
                name,
                kind: kind.to_string(),
                line_start,
                line_end,
                signature,
            });
        }
        if cursor.goto_first_child() {
            walk_symbols(cursor, content, lang, out);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// 判定节点是否为目标符号；返回 (kind, name)
fn symbol_of(node: tree_sitter::Node, lang: Lang, content: &str) -> Option<(&'static str, String)> {
    let kind = node.kind();
    let take = |label: &'static str| {
        let name = node
            .child_by_field_name("name")?
            .utf8_text(content.as_bytes())
            .ok()?
            .to_string();
        Some((label, name))
    };

    match lang {
        Lang::Rust => match kind {
            "function_item" => take("fn"),
            "struct_item" => take("struct"),
            "enum_item" => take("enum"),
            "trait_item" => take("trait"),
            "impl_item" => {
                let name = node
                    .child_by_field_name("type")?
                    .utf8_text(content.as_bytes())
                    .ok()?
                    .to_string();
                Some(("impl", name))
            }
            "mod_item" => take("mod"),
            "const_item" => take("const"),
            "macro_definition" => take("macro"),
            _ => None,
        },
        Lang::Python => match kind {
            "function_definition" => take("def"),
            "class_definition" => take("class"),
            _ => None,
        },
        Lang::JavaScript | Lang::TypeScript | Lang::TypeScriptTsx => match kind {
            "function_declaration" => take("fn"),
            "class_declaration" | "abstract_class_declaration" => take("class"),
            "method_definition" => take("method"),
            "interface_declaration" => take("interface"),
            "type_alias_declaration" => take("type"),
            _ => None,
        },
        Lang::Go => match kind {
            "function_declaration" => take("fn"),
            "method_declaration" => take("method"),
            // type_declaration → type_spec（name 字段在 type_spec 上）
            "type_declaration" => {
                let mut found = None;
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "type_spec" {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            if let Ok(text) = name_node.utf8_text(content.as_bytes()) {
                                found = Some(text.to_string());
                            }
                        }
                    }
                }
                found.map(|name| ("type", name))
            }
            _ => None,
        },
    }
}

// ========== 正则回退（无 AST 语言的声明行匹配）==========

pub(crate) fn regex_outline(content: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if crate::tools::read::is_signature_line(line) {
            out.push(Symbol {
                name: String::new(),
                kind: "decl".to_string(),
                line_start: i + 1,
                line_end: i + 1,
                signature: line.trim().chars().take(120).collect(),
            });
            if out.len() >= MAX_SYMBOLS {
                break;
            }
        }
    }
    out
}

/// 符号转 JSON（工具参数/输出用）
pub fn symbol_to_value(symbol: &Symbol) -> Value {
    serde_json::json!({
        "name": symbol.name,
        "kind": symbol.kind,
        "line_start": symbol.line_start,
        "line_end": symbol.line_end,
        "signature": symbol.signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lang_detect() {
        assert_eq!(Lang::detect(std::path::Path::new("a.rs")), Some(Lang::Rust));
        assert_eq!(Lang::detect(std::path::Path::new("a.ts")), Some(Lang::TypeScript));
        assert_eq!(Lang::detect(std::path::Path::new("a.tsx")), Some(Lang::TypeScriptTsx));
        assert_eq!(Lang::detect(std::path::Path::new("a.py")), Some(Lang::Python));
        assert_eq!(Lang::detect(std::path::Path::new("a.go")), Some(Lang::Go));
        assert_eq!(Lang::detect(std::path::Path::new("a.rb")), None);
        assert_eq!(Lang::detect(std::path::Path::new("noext")), None);
    }

    #[test]
    fn test_ast_outline_rust_with_spans() {
        let code = "\
// comment
fn helper() {}

pub struct Config {
    x: u32,
}

impl Config {
    pub fn new() -> Self {
        Self { x: 1 }
    }
}

async fn main() {
    helper();
}
";
        let symbols = ast_outline(code, Lang::Rust);
        let view: Vec<(&str, &str, usize, usize)> = symbols
            .iter()
            .map(|s| (s.kind.as_str(), s.name.as_str(), s.line_start, s.line_end))
            .collect();

        assert_eq!(view[0], ("fn", "helper", 2, 2));
        assert_eq!(view[1], ("struct", "Config", 4, 6));
        // impl 块带完整区间，内部方法也被提取
        let impl_sym = view.iter().find(|(k, n, _, _)| *k == "impl" && *n == "Config").unwrap();
        assert_eq!(impl_sym.2, 8);
        let method = view.iter().find(|(k, n, _, _)| *k == "fn" && *n == "new").unwrap();
        assert_eq!((method.2, method.3), (9, 11), "method spans its whole body");
        // async fn 也是 function_item
        assert!(view.iter().any(|(k, n, _, _)| *k == "fn" && *n == "main"));
    }

    #[test]
    fn test_ast_outline_python_and_go() {
        let py = "class Service:\n    def run(self):\n        pass\n\ndef top():\n    return 1\n";
        let symbols = ast_outline(py, Lang::Python);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Service"));
        assert!(names.contains(&"run"));
        assert!(names.contains(&"top"));

        let go = "package main\n\nfunc main() {\n\tfmt.Println(1)\n}\n\ntype Server struct {\n\tPort int\n}\n";
        let symbols = ast_outline(go, Lang::Go);
        assert!(symbols.iter().any(|s| s.name == "main" && s.kind == "fn"));
        assert!(symbols.iter().any(|s| s.name == "Server" && s.kind == "type"));
    }

    #[test]
    fn test_ast_outline_typescript() {
        let ts = "interface User {\n  id: number;\n}\n\nexport function handler(u: User) {\n  return u.id;\n}\n\nclassSvc {}\n";
        let symbols = ast_outline(ts, Lang::TypeScript);
        assert!(symbols.iter().any(|s| s.name == "User" && s.kind == "interface"));
        assert!(symbols.iter().any(|s| s.name == "handler" && s.kind == "fn"));
    }

    #[test]
    fn test_outline_falls_back_for_unknown_lang() {
        let code = "def load():\n    pass\n";
        let symbols = outline(code, None);
        assert!(!symbols.is_empty());
        assert_eq!(symbols[0].line_start, 1);
    }

    #[test]
    fn test_ast_broken_code_falls_back() {
        let code = "fn broken( {\nfn still_visible() {}\n";
        let symbols = outline(code, Some(Lang::Rust));
        assert!(!symbols.is_empty());
    }
}
