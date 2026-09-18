//! 后台任务：jobs 注册表 + `jobs` 工具（T7）
//!
//! bash 的 `run_in_background` 把命令挂到后台：立即返回任务 id，
//! 输出合流写临时文件，watchdog 任务负责收尸（退出码落账）。
//! LLM 用 `jobs` 工具管理：list（状态/时长）/ output（读输出，走
//! 截断+spill 管道）/ stop（killpg 整组击杀）。
//!
//! 生命周期：注册表 Drop 时击杀全部仍在运行的任务并清理输出文件
//! （与前台 bash 的 GroupKillGuard 同一策略——TUI/headless 进程退出
//! 不留孤儿进程）。输出文件本身不设上限，但 `jobs output` 的交付
//! 走 `truncate_with_meta`（预算截断 + ctx store spill，可经 expand 取回）。

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 任务状态
#[derive(Debug, Clone, PartialEq)]
pub enum JobState {
    Running,
    /// 正常退出（None = 被信号杀死，无退出码）
    Finished(Option<i32>),
    /// 被 stop 击杀（finish 落账时保持该状态）
    Stopped,
}

impl JobState {
    /// 展示标签（TUI /tasks 列表用）
    pub fn label(&self) -> String {
        match self {
            Self::Running => "RUNNING".to_string(),
            Self::Finished(Some(code)) => format!("EXIT {code}"),
            Self::Finished(None) => "KILLED".to_string(),
            Self::Stopped => "STOPPED".to_string(),
        }
    }
}

/// 一条后台任务记录
#[derive(Debug, Clone)]
pub struct JobRecord {
    pub id: u32,
    pub command: String,
    pub started: Instant,
    /// 进程组 id（spawn 时 process_group(0)，stop 凭此 killpg）
    pub pgid: u32,
    pub output_file: PathBuf,
    pub state: JobState,
}

/// 进程内共享的后台任务注册表（BashTool 产生、JobsTool 消费）
#[derive(Default)]
pub struct JobRegistry {
    inner: Mutex<RegistryInner>,
    /// 实例序号：输出文件名去重（同进程多个注册表互不干扰，如并行测试）
    instance: u64,
}

/// 注册表实例计数器
static REGISTRY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Default)]
struct RegistryInner {
    next_id: u32,
    jobs: Vec<JobRecord>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self {
            instance: REGISTRY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
            inner: Mutex::new(RegistryInner::default()),
        }
    }

    /// 登记一条任务，返回 (id, 输出文件路径)。调用方随后 spawn 并 attach watchdog。
    pub fn create(&self, command: &str) -> (u32, PathBuf) {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = inner.next_id;
        let output_file = std::env::temp_dir().join(format!(
            "baiji-job-{}-{}-{id}.log",
            std::process::id(),
            self.instance
        ));
        // 占位记录（pgid 由 attach 补上；此间窗口极短，list 可见 RUNNING）
        inner.jobs.push(JobRecord {
            id,
            command: command.to_string(),
            started: Instant::now(),
            pgid: 0,
            output_file: output_file.clone(),
            state: JobState::Running,
        });
        (id, output_file)
    }

    /// 补记进程组 id（spawn 成功后调用）
    pub fn attach(&self, job_id: u32, pgid: u32) {
        self.with_job(job_id, |job| job.pgid = pgid);
    }

    /// watchdog 收尸：落账退出码（被 stop 的任务保持 Stopped 状态）
    pub fn mark_finished(&self, job_id: u32, code: Option<i32>) {
        self.with_job(job_id, |job| {
            if job.state == JobState::Running {
                job.state = JobState::Finished(code);
            }
        });
    }

    /// 击杀指定任务（killpg 整组；watchdog 随后观测到退出）。
    /// 返回 false = 不存在；已结束视为成功（幂等）。
    pub fn stop(&self, job_id: u32) -> bool {
        self.with_job(job_id, |job| {
            if job.state == JobState::Running {
                if job.pgid > 0 {
                    // SAFETY: killpg 只发信号；pgid 来自我们以 process_group(0) 启动的子进程
                    unsafe { libc::killpg(job.pgid as libc::pid_t, libc::SIGKILL) };
                }
                job.state = JobState::Stopped;
            }
        })
        .is_some()
    }

    /// 锁内访问指定任务（MutexGuard 不能跨借用返回，走闭包）
    fn with_job<R>(&self, job_id: u32, f: impl FnOnce(&mut JobRecord) -> R) -> Option<R> {
        let mut inner = self.inner.lock().unwrap();
        inner.jobs.iter_mut().find(|j| j.id == job_id).map(f)
    }

    /// 任务快照（list 渲染用）
    pub fn snapshot(&self) -> Vec<(u32, String, String, JobState)> {
        self.inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .map(|j| {
                (
                    j.id,
                    j.command.clone(),
                    format_elapsed(j.started),
                    j.state.clone(),
                )
            })
            .collect()
    }

    /// 输出文件路径（jobs output 用）
    pub fn output_file(&self, job_id: u32) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|j| j.id == job_id)
            .map(|j| j.output_file.clone())
    }
}

