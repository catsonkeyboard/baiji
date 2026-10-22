//! 聊天渲染的 App 级测试：工具事件 → 摘要/内联 diff、会话回放、渲染缓存失效、确认弹窗 diff

use super::*;
use baiji_agent::{AgentRuntime, ConfirmationRequest, ToolRegistry};
use baiji_ai::{ChatRequest, ChatResponse, Protocol, Provider, StreamChunk};
use futures::StreamExt as _;
use futures::stream::BoxStream;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;

struct Echo;

#[async_trait::async_trait]
impl Provider for Echo {
    async fn chat(&self, _: ChatRequest) -> anyhow::Result<ChatResponse> {
        unreachable!()
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<StreamChunk>>> {
        Ok(futures::stream::iter(vec![Ok(StreamChunk::Done)]).boxed())
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

fn test_app(dir: &std::path::Path) -> App {
    let runtime = Arc::new(AgentRuntime::new(Arc::new(Echo)).with_tools(ToolRegistry::new()));
    let harness = AgentHarness::new(runtime, dir.join("sessions")).unwrap();
    App::new(
        Arc::new(tokio::sync::Mutex::new(harness)),
        Theme::dark(),
        None,
        dir.join("config.json"),
        RuntimeSettings {
            vendor: "glm".to_string(),
            endpoint: None,
            model: Some("glm-4.7".to_string()),
            api_key: "k".to_string(),
            thinking: None,
            external_agents: Vec::new(),
        },
        String::new(),
        AutoContinueConfig::default(),
        None,
        crate::i18n::Lang::En,
    )
}

fn screen(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

fn draw(app: &mut App, terminal: &mut Terminal<TestBackend>) -> Vec<String> {
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
    screen(terminal)
}

fn started(id: &str, name: &str, args: serde_json::Value) -> AgentEvent {
    AgentEvent::ToolStarted {
        id: id.to_string(),
        name: name.to_string(),
        args,
    }
}

fn finished(id: &str, name: &str, output: &str, is_error: bool) -> AgentEvent {
    AgentEvent::ToolFinished {
        id: id.to_string(),
        name: name.to_string(),
        output: output.to_string(),
        is_error,
        duration_ms: 1,
        original_bytes: None,
        original_tokens: None,
    }
}

fn call(id: &str, text: &str) -> ChatLine {
    ChatLine::ToolCall {
        id: id.to_string(),
        text: text.to_string(),
    }
}

fn result(text: &str) -> ChatLine {
    ChatLine::Tool(text.to_string())
}

#[tokio::test]
async fn test_tool_events_render_summary_diff_and_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());
    let ui_tx = mpsc::unbounded_channel().0;

    // bash：`$ 命令` 摘要 + 结果首行
    app.handle_agent_event(started("b1", "bash", json!({"command": "ls -la"})), &ui_tx);
    app.handle_agent_event(
        finished("b1", "bash", "exit: 0\n\n[stdout]\nCargo.toml\n", false),
        &ui_tx,
    );
    assert_eq!(app.lines[0], call("b1", "● bash($ ls -la)"));
    assert_eq!(app.lines[1], result("⎿ Cargo.toml"));

    // edit：● 行之后紧跟 diff；失败的结果把 diff 标为未应用
    let args = json!({"path": "src/a.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"});
    app.handle_agent_event(started("e1", "edit", args), &ui_tx);
    assert_eq!(app.lines[2], call("e1", "● edit(src/a.rs)"));
    let ChatLine::Diff(diff) = &app.lines[3] else {
        panic!("diff line expected, got {:?}", app.lines[3]);
    };
    assert!(!diff.failed);
    assert_eq!((diff.additions, diff.deletions), (1, 1));

    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    let rows = draw(&mut app, &mut terminal);
    assert!(rows.iter().any(|r| r.contains("● bash($ ls -la)")));
    assert!(rows.iter().any(|r| r.contains("┌─ src/a.rs")));
    assert!(rows.iter().any(|r| r.contains("│ - let x = 1;")));
    assert!(rows.iter().any(|r| r.contains("│ + let x = 2;")));
    assert!(rows.iter().any(|r| r.contains("└─ +1 -1")));

    app.handle_agent_event(
        finished(
            "e1",
            "edit",
            "[Error] old_string not found in 'src/a.rs'.",
            true,
        ),
        &ui_tx,
    );
    let ChatLine::Diff(diff) = &app.lines[3] else {
        panic!("diff line expected");
    };
    assert!(diff.failed, "failed edit marks its diff as not applied");
    assert_eq!(
        app.lines[4],
        ChatLine::Tool("⎿ ✗ old_string not found in 'src/a.rs'.".to_string())
    );
    // 原地修改的条目在缓存里被重渲染（不残留旧尾注）
    let rows = draw(&mut app, &mut terminal);
    assert!(rows.iter().any(|r| r.contains("└─ +1 -1 · ✗ not applied")));

    // 长命令：单行中间省略，头尾都可见
    let long = format!("echo start {} end-marker", "x".repeat(200));
    app.handle_agent_event(started("b2", "bash", json!({ "command": long })), &ui_tx);
    let rows = draw(&mut app, &mut terminal);
    let row = rows
        .iter()
        .find(|r| r.contains("● bash($ echo start"))
        .expect("long bash row");
    assert!(row.contains('…') && row.contains("end-marker)"), "{row}");
}

/// 并行工具（join_all）：ToolFinished 乱序到达时，结果按 id 挂回各自的调用行
#[tokio::test]
async fn test_parallel_results_pair_by_id_not_arrival_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());
    let ui_tx = mpsc::unbounded_channel().0;

