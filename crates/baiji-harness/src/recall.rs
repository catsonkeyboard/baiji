//! recall 工具：检索历史会话 / 取回原始对话
//!
//! 会话 JSONL 只追加、保留全部原文（压缩只改内存视图），所以这里能找回：
//! - 过去会话里讨论过的内容（默认限定当前项目）
//! - 当前会话里已被摘要压缩掉的原始轮次（`session: "current"`）
//!
//! 两种模式：
//! - 搜索：`query` → 关键词打分（短语命中、词覆盖率、命中密度），每个会话最多 2 条
//! - 读取：`session` + `around` 或 `start`/`end` → 返回这些消息的原文（截断）
//!
//! 不建索引，直接按修改时间倒序扫描（上限 [`MAX_SESSION_FILES`] 个文件），先做字节级预过滤再解析。

use crate::persist::Record;
use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use baiji_ai::{Message, Role};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// 搜索最多扫描的会话文件数（按修改时间倒序）
pub const MAX_SESSION_FILES: usize = 300;
const SNIPPET_CHARS: usize = 240;
const READ_MESSAGE_CHARS: usize = 1500;
const READ_TOOL_RESULT_CHARS: usize = 300;
const MAX_PER_SESSION: usize = 2;

/// 当前会话 id 的共享句柄（harness 每次 run 开始时写入）
pub type SessionCursor = Arc<RwLock<Option<String>>>;

#[derive(Clone)]
pub struct RecallTool {
    sessions_dir: PathBuf,
    project: Option<String>,
    current: SessionCursor,
}

impl RecallTool {
    pub fn new(sessions_dir: impl Into<PathBuf>, project: Option<String>, current: SessionCursor) -> Self {
        Self {
            sessions_dir: sessions_dir.into(),
            project,
            current,
        }
    }

    fn current_id(&self) -> Option<String> {
        self.current.read().ok().and_then(|g| g.clone())
    }
}

/// 一个会话文件的解析结果
struct SessionDoc {
    id: String,
    created_at: String,
    title: Option<String>,
    project: Option<String>,
    messages: Vec<Message>,
}

fn parse_session(path: &Path) -> Option<SessionDoc> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut doc = SessionDoc {
        id: path.file_stem()?.to_string_lossy().to_string(),
        created_at: String::new(),
        title: None,
        project: None,
        messages: Vec::new(),
    };
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        match record {
            Record::Started { meta } => {
                doc.created_at = meta.created_at;
                doc.title = meta.title.or(doc.title);
                doc.project = meta.project;
            }
            Record::Title { title } => doc.title = Some(title),
            Record::Message { message } => doc.messages.push(message),
            _ => {}
        }
    }
    Some(doc)
}

/// 会话文件按修改时间倒序
fn session_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .map(|p| {
            let mtime = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (mtime, p)
        })
        .collect();
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    files.into_iter().map(|(_, p)| p).collect()
}

const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "of", "and", "or", "to", "in", "on", "is", "are", "was", "for", "with",
    "it", "this", "that", "we", "how", "what", "did", "do", "be", "at", "by", "from",
];

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{3040}'..='\u{30FF}' | '\u{AC00}'..='\u{D7AF}')
}

/// 查询分词：ASCII 词（小写、≥2 字符、去停用词）+ CJK 二元组（孤字单独成词）
pub fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut push = |t: String| {
        if !terms.contains(&t) {
            terms.push(t);
        }
    };
    let mut word = String::new();
    let mut cjk_run: Vec<char> = Vec::new();
    let flush_cjk = |run: &mut Vec<char>, push: &mut dyn FnMut(String)| {
        match run.len() {
            0 => {}
            1 => push(run[0].to_string()),
            _ => {
                for pair in run.windows(2) {
                    push(pair.iter().collect());
                }
            }
        }
        run.clear();
    };
    for c in query.chars().chain(std::iter::once(' ')) {
        if is_cjk(c) {
            if !word.is_empty() {
                let w = std::mem::take(&mut word).to_lowercase();
                if w.chars().count() >= 2 && !STOP_WORDS.contains(&w.as_str()) {
                    push(w);
                }
            }
            cjk_run.push(c);
        } else {
            flush_cjk(&mut cjk_run, &mut push);
            if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' {
                word.push(c);
            } else if !word.is_empty() {
                let w = std::mem::take(&mut word)
                    .trim_matches(|c| c == '.' || c == '-')
                    .to_lowercase();
                if w.chars().count() >= 2 && !STOP_WORDS.contains(&w.as_str()) {
                    push(w);
                }
            }
        }
    }
    terms
}

