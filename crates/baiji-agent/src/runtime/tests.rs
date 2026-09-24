use super::*;
use crate::runtime::helpers::{
    ELIDED_NOTE, STEER_SUFFIX, elide_old_tool_results, retry_delay, steer_last_user,
};
use crate::tool::{AgentTool, ToolOutput};
use async_trait::async_trait;
use baiji_ai::{ChatResponse, Protocol, StreamChunk};
use futures::StreamExt;

// ===== verbosity steer =====

/// 记录每次请求消息与工具定义的捕获型 Provider
struct CapturingProvider {
    requests: std::sync::Mutex<Vec<Vec<Message>>>,
    tool_names: std::sync::Mutex<Vec<Vec<String>>>,
    /// true = 每轮都发工具调用（跑满预算），用于收尾提醒类测试
    always_tool: bool,
}

#[async_trait]
impl Provider for CapturingProvider {
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("runtime uses chat_stream")
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamChunk>>> {
        self.requests.lock().unwrap().push(request.messages.clone());
        self.tool_names.lock().unwrap().push(
            request
                .tools
                .iter()
                .flatten()
                .map(|t| t.name.clone())
                .collect(),
        );
        let chunks: Vec<Result<StreamChunk>> = if self.always_tool {
            vec![
                Ok(StreamChunk::ToolCallStart {
                    id: "t1".into(),
                    name: "unknown".into(),
                }),
                Ok(StreamChunk::ToolCallArguments {
                    id: "t1".into(),
                    arguments: "{}".into(),
                }),
                Ok(StreamChunk::Done),
            ]
        } else {
            vec![Ok(StreamChunk::Content("ok".into())), Ok(StreamChunk::Done)]
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

#[test]
fn test_retry_delay_exponential_with_cap() {
    let base = Duration::from_millis(500);
    let cap = Duration::from_millis(30_000);
    // 指数退避：500ms → 1s → 2s → 4s
    assert_eq!(retry_delay(base, 0, cap, None), Duration::from_millis(500));
    assert_eq!(
        retry_delay(base, 1, cap, None),
        Duration::from_millis(1_000)
    );
    assert_eq!(
        retry_delay(base, 2, cap, None),
        Duration::from_millis(2_000)
    );
    assert_eq!(
        retry_delay(base, 3, cap, None),
        Duration::from_millis(4_000)
    );
    // 封顶：2^6×500 = 32s > 30s cap
    assert_eq!(retry_delay(base, 6, cap, None), cap);
    // 服务端 Retry-After 优先，但同样受上限约束
    assert_eq!(
        retry_delay(base, 0, cap, Some(Duration::from_secs(2))),
        Duration::from_secs(2)
    );
    assert_eq!(
        retry_delay(base, 0, cap, Some(Duration::from_secs(120))),
        cap
    );
}

#[test]
fn test_steer_last_user_targets_last_user_only() {
    let mut msgs = vec![
        Message::user("q1"),
        Message::assistant("a1"),
        Message::user("q2"),
        Message::assistant("a2"),
    ];
    steer_last_user(&mut msgs);
    assert_eq!(msgs[0].content, "q1");
    assert_eq!(msgs[2].content.strip_suffix(STEER_SUFFIX), Some("q2"));

    // 无 user 消息：不注入
    let mut none = vec![Message::system("s"), Message::assistant("a")];
    steer_last_user(&mut none);
    assert_eq!(none[0].content, "s");
}

#[tokio::test]
async fn test_verbosity_steer_injects_into_request_copy_only() {
    let provider = Arc::new(CapturingProvider {
        requests: std::sync::Mutex::new(Vec::new()),
        tool_names: std::sync::Mutex::new(Vec::new()),
        always_tool: false,
    });
    let runtime = AgentRuntime::new(provider.clone()).with_verbosity_steer(true);
    let mut history = vec![Message::user("原始问题")];
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

    runtime
        .run(
            "sys",
            &mut history,
            &tx,
            &CancellationToken::new(),
            &SteeringQueue::new(),
        )
        .await
        .unwrap();

    // 请求副本：最后一条 user 消息带恒定后缀
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let last_user = requests[0]
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .unwrap();
        assert_eq!(
            last_user.content.strip_suffix(STEER_SUFFIX),
            Some("原始问题")
        );
    }
    // 会话历史未被污染（注入不持久化、不进上下文）
    assert_eq!(history[0].content, "原始问题");

    // 第二轮：后缀落在新的最后一条 user 消息上，旧 user 消息保持干净
    history.push(Message::user("第二个问题"));
    runtime
        .run(
            "sys",
            &mut history,
            &tx,
            &CancellationToken::new(),
            &SteeringQueue::new(),
        )
        .await
        .unwrap();
    {
        let requests = provider.requests.lock().unwrap();
        let users: Vec<&Message> = requests[1]
            .iter()
            .filter(|m| m.role == Role::User)
            .collect();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].content, "原始问题");
        assert_eq!(
            users[1].content.strip_suffix(STEER_SUFFIX),
            Some("第二个问题")
        );
    }
    assert_eq!(history[0].content, "原始问题");
    assert_eq!(history[2].content, "第二个问题");
}

