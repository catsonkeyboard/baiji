//! JSONL 持久化
//!
//! 每个会话一个文件（`<id>.jsonl`），记录逐条追加：
//! - `Started`：会话元信息（首条）
//! - `Message`：对话消息（含工具调用/结果）
//! - `Summary`：上下文压缩标记
//! - `Todo`：任务清单快照（每次变更后追加，重放取最后一条）
//!
//! 重放语义：`Message` 按序回放；`Summary` 把此前的消息折叠为
//! `[摘要 System 消息] + 最近 kept_messages 条`，与压缩发生时内存中的状态一致
//! （文件仍保留全部原文供审计；恢复会话不会把已压缩的历史重新加载回来）。

use crate::session::{Session, SessionMeta};
use crate::todo::TodoItem;
use anyhow::{Context, Result};
use baiji_ai::Message;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// 一条持久化记录
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
    Started {
        meta: SessionMeta,
    },
    /// 会话标题（首条用户消息确定后追加；文件只追加，无法回填 Started）
    Title {
        title: String,
    },
    Message {
        message: Message,
    },
    Summary {
        content: String,
        /// 压缩后保留的最近消息条数（都已写在本记录之前）。
        /// 重放时据此丢弃更早的消息；旧文件无此字段（None）= 旧语义，原文全部保留
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kept_messages: Option<usize>,
    },
    /// 任务清单快照（todo 工具每次变更后由 harness 追加；重放取最后一条）
    Todo {
        items: Vec<TodoItem>,
    },
    /// 一次运行结束时的上下文节省台账（Context IR 摘要）。
    /// token 字段带 serde default：旧记录缺失时按 0 解析（回放时本就被忽略）
    Ledger {
        tool_calls: u32,
        original_bytes: u64,
        delivered_bytes: u64,
        #[serde(default)]
        original_tokens: u64,
        #[serde(default)]
        delivered_tokens: u64,
    },
}

/// JSONL 存储目录
#[derive(Debug, Clone)]
pub struct JsonlStore {
    dir: PathBuf,
}

impl JsonlStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).ok();
        Self { dir }
    }

    fn file_path(&self, session_id: &str) -> PathBuf {
        // 会话 ID 由内部生成，这里再做一次防御性过滤
        let safe: String = session_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        self.dir.join(format!("{safe}.jsonl"))
    }

    /// 追加一条记录（同步小写入，O(1)）。
    ///
    /// 整行（含换行）一次 `write_all` 写出并 fsync，尽量避免崩溃留下半行；
    /// 若文件尾部已是半行（上次崩溃所致），先补换行，避免新记录粘在坏行上。
    pub fn append(&self, session_id: &str, record: &Record) -> Result<()> {
        let mut line = serde_json::to_string(record).context("serialize record")?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(self.file_path(session_id))
            .with_context(|| format!("open session file for '{}'", session_id))?;
        if !ends_with_newline(&mut file)? {
            line.insert(0, '\n');
        }
        file.write_all(line.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }

    /// 加载会话：重放记录
    pub fn load(&self, session_id: &str) -> Result<Session> {
        let path = self.file_path(session_id);
        if !path.exists() {
            anyhow::bail!("session '{}' not found at {}", session_id, path.display());
        }

        // 半行可能截断在 UTF-8 字符中间，按 lossy 读取，坏行在下面跳过
        let bytes = std::fs::read(&path)?;
        let content = String::from_utf8_lossy(&bytes);
        let mut session: Option<Session> = None;
        let mut messages: Vec<Message> = Vec::new();

        for (n, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            // 坏行（崩溃/磁盘满留下的半行）只跳过，不让整个会话无法加载
            let record: Record = match serde_json::from_str(line) {
                Ok(record) => record,
                Err(e) => {
                    tracing::warn!(
                        "skipping corrupt record at {}:{}: {e}",
                        path.display(),
                        n + 1
                    );
                    continue;
                }
            };
            match record {
                Record::Started { meta } => {
                    if session.is_none() {
                        session = Some(Session {
                            meta,
                            messages: Vec::new(),
                            todos: Vec::new(),
                        });
                    }
                }
                Record::Title { title } => {
                    if let Some(session) = session.as_mut() {
                        session.meta.title = Some(title);
                    }
                }
                Record::Message { message } => messages.push(message),
                Record::Summary {
                    content,
                    kept_messages,
                } => {
                    let summary = Message::system(format!("[Conversation Summary]\n{content}"));
                    match kept_messages {
                        Some(kept) => {
                            let tail = messages.split_off(messages.len().saturating_sub(kept));
                            messages = vec![summary];
                            messages.extend(tail);
                        }
                        // 旧文件：无法得知保留了多少，维持旧行为
                        None => messages.push(summary),
                    }
                }
                // 任务清单：快照语义，最后一条生效
                Record::Todo { items } => {
                    if let Some(session) = session.as_mut() {
                        session.todos = items;
                    }
                }
                // 台账只作审计，不进入对话历史
                Record::Ledger { .. } => {}
            }
        }

        let mut session = session.context("session file missing Started record")?;
        session.messages = messages;
        session.derive_title();
        Ok(session)
    }

    /// 列出全部会话元信息（按文件名排序）
    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        let mut metas = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Ok(metas);
        };
        let mut files: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        files.sort();

        for file in files {
            if let Some(meta) = read_meta(&file) {
                metas.push(meta);
            }
        }
        Ok(metas)
    }
}

