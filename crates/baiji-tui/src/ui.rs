//! Ratatui 渲染：聊天区 / 输入框 / 状态栏 + 会话选择器 overlay + 确认对话框

use crate::app::{App, ChatLine};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use ratatui::Frame;

/// 渲染一帧。消耗 &mut App：滚动哨兵值在每帧渲染后钳制。
pub fn draw(frame: &mut Frame, app: &mut App) {
    // 斜杠命令提示：向导打开时抑制（其输入框不属于命令语义）
    let hints = if app.wizard_active() {
        None
    } else {
        app.slash_hints_view().filter(|(_, h)| !h.is_empty())
    };
    let hints_active = hints.is_some();
    let chunks = if hints_active {
        Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area())
    } else {
        Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area())
    };

    draw_chat(frame, app, chunks[0]);
    // 有提示条时布局为 [chat, hints, input, status]，否则 [chat, input, status]
    let (input_area, status_area) = if hints_active {
        (chunks[2], chunks[3])
    } else {
        (chunks[1], chunks[2])
    };
    if let Some((selected, hints)) = hints {
        draw_slash_hints(frame, app, chunks[1], selected, &hints);
    }
    draw_input(frame, app, input_area);
    draw_status(frame, app, status_area);

    if let Some(rows) = app.picker_rows() {
        draw_picker(frame, app, rows);
    }
    if app.wizard_title().is_some() {
        draw_wizard(frame, app);
    }
    // 确认弹窗最后绘制：盖在所有内容之上
    if app.pending_confirm().is_some() {
        draw_confirm(frame, app, frame.area());
    }
}

fn line_style(line: &ChatLine, theme: &crate::theme::Theme) -> Style {
    use ratatui::style::Color;
    let color = match line {
        ChatLine::User(_) => theme.user,
        ChatLine::Assistant(_) => theme.assistant,
        ChatLine::Tool(_) => theme.tool,
        ChatLine::System(_) => theme.system,
    };
    let _ = Color::White; // 保持 ratatui 引用（theme 使用 Color）
    Style::default().fg(color)
}

fn draw_chat(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme();
    let block = Block::new().borders(Borders::ALL).title(" baiji ");
    let inner = block.inner(area);
    let width = inner.width.max(1) as usize;

    // 自行按显示宽度折行成「可视行」：
    // - 消息里的换行得以保留（Line 不能含 '\n'，否则代码块会被压成一行）
    // - CJK 按 2 列计宽，行数精确 → 滚动范围精确，长中文回复能滚到底
    let mut rows: Vec<Line> = Vec::new();
    let mut push_message = |label: &str, content: &str, style: Style| {
        let text = format!("[{label}] {content}");
        rows.extend(
            wrap_by_width(&text, width)
                .into_iter()
                .map(|row| Line::styled(row, style)),
        );
    };
    for line in app.lines() {
        push_message(line.label(), line.content(), line_style(line, &theme));
    }
    // 进行中的思考（暗色斜体，仅展示尾部；答案开始输出后消失）
    if let Some(thinking) = app.thinking_tail(400) {
        push_message(
            "thinking",
            &thinking,
            Style::default()
                .fg(theme.system)
                .add_modifier(Modifier::ITALIC | Modifier::DIM),
        );
    }
    // 流式中的部分回答
    if !app.streaming().is_empty() {
        push_message("baiji", app.streaming(), Style::default().fg(theme.assistant));
    }

    let visible = inner.height as usize;
    let max_scroll = rows.len().saturating_sub(visible);

    // 哨兵 usize::MAX = 贴底
    if app.scroll() > max_scroll {
        app.set_scroll(max_scroll);
    }
    let scroll = app.scroll().min(max_scroll);

    // 只渲染可见窗口（Paragraph::scroll 是 u16，长会话会溢出；也省去整段重排）
    let window: Vec<Line> = rows.into_iter().skip(scroll).take(visible).collect();
    frame.render_widget(Paragraph::new(Text::from(window)).block(block), area);

    if max_scroll > 0 {
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut ScrollbarState::new(max_scroll + 1).position(scroll),
        );
    }
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let (title, style) = if app.agent_running() {
        (
            " input (steering) ",
            Style::default().fg(theme.tool),
        )
    } else {
        (" input ", Style::default())
    };
    let input = Paragraph::new(Line::from(format!("{}▏", app.input())))
        .style(style)
        .block(Block::new().borders(Borders::ALL).title(title));
    frame.render_widget(input, area);
}

/// 斜杠命令提示条：命令列表（选中高亮）+ 选中命令用法
fn draw_slash_hints(
    frame: &mut Frame,
    app: &App,
    area: ratatui::layout::Rect,
    selected: usize,
    hints: &[(&'static str, &'static str)],
) {
    let theme = app.theme();
    let mut spans: Vec<ratatui::text::Span> = Vec::new();
    for (i, (name, _)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(ratatui::text::Span::raw("  "));
        }
        let style = if i == selected {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.system)
        };
        spans.push(ratatui::text::Span::styled(format!("/{name}"), style));
    }
    spans.push(ratatui::text::Span::styled(
        "   Tab=补全 ↑↓=选择",
        Style::default().fg(theme.system),
    ));

    let usage = hints
        .get(selected)
        .map(|(_, u)| u.to_string())
        .unwrap_or_default();
    let paragraph = Paragraph::new(vec![
        Line::from(spans),
        Line::styled(usage, Style::default().fg(theme.tool)),
    ]);
    frame.render_widget(paragraph, area);
}

