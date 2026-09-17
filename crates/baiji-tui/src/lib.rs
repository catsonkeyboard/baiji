//! baiji-tui — 终端 UI 层
//!
//! 三栏布局（聊天区 / 输入框 / 状态栏）的 Ratatui 应用：
//! - 消费 [`baiji_agent::AgentEvent`] 实时渲染流式文本与工具执行
//! - 运行中输入 → steering 队列；Esc → 取消当前运行
//! - Ctrl+O 打开会话选择器（切换 / 分叉）
//! - HITL：确认请求弹出对话框（y/a/n），由 [`InteractiveApprover`] 桥接
//! - PageUp/PageDown 滚动，新内容到达自动回底

mod app;
mod confirm;
mod settings;
mod theme;
mod ui;

use anyhow::Result;
use baiji_harness::AgentHarness;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

pub use confirm::{ConfirmDialog, InteractiveApprover};
pub use settings::RuntimeSettings;
pub use theme::Theme;

/// 启动 TUI 主循环。
/// - `theme`：配色
/// - `confirm_rx`：HITL 确认请求通道（无确认需求时传 None）
/// - `config_path`：配置文件路径（/config 向导写回）
/// - `settings`：当前生效设置镜像（/model、向导、状态栏使用）
pub async fn run(
    harness: Arc<tokio::sync::Mutex<AgentHarness>>,
    theme: Theme,
    confirm_rx: Option<UnboundedReceiver<ConfirmDialog>>,
    config_path: std::path::PathBuf,
    settings: RuntimeSettings,
) -> Result<()> {
    app::App::new(harness, theme, confirm_rx, config_path, settings)
        .run()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_chat_line_shape() {
        let line = app::ChatLine::assistant("hello".to_string());
        assert_eq!(line.label(), "baiji");
        let user = app::ChatLine::user("hi".to_string());
        assert_eq!(user.label(), "you");
    }

    #[test]
    fn theme_exports() {
        assert_eq!(Theme::parse("light"), Theme::light());
    }
}