    app.handle_agent_event(started("r1", "read", json!({"path": "a.rs"})), &ui_tx);
    app.handle_agent_event(started("r2", "grep", json!({"pattern": "todo"})), &ui_tx);
    let edit = json!({"path": "c.rs", "old_string": "x", "new_string": "y"});
    app.handle_agent_event(started("w3", "edit", edit), &ui_tx);
    // 逆序完成
    app.handle_agent_event(finished("w3", "edit", "[Error] denied", true), &ui_tx);
    app.handle_agent_event(finished("r2", "grep", "c.rs:1: todo", false), &ui_tx);
    app.handle_agent_event(finished("r1", "read", "fn a() {}", false), &ui_tx);

    assert_eq!(app.lines.len(), 7, "{:#?}", app.lines);
    assert_eq!(app.lines[0], call("r1", "● read(a.rs)"));
    assert_eq!(app.lines[1], result("⎿ fn a() {}"));
    assert_eq!(app.lines[2], call("r2", "● grep(\"todo\")"));
    assert_eq!(app.lines[3], result("⎿ c.rs:1: todo"));
    assert_eq!(app.lines[4], call("w3", "● edit(c.rs)"));
    // 结果落在该调用的 diff 之后，且只把这个调用的 diff 标为失败
    let ChatLine::Diff(diff) = &app.lines[5] else {
        panic!("diff stays attached to its call: {:?}", app.lines[5]);
    };
    assert!(diff.failed);
    assert_eq!(app.lines[6], result("⎿ ✗ denied"));

    // 屏幕上同样成对出现
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    let rows = draw(&mut app, &mut terminal);
    let pos = |needle: &str| rows.iter().position(|r| r.contains(needle)).unwrap();
    assert_eq!(pos("● read(a.rs)") + 1, pos("⎿ fn a() {}"));
    assert_eq!(pos("● grep(\"todo\")") + 1, pos("⎿ c.rs:1: todo"));

    // 找不到调用行（空 id）：按到达顺序追加
    app.handle_agent_event(finished("", "bash", "exit: 0\n", false), &ui_tx);
    assert_eq!(app.lines.last(), Some(&result("⎿ exit 0")));
}

/// 多轮运行：每轮流出的正文在工具调用前落定，不再首尾相接地堆在所有工具行之后
#[tokio::test]
async fn test_streamed_text_flushes_before_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());
    let ui_tx = mpsc::unbounded_channel().0;

    app.handle_agent_event(
        AgentEvent::TextDelta {
            text: "Let me look.".to_string(),
        },
        &ui_tx,
    );
    app.handle_agent_event(started("r1", "read", json!({"path": "a.rs"})), &ui_tx);
    app.handle_agent_event(finished("r1", "read", "fn a() {}", false), &ui_tx);
    app.handle_agent_event(
        AgentEvent::TextDelta {
            text: "Found it.".to_string(),
        },
        &ui_tx,
    );
    app.handle_agent_event(
        AgentEvent::RunCompleted {
            answer: "Found it.".to_string(),
        },
        &ui_tx,
    );
    assert_eq!(
        app.lines,
        vec![
            ChatLine::assistant("Let me look."),
            call("r1", "● read(a.rs)"),
            result("⎿ fn a() {}"),
            ChatLine::assistant("Found it."),
        ]
    );
    assert!(app.streaming.is_empty());
}

