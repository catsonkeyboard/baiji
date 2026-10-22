//! 助手正文的 Markdown 渲染：pulldown-cmark 事件流 → 按显示宽度折行的 ratatui 行。
//!
//! 覆盖：标题（粗体 + 强调色）、粗/斜/删除线、行内代码（代码色）、围栏/缩进代码块
//! （`┌─ lang` / `│ ` / `└─` 外框，单色、不做语法高亮）、有序/无序/任务列表（续行与
//! 标记后的正文对齐）、引用（`│` 竖线）、分隔线、简单表格（按显示宽度定列宽，过宽截断）、
//! 链接 `text (url)`（文本与地址相同时只显示文本）。
//!
//! 约定：
//! - 软换行按硬换行处理：LLM 输出里的单换行几乎总是有意的，CJK 段落里补空格也难看；
//!   顶层段落续行被解析器剥掉的前导缩进会恢复（纯文本伪代码 / 树形输出不被压平）
//! - 流式中未闭合的围栏：解析器把余下全文视为代码块，照常带框渲染
//! - 折行按显示宽度（CJK 计 2 列，可在任意两字之间断行；西文按词断行，超长词硬切），
//!   制表符展开为 4 空格，控制字符显示为 `?`（防止转义序列篡改界面）

use crate::theme::Theme;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::ops::Range;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 渲染 Markdown 文本为可视行（`width` 为可用显示宽度，不含聊天区两侧留白）
pub fn render(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut renderer = Renderer::new(width.max(1), theme);
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        renderer.event(event, range, text);
    }
    renderer.finish()
}

/// 块级容器：决定每个可视行的前缀
enum Container {
    /// 引用：`│ `
    Quote,
    /// 列表项：首行画标记，其余行用等宽空格对齐正文
    Item {
        marker: String,
        style: Style,
        used: bool,
        ordered: bool,
    },
}

/// 表格累积状态（单元格保留行内样式）
struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    head_rows: usize,
    row: Vec<Vec<Span<'static>>>,
    cell: Vec<Span<'static>>,
}

struct Renderer<'t> {
    theme: &'t Theme,
    width: usize,
    out: Vec<Line<'static>>,
    containers: Vec<Container>,
    /// 列表栈：有序列表的下一个序号（None = 无序）
    lists: Vec<Option<u64>>,
    /// 当前逻辑行的行内片段（遇换行/块结束时折行输出）
    inline: Vec<Span<'static>>,
    bold: usize,
    italic: usize,
    strike: usize,
    /// 链接栈：(地址, 已累积的链接文本)
    links: Vec<(String, String)>,
    heading: Option<HeadingLevel>,
    /// 代码块：(语言, 正文)
    code: Option<(String, String)>,
    table: Option<Table>,
    /// 下一个块开始前补一个空行
    pending_blank: bool,
    /// 待补空行来自列表结束（紧凑列表里嵌套列表后的兄弟项之间不补）
    blank_after_list: bool,
    /// 上一个软/硬换行的源码结束偏移（恢复续行的前导缩进）
    break_end: Option<usize>,
}

