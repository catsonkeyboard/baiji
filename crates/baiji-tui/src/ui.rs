//! Ratatui 渲染：顶栏 / 聊天区（无框）/ 斜杠提示 / 圆角输入框 / 状态栏，
//! 聊天区右上角悬浮 todo 面板；会话选择器 / 配置向导 / 确认弹窗为 overlay。
//!
//! 视觉语言对齐 Claude Code / OpenCode / Grok CLI：
//! - 无边框聊天区，消息靠前缀符号区分（`> ` 用户、`●` 工具、`⎿` 结果、暗色系统行）
//! - 信息全部退到边缘一行（顶栏）与半行（状态栏），正文区零装饰
//! - todo 清单悬浮在聊天区右上角，有任务时才出现

use crate::app::{App, ChatLine};
use baiji_harness::TodoStatus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState,
};

/// 运行中旋转指示器的帧序列
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// 欢迎屏 logo（ANSI Shadow 风格块字 "baiji"，28 列宽）
const LOGO: &[&str] = &[
    "██████╗  █████╗ ██╗     ██╗██╗",
    "██╔══██╗██╔══██╗██║     ██║██║",
    "██████╔╝███████║██║     ██║██║",
    "██╔══██╗██╔══██║██║     ██║██║",
    "██████╔╝██╔══██║██║██   ██║██║",
    "╚═════╝ ╚═╝  ╚═╝╚═╝╚█████╔╝╚═╝",
];

/// 渲染一帧。消耗 &mut App：滚动哨兵值在每帧渲染后钳制，帧计数驱动指示器动画。
pub fn draw(frame: &mut Frame, app: &mut App) {
    app.bump_frame();
    // [顶栏, 聊天区, 输入框, 状态栏]；斜杠补全在输入框内以灰色 ghost 呈现
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    draw_header(frame, app, chunks[0]);
    let chat = chunks[1];
    if app.is_fresh() {
        draw_welcome(frame, app, chat);
    } else {
        draw_chat(frame, app, chat);
    }
    draw_todo_panel(frame, app, chat);
    draw_input(frame, app, chunks[2]);
    draw_status(frame, app, chunks[3]);

    if let Some(rows) = app.picker_rows() {
        draw_picker(frame, app, rows);
    }
    if let Some(rows) = app.subagents_rows() {
        draw_subagents(frame, app, rows);
    }
    if app.wizard_title().is_some() {
        draw_wizard(frame, app);
    }
    // 确认弹窗最后绘制：盖在所有内容之上
    if app.pending_confirm().is_some() {
        draw_confirm(frame, app, frame.area());
    }
}