#[tokio::test]
async fn test_verbosity_steer_disabled_by_default() {
    let provider = Arc::new(CapturingProvider {
        requests: std::sync::Mutex::new(Vec::new()),
        tool_names: std::sync::Mutex::new(Vec::new()),
        always_tool: false,
    });
    let runtime = AgentRuntime::new(provider.clone());
    let mut history = vec![Message::user("q")];
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    runtime
        .run(
            "sys",
            &mut history,
            &tx,
            &CancellationToken::new(),
            &SteeringQueue::new(),
        )
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    assert!(!requests[0].iter().any(|m| m.content.contains(STEER_SUFFIX)));
}
use baiji_telemetry::RecordingTelemetry;
use std::sync::atomic::{AtomicUsize, Ordering};

// ===== Mock Provider：按预设脚本吐流式块 =====

#[derive(Clone)]
enum Script {
    ToolThenAnswer {
        tool_id: &'static str,
        tool_name: &'static str,
        args: &'static str,
        answer: &'static str,
    },
    AnswerOnly(&'static str),
    /// 第一轮一次返回三个工具调用，之后直接回答
    ThreeToolsThenAnswer,
    /// 每一轮都返回工具调用（用于触发 max_turns）
    AlwaysTool,
    /// 第一轮：工具参数在 max_tokens 处被截断；之后直接回答
    TruncatedToolThenAnswer,
    /// 最终答案在 max_tokens 处被截断（无工具调用）
    TruncatedAnswer(&'static str),
}

fn tool_call_chunks(id: &str) -> Vec<Result<StreamChunk>> {
    vec![
        Ok(StreamChunk::ToolCallStart {
            id: id.to_string(),
            name: "append".to_string(),
        }),
        Ok(StreamChunk::ToolCallArguments {
            id: id.to_string(),
            arguments: r#"{"text":"x"}"#.to_string(),
        }),
    ]
}

struct MockProvider {
    script: Script,
    calls: AtomicUsize,
}

impl MockProvider {
    fn new(script: Script) -> Self {
        Self {
            script,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        unreachable!("runtime uses chat_stream")
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let n = self.calls.load(Ordering::SeqCst);
        let chunks: Vec<Result<StreamChunk>> = match &self.script {
            Script::ToolThenAnswer {
                tool_id,
                tool_name,
                args,
                answer,
            } => {
                if n == 1 {
                    vec![
                        Ok(StreamChunk::Content("Let me check.".into())),
                        Ok(StreamChunk::ToolCallStart {
                            id: tool_id.to_string(),
                            name: tool_name.to_string(),
                        }),
                        Ok(StreamChunk::ToolCallArguments {
                            id: tool_id.to_string(),
                            arguments: args.to_string(),
                        }),
                        Ok(StreamChunk::Done),
                    ]
                } else {
                    vec![
                        Ok(StreamChunk::Content(answer.to_string())),
                        Ok(StreamChunk::Done),
                    ]
                }
            }
            Script::AnswerOnly(text) => vec![
                Ok(StreamChunk::Content(text.to_string())),
                Ok(StreamChunk::Done),
            ],
            Script::TruncatedAnswer(text) => vec![
                Ok(StreamChunk::Content(text.to_string())),
                Ok(StreamChunk::Stop(StopReason::MaxTokens)),
                Ok(StreamChunk::Done),
            ],
            Script::ThreeToolsThenAnswer => {
                if n == 1 {
                    let mut chunks = Vec::new();
                    for id in ["a", "b", "c"] {
                        chunks.extend(tool_call_chunks(id));
                    }
                    chunks.push(Ok(StreamChunk::Done));
                    chunks
                } else {
                    vec![
                        Ok(StreamChunk::Content("done".to_string())),
                        Ok(StreamChunk::Done),
                    ]
                }
            }
            Script::TruncatedToolThenAnswer => {
                if n == 1 {
                    vec![
                        Ok(StreamChunk::ToolCallStart {
                            id: "t1".to_string(),
                            name: "append".to_string(),
                        }),
                        Ok(StreamChunk::ToolCallArguments {
                            id: "t1".to_string(),
                            arguments: r#"{"text":"cut of"#.to_string(),
                        }),
                        Ok(StreamChunk::Stop(StopReason::MaxTokens)),
                        Ok(StreamChunk::Done),
                    ]
                } else {
                    vec![
                        Ok(StreamChunk::Content("done".to_string())),
                        Ok(StreamChunk::Done),
                    ]
                }
            }
            Script::AlwaysTool => {
                let mut chunks = tool_call_chunks(&format!("t{n}"));
                chunks.push(Ok(StreamChunk::Done));
                chunks
            }
        };
        Ok(futures::stream::iter(chunks).boxed())
    }

    fn protocol(&self) -> Protocol {
        Protocol::Anthropic
    }
    fn model(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

// ===== 测试工具 =====

struct AppendTool;

#[async_trait]
impl AgentTool for AppendTool {
    fn name(&self) -> &str {
        "append"
    }
    fn description(&self) -> &str {
        "append text"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        Ok(ToolOutput::ok(format!(
            "appended:{}",
            args["text"].as_str().unwrap_or("")
        )))
    }
}

fn runtime_with(provider: MockProvider) -> AgentRuntime {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(AppendTool));
    AgentRuntime::new(Arc::new(provider)).with_tools(tools)
}

#[test]
fn test_thinking_level_builder_and_hot_swap() {
    use baiji_ai::ThinkingLevel;
    let runtime = runtime_with(MockProvider::new(Script::AnswerOnly("ok")));
    assert_eq!(runtime.thinking(), None, "default off");
    let runtime = runtime.with_thinking(Some(ThinkingLevel::High));
    assert_eq!(runtime.thinking(), Some(ThinkingLevel::High));
    // 热切换（TUI /thinking 路径）：&self 即可修改，下一次请求生效
    runtime.set_thinking(Some(ThinkingLevel::Minimal));
    assert_eq!(runtime.thinking(), Some(ThinkingLevel::Minimal));
    runtime.set_thinking(None);
    assert_eq!(runtime.thinking(), None);
}

#[tokio::test]
async fn test_wrapup_notice_on_last_two_turns_request_copy_only() {
    // 每轮都调工具 → 跑满预算；捕获每次请求断言倒计时提醒
    let provider = Arc::new(CapturingProvider {
        requests: std::sync::Mutex::new(Vec::new()),
        tool_names: std::sync::Mutex::new(Vec::new()),
        always_tool: true,
    });
    let runtime = AgentRuntime::new(provider.clone()).with_limits(3, 8192);

    let mut messages = vec![Message::user("hi")];
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = runtime
        .run(
            "sys",
            &mut messages,
            &tx,
            &CancellationToken::new(),
            &SteeringQueue::new(),
        )
        .await;
    assert!(
        result.is_err(),
        "always-tool script must exhaust the budget"
    );

    let requests = provider.requests.lock().unwrap();
    assert!(requests.len() >= 3, "{}", requests.len());
    // 倒计时 1 轮：提醒收尾
    assert!(
        requests[requests.len() - 2]
            .iter()
            .any(|m| m.content.contains("ONE turn left")),
        "penultimate turn carries the wrap-up warning"
    );
    // 最后一轮：只作答不调工具
    assert!(
        requests
            .last()
            .unwrap()
            .iter()
            .any(|m| m.content.contains("FINAL TURN")),
        "final turn carries the answer-now notice"
    );
    // 非告急轮不注入
    assert!(
        !requests[0]
            .iter()
            .any(|m| m.content.contains("system notice"))
    );
    // 仅请求副本：会话历史不含提醒（也不落盘）
    assert!(!messages.iter().any(|m| m.content.contains("system notice")));
}

// ===== 计划模式 =====

#[tokio::test]
async fn test_plan_mode_denies_non_readonly_tool() {
    let provider = MockProvider::new(Script::ToolThenAnswer {
        tool_id: "t1",
        tool_name: "append",
        args: r#"{"text":"x"}"#,
        answer: "planned",
    });
    let runtime = runtime_with(provider).with_plan_mode(true);

    let mut messages = vec![Message::user("hi")];
    let events = drive(&runtime, &mut messages).await;

    // 工具被计划模式门控拒绝（is_error 结果回传，循环继续到最终答案）
    let result = &messages[2].tool_results.as_ref().unwrap()[0];
    assert!(result.content.contains("[Plan mode]"), "{}", result.content);
    assert_eq!(messages.last().unwrap().content, "planned");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolFinished { is_error: true, .. }))
    );
}

#[tokio::test]
async fn test_plan_mode_filters_tool_definitions_and_hot_toggles() {
    struct NamedTool(&'static str);
    #[async_trait]
    impl AgentTool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
            Ok(ToolOutput::ok("ran"))
        }
    }

