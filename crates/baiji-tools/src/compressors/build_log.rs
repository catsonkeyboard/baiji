//! 构建日志域压缩器：诊断保护的冗长区间缩减（设计参考 tokenless build_log）
//!
//! 行分为 Keep（诊断/摘要/栈帧/错误块结构）与 Reducible（编译、下载、
//! 单测通过等进度行）。**连续 ≥9 行 Reducible 才缩减**（保前 2 后 2，
//! 中间计数）；遗漏区间 >8 个则整体放弃（标记泛滥说明分类不可靠）。
//! 原文先 spill，可经 expand 取回。
//!
//! 分类原则：**宁可漏减不误减**——关键词命中即保留；真正的缩减目标是大段
//! 同质进度（`Compiling x`、`=== RUN`、`test x ... ok`、`PASSED` 等）。

use crate::compressors::adopted;
use crate::env::ExecutionEnv;
use std::ops::Range;

const MIN_LINES: usize = 40;
const RUN_MIN: usize = 9;
const RUN_KEEP_HEAD: usize = 2;
const RUN_KEEP_TAIL: usize = 2;
const MAX_RANGES: usize = 8;

/// 关键词命中即整行保留（小写子串匹配，方向保守：多保留无害）。
/// 注意 `passed`/`skipped` 不在表内：单测进度行 `test_x PASSED` 正是缩减目标；
/// 真正的汇总行总有其他锚点（`test result` / `tests:` / `failed` / `error`）。
const KEEP_KEYWORDS: &[&str] = &[
    // 诊断
    "error",
    "warn",
    "fail",
    "fatal",
    "panic",
    "exception",
    "traceback",
    "aborted",
    "denied",
    "could not compile",
    "make: ***",
    "exit code",
    // 摘要锚点（"tests: " 带尾随空格：避免误命中 Rust 模块路径 `tests::it_works`）
    "test result",
    "tests: ",
    "suites: ",
    "snapshots: ",
    "finished",
    "doc-tests",
    "vulnerabilit",
    "audited",
    "up to date",
    "up-to-date",
    // 错误块结构（rustc/jest/pytest 附带上下文）
    "note:",
    "help:",
    "captured",
    "-->",
];

/// 行是否必须逐字保留
fn is_keep_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return true;
    }
    let indented = line.trim_start();
    // 栈帧行：JS/Python `at frame (file:1:2)`、Python `File "...", line N`
    if indented.starts_with("at ") || indented.starts_with("File \"") {
        return true;
    }
    // rust 回溯编号帧：` 12: foo::bar`
    let digits = indented.len()
        - indented
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    if digits > 0 && indented[digits..].starts_with(": ") {
        return true;
    }
    // rustc 源码摘录行（`23 | let x ...`）与指示线（`   |     ^^^`）
    if let Some(rest) = indented.get(digits..)
        && (rest.starts_with(" |") || rest.starts_with('|') || rest.starts_with('^'))
    {
        return true;
    }
    // jest 失败块标记
    if indented.starts_with('●') || indented.starts_with('✗') || indented.starts_with('✘') {
        return true;
    }
    let lower = t.to_lowercase();
    KEEP_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// 连续 Reducible 行区间（长度 ≥ RUN_MIN）
fn reducible_runs(lines: &[&str]) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if !is_keep_line(line) {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take()
            && i - s >= RUN_MIN
        {
            runs.push(s..i);
        }
    }
    if let Some(s) = start.take()
        && lines.len() - s >= RUN_MIN
    {
        runs.push(s..lines.len());
    }
    runs
}

pub(crate) fn detect(content: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    lines.len() >= MIN_LINES && !reducible_runs(&lines).is_empty()
}

