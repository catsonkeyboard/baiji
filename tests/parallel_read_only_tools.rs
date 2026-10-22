//! 只读工具并发的运行时级验证（真实 AgentRuntime + 真实内置工具）：
//! - 一轮里全是只读调用 → 走 join_all 并发路径；read 的进程内重读缓存与 ctx store
//!   spill 在并发下结果正确（同参并发读取要么都拿到全文，要么拿到可 expand 的 stub）
//! - 被 HITL 门控的只读工具回退串行：审批一次只裁决一个请求

use async_trait::async_trait;
use baiji_agent::{
    AgentRuntime, AgentTool, Approver, ChatRequest, ChatResponse, ConfirmationDecision,
    ConfirmationGate, ConfirmationRequest, Message, SteeringQueue, StreamChunk, ToolOutput,
    ToolRegistry,
};
use baiji_ai::{Protocol, Provider};
use baiji_tools::ExecutionEnv;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 第一轮在同一条 assistant 消息里发出全部工具调用，第二轮给出最终回答
struct ScriptedProvider {
    calls: Vec<(&'static str, Value)>,
    turn: AtomicUsize,
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn chat(&self, _: ChatRequest) -> anyhow::Result<ChatResponse> {
        unreachable!("runtime uses chat_stream")
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<StreamChunk>>> {
        let chunks: Vec<anyhow::Result<StreamChunk>> =
            if self.turn.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut chunks = Vec::new();
                for (i, (name, args)) in self.calls.iter().enumerate() {
                    chunks.push(Ok(StreamChunk::ToolCallStart {
                        id: format!("t{i}"),
                        name: name.to_string(),
                    }));
                    chunks.push(Ok(StreamChunk::ToolCallArguments {
                        id: format!("t{i}"),
                        arguments: args.to_string(),
                    }));
                }
                chunks.push(Ok(StreamChunk::Done));
                chunks
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

/// 并发探针：两个探针只有被并发执行时才会在 barrier 相遇
struct BarrierProbe {
    name: &'static str,
    barrier: Arc<tokio::sync::Barrier>,
    met: Arc<AtomicBool>,
}

#[async_trait]
impl AgentTool for BarrierProbe {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "probe"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn parallel(&self) -> bool {
        true
    }
    async fn execute(&self, _: Value) -> anyhow::Result<ToolOutput> {
        if tokio::time::timeout(Duration::from_secs(5), self.barrier.wait())
            .await
            .is_ok()
        {
            self.met.store(true, Ordering::SeqCst);
        }
        Ok(ToolOutput::ok(self.name))
    }
}

fn workspace() -> (tempfile::TempDir, ExecutionEnv) {
    let dir = tempfile::tempdir().unwrap();
    // >2KB 的读取输出才进入重读缓存（并发时同时触发缓存与 spill）
    let big: String = (1..=200)
        .map(|i| format!("line {i} lorem ipsum dolor sit amet\n"))
        .collect();
    std::fs::write(dir.path().join("big.txt"), big).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "mod util;\nuse crate::util::helper;\n\npub fn alpha() {\n    helper(); // lorem\n}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("src/util.rs"), "pub fn helper() {}\n").unwrap();
    let env = ExecutionEnv::new(dir.path()).with_ctx_store(dir.path().join(".ctx-store"));
    (dir, env)
}

async fn run(runtime: &AgentRuntime) -> Vec<Message> {
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
    history
}

fn results(history: &[Message]) -> Vec<String> {
    history
        .iter()
        .find_map(|m| m.tool_results.as_ref())
        .expect("tool results recorded")
        .iter()
        .map(|r| r.content.clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_batch_runs_concurrently_and_stays_correct() {
    let (_dir, env) = workspace();
    let store = env.ctx_store().unwrap().to_path_buf();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let met = Arc::new(AtomicBool::new(false));

    let mut tools = ToolRegistry::new();
    for tool in baiji_tools::builtin_tools(env.clone()) {
        tools.register(tool);
    }
    for name in ["probe_a", "probe_b"] {
        tools.register(Arc::new(BarrierProbe {
            name,
            barrier: barrier.clone(),
            met: met.clone(),
        }));
    }
    let read_big = json!({"path": "big.txt"});
    let calls = vec![
        ("read", read_big.clone()),
        ("read", read_big.clone()),
        ("read", read_big),
        ("grep", json!({"pattern": "lorem"})),
        ("find", json!({"pattern": "lib"})),
        ("ls", json!({"path": "src"})),
        ("search", json!({"query": "helper"})),
        ("imports", json!({"target": "util"})),
        ("probe_a", json!({})),
        ("probe_b", json!({})),
    ];
    let runtime = AgentRuntime::new(Arc::new(ScriptedProvider {
        calls,
        turn: AtomicUsize::new(0),
    }))
    .with_tools(tools);

    let history = run(&runtime).await;
    assert!(
        met.load(Ordering::SeqCst),
        "an all-read-only turn must take the concurrent path"
    );
    let out = results(&history);
    assert_eq!(out.len(), 10);
    for content in &out {
        assert!(
            !content.starts_with("[Error") && !content.starts_with("[Policy"),
            "{content}"
        );
    }
    // 同参并发读取：全文，或指向全文的 [unchanged] stub（句柄必须可取回）
    let expand = baiji_tools::tools::ExpandTool::new(Arc::new(
        ExecutionEnv::new(store.parent().unwrap()).with_ctx_store(&store),
    ));
    for content in &out[..3] {
        let full = if content.starts_with("[unchanged:") {
            let handle = content
                .split("ctx:")
                .nth(1)
                .unwrap()
                .chars()
                .take(16)
                .collect::<String>();
            expand
                .execute(json!({"handle": handle}))
                .await
                .unwrap()
                .content
        } else {
            content.clone()
        };
        assert!(full.contains("1\tline 1 lorem"), "{full}");
        assert!(full.contains("200\tline 200 lorem"), "{full}");
    }
    assert!(out[3].contains("lorem"), "{}", out[3]);
    assert!(out[4].contains("lib.rs"), "{}", out[4]);
    assert!(out[5].contains("util.rs"), "{}", out[5]);
    assert!(out[6].contains("helper"), "{}", out[6]);
    assert!(out[7].contains("lib.rs"), "{}", out[7]);
}

/// 记录同时在审批中的请求数
struct TrackingApprover {
    active: AtomicUsize,
    max_active: AtomicUsize,
}

#[async_trait]
impl Approver for TrackingApprover {
    async fn confirm(&self, _: ConfirmationRequest, _: &CancellationToken) -> ConfirmationDecision {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(40)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        ConfirmationDecision::Allow
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hitl_gated_read_only_calls_stay_sequential() {
    let (_dir, env) = workspace();
    let mut tools = ToolRegistry::new();
    for tool in baiji_tools::builtin_tools(env) {
        tools.register(tool);
    }
    let approver = Arc::new(TrackingApprover {
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
    });
    let runtime = AgentRuntime::new(Arc::new(ScriptedProvider {
        calls: vec![
            ("read", json!({"path": "big.txt", "limit": 5})),
            ("read", json!({"path": "src/lib.rs"})),
            ("grep", json!({"pattern": "helper"})),
        ],
        turn: AtomicUsize::new(0),
    }))
    .with_tools(tools)
    .with_confirmation(ConfirmationGate::new(
        vec!["read".to_string()],
        approver.clone(),
    ));

    let history = run(&runtime).await;
    // 两个 read 都经审批放行，且审批从不重叠（并发会让 TUI 顶掉未裁决的请求）
    assert_eq!(approver.max_active.load(Ordering::SeqCst), 1);
    let out = results(&history);
    assert!(out[0].contains("1\tline 1 lorem"), "{}", out[0]);
    assert!(out[1].contains("pub fn alpha"), "{}", out[1]);
    assert!(out[2].contains("helper"), "{}", out[2]);
}
