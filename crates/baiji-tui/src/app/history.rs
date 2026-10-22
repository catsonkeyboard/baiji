//! 提示词历史：提交过的输入持久化到 `~/.baiji/prompt-history.jsonl`，↑/↓ 跨会话召回。
//!
//! - 文件：追加写，每行一个 `{"text": ..., "ts": <unix 秒>}`；多进程并发追加互不覆盖，
//!   去重在加载/压实时进行（行数超过 2×上限时整体重写一次）
//! - 内存：旧 → 新，全量去重（重复提交把旧条目挪到最新位）；连续重复不再追加写盘
//! - 召回规则：输入为空、或正显示某条历史时，↑/↓ 才走历史；
//!   显式召回（Alt/Ctrl+↑）会把手头草稿暂存，↓ 越过最新一条时原样恢复
//!
//! 路径可注入：`load(path)` 读写指定文件；`in_memory()` 不落盘（测试与无家目录场景）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 内存/加载上限（去重后保留最新的这么多条）
pub(crate) const MAX_ENTRIES: usize = 1000;
/// 单条上限：超大粘贴不进历史（召回价值低、撑大文件）
pub(crate) const MAX_ENTRY_BYTES: usize = 10 * 1024;
/// 追加文件超过此行数时压实（去重 + 截断 + 原子重写）
const COMPACT_THRESHOLD_LINES: usize = MAX_ENTRIES * 2;

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    text: String,
    #[serde(default)]
    ts: u64,
}

#[derive(Debug, Default)]
pub(crate) struct PromptHistory {
    /// 旧 → 新，已去重
    entries: Vec<String>,
    /// None = 只在内存（不落盘）
    path: Option<PathBuf>,
    /// 显式召回前暂存的草稿（↓ 越过最新一条时恢复）
    draft: Option<String>,
}

impl PromptHistory {
    /// 纯内存历史（不读写文件）
    pub(crate) fn in_memory() -> Self {
        Self::default()
    }

    /// 从文件加载（不存在 = 空历史）；之后的记录追加写回同一文件
    pub(crate) fn load(path: PathBuf) -> Self {
        Self {
            entries: load_entries(&path),
            path: Some(path),
            draft: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[String] {
        &self.entries
    }

    /// 输入框当前内容是否正是某条历史（决定 ↑/↓ 是否走历史）
    pub(crate) fn contains(&self, text: &str) -> bool {
        !text.is_empty() && self.entries.iter().any(|e| e == text)
    }

    /// 记录一次提交（去首尾空白；空/超大跳过；连续重复不写盘）
    pub(crate) fn record(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() || text.len() > MAX_ENTRY_BYTES {
            return;
        }
        self.draft = None;
        if self.entries.last().is_some_and(|last| last == text) {
            return;
        }
        self.entries.retain(|e| e != text);
        self.entries.push(text.to_string());
        if self.entries.len() > MAX_ENTRIES {
            let overflow = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..overflow);
        }
        if let Some(path) = &self.path {
            append_entry(path, text);
        }
    }

    /// ↑：更旧的一条。`current` 为空 → 最新一条；`current` 是历史 → 它的上一条
    /// （已是最旧则停在原处）；否则仅 `explicit` 时暂存草稿并跳到最新一条
    pub(crate) fn older(&mut self, current: &str, explicit: bool) -> Option<String> {
        let newest = self.entries.last()?.clone();
        if current.is_empty() {
            self.draft = None;
            return Some(newest);
        }
        if let Some(i) = self.position(current) {
            return Some(self.entries[i.saturating_sub(1)].clone());
        }
        if explicit {
            self.draft = Some(current.to_string());
            return Some(newest);
        }
        None
    }

    /// ↓：更新的一条；越过最新一条时恢复草稿（无草稿 = 清空）。
    /// `current` 不是历史条目时返回 None（交给光标移动处理）
    pub(crate) fn newer(&mut self, current: &str) -> Option<String> {
        let i = self.position(current)?;
        if i + 1 < self.entries.len() {
            return Some(self.entries[i + 1].clone());
        }
        Some(self.draft.take().unwrap_or_default())
    }

    /// 草稿作废（Esc 清空 / 提交时）
    pub(crate) fn reset_draft(&mut self) {
        self.draft = None;
    }

    fn position(&self, text: &str) -> Option<usize> {
        if text.is_empty() {
            return None;
        }
        self.entries.iter().rposition(|e| e == text)
    }
}

/// 读取历史文件：坏行/空行/超大条目跳过；兼容裸 JSON 字符串行；去重保留最后一次；截断到上限
fn load_entries(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let entries: Vec<String> = content
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<Record>(line)
                .map(|r| r.text)
                .or_else(|_| serde_json::from_str::<String>(line))
                .ok()
        })
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty() && text.len() <= MAX_ENTRY_BYTES)
        .collect();
    let mut deduped = dedupe_keep_last(entries);
    if deduped.len() > MAX_ENTRIES {
        let overflow = deduped.len() - MAX_ENTRIES;
        deduped.drain(..overflow);
    }
    deduped
}

