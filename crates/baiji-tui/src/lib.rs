//! baiji-tui — 终端 UI 层
//!
//! 三栏布局（聊天区 / 输入框 / 状态栏）的 Ratatui 应用：
//! - 消费 [`baiji_agent::AgentEvent`] 实时渲染流式文本与工具执行
//! - 运行中输入 → steering 队列；Esc → 取消当前运行
//! - Ctrl+O 打开会话选择器（切换 / 分叉）
//! - HITL：确认请求弹出对话框（y/a/n），由 [`InteractiveApprover`] 桥接
//! - 多行输入框：Shift/Alt+Enter、Ctrl+J、行尾 `\` 换行；光标编辑；粘贴保留换行
//! - ↑↓ 提示词历史（`~/.baiji/prompt-history.jsonl`）；Shift+↑↓ / PageUp/PageDown 滚动，
//!   仅在视图贴底时随新内容回底

mod app;
mod confirm;
mod diff;
mod i18n;
mod markdown;
mod settings;
mod theme;
mod tool_display;
mod ui;

use anyhow::Result;
use baiji_harness::AgentHarness;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

pub use confirm::{ConfirmDialog, InteractiveApprover};
pub use i18n::{Lang, Strings};
pub use settings::{AutoContinueConfig, RuntimeSettings};
pub use theme::Theme;

/// 启动 TUI 主循环。
/// - `theme`：配色
/// - `confirm_rx`：HITL 确认请求通道（无确认需求时传 None）
/// - `config_path`：配置文件路径（/config 向导写回）
/// - `settings`：当前生效设置镜像（/model、向导、状态栏使用）
/// - `jobs`：后台任务注册表（/tasks、/kill；未启用时传 None）
/// - `lang`：界面语言（ui.language；默认英文）
pub async fn run(
    harness: Arc<tokio::sync::Mutex<AgentHarness>>,
    theme: Theme,
    confirm_rx: Option<UnboundedReceiver<ConfirmDialog>>,
    config_path: std::path::PathBuf,
    settings: RuntimeSettings,
    settings_summary: String,
    auto_continue: AutoContinueConfig,
    jobs: Option<Arc<baiji_tools::tools::jobs::JobRegistry>>,
    lang: Lang,
) -> Result<()> {
    // 提示词历史与全局配置同目录：~/.baiji/prompt-history.jsonl
    let history_path = config_path
        .parent()
        .map(|dir| dir.join("prompt-history.jsonl"));
    let mut app = app::App::new(
        harness,
        theme,
        confirm_rx,
        config_path,
        settings,
        settings_summary,
        auto_continue,
        jobs,
        lang,
    );
    if let Some(path) = history_path {
        app.set_prompt_history(app::PromptHistory::load(path));
    }
    app.run().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_chat_line_shape() {
        let line = app::ChatLine::assistant("hello".to_string());
        assert!(matches!(line, app::ChatLine::Assistant(s) if s == "hello"));
        let user = app::ChatLine::user("hi".to_string());
        assert!(matches!(user, app::ChatLine::User(s) if s == "hi"));
    }

    #[test]
    fn theme_exports() {
        assert_eq!(Theme::parse("light"), Theme::light());
    }
}
