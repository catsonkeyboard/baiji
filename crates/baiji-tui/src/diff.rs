//! edit / write 工具的内联 diff。
//!
//! ToolStarted 时由工具参数生成（"建议中的改动"）：edit 比较 old_string → new_string
//! （含 `edits: [{old_string, new_string, replace_all?}]` 数组形式，逐项展示），
//! write 与空文件比较（全部为新增）。聊天区按 [`INLINE_DIFF_CAP`] 截断（头尾各半 +
//! `… N more changes …`），HITL 弹窗不截断、长行折行——用户必须看到自己批准的全部内容。
//!
//! 外框：`┌─ path` / `│ - old` / `│ + new` / `└─ +A -D`

use crate::i18n::{Strings, fill};
use crate::theme::Theme;
use crate::tool_display::{truncate_end_display, truncate_middle_display};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use unicode_width::UnicodeWidthStr;

/// 聊天区内联 diff 最多展示的改动行数（头尾各一半）
pub const INLINE_DIFF_CAP: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Add,
    Del,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiffRow {
    /// 一行改动（不含行尾换行）
    Change(DiffKind, String),
    /// `edits` 数组相邻两处编辑之间的分隔
    Separator,
    /// 截断省略掉的改动行数
    Elided(usize),
}

/// 一个工具调用的 diff 视图（聊天行与确认弹窗共用）
#[derive(Debug, Clone, PartialEq)]
pub struct DiffView {
    /// 所属工具调用 id（ToolFinished 失败时据此标记）
    pub tool_id: String,
    pub path: String,
    pub replace_all: bool,
    pub rows: Vec<DiffRow>,
    /// 全量统计（不受截断影响）
    pub additions: usize,
    pub deletions: usize,
    /// 工具执行失败（改动未落盘）：整块转暗并在尾注标出
    pub failed: bool,
}