/// 顶栏一行：左侧品牌 + git 分支 + 工作目录（无仓库时回退项目名）；
/// 右侧运行指示（或厂商·模型提示）
fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let dim = Style::default().fg(theme.system);

    let mut left = vec![Span::styled(
        "baiji",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(branch) = app.git_branch() {
        left.push(Span::styled(format!("  {branch}"), dim));
        left.push(Span::styled(format!("  {}", app.workdir_display()), dim));
    } else if let Some(project) = app.project() {
        left.push(Span::styled(format!(" ⌂{project}"), dim));
    }

    let (right_text, right_style) = if app.agent_running() {
        (
            format!("{} {}", spinner(app.frame()), app.status_left()),
            Style::default().fg(theme.accent),
        )
    } else {
        (app.hint().to_string(), dim)
    };
    let right_width = display_width(&right_text) as u16;
    // 左侧过长时从尾部整段收缩（先去路径、再去分支），右侧始终完整
    let max_left = area.width.saturating_sub(right_width + 4);
    while left.len() > 1
        && left
            .iter()
            .map(|s| display_width(&s.content) as u16)
            .sum::<u16>()
            > max_left
    {
        left.pop();
    }
    let left_width: u16 = left.iter().map(|s| display_width(&s.content) as u16).sum();
    let cols = Layout::horizontal([
        Constraint::Length(left_width),
        Constraint::Fill(1),
        Constraint::Length(right_width),
    ])
    .split(area);

    frame.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    frame.render_widget(
        Paragraph::new(Line::styled(right_text, right_style)).right_aligned(),
        cols[2],
    );
}

fn spinner(frame: u64) -> char {
    SPINNER[(frame % SPINNER.len() as u64) as usize]
}

/// 聊天区逻辑行的分组键：同类连续行（如一串工具活动）聚成一块，块间空行
fn kind_of(line: &ChatLine) -> u8 {
    match line {
        ChatLine::User(_) => 0,
        ChatLine::Assistant(_) => 1,
        ChatLine::Tool(_) => 2,
        ChatLine::System(_) => 3,
    }
}

/// 用户消息：`❯ ` 加粗前缀 + 整行底色条（首行文本区宽 = width-2，续行悬挂缩进对齐）
fn user_rows(content: &str, width: usize, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
    let bar = Style::default().fg(theme.user).bg(theme.highlight);
    let prompt = bar.add_modifier(Modifier::BOLD);
    let text_width = width.saturating_sub(2);
    let wrapped = wrap_by_width_indented(content, text_width, "  ");
    let mut out = Vec::new();
    for (i, row) in wrapped.into_iter().enumerate() {
        let mut spans = Vec::new();
        if i == 0 {
            spans.push(Span::styled("❯ ", prompt));
            spans.push(Span::styled(pad_to_width(&row, text_width), bar));
        } else {
            // 续行已带 2 列悬挂缩进，整行补齐到全宽形成完整色条
            spans.push(Span::styled(pad_to_width(&row, width), bar));
        }
        out.push(Line::from(spans));
    }
    out
}

/// 工具活动行：
/// - `● Name(args)`：点亮色圆点 + 加粗名字 + 默认色参数（Claude Code 树形）
/// - `⎿ …`：结果行缩进 2 列暗色（`⎿ ✗` 失败为错误色），续行对齐内容
fn tool_rows(content: &str, width: usize, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
    let dim = Style::default().fg(theme.system);
    if content.starts_with("● ") {
        let wrapped = wrap_by_width(content, width);
        let mut out = Vec::new();
        for (i, row) in wrapped.iter().enumerate() {
            if i == 0 {
                let rest = row.strip_prefix("● ").unwrap_or(row.as_str());
                let (name, tail) = match rest.split_once('(') {
                    Some((n, t)) => (n, format!("({t}")),
                    None => (rest, String::new()),
                };
                let mut spans = vec![
                    Span::styled("● ", Style::default().fg(theme.tool)),
                    Span::styled(
                        name.to_string(),
                        Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
                    ),
                ];
                if !tail.is_empty() {
                    spans.push(Span::styled(tail, Style::default()));
                }
                out.push(Line::from(spans));
            } else {
                out.push(Line::styled(row.clone(), Style::default()));
            }
        }
        return out;
    }
    let error = content.starts_with("⎿ ✗");
    let style = if error {
        Style::default().fg(theme.error)
    } else {
        dim
    };
    wrap_by_width_indented(&format!("  {content}"), width, "    ")
        .into_iter()
        .map(|row| Line::styled(row, style))
        .collect()
}

/// 用空格补齐到目标显示宽度（已超出则原样返回）
fn pad_to_width(s: &str, target: usize) -> String {
    let current = display_width(s);
    if current >= target {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(target - current))
}

/// 聊天区：无边框、水平留白 2 列。消息样式 —
/// 用户 `❯ ` 加粗 + 整行底色条 / 助手正文灰 / 工具 `● Name(args)` 点亮名粗、
/// `⎿` 结果缩进暗色 / 系统行暗斜体
fn draw_chat(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme();
    let dim = Style::default().fg(theme.system);
    let block = Block::new().padding(Padding::horizontal(2));
    let inner = block.inner(area);
    let width = inner.width.max(1) as usize;

    // 自行按显示宽度折行成「可视行」：
    // - 消息里的换行得以保留（Line 不能含 '\n'，否则代码块会被压成一行）
    // - CJK 按 2 列计宽，行数精确 → 滚动范围精确，长中文回复能滚到底
    let mut rows: Vec<Line> = Vec::new();
    let mut prev_kind: Option<u8> = None;
    for line in app.lines() {
        let kind = kind_of(line);
        if !rows.is_empty() && prev_kind != Some(kind) {
            rows.push(Line::from(""));
        }
        prev_kind = Some(kind);
        match line {
            ChatLine::User(s) => rows.extend(user_rows(s, width, &theme)),
            ChatLine::Assistant(s) => rows.extend(
                wrap_by_width(s, width)
                    .into_iter()
                    .map(|row| Line::styled(row, Style::default().fg(theme.assistant))),
            ),
            ChatLine::Tool(s) => rows.extend(tool_rows(s, width, &theme)),
            ChatLine::System(s) => rows.extend(
                wrap_by_width(s, width)
                    .into_iter()
                    .map(|row| Line::styled(row, dim.add_modifier(Modifier::ITALIC))),
            ),
        }
    }
    // 进行中的思考（暗色斜体）：全文进入行流，与回答正文共用贴底滚动——
    // 旧实现是 400 字符固定尾部窗口，前沿不断消失像"收缩"而非滚动
    let thinking = app.thinking();
    if !thinking.trim().is_empty() {
        if !rows.is_empty() {
            rows.push(Line::from(""));
        }
        let style = dim.add_modifier(Modifier::ITALIC);
        rows.extend(
            wrap_by_width(&format!("✻ {thinking}"), width)
                .into_iter()
                .map(|row| Line::styled(row, style)),
        );
    }
    // 流式中的部分回答
    if !app.streaming().is_empty() {
        if !rows.is_empty() {
            rows.push(Line::from(""));
        }
        rows.extend(
            wrap_by_width(app.streaming(), width)
                .into_iter()
                .map(|row| Line::styled(row, Style::default().fg(theme.assistant))),
        );
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
        // 比例 thumb（viewport_content_length）：滚动进度可见——固定 1 格 thumb 看不出在滚
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut ScrollbarState::new(max_scroll + 1)
                .position(scroll)
                .viewport_content_length(visible.min(u16::MAX as usize)),
        );
    }
}