impl<'t> Renderer<'t> {
    fn new(width: usize, theme: &'t Theme) -> Self {
        Self {
            theme,
            width,
            out: Vec::new(),
            containers: Vec::new(),
            lists: Vec::new(),
            inline: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            links: Vec::new(),
            heading: None,
            code: None,
            table: None,
            pending_blank: false,
            blank_after_list: false,
            break_end: None,
        }
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>, src: &str) {
        if let Some(end) = self.break_end.take()
            && !matches!(event, Event::End(_) | Event::SoftBreak | Event::HardBreak)
        {
            self.restore_indent(end, range.start, src);
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, body)) = &mut self.code {
                    body.push_str(&text);
                } else {
                    self.push_text(&text, self.style());
                }
            }
            Event::Code(code) => {
                let mut style = Style::default().fg(self.theme.code);
                if self.heading.is_some() || self.bold > 0 {
                    style = style.add_modifier(Modifier::BOLD);
                }
                self.push_text(&code, style);
            }
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(&html, self.style()),
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                self.push_text(&math, self.style())
            }
            Event::FootnoteReference(label) => self.push_text(&format!("[{label}]"), self.style()),
            Event::SoftBreak | Event::HardBreak => {
                if self.table.is_some() {
                    self.push_text(" ", self.style());
                } else {
                    self.flush_line(true);
                    self.break_end = Some(range.end);
                }
            }
            Event::Rule => {
                self.start_block();
                let rule = "─".repeat(self.avail());
                self.emit_row(vec![Span::styled(rule, self.dim())]);
                self.end_block();
            }
            Event::TaskListMarker(checked) => self.task_marker(checked),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.start_block(),
            Tag::Heading { level, .. } => {
                self.start_block();
                self.heading = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.start_block();
                self.containers.push(Container::Quote);
            }
            Tag::CodeBlock(kind) => {
                self.start_block();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.start_block();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush_line(false);
                if self.pending_blank && !self.blank_after_list && !self.out.is_empty() {
                    self.emit_blank();
                }
                self.pending_blank = false;
                let depth = self.lists.len().saturating_sub(1);
                let (marker, ordered) = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        (marker, true)
                    }
                    _ => (
                        match depth {
                            0 => "• ",
                            1 => "◦ ",
                            _ => "▪ ",
                        }
                        .to_string(),
                        false,
                    ),
                };
                self.containers.push(Container::Item {
                    marker,
                    style: Style::default().fg(self.theme.accent),
                    used: false,
                    ordered,
                });
            }
            Tag::Table(aligns) => {
                self.start_block();
                self.table = Some(Table {
                    aligns,
                    rows: Vec::new(),
                    head_rows: 0,
                    row: Vec::new(),
                    cell: Vec::new(),
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.cell.clear();
                }
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.links.push((dest_url.to_string(), String::new()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.end_block(),
            TagEnd::Heading(_) => {
                self.flush_line(false);
                self.heading = None;
                self.end_block();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line(false);
                self.containers.pop();
                self.pending_blank = true;
                self.blank_after_list = false;
            }
            TagEnd::CodeBlock => {
                if let Some((lang, body)) = self.code.take() {
                    self.code_block(&lang, &body);
                }
                self.end_block();
            }
            TagEnd::List(_) => {
                self.flush_line(false);
                self.lists.pop();
                self.pending_blank = true;
                self.blank_after_list = true;
            }
            TagEnd::Item => {
                self.flush_line(false);
                // 空列表项（`- `）也要画出标记
                if let Some(Container::Item { used: false, .. }) = self.containers.last() {
                    self.emit_row(Vec::new());
                }
                self.containers.pop();
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    let cell = std::mem::take(&mut table.cell);
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                    table.head_rows = table.rows.len();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.render_table(table);
                }
                self.end_block();
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link | TagEnd::Image => {
                if let Some((url, text)) = self.links.pop() {
                    let same = url == text || url.strip_prefix("mailto:") == Some(text.as_str());
                    if !url.is_empty() && !same {
                        self.push_text(&format!(" ({url})"), self.dim());
                    }
                }
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line(false);
        if let Some((lang, body)) = self.code.take() {
            self.code_block(&lang, &body);
        }
        if let Some(table) = self.table.take() {
            self.render_table(table);
        }
        self.out
    }

    // ---- 样式 ----

    fn dim(&self) -> Style {
        Style::default().fg(self.theme.system)
    }

    /// 当前行内样式（标题 / 粗斜删 / 链接叠加在助手正文色上）
    fn style(&self) -> Style {
        let theme = self.theme;
        let mut style = Style::default().fg(theme.assistant);
        if let Some(level) = self.heading {
            style = style.add_modifier(Modifier::BOLD).fg(match level {
                HeadingLevel::H1 | HeadingLevel::H2 => theme.accent,
                _ => theme.user,
            });
            if level == HeadingLevel::H1 {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
        }
        if self.bold > 0 {
            style = style.add_modifier(Modifier::BOLD);
            if self.heading.is_none() {
                style = style.fg(theme.user);
            }
        }
        if self.italic > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if !self.links.is_empty() {
            style = style.fg(theme.accent).add_modifier(Modifier::UNDERLINED);
        }
        style
    }

    // ---- 行内累积 ----

    /// 追加行内文本（文本里的换行拆成逻辑行；表格单元格内并入单元格）
    fn push_text(&mut self, text: &str, style: Style) {
        if let Some((_, link_text)) = self.links.last_mut() {
            link_text.push_str(text);
        }
        if let Some(table) = &mut self.table {
            table
                .cell
                .push(Span::styled(text.replace('\n', " "), style));
            return;
        }
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                self.inline.push(Span::styled(part.to_string(), style));
            }
            if parts.peek().is_some() {
                self.flush_line(false);
            }
        }
    }

    /// 顶层段落续行：恢复被解析器剥掉的前导空白（列表/引用里的缩进属于容器语法，不恢复）
    fn restore_indent(&mut self, from: usize, to: usize, src: &str) {
        if !self.containers.is_empty() || self.table.is_some() || self.code.is_some() {
            return;
        }
        if let Some(gap) = src.get(from..to)
            && !gap.is_empty()
            && gap.chars().all(|c| c == ' ' || c == '\t')
        {
            self.inline
                .push(Span::styled(gap.to_string(), self.style()));
        }
    }

    fn task_marker(&mut self, checked: bool) {
        let (text, style) = if checked {
            ("☑ ", Style::default().fg(self.theme.accent))
        } else {
            ("☐ ", self.dim())
        };
        match self.containers.last_mut() {
            // 无序任务项：复选框替换圆点（续行对齐不变：同为 2 列）
            Some(Container::Item {
                marker,
                style: marker_style,
                used: false,
                ordered: false,
            }) => {
                *marker = text.to_string();
                *marker_style = style;
            }
            _ => self.inline.push(Span::styled(text, style)),
        }
    }

    // ---- 块与输出 ----

    fn start_block(&mut self) {
        self.flush_line(false);
        if self.pending_blank && !self.out.is_empty() {
            self.emit_blank();
        }
        self.pending_blank = false;
        self.blank_after_list = false;
    }

    fn end_block(&mut self) {
        self.flush_line(false);
        self.pending_blank = true;
        self.blank_after_list = false;
    }

    /// 当前容器前缀宽度
    fn prefix_width(&self) -> usize {
        self.containers
            .iter()
            .map(|c| match c {
                Container::Quote => 2,
                Container::Item { marker, .. } => marker.width(),
            })
            .sum()
    }

    /// 正文可用宽度
    fn avail(&self) -> usize {
        self.width.saturating_sub(self.prefix_width()).max(1)
    }

    /// 前缀片段：`consume` 时未画过的列表标记在本行画出并标记已用
    fn prefix(&mut self, consume: bool) -> Vec<Span<'static>> {
        let dim = self.dim();
        let mut spans = Vec::new();
        for container in &mut self.containers {
            match container {
                Container::Quote => spans.push(Span::styled("│ ", dim)),
                Container::Item {
                    marker,
                    style,
                    used,
                    ..
                } => {
                    if consume && !*used {
                        spans.push(Span::styled(marker.clone(), *style));
                        *used = true;
                    } else {
                        spans.push(Span::raw(" ".repeat(marker.width())));
                    }
                }
            }
        }
        spans
    }

    fn emit_row(&mut self, content: Vec<Span<'static>>) {
        let mut spans = self.prefix(true);
        spans.extend(content);
        self.out.push(Line::from(spans));
    }

    /// 块间空行：引用内保留竖线，其余前缀去掉尾随空白
    fn emit_blank(&mut self) {
        let mut spans = self.prefix(false);
        while spans.last().is_some_and(|s| s.content.trim().is_empty()) {
            spans.pop();
        }
        if let Some(last) = spans.last_mut() {
            *last = Span::styled(last.content.trim_end().to_string(), last.style);
        }
        self.out.push(Line::from(spans));
    }

    /// 当前逻辑行折行输出。`force`：空行也输出（连续换行）
    fn flush_line(&mut self, force: bool) {
        if self.inline.is_empty() {
            if force {
                self.emit_row(Vec::new());
            }
            return;
        }
        let spans = std::mem::take(&mut self.inline);
        for row in wrap_spans(&spans, self.avail()) {
            self.emit_row(row);
        }
    }

    fn code_block(&mut self, lang: &str, body: &str) {
        let frame = self.dim();
        let code = Style::default().fg(self.theme.code);
        let header = if lang.is_empty() {
            "┌─".to_string()
        } else {
            format!("┌─ {lang}")
        };
        self.emit_row(vec![Span::styled(header, frame)]);
        let inner = self.avail().saturating_sub(2).max(1);
        let body = body.strip_suffix('\n').unwrap_or(body);
        if !body.is_empty() {
            for line in body.split('\n') {
                for piece in hard_wrap(line.trim_end_matches('\r'), inner) {
                    self.emit_row(vec![Span::styled("│ ", frame), Span::styled(piece, code)]);
                }
            }
        }
        self.emit_row(vec![Span::styled("└─", frame)]);
    }

    fn render_table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let cell_width = |cell: &[Span<'static>]| -> usize {
            cell.iter().map(|s| sanitize(&s.content).width()).sum()
        };
        let mut widths = vec![1usize; columns];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell_width(cell));
            }
        }
        // 过宽：反复收窄最宽列（每列至少 3 列宽，仍放不下则交给区域裁剪）
        let separators = 3 * (columns - 1);
        let avail = self.avail();
        while widths.iter().sum::<usize>() + separators > avail {
            let Some((i, w)) = widths.iter().copied().enumerate().max_by_key(|(_, w)| *w) else {
                break;
            };
            if w <= 3 {
                break;
            }
            widths[i] -= 1;
        }
        let dim = self.dim();
        let empty: Vec<Span<'static>> = Vec::new();
        for (r, row) in table.rows.iter().enumerate() {
            let header = r < table.head_rows;
            let mut spans = Vec::new();
            for (c, width) in widths.iter().enumerate() {
                if c > 0 {
                    spans.push(Span::styled(" │ ", dim));
                }
                let cell = row.get(c).unwrap_or(&empty);
                let cell: Vec<Span<'static>> = cell
                    .iter()
                    .map(|s| {
                        let style = if header {
                            s.style.add_modifier(Modifier::BOLD)
                        } else {
                            s.style
                        };
                        Span::styled(sanitize(&s.content), style)
                    })
                    .collect();
                let align = table.aligns.get(c).copied().unwrap_or(Alignment::None);
                spans.extend(fit_spans(cell, *width, align));
            }
            self.emit_row(spans);
            if header && r + 1 == table.head_rows {
                let rule = widths
                    .iter()
                    .map(|w| "─".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("─┼─");
                self.emit_row(vec![Span::styled(rule, dim)]);
            }
        }
    }
}

