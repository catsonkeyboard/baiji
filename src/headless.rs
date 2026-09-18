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
    /// 本次 run 的 LLM 轮次数（自动接力的预算计量）
    #[cfg_attr(not(test), allow(dead_code))]
    pub turns: u32,
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
    harness: &mut AgentHarness,
    input: &str,
    quiet: bool,
    cancel: CancellationToken,
) -> Result<Outcome> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

    let printer = tokio::spawn(async move {
        use std::io::Write as _;
        let mut denied = 0usize;
        let mut turns = 0u32;
        while let Some(event) = rx.recv().await {
            if let AgentEvent::ToolFinished { output, .. } = &event
                && output.starts_with("[Denied")
            {
                denied += 1;
            }
            if matches!(event, AgentEvent::TurnStarted { .. }) {
                turns += 1;
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
        (denied, turns)
    });

    let result = harness
        .run(input, &tx, &cancel, &SteeringQueue::new())
        .await;
    drop(tx);
    let (denied_tools, turns) = printer.await.unwrap_or((0, 0));

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
        turns,
    })
}

/// 自动接力执行（`--continue-until-done`）：run 结束后若 todo 仍有未完成项
/// 且轮次未超上限，以固定输入继续，直到完成 / 触上限 / 被取消。
/// Ctrl-C 一旦触发即停止接力（优雅收尾当前 run）。
pub async fn run_auto(
    harness: &mut AgentHarness,
    input: &str,
    quiet: bool,
    cancel: CancellationToken,
    max_turns: u32,
) -> Result<Outcome> {
    let mut turns_total = 0u32;
    let mut current = input.to_string();
    let mut last;
    loop {
        last = run_once(harness, &current, quiet, cancel.clone()).await?;
        turns_total += last.turns;
        if !baiji_harness::should_auto_continue(
            true,
            max_turns,
            harness.has_open_todos(),
            turns_total,
            !last.cancelled,
        ) {
            break;
        }
        if !quiet {
            eprintln!("[auto-continue {turns_total}/{max_turns} turns] todo 仍有未完成项，继续");
        }
        current = baiji_harness::AUTO_CONTINUE_PROMPT.to_string();
    }
    last.turns = turns_total;
    Ok(last)
}

/// 打印会话列表（--sessions）。直接读存储，不创建新会话。
/// 按项目分组：当前目录所属项目在最前，其余按最近活动排序，旧会话最后。
pub fn print_sessions(sessions_dir: &std::path::Path) -> Result<()> {
    let sessions = baiji_harness::JsonlStore::new(sessions_dir).list()?;
    if sessions.is_empty() {
        println!("（暂无会话，位于 ~/.baiji/sessions/）");
        return Ok(());
    }
    let current_key = std::env::current_dir()
        .ok()
        .map(|dir| baiji_harness::project_key(&dir));
    print!(
        "{}",
        format_session_groups(sessions, current_key.as_deref())
    );
    Ok(())
}

