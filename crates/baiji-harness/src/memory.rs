//! 跨会话项目记忆（知识时间窗）
//!
//! 每个项目（按工作目录哈希）一个 JSONL 文件，存放 LLM 在工作中
//! 学到的持久事实/决策（lean-ctx session memory 的简化版）：
//! - 条目带 `valid_until` 有效期窗口，过期自动淘汰（不用显式清理）
//! - 有效条目注入系统提示（`## Project memory` 段）
//! - LLM 通过 `memory` 工具读写（add/list/forget）
//!
//! 存储：`~/.baiji/memory/<project-key>.jsonl`

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// 注入系统提示的最大条目数
const MAX_PROMPT_ENTRIES: usize = 20;
/// 单条事实在提示中的最大字符数
const MAX_FACT_CHARS: usize = 200;

/// 记忆类别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    /// 事实（"这个项目用 pnpm 不用 npm"）
    Fact,
    /// 决策（"选择了 JSONL 而非 SQLite 做持久化"）
    Decision,
    /// 用户偏好（"回答用中文"）
    Preference,
    /// 踩坑记录（"测试前必须先 cargo fmt，否则 CI 挂"）
    Gotcha,
}

impl MemoryKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "fact" => Some(Self::Fact),
            "decision" => Some(Self::Decision),
            "preference" => Some(Self::Preference),
            "gotcha" => Some(Self::Gotcha),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Decision => "decision",
            Self::Preference => "preference",
            Self::Gotcha => "gotcha",
        }
    }
}

/// 一条记忆（JSONL 记录）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub fact: String,
    pub kind: MemoryKind,
    /// 学到时间（RFC 3339）
    pub learned_at: String,
    /// 有效期至（None = 永久）；过期条目不再注入且加载时淘汰
    pub valid_until: Option<String>,
    /// 来源（"agent" / "session:<id>"）
    pub source: String,
}

impl MemoryEntry {
    fn is_active(&self, now: &chrono::DateTime<chrono::Utc>) -> bool {
        self.valid_until
            .as_deref()
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|until| until.with_timezone(&chrono::Utc) > *now)
            .unwrap_or(true)
    }
}

/// 项目记忆存储
#[derive(Debug, Clone)]
pub struct MemoryStore {
    dir: PathBuf,
}

impl MemoryStore {
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).ok();
        Self { dir }
    }

    fn file(&self, project: &str) -> PathBuf {
        let safe: String = project
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        self.dir.join(format!("{safe}.jsonl"))
    }

    fn load_raw(&self, project: &str) -> Vec<MemoryEntry> {
        let Ok(content) = std::fs::read_to_string(self.file(project)) else {
            return Vec::new();
        };
        content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn write_all(&self, project: &str, entries: &[MemoryEntry]) -> Result<()> {
        let path = self.file(project);
        let content = entries
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, if content.is_empty() { String::new() } else { format!("{content}\n") })?;
        Ok(())
    }

    /// 追加一条记忆。`ttl_days` 为有效期（None = 永久）。
    pub fn add(
        &self,
        project: &str,
        fact: impl Into<String>,
        kind: MemoryKind,
        ttl_days: Option<u64>,
        source: impl Into<String>,
    ) -> Result<MemoryEntry> {
        let now = chrono::Utc::now();
        let entry = MemoryEntry {
            fact: fact.into(),
            kind,
            learned_at: now.to_rfc3339(),
            valid_until: ttl_days
                .map(|d| (now + chrono::Duration::days(d as i64)).to_rfc3339()),
            source: source.into(),
        };
        // 去重：同文本事实不重复堆积（刷新时间窗）
        let mut entries = self.active(project);
        entries.retain(|e| e.fact != entry.fact);
        entries.push(entry.clone());
        // 简单容量上限：最多保留 200 条（新在前裁掉旧的）
        if entries.len() > 200 {
            let split = entries.len() - 200;
            entries.drain(..split);
        }
        self.write_all(project, &entries)?;
        Ok(entry)
    }

    /// 有效记忆（过期条目顺带从存储中淘汰）
    pub fn active(&self, project: &str) -> Vec<MemoryEntry> {
        let now = chrono::Utc::now();
        let raw = self.load_raw(project);
        let active: Vec<MemoryEntry> = raw.into_iter().filter(|e| e.is_active(&now)).collect();
        // 惰性清理：有过期条目时重写文件
        if active.len() != self.load_raw(project).len() {
            let _ = self.write_all(project, &active);
        }
        active
    }

    /// 删除匹配子串的记忆，返回删除数
    pub fn forget(&self, project: &str, pattern: &str) -> Result<usize> {
        let now = chrono::Utc::now();
        let raw = self.load_raw(project);
        let mut kept = Vec::new();
        let mut removed = 0;
        for entry in raw {
            if entry.is_active(&now) && entry.fact.contains(pattern) {
                removed += 1;
            } else {
                kept.push(entry);
            }
        }
        if removed > 0 {
            self.write_all(project, &kept)?;
        }
        Ok(removed)
    }
}