impl DiffView {
    /// 由工具参数构造。`cap = Some(n)`：聊天区内联（截断到 n 行改动、跳过纯空白行）；
    /// `None`：确认弹窗（完整保留）。非 edit/write 或无改动返回 None
    pub fn from_tool(tool_id: &str, name: &str, args: &Value, cap: Option<usize>) -> Option<Self> {
        let path = args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(Value::as_str)?
            .to_string();
        let skip_blank = cap.is_some();
        let flag = |v: &Value| v.get("replace_all").and_then(Value::as_bool) == Some(true);
        let mut replace_all = flag(args);
        let mut rows: Vec<DiffRow> = Vec::new();
        let (mut additions, mut deletions) = (0usize, 0usize);
        let mut push_pair = |old: &str, new: &str, rows: &mut Vec<DiffRow>| {
            let (pair_rows, add, del) = diff_lines(old, new, skip_blank);
            additions += add;
            deletions += del;
            if !pair_rows.is_empty() {
                if !rows.is_empty() {
                    rows.push(DiffRow::Separator);
                }
                rows.extend(pair_rows);
            }
        };
        match name {
            "edit" => match args.get("edits").and_then(Value::as_array) {
                Some(edits) => {
                    for edit in edits {
                        replace_all |= flag(edit);
                        push_pair(
                            str_field(edit, "old_string"),
                            str_field(edit, "new_string"),
                            &mut rows,
                        );
                    }
                }
                None => push_pair(
                    str_field(args, "old_string"),
                    str_field(args, "new_string"),
                    &mut rows,
                ),
            },
            "write" => push_pair("", str_field(args, "content"), &mut rows),
            _ => return None,
        }
        if additions == 0 && deletions == 0 {
            return None;
        }
        if let Some(cap) = cap {
            rows = cap_rows(rows, cap);
        }
        Some(Self {
            tool_id: tool_id.to_string(),
            path,
            replace_all,
            rows,
            additions,
            deletions,
            failed: false,
        })
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// 行级 diff：(展示行, 新增数, 删除数)。统计含空白行；`skip_blank` 只影响展示
fn diff_lines(old: &str, new: &str, skip_blank: bool) -> (Vec<DiffRow>, usize, usize) {
    // 末行补换行：避免 "x" → "x\ny" 被判为 "-x +x +y"
    let old = with_trailing_newline(old);
    let new = with_trailing_newline(new);
    let diff = TextDiff::from_lines(old.as_str(), new.as_str());
    let mut rows = Vec::new();
    let (mut add, mut del) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        let kind = match change.tag() {
            ChangeTag::Insert => {
                add += 1;
                DiffKind::Add
            }
            ChangeTag::Delete => {
                del += 1;
                DiffKind::Del
            }
            ChangeTag::Equal => continue,
        };
        let text = change.value().trim_end_matches(['\n', '\r']);
        if skip_blank && text.trim().is_empty() {
            continue;
        }
        rows.push(DiffRow::Change(kind, text.to_string()));
    }
    (rows, add, del)
}

fn with_trailing_newline(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

/// 截断到 `cap` 行改动：保留开头 cap/2 与结尾 cap-cap/2 行，中间以 Elided 标出。
/// 落在省略区或紧贴省略标记的分隔行一并丢弃
fn cap_rows(rows: Vec<DiffRow>, cap: usize) -> Vec<DiffRow> {
    let total = rows
        .iter()
        .filter(|r| matches!(r, DiffRow::Change(..)))
        .count();
    if total <= cap {
        return rows;
    }
    let head = cap / 2;
    let tail = cap - head;
    let tail_start = total - tail;
    let mut out = Vec::with_capacity(cap + 2);
    let mut seen = 0usize; // 已经过的改动行数
    let mut elided = false;
    for row in rows {
        match row {
            DiffRow::Change(..) => {
                if seen < head || seen >= tail_start {
                    out.push(row);
                } else if !elided {
                    out.push(DiffRow::Elided(total - head - tail));
                    elided = true;
                }
                seen += 1;
            }
            // 分隔行：两侧都保留时才有意义
            DiffRow::Separator if (seen > 0 && seen < head) || seen > tail_start => out.push(row),
            _ => {}
        }
    }
    out
}

/// 渲染选项
#[derive(Debug, Clone, Copy)]
pub struct DiffStyle {
    /// true：长行折行（弹窗，完整可见）；false：单行截断（聊天区）
    pub wrap: bool,
}

/// 渲染为带外框的行（不含外层缩进；`width` 为可用显示宽度）
pub fn render(
    view: &DiffView,
    width: usize,
    theme: &Theme,
    strings: &Strings,
    opts: DiffStyle,
) -> Vec<Line<'static>> {
    let width = width.max(8);
    let frame = Style::default().fg(theme.system);
    let (add_style, del_style) = if view.failed {
        (frame, frame)
    } else {
        (
            Style::default().fg(theme.diff_add),
            Style::default().fg(theme.diff_del),
        )
    };
    let mut out = Vec::with_capacity(view.rows.len() + 2);

    // ┌─ path（· replace_all）
    let suffix = if view.replace_all {
        " · replace_all"
    } else {
        ""
    };
    let path_budget = width.saturating_sub(3 + suffix.width());
    out.push(Line::from(vec![
        Span::styled("┌─ ", frame),
        Span::styled(
            truncate_middle_display(&view.path, path_budget),
            Style::default().fg(theme.user),
        ),
        Span::styled(suffix.to_string(), frame),
    ]));

    // `│ ` + `- `/`+ ` 共 4 列前缀
    let text_width = width.saturating_sub(4).max(1);
    for row in &view.rows {
        match row {
            DiffRow::Change(kind, text) => {
                let (sign, style) = match kind {
                    DiffKind::Add => ("+ ", add_style),
                    DiffKind::Del => ("- ", del_style),
                };
                if opts.wrap {
                    for (i, piece) in crate::markdown::hard_wrap(text, text_width)
                        .into_iter()
                        .enumerate()
                    {
                        out.push(Line::from(vec![
                            Span::styled("│ ", frame),
                            Span::styled(if i == 0 { sign } else { "  " }, style),
                            Span::styled(piece, style),
                        ]));
                    }
                } else {
                    let line = crate::markdown::sanitize(text);
                    out.push(Line::from(vec![
                        Span::styled("│ ", frame),
                        Span::styled(sign, style),
                        Span::styled(truncate_end_display(&line, text_width), style),
                    ]));
                }
            }
            DiffRow::Separator => out.push(Line::styled("│ ⋯", frame)),
            DiffRow::Elided(n) => out.push(Line::styled(
                format!("│ {}", fill(strings.diff_more_tpl, &[n])),
                frame,
            )),
        }
    }

    // └─ +A -D（失败：· ✗ not applied）
    let mut footer = vec![
        Span::styled("└─ ", frame),
        Span::styled(format!("+{}", view.additions), add_style),
        Span::styled(" ", frame),
        Span::styled(format!("-{}", view.deletions), del_style),
    ];
    if view.failed {
        footer.push(Span::styled(
            format!(" · {}", strings.diff_failed),
            Style::default().fg(theme.error),
        ));
    }
    out.push(Line::from(footer));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn changes(view: &DiffView) -> Vec<String> {
        view.rows
            .iter()
            .map(|r| match r {
                DiffRow::Change(DiffKind::Add, t) => format!("+{t}"),
                DiffRow::Change(DiffKind::Del, t) => format!("-{t}"),
                DiffRow::Separator => "~".to_string(),
                DiffRow::Elided(n) => format!("…{n}"),
            })
            .collect()
    }

    #[test]
    fn test_edit_diff_basic() {
        let args = json!({"path": "src/a.rs", "old_string": "fn a() {\n    old();\n}", "new_string": "fn a() {\n    new();\n}"});
        let view = DiffView::from_tool("t1", "edit", &args, Some(INLINE_DIFF_CAP)).unwrap();
        assert_eq!(view.path, "src/a.rs");
        assert_eq!(changes(&view), vec!["-    old();", "+    new();"]);
        assert_eq!((view.additions, view.deletions), (1, 1));
        // 末行无换行不产生伪改动
        let view = DiffView::from_tool(
            "t",
            "edit",
            &json!({"path": "a", "old_string": "x", "new_string": "x\ny"}),
            None,
        )
        .unwrap();
        assert_eq!(changes(&view), vec!["+y"]);
        // 无改动 / 非编辑工具：无 diff
        assert!(
            DiffView::from_tool(
                "t",
                "edit",
                &json!({"path": "a", "old_string": "x", "new_string": "x"}),
                None
            )
            .is_none()
        );
        assert!(DiffView::from_tool("t", "read", &json!({"path": "a"}), None).is_none());
    }

    #[test]
    fn test_edits_array_and_write() {
        let args = json!({"path": "a.rs", "edits": [
            {"old_string": "one", "new_string": "uno"},
            {"old_string": "two", "new_string": "dos", "replace_all": true}
        ]});
        let view = DiffView::from_tool("t", "edit", &args, Some(INLINE_DIFF_CAP)).unwrap();
        assert_eq!(changes(&view), vec!["-one", "+uno", "~", "-two", "+dos"]);
        assert!(view.replace_all);
        assert_eq!((view.additions, view.deletions), (2, 2));

        let view = DiffView::from_tool(
            "t",
            "write",
            &json!({"path": "new.txt", "content": "a\n\nb\n"}),
            Some(INLINE_DIFF_CAP),
        )
        .unwrap();
        // 统计含空行，内联展示跳过纯空白行
        assert_eq!(changes(&view), vec!["+a", "+b"]);
        assert_eq!((view.additions, view.deletions), (3, 0));
        // 弹窗模式完整保留
        let full = DiffView::from_tool(
            "t",
            "write",
            &json!({"path": "n", "content": "a\n\nb\n"}),
            None,
        )
        .unwrap();
        assert_eq!(changes(&full), vec!["+a", "+", "+b"]);
    }

    #[test]
    fn test_diff_capping_head_and_tail() {
        let content: String = (1..=40).map(|i| format!("line{i}\n")).collect();
        let view = DiffView::from_tool(
            "t",
            "write",
            &json!({"path": "big.txt", "content": content}),
            Some(INLINE_DIFF_CAP),
        )
        .unwrap();
        let rows = changes(&view);
        assert_eq!(rows.len(), INLINE_DIFF_CAP + 1);
        assert_eq!(rows[0], "+line1");
        assert_eq!(rows[5], "+line6");
        assert_eq!(rows[6], "…28");
        assert_eq!(rows[7], "+line35");
        assert_eq!(rows[12], "+line40");
        assert_eq!(view.additions, 40);

        // 刚好 cap 行：不截断
        let content: String = (1..=12).map(|i| format!("l{i}\n")).collect();
        let view = DiffView::from_tool(
            "t",
            "write",
            &json!({"path": "p", "content": content}),
            Some(12),
        )
        .unwrap();
        assert!(!view.rows.iter().any(|r| matches!(r, DiffRow::Elided(_))));

        // 分隔行落在省略区内被丢弃，不紧贴省略标记
        let edits: Vec<Value> = (0..10)
            .map(|i| json!({"old_string": format!("o{i}"), "new_string": format!("n{i}")}))
            .collect();
        let view = DiffView::from_tool("t", "edit", &json!({"path": "p", "edits": edits}), Some(6))
            .unwrap();
        let rows = changes(&view);
        assert_eq!(
            rows,
            vec!["-o0", "+n0", "~", "-o1", "…14", "+n8", "~", "-o9", "+n9"]
        );
    }

    #[test]
    fn test_render_frame_and_failed() {
        let theme = Theme::dark();
        let strings = Strings::en();
        let args = json!({"path": "src/a.rs", "old_string": "old", "new_string": "new"});
        let mut view = DiffView::from_tool("t", "edit", &args, Some(INLINE_DIFF_CAP)).unwrap();
        let lines = render(&view, 40, &theme, &strings, DiffStyle { wrap: false });
        let texts: Vec<String> = lines.iter().map(text_of).collect();
        assert_eq!(texts, vec!["┌─ src/a.rs", "│ - old", "│ + new", "└─ +1 -1"]);
        assert_eq!(lines[1].spans[2].style.fg, Some(theme.diff_del));
        assert_eq!(lines[2].spans[2].style.fg, Some(theme.diff_add));

        view.failed = true;
        let lines = render(&view, 40, &theme, &strings, DiffStyle { wrap: false });
        assert_eq!(text_of(lines.last().unwrap()), "└─ +1 -1 · ✗ not applied");
        assert_eq!(lines[1].spans[2].style.fg, Some(theme.system));
    }

    #[test]
    fn test_render_truncate_vs_wrap() {
        let theme = Theme::dark();
        let strings = Strings::en();
        let long = "x".repeat(50);
        let view = DiffView::from_tool("t", "write", &json!({"path": "p", "content": long}), None)
            .unwrap();
        let inline = render(&view, 20, &theme, &strings, DiffStyle { wrap: false });
        assert_eq!(inline.len(), 3);
        assert!(text_of(&inline[1]).ends_with('…'));
        assert!(inline.iter().all(|l| l.width() <= 20));
        // 弹窗折行：内容完整
        let wrapped = render(&view, 20, &theme, &strings, DiffStyle { wrap: true });
        let body: String = wrapped[1..wrapped.len() - 1]
            .iter()
            .map(|l| text_of(l)["│ + ".len()..].to_string())
            .collect();
        assert_eq!(body, long);
        assert!(wrapped.iter().all(|l| l.width() <= 20));
    }
}