pub(crate) fn compress(content: &str, env: &ExecutionEnv, allow_lossy: bool) -> String {
    if !allow_lossy {
        return content.to_string();
    }
    let lines: Vec<&str> = content.lines().collect();
    // 自卫：即使调用方绕过 detect()，也只在值得的日志上动手
    if lines.len() < MIN_LINES {
        return content.to_string();
    }
    let runs = reducible_runs(&lines);
    if runs.is_empty() || runs.len() > MAX_RANGES {
        return content.to_string();
    }
    // 有损必先 spill：spill 失败即放弃
    let Some(handle) = env.spill(content) else {
        return content.to_string();
    };

    let mut out: Vec<String> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        match runs.iter().find(|r| r.contains(&i)) {
            Some(run) => {
                let from_start = i - run.start;
                let to_end = run.end - 1 - i;
                if from_start < RUN_KEEP_HEAD || to_end < RUN_KEEP_TAIL {
                    out.push((*line).to_string());
                } else if from_start == RUN_KEEP_HEAD {
                    // 区间内首个被省略的位置：插入一次性计数标记
                    let omitted = run.len() - RUN_KEEP_HEAD - RUN_KEEP_TAIL;
                    out.push(format!(
                        "⟨… {omitted} lines elided (verbose build/test progress); full log: ctx:{handle} (expand)⟩"
                    ));
                }
                // 其余中间行丢弃
            }
            None => out.push((*line).to_string()),
        }
    }
    let joined = out.join("\n");
    if adopted(content, &joined) {
        joined
    } else {
        content.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with_store() -> (tempfile::TempDir, ExecutionEnv) {
        let dir = tempfile::tempdir().unwrap();
        let env = ExecutionEnv::new(".").with_ctx_store(dir.path());
        (dir, env)
    }

    /// cargo build 风格 fixture：大量 Compiling 行 + 夹在其中的 warning +
    /// 结尾 error 块（rustc 结构）
    fn cargo_fixture(compiling: usize) -> String {
        let mut out = String::from("   Compiling serde v1.0.104\n");
        for i in 0..compiling {
            out.push_str(&format!(
                "   Compiling crate-{i:02} v0.1.0 (path/to/crate-{i:02})\n"
            ));
            if i == compiling / 2 {
                out.push_str("warning: unused variable: `x`\n");
                out.push_str("  --> src/lib.rs:23:9\n");
                out.push_str("   |\n");
                out.push_str("23 |     let x = 5;\n");
                out.push_str(
                    "   |         ^ help: if this is intentional, prefix it with an underscore\n",
                );
            }
        }
        out.push_str("\nerror[E0308]: mismatched types\n");
        out.push_str("  --> src/main.rs:42:17\n");
        out.push_str("   |\n");
        out.push_str("42 |     let n: u32 = \"text\";\n");
        out.push_str("   |            ^^^ expected `u32`, found `&str`\n");
        out.push_str("\nerror: could not compile `app` (bin \"app\") due to 1 previous error\n");
        out
    }

    #[test]
    fn test_cargo_log_reduction_keeps_diagnostics_verbatim() {
        let content = cargo_fixture(40);
        let (_dir, env) = env_with_store();
        let out = compress(&content, &env, true);

        // 任务事实（PROBES）逐字存活
        for probe in [
            "error[E0308]: mismatched types",
            "  --> src/main.rs:42:17",
            "42 |     let n: u32 = \"text\";",
            "expected `u32`, found `&str`",
            "warning: unused variable: `x`",
            "error: could not compile `app`",
        ] {
            assert!(out.contains(probe), "probe missing: {probe}\n---\n{out}");
        }
        // 编译进度被缩减：每个区间保前 2 后 2，中间计数
        assert!(out.contains("lines elided"), "{out}");
        // 区间断言：warning 打断了 Compiling 连续段
        // 段 A = [serde, crate-00..crate-19]：头部保 serde + crate-00，尾部保 crate-18/19
        assert!(out.contains("   Compiling serde v1.0.104"));
        assert!(out.contains("   Compiling crate-00 v0.1.0"));
        assert!(out.contains("   Compiling crate-19 v0.1.0"));
        // 中段被省略（crate-01 在段 A 中部，crate-30 在段 B 中部）
        assert!(!out.contains("   Compiling crate-01"), "{out}");
        assert!(!out.contains("   Compiling crate-30"), "{out}");
        // 标记携带句柄且原文可取回
        assert!(out.contains("ctx:"));
        let handle = &out[out.find("ctx:").unwrap() + 4..][..16];
        assert_eq!(env.retrieve(handle).unwrap(), content);
        // 节省达门槛
        assert!(out.len() < content.len() * 85 / 100);
    }

    #[test]
    fn test_pytest_log_reduction() {
        let mut content = String::from(
            "============================= test session starts ==============================\n",
        );
        for i in 0..60 {
            content.push_str(&format!(
                "tests/test_file_{i:02}.py::test_case_{i:02} PASSED [ 12%]\n"
            ));
        }
        content.push_str("tests/test_file_60.py::test_case_60 FAILED [ 98%]\n");
        content.push_str("=== 1 failed, 60 passed, 3 skipped in 12.34s ===\n");
        let (_dir, env) = env_with_store();
        let out = compress(&content, &env, true);

        // 失败用例与汇总存活
        assert!(out.contains("test_case_60 FAILED"));
        assert!(out.contains("=== 1 failed, 60 passed, 3 skipped in 12.34s ==="));
        // PASSED 进度被缩减
        assert!(out.contains("lines elided"));
        assert!(!out.contains("test_case_30 PASSED"));
    }

    #[test]
    fn test_too_many_ranges_abandoned() {
        // keep 行把输出切成 >8 个可减区间 → 分类不可靠，整体放弃
        let mut content = String::new();
        for _ in 0..12 {
            for i in 0..10 {
                content.push_str(&format!("   Compiling crate-{i} v0.1.0\n"));
            }
            content.push_str("error: something failed here\n");
        }
        let (_dir, env) = env_with_store();
        let out = compress(&content, &env, true);
        assert_eq!(out, content, "too many ranges must abandon");
    }

    #[test]
    fn test_short_log_untouched() {
        let mut content = String::new();
        for i in 0..30 {
            content.push_str(&format!("   Compiling crate-{i} v0.1.0\n"));
        }
        assert!(!detect(&content));
        let (_dir, env) = env_with_store();
        assert_eq!(compress(&content, &env, true), content);
    }

    #[test]
    fn test_lossy_denied_for_failed_commands() {
        let mut content = String::new();
        for i in 0..60 {
            content.push_str(&format!("   Compiling crate-{i:02} v0.1.0\n"));
        }
        let (_dir, env) = env_with_store();
        assert_eq!(compress(&content, &env, false), content);
    }

    #[test]
    fn test_run_below_threshold_not_reduced() {
        // 连续 8 行 < RUN_MIN(9)：不构成可减区间
        let mut content = String::new();
        for _ in 0..10 {
            for i in 0..8 {
                content.push_str(&format!("   Compiling crate-{i} v0.1.0\n"));
            }
            content.push_str("warning: some warning\n");
        }
        assert!(!detect(&content));
    }

    #[test]
    fn test_keep_line_classification() {
        // 必须保留的结构行
        for keep in [
            "error[E0308]: mismatched types",
            "warning: unused import: `std::io`",
            "test result: FAILED. 3 passed; 12 failed",
            "   1:     0x561f - core::panic",
            "    at processTicksAndRejections (node:internal/task_runs:95:5)",
            "  File \"/app/main.py\", line 3, in <module>",
            "42 |     let n: u32 = \"text\";",
            "   |            ^^^ expected `u32`",
            "  --> src/main.rs:42:17",
            "note: an alias of an item is created",
            "help: consider removing this",
            "make: *** [Makefile:12: build] Error 1",
            "Tests:       2 failed, 45 passed, 1 skipped",
            "✗ should do the thing",
            "● suite › component › renders",
            "45 passed; 3 failed; 0 ignored; 0 measured",
            "    Finished `dev` profile [unoptimized + debuginfo]",
            "added 52 packages, and audited 53 packages in 4s",
        ] {
            assert!(is_keep_line(keep), "must keep: {keep}");
        }
        // 可减的进度行
        for reduce in [
            "   Compiling serde v1.0.104",
            "   Downloading index.crates.io-6f17d22bba15001f",
            "   Fresh libc v0.2.155",
            "test tests::it_works ... ok",
            "=== RUN   TestParse",
            "--- PASS: TestParse (0.00s)",
            "tests/test_a.py::test_b PASSED [ 12%]",
            "✓ renders the component",
            "=========== 1 skipped ===========",
        ] {
            assert!(!is_keep_line(reduce), "must reduce: {reduce}");
        }
    }
}
