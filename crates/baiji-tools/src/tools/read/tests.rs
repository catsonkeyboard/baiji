//! ReadTool 集成测试（density / outline_fallback / walk_map 的纯函数测试在各自子模块）

use super::*;

async fn setup() -> (tempfile::TempDir, ReadTool) {
    let dir = tempfile::tempdir().unwrap();
    let tool = ReadTool::new(Arc::new(ExecutionEnv::new(dir.path())));
    (dir, tool)
}

#[tokio::test]
async fn test_read_full_with_line_numbers_and_range() {
    let (dir, tool) = setup().await;
    let file = dir.path().join("a.txt");
    fs::write(&file, "one\ntwo\nthree\n").await.unwrap();

    let out = tool
        .execute(serde_json::json!({"path": "a.txt"}))
        .await
        .unwrap();
    assert!(out.content.contains("1\tone"));
    assert!(out.content.contains("3\tthree"));

    let out = tool
        .execute(serde_json::json!({"path": "a.txt", "offset": 2, "limit": 1}))
        .await
        .unwrap();
    assert_eq!(out.content, "2\ttwo");
}

#[tokio::test]
async fn test_read_signatures_mode() {
    let (dir, tool) = setup().await;
    let file = dir.path().join("lib.rs");
    let code = "// header comment\nfn helper() {}\n\npub struct Config {\n    x: u32,\n}\n\nimpl Config {\n    pub fn new() -> Self { Self { x: 1 } }\n}\n\nasync fn main() {}\n";
    fs::write(&file, code).await.unwrap();

    let out = tool
        .execute(serde_json::json!({"path": "lib.rs", "mode": "signatures"}))
        .await
        .unwrap();
    assert!(!out.is_error);
    // 每个符号带行区间锚点 + kind + 名称
    assert!(out.content.contains("L2-2"), "{}", out.content);
    assert!(out.content.contains("fn helper"));
    assert!(out.content.contains("struct Config"));
    assert!(out.content.contains("impl Config"));
    assert!(out.content.contains("fn new"));
    // 行区间完整（impl 块区间）
    assert!(
        out.content.contains("L8-10"),
        "impl spans to its closing brace: {}",
        out.content
    );
    // 注释行不是符号
    assert!(!out.content.contains("header comment"));
}

#[tokio::test]
async fn test_read_map_mode() {
    let (dir, tool) = setup().await;
    std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    std::fs::write(dir.path().join("src/nested/util.rs"), "fn x() {}").unwrap();
    std::fs::write(dir.path().join("README.md"), "hi").unwrap();
    std::fs::create_dir_all(dir.path().join("target")).unwrap(); // 构建产物跳过

    let out = tool
        .execute(serde_json::json!({"path": ".", "mode": "map"}))
        .await
        .unwrap();
    assert!(out.content.contains("src/"));
    assert!(out.content.contains("nested/"));
    assert!(out.content.contains("util.rs"));
    assert!(out.content.contains("README.md"));
    assert!(!out.content.contains("target/"));
}

#[tokio::test]
async fn test_read_mode_mismatch_errors() {
    let (dir, tool) = setup().await;
    std::fs::create_dir_all(dir.path().join("d")).unwrap();
    std::fs::write(dir.path().join("f.txt"), "x\n").unwrap();

    // signatures 用于目录 → 提示用 map
    let out = tool
        .execute(serde_json::json!({"path": "d", "mode": "signatures"}))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("mode=map"));

    // full 用于目录 → 提示用 map
    let out = tool
        .execute(serde_json::json!({"path": "d"}))
        .await
        .unwrap();
    assert!(out.is_error);

    // map 用于文件 → 返回摘要提示
    let out = tool
        .execute(serde_json::json!({"path": "f.txt", "mode": "map"}))
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("is a file"));
}

#[tokio::test]
async fn test_read_over_budget_reports_original_bytes() {
    let (dir, tool) = setup().await;
    let big: String = "0123456789\n".repeat(5000); // 55KB
    let file = dir.path().join("big.txt");
    fs::write(&file, &big).await.unwrap();
    let out = tool
        .execute(serde_json::json!({"path": "big.txt"}))
        .await
        .unwrap();
    // 默认 density_fallback 开启：超预算走自动熵降级（不再硬截断）
    assert!(
        out.content.contains("[auto-density"),
        "got: {}",
        out.content.lines().next().unwrap()
    );
    assert!(out.original_bytes.is_some());
    assert!(out.bytes_saved() > 0);
}