/// 项目键：目录名 + 规范路径哈希前 8 位（可读且唯一）
pub fn project_key(workdir: &Path) -> String {
    let canonical = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let hash: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    format!("{name}-{hash}")
}

/// `memory` 工具：LLM 读写跨会话项目记忆
pub struct MemoryTool {
    store: std::sync::Arc<MemoryStore>,
    project: String,
}

impl MemoryTool {
    pub fn new(store: std::sync::Arc<MemoryStore>, project: impl Into<String>) -> Self {
        Self {
            store,
            project: project.into(),
        }
    }
}

#[async_trait::async_trait]
impl baiji_agent::AgentTool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Persist and recall durable project knowledge across sessions. \
         Actions: 'add' (fact + optional kind: fact/decision/preference/gotcha, \
         optional ttl_days for time-bounded facts), 'list' (active entries), \
         'forget' (remove entries matching a substring pattern). \
         Use for: build commands, conventions, decisions, pitfalls — not transient state."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "list", "forget"]},
                "fact": {"type": "string", "description": "The knowledge to persist (action=add)"},
                "kind": {"type": "string", "enum": ["fact", "decision", "preference", "gotcha"]},
                "ttl_days": {"type": "integer", "description": "Validity window in days (optional; omit = permanent)"},
                "pattern": {"type": "string", "description": "Substring to match for removal (action=forget)"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<baiji_agent::ToolOutput> {
        use baiji_agent::ToolOutput;
        let action = args["action"].as_str().unwrap_or_default();
        match action {
            "add" => {
                let Some(fact) = args["fact"].as_str().map(str::trim).filter(|f| !f.is_empty())
                else {
                    return Ok(ToolOutput::err("[Error] action=add requires a non-empty 'fact'"));
                };
                let kind = args["kind"]
                    .as_str()
                    .and_then(MemoryKind::parse)
                    .unwrap_or(MemoryKind::Fact);
                let ttl = args["ttl_days"].as_u64();
                let entry = self
                    .store
                    .add(&self.project, fact, kind, ttl, "agent")?;
                Ok(ToolOutput::ok(format!(
                    "Remembered [{}] {}{}",
                    entry.kind.as_str(),
                    entry.fact,
                    ttl.map(|d| format!(" (valid {d}d")).unwrap_or_default()
                )))
            }
            "list" => {
                let active = self.store.active(&self.project);
                if active.is_empty() {
                    return Ok(ToolOutput::ok("(no active memories for this project)"));
                }
                let lines: Vec<String> = active
                    .iter()
                    .map(|e| format!("[{}] {} (learned {})", e.kind.as_str(), e.fact, e.learned_at))
                    .collect();
                Ok(ToolOutput::ok(lines.join("\n")))
            }
            "forget" => {
                let Some(pattern) = args["pattern"].as_str().map(str::trim).filter(|p| !p.is_empty())
                else {
                    return Ok(ToolOutput::err("[Error] action=forget requires a non-empty 'pattern'"));
                };
                match self.store.forget(&self.project, pattern)? {
                    0 => Ok(ToolOutput::ok(format!("No memories matched '{pattern}'"))),
                    n => {
                        let plural = if n == 1 { "entry" } else { "entries" };
                        Ok(ToolOutput::ok(format!(
                            "Forgot {n} memory {plural} matching '{pattern}'"
                        )))
                    }
                }
            }
            other => Ok(ToolOutput::err(format!(
                "[Error] unknown action '{other}' (expected add/list/forget)"
            ))),
        }
    }
}