/// 空会话欢迎屏：logo + 提示键位（替代旧的首行系统提示）
fn draw_welcome(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let dim = Style::default().fg(theme.system);
    let accent = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line> = LOGO.iter().map(|row| Line::styled(*row, accent)).collect();
    lines.push(Line::from(""));
    let strings = app.strings();
    lines.push(Line::styled(strings.welcome_tagline, dim));
    lines.push(Line::from(""));
    lines.push(Line::styled(strings.welcome_hint1, dim));
    lines.push(Line::styled(strings.welcome_hint2, dim));

    let block = Block::new().padding(Padding::horizontal(2));
    let inner = block.inner(area);
    if (lines.len() as u16) > inner.height {
        return; // 窗口太矮：不硬塞
    }
    let y = inner.y + (inner.height - lines.len() as u16) / 2;
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .alignment(ratatui::layout::Alignment::Center),
        Rect {
            y,
            height: inner.height.saturating_sub(y - inner.y),
            ..area
        },
    );
}

/// 聊天区右上角悬浮 todo 面板：会话有任务清单时才出现。
/// 只覆盖聊天区的旧行（跟随底部时是历史内容），不遮挡输入与状态栏；
/// 窄终端（<50 列）不悬浮，避免大面积遮挡正文
fn draw_todo_panel(frame: &mut Frame, app: &App, chat: Rect) {
    let items = app.todo_items();
    if items.is_empty() || chat.width < 50 || chat.height < 8 {
        return;
    }
    let theme = app.theme();
    let dim = Style::default().fg(theme.system);

    let width = ((chat.width as u32 * 45 / 100).clamp(30, 44)) as u16;
    // 至多占聊天区高度一半，且 3..=10 行
    let max_rows = ((chat.height / 2).saturating_sub(2) as usize).clamp(3, 10);
    let shown = items.len().min(max_rows);
    let extra = items.len() - shown;
    let height = (shown + usize::from(extra > 0) + 2) as u16; // +2 行边框
    let area = Rect {
        x: chat.right().saturating_sub(width + 1),
        y: chat.y + 1,
        width: width.min(chat.width),
        height: height.min(chat.height),
    };
    let inner_width = area.width.saturating_sub(2).max(1) as usize;

    let mut lines: Vec<Line> = Vec::new();
    for item in &items[..shown] {
        let (mark, style, content_style) = match item.status {
            TodoStatus::Pending => ("○", dim, Style::default().fg(theme.assistant)),
            TodoStatus::InProgress => (
                "◉",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme.user),
            ),
            TodoStatus::Done => ("✓", dim, dim.add_modifier(Modifier::CROSSED_OUT)),
        };
        // 预算 = 行宽 - 标记 2 列 - 边距
        let text = truncate_to_width(&item.content, inner_width.saturating_sub(4));
        lines.push(Line::from(vec![
            Span::styled(format!("{mark} "), style),
            Span::styled(text, content_style),
        ]));
    }
    if extra > 0 {
        lines.push(Line::styled(
            crate::i18n::fill(app.strings().todo_overflow_tpl, &[&extra]),
            dim,
        ));
    }

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Todo ")
                .border_style(dim),
        ),
        area,
    );
}

