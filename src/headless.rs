//! headless 一次性执行：流式输出到 stdout，工具活动到 stderr

use anyhow::Result;
use baiji_agent::{AgentEvent, SteeringQueue};
use baiji_harness::AgentHarness;
use tokio_util::sync::CancellationToken;

/// headless 运行结果
#[derive(Debug, Default)]
pub struct Outcome {
    /// 最终答案（已流式打印；保留给测试与后续的 --json 输出）
    #[cfg_attr(not(test), allow(dead_code))]
    pub answer: String,
    /// 被 Ctrl-C 中断
    pub cancelled: bool,
    /// 被确认策略 / hook 拒绝的工具调用数
    pub denied_tools: usize,
}

impl Outcome {
    /// 进程退出码：0 成功；2 有工具被拒绝（任务很可能没按预期完成）；130 被中断（128+SIGINT）
    pub fn exit_code(&self) -> i32 {
        if self.cancelled {
            130
        } else if self.denied_tools > 0 {
            2
        } else {
            0
        }
    }
}

/// 执行一条消息。
///
/// - 文本增量实时打印到 stdout（`quiet = true` 关闭，测试用）
/// - 工具开始/失败与运行错误打印到 stderr（不污染 stdout 的答案流）
/// - `cancel`：Ctrl-C 触发后优雅收尾（终止正在跑的命令、落盘已完成的轮次）
pub async fn run_once(
    mut harness: AgentHarness,
    input: &str,
    quiet: bool,
    cancel: CancellationToken,
) -> Result<Outcome> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

    let printer = tokio::spawn(async move {
        use std::io::Write as _;
        let mut denied = 0usize;
        while let Some(event) = rx.recv().await {
            if let AgentEvent::ToolFinished { output, .. } = &event
                && output.starts_with("[Denied")
            {
                denied += 1;
            }
            if quiet {
                continue;
            }
            match event {
                AgentEvent::TextDelta { text } => {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                AgentEvent::StreamRestarted => {
                    eprintln!("\n[retrying — the partial answer above is discarded]");
                }
                AgentEvent::ToolStarted { name, .. } => {
                    eprintln!("→ {name}");
                }
                AgentEvent::ToolFinished {
                    name,
                    output,
                    is_error: true,
                    ..
                } => {
                    let brief: String = output.chars().take(100).collect();
                    eprintln!("✗ {name}: {brief}");
                }
                AgentEvent::RunFailed { error } => {
                    eprintln!("[error] {error}");
                }
                AgentEvent::Interrupted => {
                    eprintln!("\n[interrupted]");
                }
                _ => {}
            }
        }
        denied
    });

    let result = harness
        .run(input, &tx, &cancel, &SteeringQueue::new())
        .await;
    drop(tx);
    let denied_tools = printer.await.unwrap_or(0);

    if !quiet && result.is_ok() {
        // 流式答案收尾换行
        println!();
    }
    if !quiet && denied_tools > 0 {
        eprintln!(
            "[warning] {denied_tools} tool call(s) were denied (headless 默认拒绝需确认的工具；加 -y 放行)"
        );
    }
    Ok(Outcome {
        answer: result?,
        cancelled: cancel.is_cancelled(),
        denied_tools,
    })
}

/// 打印会话列表（--sessions）。直接读存储，不创建新会话。
pub fn print_sessions(sessions_dir: &std::path::Path) -> Result<()> {
    let sessions = baiji_harness::JsonlStore::new(sessions_dir).list()?;
    if sessions.is_empty() {
        println!("（暂无会话，位于 ~/.baiji/sessions/）");
        return Ok(());
    }
    // 树形：根会话最新在前，分叉出的子会话缩进显示在其父之下
    let tree = baiji_harness::SessionTree::from_metas(sessions);
    println!("{:<28} {:<20} TITLE", "SESSION", "CREATED");
    for (depth, meta) in tree.flattened() {
        let branch = if depth > 0 {
            format!("{}└ ", "  ".repeat(depth - 1))
        } else {
            String::new()
        };
        println!(
            "{:<28} {:<20} {}{}",
            meta.id,
            &meta.created_at[..meta.created_at.len().min(19)],
            branch,
            meta.title.as_deref().unwrap_or("(无标题)"),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use baiji_agent::{AgentRuntime, ToolRegistry};
    use baiji_ai::{ChatRequest, ChatResponse, Protocol, Provider, StreamChunk};
    use futures::stream::BoxStream;
    use futures::StreamExt as _;
    use std::sync::Arc;

    struct EchoProvider;

    #[async_trait]
    impl Provider for EchoProvider {
        async fn chat(&self, _: ChatRequest) -> anyhow::Result<ChatResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> anyhow::Result<BoxStream<'static, anyhow::Result<StreamChunk>>> {
            Ok(futures::stream::iter(vec![
                Ok(StreamChunk::Content("答案".into())),
                Ok(StreamChunk::Done),
            ])
            .boxed())
        }
        fn protocol(&self) -> Protocol {
            Protocol::OpenAIChat
        }
        fn model(&self) -> &str {
            "mock"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[tokio::test]
    async fn test_headless_run_persists_session() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("sessions");

        let session_id = {
            let runtime =
                Arc::new(AgentRuntime::new(Arc::new(EchoProvider)).with_tools(ToolRegistry::new()));
            let harness = AgentHarness::new(runtime, &sessions_dir).unwrap();
            let id = harness.session().meta.id.clone();
            let outcome = run_once(harness, "你好", true, CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(outcome.answer, "答案");
            assert_eq!(outcome.exit_code(), 0);
            id
        };

        // 会话恰好一个（headless 列表/恢复不产生多余会话文件）
        let metas = baiji_harness::JsonlStore::new(&sessions_dir).list().unwrap();
        assert_eq!(metas.len(), 1);

        // 可恢复（--session 路径），历史完整
        let runtime =
            Arc::new(AgentRuntime::new(Arc::new(EchoProvider)).with_tools(ToolRegistry::new()));
        let reloaded = AgentHarness::load(runtime, &sessions_dir, &session_id).unwrap();
        assert_eq!(reloaded.session().messages.len(), 2);
    }

    #[test]
    fn test_exit_codes() {
        let ok = Outcome::default();
        assert_eq!(ok.exit_code(), 0);
        let denied = Outcome { denied_tools: 1, ..Default::default() };
        assert_eq!(denied.exit_code(), 2);
        // 中断优先
        let cancelled = Outcome { cancelled: true, denied_tools: 1, ..Default::default() };
        assert_eq!(cancelled.exit_code(), 130);
    }
}