#[tokio::test]
async fn test_rebuild_lines_replays_summaries_and_diffs() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());

    let mut assistant = baiji_ai::Message::assistant("I'll fix it.");
    assistant.tool_calls = Some(vec![
        baiji_ai::ToolCall {
            id: "c1".to_string(),
            name: "edit".to_string(),
            arguments: json!({"path": "a.rs", "edits": [
                {"old_string": "one", "new_string": "uno"},
                {"old_string": "two", "new_string": "dos"}
            ]}),
        },
        baiji_ai::ToolCall {
            id: "c2".to_string(),
            name: "bash".to_string(),
            arguments: json!({"command": "cargo test"}),
        },
    ]);
    let mut results = baiji_ai::Message::tool("");
    results.tool_results = Some(vec![
        baiji_ai::ToolResult {
            tool_call_id: "c1".to_string(),
            content: "[Error] old_string not found in 'a.rs'.".to_string(),
        },
        baiji_ai::ToolResult {
            tool_call_id: "c2".to_string(),
            content: "exit: 101\n\n[stderr]\nerror: test failed\n".to_string(),
        },
    ]);
    let messages = vec![
        baiji_ai::Message::user("fix a.rs"),
        assistant,
        results,
        baiji_ai::Message::assistant("**Done**"),
    ];
    app.rebuild_lines(&messages);

    assert_eq!(app.lines[0], ChatLine::user("fix a.rs"));
    // 带工具调用的助手消息：正文不再丢失
    assert_eq!(app.lines[1], ChatLine::assistant("I'll fix it."));
    assert_eq!(app.lines[2], call("c1", "● edit(a.rs (2 edits))"));
    let ChatLine::Diff(diff) = &app.lines[3] else {
        panic!("replayed diff expected, got {:?}", app.lines[3]);
    };
    assert_eq!((diff.additions, diff.deletions), (2, 2));
    // 历史不记 is_error：按结果文本推断失败
    assert!(diff.failed);
    // 结果按 tool_call_id 挂回各自的调用（● / ⎿ 成对交错，而不是先全部调用再全部结果）
    assert_eq!(app.lines[4], result("⎿ ✗ old_string not found in 'a.rs'."));
    assert_eq!(app.lines[5], call("c2", "● bash($ cargo test)"));
    assert_eq!(app.lines[6], result("⎿ ✗ exit 101 · error: test failed"));
    assert_eq!(app.lines[7], ChatLine::assistant("**Done**"));

    // 回放后的 Markdown 正文：粗体标记不外露
    let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
    let rows = draw(&mut app, &mut terminal);
    assert!(rows.iter().any(|r| r.trim() == "Done"), "{rows:#?}");
    assert!(!rows.iter().any(|r| r.contains("**Done**")));
    assert!(rows.iter().any(|r| r.contains("│ ⋯")), "edits separator");
}

#[tokio::test]
async fn test_render_cache_follows_resize_and_session_switch() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());
    let words = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
    app.lines.push(ChatLine::assistant(words));

    // 宽终端：一行放下
    let mut wide = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let rows = draw(&mut app, &mut wide);
    assert!(rows.iter().any(|r| r.contains(words)));

    // 缩窄：缓存按宽度失效，重新折行（不残留宽版行）
    let mut narrow = Terminal::new(TestBackend::new(30, 12)).unwrap();
    let rows = draw(&mut app, &mut narrow);
    assert!(!rows.iter().any(|r| r.contains(words)));
    assert!(rows.iter().any(|r| r.contains("alpha beta gamma")));
    assert!(
        rows.iter()
            .all(|r| !r.contains("kappa") || !r.contains("alpha"))
    );

    // 会话切换/重建：同位置条目内容变化 → 重新渲染
    app.rebuild_lines(&[baiji_ai::Message::assistant("# Fresh session")]);
    let rows = draw(&mut app, &mut wide);
    assert!(rows.iter().any(|r| r.contains("Fresh session")));
    assert!(!rows.iter().any(|r| r.contains("alpha")));
    assert!(
        !rows.iter().any(|r| r.contains("# Fresh")),
        "heading marker hidden"
    );

    // 流式尾部：未闭合围栏按代码块渲染，每帧随内容更新
    app.streaming = "Plan:\n\n```sh\ncargo bu".to_string();
    let rows = draw(&mut app, &mut wide);
    assert!(rows.iter().any(|r| r.contains("┌─ sh")));
    assert!(rows.iter().any(|r| r.contains("│ cargo bu")));
    app.streaming.push_str("ild\n```\n");
    let rows = draw(&mut app, &mut wide);
    assert!(rows.iter().any(|r| r.contains("│ cargo build")));
}