/// 输入框：圆角边框 + `> ` 提示符；厂商·模型退到右下角边框标题。
/// 命令输入态在光标后以灰色 ghost 展示首个匹配命令的余下部分（Tab 接受）；
/// 运行中边框转强调色，顶边提示 steering 语义
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let dim = Style::default().fg(theme.system);
    let running = app.agent_running();

    let mut block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if running {
            Style::default().fg(theme.accent)
        } else {
            dim
        })
        .title_bottom(Line::styled(format!(" {} ", app.hint()), dim).right_aligned());
    if running {
        block = block.title_top(Line::styled(
            app.strings().input_running_title,
            Style::default().fg(theme.accent),
        ));
    } else if app.plan_mode() {
        // 计划模式静态指示：非运行态才占用顶边（运行态已有 steering 提示）
        let tip = if app.awaiting_plan() {
            app.strings().input_plan_await_title
        } else {
            app.strings().input_plan_title
        };
        block = block.title_top(Line::styled(tip, Style::default().fg(theme.accent)));
    }

    let mut spans = vec![Span::styled(
        "> ",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )];
    if app.input().is_empty() {
        spans.push(Span::styled(app.strings().input_placeholder, dim));
    }
    let app_input = app.input().to_string();
    spans.push(Span::raw(app_input.clone()));
    // ghost 显示时不再画 ▏ 光标符——补全与已输入文本的颜色边界即光标位置
    //（避免"输入与补全之间隔了一个字符"的观感）；无补全时 ▏ 指示光标
    match input_ghost(app, &app_input, area.width.saturating_sub(6) as usize) {
        Some(ghost) => spans.push(Span::styled(ghost, dim)),
        None => spans.push(Span::raw("▏")),
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

/// 输入框灰色补全文本：
/// - 裸 `/`：全部命令清单（静态 + 外部 agent），宽度内尽量多列、超限 …
/// - `/前缀`：首个匹配命令的余下部分（紧跟已输入文本，无分隔）
fn input_ghost(app: &App, input: &str, budget: usize) -> Option<String> {
    if !input.starts_with('/') {
        return None;
    }
    if input == "/" {
        let names = app.command_names();
        if names.is_empty() || budget < 8 {
            return None;
        }
        let mut line = String::new();
        let mut used = 0usize;
        for name in names {
            let extra = if line.is_empty() {
                name.len()
            } else {
                name.len() + 1
            };
            if used + extra > budget.saturating_sub(1) {
                line.push_str(" …");
                break;
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&name);
            used += extra;
        }
        return (!line.is_empty()).then_some(line);
    }
    ghost_remainder(app, input)
}

/// ghost 补全的可见部分：完整 "/name " 去掉已输入前缀（大小写不敏感比较，
/// 命令名是 ASCII，按字节切片安全）
fn ghost_remainder(app: &App, typed: &str) -> Option<String> {
    let (name, _) = app.slash_ghost()?;
    let full = format!("/{name} ");
    if full.to_lowercase().starts_with(&typed.to_lowercase()) {
        Some(full[typed.len()..].to_string())
    } else {
        None
    }
}

/// 状态栏一行：左侧运行状态（旋转指示器），右侧会话台账（暗色退后）。
/// 命令输入态时右侧临时换成 ghost 命令的用法说明
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme();
    let dim = Style::default().fg(theme.system);
    let left = if app.agent_running() {
        Line::styled(
            format!("{} {}", spinner(app.frame()), app.status_left()),
            Style::default().fg(theme.accent),
        )
    } else {
        Line::styled(app.strings().status_ready, dim)
    };
    let right = match app.slash_ghost() {
        Some((_, usage)) => Line::styled(usage.to_string(), dim),
        None => Line::styled(app.status_right(), dim),
    };
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    frame.render_widget(Paragraph::new(left), cols[0]);
    frame.render_widget(Paragraph::new(right).right_aligned(), cols[1]);
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

    let mut lines = vec![Line::styled(
        crate::i18n::fill(app.strings().confirm_warn_tpl, &[&pending.tool_name]),
        warn,
    )];
    if hidden > 0 {
        // 留一行给溢出提示
        let shown = body_height.saturating_sub(1);
        lines.extend(wrapped.iter().take(shown).cloned().map(Line::raw));
        lines.push(Line::styled(
            crate::i18n::fill(
                app.strings().confirm_hidden_tpl,
                &[&(wrapped.len() - shown)],
            ),
            warn,
        ));
    } else {
        lines.extend(wrapped.into_iter().map(Line::raw));
    }
    lines.push(Line::styled(app.strings().confirm_keys, warn));

    let widget = Paragraph::new(lines).block(
        Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Confirm execution? ")
            .border_style(Style::default().fg(theme.error)),
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(widget, popup);
}

/// 按终端显示宽度硬折行（CJK 占 2 列；制表符按 4 列；控制字符显示为 `?`，
/// 防止转义序列篡改弹窗内容）。续行无缩进
fn wrap_by_width(text: &str, width: usize) -> Vec<String> {
    wrap_by_width_indented(text, width, "")
}

/// 同 [`wrap_by_width`]，但续行带固定缩进（工具 `⎿` 结果行的悬挂缩进）
fn wrap_by_width_indented(text: &str, width: usize, cont: &str) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let cont_width: usize = cont.chars().map(|c| c.width().unwrap_or(0)).sum();
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
                line.push_str(cont);
                used = cont_width;
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

/// 字符串的终端显示宽度（CJK 计 2 列）
fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// 按显示宽度截断（超预算时以 … 结尾，保证结果宽度 ≤ budget）
fn truncate_to_width(s: &str, budget: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if budget == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > budget.saturating_sub(1) {
            return format!("{out}…");
        }
        out.push(c);
        used += w;
    }
    out
}

