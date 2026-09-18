//! 表格域压缩器（CSV/TSV）：行缩减（设计参考 tokenless tabular-compression）
//!
//! 检测：非空行矩形（每行分隔符数一致）、≥2 列、首行是表头（无纯数字
//! 单元格）；Markdown 管道表（行首 `|`）明确拒绝。带引号内嵌分隔符的
//! CSV 天然行长不一致，会被矩形检查拒绝（fail-open：原样返回）。
//!
//! 行缩减（有损，原文先 spill）：数据行 >32 时保留 表头 + 首尾各 4 行 +
//! 诊断行（error/failed/…），等距采样补足 32 行预算。

use crate::compressors::adopted;
use crate::env::ExecutionEnv;

/// 触发行缩减的最小数据行数（>32 触发，保留预算 32）
const MAX_DATA_ROWS: usize = 32;
/// 首尾各保留的行数
const EDGE_KEEP: usize = 4;
/// 诊断行关键词（小写包含）
const DIAGNOSTIC_KEYWORDS: &[&str] = &["error", "failed", "failure", "fatal", "panic", "warn"];

pub(crate) fn detect(content: &str) -> bool {
    table(content).is_some()
}

/// 解析为矩形表：(分隔符, 行)。None = 不是可信的矩形表。
fn table(content: &str) -> Option<(&'static str, Vec<Vec<String>>)> {
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    // 表头 + 33 数据行起才有行缩减空间
    if lines.len() < MAX_DATA_ROWS + 2 {
        return None;
    }
    for delim in [",", "\t"] {
        // Markdown 管道表语义不同，明确拒绝
        if lines.iter().any(|l| l.trim_start().starts_with('|')) {
            continue;
        }
        let counts: Vec<usize> = lines.iter().map(|l| l.matches(delim).count()).collect();
        let delims_per_line = counts[0];
        if delims_per_line < 1 || counts.iter().any(|c| *c != delims_per_line) {
            continue;
        }
        let rows: Vec<Vec<String>> = lines
            .iter()
            .map(|l| l.split(delim).map(str::to_string).collect())
            .collect();
        // 首行是表头的证据：没有任何纯数字单元格
        if rows[0].iter().all(|c| !is_pure_number(c)) {
            return Some((delim, rows));
        }
    }
    None
}

fn is_pure_number(cell: &str) -> bool {
    let t = cell.trim();
    !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || c == '.')
}

