//! search 工具：BM25 符号检索（混合检索的轻量实现）
//!
//! 不需要精确正则时用关键词找相关符号：对项目符号表做 BM25 排序
//! （标识符按 snake_case/camelCase 切词），返回 `file:L 起-止` 锚点
//! 与声明行——直接衔接 read 的 offset/limit 精读（JIT disclosure 入口）。
//! 文件名也参与索引（文件级文档），实现「符号 + 文件」混合召回。

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use serde_json::Value;
use std::sync::Arc;

use crate::env::ExecutionEnv;
use crate::index::CodeIndex;
use crate::signatures::Symbol;

/// 返回结果数上限
const MAX_RESULTS: usize = 20;

pub struct SearchTool {
    env: Arc<ExecutionEnv>,
}

impl SearchTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self { env }
    }
}

/// BM25 文档
struct Doc {
    /// 展示锚点：`path:L 起-止`
    anchor: String,
    /// 展示行（声明或文件名）
    text: String,
    tokens: Vec<String>,
}

/// 标识符切词：camelCase / PascalCase / snake_case / kebab-case 拆分 + 小写化，
/// 支持缩写词边界（HTTPServer → http + server）
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;

    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && prev_lower && !current.is_empty() {
                // camelCase 边界：lower → Upper
                tokens.push(current.to_ascii_lowercase());
                current = String::new();
            } else if ch.is_ascii_lowercase()
                && current.len() >= 2
                && current.chars().last().is_some_and(|c| c.is_ascii_uppercase())
                && current[..current.chars().count() - 1]
                    .chars()
                    .all(|c| c.is_ascii_uppercase())
            {
                // 缩写词边界：HTTP + Server（末位大写归属新词）
                let chars: Vec<char> = current.chars().collect();
                let (head, tail) = chars.split_at(chars.len() - 1);
                tokens.push(head.iter().collect::<String>().to_ascii_lowercase());
                current = tail.iter().collect();
            }
            current.push(ch);
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !current.is_empty() {
                tokens.push(current.to_ascii_lowercase());
                current = String::new();
            }
            prev_lower = false;
        }
    }
    if !current.is_empty() {
        tokens.push(current.to_ascii_lowercase());
    }
    tokens
}

/// BM25 打分排序（k1=1.2, b=0.75），返回按相关度降序的前 K 个文档
fn bm25_rank<'a>(query_tokens: &[String], docs: &'a [Doc], top_k: usize) -> Vec<(f64, &'a Doc)> {
    let n = docs.len() as f64;
    let avgdl = if docs.is_empty() {
        1.0
    } else {
        docs.iter().map(|d| d.tokens.len()).sum::<usize>() as f64 / n
    };
    // 文档频率
    let mut df: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for doc in docs {
        let mut seen = std::collections::HashSet::new();
        for t in &doc.tokens {
            seen.insert(t.as_str());
        }
        for t in seen {
            *df.entry(t).or_insert(0) += 1;
        }
    }

    let k1 = 1.2;
    let b = 0.75;
    let mut scored: Vec<(f64, &Doc)> = docs
        .iter()
        .map(|doc| {
            let dl = doc.tokens.len() as f64;
            let mut score = 0.0;
            for q in query_tokens {
                let df_q = *df.get(q.as_str()).unwrap_or(&0) as f64;
                if df_q == 0.0 {
                    continue;
                }
                let tf = doc
                    .tokens
                    .iter()
                    .filter(|t| t.as_str() == q.as_str())
                    .count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let idf = ((n - df_q + 0.5) / (df_q + 0.5) + 1.0).ln();
                score += idf * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b + b * dl / avgdl));
            }
            (score, doc)
        })
        .filter(|(s, _)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b_| b_.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);
    scored
}

/// 构建检索文档集：符号文档 + 文件名文档
fn build_docs(index: &CodeIndex) -> Vec<Doc> {
    let mut docs = Vec::new();
    for entry in &index.entries {
        let rel = entry.path.display().to_string();
        for symbol in &entry.symbols {
            let text = symbol_display(symbol);
            let mut tokens = tokenize(&symbol.name);
            tokens.extend(tokenize(&symbol.kind));
            tokens.extend(tokenize(&symbol.signature));
            docs.push(Doc {
                anchor: format!("{}:L{}-{}", rel, symbol.line_start, symbol.line_end),
                text,
                tokens,
            });
        }
        // 文件名文档（让“按名字找文件”也可召回）
        let name = entry
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        docs.push(Doc {
            anchor: rel.clone(),
            text: format!("(file) {rel}"),
            tokens: tokenize(&name),
        });
    }
    docs
}