impl Drop for JobRegistry {
    fn drop(&mut self) {
        // 进程收尾：击杀全部仍在运行的任务（killpg），清理输出文件
        let jobs = std::mem::take(&mut self.inner.lock().unwrap().jobs);
        for job in jobs {
            if job.state == JobState::Running && job.pgid > 0 {
                unsafe { libc::killpg(job.pgid as libc::pid_t, libc::SIGKILL) };
            }
            std::fs::remove_file(&job.output_file).ok();
        }
    }
}

/// 运行时长格式化（"3m12s" / "45s"）
fn format_elapsed(started: Instant) -> String {
    let secs = started.elapsed().as_secs();
    if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// watchdog：等待子进程退出并落账（tokio::spawn 独立于工具 future——
/// run 被取消/结束后台任务继续；进程退出时 registry Drop 负责击杀）
pub(crate) fn spawn_watchdog(
    registry: Arc<JobRegistry>,
    job_id: u32,
    mut child: tokio::process::Child,
) {
    tokio::spawn(async move {
        let status = child.wait().await;
        registry.mark_finished(job_id, status.ok().and_then(|s| s.code()));
    });
}

/// `jobs` 工具：list / output / stop
pub struct JobsTool {
    env: Arc<crate::env::ExecutionEnv>,
    registry: Arc<JobRegistry>,
}

impl JobsTool {
    pub fn new(env: Arc<crate::env::ExecutionEnv>, registry: Arc<JobRegistry>) -> Self {
        Self { env, registry }
    }
}

#[async_trait]
impl AgentTool for JobsTool {
    fn name(&self) -> &str {
        "jobs"
    }

    fn description(&self) -> &str {
        "Manage background tasks started by bash with run_in_background. \
         Actions: 'list' (id, status, elapsed, command), 'output' (id — output so far, \
         truncated with a ctx: handle when large), 'stop' (id — kill the process group). \
         Use for long-running commands (dev servers, watchers, long builds)."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "output", "stop"]},
                "id": {"type": "integer", "description": "Job id (actions output/stop)"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let action = args["action"].as_str().unwrap_or_default();
        match action {
            "list" => {
                let jobs = self.registry.snapshot();
                if jobs.is_empty() {
                    return Ok(ToolOutput::ok("(no background jobs)"));
                }
                let lines: Vec<String> = jobs
                    .iter()
                    .map(|(id, command, elapsed, state)| {
                        format!("#{id}  {:<8} {elapsed:>6}  {command}", state.label())
                    })
                    .collect();
                Ok(ToolOutput::ok(lines.join("\n")))
            }
            "output" => {
                let Some(id) = args["id"].as_u64() else {
                    return Ok(ToolOutput::err("[Error] action=output requires an 'id'"));
                };
                let Some(path) = self.registry.output_file(id as u32) else {
                    return Ok(ToolOutput::err(format!(
                        "[Error] no job with id {id} — use action=list to see ids"
                    )));
                };
                // 尚无输出（文件未创建/为空）时读作空串
                let content = std::fs::read_to_string(&path).unwrap_or_default();
                let (delivered, original, original_tokens) = self.env.truncate_with_meta(&content);
                let mut out = ToolOutput::ok(delivered);
                if let Some(bytes) = original {
                    out = out.with_original_bytes(bytes);
                }
                if let Some(tokens) = original_tokens {
                    out = out.with_original_tokens(tokens);
                }
                Ok(out)
            }
            "stop" => {
                let Some(id) = args["id"].as_u64() else {
                    return Ok(ToolOutput::err("[Error] action=stop requires an 'id'"));
                };
                if self.registry.stop(id as u32) {
                    Ok(ToolOutput::ok(format!("stopped job #{id}")))
                } else {
                    Ok(ToolOutput::err(format!(
                        "[Error] no job with id {id} — use action=list to see ids"
                    )))
                }
            }
            other => Ok(ToolOutput::err(format!(
                "[Error] unknown action '{other}' (list/output/stop)"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ExecutionEnv;
    use crate::tools::{BashTool, JobRegistry as _unused};

    fn kit(dir: &std::path::Path) -> (BashTool, JobsTool, Arc<JobRegistry>) {
        let env = Arc::new(ExecutionEnv::new(dir));
        let registry = Arc::new(JobRegistry::new());
        (
            BashTool::with_jobs(env.clone(), registry.clone()),
            JobsTool::new(env, registry.clone()),
            registry,
        )
    }

    #[tokio::test]
    async fn test_background_start_list_stop() {
        let dir = tempfile::tempdir().unwrap();
        let (bash, jobs, _registry) = kit(dir.path());

        // 后台起 sleep：立即返回任务 id
        let started = std::time::Instant::now();
        let out = bash
            .execute(serde_json::json!({"command": "sleep 30", "run_in_background": true}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "must return immediately"
        );
        assert!(out.content.contains("job #1"), "{}", out.content);

        // list 可见 RUNNING
        let out = jobs
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(out.content.contains("#1"), "{}", out.content);
        assert!(out.content.contains("RUNNING"), "{}", out.content);
        assert!(out.content.contains("sleep 30"), "{}", out.content);

        // stop → STOPPED
        let out = jobs
            .execute(serde_json::json!({"action": "stop", "id": 1}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let out = jobs
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(out.content.contains("STOPPED"), "{}", out.content);
        assert!(!out.content.contains("RUNNING"), "{}", out.content);
    }

    #[tokio::test]
    async fn test_background_output_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let (bash, jobs, _registry) = kit(dir.path());

        bash.execute(serde_json::json!({
            "command": "echo hello-from-bg; sleep 1",
            "run_in_background": true
        }))
        .await
        .unwrap();

        // 运行中读输出：echo 已落盘
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let out = jobs
            .execute(serde_json::json!({"action": "output", "id": 1}))
            .await
            .unwrap();
        assert!(out.content.contains("hello-from-bg"), "{}", out.content);

        // 等待自然退出 → EXIT 0（轮询至多 5s）
        let mut exited = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let list = jobs
                .execute(serde_json::json!({"action": "list"}))
                .await
                .unwrap();
            if list.content.contains("EXIT 0") {
                exited = true;
                break;
            }
        }
        assert!(exited, "job should finish with EXIT 0");
        // 结束后输出仍可读
        let out = jobs
            .execute(serde_json::json!({"action": "output", "id": 1}))
            .await
            .unwrap();
        assert!(out.content.contains("hello-from-bg"));
    }

    #[tokio::test]
    async fn test_jobs_error_paths() {
        let dir = tempfile::tempdir().unwrap();
        let (_bash, jobs, _registry) = kit(dir.path());

        let out = jobs
            .execute(serde_json::json!({"action": "output", "id": 42}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no job with id 42"));

        let out = jobs
            .execute(serde_json::json!({"action": "stop", "id": 42}))
            .await
            .unwrap();
        assert!(out.is_error);

        let out = jobs
            .execute(serde_json::json!({"action": "destroy"}))
            .await
            .unwrap();
        assert!(out.is_error);

        // 空 list
        let out = jobs
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert_eq!(out.content, "(no background jobs)");
    }

    #[tokio::test]
    async fn test_foreground_unaffected_without_flag() {
        let dir = tempfile::tempdir().unwrap();
        let (bash, jobs, _registry) = kit(dir.path());

        // 不带 run_in_background：前台同步执行，不产生后台任务
        let out = bash
            .execute(serde_json::json!({"command": "echo fg"}))
            .await
            .unwrap();
        assert!(out.content.contains("exit: 0"));
        assert!(out.content.contains("fg"));
        let list = jobs
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert_eq!(list.content, "(no background jobs)");
    }
}
