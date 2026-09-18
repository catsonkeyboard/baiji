//! grep 结果路径共享（参考 tokenless search_results：无损、零 spill）
//!
//! `path:line: text` 连续同路径行归组为 `File: path` 头 + `line: text` 行，
//! 重复路径只出现一次。行数不足（<8）或格式不符时回退原样拼接。

/// 归组渲染；比逐行拼接更短时才采用（无损，但保持确定性）
pub(crate) fn render(matches: &[String]) -> String {
    let flat = matches.join("\n");
    if matches.len() < 8 {
        return flat;
    }
    let mut out = String::with_capacity(flat.len());
    let mut current_path: Option<&str> = None;
    for m in matches {
        // 解析工具自身的输出格式 "path:line: text"
        let Some((loc, text)) = m.split_once(": ") else {
            return flat; // 兜底：非预期格式，原样返回
        };
        let Some((path, line)) = loc.rsplit_once(':') else {
            return flat;
        };
        if current_path != Some(path) {
            if current_path.is_some() {
                out.push('\n');
            }
            out.push_str("File: ");
            out.push_str(path);
            out.push('\n');
            current_path = Some(path);
        } else {
            out.push('\n');
        }
        out.push_str(line);
        out.push_str(": ");
        out.push_str(text);
    }
    // 无损：只在确实更短时采用
    if out.len() < flat.len() { out } else { flat }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches_for(path: &str, lines: &[u32]) -> Vec<String> {
        lines
            .iter()
            .map(|&l| format!("{path}:{l}: some matched text on line {l}"))
            .collect()
    }

    #[test]
    fn test_grouping_shares_path_prefix() {
        let matches = [
            matches_for("src/main.rs", &[1, 5, 9, 11, 14]),
            matches_for("src/lib/long/path/module.rs", &[10, 20, 30]),
            matches_for("src/main.rs", &[2]),
        ]
        .concat();
        assert_eq!(matches.len(), 9);
        let out = render(&matches);

        // 组头 + 行号行；所有匹配文本无损存活
        assert!(
            out.starts_with("File: src/main.rs\n1: some matched text on line 1"),
            "{out}"
        );
        assert!(
            out.contains("File: src/lib/long/path/module.rs\n10: some matched text on line 10")
        );
        // 路径再次出现时重新归组
        assert!(out.contains("File: src/main.rs\n2: some matched text on line 2"));
        for text in [
            "line 1", "line 5", "line 9", "line 11", "line 14", "line 10", "line 20", "line 30",
            "line 2",
        ] {
            assert!(out.contains(text), "lost: {text}");
        }
        // 比扁平格式短
        assert!(out.len() < matches.join("\n").len());
    }

    #[test]
    fn test_few_matches_stay_flat() {
        let matches = matches_for("src/a.rs", &[1, 2, 3]);
        assert_eq!(render(&matches), matches.join("\n"));
    }

    #[test]
    fn test_unparseable_lines_fall_back() {
        let matches: Vec<String> = (0..10).map(|i| format!("no-colon-line-{i}")).collect();
        assert_eq!(render(&matches), matches.join("\n"));
    }

    #[test]
    fn test_text_with_colons_is_safe() {
        // 匹配文本内含 ": "：解析在第一个 ": " 处切分，文本部分完整保留
        let mut matches = matches_for("src/a.rs", &[1]);
        for l in 2..10 {
            matches.push(format!("src/a.rs:{l}: key: value pair on line {l}"));
        }
        let out = render(&matches);
        assert!(out.contains("key: value pair on line 5"), "{out}");
    }
}