/// 消息文本打分：None = 命中词数不够
fn score_text(text_lower: &str, terms: &[String], phrase: &str) -> Option<f64> {
    if terms.is_empty() {
        return None;
    }
    let matched: Vec<&String> = terms.iter().filter(|t| text_lower.contains(t.as_str())).collect();
    let needed = match terms.len() {
        1 => 1,
        2 | 3 => 2,
        n => n.div_ceil(2),
    };
    if matched.len() < needed {
        return None;
    }
    let coverage = matched.len() as f64 / terms.len() as f64;
    let occurrences: usize = matched
        .iter()
        .map(|t| text_lower.matches(t.as_str()).count().min(5))
        .sum();
    let density = occurrences as f64 / (text_lower.len() as f64 / 400.0 + 1.0);
    let phrase_bonus = if phrase.chars().count() > 3 && text_lower.contains(phrase) {
        4.0
    } else {
        0.0
    };
    Some(phrase_bonus + coverage * 3.0 + matched.len() as f64 * 0.25 + density.min(2.0))
}

/// 命中处附近的单行摘要
fn snippet(text: &str, terms: &[String]) -> String {
    let lower = text.to_lowercase();
    // to_lowercase 可能改变字节长度：按字符索引定位
    let first_char = terms
        .iter()
        .filter_map(|t| lower.find(t.as_str()))
        .min()
        .map(|byte| lower[..byte].chars().count())
        .unwrap_or(0);
    let chars: Vec<char> = text.chars().collect();
    let start = first_char.saturating_sub(SNIPPET_CHARS / 3);
    let end = (start + SNIPPET_CHARS).min(chars.len());
    let mut out: String = chars[start..end]
        .iter()
        .map(|c| if c.is_whitespace() { ' ' } else { *c })
        .collect();
    out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if start > 0 {
        out.insert(0, '…');
    }
    if end < chars.len() {
        out.push('…');
    }
    out
}

