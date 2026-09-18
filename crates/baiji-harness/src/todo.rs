//! 会话级任务清单（todo）——长程任务的状态外置
//!
//! 计划只活在对话流里会被压缩摘要丢掉；本模块把任务清单外置到稳定结构：
//! - `TodoTool` 供 LLM 增删改查（add / update / list / clear）
//! - 当前清单渲染进**系统提示**（`## Task list` 段）——系统提示永不参与压缩，
//!   这是"目标外置"的关键性质；resume 会话后自动恢复定位
//! - 每次变更由 harness 在 run 结束时落盘（`Record::Todo`，重放取最后状态）
//!
//! 参照：Claude Code 的 TodoWrite、pi 的 todo 扩展、lean-ctx 的 ctx_task。

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 任务状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "pending" | "todo" => Some(Self::Pending),
            "in_progress" | "inprogress" | "doing" => Some(Self::InProgress),
            "done" | "complete" | "completed" => Some(Self::Done),
            _ => None,
        }
    }

    /// 渲染标记：`[ ]` 待办 / `[~]` 进行中 / `[x]` 完成
    pub fn marker(&self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Done => "[x]",
        }
    }
}

/// 一条任务
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: usize,
    pub content: String,
    pub status: TodoStatus,
    /// 附注（进展、阻塞原因等；可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 会话级任务清单存储：harness（注入/落盘）与 `TodoTool` 共享同一 Arc。
/// `dirty` 标记变更，harness 在 run 结束时据此落盘并清除。
#[derive(Default)]
pub struct TodoStore {
    items: Mutex<Vec<TodoItem>>,
    dirty: AtomicBool,
}

impl TodoStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前清单快照
    pub fn items(&self) -> Vec<TodoItem> {
        self.items.lock().unwrap().clone()
    }

    /// 整体替换（resume/切换会话时由 harness 调用）。
    /// 状态同步不是用户变更：不置 dirty（否则首次 run 会落一条多余快照）
    pub fn replace(&self, items: Vec<TodoItem>) {
        *self.items.lock().unwrap() = items;
    }

    /// 是否有未完成任务（自动接力的判定条件之一）
    pub fn has_open(&self) -> bool {
        self.items
            .lock()
            .unwrap()
            .iter()
            .any(|t| t.status != TodoStatus::Done)
    }

    /// 取走 dirty 标记（harness 落盘前调用；false = 无变更）
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn add(&self, content: String) -> TodoItem {
        let mut items = self.items.lock().unwrap();
        let id = items.last().map(|t| t.id + 1).unwrap_or(1);
        let item = TodoItem {
            id,
            content,
            status: TodoStatus::Pending,
            note: None,
        };
        items.push(item.clone());
        drop(items);
        self.dirty.store(true, Ordering::Relaxed);
        item
    }

    /// 按 id 更新（只改出现的字段）；返回更新后的条目
    pub(crate) fn update(
        &self,
        id: usize,
        status: Option<TodoStatus>,
        content: Option<String>,
        note: Option<Option<String>>,
    ) -> Option<TodoItem> {
        let mut items = self.items.lock().unwrap();
        let item = items.iter_mut().find(|t| t.id == id)?;
        if let Some(status) = status {
            item.status = status;
        }
        if let Some(content) = content {
            item.content = content;
        }
        if let Some(note) = note {
            item.note = note;
        }
        let updated = item.clone();
        drop(items);
        self.dirty.store(true, Ordering::Relaxed);
        Some(updated)
    }

    fn clear(&self) -> usize {
        let mut items = self.items.lock().unwrap();
        let count = items.len();
        items.clear();
        drop(items);
        self.dirty.store(true, Ordering::Relaxed);
        count
    }
}

/// 渲染任务清单（系统提示注入与工具返回共用）。
/// 空清单返回 None（不注入）。
pub fn todo_section(items: &[TodoItem]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut out = String::from("## Task list\n");
    for item in items {
        out.push_str(&format!(
            "{} {}. {}",
            item.status.marker(),
            item.id,
            item.content
        ));
        if let Some(note) = item.note.as_deref().filter(|n| !n.trim().is_empty()) {
            out.push_str(&format!(" — {note}"));
        }
        out.push('\n');
    }
    out.pop(); // 去掉末尾换行
    Some(out)
}

/// `todo` 工具：LLM 维护任务清单（状态外置，抗压缩）
pub struct TodoTool {
    store: Arc<TodoStore>,
}

