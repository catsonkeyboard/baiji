//! 会话模型与分支树

use baiji_ai::Message;
use serde::{Deserialize, Serialize};

use crate::todo::TodoItem;

/// 会话元信息（JSONL 首条 Started 记录）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    /// 分叉来源会话（根会话为 None）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// 创建时间（RFC 3339）
    pub created_at: String,
    /// 标题（取首条用户消息摘要）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 所属项目（project_key = 目录名 + 路径哈希；旧会话为 None）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// 按项目分组：Some(key) 组按组内最新会话时间降序；None（旧会话，未记录
/// 项目）固定在最后。组内保持传入顺序（调用方自行排树）。
pub fn group_by_project(metas: Vec<SessionMeta>) -> Vec<(Option<String>, Vec<SessionMeta>)> {
    let mut groups: Vec<(Option<String>, Vec<SessionMeta>)> = Vec::new();
    for meta in metas {
        match groups.iter_mut().find(|(key, _)| *key == meta.project) {
            Some((_, list)) => list.push(meta),
            None => groups.push((meta.project.clone(), vec![meta])),
        }
    }
    // None 组最后，其余按组内最新 created_at 降序（RFC3339 同格式，字典序即时间序）
    groups.sort_by(|a, b| match (a.0.as_ref(), b.0.as_ref()) {
        (None, _) => std::cmp::Ordering::Greater,
        (_, None) => std::cmp::Ordering::Less,
        _ => latest_of(&b.1).cmp(&latest_of(&a.1)),
    });
    groups
}

fn latest_of(metas: &[SessionMeta]) -> String {
    metas
        .iter()
        .map(|m| m.created_at.as_str())
        .max()
        .unwrap_or_default()
        .to_string()
}

/// 会话 = 元信息 + 对话历史（不含 runtime 注入的 system prompt）
#[derive(Debug, Clone)]
pub struct Session {
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
    /// 任务清单（Record::Todo 重放的最终状态；运行期内存态由 harness 维护）
    pub todos: Vec<TodoItem>,
}

impl Session {
    pub fn new(parent_id: Option<String>) -> Self {
        Self {
            meta: SessionMeta {
                id: new_session_id(),
                parent_id,
                created_at: chrono::Utc::now().to_rfc3339(),
                title: None,
                project: None,
            },
            messages: Vec::new(),
            todos: Vec::new(),
        }
    }

    /// 首条用户消息的前 60 字符作为标题
    pub fn derive_title(&mut self) {
        if self.meta.title.is_none() {
            self.meta.title = self
                .messages
                .iter()
                .find(|m| m.role == baiji_ai::Role::User)
                .map(|m| title_from(&m.content));
        }
    }
}

/// 用户消息 → 标题：单行、前 60 字符
pub fn title_from(content: &str) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title: String = flat.chars().take(60).collect();
    if flat.chars().count() > 60 {
        title.push('…');
    }
    title
}

/// 生成会话 ID：时间戳 + 随机后缀
pub fn new_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let noise = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("sess_{ts}_{:04x}{:04x}", n & 0xffff, noise & 0xffff)
}

/// 会话树：由 store 中的元信息构建的父子关系视图
#[derive(Debug, Default)]
pub struct SessionTree {
    metas: Vec<SessionMeta>,
}

impl SessionTree {
    pub fn from_metas(metas: Vec<SessionMeta>) -> Self {
        Self { metas }
    }

    pub fn all(&self) -> &[SessionMeta] {
        &self.metas
    }

    /// 根会话（无 parent）
    pub fn roots(&self) -> Vec<&SessionMeta> {
        self.metas
            .iter()
            .filter(|m| m.parent_id.is_none())
            .collect()
    }

    /// 某会话的直接子会话
    pub fn children_of(&self, id: &str) -> Vec<&SessionMeta> {
        self.metas
            .iter()
            .filter(|m| m.parent_id.as_deref() == Some(id))
            .collect()
    }

    /// 深度优先展开为 (缩进深度, 会话)：根会话最新在前，分叉出的子会话紧跟其父。
    /// 父会话文件已不存在的"孤儿"按根处理。供会话列表 / 选择器渲染树形结构。
    pub fn flattened(&self) -> Vec<(usize, &SessionMeta)> {
        fn visit<'a>(
            tree: &'a SessionTree,
            node: &'a SessionMeta,
            depth: usize,
            out: &mut Vec<(usize, &'a SessionMeta)>,
        ) {
            // 防御：损坏的 parent 链成环时不无限递归
            if out.iter().any(|(_, seen)| seen.id == node.id) {
                return;
            }
            out.push((depth, node));
            let mut children = tree.children_of(&node.id);
            children.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            for child in children {
                visit(tree, child, depth + 1, out);
            }
        }

        let mut roots: Vec<&SessionMeta> = self
            .metas
            .iter()
            .filter(|m| match m.parent_id.as_deref() {
                None => true,
                Some(pid) => !self.metas.iter().any(|other| other.id == pid),
            })
            .collect();
        roots.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        let mut out = Vec::with_capacity(self.metas.len());
        for root in roots {
            visit(self, root, 0, &mut out);
        }
        out
    }