#[tokio::test]
async fn test_confirm_dialog_shows_diff_and_bash_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();

    // edit：弹窗展示 diff 行，而不是带 \n 转义的 JSON
    let (reply, _rx) = tokio::sync::oneshot::channel();
    let pending = PendingConfirm::from_dialog(crate::confirm::ConfirmDialog {
        request: ConfirmationRequest {
            tool_name: "edit".to_string(),
            args: json!({
                "path": "src/lib.rs",
                "old_string": "fn a() {\n    old_call();\n}",
                "new_string": "fn a() {\n    new_call();\n    more();\n}"
            }),
            reason: None,
        },
        reply,
    });
    assert!(matches!(pending.body, ConfirmBody::Diff(_)));
    assert!(!pending.background);
    app.pending = Some(pending);
    let rows = draw(&mut app, &mut terminal);
    assert!(rows.iter().any(|r| r.contains("┌─ src/lib.rs")));
    assert!(rows.iter().any(|r| r.contains("│ -     old_call();")));
    assert!(rows.iter().any(|r| r.contains("│ +     new_call();")));
    assert!(rows.iter().any(|r| r.contains("│ +     more();")));
    assert!(
        !rows.iter().any(|r| r.contains("\\n")),
        "no escaped newlines"
    );
    assert!(rows.iter().any(|r| r.contains("y=allow")), "keys unchanged");

    // 大 write：弹窗不截断改动（与聊天区 12 行上限不同），放不下时给出隐藏行数
    let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    let (reply, _rx2) = tokio::sync::oneshot::channel();
    let pending = PendingConfirm::from_dialog(crate::confirm::ConfirmDialog {
        request: ConfirmationRequest {
            tool_name: "write".to_string(),
            args: json!({"path": "big.txt", "content": content}),
            reason: None,
        },
        reply,
    });
    let ConfirmBody::Diff(view) = &pending.body else {
        panic!("write renders as a diff");
    };
    assert_eq!(view.rows.len(), 100, "dialog diff is uncapped");
    app.pending = Some(pending);
    let rows = draw(&mut app, &mut terminal);
    assert!(rows.iter().any(|r| r.contains("more lines hidden")));

    // bash：命令原文（多行保留）+ 后台运行附注
    let (reply, _rx3) = tokio::sync::oneshot::channel();
    let pending = PendingConfirm::from_dialog(crate::confirm::ConfirmDialog {
        request: ConfirmationRequest {
            tool_name: "bash".to_string(),
            args: json!({"command": "cd /tmp &&\n  make all", "run_in_background": true}),
            reason: None,
        },
        reply,
    });
    assert!(pending.background);
    app.pending = Some(pending);
    let rows = draw(&mut app, &mut terminal);
    assert!(rows.iter().any(|r| r.contains("cd /tmp &&")));
    assert!(rows.iter().any(|r| r.contains("  make all")));
    assert!(rows.iter().any(|r| r.contains("runs in background")));

    // 其它工具：仍为 pretty JSON
    let (reply, _rx4) = tokio::sync::oneshot::channel();
    let pending = PendingConfirm::from_dialog(crate::confirm::ConfirmDialog {
        request: ConfirmationRequest {
            tool_name: "mcp.fs.delete".to_string(),
            args: json!({"target": "x"}),
            reason: None,
        },
        reply,
    });
    assert!(matches!(&pending.body, ConfirmBody::Text(t) if t.contains("\"target\": \"x\"")));

    // 升级确认：原因（含模型的 justification）逐行展示在命令上方
    let (reply, _rx5) = tokio::sync::oneshot::channel();
    let pending = PendingConfirm::from_dialog(crate::confirm::ConfirmDialog {
        request: ConfirmationRequest {
            tool_name: "bash".to_string(),
            args: json!({"command": "git reset --hard HEAD~1"}),
            reason: Some(
                "- `git reset --hard` discards uncommitted changes irreversibly\n\
                 justification: user asked to drop the last commit"
                    .to_string(),
            ),
        },
        reply,
    });
    app.pending = Some(pending);
    let rows = draw(&mut app, &mut terminal);
    let reason_row = rows.iter().position(|r| r.contains("Irreversible")).expect("reason title");
    let command_row = rows.iter().position(|r| r.contains("git reset --hard HEAD~1")).unwrap();
    assert!(reason_row < command_row, "reason sits above the command");
    assert!(rows.iter().any(|r| r.contains("discards uncommitted changes")));
    assert!(rows.iter().any(|r| r.contains("justification: user asked")));
}
