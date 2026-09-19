//! bash 工具：在 workdir 中执行 shell 命令（带超时与输出截断）

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput, estimate_text_tokens};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

use crate::compressors;
use crate::env::ExecutionEnv;

/// 单条命令允许的最大超时
const MAX_TIMEOUT_MS: u64 = 300_000;
/// 每个输出流在内存中最多保留的字节数（超出部分读走即丢，防止 `yes` 类命令撑爆内存）
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
/// 主进程退出后等待管道读空的宽限期。后台子进程（`server &`）会一直持有管道，
/// 不能等 EOF，否则调用会挂到超时
pub(crate) const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(300);

/// 有上限的流捕获：持续读取（避免子进程因管道写满而阻塞），只保留前 `MAX_CAPTURE_BYTES`
#[derive(Default)]
pub(crate) struct Captured {
    data: Vec<u8>,
    total: usize,
}

type SharedCapture = Arc<std::sync::Mutex<Captured>>;

pub(crate) fn spawn_capture<R>(reader: Option<R>) -> (SharedCapture, tokio::task::JoinHandle<()>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let shared: SharedCapture = Arc::default();
    let sink = shared.clone();
    let handle = tokio::spawn(async move {
        let Some(mut reader) = reader else { return };
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut captured = sink.lock().unwrap();
                    captured.total += n;
                    let room = MAX_CAPTURE_BYTES.saturating_sub(captured.data.len());
                    captured.data.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
    });
    (shared, handle)
}

/// 等读取任务在宽限期内结束；超时则放弃（后台进程仍持有管道），取已读到的内容
pub(crate) async fn finish_capture(
    shared: SharedCapture,
    mut handle: tokio::task::JoinHandle<()>,
    grace: Duration,
) -> (String, usize) {
    if tokio::time::timeout(grace, &mut handle).await.is_err() {
        handle.abort();
    }
    let captured = shared.lock().unwrap();
    let mut text = String::from_utf8_lossy(&captured.data).into_owned();
    if captured.total > captured.data.len() {
        text.push_str(&format!(
            "\n[output capped: kept first {} of {} bytes]",
            captured.data.len(),
            captured.total
        ));
    }
    (text, captured.total)
}

/// 进程组击杀守卫：超时、或 future 被丢弃（取消）时 SIGKILL 整个进程组，
/// 连同 `npm test` 这类命令派生的孙进程一起清理。正常退出后 disarm——
/// 用户有意放到后台的进程不受影响。
pub(crate) struct GroupKillGuard(Option<u32>);

impl GroupKillGuard {
    pub(crate) fn new(pgid: Option<u32>) -> Self {
        Self(pgid)
    }

    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.0 {
            // SAFETY: killpg 只发送信号；pgid 来自我们以 process_group(0) 启动的子进程
            unsafe {
                libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

/// 压缩 shell 输出（启发式规则）：
/// - 删除进度/噪声行（spinner、进度条、npm reify、纯符号行）
/// - 连续重复行折叠为一次 + `⟨… repeated N×⟩`（编译警告的经典形态）
/// - 连续空行折叠为单空行
pub(crate) fn compress_shell_output(output: &str) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut prev: Option<String> = None;
    let mut repeat = 0usize;

    fn flush(result: &mut Vec<String>, prev: &Option<String>, repeat: usize) {
        if let Some(p) = prev {
            result.push(p.clone());
            // 空行的折叠不加标记（本身无信息量）
            if repeat > 1 && !p.trim().is_empty() {
                result.push(format!("⟨… repeated {repeat}×⟩"));
            }
        }
    }

    for line in output.lines() {
        if is_noise_line(line) {
            continue;
        }
        let normalized = line.trim_end().to_string();
        if Some(&normalized) == prev.as_ref() {
            repeat += 1;
        } else {
            flush(&mut result, &prev, repeat);
            prev = Some(normalized);
            repeat = 1;
        }
    }
    flush(&mut result, &prev, repeat);
    result.join("\n")
}

/// 进度/噪声行判定。**宁可漏删也不误删**：测试汇总（`FAILED 3/12 tests`）、
/// diff 分隔线（`---`）、Markdown/表格线都是模型需要的信息。
/// 只删"整行除了进度什么都没有"的行。
fn is_noise_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false; // 空行保留（由折叠逻辑处理）
    }
    let is_braille = |c: char| ('\u{2800}'..='\u{28FF}').contains(&c);
    let is_block = |c: char| {
        matches!(
            c,
            '█' | '▓' | '▒' | '░' | '▏' | '▎' | '▍' | '▌' | '▋' | '▊' | '▉'
        )
    };

