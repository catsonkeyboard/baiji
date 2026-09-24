//! density 视图：按行信息熵选行，保留原顺序与行号，跳过的连续段
//! 以 `[La-Lb skipped]` 标记（行号锚点不丢，可继续 offset/limit 精读）。
//! 返回（渲染文本, 保留行数）。确定性：熵相同时取更早的行。

use std::collections::BTreeMap;

/// density 下限（防止把文件压到没法看）
pub(crate) const MIN_DENSITY: f32 = 0.05;

pub fn density_view(lines: &[&str], start_index: usize, keep_ratio: f32) -> (String, usize) {
    let total = lines.len();
    let keep = (((total as f32) * keep_ratio.clamp(MIN_DENSITY, 1.0)).ceil() as usize)
        .min(total)
        .max(1);
    if keep >= total {
        return (
            lines
                .iter()
                .enumerate()
                .map(|(i, l)| format!("{}\t{}", start_index + i + 1, l))
                .collect::<Vec<_>>()
                .join("\n"),
            total,
        );
    }

    // 按熵降序选前 keep 行（total_cmp 全序 + 索引决胜，保证确定性与排序一致性）
    let entropies: Vec<f64> = lines.iter().map(|l| line_entropy(l)).collect();
    let mut ranked: Vec<usize> = (0..total).collect();
    ranked.sort_by(|&a, &b| entropies[b].total_cmp(&entropies[a]).then(a.cmp(&b)));
    ranked.truncate(keep);
    ranked.sort_unstable();

    let mut out = Vec::new();
    let mut prev: Option<usize> = None;
    for &i in &ranked {
        match prev {
            Some(p) if i > p + 1 => out.push(format!(
                "[L{}-L{} skipped]",
                start_index + p + 2,
                start_index + i
            )),
            None if i > 0 => out.push(format!(
                "[L{}-L{} skipped]",
                start_index + 1,
                start_index + i
            )),
            _ => {}
        }
        out.push(format!("{}\t{}", start_index + i + 1, lines[i]));
        prev = Some(i);
    }
    if let Some(p) = prev {
        if p + 1 < total {
            out.push(format!(
                "[L{}-L{} skipped]",
                start_index + p + 2,
                start_index + total
            ));
        }
    }
    (out.join("\n"), keep)
}

/// 单行香农熵（字符分布多样性）：空行 0，重复字符行低，代码行高
fn line_entropy(line: &str) -> f64 {
    let n = line.chars().count();
    if n == 0 {
        return 0.0;
    }
    // BTreeMap：固定的遍历（=浮点求和）顺序。HashMap 的随机迭代顺序会让相同的行
    // 算出末位不同的熵，破坏 density_view 的确定性
    let mut freq: BTreeMap<char, u32> = BTreeMap::new();
    for c in line.chars() {
        *freq.entry(c).or_insert(0) += 1;
    }
    freq.values()
        .map(|&k| {
            let p = k as f64 / n as f64;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_density_view_keeps_order_and_marks_skips() {
        // 熵：代码行（多样字符）> 混合行 > 单一字符重复行（熵 0）
        let lines: Vec<&str> = vec![
            "xxxxxxxxxxxx",              // L1 熵 0 → 丢
            "fn alpha() -> Config {",    // L2 高熵 → 保留
            "yyyyyyyy",                  // L3 熵 0 → 丢
            "let v = compute(a, b, c)?;", // L4 高熵 → 保留
            "zzzzzzzzzzzzzzzz",          // L5 熵 0 → 丢
        ];
        // keep_ratio=0.4 → keep 2 行（L2 与 L4，熵最高）
        let (view, kept) = density_view(&lines, 0, 0.4);
        assert_eq!(kept, 2);
        assert!(view.contains("2\tfn alpha()"), "{view}");
        assert!(view.contains("4\tlet v = compute"), "{view}");
        assert!(view.contains("[L1-L1 skipped]"), "{view}");
        assert!(view.contains("[L3-L3 skipped]"), "{view}");
        assert!(view.contains("[L5-L5 skipped]"), "{view}");
        assert!(!view.contains("xxxx"), "{view}");
    }

    #[test]
    fn test_density_view_full_ratio_returns_all() {
        let lines: Vec<&str> = vec!["a", "bb"];
        let (view, kept) = density_view(&lines, 0, 1.0);
        assert_eq!(kept, 2);
        assert!(view.contains("1\ta"));
        assert!(view.contains("2\tbb"));
    }

    #[test]
    fn test_line_entropy_ordering() {
        assert_eq!(line_entropy(""), 0.0);
        assert!(line_entropy("abcdefgh") > line_entropy("aaaaaaaa"));
    }
}