/// 单元格定宽：超宽截断（`…` 结尾），不足按对齐方式补空格
fn fit_spans(spans: Vec<Span<'static>>, width: usize, align: Alignment) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| s.content.width()).sum();
    if total > width {
        return truncate_spans(spans, width);
    }
    let pad = width - total;
    let (left, right) = match align {
        Alignment::Right => (pad, 0),
        Alignment::Center => (pad / 2, pad - pad / 2),
        Alignment::Left | Alignment::None => (0, pad),
    };
    let mut out = Vec::with_capacity(spans.len() + 2);
    if left > 0 {
        out.push(Span::raw(" ".repeat(left)));
    }
    out.extend(spans);
    if right > 0 {
        out.push(Span::raw(" ".repeat(right)));
    }
    out
}

/// 按显示宽度截断样式片段（结果宽度恰好 ≤ width，末尾 `…`）
fn truncate_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let budget = width - 1;
    let mut used = 0usize;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut last_style = Style::default();
    'outer: for span in spans {
        last_style = span.style;
        let mut piece = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > budget {
                if !piece.is_empty() {
                    out.push(Span::styled(piece, span.style));
                }
                break 'outer;
            }
            used += w;
            piece.push(c);
        }
        if !piece.is_empty() {
            out.push(Span::styled(piece, span.style));
        }
    }
    out.push(Span::styled("…", last_style));
    // 宽字符截断可能留下 1 列空缺：补齐，保证列对齐
    if used + 1 < width {
        out.push(Span::raw(" ".repeat(width - used - 1)));
    }
    out
}