/// 子代理角色面板：居中 overlay（手风琴式列表 + 目录脚注）
fn draw_subagents(frame: &mut Frame, app: &App, rows: Vec<String>) {
    let theme = app.theme();
    let area = centered_rect(frame.area(), 75, 60);

    let items: Vec<ListItem> = rows
        .into_iter()
        .map(|row| ListItem::new(Line::from(row)))
        .collect();
    let list = List::new(items)
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Subagents (↑↓ select · r reload · Esc close) ")
                .border_style(Style::default().fg(theme.accent)),
        )
        .highlight_style(
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_widget(Clear, area);
    let mut state = ListState::default();
    if let Some(selected) = app.subagents_selected() {
        state.select(Some(selected));
    }
    frame.render_stateful_widget(list, area, &mut state);

    // 目录脚注贴在列表框下方
    if let Some(hint) = app.subagents_dirs_hint() {
        let footer = Rect {
            y: area.bottom(),
            height: 1.min(frame.area().bottom().saturating_sub(area.bottom())),
            x: area.x,
            width: area.width,
        };
        if footer.height > 0 {
            frame.render_widget(
                Paragraph::new(Line::styled(hint, Style::default().fg(theme.system))),
                footer,
            );
        }
    }
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
                .border_type(BorderType::Rounded)
                .title(" Sessions (Enter=switch b=fork Esc=close) ")
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
    let Some(title) = app.wizard_title() else {
        return;
    };

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
                        .border_type(BorderType::Rounded)
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
            let Some(prompt) = app.wizard_input_prompt() else {
                return;
            };
            let paragraph = Paragraph::new(vec![
                Line::styled(prompt.clone(), Style::default().fg(theme.tool)),
                Line::from(format!("{}▏", app.input())),
            ])
            .block(
                Block::new()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
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

    #[test]
    fn test_wrap_indented_continuation() {
        // 首行无缩进，续行带 2 列悬挂缩进
        let wrapped = wrap_by_width_indented("⎿ abcdefghijklmn", 10, "  ");
        assert_eq!(wrapped[0], "⎿ abcdefgh");
        assert_eq!(wrapped[1], "  ijklmn");
    }

    #[test]
    fn test_truncate_to_width() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("hello", 4), "hel…");
        // CJK 按显示宽度计
        assert_eq!(truncate_to_width("你好世界", 5), "你好…");
        assert_eq!(truncate_to_width("anything", 0), "");
    }

    #[test]
    fn test_display_width_cjk() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("你好"), 4);
    }

    #[test]
    fn test_pad_to_width() {
        assert_eq!(pad_to_width("ab", 5), "ab   ");
        // CJK 按显示宽度补
        assert_eq!(pad_to_width("你好", 6), "你好  ");
        assert_eq!(pad_to_width("abcdef", 3), "abcdef");
    }
}