    let provider = Arc::new(CapturingProvider {
        requests: std::sync::Mutex::new(Vec::new()),
        tool_names: std::sync::Mutex::new(Vec::new()),
        always_tool: false,
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(NamedTool("append"))); // 白名单外
    tools.register(Arc::new(NamedTool("read"))); // 白名单内
    let runtime = AgentRuntime::new(provider.clone())
        .with_tools(tools)
        .with_plan_mode(true);

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let steering = SteeringQueue::new();
    let mut messages = vec![Message::user("plan this")];
    runtime
        .run(
            "sys",
            &mut messages,
            &tx,
            &CancellationToken::new(),
            &steering,
        )
        .await
        .unwrap();
    {
        let names = provider.tool_names.lock().unwrap();
        assert_eq!(names[0], vec!["read"], "plan mode hides non-readonly tools");
    }

    // 热切换关闭（TUI Enter 执行计划路径）：下一次请求恢复全量定义
    runtime.set_plan_mode(false);
    messages.push(Message::user("execute"));
    runtime
        .run(
            "sys",
            &mut messages,
            &tx,
            &CancellationToken::new(),
            &steering,
        )
        .await
        .unwrap();
    let names = provider.tool_names.lock().unwrap();
    assert_eq!(
        names[1],
        vec!["append", "read"],
        "registration order restored"
    );
}

#[tokio::test]
async fn test_plan_mode_denies_before_hook_gate() {
    // 门控顺序回归：计划模式拒绝必须发生在 hook 门控之前
    //（不该为注定被拒的调用咨询 hook / 弹 HITL 确认）
    struct RecordingHook(std::sync::Mutex<Vec<String>>);

    #[async_trait]
    impl crate::hooks::Hook for RecordingHook {
        fn name(&self) -> &str {
            "recording"
        }
        async fn on_tool_call(
            &self,
            name: &str,
            _args: &serde_json::Value,
        ) -> anyhow::Result<crate::HookDecision> {
            self.0.lock().unwrap().push(name.to_string());
            Ok(crate::HookDecision::Proceed)
        }
    }

    let seen = Arc::new(RecordingHook(std::sync::Mutex::new(Vec::new())));
    let provider = MockProvider::new(Script::ToolThenAnswer {
        tool_id: "t1",
        tool_name: "append",
        args: r#"{"text":"x"}"#,
        answer: "planned",
    });
    let mut hooks = HookRegistry::new();
    hooks.register(seen.clone());
    let runtime = runtime_with(provider)
        .with_hooks(hooks)
        .with_plan_mode(true);

    let mut messages = vec![Message::user("hi")];
    drive(&runtime, &mut messages).await;

    assert!(
        seen.0.lock().unwrap().is_empty(),
        "plan-mode denial must short-circuit before the hook gate"
    );
    let result = &messages[2].tool_results.as_ref().unwrap()[0];
    assert!(result.content.contains("[Plan mode]"));
}

// ===== 并行工具编排 =====

/// 第一轮发两个指定名字的工具调用，第二轮给最终答案
struct TwoCallsProvider {
    names: [&'static str; 2],
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for TwoCallsProvider {
    async fn chat(&self, _: ChatRequest) -> anyhow::Result<ChatResponse> {
        unreachable!("runtime uses chat_stream")
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<StreamChunk>>>
    {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let chunks: Vec<anyhow::Result<StreamChunk>> = if n == 1 {
            let mut v = Vec::new();
            for (i, name) in self.names.iter().enumerate() {
                v.push(Ok(StreamChunk::ToolCallStart {
                    id: format!("t{i}"),
                    name: name.to_string(),
                }));
                v.push(Ok(StreamChunk::ToolCallArguments {
                    id: format!("t{i}"),
                    arguments: "{}".to_string(),
                }));
            }
            v.push(Ok(StreamChunk::Done));
            v
        } else {
            vec![
                Ok(StreamChunk::Content("done".into())),
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

/// 探针工具：并发时两个探针在 barrier 相遇；串行时各自超时（overlap 恒 false）
struct BarrierProbe {
    name: &'static str,
    barrier: Arc<tokio::sync::Barrier>,
    overlap: Arc<std::sync::atomic::AtomicBool>,
    parallel: bool,
    /// 单独等待 barrier 的超时（并行相遇在毫秒级；串行路径靠超时放行）
    wait_ms: u64,
}

#[async_trait]
impl AgentTool for BarrierProbe {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "probe"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn parallel(&self) -> bool {
        self.parallel
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let met = tokio::time::timeout(
            std::time::Duration::from_millis(self.wait_ms),
            self.barrier.wait(),
        )
        .await
        .is_ok();
        if met {
            self.overlap.store(true, Ordering::Relaxed);
        }
        Ok(ToolOutput::ok(format!("{} met={met}", self.name)))
    }
}

#[tokio::test]
async fn test_parallel_tools_run_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let overlap = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BarrierProbe {
        name: "probe_a",
        barrier: barrier.clone(),
        overlap: overlap.clone(),
        parallel: true,
        wait_ms: 2000,
    }));
    tools.register(Arc::new(BarrierProbe {
        name: "probe_b",
        barrier,
        overlap: overlap.clone(),
        parallel: true,
        wait_ms: 2000,
    }));
    let runtime = AgentRuntime::new(Arc::new(TwoCallsProvider {
        names: ["probe_a", "probe_b"],
        calls: AtomicUsize::new(0),
    }))
    .with_tools(tools);

    let mut history = vec![Message::user("go")];
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let answer = runtime
        .run(
            "sys",
            &mut history,
            &tx,
            &CancellationToken::new(),
            &SteeringQueue::new(),
        )
        .await
        .unwrap();
    assert_eq!(answer, "done");

    assert!(
        overlap.load(Ordering::Relaxed),
        "two parallel-capable tools in one turn must run concurrently"
    );
    // 结果按原 id 顺序配对（tool_use/tool_result 稳定）
    let results_msg = history.iter().find(|m| m.tool_results.is_some()).unwrap();
    let results = results_msg.tool_results.as_ref().unwrap();
    assert_eq!(results[0].tool_call_id, "t0");
    assert_eq!(results[1].tool_call_id, "t1");
    assert!(results[0].content.contains("probe_a"));
    assert!(results[1].content.contains("probe_b"));
}

#[tokio::test]
async fn test_mixed_tools_fall_back_to_sequential() {
    // 一轮里有非 parallel 工具 → 整轮回退串行（in-flight 计数恒 ≤ 1；结果仍配对）
    use std::sync::atomic::{AtomicUsize as AU, Ordering as O};

    struct Tracker {
        name: &'static str,
        parallel: bool,
        active: Arc<AU>,
        max_active: Arc<AU>,
    }

    #[async_trait]
    impl AgentTool for Tracker {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "tracker"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn parallel(&self) -> bool {
            self.parallel
        }
        async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
            let n = self.active.fetch_add(1, O::SeqCst) + 1;
            self.max_active.fetch_max(n, O::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.active.fetch_sub(1, O::SeqCst);
            Ok(ToolOutput::ok(self.name))
        }
    }

    let active = Arc::new(AU::new(0));
    let max_active = Arc::new(AU::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Tracker {
        name: "para",
        parallel: true,
        active: active.clone(),
        max_active: max_active.clone(),
    }));
    tools.register(Arc::new(Tracker {
        name: "seq",
        parallel: false,
        active,
        max_active: max_active.clone(),
    }));
    let runtime = AgentRuntime::new(Arc::new(TwoCallsProvider {
        names: ["para", "seq"],
        calls: AtomicUsize::new(0),
    }))
    .with_tools(tools);

    let mut history = vec![Message::user("go")];
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    runtime
        .run(
            "sys",
            &mut history,
            &tx,
            &CancellationToken::new(),
            &SteeringQueue::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        max_active.load(O::SeqCst),
        1,
        "mixed turn must fall back to sequential execution"
    );
    let results_msg = history.iter().find(|m| m.tool_results.is_some()).unwrap();
    assert_eq!(results_msg.tool_results.as_ref().unwrap().len(), 2);
}

async fn drive(runtime: &AgentRuntime, messages: &mut Vec<Message>) -> Vec<AgentEvent> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let steering = SteeringQueue::new();
    let handle = tokio::spawn(async move {
        let mut collected = Vec::new();
        while let Some(event) = rx.recv().await {
            collected.push(event);
        }
        collected
    });
    runtime
        .run(
            "You are a test agent.",
            messages,
            &tx,
            &CancellationToken::new(),
            &steering,
        )
        .await
        .unwrap();
    drop(tx);
    handle.await.unwrap()
}

#[tokio::test]
async fn test_tool_loop_and_message_history() {
    let provider = MockProvider::new(Script::ToolThenAnswer {
        tool_id: "t1",
        tool_name: "append",
        args: r#"{"text":"hello"}"#,
        answer: "All done.",
    });
    let runtime = runtime_with(provider);

    let mut messages = vec![Message::user("run the tool")];
    let events = drive(&runtime, &mut messages).await;

    // 消息历史：user + assistant(tool_calls) + tool + assistant(final)
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1].role, Role::Assistant);
    assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].name, "append");
    assert_eq!(messages[2].role, Role::Tool);
    assert_eq!(
        messages[2].tool_results.as_ref().unwrap()[0].content,
        "appended:hello"
    );
    assert_eq!(messages[3].content, "All done.");

    // 事件序列
    let kinds: Vec<_> = events.iter().map(|e| e.kind()).collect();
    assert!(kinds.contains(&"text_delta"));
    assert!(kinds.contains(&"tool_started"));
    assert!(kinds.contains(&"tool_finished"));
    assert!(kinds.contains(&"run_completed"));
}