/// 消息的可检索文本（工具结果默认不参与：多为命令输出噪声）
fn searchable_text(message: &Message, include_tools: bool) -> Option<String> {
    match message.role {
        Role::User | Role::Assistant if !message.content.trim().is_empty() => {
            // 自动注入的提示（auto-continue 等）不当作对话内容
            (!message.content.starts_with("[system notice]")).then(|| message.content.clone())
        }
        Role::Tool if include_tools => Some(
            message
                .tool_results
                .iter()
                .flatten()
                .map(|r| r.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

fn role_label(role: &Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "system",
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}… [{} more chars]", s.chars().count() - max)
}

struct Hit {
    score: f64,
    rank: usize,
    session: String,
    created_at: String,
    title: Option<String>,
    index: usize,
    role: &'static str,
    snippet: String,
}

impl RecallTool {
    fn search(&self, query: &str, scope: &str, limit: usize, include_tools: bool) -> ToolOutput {
        let terms = query_terms(query);
        if terms.is_empty() {
            return ToolOutput::err("[Error] query has no searchable terms");
        }
        let phrase = query.trim().to_lowercase();
        let current = self.current_id();
        let mut hits: Vec<Hit> = Vec::new();
        let mut scanned = 0;
        for (rank, path) in session_files(&self.sessions_dir)
            .into_iter()
            .take(MAX_SESSION_FILES)
            .enumerate()
        {
            // 字节级预过滤：任一词都没出现就不解析
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let lower = String::from_utf8_lossy(&bytes).to_lowercase();
            if !terms.iter().any(|t| lower.contains(t.as_str())) {
                scanned += 1;
                continue;
            }
            let Some(doc) = parse_session(&path) else {
                continue;
            };
            let is_current = current.as_deref() == Some(doc.id.as_str());
            match scope {
                "current" if !is_current => continue,
                // 旧会话（无项目字段）在项目范围内也参与，避免遗漏
                "project" if doc.project.is_some() && doc.project != self.project => continue,
                _ => {}
            }
            scanned += 1;
            let mut session_hits: Vec<Hit> = doc
                .messages
                .iter()
                .enumerate()
                .filter_map(|(index, message)| {
                    let text = searchable_text(message, include_tools)?;
                    let score = score_text(&text.to_lowercase(), &terms, &phrase)?;
                    Some(Hit {
                        score,
                        rank,
                        session: doc.id.clone(),
                        created_at: doc.created_at.clone(),
                        title: doc.title.clone(),
                        index,
                        role: role_label(&message.role),
                        snippet: snippet(&text, &terms),
                    })
                })
                .collect();
            session_hits.sort_by(|a, b| b.score.total_cmp(&a.score));
            session_hits.truncate(MAX_PER_SESSION);
            hits.extend(session_hits);
        }
        // 相关度优先，同分时更新的会话在前
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.rank.cmp(&b.rank)));
        hits.truncate(limit);
        if hits.is_empty() {
            return ToolOutput::ok(format!(
                "No matches for \"{query}\" (scope: {scope}, {scanned} sessions searched)."
            ));
        }
        let mut out = format!(
            "{} match(es) for \"{query}\" (scope: {scope}, {scanned} sessions searched):\n",
            hits.len()
        );
        for (i, hit) in hits.iter().enumerate() {
            let date = hit.created_at.get(..10).unwrap_or(&hit.created_at);
            let mut label = format!("{date} · {}", hit.session);
            if current.as_deref() == Some(hit.session.as_str()) {
                label.push_str(" (current)");
            }
            if let Some(title) = &hit.title {
                label.push_str(&format!(" · \"{}\"", truncate_chars(title, 60)));
            }
            out.push_str(&format!(
                "{}. [{label}] #{} {}: {}\n",
                i + 1,
                hit.index,
                hit.role,
                hit.snippet
            ));
        }
        out.push_str("Read the surrounding turns with recall {session, around: <#>}.");
        ToolOutput::ok(out)
    }

    fn read(&self, session: &str, start: usize, end: usize) -> ToolOutput {
        let id = if session == "current" {
            match self.current_id() {
                Some(id) => id,
                None => return ToolOutput::err("[Error] no current session yet"),
            }
        } else {
            session.to_string()
        };
        let safe: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let path = self.sessions_dir.join(format!("{safe}.jsonl"));
        let Some(doc) = parse_session(&path) else {
            return ToolOutput::err(format!("[Error] session '{id}' not found"));
        };
        if doc.messages.is_empty() {
            return ToolOutput::ok(format!("Session {id} has no messages."));
        }
        let last = doc.messages.len() - 1;
        let (start, end) = (start.min(last), end.min(last));
        let mut out = format!(
            "Session {id}{} — messages #{start}..#{end} of {}:\n",
            doc.title
                .as_deref()
                .map(|t| format!(" \"{}\"", truncate_chars(t, 60)))
                .unwrap_or_default(),
            doc.messages.len()
        );
        for (index, message) in doc.messages.iter().enumerate().take(end + 1).skip(start) {
            out.push_str(&format!("\n#{index} {}:", role_label(&message.role)));
            if !message.content.trim().is_empty() {
                out.push_str(&format!(" {}", truncate_chars(message.content.trim(), READ_MESSAGE_CHARS)));
            }
            for call in message.tool_calls.iter().flatten() {
                out.push_str(&format!(
                    "\n  → {}({})",
                    call.name,
                    truncate_chars(&call.arguments.to_string(), 200)
                ));
            }
            for result in message.tool_results.iter().flatten() {
                out.push_str(&format!(
                    "\n  ⎿ {}",
                    truncate_chars(result.content.trim(), READ_TOOL_RESULT_CHARS)
                ));
            }
        }
        ToolOutput::ok(out)
    }
}

#[async_trait]
impl AgentTool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Search past conversations, or read back raw turns that context compaction summarized away. \
         Search: {query, scope?: project (default) | current | all, limit?}. \
         Read: {session: <id> | \"current\", around: <#>} or {session, start, end} using message \
         numbers from search results."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Keywords or a phrase to find"},
                "scope": {"type": "string", "enum": ["project", "current", "all"], "description": "Which sessions to search (default project)"},
                "limit": {"type": "integer", "description": "Max results (default 8, max 30)"},
                "include_tool_output": {"type": "boolean", "description": "Also search tool results (default false)"},
                "session": {"type": "string", "description": "Session id to read, or \"current\""},
                "around": {"type": "integer", "description": "Read ±3 messages around this message number"},
                "start": {"type": "integer", "description": "First message number to read"},
                "end": {"type": "integer", "description": "Last message number to read (max 40 messages per call)"}
            }
        })
    }

    fn parallel(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        if let Some(session) = args["session"].as_str().filter(|s| !s.trim().is_empty()) {
            let (start, end) = match (args["around"].as_u64(), args["start"].as_u64()) {
                (Some(around), _) => (around.saturating_sub(3), around + 3),
                (None, Some(start)) => {
                    let end = args["end"].as_u64().unwrap_or(start + 10);
                    (start, end.max(start))
                }
                (None, None) => (0, 10),
            };
            let end = end.min(start + 39);
            let me = self.clone();
            let session = session.to_string();
            return Ok(tokio::task::spawn_blocking(move || {
                me.read(&session, start as usize, end as usize)
            })
            .await
            .unwrap_or_else(|e| ToolOutput::err(format!("[Error] recall failed: {e}"))));
        }
        let Some(query) = args["query"].as_str().filter(|q| !q.trim().is_empty()) else {
            return Ok(ToolOutput::err(
                "[Error] pass either query (to search) or session (to read turns)",
            ));
        };
        let scope = match args["scope"].as_str() {
            Some(s @ ("project" | "current" | "all")) => s.to_string(),
            _ => "project".to_string(),
        };
        let limit = args["limit"].as_u64().unwrap_or(8).clamp(1, 30) as usize;
        let include_tools = args["include_tool_output"].as_bool().unwrap_or(false);
        let me = self.clone();
        let query = query.to_string();
        Ok(tokio::task::spawn_blocking(move || me.search(&query, &scope, limit, include_tools))
            .await
            .unwrap_or_else(|e| ToolOutput::err(format!("[Error] recall failed: {e}"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::JsonlStore;
    use crate::session::SessionMeta;

    fn write_session(store: &JsonlStore, id: &str, project: Option<&str>, msgs: &[(Role, &str)]) {
        store
            .append(
                id,
                &Record::Started {
                    meta: SessionMeta {
                        id: id.to_string(),
                        parent_id: None,
                        created_at: "2026-10-01T10:00:00+08:00".to_string(),
                        title: Some(format!("title {id}")),
                        project: project.map(str::to_string),
                    },
                },
            )
            .unwrap();
        for (role, text) in msgs {
            let message = match role {
                Role::User => Message::user(*text),
                _ => Message::assistant(*text),
            };
            store.append(id, &Record::Message { message }).unwrap();
        }
    }

    fn tool(dir: &Path, current: Option<&str>) -> RecallTool {
        RecallTool::new(
            dir,
            Some("proj".to_string()),
            Arc::new(RwLock::new(current.map(str::to_string))),
        )
    }

    #[test]
    fn test_query_terms_ascii_and_cjk() {
        assert_eq!(query_terms("How does the Retry-After header work?"), vec![
            "does", "retry-after", "header", "work"
        ]);
        assert_eq!(query_terms("压缩策略"), vec!["压缩", "缩策", "策略"]);
        assert_eq!(query_terms("JSONL 存储 a"), vec!["jsonl", "存储"]);
    }

    #[tokio::test]
    async fn test_search_scopes_rank_and_snippets() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path());
        write_session(&store, "s_old", Some("proj"), &[
            (Role::User, "we decided to use exponential backoff for retries"),
            (Role::Assistant, "Yes: retry with exponential backoff, capped at 30s."),
        ]);
        write_session(&store, "s_other", Some("elsewhere"), &[
            (Role::User, "exponential backoff in another project"),
        ]);
        write_session(&store, "s_now", Some("proj"), &[(Role::User, "unrelated chat")]);
        let tool = tool(dir.path(), Some("s_now"));

        let out = tool
            .execute(serde_json::json!({"query": "exponential backoff"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("s_old"), "{}", out.content);
        assert!(!out.content.contains("s_other"), "project scope: {}", out.content);
        assert!(out.content.contains("#0 user"), "{}", out.content);

        let out = tool
            .execute(serde_json::json!({"query": "exponential backoff", "scope": "all"}))
            .await
            .unwrap();
        assert!(out.content.contains("s_other"), "{}", out.content);

        let out = tool
            .execute(serde_json::json!({"query": "kubernetes"}))
            .await
            .unwrap();
        assert!(out.content.starts_with("No matches"), "{}", out.content);
    }

    #[tokio::test]
    async fn test_read_current_session_turns() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path());
        let msgs: Vec<(Role, String)> = (0..10)
            .map(|i| (if i % 2 == 0 { Role::User } else { Role::Assistant }, format!("message {i}")))
            .collect();
        let refs: Vec<(Role, &str)> = msgs.iter().map(|(r, t)| (*r, t.as_str())).collect();
        write_session(&store, "s_now", Some("proj"), &refs);
        let tool = tool(dir.path(), Some("s_now"));

        let out = tool
            .execute(serde_json::json!({"session": "current", "around": 5}))
            .await
            .unwrap();
        assert!(out.content.contains("#2 user: message 2"), "{}", out.content);
        assert!(out.content.contains("#8 user: message 8"), "{}", out.content);
        assert!(!out.content.contains("#9"), "{}", out.content);

        let out = tool
            .execute(serde_json::json!({"session": "s_now", "start": 8, "end": 50}))
            .await
            .unwrap();
        assert!(out.content.contains("#9 assistant: message 9"), "{}", out.content);

        let out = tool
            .execute(serde_json::json!({"session": "../../etc/passwd"}))
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn test_requires_query_or_session() {
        let dir = tempfile::tempdir().unwrap();
        let out = tool(dir.path(), None).execute(serde_json::json!({})).await.unwrap();
        assert!(out.is_error);
    }
}