/// 显示用净化：制表符 → 4 空格，控制字符 → `?`
pub(crate) fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => out.push('?'),
            c => out.push(c),
        }
    }
    out
}

/// 单行文本按显示宽度逐字符硬切（代码块 / diff 折行；先净化，至少返回一段）
pub(crate) fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    let mut used = 0usize;
    for c in sanitize(text).chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && !line.is_empty() {
            out.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push(c);
        used += w;
    }
    out.push(line);
    out
}

/// 样式感知的按显示宽度折行（一条逻辑行 → 若干可视行）。
/// 西文按词断行（超长词硬切），CJK 等宽字符可在任意两字之间断行；
/// 折出的续行丢弃行首空格，首行保留（缩进有意义）
pub(crate) fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    // 1) 拍平为 (字符, 样式, 宽度)
    let mut cells: Vec<(char, Style, usize)> = Vec::new();
    for span in spans {
        for c in span.content.chars() {
            match c {
                '\t' => cells.extend(std::iter::repeat_n((' ', span.style, 1), 4)),
                c if c.is_control() => cells.push(('?', span.style, 1)),
                c => cells.push((c, span.style, c.width().unwrap_or(0))),
            }
        }
    }

    // 2) 切词：连续空格 / 连续窄字符（一个词）/ 单个宽字符（附带其后的零宽字符）
    struct Token {
        range: Range<usize>,
        width: usize,
        space: bool,
    }
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0;
    while i < cells.len() {
        let start = i;
        let (c, _, w) = cells[i];
        if c == ' ' {
            while i < cells.len() && cells[i].0 == ' ' {
                i += 1;
            }
            tokens.push(Token {
                range: start..i,
                width: i - start,
                space: true,
            });
        } else if w >= 2 {
            i += 1;
            while i < cells.len() && cells[i].2 == 0 && cells[i].0 != ' ' {
                i += 1;
            }
            tokens.push(Token {
                range: start..i,
                width: w,
                space: false,
            });
        } else {
            let mut tw = 0;
            while i < cells.len() && cells[i].0 != ' ' && cells[i].2 < 2 {
                tw += cells[i].2;
                i += 1;
            }
            tokens.push(Token {
                range: start..i,
                width: tw,
                space: false,
            });
        }
    }

    // 3) 贪心放置
    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0usize;
    fn trim_trailing_spaces(line: &mut Vec<(char, Style)>) {
        while line.last().is_some_and(|(c, _)| *c == ' ') {
            line.pop();
        }
    }
    for token in tokens {
        if token.space {
            if cur.is_empty() && !lines.is_empty() {
                continue; // 续行行首空格
            }
            if cur_w + token.width > width {
                if !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                continue; // 行尾空格随断行丢弃
            }
            cur.extend(cells[token.range].iter().map(|(c, s, _)| (*c, *s)));
            cur_w += token.width;
            continue;
        }
        if cur_w + token.width > width && !cur.is_empty() {
            trim_trailing_spaces(&mut cur);
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if token.width > width {
            for &(c, style, w) in &cells[token.range] {
                if cur_w + w > width && !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push((c, style));
                cur_w += w;
            }
        } else {
            cur.extend(cells[token.range].iter().map(|(c, s, _)| (*c, *s)));
            cur_w += token.width;
        }
    }
    trim_trailing_spaces(&mut cur);
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }

    // 4) 同样式相邻字符合并回片段
    lines
        .into_iter()
        .map(|line| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut text = String::new();
            let mut style: Option<Style> = None;
            for (c, s) in line {
                if style.is_some_and(|cur| cur != s) {
                    spans.push(Span::styled(
                        std::mem::take(&mut text),
                        style.unwrap_or_default(),
                    ));
                }
                style = Some(s);
                text.push(c);
            }
            if !text.is_empty() {
                spans.push(Span::styled(text, style.unwrap_or_default()));
            }
            spans
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn md(text: &str, width: usize) -> Vec<String> {
        texts(&render(text, width, &Theme::dark()))
    }

    /// 找到包含 needle 的片段的样式
    fn style_of(lines: &[Line], needle: &str) -> Style {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(needle))
            .map(|s| s.style)
            .unwrap_or_else(|| panic!("no span contains {needle:?}"))
    }

    #[test]
    fn test_paragraphs_and_soft_breaks() {
        assert_eq!(md("hello world", 80), vec!["hello world"]);
        // 段落间空行；软换行保留为换行
        assert_eq!(md("a\nb\n\nc", 80), vec!["a", "b", "", "c"]);
        // 顶层续行缩进恢复（纯文本伪代码不被压平）
        assert_eq!(
            md("fn main() {\n    hi();\n}", 80),
            vec!["fn main() {", "    hi();", "}"]
        );
    }

    #[test]
    fn test_word_wrap_and_cjk() {
        assert_eq!(md("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        // CJK 按 2 列计宽，任意两字之间可断
        assert_eq!(md("你好世界你好", 8), vec!["你好世界", "你好"]);
        // 中英混排：西文词不被拆开
        assert_eq!(md("中文 hello 世界", 6), vec!["中文", "hello", "世界"]);
        // 超长词硬切
        assert_eq!(md("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        for row in render(&"测试 wrap 宽度 ".repeat(30), 13, &Theme::dark()) {
            assert!(row.width() <= 13, "row too wide: {row:?}");
        }
    }

    #[test]
    fn test_headings_and_inline_styles() {
        let theme = Theme::dark();
        let lines = render(
            "# Title\n\nsome **bold** and *ital* and ~~gone~~ and `code`",
            80,
            &theme,
        );
        assert_eq!(
            texts(&lines),
            vec!["Title", "", "some bold and ital and gone and code"]
        );
        let title = style_of(&lines, "Title");
        assert_eq!(title.fg, Some(theme.accent));
        assert!(title.add_modifier.contains(Modifier::BOLD));
        assert!(
            style_of(&lines, "bold")
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            style_of(&lines, "ital")
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert!(
            style_of(&lines, "gone")
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert_eq!(style_of(&lines, "code").fg, Some(theme.code));
        assert_eq!(style_of(&lines, "some").fg, Some(theme.assistant));
        // 样式跨折行保持
        let lines = render("**aaaa bbbb**", 4, &theme);
        assert_eq!(texts(&lines), vec!["aaaa", "bbbb"]);
        assert!(
            style_of(&lines, "bbbb")
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn test_code_blocks() {
        assert_eq!(
            md("```rust\nfn main() {\n    hi();\n}\n```", 80),
            vec!["┌─ rust", "│ fn main() {", "│     hi();", "│ }", "└─"]
        );
        // 缩进代码块（无语言）
        assert_eq!(
            md("text\n\n    let x = 1;\n", 80),
            vec!["text", "", "┌─", "│ let x = 1;", "└─"]
        );
        // 长代码行折行，前缀保留
        assert_eq!(
            md("```\nabcdefghij\n```", 6),
            vec!["┌─", "│ abcd", "│ efgh", "│ ij", "└─"]
        );
        // 代码块内的空行保留，不做 Markdown 解析
        assert_eq!(
            md("```\na\n\n**b**\n```", 80),
            vec!["┌─", "│ a", "│ ", "│ **b**", "└─"]
        );
        let theme = Theme::dark();
        let lines = render("```\nx\n```", 80, &theme);
        assert_eq!(style_of(&lines, "x").fg, Some(theme.code));
    }

    #[test]
    fn test_unclosed_fence_mid_stream() {
        // 流式中围栏尚未闭合：余下全文作为代码块渲染，正文不被当作 Markdown
        assert_eq!(
            md("Here:\n\n```py\nprint('hi')\n# not a heading", 80),
            vec![
                "Here:",
                "",
                "┌─ py",
                "│ print('hi')",
                "│ # not a heading",
                "└─"
            ]
        );
        // 刚打出开围栏
        assert_eq!(md("Here:\n\n```", 80), vec!["Here:", "", "┌─", "└─"]);
    }

    #[test]
    fn test_lists() {
        assert_eq!(md("- a\n- b", 80), vec!["• a", "• b"]);
        assert_eq!(md("1. one\n2. two", 80), vec!["1. one", "2. two"]);
        // 起始序号
        assert_eq!(md("3. x\n4. y", 80), vec!["3. x", "4. y"]);
        // 嵌套 + 续行对齐
        assert_eq!(
            md("- outer\n  - inner\n- next", 80),
            vec!["• outer", "  ◦ inner", "• next"]
        );
        assert_eq!(
            md("- aaaa bbbb cccc", 8),
            vec!["• aaaa", "  bbbb", "  cccc"]
        );
        assert_eq!(md("10. ten\n    more", 80), vec!["10. ten", "    more"]);
        // 任务列表
        assert_eq!(md("- [ ] todo\n- [x] done", 80), vec!["☐ todo", "☑ done"]);
        // 宽松列表：项间空行；列表后接段落空一行
        assert_eq!(
            md("- a\n\n- b\n\nafter", 80),
            vec!["• a", "", "• b", "", "after"]
        );
        // 列表项内的代码块挂在标记之后
        assert_eq!(
            md("- step:\n\n  ```\n  run\n  ```", 80),
            vec!["• step:", "", "  ┌─", "  │ run", "  └─"]
        );
    }

    #[test]
    fn test_blockquote_rule_and_links() {
        assert_eq!(md("> quoted\n> more", 80), vec!["│ quoted", "│ more"]);
        assert_eq!(md("> a\n>\n> b", 80), vec!["│ a", "│", "│ b"]);
        assert_eq!(md("> aaaa bbbb", 8), vec!["│ aaaa", "│ bbbb"]);
        assert_eq!(md("a\n\n---\n\nb", 5), vec!["a", "", "─────", "", "b"]);
        assert_eq!(
            md("see [docs](https://x.dev/d) now", 80),
            vec!["see docs (https://x.dev/d) now"]
        );
        // 文本与地址相同：只显示一次
        assert_eq!(md("<https://x.dev>", 80), vec!["https://x.dev"]);
        let theme = Theme::dark();
        let lines = render("[docs](https://x.dev)", 80, &theme);
        assert!(
            style_of(&lines, "docs")
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert_eq!(style_of(&lines, "(https://x.dev)").fg, Some(theme.system));
    }

    #[test]
    fn test_tables() {
        let rows = md(
            "| Name | 数量 |\n|---|--:|\n| apple | 3 |\n| 香蕉 | 12 |",
            80,
        );
        assert_eq!(
            rows,
            vec![
                "Name  │ 数量",
                "──────┼─────",
                "apple │    3",
                "香蕉  │   12",
            ]
        );
        // 过宽：收窄最宽列并截断
        let rows = md("| a | b |\n|---|---|\n| aaaaaaaaaaaaaaaaaaaa | bb |", 12);
        assert!(rows.iter().all(|r| r.width() <= 12), "{rows:?}");
        assert!(rows[2].contains('…'));
        let theme = Theme::dark();
        let lines = render("| H |\n|---|\n| v |", 80, &theme);
        assert!(style_of(&lines, "H").add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_control_chars_and_tabs_neutralized() {
        assert_eq!(md("a\u{1b}[2Jb", 80), vec!["a?[2Jb"]);
        assert_eq!(hard_wrap("\tx", 80), vec!["    x"]);
        assert_eq!(hard_wrap("", 5), vec![""]);
        assert_eq!(hard_wrap("你好世界", 5), vec!["你好", "世界"]);
    }

    #[test]
    fn test_wrap_spans_keeps_styles() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let spans = vec![
            Span::raw("ab "),
            Span::styled("cd", bold),
            Span::raw("ef gh"),
        ];
        let rows = wrap_spans(&spans, 6);
        let flat: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(flat, vec!["ab", "cdef", "gh"]);
        assert_eq!(rows[1][0].style, bold);
        assert_eq!(rows[1][1].content, "ef");
        // 首行前导空格保留，空输入得到一个空行
        assert_eq!(wrap_spans(&[Span::raw("  x")], 10)[0][0].content, "  x");
        assert_eq!(wrap_spans(&[], 10).len(), 1);
    }
}