#[tokio::test]
async fn test_answer_only_single_turn() {
    let provider = MockProvider::new(Script::AnswerOnly("直接回答"));
    let runtime = runtime_with(provider);

    let mut messages = vec![Message::user("hi")];
    let events = drive(&runtime, &mut messages).await;

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].content, "直接回答");
    assert_eq!(events.last().unwrap().kind(), "run_completed");
}

#[tokio::test]
async fn test_hook_denies_tool() {
    struct DenyHook;

    #[async_trait]
    impl crate::Hook for DenyHook {
        fn name(&self) -> &str {
            "deny-append"
        }
        async fn on_tool_call(
            &self,
            _name: &str,
            _args: &serde_json::Value,
        ) -> Result<crate::HookDecision> {
            Ok(crate::HookDecision::Deny("not allowed".to_string()))
        }
    }

    let provider = MockProvider::new(Script::ToolThenAnswer {
        tool_id: "t1",
        tool_name: "append",
        args: r#"{"text":"x"}"#,
        answer: "ok",
    });
    let mut hooks = HookRegistry::new();
    hooks.register(Arc::new(DenyHook));
    let runtime = runtime_with(provider).with_hooks(hooks);

    let mut messages = vec![Message::user("hi")];
    drive(&runtime, &mut messages).await;

    // 工具结果被替换为 Deny 文案，但循环继续并给出最终答案
    assert!(
        messages[2].tool_results.as_ref().unwrap()[0]
            .content
            .contains("[Denied by hook]")
    );
    assert_eq!(messages[3].content, "ok");
}