/// 生成注入系统提示的记忆段
pub fn memory_section(entries: &[MemoryEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let now = chrono::Utc::now();
    let lines: Vec<String> = entries
        .iter()
        .take(MAX_PROMPT_ENTRIES)
        .map(|e| {
            let age = chrono::DateTime::parse_from_rfc3339(&e.learned_at)
                .map(|t| {
                    let days = (now - t.with_timezone(&chrono::Utc)).num_days();
                    if days == 0 {
                        "today".to_string()
                    } else {
                        format!("{days}d ago")
                    }
                })
                .unwrap_or_else(|_| "?".to_string());
            let fact: String = e
                .fact
                .chars()
                .take(MAX_FACT_CHARS)
                .collect::<String>()
                + if e.fact.chars().count() > MAX_FACT_CHARS {
                    "…"
                } else {
                    ""
                };
            format!("- [{} {}] {fact}", e.kind.as_str(), age)
        })
        .collect();
    Some(format!(
        "## Project memory\nDurable facts learned in previous sessions (use the memory tool to add/forget):\n{}",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use baiji_agent::AgentTool as _;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_memory_tool_actions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path().join("mem")));
        let tool = MemoryTool::new(store.clone(), "proj");

        // add（默认 fact）
        let out = tool
            .execute(serde_json::json!({"action": "add", "fact": "uses pnpm", "kind": "gotcha"}))
            .await
            .unwrap();
        assert!(out.content.contains("uses pnpm"));
        assert!(out.content.contains("gotcha"));

        // add 带 TTL
        let out = tool
            .execute(serde_json::json!({"action": "add", "fact": "on branch foo", "ttl_days": 0}))
            .await
            .unwrap();
        assert!(out.content.contains("valid 0d"));

        // list（过期条目不出现）
        let out = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(out.content.contains("uses pnpm"));
        assert!(!out.content.contains("branch foo"));

        // forget
        let out = tool
            .execute(serde_json::json!({"action": "forget", "pattern": "pnpm"}))
            .await
            .unwrap();
        assert_eq!(out.content, "Forgot 1 memory entry matching 'pnpm'");
        let out = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert_eq!(out.content, "(no active memories for this project)");

        // 参数错误
        let out = tool
            .execute(serde_json::json!({"action": "add"}))
            .await
            .unwrap();
        assert!(out.is_error);
        let out = tool
            .execute(serde_json::json!({"action": "bogus"}))
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn test_project_key_stable_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let a = project_key(dir.path());
        let b = project_key(dir.path());
        assert_eq!(a, b, "same dir → same key");
        assert!(a.starts_with(dir.path().file_name().unwrap().to_string_lossy().as_ref()));
        assert!(a.contains('-'));

        let other = tempfile::tempdir().unwrap();
        assert_ne!(a, project_key(other.path()));
    }

    #[test]
    fn test_add_active_forget_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path().join("mem"));

        store
            .add("proj", "uses pnpm not npm", MemoryKind::Fact, None, "agent")
            .unwrap();
        store
            .add("proj", "answers in Chinese", MemoryKind::Preference, None, "agent")
            .unwrap();
        // TTL 0 天：立即过期
        store
            .add("proj", "branch foo is checked out", MemoryKind::Fact, Some(0), "agent")
            .unwrap();

        let active = store.active("proj");
        assert_eq!(active.len(), 2, "expired entry dropped");
        assert!(active.iter().any(|e| e.fact.contains("pnpm")));

        // 项目隔离
        assert!(store.active("other").is_empty());

        // 同文本去重（刷新）
        store
            .add("proj", "uses pnpm not npm", MemoryKind::Fact, None, "agent")
            .unwrap();
        assert_eq!(store.active("proj").len(), 2);

        // forget 子串匹配
        assert_eq!(store.forget("proj", "pnpm").unwrap(), 1);
        assert_eq!(store.active("proj").len(), 1);
        assert_eq!(store.forget("proj", "nothing-matches").unwrap(), 0);
    }

    #[test]
    fn test_memory_section_rendering() {
        assert!(memory_section(&[]).is_none());

        let entries = vec![MemoryEntry {
            fact: "构建用 cargo build --release".to_string(),
            kind: MemoryKind::Gotcha,
            learned_at: chrono::Utc::now().to_rfc3339(),
            valid_until: None,
            source: "agent".to_string(),
        }];
        let section = memory_section(&entries).unwrap();
        assert!(section.contains("## Project memory"));
        assert!(section.contains("构建用 cargo build --release"));
        assert!(section.contains("today"));

        // 超长事实截断
        let long = MemoryEntry {
            fact: "x".repeat(500),
            kind: MemoryKind::Fact,
            learned_at: chrono::Utc::now().to_rfc3339(),
            valid_until: None,
            source: "agent".to_string(),
        };
        let section = memory_section(&[long]).unwrap();
        assert!(section.contains('…'));
        assert!(section.chars().count() < 400);
    }
}