/// 分组渲染（纯函数，测试用）：每组一段 = 头部 + 树形表
pub fn format_session_groups(
    sessions: Vec<baiji_harness::SessionMeta>,
    current_project: Option<&str>,
) -> String {
    let mut groups = baiji_harness::group_by_project(sessions);
    // 当前项目组提到最前（若存在）
    if let Some(current) = current_project {
        groups.sort_by_key(|(key, _)| *key != Some(current.to_string()));
    }

    let mut out = String::new();
    for (key, metas) in &groups {
        let header = match key {
            Some(key) if Some(key.as_str()) == current_project => format!("⌂ {key}（当前项目）"),
            Some(key) => format!("⌂ {key}"),
            None => "⌂ （未记录项目——旧版会话）".to_string(),
        };
        out.push_str(&format!("{header}\n"));
        let tree = baiji_harness::SessionTree::from_metas(metas.clone());
        for (depth, meta) in tree.flattened() {
            let branch = if depth > 0 {
                format!("{}└ ", "  ".repeat(depth - 1))
            } else {
                String::new()
            };
            out.push_str(&format!(
                "  {:<28} {:<20} {}{}\n",
                meta.id,
                &meta.created_at[..meta.created_at.len().min(19)],
                branch,
                meta.title.as_deref().unwrap_or("(无标题)"),
            ));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use baiji_agent::{AgentRuntime, ToolRegistry};
    use baiji_ai::{ChatRequest, ChatResponse, Protocol, Provider, StreamChunk};
    use futures::StreamExt as _;
    use futures::stream::BoxStream;
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
            let mut harness = AgentHarness::new(runtime, &sessions_dir).unwrap();
            let id = harness.session().meta.id.clone();
            let outcome = run_once(&mut harness, "你好", true, CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(outcome.answer, "答案");
            assert_eq!(outcome.exit_code(), 0);
            id
        };

        // 会话恰好一个（headless 列表/恢复不产生多余会话文件）
        let metas = baiji_harness::JsonlStore::new(&sessions_dir)
            .list()
            .unwrap();
        assert_eq!(metas.len(), 1);

        // 可恢复（--session 路径），历史完整
        let runtime =
            Arc::new(AgentRuntime::new(Arc::new(EchoProvider)).with_tools(ToolRegistry::new()));
        let reloaded = AgentHarness::load(runtime, &sessions_dir, &session_id).unwrap();
        assert_eq!(reloaded.session().messages.len(), 2);
    }

    /// 脚本化 todo Provider：奇数次调用发工具调用（按轮次 add/update），
    /// 偶数次调用直接回答——驱动自动接力循环走完
    struct TodoScriptProvider {
        /// 永远只加不完成（上限测试用）
        never_done: bool,
        calls: std::sync::atomic::AtomicU32,
    }

    impl TodoScriptProvider {
        fn tool_calls(&self, n: u32) -> Vec<StreamChunk> {
            let (id, args) = if self.never_done || n > 1 {
                let i = (n + 1) / 2;
                (
                    format!("t{n}"),
                    format!("{{\"action\":\"add\",\"content\":\"task {i}\"}}"),
                )
            } else {
                (
                    "t1".to_string(),
                    "{\"action\":\"add\",\"content\":\"task 1\"}{\"action\":\"add\",\"content\":\"task 2\"}{\"action\":\"add\",\"content\":\"task 3\"}"
                        .to_string(),
                )
            };
            // 首个 add 之后跟两个 add（一次三个工具调用的简化：逐个发出）
            if n == 1 {
                vec![
                    StreamChunk::ToolCallStart {
                        id: "a1".into(),
                        name: "todo".into(),
                    },
                    StreamChunk::ToolCallArguments {
                        id: "a1".into(),
                        arguments: "{\"action\":\"add\",\"content\":\"task 1\"}".into(),
                    },
                    StreamChunk::ToolCallStart {
                        id: "a2".into(),
                        name: "todo".into(),
                    },
                    StreamChunk::ToolCallArguments {
                        id: "a2".into(),
                        arguments: "{\"action\":\"add\",\"content\":\"task 2\"}".into(),
                    },
                    StreamChunk::ToolCallStart {
                        id: "a3".into(),
                        name: "todo".into(),
                    },
                    StreamChunk::ToolCallArguments {
                        id: "a3".into(),
                        arguments: "{\"action\":\"add\",\"content\":\"task 3\"}".into(),
                    },
                    StreamChunk::Done,
                ]
            } else if self.never_done {
                vec![
                    StreamChunk::ToolCallStart {
                        id: id.clone(),
                        name: "todo".into(),
                    },
                    StreamChunk::ToolCallArguments {
                        id,
                        arguments: args,
                    },
                    StreamChunk::Done,
                ]
            } else {
                // 接力轮：完成一个任务（id = 轮次序号）
                let done_id = (n - 1) / 2;
                vec![
                    StreamChunk::ToolCallStart {
                        id: id.clone(),
                        name: "todo".into(),
                    },
                    StreamChunk::ToolCallArguments {
                        id,
                        arguments: format!(
                            "{{\"action\":\"update\",\"id\":{done_id},\"status\":\"done\"}}"
                        ),
                    },
                    StreamChunk::Done,
                ]
            }
        }
    }

    #[async_trait]
    impl Provider for TodoScriptProvider {
        async fn chat(&self, _: ChatRequest) -> anyhow::Result<ChatResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> anyhow::Result<BoxStream<'static, anyhow::Result<StreamChunk>>> {
            use std::sync::atomic::Ordering;
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let chunks: Vec<anyhow::Result<StreamChunk>> = if n % 2 == 1 {
                self.tool_calls(n).into_iter().map(Ok).collect()
            } else {
                vec![
                    Ok(StreamChunk::Content("step done".into())),
                    Ok(StreamChunk::Done),
                ]
            };
            Ok(futures::stream::iter(chunks).boxed())
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

    fn todo_harness(
        provider: Arc<TodoScriptProvider>,
        sessions_dir: &std::path::Path,
    ) -> (AgentHarness, std::sync::Arc<baiji_harness::TodoStore>) {
        let todos = std::sync::Arc::new(baiji_harness::TodoStore::new());
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(baiji_harness::TodoTool::new(
            todos.clone(),
        )));
        let runtime = Arc::new(AgentRuntime::new(provider).with_tools(registry));
        let mut harness = AgentHarness::new(runtime, sessions_dir).unwrap();
        harness.set_todos(todos.clone());
        (harness, todos)
    }

    #[tokio::test]
    async fn test_run_auto_completes_all_todos() {
        let dir = tempfile::tempdir().unwrap();
        let (mut harness, todos) = todo_harness(
            std::sync::Arc::new(TodoScriptProvider {
                never_done: false,
                calls: std::sync::atomic::AtomicU32::new(0),
            }),
            dir.path(),
        );

        let outcome = run_auto(
            &mut harness,
            "做个三步任务",
            true,
            CancellationToken::new(),
            20,
        )
        .await
        .unwrap();

        // 4 次 run(初始 + 3 次接力)× 每次 2 轮 = 8 轮
        assert_eq!(outcome.turns, 8, "expected 4 runs x 2 turns");
        assert!(!outcome.cancelled);
        assert_eq!(outcome.exit_code(), 0);
        // 全部完成
        assert!(!todos.has_open());
        let items = todos.items();
        assert_eq!(items.len(), 3);
        assert!(
            items
                .iter()
                .all(|t| t.status == baiji_harness::TodoStatus::Done)
        );
        // 接力输入进入历史（3 次）
        let relay_count = harness
            .session()
            .messages
            .iter()
            .filter(|m| m.content == baiji_harness::AUTO_CONTINUE_PROMPT)
            .count();
        assert_eq!(relay_count, 3);
    }

    #[tokio::test]
    async fn test_run_auto_stops_at_turn_cap() {
        let dir = tempfile::tempdir().unwrap();
        let (mut harness, todos) = todo_harness(
            std::sync::Arc::new(TodoScriptProvider {
                never_done: true,
                calls: std::sync::atomic::AtomicU32::new(0),
            }),
            dir.path(),
        );

        let outcome = run_auto(&mut harness, "无限任务", true, CancellationToken::new(), 4)
            .await
            .unwrap();

        // 2 次 run × 2 轮 = 4 轮触顶停止；todo 仍有未完成项
        assert_eq!(outcome.turns, 4);
        assert!(todos.has_open());
        let relay_count = harness
            .session()
            .messages
            .iter()
            .filter(|m| m.content == baiji_harness::AUTO_CONTINUE_PROMPT)
            .count();
        assert_eq!(relay_count, 1, "only one relay before the cap");
    }

    #[test]
    fn test_format_session_groups() {
        let mk = |id: &str, project: Option<&str>, created: &str| baiji_harness::SessionMeta {
            id: id.into(),
            parent_id: None,
            created_at: created.into(),
            title: Some(format!("t-{id}")),
            project: project.map(str::to_string),
        };
        let sessions = vec![
            mk("old", None, "2026-01-01T10:00:00+00:00"),
            mk("b1", Some("beta-2222"), "2026-04-01T10:00:00+00:00"),
            mk("a1", Some("alpha-1111"), "2026-02-01T10:00:00+00:00"),
        ];
        let out = format_session_groups(sessions, Some("alpha-1111"));
        // 当前项目组在最前，附标注；旧会话组殿后
        let alpha_pos = out.find("alpha-1111（当前项目）").unwrap();
        let beta_pos = out.find("beta-2222").unwrap();
        let legacy_pos = out.find("未记录项目").unwrap();
        assert!(alpha_pos < beta_pos && beta_pos < legacy_pos, "{out}");
        assert!(out.contains("a1"), "{out}");
        assert!(out.contains("t-old"), "{out}");
    }

    #[test]
    fn test_exit_codes() {
        let ok = Outcome::default();
        assert_eq!(ok.exit_code(), 0);
        let denied = Outcome {
            denied_tools: 1,
            ..Default::default()
        };
        assert_eq!(denied.exit_code(), 2);
        // 中断优先
        let cancelled = Outcome {
            cancelled: true,
            denied_tools: 1,
            ..Default::default()
        };
        assert_eq!(cancelled.exit_code(), 130);
    }
}