#[tokio::test]
async fn test_cancel_interrupts() {
    struct PendingProvider;

    #[async_trait]
    impl Provider for PendingProvider {
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<futures::stream::BoxStream<'static, Result<StreamChunk>>> {
            // 永不结束的流，等待被取消
            Ok(futures::stream::pending().boxed())
        }
        fn protocol(&self) -> Protocol {
            Protocol::Anthropic
        }
        fn model(&self) -> &str {
            "mock"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    let runtime = AgentRuntime::new(Arc::new(PendingProvider));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let steering = SteeringQueue::new();

    let mut messages = vec![Message::user("hi")];
    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        }
    });

    let answer = runtime
        .run("sys", &mut messages, &tx, &cancel, &steering)
        .await
        .unwrap();
    assert_eq!(answer, "");
}

#[tokio::test]
async fn test_steering_injected_between_turns() {
    // 第一轮带工具调用，第二轮直接回答
    let provider = MockProvider::new(Script::ToolThenAnswer {
        tool_id: "t1",
        tool_name: "append",
        args: r#"{"text":"x"}"#,
        answer: "steered answer",
    });
    let runtime = runtime_with(provider);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let steering = SteeringQueue::new();
    steering.push("stop and answer directly");

    let mut messages = vec![Message::user("hi")];
    runtime
        .run(
            "sys",
            &mut messages,
            &tx,
            &CancellationToken::new(),
            &steering,
        )
        .await
        .unwrap();
    drop(tx);

    // steering 消息被注入到持久化历史（user 消息）
    let steered = messages
        .iter()
        .any(|m| m.role == Role::User && m.content == "stop and answer directly");
    assert!(steered, "steering message should be persisted");

    // 事件流以 run_completed 收尾
    let mut kinds = Vec::new();
    while let Some(event) = rx.recv().await {
        kinds.push(event.kind());
    }
    assert_eq!(kinds.last(), Some(&"run_completed"));
}