    // 纯 spinner / 方块进度条（必须含 braille 或方块字符；`---`、`===`、`...` 不算）
    if t.chars().any(|c| is_braille(c) || is_block(c))
        && t.chars()
            .all(|c| is_braille(c) || is_block(c) || matches!(c, ' ' | '|' | '[' | ']'))
    {
        return true;
    }
    // spinner 帧行：以 braille 转轮字符开头 + 空格 + 文本
    if let Some(rest) = t.strip_prefix(is_braille) {
        if rest.starts_with(' ') {
            return true;
        }
    }
    // npm 安装进度
    if t.starts_with("reify:") {
        return true;
    }
    // ASCII 进度条行："[====>    ] 45%"、"[####  ] 12/26"
    if let (Some(open), Some(close)) = (t.find('['), t.find(']')) {
        let bar = t.get(open + 1..close).unwrap_or("");
        if bar.len() >= 3
            && bar
                .chars()
                .all(|c| matches!(c, '=' | '>' | '#' | '-' | '.' | ' '))
        {
            return true;
        }
    }
    // 整行只由进度 token 组成："45%"、"12/26"、"45% 12/26"。
    // 含任何其它单词（FAILED / passed / 日期上下文）的行一律保留。
    let is_progress_token = |tok: &str| {
        let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.');
        tok.strip_suffix('%').is_some_and(digits)
            || tok
                .split_once('/')
                .is_some_and(|(a, b)| digits(a) && digits(b))
    };
    t.split_whitespace().all(is_progress_token)
}

pub struct BashTool {
    env: Arc<ExecutionEnv>,
    /// 后台任务注册表（run_in_background 产生；jobs 工具消费）
    jobs: Arc<crate::tools::jobs::JobRegistry>,
}