/// 文件为空或以换行结尾时返回 true
fn ends_with_newline(file: &mut std::fs::File) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// 列表用的轻量读取：只看文件头部几行（Started → 首条 user 消息 → Title），
/// 不加载整个会话。旧文件没有 Title 记录时，用首条 user 消息现场推导。
fn read_meta(path: &std::path::Path) -> Option<SessionMeta> {
    use std::io::BufRead;
    const HEAD_LINES: usize = 8;
    let file = std::fs::File::open(path).ok()?;
    let mut meta: Option<SessionMeta> = None;
    let mut first_user: Option<String> = None;

    for line in std::io::BufReader::new(file).lines().take(HEAD_LINES) {
        let Ok(line) = line else { break };
        match serde_json::from_str::<Record>(&line) {
            Ok(Record::Started { meta: started }) if meta.is_none() => meta = Some(started),
            Ok(Record::Title { title }) => {
                meta.as_mut()?.title = Some(title);
                break;
            }
            Ok(Record::Message { message })
                if first_user.is_none() && message.role == baiji_ai::Role::User =>
            {
                first_user = Some(message.content);
            }
            _ => {}
        }
    }

    let mut meta = meta?;
    if meta.title.is_none() {
        meta.title = first_user.map(|content| crate::session::title_from(&content));
    }
    Some(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baiji_ai::Role;

    #[test]
    fn test_append_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path());

        let mut session = Session::new(None);
        session.messages.push(Message::user("hello"));
        session.messages.push(Message::assistant("hi there"));

        store
            .append(
                &session.meta.id,
                &Record::Started {
                    meta: session.meta.clone(),
                },
            )
            .unwrap();
        for message in &session.messages {
            store
                .append(
                    &session.meta.id,
                    &Record::Message {
                        message: message.clone(),
                    },
                )
                .unwrap();
        }

        let loaded = store.load(&session.meta.id).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].content, "hello");
        assert_eq!(loaded.messages[1].content, "hi there");
        assert_eq!(loaded.meta.id, session.meta.id);
    }

    #[test]
    fn test_summary_record_becomes_system_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path());
        let session = Session::new(None);

        store
            .append(
                &session.meta.id,
                &Record::Started {
                    meta: session.meta.clone(),
                },
            )
            .unwrap();
        store
            .append(
                &session.meta.id,
                &Record::Message {
                    message: Message::user("q1"),
                },
            )
            .unwrap();
        store
            .append(
                &session.meta.id,
                &Record::Summary {
                    content: "early turns condensed".into(),
                    kept_messages: None,
                },
            )
            .unwrap();

        let loaded = store.load(&session.meta.id).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[1].role, Role::System);
        assert!(
            loaded.messages[1]
                .content
                .contains("[Conversation Summary]")
        );
    }

    #[test]
    fn test_summary_with_kept_count_replays_compacted_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path());
        let session = Session::new(None);
        let id = &session.meta.id;

        store
            .append(
                id,
                &Record::Started {
                    meta: session.meta.clone(),
                },
            )
            .unwrap();
        for text in ["q1", "a1", "q2", "a2", "q3"] {
            store
                .append(
                    id,
                    &Record::Message {
                        message: Message::user(text),
                    },
                )
                .unwrap();
        }
        store
            .append(
                id,
                &Record::Summary {
                    content: "q1/q2 condensed".into(),
                    kept_messages: Some(1),
                },
            )
            .unwrap();
        store
            .append(
                id,
                &Record::Message {
                    message: Message::assistant("a3"),
                },
            )
            .unwrap();

        let loaded = store.load(id).unwrap();
        let contents: Vec<&str> = loaded.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["[Conversation Summary]\nq1/q2 condensed", "q3", "a3"],
            "resume must not reload history that was already compacted"
        );
    }

    #[test]
    fn test_truncated_tail_line_is_skipped_and_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path());
        let session = Session::new(None);
        let id = &session.meta.id;

        store
            .append(
                id,
                &Record::Started {
                    meta: session.meta.clone(),
                },
            )
            .unwrap();
        store
            .append(
                id,
                &Record::Message {
                    message: Message::user("你好"),
                },
            )
            .unwrap();

        // 模拟崩溃：半行 JSON，且截断在多字节字符中间，无换行
        let mut partial =
            br#"{"record":"message","message":{"role":"assistant","content":""#.to_vec();
        partial.extend_from_slice(&"好".as_bytes()[..2]);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(store.file_path(id))
            .unwrap();
        file.write_all(&partial).unwrap();
        drop(file);

        let loaded = store.load(id).unwrap();
        assert_eq!(loaded.messages.len(), 1);

        // 之后的追加不会粘在坏行上
        store
            .append(
                id,
                &Record::Message {
                    message: Message::assistant("hi"),
                },
            )
            .unwrap();
        let loaded = store.load(id).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[1].content, "hi");
    }

    #[test]
    fn test_list_reads_title_record_and_derives_for_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path());

        let titled = Session::new(None);
        store
            .append(
                &titled.meta.id,
                &Record::Started {
                    meta: titled.meta.clone(),
                },
            )
            .unwrap();
        store
            .append(
                &titled.meta.id,
                &Record::Message {
                    message: Message::user("hello"),
                },
            )
            .unwrap();
        store
            .append(
                &titled.meta.id,
                &Record::Title {
                    title: "Saved title".into(),
                },
            )
            .unwrap();

        // 旧文件：没有 Title 记录
        let legacy = Session::new(None);
        store
            .append(
                &legacy.meta.id,
                &Record::Started {
                    meta: legacy.meta.clone(),
                },
            )
            .unwrap();
        store
            .append(
                &legacy.meta.id,
                &Record::Message {
                    message: Message::user("旧会话的问题"),
                },
            )
            .unwrap();

        let metas = store.list().unwrap();
        let title_of = |id: &str| {
            metas
                .iter()
                .find(|m| m.id == id)
                .unwrap()
                .title
                .clone()
                .unwrap()
        };
        assert_eq!(title_of(&titled.meta.id), "Saved title");
        assert_eq!(title_of(&legacy.meta.id), "旧会话的问题");
        assert_eq!(
            store.load(&titled.meta.id).unwrap().meta.title.as_deref(),
            Some("Saved title")
        );
    }

    #[test]
    fn test_project_roundtrips_and_legacy_defaults_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path());

        // 新会话:Started 携带 project
        let mut meta = Session::new(None).meta;
        meta.project = Some("myproj-ab12cd34".into());
        store
            .append(&meta.id, &Record::Started { meta: meta.clone() })
            .unwrap();
        let loaded = store.load(&meta.id).unwrap();
        assert_eq!(loaded.meta.project.as_deref(), Some("myproj-ab12cd34"));

        // 旧文件:Started 无 project 字段 → None
        let legacy = Session::new(None);
        let raw = serde_json::json!({
            "record": "started",
            "meta": {
                "id": legacy.meta.id,
                "created_at": "2026-01-01T00:00:00+00:00"
            }
        });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(store.file_path(&legacy.meta.id))
            .unwrap();
        use std::io::Write as _;
        writeln!(file, "{raw}").unwrap();
        drop(file);
        let loaded = store.load(&legacy.meta.id).unwrap();
        assert_eq!(loaded.meta.project, None);
    }

    #[test]
    fn test_todo_record_last_wins() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path());
        let session = Session::new(None);
        let id = &session.meta.id;
        store
            .append(
                id,
                &Record::Started {
                    meta: session.meta.clone(),
                },
            )
            .unwrap();
        let item = |n: usize| crate::todo::TodoItem {
            id: n,
            content: format!("task {n}"),
            status: crate::todo::TodoStatus::Pending,
            note: None,
        };
        // 两条快照：重放取最后一条
        store
            .append(
                id,
                &Record::Todo {
                    items: vec![item(1), item(2)],
                },
            )
            .unwrap();
        store
            .append(
                id,
                &Record::Todo {
                    items: vec![item(3)],
                },
            )
            .unwrap();

        let loaded = store.load(id).unwrap();
        assert_eq!(loaded.todos.len(), 1);
        assert_eq!(loaded.todos[0].content, "task 3");
    }

    #[test]
    fn test_list_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlStore::new(dir.path());

        for _ in 0..3 {
            let session = Session::new(None);
            store
                .append(
                    &session.meta.id,
                    &Record::Started {
                        meta: session.meta.clone(),
                    },
                )
                .unwrap();
        }
        assert_eq!(store.list().unwrap().len(), 3);

        // 不存在的会话
        assert!(store.load("nope").is_err());
    }
}
