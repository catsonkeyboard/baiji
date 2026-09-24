//! 会话选择器与子代理面板、待裁决确认。

use baiji_agent::ConfirmationDecision;
use baiji_harness::SessionMeta;
use tokio::sync::oneshot;

use crate::confirm::ConfirmDialog;

pub struct SessionPicker {
    /// 全部会话（过滤的源头）
    all: Vec<SessionMeta>,
    /// 项目过滤键（None = 不过滤显示全部）
    filter: Option<String>,
    /// 树形顺序：根会话最新在前，分叉出的子会话紧跟其父
    pub items: Vec<SessionMeta>,
    /// 与 items 一一对应的缩进深度
    pub depths: Vec<usize>,
    pub selected: usize,
    /// 当前会话 id（过滤后重定位选中用）
    current_id: String,
}

impl SessionPicker {
    /// 按分叉关系排成树，默认选中当前会话。
    /// `project` 有值时初始只显示该项目的会话（当前会话无项目则显示全部）
    pub fn from_metas(metas: Vec<SessionMeta>, current_id: &str) -> Self {
        let current_project = metas
            .iter()
            .find(|m| m.id == current_id)
            .and_then(|m| m.project.clone());
        // 当前会话有项目时初始只显示该项目（无项目 = 旧会话，显示全部）
        let filter = current_project.clone().filter(|key| {
            metas
                .iter()
                .any(|m| m.project.as_deref() == Some(key.as_str()))
        });
        let mut picker = Self {
            all: metas,
            filter,
            items: Vec::new(),
            depths: Vec::new(),
            selected: 0,
            current_id: current_id.to_string(),
        };
        picker.rebuild();
        picker
    }

    /// 用当前过滤条件重建可见列表
    fn rebuild(&mut self) {
        let visible: Vec<SessionMeta> = self
            .all
            .iter()
            .filter(|m| {
                self.filter
                    .as_ref()
                    .is_none_or(|key| m.project.as_deref() == Some(key.as_str()))
            })
            .cloned()
            .collect();
        let tree = baiji_harness::SessionTree::from_metas(visible);
        let (depths, items): (Vec<usize>, Vec<SessionMeta>) = tree
            .flattened()
            .into_iter()
            .map(|(depth, meta)| (depth, meta.clone()))
            .unzip();
        self.selected = items
            .iter()
            .position(|m| m.id == self.current_id)
            .unwrap_or(0);
        self.items = items;
        self.depths = depths;
    }

    /// a 键：本项目 ↔ 全部切换
    pub fn toggle_project_filter(&mut self) {
        if self.filter.is_some() {
            self.filter = None;
        } else {
            // 取当前会话（或首个有项目的会话）的项目作为过滤键
            self.filter = self
                .all
                .iter()
                .find(|m| m.id == self.current_id)
                .and_then(|m| m.project.clone())
                .or_else(|| self.all.iter().find_map(|m| m.project.clone()));
        }
        self.rebuild();
    }

    /// 是否处于项目过滤态（选择器测试断言用）
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn filtering_by_project(&self) -> bool {
        self.filter.is_some()
    }

    pub fn selected_meta(&self) -> Option<&SessionMeta> {
        self.items.get(self.selected)
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    /// 选择器展示行：树形缩进 + id · 标题（当前会话打标；
    /// 全部视图下其它项目的会话附项目标注）
    pub fn display_rows(&self, current_id: &str, no_title: &str) -> Vec<String> {
        self.items
            .iter()
            .zip(&self.depths)
            .map(|(meta, depth)| {
                let mark = if meta.id == current_id { "▸ " } else { "  " };
                let title = meta.title.as_deref().unwrap_or(no_title);
                let branch = if *depth > 0 {
                    format!("{}└ ", "  ".repeat(depth - 1))
                } else {
                    String::new()
                };
                // 项目过滤关闭时，为非当前项目的会话附标注
                let project_tag = if self.filter.is_none() {
                    match &meta.project {
                        Some(project) => format!(" · ⌂{project}"),
                        None => " · ⌂?".to_string(),
                    }
                } else {
                    String::new()
                };
                format!("{mark}{branch}{} · {}{project_tag}", meta.id, title)
            })
            .collect()
    }
}


/// 子代理角色面板状态（数据每帧从 harness 快照，热重载后即时刷新）
pub struct SubagentsPanel {
    pub selected: usize,
}

impl SubagentsPanel {
    pub(crate) fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub(crate) fn move_down(&mut self, len: usize) {
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }
}

/// 待用户裁决的确认
pub struct PendingConfirm {
    pub tool_name: String,
    pub args: String,
    pub reply: oneshot::Sender<ConfirmationDecision>,
}

impl PendingConfirm {
    pub(crate) fn from_dialog(dialog: ConfirmDialog) -> Self {
        Self {
            // 完整展示：用户必须能看到自己批准的全部内容（UI 负责折行与溢出提示）
            args: match dialog.request.args.get("command").and_then(|c| c.as_str()) {
                // bash：直接展示命令原文（保留换行）
                Some(command) if dialog.request.tool_name == "bash" => command.to_string(),
                _ => serde_json::to_string_pretty(&dialog.request.args)
                    .unwrap_or_else(|_| dialog.request.args.to_string()),
            },
            tool_name: dialog.request.tool_name,
            reply: dialog.reply,
        }
    }
}