impl BashTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self {
            env,
            jobs: Arc::new(crate::tools::jobs::JobRegistry::new()),
        }
    }

    /// 与 jobs 工具共享注册表（builtin_tools 装配用；new() 为隔离注册表）
    pub fn with_jobs(env: Arc<ExecutionEnv>, jobs: Arc<crate::tools::jobs::JobRegistry>) -> Self {
        Self { env, jobs }
    }
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command in the working directory. Returns exit code, stdout and stderr. \
         Commands are killed after a timeout (default 30s). Set run_in_background=true for \
         long-running commands (dev servers, watchers): returns a job id immediately; manage it \
         with the jobs tool (list/output/stop)."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 30000, max 300000)"},
                "run_in_background": {"type": "boolean", "description": "Detach: return a job id immediately (no timeout); manage via the jobs tool"}
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(command) = args["command"].as_str() else {
            return Ok(ToolOutput::err(
                "[Error] missing required argument 'command'",
            ));
        };
        // 后台模式：登记任务、重定向输出到文件、spawn 后立即返回
        // （watchdog 收尸；无超时——stop 由 jobs 工具触发）
        if args["run_in_background"].as_bool().unwrap_or(false) {
            let (job_id, output_file) = self.jobs.create(command);
            let stdout_file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&output_file);
            let stderr_file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&output_file);
            let (Ok(stdout_file), Ok(stderr_file)) = (stdout_file, stderr_file) else {
                return Ok(ToolOutput::err(format!(
                    "[Error] opening background output file {}: io error",
                    output_file.display()
                )));
            };
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg(command)
                .current_dir(&self.env.workdir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::from(stdout_file))
                .stderr(std::process::Stdio::from(stderr_file));
            #[cfg(unix)]
            cmd.process_group(0);
            cmd.kill_on_drop(true);
            return match cmd.spawn() {
                Ok(child) => {
                    self.jobs.attach(job_id, child.id().unwrap_or(0));
                    crate::tools::jobs::spawn_watchdog(self.jobs.clone(), job_id, child);
                    Ok(ToolOutput::ok(format!(
                        "[Background] job #{job_id} started: {command} \
                         — use jobs list / jobs output {job_id} / jobs stop {job_id} to manage"
                    )))
                }
                Err(e) => Ok(ToolOutput::err(format!("[Error] spawning command: {e}"))),
            };
        }

        let timeout_ms = args["timeout_ms"]
            .as_u64()
            .unwrap_or(self.env.command_timeout.as_millis() as u64)
            .min(MAX_TIMEOUT_MS);
        let timeout = Duration::from_millis(timeout_ms.max(1));

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&self.env.workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // 独立进程组：超时/取消时可整组击杀，不残留孙进程
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return Ok(ToolOutput::err(format!("[Error] spawning command: {e}"))),
        };
        let mut guard = GroupKillGuard(child.id());
        let (out_buf, out_task) = spawn_capture(child.stdout.take());
        let (err_buf, err_task) = spawn_capture(child.stderr.take());

        // 等主进程退出（而不是等管道 EOF：后台子进程会一直持有管道）
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => return Ok(ToolOutput::err(format!("[Error] waiting for command: {e}"))),
            Err(_) => {
                drop(guard); // SIGKILL 整个进程组
                child.wait().await.ok(); // 回收，避免僵尸进程
                let (stdout, _) = finish_capture(out_buf, out_task, PIPE_DRAIN_GRACE).await;
                let (stderr, _) = finish_capture(err_buf, err_task, PIPE_DRAIN_GRACE).await;
                let mut message = format!(
                    "[Timeout] command killed after {}ms: {}",
                    timeout_ms, command
                );
                for (label, text) in [("stdout", stdout), ("stderr", stderr)] {
                    let text = compress_shell_output(&text);
                    if !text.is_empty() {
                        message.push_str(&format!("\n\n[partial {label}]\n{text}"));
                    }
                }
                return Ok(ToolOutput::err(self.env.truncate_output(&message)));
            }
        };
        guard.disarm();

        let (stdout, stdout_total) = finish_capture(out_buf, out_task, PIPE_DRAIN_GRACE).await;
        let (stderr, stderr_total) = finish_capture(err_buf, err_task, PIPE_DRAIN_GRACE).await;
        let raw_bytes = stdout_total + stderr_total;
        // 反事实基准：不做任何压缩时会发送多少 token（对原始管道文本估算）
        let raw_tokens = (estimate_text_tokens(&stdout) + estimate_text_tokens(&stderr)) as u64;
        let success = status.success();

        // 输出压缩，三级管道：
        // 1) ANSI 剥离（无损）
        // 2) 通用规则：噪声行过滤 + 连续重复行折叠
        // 3) 内容感知域压缩：JSON / 表格 / 构建日志。
        //    失败命令只做无损清理——诊断上下文宁可多给（有损必须可经 expand 取回）
        let stdout = compressors::compress(
            &compress_shell_output(&compressors::strip_ansi(&stdout)),
            &self.env,
            success,
        );
        let stderr = compressors::compress(
            &compress_shell_output(&compressors::strip_ansi(&stderr)),
            &self.env,
            success,
        );

        let mut combined = format!("exit: {}\n", status.code().unwrap_or(-1));
        if !stdout.is_empty() {
            combined.push_str(&format!("\n[stdout]\n{stdout}"));
        }
        if !stderr.is_empty() {
            combined.push_str(&format!("\n[stderr]\n{stderr}"));
        }

        let (delivered, truncated_at, truncated_tokens) = self.env.truncate_with_meta(&combined);
        // 台账口径：原始管道字节/token 是"不压缩会发送多少"的反事实基准；
        // 截断时的预截断长度取两者较大值
        let original = truncated_at.unwrap_or(0).max(raw_bytes as u64);
        let original_tokens = truncated_tokens.unwrap_or(0).max(raw_tokens);
        let mut result = if (delivered.len() as u64) < original {
            ToolOutput::ok(delivered)
                .with_original_bytes(original)
                .with_original_tokens(original_tokens)
        } else {
            ToolOutput::ok(delivered)
        };
        result.is_error = !success;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bash_success() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"command": "echo hello && echo err >&2"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("exit: 0"));
        assert!(out.content.contains("hello"));
        assert!(out.content.contains("err"));
    }

    #[tokio::test]
    async fn test_bash_nonzero_exit_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"command": "exit 3"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("exit: 3"));
    }

    #[tokio::test]
    async fn test_bash_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"command": "sleep 5", "timeout_ms": 100}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("[Timeout]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_bash_timeout_kills_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        // 子 shell 里的孙进程 2 秒后写标记文件；超时击杀整个进程组后它不应存活
        let out = tool
            .execute(serde_json::json!({
                "command": "(sleep 2; touch survived) & wait",
                "timeout_ms": 200
            }))
            .await
            .unwrap();
        assert!(out.content.contains("[Timeout]"));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !dir.path().join("survived").exists(),
            "grandchild survived timeout"
        );
    }

    #[tokio::test]
    async fn test_bash_background_process_does_not_hang() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        // 后台进程继承并持有 stdout；调用应在主 shell 退出后很快返回
        let started = std::time::Instant::now();
        let out = tool
            .execute(serde_json::json!({"command": "sleep 20 & echo started"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("started"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn test_bash_output_is_capped_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        // ~4MB 不重复输出：内存只保留 1MB，并标注总量
        let out = tool
            .execute(serde_json::json!({"command": "seq 1 600000"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.original_bytes.unwrap() > MAX_CAPTURE_BYTES as u64);
    }

    #[tokio::test]
    async fn test_bash_output_compression_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        // 连续重复行（编译警告形态）+ 噪声行
        let script = "printf 'warning: unused var\\nwarning: unused var\\nwarning: unused var\\nreify:iterables@2.0.2\\n45%% 12/26\\nok\\n'";
        let out = tool
            .execute(serde_json::json!({"command": script}))
            .await
            .unwrap();
        assert!(!out.is_error);
        // 重复折叠（3 次相同行）
        assert!(out.content.contains("⟨… repeated 3×⟩"), "{}", out.content);
        // 噪声行被删
        assert!(!out.content.contains("reify:"));
        assert!(!out.content.contains("12/26"));
        // 台账记录了原始字节
        assert!(out.original_bytes.is_some());
        assert!(out.bytes_saved() > 0);
    }

    #[tokio::test]
    async fn test_bash_json_domain_compression_with_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let env = Arc::new(ExecutionEnv::new(dir.path()).with_ctx_store(store.path()));
        let tool = BashTool::new(env.clone());

        // 紧凑 JSON + 60 元素数组：无损段无可省 → 有损段截数组 + spill
        let items: Vec<serde_json::Value> = (0..60)
            .map(|i| serde_json::json!({"id": i, "name": format!("record-number-{i}")}))
            .collect();
        let compact = serde_json::to_string(&serde_json::json!({"records": items})).unwrap();
        std::fs::write(dir.path().join("data.json"), &compact).unwrap();

        let out = tool
            .execute(serde_json::json!({"command": "cat data.json"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("more item(s) elided"),
            "{}",
            out.content
        );
        assert!(out.content.contains("ctx:"), "must carry recovery handle");
        assert!(out.content.contains("record-number-0\""));
        assert!(out.content.contains("record-number-59\""));
        assert!(!out.content.contains("record-number-45\""));
        // 台账：域压缩计入原始字节
        assert!(out.original_bytes.is_some());
        assert!(out.bytes_saved() > 0);
        // 原文可经 expand 底层路径完整取回
        let handle = &out.content[out.content.find("ctx:").unwrap() + 4..][..16];
        assert_eq!(env.retrieve(handle).unwrap(), compact);
    }

    #[tokio::test]
    async fn test_bash_failed_command_skips_lossy_compression() {
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Arc::new(
            ExecutionEnv::new(dir.path()).with_ctx_store(store.path()),
        ));

        let items: Vec<serde_json::Value> = (0..60)
            .map(|i| serde_json::json!({"id": i, "name": format!("record-number-{i}")}))
            .collect();
        let compact = serde_json::to_string(&serde_json::json!({"records": items})).unwrap();
        std::fs::write(dir.path().join("data.json"), &compact).unwrap();

        // exit=1：诊断上下文宁可多给——只做无损清理，不做有损截断
        let out = tool
            .execute(serde_json::json!({"command": "cat data.json; exit 1"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(!out.content.contains("elided"), "{}", out.content);
        assert!(!out.content.contains("ctx:"));
        assert_eq!(out.content.matches("record-number-").count(), 60);
    }

    #[tokio::test]
    async fn test_bash_build_log_domain_compression() {
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Arc::new(
            ExecutionEnv::new(dir.path()).with_ctx_store(store.path()),
        ));

        let out = tool
            .execute(serde_json::json!({
                "command": "for i in $(seq 0 59); do echo \"   Compiling crate-$i v0.1.0\"; done"
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // 区间保前 2 后 2，中间计数；标记携带句柄
        assert!(
            out.content.contains("Compiling crate-0 v0.1.0"),
            "{}",
            out.content
        );
        assert!(out.content.contains("Compiling crate-59 v0.1.0"));
        assert!(!out.content.contains("Compiling crate-30"));
        assert!(out.content.contains("lines elided"));
        assert!(out.content.contains("ctx:"));
        assert!(out.original_bytes.is_some());
        assert!(out.bytes_saved() > 0);
    }

    #[tokio::test]
    async fn test_bash_ansi_codes_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Arc::new(ExecutionEnv::new(dir.path())));

        let out = tool
            .execute(serde_json::json!({"command": "printf '\\033[32mok\\033[0m done\\n'"}))
            .await
            .unwrap();
        assert!(out.content.contains("ok done"), "{}", out.content);
        assert!(
            !out.content.contains('\x1b'),
            "ANSI escape must be stripped"
        );
    }

    #[test]
    fn test_compress_shell_output_rules() {
        // 连续重复折叠（3 行相同 → 1 行 + 总次数标记）
        let input = "A\nA\nA\nB\n";
        assert_eq!(compress_shell_output(input), "A\n⟨… repeated 3×⟩\nB");

        // 噪声行删除
        let input = "⠋ building\n⠙ building\nreal line";
        assert_eq!(compress_shell_output(input), "real line");

        // 空行折叠（连续多个空行 → 单个）
        let input = "x\n\n\n\ny";
        assert_eq!(compress_shell_output(input), "x\n\ny");

        // 不连续的重复不折叠
        let input = "A\nB\nA";
        assert_eq!(compress_shell_output(input), "A\nB\nA");

        // 曾被误删的有信息行：测试汇总、日期、diff/Markdown 分隔线
        for keep in [
            "FAILED 3/12 tests",
            "test result: 1/2 passed",
            "released 2024/05",
            "---",
            "===",
            "-",
            "| a | b |",
            "...",
            "coverage: 45%",
        ] {
            assert_eq!(compress_shell_output(keep), keep, "must keep: {keep}");
        }
        // 纯进度行仍然删除
        for noise in [
            "45% 12/26",
            "12/26",
            "[====>     ] 45%",
            "████░░░░",
            "[###   ] 3/9",
        ] {
            assert_eq!(compress_shell_output(noise), "", "must drop: {noise}");
        }

        // 有信息的行保留（git/npm 树、百分比以外的数字）
        assert_eq!(
            compress_shell_output("12 files changed"),
            "12 files changed"
        );
        assert_eq!(
            compress_shell_output("├── left-pad@1.0.0"),
            "├── left-pad@1.0.0"
        );
    }
}