pub(crate) fn compress(content: &str, env: &ExecutionEnv, allow_lossy: bool) -> String {
    let Some((delim, rows)) = table(content) else {
        return content.to_string();
    };
    let data_rows = rows.len() - 1;
    if data_rows <= MAX_DATA_ROWS || !allow_lossy {
        return content.to_string();
    }
    // 有损必先 spill：spill 失败即放弃
    let Some(handle) = env.spill(content) else {
        return content.to_string();
    };

    let is_diagnostic = |row: &[String]| {
        let joined = row.join(" ").to_lowercase();
        DIAGNOSTIC_KEYWORDS.iter().any(|kw| joined.contains(kw))
    };
    let last = data_rows; // 数据行数（0 基位置 < last）
    // 必保：首尾各 EDGE_KEEP 行 + 诊断行（rows 下标 1..，数据位置 = i-1）
    let mut keep: Vec<usize> = (1..rows.len())
        .filter(|&i| {
            let pos = i - 1;
            pos < EDGE_KEEP || pos + EDGE_KEEP >= last || is_diagnostic(&rows[i])
        })
        .collect();
    // 等距采样补足预算
    if keep.len() < MAX_DATA_ROWS {
        let need = MAX_DATA_ROWS - keep.len();
        let remaining: Vec<usize> = (1..rows.len()).filter(|i| !keep.contains(i)).collect();
        let take = need.min(remaining.len());
        for k in 0..take {
            let idx = ((k + 1) * remaining.len()) / (take + 1);
            keep.push(remaining[idx.min(remaining.len() - 1)]);
        }
    }
    keep.sort_unstable();
    keep.dedup();

    let mut out = rows[0].join(delim);
    for i in &keep {
        out.push('\n');
        out.push_str(&rows[*i].join(delim));
    }
    let omitted = data_rows - keep.len();
    out.push_str(&format!(
        "\n[{omitted} of {data_rows} data rows elided (kept first/last {EDGE_KEEP}, diagnostic rows, evenly spaced sample); full table: ctx:{handle} (expand tool)]"
    ));
    if adopted(content, &out) {
        out
    } else {
        content.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csv_fixture(rows: usize, error_at: Option<usize>) -> String {
        let mut out = String::from("name,status,count,note\n");
        for i in 0..rows {
            let status = if Some(i) == error_at { "error" } else { "ok" };
            out.push_str(&format!(
                "item-{i:03},{status},{i},some padding text for width\n"
            ));
        }
        out
    }

    fn env_with_store() -> (tempfile::TempDir, ExecutionEnv) {
        let dir = tempfile::tempdir().unwrap();
        let env = ExecutionEnv::new(".").with_ctx_store(dir.path());
        (dir, env)
    }

    #[test]
    fn test_row_reduction_keeps_edges_diagnostics_and_header() {
        let content = csv_fixture(60, Some(40));
        assert!(detect(&content));
        let (_dir, env) = env_with_store();
        let out = compress(&content, &env, true);

        // 表头 + 首尾各 4 + 诊断行 + 采样 ≤ 32 数据行 + 标记
        let data_lines = out.lines().filter(|l| l.starts_with("item-")).count();
        assert!(data_lines <= MAX_DATA_ROWS, "{data_lines}");
        assert!(out.starts_with("name,status,count,note"));
        assert!(out.contains("item-000,ok,0,"));
        assert!(out.contains("item-003,ok,3,"));
        assert!(out.contains("item-059,ok,59,"));
        // 诊断行存活（第 40 行不在首尾 4 行内）
        assert!(
            out.contains("item-040,error,40,"),
            "diagnostic row must survive"
        );
        // 标记 + 句柄
        assert!(out.contains("data rows elided"));
        assert!(out.contains("ctx:"));
        // 节省达门槛
        assert!(out.len() < content.len() * 85 / 100);
        // 原文可取回
        let handle = &out[out.find("ctx:").unwrap() + 4..][..16];
        assert_eq!(env.retrieve(handle).unwrap(), content);
    }

    #[test]
    fn test_small_table_untouched() {
        let content = csv_fixture(20, None);
        assert!(!detect(&content));
        let (_dir, env) = env_with_store();
        assert_eq!(compress(&content, &env, true), content);
    }

    #[test]
    fn test_lossy_denied_for_failed_commands() {
        let content = csv_fixture(60, None);
        let (_dir, env) = env_with_store();
        assert_eq!(compress(&content, &env, false), content);
    }

    #[test]
    fn test_lossy_requires_ctx_store() {
        let content = csv_fixture(60, None);
        assert_eq!(compress(&content, &ExecutionEnv::new("."), true), content);
    }

    #[test]
    fn test_markdown_pipe_table_rejected() {
        let mut content = String::from("| name | status |\n| --- | --- |\n");
        for i in 0..50 {
            content.push_str(&format!("| item-{i} | ok |\n"));
        }
        assert!(!detect(&content));
    }

    #[test]
    fn test_ragged_input_rejected() {
        let mut content = String::from("a,b\n");
        for i in 0..50 {
            content.push_str(&format!("item-{i},ok,extra_col_{i}\n")); // 列数不一致
        }
        assert!(!detect(&content));
    }

    #[test]
    fn test_numeric_header_rejected() {
        // 首行纯数字 → 不是表头证据，拒绝
        let mut content = String::from("1,2\n");
        for i in 0..50 {
            content.push_str(&format!("{i},value-{i}\n"));
        }
        assert!(!detect(&content));
    }

    #[test]
    fn test_tsv_supported() {
        let mut content = String::from("name\tstatus\tnote\n");
        for i in 0..50 {
            content.push_str(&format!("item-{i:03}\tok\tsome padding text for width\n"));
        }
        let (_dir, env) = env_with_store();
        let out = compress(&content, &env, true);
        assert!(out.contains("data rows elided"));
        assert!(out.contains("name\tstatus\tnote"));
        assert!(out.contains("item-000\tok"));
    }
}