impl TodoTool {
    pub fn new(store: Arc<TodoStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl baiji_agent::AgentTool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Manage the session task list — the plan lives OUTSIDE the conversation and is \
         never lost to context compaction. Actions: 'add' (content), 'update' (id + \
         status: pending/in_progress/done, optional content/note), 'list', 'clear'. \
         Workflow: before a multi-step task, add the steps; mark one in_progress when \
         you start it and done when finished — the current list is always visible to \
         you in the system prompt."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "update", "list", "clear"]},
                "content": {"type": "string", "description": "Task text (action=add)"},
                "id": {"type": "integer", "description": "Task id to modify (action=update)"},
                "status": {"type": "string", "enum": ["pending", "in_progress", "done"]},
                "note": {"type": "string", "description": "Short note on progress/blockers (action=update, optional)"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<baiji_agent::ToolOutput> {
        use baiji_agent::ToolOutput;
        let action = args["action"].as_str().unwrap_or_default();
        let render = |items: &[TodoItem]| {
            todo_section(items).unwrap_or_else(|| "(task list is empty)".to_string())
        };
        match action {
            "add" => {
                let Some(content) = args["content"]
                    .as_str()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                else {
                    return Ok(ToolOutput::err(
                        "[Error] action=add requires a non-empty 'content'",
                    ));
                };
                let item = self.store.add(content.to_string());
                Ok(ToolOutput::ok(format!(
                    "added task #{}: {}\n\n{}",
                    item.id,
                    item.content,
                    render(&self.store.items())
                )))
            }
            "update" => {
                let Some(id) = args["id"].as_u64() else {
                    return Ok(ToolOutput::err("[Error] action=update requires an 'id'"));
                };
                let status = match args["status"].as_str() {
                    Some(s) => match TodoStatus::parse(s) {
                        Some(st) => Some(st),
                        None => {
                            return Ok(ToolOutput::err(format!(
                                "[Error] invalid status '{s}' (pending/in_progress/done)"
                            )));
                        }
                    },
                    None => None,
                };
                let content = args["content"]
                    .as_str()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .map(str::to_string);
                let note = if args.get("note").is_some() {
                    Some(args["note"].as_str().map(str::to_string))
                } else {
                    None
                };
                match self.store.update(id as usize, status, content, note) {
                    Some(item) => Ok(ToolOutput::ok(format!(
                        "updated task #{}: {} {}\n\n{}",
                        item.id,
                        item.status.marker(),
                        item.content,
                        render(&self.store.items())
                    ))),
                    None => Ok(ToolOutput::err(format!(
                        "[Error] no task with id {id} — use action=list to see ids"
                    ))),
                }
            }
            "list" => Ok(ToolOutput::ok(render(&self.store.items()))),
            "clear" => {
                let removed = self.store.clear();
                Ok(ToolOutput::ok(format!(
                    "cleared {removed} task(s) — the task list is now empty"
                )))
            }
            other => Ok(ToolOutput::err(format!(
                "[Error] unknown action '{other}' (add/update/list/clear)"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baiji_agent::AgentTool;

    fn store_with(items: &[(&str, TodoStatus)]) -> Arc<TodoStore> {
        let store = Arc::new(TodoStore::new());
        for (content, status) in items {
            let item = store.add(content.to_string());
            store.update(item.id, Some(*status), None, None).unwrap();
        }
        store
    }

    #[test]
    fn test_store_crud_and_dirty() {
        let store = TodoStore::new();
        assert!(!store.take_dirty());
        assert!(!store.has_open());

        let a = store.add("first".into());
        let b = store.add("second".into());
        assert_eq!((a.id, b.id), (1, 2));
        assert!(store.take_dirty()); // add 置脏
        assert!(!store.take_dirty()); // 取走后清零

        store
            .update(b.id, Some(TodoStatus::InProgress), None, None)
            .unwrap();
        assert!(store.take_dirty());
        assert!(store.has_open());

        store
            .update(
                a.id,
                Some(TodoStatus::Done),
                None,
                Some(Some("done early".into())),
            )
            .unwrap();
        store
            .update(b.id, Some(TodoStatus::Done), None, None)
            .unwrap();
        assert!(!store.has_open());

        // 不存在的 id
        assert!(
            store
                .update(99, Some(TodoStatus::Done), None, None)
                .is_none()
        );

        // 整体替换（resume 语义）
        store.replace(vec![TodoItem {
            id: 1,
            content: "resumed".into(),
            status: TodoStatus::Pending,
            note: None,
        }]);
        assert!(store.has_open());
        assert_eq!(store.items().len(), 1);
    }

    #[test]
    fn test_todo_section_rendering() {
        assert!(todo_section(&[]).is_none());
        let store = store_with(&[
            ("setup project", TodoStatus::Done),
            ("implement parser", TodoStatus::InProgress),
            ("write tests", TodoStatus::Pending),
        ]);
        let section = todo_section(&store.items()).unwrap();
        assert!(section.starts_with("## Task list"), "{section}");
        assert!(section.contains("[x] 1. setup project"), "{section}");
        assert!(section.contains("[~] 2. implement parser"), "{section}");
        assert!(section.contains("[ ] 3. write tests"), "{section}");
    }

    #[tokio::test]
    async fn test_todo_tool_actions() {
        let store = Arc::new(TodoStore::new());
        let tool = TodoTool::new(store.clone());

        // add
        let out = tool
            .execute(serde_json::json!({"action": "add", "content": "refactor module"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content.contains("added task #1: refactor module"),
            "{}",
            out.content
        );
        assert!(out.content.contains("[ ] 1. refactor module"));

        // update 状态
        let out = tool
            .execute(serde_json::json!({"action": "update", "id": 1, "status": "in_progress", "note": "wip"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content.contains("[~] 1. refactor module — wip"),
            "{}",
            out.content
        );

        // list
        let out = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(out.content.contains("[~] 1. refactor module — wip"));

        // 错误路径：非法状态 / 未知 id / 空 content / 未知 action
        let out = tool
            .execute(serde_json::json!({"action": "update", "id": 1, "status": "bogus"}))
            .await
            .unwrap();
        assert!(out.is_error);
        let out = tool
            .execute(serde_json::json!({"action": "update", "id": 42, "status": "done"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no task with id 42"));
        let out = tool
            .execute(serde_json::json!({"action": "add", "content": "  "}))
            .await
            .unwrap();
        assert!(out.is_error);
        let out = tool
            .execute(serde_json::json!({"action": "destroy"}))
            .await
            .unwrap();
        assert!(out.is_error);

        // clear
        let out = tool
            .execute(serde_json::json!({"action": "clear"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("cleared 1 task"));
        assert!(!store.has_open());
    }
}
