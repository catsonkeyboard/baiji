//! 多行输入框（composer）缓冲区：文本 + 光标 + 粘贴占位符。
//!
//! - 光标是字节下标，恒落在 char 边界——所有编辑操作对 CJK 等多字节字符安全
//! - 可视行布局按终端显示宽度折行（CJK 计 2 列），渲染、光标定位、↑↓ 移动共用同一套布局
//! - ≥5 行的粘贴在缓冲区里显示为占位符（`[pasted N lines]`），提交时展开回原文
//! - 缓冲区内不存控制字符（换行除外）：写入时统一净化，渲染不会被转义序列篡改

use unicode_width::UnicodeWidthChar;

/// 粘贴折叠为占位符的行数阈值（≥ 此值显示占位符）
pub(crate) const PASTE_PLACEHOLDER_MIN_LINES: usize = 5;

#[derive(Debug, Clone, Default)]
pub(crate) struct InputBuffer {
    text: String,
    /// 光标字节下标（char 边界）
    cursor: usize,
    /// (占位符, 原文)，按粘贴顺序；占位符在缓冲区内唯一
    pastes: Vec<(String, String)>,
}

impl InputBuffer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
    }

    /// 整体替换内容（历史召回 / Tab 补全 / fork 回填），光标置尾，占位符作废
    pub(crate) fn set(&mut self, text: impl AsRef<str>) {
        self.text = sanitize(text.as_ref());
        self.cursor = self.text.len();
        self.pastes.clear();
    }

    /// 设置光标（钳制到末尾并对齐到 char 边界）
    pub(crate) fn set_cursor(&mut self, pos: usize) {
        let mut pos = pos.min(self.text.len());
        while !self.text.is_char_boundary(pos) {
            pos -= 1;
        }
        self.cursor = pos;
    }

    pub(crate) fn insert_char(&mut self, c: char) {
        if c == '\t' {
            self.insert_str("    ");
            return;
        }
        if c.is_control() && c != '\n' {
            return;
        }
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub(crate) fn insert_str(&mut self, s: &str) {
        let clean = sanitize(s);
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    pub(crate) fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// 退格：光标前一个字符；光标恰在占位符末尾时整体删除该占位符（连同原文）
    pub(crate) fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let before = &self.text[..self.cursor];
        if let Some(idx) = self
            .pastes
            .iter()
            .position(|(placeholder, _)| before.ends_with(placeholder.as_str()))
        {
            let start = self.cursor - self.pastes[idx].0.len();
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
            self.pastes.remove(idx);
            return true;
        }
        let prev = prev_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        true
    }

    /// Delete：光标后一个字符
    pub(crate) fn delete(&mut self) -> bool {
        if self.cursor >= self.text.len() {
            return false;
        }
        let next = next_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
        true
    }

    pub(crate) fn move_left(&mut self) {
        self.cursor = prev_boundary(&self.text, self.cursor);
    }

    pub(crate) fn move_right(&mut self) {
        self.cursor = next_boundary(&self.text, self.cursor);
    }

    /// 当前逻辑行行首（Home / Ctrl+A）
    pub(crate) fn move_line_start(&mut self) {
        self.cursor = self.line_start();
    }

    /// 当前逻辑行行尾（End / Ctrl+E）
    pub(crate) fn move_line_end(&mut self) {
        self.cursor = self.line_end();
    }

    pub(crate) fn move_buffer_start(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn move_buffer_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// 向左跳词（Alt/Ctrl+←、Alt+B）：先跳过非词字符，再跳过词字符
    pub(crate) fn move_word_left(&mut self) {
        self.cursor = word_left(&self.text, self.cursor);
    }

    /// 向右跳词（Alt/Ctrl+→、Alt+F）：先跳过非词字符，再落到词尾
    pub(crate) fn move_word_right(&mut self) {
        let mut pos = self.cursor;
        let len = self.text.len();
        while pos < len && !is_word_char(char_at(&self.text, pos)) {
            pos = next_boundary(&self.text, pos);
        }
        while pos < len && is_word_char(char_at(&self.text, pos)) {
            pos = next_boundary(&self.text, pos);
        }
        self.cursor = pos;
    }

    /// Ctrl+U：删到行首；已在行首时删掉前一个换行（与上一行合并）
    pub(crate) fn delete_to_line_start(&mut self) -> bool {
        let start = self.line_start();
        if start == self.cursor {
            return self.backspace_newline();
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        true
    }

    /// Ctrl+K：删到行尾；已在行尾时删掉该换行（与下一行合并）
    pub(crate) fn delete_to_line_end(&mut self) -> bool {
        let end = self.line_end();
        if end == self.cursor {
            return self.delete();
        }
        self.text.replace_range(self.cursor..end, "");
        true
    }

    /// Ctrl+W：按空白分词向前删一个"词"（bash unix-word-rubout 语义，路径整段删）；
    /// 紧邻光标的是换行时只删换行
    pub(crate) fn delete_word_back(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        if self.backspace_newline() {
            return true;
        }
        let mut pos = self.cursor;
        while pos > 0 {
            let prev = prev_boundary(&self.text, pos);
            let c = char_at(&self.text, prev);
            if c == '\n' || !c.is_whitespace() {
                break;
            }
            pos = prev;
        }
        while pos > 0 {
            let prev = prev_boundary(&self.text, pos);
            if char_at(&self.text, prev).is_whitespace() {
                break;
            }
            pos = prev;
        }
        self.text.replace_range(pos..self.cursor, "");
        self.cursor = pos;
        true
    }

    /// Alt/Ctrl+Backspace：按词边界向前删（停在标点处，同 Alt+← 的跳词规则）
    pub(crate) fn delete_word_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = word_left(&self.text, self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        true
    }

    /// 行尾反斜杠续行：光标在末尾且末尾是奇数个 `\` 时，去掉一个 `\` 并插入换行。
    /// 偶数个（`\\`，转义的字面反斜杠）照常提交。
    pub(crate) fn consume_backslash_continuation(&mut self) -> bool {
        if self.cursor != self.text.len() || !self.text.ends_with('\\') {
            return false;
        }
        let trailing = self.text.len() - self.text.trim_end_matches('\\').len();
        if trailing.is_multiple_of(2) {
            return false;
        }
        self.text.pop();
        self.cursor = self.text.len();
        self.insert_newline();
        true
    }

    /// 粘贴：≥5 行折叠成占位符（原文另存，提交时展开），否则原样插入光标处。
    /// `placeholder_tpl` 是含一个 `{}`（行数）的 i18n 模板；同名占位符追加 `#n` 去重
    pub(crate) fn insert_paste(&mut self, text: &str, placeholder_tpl: &str) {
        let text = sanitize(text);
        let lines = text.lines().count().max(1);
        if lines < PASTE_PLACEHOLDER_MIN_LINES {
            self.insert_str(&text);
            return;
        }
        // 已被用户删掉的占位符对应的原文不再保留
        let current = self.text.clone();
        self.pastes
            .retain(|(placeholder, _)| current.contains(placeholder.as_str()));
        let base = crate::i18n::fill(placeholder_tpl, &[&lines]);
        let mut placeholder = base.clone();
        let mut n = 2;
        while self.pastes.iter().any(|(p, _)| *p == placeholder) || current.contains(&placeholder) {
            placeholder = match base.strip_suffix(']') {
                Some(head) => format!("{head} #{n}]"),
                None => format!("{base} #{n}"),
            };
            n += 1;
        }
        self.text.insert_str(self.cursor, &placeholder);
        self.cursor += placeholder.len();
        self.pastes.push((placeholder, text));
    }

    /// 提交用全文：占位符展开回粘贴原文（按原文中的位置一次性拼接，
    /// 不会把已展开内容里的同形文本二次替换）
    pub(crate) fn expanded(&self) -> String {
        let mut spots: Vec<(usize, usize, &str)> = self
            .pastes
            .iter()
            .filter_map(|(placeholder, content)| {
                self.text
                    .find(placeholder.as_str())
                    .map(|pos| (pos, placeholder.len(), content.as_str()))
            })
            .collect();
        spots.sort_by_key(|(pos, _, _)| std::cmp::Reverse(*pos));
        let mut out = self.text.clone();
        for (pos, len, content) in spots {
            out.replace_range(pos..pos + len, content);
        }
        out
    }

    /// 缓冲区内占位符的字节区间（渲染高亮用），按位置升序
    pub(crate) fn placeholder_ranges(&self) -> Vec<(usize, usize)> {
        let mut ranges: Vec<(usize, usize)> = self
            .pastes
            .iter()
            .filter_map(|(placeholder, _)| {
                self.text
                    .find(placeholder.as_str())
                    .map(|pos| (pos, pos + placeholder.len()))
            })
            .collect();
        ranges.sort_unstable();
        ranges
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len())
    }

    /// 光标前紧邻换行时删除它
    fn backspace_newline(&mut self) -> bool {
        if self.cursor > 0 && self.text.as_bytes()[self.cursor - 1] == b'\n' {
            self.text.remove(self.cursor - 1);
            self.cursor -= 1;
            return true;
        }
        false
    }
}

/// 写入缓冲区前的净化：`\r\n`/`\r` → `\n`，制表符 → 4 空格，其余控制字符丢弃
pub(crate) fn sanitize(s: &str) -> String {
    let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len());
    for c in normalized.chars() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// 粘贴文本归一：换行统一为 `\n`、保留多行结构（代码/堆栈不再被压成一行），
/// 去掉首部空行与尾部空白（复制整行常带的尾随换行不应变成空行）
pub(crate) fn normalize_paste(text: &str) -> String {
    sanitize(text)
        .trim_start_matches('\n')
        .trim_end()
        .to_string()
}

/// 词字符：字母数字（含 CJK 表意文字）与下划线
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn char_at(s: &str, pos: usize) -> char {
    s[pos..].chars().next().unwrap_or('\0')
}

fn prev_boundary(s: &str, pos: usize) -> usize {
    s[..pos]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_boundary(s: &str, pos: usize) -> usize {
    s[pos..]
        .chars()
        .next()
        .map(|c| pos + c.len_utf8())
        .unwrap_or(s.len())
}

fn word_left(s: &str, from: usize) -> usize {
    let mut pos = from;
    while pos > 0 {
        let prev = prev_boundary(s, pos);
        if is_word_char(char_at(s, prev)) {
            break;
        }
        pos = prev;
    }
    while pos > 0 {
        let prev = prev_boundary(s, pos);
        if !is_word_char(char_at(s, prev)) {
            break;
        }
        pos = prev;
    }
    pos
}

fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// 字符串显示宽度（CJK 计 2 列）
pub(crate) fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 一个可视行：文本字节区间 `[start, end)`（不含换行符）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VisualRow {
    pub start: usize,
    pub end: usize,
}

/// 按显示宽度把文本折成可视行。`width` 为可用列数，最后一列留给行尾光标
/// （光标永远落在文本区内，不会压到边框上）
pub(crate) fn layout_rows(text: &str, width: usize) -> Vec<VisualRow> {
    let wrap_at = width.saturating_sub(1).max(1);
    let mut rows = Vec::new();
    let mut line_start = 0;
    for line in text.split('\n') {
        let mut row_start = line_start;
        let mut used = 0;
        for (i, c) in line.char_indices() {
            let w = char_width(c);
            if used + w > wrap_at && used > 0 {
                rows.push(VisualRow {
                    start: row_start,
                    end: line_start + i,
                });
                row_start = line_start + i;
                used = 0;
            }
            used += w;
        }
        rows.push(VisualRow {
            start: row_start,
            end: line_start + line.len(),
        });
        line_start += line.len() + 1;
    }
    rows
}

/// 光标所在可视行与列：软折行边界上的光标归下一行行首（与输入的视觉一致）
pub(crate) fn cursor_row_col(text: &str, rows: &[VisualRow], cursor: usize) -> (usize, usize) {
    let row = rows.iter().rposition(|r| r.start <= cursor).unwrap_or(0);
    let start = rows.get(row).map(|r| r.start).unwrap_or(0);
    let end = cursor.clamp(start, text.len());
    (row, display_width(&text[start..end]))
}

/// ↑↓ 在可视行间移动光标（保持显示列，落在字符边界）。已在首/末行时返回 None
pub(crate) fn vertical_move(text: &str, cursor: usize, width: usize, up: bool) -> Option<usize> {
    let rows = layout_rows(text, width);
    let (row, col) = cursor_row_col(text, &rows, cursor);
    let target = if up {
        row.checked_sub(1)?
    } else if row + 1 < rows.len() {
        row + 1
    } else {
        return None;
    };
    let VisualRow { start, end } = rows[target];
    let mut pos = start;
    let mut used = 0;
    for (i, c) in text[start..end].char_indices() {
        let w = char_width(c);
        if used + w > col {
            break;
        }
        used += w;
        pos = start + i + c.len_utf8();
    }
    Some(pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TPL: &str = "[pasted {} lines]";

    fn buf(text: &str) -> InputBuffer {
        let mut b = InputBuffer::new();
        b.set(text);
        b
    }

    #[test]
    fn test_insert_and_cursor_moves_cjk() {
        let mut b = InputBuffer::new();
        for c in "你好ab".chars() {
            b.insert_char(c);
        }
        assert_eq!(b.text(), "你好ab");
        assert_eq!(b.cursor(), "你好ab".len());
        b.move_left();
        b.move_left();
        b.move_left(); // 落在 "好" 前（多字节边界）
        assert_eq!(b.cursor(), "你".len());
        b.insert_char('X');
        assert_eq!(b.text(), "你X好ab");
        b.move_right();
        b.backspace(); // 删掉 "好"
        assert_eq!(b.text(), "你Xab");
        b.move_buffer_start();
        b.move_left(); // 开头不越界
        assert_eq!(b.cursor(), 0);
        assert!(b.delete());
        assert_eq!(b.text(), "Xab");
        b.move_buffer_end();
        assert!(!b.delete(), "delete at end is a no-op");
        b.move_right();
        assert_eq!(b.cursor(), b.text().len());
    }

    #[test]
    fn test_line_ops() {
        let mut b = buf("first\nsecond line");
        b.move_line_start();
        assert_eq!(b.cursor(), "first\n".len());
        b.move_line_end();
        assert_eq!(b.cursor(), b.text().len());
        // Ctrl+U：删到行首（只动当前逻辑行）
        b.set_cursor("first\nsecond".len());
        assert!(b.delete_to_line_start());
        assert_eq!(b.text(), "first\n line");
        // 行首再 Ctrl+U：合并上一行
        assert!(b.delete_to_line_start());
        assert_eq!(b.text(), "first line");
        // Ctrl+K：删到行尾；行尾再 Ctrl+K 合并下一行
        let mut b = buf("ab\ncd");
        b.set_cursor(1);
        assert!(b.delete_to_line_end());
        assert_eq!(b.text(), "a\ncd");
        assert!(b.delete_to_line_end());
        assert_eq!(b.text(), "acd");
    }

    #[test]
    fn test_word_ops() {
        let mut b = buf("cargo test --workspace src/app.rs");
        // Ctrl+W 空白分词：路径整段删除
        assert!(b.delete_word_back());
        assert_eq!(b.text(), "cargo test --workspace ");
        assert!(b.delete_word_back());
        assert_eq!(b.text(), "cargo test ");
        // Alt+Backspace 词边界：停在标点
        let mut b = buf("open src/app.rs");
        assert!(b.delete_word_left());
        assert_eq!(b.text(), "open src/app.");
        // 跳词
        let mut b = buf("foo_bar baz.qux");
        b.move_word_left();
        assert_eq!(b.cursor(), "foo_bar baz.".len());
        b.move_word_left();
        assert_eq!(b.cursor(), "foo_bar ".len());
        b.move_word_left();
        assert_eq!(b.cursor(), 0);
        b.move_word_right();
        assert_eq!(b.cursor(), "foo_bar".len());
        // CJK 连续表意文字视为一个词
        let mut b = buf("修复 输入框");
        b.move_word_left();
        assert_eq!(b.cursor(), "修复 ".len());
        assert!(b.delete_word_back());
        assert_eq!(
            b.text(),
            "输入框",
            "cursor mid-text: deletes the word before it"
        );
        let mut b = buf("修复 输入框");
        assert!(b.delete_word_back());
        assert_eq!(b.text(), "修复 ");
        // 紧邻换行：只删换行
        let mut b = buf("a\n");
        assert!(b.delete_word_back());
        assert_eq!(b.text(), "a");
    }

    #[test]
    fn test_backslash_continuation() {
        let mut b = buf("line one\\");
        assert!(b.consume_backslash_continuation());
        assert_eq!(b.text(), "line one\n");
        // 转义的字面反斜杠：照常提交
        let mut b = buf("path\\\\");
        assert!(!b.consume_backslash_continuation());
        // 光标不在末尾：不触发
        let mut b = buf("a\\");
        b.move_left();
        assert!(!b.consume_backslash_continuation());
    }

    #[test]
    fn test_sanitize_normalizes_newlines_and_controls() {
        assert_eq!(sanitize("a\r\nb\rc\n"), "a\nb\nc\n");
        assert_eq!(sanitize("x\ty"), "x    y");
        assert_eq!(sanitize("evil\u{1b}[2J"), "evil[2J");
        let mut b = InputBuffer::new();
        b.insert_char('\u{7}');
        assert!(b.is_empty(), "control chars never enter the buffer");
        // 粘贴归一：多行保留，首部空行与尾随换行去掉，行首缩进保留
        assert_eq!(
            normalize_paste("\r\n    fn main() {\r\n        hi();\r\n    }\r\n\r\n"),
            "    fn main() {\n        hi();\n    }"
        );
        assert_eq!(normalize_paste("sk-abc\n"), "sk-abc");
    }

    #[test]
    fn test_paste_placeholder_roundtrip() {
        let mut b = buf("see: ");
        b.insert_paste("short\npaste", TPL);
        assert_eq!(b.text(), "see: short\npaste", "<5 lines inserted verbatim");

        let big = (1..=6)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut b = buf("trace: ");
        b.insert_paste(&big, TPL);
        assert_eq!(b.text(), "trace: [pasted 6 lines]");
        b.insert_str(" end");
        assert_eq!(b.expanded(), format!("trace: {big} end"));
        assert_eq!(
            b.placeholder_ranges(),
            vec![(7, 7 + "[pasted 6 lines]".len())]
        );

        // 同行数的第二次粘贴：占位符去重，展开各归其位
        let other = big.replace("line", "row");
        b.move_buffer_start();
        b.insert_paste(&other, TPL);
        assert!(b.text().starts_with("[pasted 6 lines #2]"), "{}", b.text());
        assert_eq!(b.expanded(), format!("{other}trace: {big} end"));

        // 退格在占位符末尾：整体删除
        let mut b = buf("");
        b.insert_paste(&big, TPL);
        assert!(b.backspace());
        assert!(b.is_empty());
        assert_eq!(b.expanded(), "");

        // set / clear 作废占位符
        let mut b = buf("");
        b.insert_paste(&big, TPL);
        b.set("[pasted 6 lines]");
        assert_eq!(b.expanded(), "[pasted 6 lines]");
    }

    #[test]
    fn test_layout_and_cursor_cjk() {
        // 宽 5 → 可折行 4 列（末列留给光标）
        let rows = layout_rows("abcdefg", 5);
        assert_eq!(
            rows,
            vec![
                VisualRow { start: 0, end: 4 },
                VisualRow { start: 4, end: 7 }
            ]
        );
        // CJK 2 列：两个汉字占满 4 列
        let text = "你好世界";
        let rows = layout_rows(text, 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(&text[rows[0].start..rows[0].end], "你好");
        // 软折行边界上的光标归下一行行首
        assert_eq!(cursor_row_col(text, &rows, "你好".len()), (1, 0));
        assert_eq!(cursor_row_col(text, &rows, text.len()), (1, 4));
        // 硬换行：行尾光标留在本行；空行也占一行
        let text = "ab\n\ncd";
        let rows = layout_rows(text, 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(cursor_row_col(text, &rows, 2), (0, 2));
        assert_eq!(cursor_row_col(text, &rows, 3), (1, 0));
        assert_eq!(layout_rows("", 10).len(), 1);
    }

    #[test]
    fn test_vertical_move_keeps_column() {
        let text = "hello\nhi\nworld";
        // 末行第 3 列 → 上移到 "hi" 行尾（列钳制），再上移到 "hello" 第 2 列
        let pos = vertical_move(text, "hello\nhi\nwor".len(), 80, true).unwrap();
        assert_eq!(pos, "hello\nhi".len());
        let pos = vertical_move(text, pos, 80, true).unwrap();
        assert_eq!(pos, 2);
        assert!(vertical_move(text, pos, 80, true).is_none(), "top row");
        let pos = vertical_move(text, 0, 80, false).unwrap();
        assert_eq!(pos, "hello\n".len());
        assert!(
            vertical_move(text, text.len(), 80, false).is_none(),
            "bottom row"
        );
        // CJK 列对齐：第 2 行 "ab" 的第 2 列对应首行 "你" 之后
        let text = "你好\nab";
        assert_eq!(vertical_move(text, text.len(), 80, true), Some("你".len()));
    }
}