#[tokio::test]
async fn test_steering_skipped_tools_still_get_results() {
    /// 执行时模拟用户插话：向 steering 队列推入一条消息
    struct SteeringTool(Arc<SteeringQueue>);

    #[async_trait]
    impl AgentTool for SteeringTool {
        fn name(&self) -> &str {
            "append"
        }
        fn description(&self) -> &str {
            "append text"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
            self.0.push("change of plan");
            Ok(ToolOutput::ok("appended:x"))
        }
    }

    let steering = Arc::new(SteeringQueue::new());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SteeringTool(steering.clone())));
    let runtime = AgentRuntime::new(Arc::new(MockProvider::new(Script::ThreeToolsThenAnswer)))
        .with_tools(tools);

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

    let mut messages = vec![Message::user("hi")];
    runtime
        .run(
            "sys",
            &mut messages,
            &tx,
            &CancellationToken::new(),
            &steering,
        )
        .await
        .unwrap();

    let call_ids: Vec<String> = messages
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|c| c.id.clone())
        .collect();
    let results: Vec<&ToolResult> = messages
        .iter()
        .filter_map(|m| m.tool_results.as_ref())
        .flatten()
        .collect();
    let result_ids: Vec<String> = results.iter().map(|r| r.tool_call_id.clone()).collect();

    assert_eq!(call_ids, vec!["a", "b", "c"]);
    assert_eq!(result_ids, call_ids, "every tool_call needs a tool_result");
    assert!(results[0].content.starts_with("appended:"));
    assert_eq!(results[1].content, SKIPPED_BY_STEERING);
    assert_eq!(results[2].content, SKIPPED_BY_STEERING);
}

#[tokio::test]
async fn test_truncated_tool_args_are_not_executed() {
    static EXECUTED: AtomicUsize = AtomicUsize::new(0);
    struct CountingTool;

    #[async_trait]
    impl AgentTool for CountingTool {
        fn name(&self) -> &str {
            "append"
        }
        fn description(&self) -> &str {
            "append text"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
            EXECUTED.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::ok("ran"))
        }
    }

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(CountingTool));
    let runtime =
        AgentRuntime::new(Arc::new(MockProvider::new(Script::TruncatedToolThenAnswer)))
            .with_tools(tools);

    let mut messages = vec![Message::user("hi")];
    let events = drive(&runtime, &mut messages).await;

    assert_eq!(
        EXECUTED.load(Ordering::SeqCst),
        0,
        "must not run with `{{}}`"
    );
    let result = &messages[2].tool_results.as_ref().unwrap()[0];
    assert_eq!(result.tool_call_id, "t1");
    assert!(result.content.contains("NOT executed"));
    assert!(result.content.contains("max_tokens"));
    // 历史中的参数仍是合法对象，UI 收到错误结果，运行继续到最终答案
    assert!(
        messages[1].tool_calls.as_ref().unwrap()[0]
            .arguments
            .is_object()
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolFinished { is_error: true, .. }))
    );
    assert_eq!(messages.last().unwrap().content, "done");
}

#[tokio::test]
async fn test_cancel_interrupts_running_tool() {
    struct SlowTool;

    #[async_trait]
    impl AgentTool for SlowTool {
        fn name(&self) -> &str {
            "append"
        }
        fn description(&self) -> &str {
            "slow"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(ToolOutput::ok("never"))
        }
    }

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SlowTool));
    let runtime = AgentRuntime::new(Arc::new(MockProvider::new(Script::ThreeToolsThenAnswer)))
        .with_tools(tools);

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });

    let mut messages = vec![Message::user("hi")];
    let started = std::time::Instant::now();
    let answer = runtime
        .run("sys", &mut messages, &tx, &cancel, &SteeringQueue::new())
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "tool was not interrupted"
    );
    assert_eq!(answer, "");

    // 三个调用都有结果：被中断的 + 两个未执行的
    let results = messages[2].tool_results.as_ref().unwrap();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r.content == CANCELLED_BY_USER));
}