/// HITL 确认对话框：居中弹窗，完整展示待执行内容（按显示宽度折行）。
/// 放不下时明确标出被隐藏的行数——用户不会在不知情的情况下批准看不到的内容。
fn draw_confirm(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let Some(pending) = app.pending_confirm() else {
        return;
    };
    let popup = centered_rect(area, 90, 70);
    let inner_width = popup.width.saturating_sub(2).max(1) as usize;
    // 边框 2 行 + 标题行 + 操作提示行
    let body_height = popup.height.saturating_sub(4) as usize;

    let wrapped = wrap_by_width(&pending.args, inner_width);
    let hidden = wrapped.len().saturating_sub(body_height);
    let warn = Style::default()
        .fg(theme.error)
        .add_modifier(Modifier::BOLD);

    let mut lines = vec![Line::styled(format!("⚠ 执行工具 {}", pending.tool_name), warn)];
    if hidden > 0 {
        // 留一行给溢出提示
        let shown = body_height.saturating_sub(1);
        lines.extend(wrapped.iter().take(shown).cloned().map(Line::raw));
        lines.push(Line::styled(
            format!(
                "… 还有 {} 行未显示（窗口太小）；不确定请按 n 拒绝",
                wrapped.len() - shown
            ),
            warn,
        ));
    } else {
        lines.extend(wrapped.into_iter().map(Line::raw));
    }
    lines.push(Line::styled("y=允许  a=本次全部允许  n=拒绝", warn));

    let widget = Paragraph::new(lines).block(
        Block::new()
            .borders(Borders::ALL)
            .title(" 确认执行? ")
            .border_style(Style::default().fg(theme.error)),
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(widget, popup);
}

/// 按终端显示宽度硬折行（CJK 占 2 列；制表符按 4 列；控制字符显示为 `?`，
/// 防止转义序列篡改弹窗内容）
fn wrap_by_width(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut line = String::new();
        let mut used = 0;
        for c in raw.chars() {
            let (c, w) = match c {
                '\t' => (' ', 4),
                c if c.is_control() => ('?', 1),
                c => (c, c.width().unwrap_or(0)),
            };
            if used + w > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            if c == ' ' && w == 4 {
                line.push_str("    ");
            } else {
                line.push(c);
            }
            used += w;
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let style = if app.agent_running() {
        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.system)
    };
    let status = Paragraph::new(Line::from(app.status_line())).style(style);
    frame.render_widget(status, area);
}

/// 会话选择器：居中 overlay
fn draw_picker(frame: &mut Frame, app: &App, rows: Vec<String>) {
    let theme = app.theme();
    let area = centered_rect(frame.area(), 70, 60);

    let items: Vec<ListItem> = rows
        .into_iter()
        .map(|row| ListItem::new(Line::from(row)))
        .collect();
    let list = List::new(items)
        .block(
            Block::new()
                .borders(Borders::ALL)
                .title(" 会话 (Enter=切换 b=分叉 Esc=关闭) ")
                .border_style(Style::default().fg(theme.accent)),
        )
        .highlight_style(
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_widget(Clear, area);
    let mut state = ListState::default();
    if let Some(picker) = app.picker() {
        state.select(Some(picker.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

/// 配置向导 overlay：列表步骤居中列表，输入步骤居中对话框
fn draw_wizard(frame: &mut Frame, app: &App) {
    let theme = app.theme();
    let Some(title) = app.wizard_title() else { return };

    match app.wizard_rows() {
        Some(rows) => {
            let area = centered_rect(frame.area(), 75, 65);
            let items: Vec<ListItem> = rows
                .iter()
                .map(|row| ListItem::new(Line::from(row.as_str())))
                .collect();
            let list = List::new(items)
                .block(
                    Block::new()
                        .borders(Borders::ALL)
                        .title(format!(" {title} "))
                        .border_style(Style::default().fg(theme.accent)),
                )
                .highlight_style(
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                );
            frame.render_widget(Clear, area);
            let mut state = ListState::default();
            state.select(Some(app.wizard_selected()));
            frame.render_stateful_widget(list, area, &mut state);
        }
        None => {
            // 输入步骤：对话框 + 提示
            let area = centered_rect(frame.area(), 70, 18);
            let Some(prompt) = app.wizard_input_prompt() else { return };
            let paragraph = Paragraph::new(vec![
                Line::styled(prompt.clone(), Style::default().fg(theme.tool)),
                Line::from(format!("{}▏", app.input())),
            ])
            .block(
                Block::new()
                    .borders(Borders::ALL)
                    .title(format!(" {title} "))
                    .border_style(Style::default().fg(theme.accent)),
            );
            frame.render_widget(Clear, area);
            frame.render_widget(paragraph, area);
        }
    }
}

/// 居中矩形（百分比宽高）
fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let popup = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_by_width_keeps_everything() {
        // 长命令不丢内容：折行后拼回等于原文
        let command = format!("echo start && {} && rm -rf ./important", "x".repeat(300));
        let wrapped = wrap_by_width(&command, 40);
        assert!(wrapped.len() > 1);
        assert_eq!(wrapped.concat(), command);

        // CJK 按 2 列计宽
        assert_eq!(wrap_by_width("你好世界", 4), vec!["你好", "世界"]);
        // 空行保留（代码块里的空行不能丢）
        assert_eq!(wrap_by_width("a\n\nb", 80), vec!["a", "", "b"]);
        // 多行命令保留换行；控制字符被中和
        assert_eq!(wrap_by_width("a\nb\u{1b}[2J", 80), vec!["a", "b?[2J"]);
    }
}
