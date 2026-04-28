use crate::app::{App, MessageRole};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

/// UI 渲染器
pub struct UI;

impl UI {
    /// 绘制主界面
    pub fn draw(f: &mut Frame, app: &mut App) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),      // 对话区域
                Constraint::Length(3),   // 输入框
                Constraint::Length(1),   // 状态栏
            ])
            .split(f.area());

        // 渲染对话区域
        Self::render_chat_area(f, app, chunks[0]);

        // 渲染输入框
        Self::render_input_box(f, app, chunks[1]);

        // 渲染状态栏
        Self::render_status_bar(f, app, chunks[2]);
    }

    /// 渲染对话区域
    fn render_chat_area(f: &mut Frame, app: &mut App, area: Rect) {
        let block = Block::default()
            .title(" Chat ")
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue));

        // 构建消息文本
        let mut text_lines: Vec<Line> = Vec::new();

        for (idx, message) in app.messages.iter().enumerate() {
            // 添加空行分隔（除了第一条消息）
            if idx > 0 {
                text_lines.push(Line::from(""));
            }

            // 消息头部
            let (prefix, color) = match message.role {
                MessageRole::User => ("You", Color::Green),
                MessageRole::Assistant => ("Assistant", Color::Cyan),
                MessageRole::System => ("System", Color::Yellow),
            };

            let time_str = message.timestamp.format("%H:%M:%S").to_string();
            let header = Line::from(vec![
                Span::styled(
                    format!("[{}] ", time_str),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{}", prefix),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(":", Style::default().fg(color)),
            ]);
            text_lines.push(header);

            // 消息内容
            for line in message.content.lines() {
                let content_line = Line::from(Span::styled(
                    format!("  {}", line),
                    Style::default().fg(Color::White),
                ));
                text_lines.push(content_line);
            }
        }

        // 如果是流式输出，添加闪烁光标
        if app.is_streaming {
            text_lines.push(Line::from(Span::styled(
                "  █",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::SLOW_BLINK),
            )));
        }

        // 计算滚动 - 禁用 wrap 以确保滚动准确
        let visible_lines = area.height.saturating_sub(2) as usize; // 减去边框
        let total_lines = text_lines.len();

        // 计算最大滚动值
        let max_scroll = total_lines.saturating_sub(visible_lines);
        let scroll = app.scroll.min(max_scroll);
        // 将实际滚动位置写回，确保键盘事件始终在有效范围内操作
        app.scroll = scroll;

        // 不使用 wrap，确保 scroll 按行精确控制
        let paragraph = Paragraph::new(Text::from(text_lines))
            .block(block)
            .scroll((scroll as u16, 0));

        f.render_widget(paragraph, area);

        // 渲染滚动条
        if total_lines > visible_lines {
            let scrollbar = Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None);

            let mut scrollbar_state = ScrollbarState::new(max_scroll + 1)
                .position(scroll)
                .viewport_content_length(visible_lines);

            f.render_stateful_widget(
                scrollbar,
                area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
        }
    }

    /// 渲染输入框
    fn render_input_box(f: &mut Frame, app: &App, area: Rect) {
        let is_active = !app.is_streaming;

        let block = Block::default()
            .title(" Input ")
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(if is_active {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Gray)
            });

        let input_text = if app.input.is_empty() {
            if is_active {
                Span::styled(
                    "Type your message here...",
                    Style::default().fg(Color::DarkGray),
                )
            } else {
                Span::styled(
                    "Waiting for response...",
                    Style::default().fg(Color::DarkGray),
                )
            }
        } else {
            Span::raw(&app.input)
        };

        let paragraph = Paragraph::new(Line::from(input_text)).block(block);

        f.render_widget(paragraph, area);

        // 设置光标位置（仅在非流式输出时显示）
        if is_active {
            let cursor_x = area.x + 1 + app.input[..app.cursor_position].width() as u16;
            let cursor_y = area.y + 1;
            f.set_cursor_position((cursor_x, cursor_y));
        }
    }

    /// 渲染状态栏
    fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        // 左侧：状态和模型信息
        let status_text = if app.is_streaming {
            Span::styled(
                format!(
                    "⏳ Turn {} | {} tools",
                    app.current_turn, app.tool_call_count
                ),
                Style::default().fg(Color::Yellow),
            )
        } else {
            Span::styled("✓ Ready", Style::default().fg(Color::Green))
        };

        let left_spans = vec![
            status_text,
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}/{}", app.config.llm.provider, app.config.llm.model),
                Style::default().fg(Color::Blue),
            ),
        ];

        let left = Paragraph::new(Line::from(left_spans));
        f.render_widget(left, chunks[0]);

        // 右侧：帮助信息
        let right_spans = vec![
            Span::styled("Enter: Send", Style::default().fg(Color::DarkGray)),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled("Ctrl+C: Quit", Style::default().fg(Color::DarkGray)),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled("↑/↓: Scroll", Style::default().fg(Color::DarkGray)),
        ];

        let right = Paragraph::new(Line::from(right_spans)).alignment(Alignment::Right);
        f.render_widget(right, chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn create_test_app() -> App {
        App::new(Config::default(), None, None, 0)
    }

    #[test]
    fn test_ui_creation() {
        let app = create_test_app();
        // 验证应用可以正常创建
        assert!(!app.should_quit);
    }
}