#[tokio::test]
async fn test_max_turns_does_not_duplicate_messages() {
    let runtime = runtime_with(MockProvider::new(Script::AlwaysTool)).with_limits(2, 4096);

    let mut messages = vec![Message::user("hi")];
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = runtime
        .run(
            "sys",
            &mut messages,
            &tx,
            &CancellationToken::new(),
            &SteeringQueue::new(),
        )
        .await;
    assert!(result.is_err());

    // user + 2 × (assistant + tool)；部分进度保留且不重复
    assert_eq!(messages.len(), 5);
    let mut ids: Vec<String> = messages
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|c| c.id.clone())
        .collect();
    let total = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), total, "tool_call ids must be unique");
}

#[tokio::test]
async fn test_telemetry_spans_emitted() {
    let provider = MockProvider::new(Script::AnswerOnly("ok"));
    let telemetry = RecordingTelemetry::new();
    let runtime = runtime_with(provider).with_telemetry(telemetry.shared());

    let mut messages = vec![Message::user("hi")];
    drive(&runtime, &mut messages).await;

    let spans = telemetry.span_names();
    assert!(spans.contains(&"agent.run".to_string()));
    assert!(spans.contains(&"agent.turn".to_string()));
}

#[tokio::test]
async fn test_confirmation_gate_denies_tool() {
    struct StrictApprover;

    #[async_trait]
    impl crate::Approver for StrictApprover {
        async fn confirm(
            &self,
            _request: crate::ConfirmationRequest,
            _cancel: &CancellationToken,
        ) -> crate::ConfirmationDecision {
            crate::ConfirmationDecision::Deny("user said no".to_string())
        }
    }

    let provider = MockProvider::new(Script::ToolThenAnswer {
        tool_id: "t1",
        tool_name: "append",
        args: r#"{"text":"x"}"#,
        answer: "ok",
    });
    let gate =
        crate::ConfirmationGate::new(vec!["append".to_string()], Arc::new(StrictApprover));
    let runtime = runtime_with(provider).with_confirmation(gate);

    let mut messages = vec![Message::user("hi")];
    drive(&runtime, &mut messages).await;

    // 工具被用户拒绝，结果回传 Deny 文案，循环继续到最终答案
    assert!(
        messages[2].tool_results.as_ref().unwrap()[0]
            .content
            .contains("[Denied by user]")
    );
    assert_eq!(messages[3].content, "ok");
}

#[tokio::test]
async fn test_confirmation_allow_all_skips_reprompts() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct AllowAllApprover {
        prompts: AtomicUsize,
    }

    #[async_trait]
    impl crate::Approver for AllowAllApprover {
        async fn confirm(
            &self,
            _request: crate::ConfirmationRequest,
            _cancel: &CancellationToken,
        ) -> crate::ConfirmationDecision {
            self.prompts.fetch_add(1, Ordering::SeqCst);
            crate::ConfirmationDecision::AllowAll
        }
    }

    let provider = MockProvider::new(Script::ToolThenAnswer {
        tool_id: "t1",
        tool_name: "append",
        args: r#"{"text":"x"}"#,
        answer: "ok",
    });
    let approver = Arc::new(AllowAllApprover {
        prompts: AtomicUsize::new(0),
    });
    let gate = crate::ConfirmationGate::new(vec!["append".to_string()], approver.clone());
    let runtime = runtime_with(provider).with_confirmation(gate);

    let mut messages = vec![Message::user("hi")];
    drive(&runtime, &mut messages).await;

    // 工具正常执行（AllowAll 放行），且只询问一次
    assert_eq!(
        messages[2].tool_results.as_ref().unwrap()[0].content,
        "appended:x"
    );
    assert_eq!(approver.prompts.load(Ordering::SeqCst), 1);
}

/// 前 `failures` 次调用：先流出半截文本，再以给定 HTTP 状态失败；之后正常回答
struct FlakyProvider {
    status: u16,
    failures: usize,
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for FlakyProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        unreachable!("runtime uses chat_stream")
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamChunk>>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let chunks: Vec<Result<StreamChunk>> = if n < self.failures {
            vec![
                Ok(StreamChunk::Content("partial ".into())),
                Err(anyhow::Error::new(baiji_ai::ApiError {
                    provider: "mock",
                    status: self.status,
                    body: "{}".into(),
                    retry_after: Some(Duration::from_millis(1)),
                })),
            ]
        } else {
            vec![
                Ok(StreamChunk::Content("full answer".into())),
                Ok(StreamChunk::Done),
            ]
        };
        Ok(futures::stream::iter(chunks).boxed())
    }
    fn protocol(&self) -> Protocol {
        Protocol::Anthropic
    }
    fn model(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn test_retry_on_transient_status_resets_partial_stream() {
    let runtime = AgentRuntime::new(Arc::new(FlakyProvider {
        status: 503,
        failures: 1,
        calls: AtomicUsize::new(0),
    }));
    let mut messages = vec![Message::user("hi")];
    let events = drive(&runtime, &mut messages).await;

    assert_eq!(messages.last().unwrap().content, "full answer");
    // 半截文本作废的通知必须出现在重试的文本之前
    let restart = events
        .iter()
        .position(|e| matches!(e, AgentEvent::StreamRestarted))
        .expect("StreamRestarted emitted");
    let full = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TextDelta { text } if text == "full answer"))
        .unwrap();
    assert!(restart < full);
}