#[tokio::test]
async fn test_read_missing_and_policy() {
    let (_dir, tool) = setup().await;

    let out = tool
        .execute(serde_json::json!({"path": "missing.txt"}))
        .await
        .unwrap();
    assert!(out.is_error);

    let out = tool
        .execute(serde_json::json!({"path": "../../etc/passwd"}))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("[Policy denied]"));
}

#[tokio::test]
async fn test_read_explicit_density_and_auto_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let env = ExecutionEnv::new(dir.path())
        .with_ctx_store(dir.path().join("ctx"))
        .with_max_output_bytes(8 * 1024); // 显式小预算，确定触发降级
    let tool = ReadTool::new(Arc::new(env));

    // 显式 density：无论预算都按比例熵选行
    let content = (0..100)
        .map(|i| format!("line {i:03}: let value_{i} = compute(input_{i}, factor)?;\n"))
        .collect::<String>();
    let file = dir.path().join("mid.rs");
    fs::write(&file, &content).await.unwrap();

    let out = tool
        .execute(serde_json::json!({"path": "mid.rs", "density": 0.1}))
        .await
        .unwrap();
    assert!(
        out.content.contains("[density 10%"),
        "{}",
        out.content.lines().next().unwrap()
    );
    assert!(out.original_bytes.is_some());
    assert!(out.bytes_saved() > 0);

    // 超预算未指定 density → 自动降级（不硬截断）
    let content = (0..200)
        .map(|i| format!("line {i:03}: let value_{i} = compute(input_{i}, factor, modifier_{i}, extra_padding)?;\n"))
        .collect::<String>();
    let file = dir.path().join("big.rs");
    fs::write(&file, &content).await.unwrap();

    let out = tool
        .execute(serde_json::json!({"path": "big.rs"}))
        .await
        .unwrap();
    assert!(
        out.content.contains("[auto-density"),
        "should auto-degrade, got: {}",
        out.content.lines().next().unwrap()
    );
    // 密度选行优先；若仍略超预算，CCR 截断兜底（两者可共存）
    assert!(out.original_bytes.is_some());
}

#[tokio::test]
async fn test_read_auto_fallback_can_be_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let env = ExecutionEnv::new(dir.path())
        .without_density_fallback()
        .with_ctx_store(dir.path().join("ctx"))
        .with_max_output_bytes(8 * 1024);
    let tool = ReadTool::new(Arc::new(env));

    let content = (0..200)
        .map(|i| format!("line {i:03}: let value_{i} = compute(input_{i}, factor, modifier_{i}, extra_padding)?;\n"))
        .collect::<String>();
    let file = dir.path().join("big.rs");
    fs::write(&file, &content).await.unwrap();

    let out = tool
        .execute(serde_json::json!({"path": "big.rs"}))
        .await
        .unwrap();
    // 关闭降级 → 回到 CCR 硬截断（带句柄）
    assert!(!out.content.contains("[auto-density"));
    assert!(
        out.content.contains("full content handle"),
        "expected CCR handle, got: {}",
        out.content.lines().last().unwrap()
    );
}

// ===== 缓存重读 =====

fn cached_env(dir: &tempfile::TempDir) -> Arc<ExecutionEnv> {
    Arc::new(ExecutionEnv::new(dir.path()).with_ctx_store(dir.path().join("ctx")))
}

async fn big_file(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    let content = (0..100)
        .map(|i| format!("line {i:03}: let value_{i} = compute(input_{i}, factor)?;\n"))
        .collect::<String>();
    let file = dir.path().join(name);
    fs::write(&file, content).await.unwrap();
    file
}

#[tokio::test]
async fn test_cached_reread_returns_stub_with_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let env = cached_env(&dir);
    let tool = ReadTool::new(env.clone());
    big_file(&dir, "a.rs").await;

    let first = tool
        .execute(serde_json::json!({"path": "a.rs"}))
        .await
        .unwrap();
    assert!(!first.content.contains("[unchanged:"));
    assert!(first.content.len() >= MIN_CACHE_BYTES);

    // 同参数重读：短引用 stub（带句柄 + 台账）
    let second = tool
        .execute(serde_json::json!({"path": "a.rs"}))
        .await
        .unwrap();
    assert!(second.content.contains("[unchanged:"), "{}", second.content);
    assert!(second.content.contains("ctx:"));
    assert!(second.original_bytes.is_some());
    assert_eq!(second.original_bytes, Some(first.content.len() as u64));
    assert!(second.bytes_saved() > 0);
    assert!(second.content.len() < first.content.len() / 10);

    // 上次交付可逐字取回（expand 底层路径）
    let handle = &second.content[second.content.find("ctx:").unwrap() + 4..][..16];
    assert_eq!(env.retrieve(handle).unwrap(), first.content);
}