    /// 从根到该会话的祖先链（含自身）
    pub fn ancestry(&self, id: &str) -> Vec<&SessionMeta> {
        let mut chain = Vec::new();
        let mut current = self.metas.iter().find(|m| m.id == id);
        while let Some(meta) = current {
            chain.push(meta);
            current = meta
                .parent_id
                .as_deref()
                .and_then(|pid| self.metas.iter().find(|m| m.id == pid));
        }
        chain.reverse();
        chain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group_by_project_orders_and_pins_legacy() {
        let meta = |id: &str, project: Option<&str>, created: &str| SessionMeta {
            id: id.into(),
            parent_id: None,
            created_at: created.into(),
            title: None,
            project: project.map(str::to_string),
        };
        let metas = vec![
            meta("legacy1", None, "2026-01-05T10:00:00+00:00"),
            meta("a-old", Some("alpha-1111"), "2026-01-01T10:00:00+00:00"),
            meta("legacy2", None, "2026-03-01T10:00:00+00:00"),
            meta("beta-new", Some("beta-2222"), "2026-04-01T10:00:00+00:00"),
            meta("a-new", Some("alpha-1111"), "2026-02-01T10:00:00+00:00"),
        ];
        let groups = group_by_project(metas);
        let keys: Vec<Option<String>> = groups.iter().map(|(k, _)| k.clone()).collect();
        // 最新活动的组在前(beta 4月 > alpha 2月);旧会话(None)即使更新也固定最后
        assert_eq!(
            keys,
            vec![Some("beta-2222".into()), Some("alpha-1111".into()), None]
        );
        assert_eq!(groups[1].1.len(), 2); // 组内保持传入顺序
        assert_eq!(groups[2].1.len(), 2);
    }

    #[test]
    fn test_session_id_unique_and_shaped() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
        assert!(a.starts_with("sess_"));
    }

    #[test]
    fn test_derive_title() {
        let mut session = Session::new(None);
        session
            .messages
            .push(Message::user("帮我重构这个项目的模块划分结构 blah"));
        session.derive_title();
        let title = session.meta.title.clone().unwrap();
        assert!(title.starts_with("帮我重构"));
    }

    #[test]
    fn test_session_tree_relationships() {
        let root = SessionMeta {
            id: "s1".into(),
            parent_id: None,
            created_at: "t".into(),
            title: None,
            project: None,
        };
        let child = SessionMeta {
            id: "s2".into(),
            parent_id: Some("s1".into()),
            created_at: "t".into(),
            title: None,
            project: None,
        };
        let grand = SessionMeta {
            id: "s3".into(),
            parent_id: Some("s2".into()),
            created_at: "t".into(),
            title: None,
            project: None,
        };
        let tree = SessionTree::from_metas(vec![root, child.clone(), grand]);

        assert_eq!(tree.roots().len(), 1);
        assert_eq!(tree.children_of("s1"), vec![&child]);
        let ancestry: Vec<_> = tree.ancestry("s3").iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ancestry, vec!["s1", "s2", "s3"]);

        let flat: Vec<(usize, &str)> = tree
            .flattened()
            .iter()
            .map(|(d, m)| (*d, m.id.as_str()))
            .collect();
        assert_eq!(flat, vec![(0, "s1"), (1, "s2"), (2, "s3")]);
    }

    #[test]
    fn test_flattened_orders_roots_and_handles_orphans() {
        let meta = |id: &str, parent: Option<&str>, at: &str| SessionMeta {
            id: id.into(),
            parent_id: parent.map(String::from),
            created_at: at.into(),
            title: None,
            project: None,
        };
        let tree = SessionTree::from_metas(vec![
            meta("old", None, "2026-01-01"),
            meta("new", None, "2026-02-01"),
            meta("fork", Some("old"), "2026-01-02"),
            meta("orphan", Some("deleted"), "2026-03-01"),
        ]);
        let flat: Vec<(usize, &str)> = tree
            .flattened()
            .iter()
            .map(|(d, m)| (*d, m.id.as_str()))
            .collect();
        assert_eq!(
            flat,
            vec![(0, "orphan"), (0, "new"), (0, "old"), (1, "fork")]
        );
    }
}