fn dedupe_keep_last(entries: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = entries
        .into_iter()
        .rev()
        .filter(|e| seen.insert(e.clone()))
        .collect();
    out.reverse();
    out
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn append_entry(path: &Path, text: &str) {
    use std::io::Write;
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!("prompt history: cannot create {}: {e}", parent.display());
        return;
    }
    let Ok(line) = serde_json::to_string(&Record {
        text: text.to_string(),
        ts: now_secs(),
    }) else {
        return;
    };
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(e) = appended {
        tracing::warn!("prompt history: append to {} failed: {e}", path.display());
        return;
    }
    maybe_compact(path);
}

/// 追加文件过长时重写为去重后的最新 MAX_ENTRIES 条（临时文件 + rename，原子替换）
fn maybe_compact(path: &Path) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    if content.lines().count() <= COMPACT_THRESHOLD_LINES {
        return;
    }
    let ts = now_secs();
    let mut out = String::new();
    for text in load_entries(path) {
        if let Ok(line) = serde_json::to_string(&Record { text, ts }) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    let tmp = path.with_extension("jsonl.tmp");
    if let Err(e) = std::fs::write(&tmp, out).and_then(|_| std::fs::rename(&tmp, path)) {
        tracing::warn!("prompt history: compaction failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_navigation_and_draft_restore() {
        let mut h = PromptHistory::in_memory();
        assert!(h.older("", false).is_none(), "empty history");
        h.record("first");
        h.record("second");
        h.record("third");

        // 空输入 ↑：最新 → 逐条更旧 → 最旧处停住
        let a = h.older("", false).unwrap();
        assert_eq!(a, "third");
        let b = h.older(&a, false).unwrap();
        assert_eq!(b, "second");
        let c = h.older(&b, false).unwrap();
        assert_eq!(c, "first");
        assert_eq!(h.older(&c, false).unwrap(), "first", "stays at the oldest");
        // ↓ 回到更新，越过最新恢复（空）草稿
        assert_eq!(h.newer(&c).unwrap(), "second");
        assert_eq!(h.newer("third").unwrap(), "");
        assert!(h.newer("").is_none(), "empty input is not an entry");

        // 非历史草稿：普通 ↑ 不召回；显式召回暂存草稿，↓ 越过最新时原样恢复
        assert!(h.older("my draft", false).is_none());
        assert_eq!(h.older("my draft", true).unwrap(), "third");
        assert_eq!(h.older("third", false).unwrap(), "second");
        assert_eq!(h.newer("second").unwrap(), "third");
        assert_eq!(h.newer("third").unwrap(), "my draft");
        // 草稿只恢复一次
        assert_eq!(h.newer("third").unwrap(), "");
        assert!(h.contains("second"));
        assert!(!h.contains("my draft"));
        assert!(!h.contains(""));
    }

    #[test]
    fn test_record_dedupe_and_limits() {
        let mut h = PromptHistory::in_memory();
        h.record("  a  ");
        h.record("a"); // 连续重复
        h.record("b");
        h.record("a"); // 非连续重复：挪到最新
        assert_eq!(h.entries(), ["b", "a"]);
        h.record("   ");
        h.record(&"x".repeat(MAX_ENTRY_BYTES + 1));
        assert_eq!(
            h.entries().len(),
            2,
            "blank and oversized entries are skipped"
        );
        for i in 0..MAX_ENTRIES + 5 {
            h.record(&format!("p{i}"));
        }
        assert_eq!(h.entries().len(), MAX_ENTRIES);
        assert_eq!(
            h.entries().last().unwrap(),
            &format!("p{}", MAX_ENTRIES + 4)
        );
    }

    #[test]
    fn test_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("prompt-history.jsonl");
        let mut h = PromptHistory::load(path.clone());
        assert!(h.entries().is_empty(), "missing file = empty history");
        h.record("fix the bug\nwith two lines");
        h.record("run tests");
        h.record("run tests"); // 连续重复不写盘
        h.record("fix the bug\nwith two lines");

        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            raw.lines().count(),
            3,
            "append-only, consecutive dupes skipped"
        );
        let first: serde_json::Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(first["text"], "fix the bug\nwith two lines");
        assert!(first["ts"].as_u64().unwrap() > 0);

        // 坏行 / 裸字符串 / 空文本混入：加载时容错
        std::fs::write(
            &path,
            format!("{raw}not json\n\"legacy string\"\n{{\"text\":\"  \"}}\n"),
        )
        .unwrap();
        let reloaded = PromptHistory::load(path.clone());
        assert_eq!(
            reloaded.entries(),
            ["run tests", "fix the bug\nwith two lines", "legacy string"],
            "deduped keeping the latest occurrence"
        );
    }

    #[test]
    fn test_compaction_caps_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt-history.jsonl");
        let lines: String = (0..COMPACT_THRESHOLD_LINES)
            .map(|i| format!("{{\"text\":\"p{}\",\"ts\":1}}\n", i % 10))
            .collect();
        std::fs::write(&path, lines).unwrap();
        let mut h = PromptHistory::load(path.clone());
        assert_eq!(h.entries().len(), 10);
        h.record("fresh");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            raw.lines().count(),
            11,
            "rewritten deduped once over the threshold"
        );
        assert!(raw.lines().last().unwrap().contains("fresh"));
    }
}