#[tokio::test]
async fn test_cache_invalidated_by_modification() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ReadTool::new(cached_env(&dir));
    let file = big_file(&dir, "b.rs").await;

    let first = tool
        .execute(serde_json::json!({"path": "b.rs"}))
        .await
        .unwrap();
    assert!(!first.content.contains("[unchanged:"));

    // 修改（尺寸不同 → mtime+size 双保险失效）
    let bigger: String = (0..150)
        .map(|i| format!("line {i:03}: let value_{i} = compute(input_{i}, factor, extra)?;\n"))
        .collect::<String>();
    fs::write(&file, bigger).await.unwrap();

    let reread = tool
        .execute(serde_json::json!({"path": "b.rs"}))
        .await
        .unwrap();
    assert!(
        !reread.content.contains("[unchanged:"),
        "must re-read after change"
    );
    assert!(reread.content.contains("line 149"));
}

#[tokio::test]
async fn test_cache_distinguishes_request_params() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ReadTool::new(cached_env(&dir));
    big_file(&dir, "c.rs").await;

    let _ = tool
        .execute(serde_json::json!({"path": "c.rs"}))
        .await
        .unwrap();
    // 不同参数是不同请求：完整读取，不命中缓存（limit=100 保证输出过缓存阈值）
    let paged = tool
        .execute(serde_json::json!({"path": "c.rs", "offset": 1, "limit": 100}))
        .await
        .unwrap();
    assert!(!paged.content.contains("[unchanged:"));
    assert!(paged.content.contains("10\tline 009"));
    // 再来一次同样的分页 → 命中
    let paged_again = tool
        .execute(serde_json::json!({"path": "c.rs", "offset": 1, "limit": 100}))
        .await
        .unwrap();
    assert!(
        paged_again.content.contains("[unchanged:"),
        "{}",
        paged_again.content
    );
}

#[tokio::test]
async fn test_small_files_never_stubbed() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ReadTool::new(cached_env(&dir));
    fs::write(dir.path().join("small.txt"), "tiny content\n")
        .await
        .unwrap();

    for _ in 0..2 {
        let out = tool
            .execute(serde_json::json!({"path": "small.txt"}))
            .await
            .unwrap();
        assert_eq!(out.content, "1\ttiny content");
    }
}

#[tokio::test]
async fn test_cache_requires_ctx_store() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ReadTool::new(Arc::new(ExecutionEnv::new(dir.path()))); // 无 ctx store
    big_file(&dir, "d.rs").await;

    let _ = tool
        .execute(serde_json::json!({"path": "d.rs"}))
        .await
        .unwrap();
    let second = tool
        .execute(serde_json::json!({"path": "d.rs"}))
        .await
        .unwrap();
    assert!(!second.content.contains("[unchanged:"));
}

#[tokio::test]
async fn test_signatures_reread_cached() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ReadTool::new(cached_env(&dir));
    // 足够多符号让大纲输出超过阈值
    let code: String = (0..80)
        .map(|i| format!("pub fn handler_{i}(input_{i}: u32, factor_{i}: u32) -> u32 {{ input_{i} + factor_{i} }}\n"))
        .collect::<String>();
    fs::write(dir.path().join("many.rs"), code).await.unwrap();

    let first = tool
        .execute(serde_json::json!({"path": "many.rs", "mode": "signatures"}))
        .await
        .unwrap();
    assert!(!first.content.contains("[unchanged:"));
    assert!(
        first.content.len() >= MIN_CACHE_BYTES,
        "{}",
        first.content.len()
    );

    let second = tool
        .execute(serde_json::json!({"path": "many.rs", "mode": "signatures"}))
        .await
        .unwrap();
    assert!(second.content.contains("[unchanged:"), "{}", second.content);
    assert!(second.original_bytes.is_some());
}