fn symbol_display(symbol: &Symbol) -> String {
    if symbol.name.is_empty() {
        symbol.signature.clone()
    } else {
        format!("{} {} — {}", symbol.kind, symbol.name, symbol.signature)
    }
}

#[async_trait]
impl AgentTool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "Keyword search over the project's symbol index (BM25-ranked, no exact regex needed). \
         Returns symbol anchors like 'src/foo.rs:L12-40' with the declaration line — \
         read the spans with offset/limit. Use grep for exact pattern matching; \
         use this to find relevant code by keywords."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Space-separated keywords (identifiers are split on camelCase/snake_case)"},
                "path": {"type": "string", "description": "Directory to index (default '.')"},
                "limit": {"type": "integer", "description": "Max results (default 20)"}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(query) = args["query"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'query'"));
        };
        let path = args["path"].as_str().unwrap_or(".");
        let limit = args["limit"].as_u64().unwrap_or(MAX_RESULTS as u64) as usize;

        let root = match self.env.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("[Policy denied] {e}"))),
        };

        let env = self.env.clone();
        let query_text = query.to_string();
        let (lines, matched, total_symbols, truncated) = tokio::task::spawn_blocking(move || {
            let index = CodeIndex::build(&root, &env);
            let total_symbols = index.entries.iter().map(|e| e.symbols.len()).sum::<usize>();
            let docs = build_docs(&index);
            let query_tokens = tokenize(&query_text);
            let ranked = bm25_rank(&query_tokens, &docs, limit.min(MAX_RESULTS));
            let lines: Vec<String> = ranked
                .iter()
                .map(|(score, doc)| format!("{:.2}  {}  {}", score, doc.anchor, doc.text))
                .collect();
            (lines, ranked.len(), total_symbols, index.truncated)
        })
        .await
        .map_err(|e| anyhow::anyhow!("search task failed: {e}"))?;

        let partial = if truncated {
            " — INDEX INCOMPLETE: file limit reached, narrow 'path'"
        } else {
            ""
        };
        if lines.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "No symbols matched '{query}' ({total_symbols} symbols indexed{partial}). Try different keywords or grep."
            )));
        }
        let header = format!(
            "== {matched} matches for '{query}' ({total_symbols} symbols indexed{partial}) =="
        );
        Ok(ToolOutput::ok(self.env.truncate_output(
            &std::iter::once(header).chain(lines).collect::<Vec<_>>().join("\n"),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_identifier_splitting() {
        assert_eq!(
            tokenize("AgentRuntime"),
            vec!["agent", "runtime"]
        );
        assert_eq!(
            tokenize("chat_stream"),
            vec!["chat", "stream"]
        );
        assert_eq!(
            tokenize("HTTPServer2"),
            vec!["http", "server2"]
        );
        assert_eq!(tokenize("a-b c.d"), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn test_bm25_ranks_relevant_doc_first() {
        let docs = vec![
            Doc {
                anchor: "a.rs:L1-10".into(),
                text: "fn parse_config".into(),
                tokens: tokenize("fn parse_config Config parse json parser"),
            },
            Doc {
                anchor: "b.rs:L1-5".into(),
                text: "fn unrelated".into(),
                tokens: tokenize("fn unrelated totally different topic widgets"),
            },
            Doc {
                anchor: "c.rs:L1-8".into(),
                text: "struct Config".into(),
                tokens: tokenize("struct Config settings app"),
            },
        ];
        let query = tokenize("parse config");
        let ranked = bm25_rank(&query, &docs, 10);
        assert!(!ranked.is_empty());
        // 同时命中 parse + config 的文档排最前
        assert_eq!(ranked[0].1.anchor, "a.rs:L1-10");
        // 无关文档被过滤
        assert!(ranked.iter().all(|(_, d)| d.anchor != "b.rs:L1-5"));
        // 分数降序
        assert!(ranked[0].0 >= ranked[1].0);
    }

    #[tokio::test]
    async fn test_search_tool_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/parser.rs"),
            "pub fn parse_config(raw: &str) -> Config {\n    todo!()\n}\n\npub struct Config { x: u32 }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/widgets.rs"),
            "pub fn render_button() {}\npub fn render_slider() {}\n",
        )
        .unwrap();

        let tool = SearchTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"query": "parse config"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("parse_config"), "{}", out.content);
        assert!(out.content.contains("parser.rs:L1-3"), "{}", out.content);
        assert!(!out.content.contains("render_button"));

        // 无命中
        let out = tool
            .execute(serde_json::json!({"query": "zzz_nonexistent"}))
            .await
            .unwrap();
        assert!(out.content.contains("No symbols matched"));
    }
}