#[tokio::test]
async fn test_no_retry_on_client_error() {
    let provider = Arc::new(FlakyProvider {
        status: 401,
        failures: 5,
        calls: AtomicUsize::new(0),
    });
    let runtime = AgentRuntime::new(provider.clone());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut messages = vec![Message::user("hi")];
    let result = runtime
        .run(
            "sys",
            &mut messages,
            &tx,
            &CancellationToken::new(),
            &SteeringQueue::new(),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("401"));
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "401 must not be retried"
    );
}

#[test]
fn test_elide_old_tool_results_keeps_recent_and_pairing() {
    let tool_msg = |id: &str, size: usize| Message {
        tool_results: Some(vec![ToolResult {
            tool_call_id: id.to_string(),
            content: "x".repeat(size),
        }]),
        ..Message::tool("")
    };
    let mut convo = vec![Message::system("sys"), Message::user("go")];
    for i in 0..5 {
        convo.push(Message::assistant(format!("step {i}")));
        convo.push(tool_msg(&format!("t{i}"), 5000));
    }
    convo.push(tool_msg("small", 10)); // 第 6 条工具消息，很小

    let before = convo.len();
    let freed = elide_old_tool_results(&mut convo, None);
    assert!(freed > 0);
    assert_eq!(convo.len(), before, "message count must not change");

    let contents: Vec<&str> = convo
        .iter()
        .filter_map(|m| m.tool_results.as_ref())
        .map(|r| r[0].content.as_str())
        .collect();
    // 6 条里最早 3 条被精简，最近 3 条原样
    assert!(contents[..3].iter().all(|c| *c == ELIDED_NOTE));
    assert!(contents[3..5].iter().all(|c| c.len() == 5000));
    assert_eq!(contents[5].len(), 10);
    // id 配对不变
    let ids: Vec<&str> = convo
        .iter()
        .filter_map(|m| m.tool_results.as_ref())
        .map(|r| r[0].tool_call_id.as_str())
        .collect();
    assert_eq!(ids, vec!["t0", "t1", "t2", "t3", "t4", "small"]);
    // 幂等
    assert_eq!(elide_old_tool_results(&mut convo, None), 0);
}

#[test]
fn test_elide_with_spill_is_reversible() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("ctx");
    // 生产同款 spill 实现（main.rs 注入的就是它）
    let spill: SpillFn =
        Arc::new(move |content: &str| baiji_tools::spill_to_store(&store, content));
    let original = "meaningful old tool output\n".repeat(80); // 2.2KB
    let tool_msg = |id: &str, content: String| Message {
        tool_results: Some(vec![ToolResult {
            tool_call_id: id.to_string(),
            content,
        }]),
        ..Message::tool("")
    };
    let mut convo = vec![Message::system("sys"), Message::user("go")];
    convo.push(tool_msg("t0", original.clone()));
    // 凑满 KEEP_RECENT_TOOL_MESSAGES(3) 窗口，让 t0 落入精简区
    for id in ["t1", "t2", "t3"] {
        convo.push(tool_msg(id, "recent".into()));
    }

    let freed = elide_old_tool_results(&mut convo, Some(&spill));
    assert!(freed > 0);
    let stub = &convo[2].tool_results.as_ref().unwrap()[0].content;
    assert!(stub.starts_with("[ctx stub:"), "{stub}");
    assert!(stub.contains("ctx:"), "must carry recovery handle");
    // 原文可凭句柄逐字取回
    let handle = &stub[stub.find("ctx:").unwrap() + 4..][..16];
    let retrieved = std::fs::read_to_string(dir.path().join("ctx").join(handle)).unwrap();
    assert_eq!(retrieved, original);
    // 幂等：占位已小于阈值
    assert_eq!(elide_old_tool_results(&mut convo, Some(&spill)), 0);
}

#[tokio::test]
async fn test_truncated_final_answer_carries_marker() {
    let provider = Arc::new(MockProvider::new(Script::TruncatedAnswer(
        "这是一段被截断的回答",
    )));
    let runtime = AgentRuntime::new(provider).with_max_tokens(100);
    let mut history = vec![Message::user("问个长问题")];
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let answer = runtime
        .run(
            "sys",
            &mut history,
            &tx,
            &CancellationToken::new(),
            &SteeringQueue::new(),
        )
        .await
        .unwrap();
    // 答案带明示标记（用户可见，模型下轮可续写）
    assert!(answer.contains("这是一段被截断的回答"), "{answer}");
    assert!(answer.contains("被截断"), "{answer}");
    assert!(answer.contains("max_tokens=100"), "{answer}");
    // 历史中的 assistant 消息同样携带标记
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].content, answer);

    // 正常完成的答案不带标记
    let provider = Arc::new(MockProvider::new(Script::AnswerOnly("正常回答")));
    let runtime = AgentRuntime::new(provider);
    let mut history = vec![Message::user("q")];
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let answer = runtime
        .run(
            "sys",
            &mut history,
            &tx,
            &CancellationToken::new(),
            &SteeringQueue::new(),
        )
        .await
        .unwrap();
    assert_eq!(answer, "正常回答");
}
