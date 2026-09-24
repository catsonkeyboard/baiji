//! App 与向导/选择器/补全的测试（原 app.rs 内联测试整体迁出，含 540 行渲染冒烟）

use super::helpers::SLASH_COMMANDS;
use super::*;
use baiji_harness::SessionMeta;
use helpers::slash_hints;

#[test]
fn test_format_bytes() {
    assert_eq!(format_bytes(512), "512B");
    assert_eq!(format_bytes(2048), "2.0KB");
    assert_eq!(format_bytes(3 * 1024 * 1024), "3.0MB");
}

#[test]
fn test_sanitize_paste() {
    // Key 粘贴：常见尾随换行被去掉
    assert_eq!(sanitize_paste("sk-abc123\n"), "sk-abc123");
    assert_eq!(sanitize_paste("  sk-abc123  \r\n"), "sk-abc123");
    // 多行折叠为单行
    assert_eq!(sanitize_paste("line1\nline2\n"), "line1 line2");
    assert_eq!(sanitize_paste(""), "");
}

/// 全布局渲染冒烟（TestBackend）：修复过 chunks 越界 panic 的回归测试；
/// 含向导输入步骤的按键/粘贴路径（回归：字符曾被吞掉）
#[tokio::test]
async fn test_render_all_layouts_no_panic() {
    use async_trait::async_trait;
    use baiji_agent::{AgentRuntime, ToolRegistry};
    use baiji_ai::{ChatRequest, ChatResponse, Protocol, Provider, StreamChunk};
    use futures::StreamExt as _;
    use futures::stream::BoxStream;

    struct Echo;
    #[async_trait]
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

    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(AgentRuntime::new(Arc::new(Echo)).with_tools(ToolRegistry::new()));
    let mut harness = AgentHarness::new(runtime, dir.path().join("sessions")).unwrap();
    let todo_store = Arc::new(baiji_harness::TodoStore::new());
    harness.set_todos(todo_store.clone());
    let mut app = App::new(
        Arc::new(tokio::sync::Mutex::new(harness)),
        Theme::dark(),
        None,
        dir.path().join("config.json"),
        RuntimeSettings {
            vendor: "glm".to_string(),
            endpoint: None,
            model: Some("glm-4.7".to_string()),
            api_key: "k".to_string(),
            thinking: None,
            // 假外部 agent：验证动态命令 ghost 与分发（printf 立即返回）
            external_agents: vec![baiji_tools::tools::external_agent::ExternalAgentSpec {
                name: "fakeagent".to_string(),
                command: "printf 'ans:%s' {prompt}".to_string(),
                description: String::new(),
                timeout_secs: 10,
            }],
        },
        "max_turns: 24 · compaction: on (auto)".to_string(),
        AutoContinueConfig::default(),
        None,
        // 冒烟测试验证中文包（默认英文包另有单测）
        crate::i18n::Lang::Zh,
    );
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    let screen = |t: &ratatui::Terminal<ratatui::backend::TestBackend>| -> Vec<String> {
        let buffer = t.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    };

    // 普通三段布局（空会话：欢迎屏兜底）
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    assert!(
        rows.iter().any(|r| r.contains("██████╗")),
        "welcome screen shows the logo"
    );
    assert!(
        rows.iter().any(|r| r.contains("/help")),
        "welcome screen shows key hints"
    );

    // 多行回答保留换行（回归：曾被压成一行）；长中文回复贴底时末行可见
    // （回归：按字符数而非显示宽度估行，滚不到底）
    app.lines
        .push(ChatLine::Assistant("fn main() {\n    hi();\n}".to_string()));
    app.lines.push(ChatLine::Assistant(format!(
        "{}终点标记",
        "中文".repeat(400)
    )));
    app.scroll_to_bottom();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    // 宽字符后跟一个空占位格，去掉空格再比对
    assert!(
        rows.iter().any(|r| r.replace(' ', "").contains("终点标记")),
        "bottom of a long CJK reply must be reachable"
    );
    // 思考流同样贴底滚动（回归：旧 400 字符尾部窗口呈"前沿收缩"而非滚动）
    // 保留一行历史：空会话画的是欢迎屏而非聊天区
    app.lines = vec![ChatLine::user("go")];
    app.thinking = format!("{}思考尾部标记", "推理".repeat(400));
    app.scroll_to_bottom();
    terminal.clear().unwrap(); // TestBackend 宽字符差分残留：断言前全量重绘
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert!(
        screen(&terminal)
            .iter()
            .any(|r| r.replace(' ', "").contains("思考尾部标记")),
        "long thinking must follow the bottom like the answer stream"
    );
    app.thinking.clear();
    // 恢复后续断言用的代码块消息
    app.lines = vec![
        ChatLine::Assistant("fn main() {\n    hi();\n}".to_string()),
        ChatLine::Assistant(format!("{}终点标记", "中文".repeat(400))),
    ];
    app.scroll = 0;
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    let code_row = rows.iter().position(|r| r.contains("fn main() {")).unwrap();

    assert!(
        rows[code_row + 1].contains("    hi();"),
        "newlines preserved"
    );
    assert!(rows[code_row + 2].contains('}'));
    app.lines.clear();

    // 用户消息（❯ 前缀 + 底色条）与工具活动行：● Name(args) / ⎿ 结果（错误带 ✗）
    app.lines.push(ChatLine::user("帮我看看这个项目"));
    app.lines
        .push(ChatLine::Tool(r#"● read({"path":"lib.rs"})"#.to_string()));
    app.lines.push(ChatLine::Tool("⎿ 200 行已读取".to_string()));
    app.lines
        .push(ChatLine::Tool("⎿ ✗ bash: exit 1".to_string()));
    app.scroll_to_bottom();
    // TestBackend 的增量刷新对宽字符覆盖有残留：断言前强制全量重绘
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    assert!(rows.iter().any(|r| r.contains("❯")), "user prompt mark");
    assert!(
        rows.iter()
            .any(|r| r.replace(' ', "").contains("帮我看看这个项目"))
    );
    assert!(rows.iter().any(|r| r.contains("● read")));
    assert!(
        rows.iter()
            .any(|r| r.replace(' ', "").contains("⎿200行已读取"))
    );

    // 右上角悬浮 todo 面板：有清单才出现
    todo_store.replace(vec![
        baiji_harness::TodoItem {
            id: 1,
            content: "分析依赖".to_string(),
            status: baiji_harness::TodoStatus::Done,
            note: None,
        },
        baiji_harness::TodoItem {
            id: 2,
            content: "实现悬浮面板".to_string(),
            status: baiji_harness::TodoStatus::InProgress,
            note: None,
        },
        baiji_harness::TodoItem {
            id: 3,
            content: "补测试".to_string(),
            status: baiji_harness::TodoStatus::Pending,
            note: None,
        },
    ]);
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    // 宽字符的跳过格在 TestBackend 里呈现为空格：比对前去掉
    let plain = |s: &str| s.replace(' ', "");
    assert!(rows.iter().any(|r| r.contains("Todo")), "todo panel title");
    assert!(
        rows.iter()
            .any(|r| plain(r).contains(&plain("实现悬浮面板")))
    );
    // 清空后面板消失
    todo_store.replace(Vec::new());
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert!(
        !screen(&terminal).iter().any(|r| r.contains("Todo")),
        "panel hidden when list empty"
    );

    // 运行态：顶栏/状态栏运行指示 + 输入框顶边 steering 提示
    app.agent_running = true;
    app.current_turn = 2;
    app.tool_calls = 3;
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    assert!(rows.iter().any(|r| r.contains("Turn 2 · 3 tools")));
    assert!(rows.iter().any(|r| {
        r.replace(' ', "")
            .contains(&"Esc 取消 · 输入即 steering".replace(' ', ""))
    }));
    app.agent_running = false;
    app.current_turn = 0;
    app.tool_calls = 0;

    // /thinking <level>：热切换 + 落盘（save 读文件，先写入基线配置）
    let ui_tx = tokio::sync::mpsc::unbounded_channel().0;
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"vendor":"glm","api_key":"k"}"#,
    )
    .unwrap();
    app.handle_slash("thinking", "high", &ui_tx).await;
    assert_eq!(app.settings.thinking, Some(baiji_ai::ThinkingLevel::High));
    assert!(
        std::fs::read_to_string(dir.path().join("config.json"))
            .unwrap()
            .contains("\"thinking\": \"high\""),
        "persisted to the config file"
    );
    assert_eq!(
        app.harness.try_lock().unwrap().thinking_level(),
        Some(baiji_ai::ThinkingLevel::High),
        "runtime hot-swapped"
    );
    // off 关闭
    app.handle_slash("thinking", "off", &ui_tx).await;
    assert_eq!(app.settings.thinking, None);
    // 非法值提示用法
    app.handle_slash("thinking", "bogus", &ui_tx).await;
    assert!(
        app.lines
            .iter()
            .any(|l| matches!(l, ChatLine::System(s) if s.contains("用法：/thinking")))
    );

    // /plan 计划模式：镜像切换 + runtime 门控生效 + 状态栏与输入框指示
    app.handle_slash("plan", "", &ui_tx).await;
    assert!(app.plan_mode(), "slash toggles the display mirror");
    assert!(
        app.harness.try_lock().unwrap().plan_mode(),
        "runtime gate hot-swapped"
    );
    assert!(app.status_right().contains("计划·只读"));
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert!(
        screen(&terminal)
            .iter()
            .any(|r| r.replace(' ', "").contains("计划模式（只读）")),
        "input box top border shows the plan-mode hint"
    );
    app.handle_slash("plan", "off", &ui_tx).await;
    assert!(!app.plan_mode());
    assert!(!app.harness.try_lock().unwrap().plan_mode());

    // 批准流：开启 → 模拟计划回答完成 → awaiting → 空回车批准执行
    app.handle_slash("plan", "on", &ui_tx).await;
    app.handle_agent_event(
        AgentEvent::RunCompleted {
            answer: "计划：三步实施".to_string(),
        },
        &ui_tx,
    );
    assert!(app.awaiting_plan(), "plan answer enters approval state");
    assert!(app.status_right().contains("计划·待执行"));
    app.input.clear();
    app.handle_key(KeyEvent::new(K::Enter, M::NONE), &ui_tx)
        .await;
    assert!(!app.plan_mode(), "approval exits plan mode");
    assert!(!app.awaiting_plan());
    assert!(app.agent_running, "approval spawns the execute run");
    assert!(
        app.lines
            .iter()
            .any(|l| matches!(l, ChatLine::System(s) if s.contains("执行计划"))),
        "execute prompt visible in history"
    );
    app.agent_running = false; // 后台 run 与后续断言解耦

    // /subagents 面板：打开 → 渲染（角色行 + 目录脚注）→ r 重载 → Esc 关闭
    let agents_dir = dir.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("reviewer.md"),
        "---\nname: reviewer\ndescription: finds issues\n---\nYou review code.",
    )
    .unwrap();
    let registry = Arc::new(baiji_agent::SubagentRegistry::new(vec![agents_dir]));
    registry.load();
    app.harness.lock().await.set_subagents(registry);
    app.handle_slash("subagents", "", &ui_tx).await;
    assert!(app.subagents_rows().is_some(), "panel opens");
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    assert!(rows.iter().any(|r| r.contains("Subagents")));
    assert!(rows.iter().any(|r| r.contains("reviewer")));
    assert!(
        rows.iter().any(|r| r.replace(' ', "").contains("角色目录")),
        "dirs footer shown"
    );
    // r 热重载（面板键路由）
    app.handle_key(KeyEvent::new(K::Char('r'), M::NONE), &ui_tx)
        .await;
    assert!(
        app.lines
            .iter()
            .any(|l| matches!(l, ChatLine::System(s) if s.contains("已重载子代理角色：1"))),
        "reload reports the count"
    );
    // Esc 关闭
    app.handle_key(KeyEvent::new(K::Esc, M::NONE), &ui_tx).await;
    assert!(app.subagents_rows().is_none(), "panel closes");

    // 外部 coding agent 动态命令：ghost 提示 → Tab 补全 → 分发执行
    app.input = "/fak".to_string();
    let ghost = app.slash_ghost().expect("dynamic agent ghost");
    assert_eq!(ghost.0, "fakeagent");
    assert!(ghost.1.contains("外部"), "{}", ghost.1);
    app.handle_key(KeyEvent::new(K::Tab, M::NONE), &ui_tx).await;
    assert_eq!(
        app.input(),
        "/fakeagent ",
        "Tab completes the dynamic command"
    );
    // 分发：独立通道收事件（ToolStarted / ToolFinished / RunCompleted）
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    app.handle_slash("fakeagent", "hello", &tx2).await;
    assert!(app.agent_running, "external run marks the running state");
    let mut finished = String::new();
    let mut seen = 0;
    while seen < 3 {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx2.recv())
            .await
            .expect("event within timeout")
            .expect("channel open");
        if let UiEvent::Agent(AgentEvent::ToolFinished { output, .. }) = ev {
            finished = output;
        }
        seen += 1;
    }
    assert!(finished.contains("ans:hello"), "{finished}");
    assert!(
        app.lines
            .iter()
            .any(|l| matches!(l, ChatLine::User(s) if s.contains("/fakeagent hello"))),
        "the prompt is echoed into the chat"
    );
    app.agent_running = false; // RunCompleted 由事件循环处理，这里手动复位

    // 工作流模式命令：spec / goal / experts
    let (tx3, mut rx3) = tokio::sync::mpsc::unbounded_channel();
    // 等待此前后台 run（计划批准/Echo）释放 harness 锁，保证断言确定性
    async fn wait_lock(app: &App) {
        for _ in 0..200 {
            if app.harness.try_lock().is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    wait_lock(&app).await;
    app.handle_slash("experts", "", &tx3).await;
    assert!(
        app.harness.try_lock().unwrap().experts_active(),
        "experts toggled on"
    );
    app.handle_slash("goal", "ship the release", &tx3).await;
    wait_lock(&app).await;
    assert_eq!(
        app.harness.try_lock().unwrap().goal_objective(),
        Some("ship the release".to_string()).as_deref()
    );
    assert!(app.agent_running, "goal starts a run");
    app.agent_running = false;
    // spec：无任务清单时 approve 提示补稿；show/list 不 panic
    app.handle_slash("spec", "add dark mode toggle", &tx3).await;
    wait_lock(&app).await;
    assert!(app.harness.try_lock().unwrap().spec_active().is_some());
    app.agent_running = false;
    app.handle_slash("spec", "approve", &tx3).await;
    // 该冒烟测试用中文包运行（见 App::new 的 Lang::Zh）；
    // spec approve 提示语已 i18n 化，两语言断言各自的关键词
    assert!(
        app.lines.iter().any(|l| matches!(l, ChatLine::System(s)
            if s.contains("no '- [ ] task' lines") || s.contains("还没有 '- [ ] task'"))),
        "approve without tasks asks for a draft"
    );
    app.handle_slash("spec", "list", &tx3).await;
    app.handle_slash("spec", "off", &tx3).await;
    assert!(app.harness.try_lock().unwrap().spec_active().is_none());
    app.handle_slash("goal", "off", &tx3).await;
    app.handle_slash("experts", "off", &tx3).await;
    assert!(!app.harness.try_lock().unwrap().experts_active());
    let _ = rx3.try_recv(); // 排空（后台 run 事件，不参与断言）

    // /todos 与 /quit 命令分发（Enter 路径）
    let ui_tx = tokio::sync::mpsc::unbounded_channel().0;
    todo_store.replace(vec![baiji_harness::TodoItem {
        id: 1,
        content: "分析依赖".to_string(),
        status: baiji_harness::TodoStatus::Done,
        note: None,
    }]);
    app.input = "/todos".to_string();
    app.handle_key(KeyEvent::new(K::Enter, M::NONE), &ui_tx)
        .await;
    assert!(
        app.lines
            .iter()
            .any(|l| matches!(l, ChatLine::System(s) if s.contains("[x] 分析依赖"))),
        "/todos prints the list"
    );
    app.input = "/quit".to_string();
    assert!(
        app.handle_key(KeyEvent::new(K::Enter, M::NONE), &ui_tx)
            .await,
        "/quit requests exit"
    );

    // 输入框灰色补全（ghost）：补全紧跟已输入文本（颜色边界即光标，
    // 不再有 ▏ 分隔），状态栏右侧临时显示用法；Tab 接受补全
    app.input = "/mo".to_string();
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    let input_row = rows
        .iter()
        .find(|r| r.contains("> /mo"))
        .expect("input row");
    assert!(
        input_row.contains("/model "),
        "ghost continues the typed text with no separator: {input_row}"
    );
    assert!(
        !input_row.contains('▏'),
        "no cursor artifact while ghosting"
    );
    assert!(
        rows.iter()
            .any(|r| r.replace(' ', "").contains(&"切换模型".to_string())),
        "status bar shows the ghost command's usage"
    );
    let ui_tx = tokio::sync::mpsc::unbounded_channel().0;
    app.handle_key(KeyEvent::new(K::Tab, M::NONE), &ui_tx).await;
    assert_eq!(app.input(), "/model ", "Tab accepts the ghost completion");
    // 参数区不再出补全；未知前缀也没有
    for input in ["/model glm-4.7", "/zzz", "普通消息"] {
        app.input = input.to_string();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    }

    // 裸 "/"：展示全部命令清单（灰字，宽度内尽量多列）+ Tab 补全首个
    app.input = "/".to_string();
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    let bare_row = rows
        .iter()
        .find(|r| r.contains("> /"))
        .expect("bare slash row");
    assert!(
        bare_row.contains("btw") && bare_row.contains("compact"),
        "bare slash lists the commands: {bare_row}"
    );
    assert!(
        bare_row.contains('…'),
        "list is width-capped with an ellipsis"
    );
    assert!(!bare_row.contains('▏'));
    app.handle_key(KeyEvent::new(K::Tab, M::NONE), &ui_tx).await;
    assert_eq!(
        app.input(),
        "/btw ",
        "Tab from bare slash completes the first command"
    );
    app.input.clear();

    // 输入单字符渲染恰好一次（回归：输入框曾把已输入文本重复渲染两遍）
    app.input = "s".to_string();
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    let row = rows.iter().find(|r| r.contains("> s")).expect("input row");
    assert_eq!(
        row.matches('s').count(),
        1,
        "typed char rendered exactly once: {row}"
    );
    app.input.clear();

    // 向导打开（overlay 渲染路径）
    app.input.clear();
    app.wizard = Some(ConfigWizard::new("glm", None));
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    // 端点列表 + 模型列表（含错误态）
    if let Some(w) = app.wizard.as_mut() {
        w.step = WizardStep::Endpoint;
    }
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    if let Some(w) = app.wizard.as_mut() {
        w.step = WizardStep::Model;
        w.fetching = false;
        w.models_error = Some("401".to_string());
    }
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    // Key 输入步骤（输入型对话框）
    if let Some(w) = app.wizard.as_mut() {
        w.step = WizardStep::Key;
    }
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();

    // 输入步骤按键路径：字符/退格/粘贴必须进输入框（回归：曾被吞掉）
    use crossterm::event::{KeyCode as K, KeyModifiers as M};
    let ui_tx = tokio::sync::mpsc::unbounded_channel().0;
    app.handle_key(KeyEvent::new(K::Char('s'), M::NONE), &ui_tx)
        .await;
    assert_eq!(app.input(), "s", "after 's'");
    app.handle_key(KeyEvent::new(K::Char('k'), M::NONE), &ui_tx)
        .await;
    assert_eq!(app.input(), "sk", "after 'k'");
    app.handle_key(KeyEvent::new(K::Backspace, M::NONE), &ui_tx)
        .await;
    assert_eq!(app.input(), "s", "after backspace");
    app.handle_key(KeyEvent::new(K::Char('-'), M::NONE), &ui_tx)
        .await;
    assert_eq!(app.input(), "s-", "after '-'");
    // 粘贴同样进输入框（含尾随换行被净化）
    app.handle_paste("live-key\n".to_string());
    assert_eq!(app.input(), "s-live-key");
    // Esc 关闭向导
    app.handle_key(KeyEvent::new(K::Esc, M::NONE), &ui_tx).await;
    assert!(!app.wizard_active());
}

#[test]
fn test_wizard_input_step_routing() {
    // 向导输入步骤只拦截 Enter/Esc；字符、退格、粘贴必须放行到输入框
    assert!(wizard_input_step_intercept(KeyCode::Enter));
    assert!(wizard_input_step_intercept(KeyCode::Esc));
    assert!(!wizard_input_step_intercept(KeyCode::Char('g')));
    assert!(!wizard_input_step_intercept(KeyCode::Backspace));
    assert!(!wizard_input_step_intercept(KeyCode::Tab));
}

#[test]
fn test_slash_hints_filtering() {
    // "/" → 全部命令
    let all = slash_hints("/", crate::i18n::Lang::Zh).unwrap();
    assert_eq!(all.len(), SLASH_COMMANDS.len());
    // 前缀过滤
    let hints = slash_hints("/m", crate::i18n::Lang::Zh).unwrap();
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].0, "model");
    let hints = slash_hints("/MO", crate::i18n::Lang::Zh).unwrap(); // 大小写不敏感
    assert_eq!(hints[0].0, "model");
    // 完整命令仍显示（用于用法提示）
    let hints = slash_hints("/model", crate::i18n::Lang::Zh).unwrap();
    assert_eq!(hints.len(), 1);
    // 参数区：只保留精确命令的用法
    let hints = slash_hints("/model glm-4.7", crate::i18n::Lang::Zh).unwrap();
    assert_eq!(hints.len(), 1);
    assert!(hints[0].1.contains("切换模型"));
    // 无匹配 / 非斜杠
    assert!(
        slash_hints("/xyz", crate::i18n::Lang::Zh)
            .unwrap()
            .is_empty()
    );
    assert!(slash_hints("普通消息", crate::i18n::Lang::Zh).is_none());
}

#[test]
fn test_ghost_completion() {
    // 裸 "/"、非斜杠输入、参数区、无匹配：都不出补全
    assert!(ghost_completion("/", crate::i18n::Lang::Zh).is_none());
    assert!(ghost_completion("普通消息", crate::i18n::Lang::Zh).is_none());
    assert!(ghost_completion("/model glm-4.7", crate::i18n::Lang::Zh).is_none());
    assert!(ghost_completion("/zzz", crate::i18n::Lang::Zh).is_none());
    // 首个前缀匹配（大小写不敏感；字典序）
    assert_eq!(
        ghost_completion("/mo", crate::i18n::Lang::Zh).unwrap().0,
        "model"
    );
    assert_eq!(
        ghost_completion("/MO", crate::i18n::Lang::Zh).unwrap().0,
        "model"
    );
    assert_eq!(
        ghost_completion("/t", crate::i18n::Lang::Zh).unwrap().0,
        "tasks"
    );
}

#[test]
fn test_split_slash() {
    assert_eq!(split_slash("/model glm-4.7"), Some(("model", "glm-4.7")));
    assert_eq!(split_slash("/status"), Some(("status", "")));
    assert_eq!(split_slash("/config  "), Some(("config", "")));
    assert_eq!(split_slash("普通消息"), None);
    assert_eq!(split_slash("/"), None);
}

#[test]
fn test_wizard_step_traits() {
    assert!(WizardStep::Vendor.is_list());
    assert!(!WizardStep::Vendor.is_input());
    assert!(WizardStep::Key.is_input());
    assert!(WizardStep::Model.is_list());
    assert!(WizardStep::ModelManual.is_input());

    let mut wizard = ConfigWizard::new("glm", None);
    assert_eq!(wizard.step, WizardStep::Vendor);
    wizard.move_down(3);
    wizard.move_down(3); // 到底不再前进
    assert_eq!(wizard.selected, 2);
    wizard.move_up();
    assert_eq!(wizard.selected, 1);
}

#[test]
fn test_picker_project_filter_toggle_and_tags() {
    let mk = |id: &str, project: Option<&str>| SessionMeta {
        id: id.to_string(),
        parent_id: None,
        created_at: format!("2026-09-17T00:00:0{id}:00Z"),
        title: Some(format!("t-{id}")),
        project: project.map(str::to_string),
    };
    // 三个项目:alpha(当前)、beta、旧会话(无项目)
    let metas = vec![
        mk("cur", Some("alpha-1111")),
        mk("other", Some("beta-2222")),
        mk("legacy", None),
    ];

    // 当前会话有项目 → 初始只显示本项目
    let picker = SessionPicker::from_metas(metas.clone(), "cur");
    let ids: Vec<&str> = picker.items.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["cur"], "project view shows only current project");
    assert!(picker.filtering_by_project());
    assert_eq!(picker.selected, 0);

    // a 切换到全部:跨项目附 ⌂ 标注
    let mut picker = SessionPicker::from_metas(metas.clone(), "cur");
    picker.toggle_project_filter();
    let ids: Vec<&str> = picker.items.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids.len(), 3, "all view");
    assert!(!picker.filtering_by_project());
    let rows = picker.display_rows("cur", "(无标题)");
    assert!(rows.iter().any(|r| r.contains("⌂beta-2222")), "{rows:?}");
    assert!(
        rows.iter().any(|r| r.contains("⌂?")),
        "legacy tagged: {rows:?}"
    );
    assert!(picker.selected < picker.items.len());

    // 当前会话为旧会话(无项目) → 初始显示全部
    let picker = SessionPicker::from_metas(metas, "legacy");
    assert!(!picker.filtering_by_project());
    assert_eq!(picker.items.len(), 3);
}

#[test]
fn test_picker_navigation_and_rows() {
    let mk = |id: &str, parent: Option<&str>| SessionMeta {
        id: id.to_string(),
        parent_id: parent.map(String::from),
        created_at: format!("2026-09-17T00:0{id}:00Z"),
        title: Some(format!("title-{id}")),
        project: None,
    };
    let mut picker =
        SessionPicker::from_metas(vec![mk("3", None), mk("2", Some("1")), mk("1", None)], "2");
    // 树形顺序：根最新在前，分叉紧跟其父
    let ids: Vec<&str> = picker.items.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["3", "1", "2"]);
    assert_eq!(picker.selected, 2, "current session preselected");
    let rows = picker.display_rows("2", "(无标题)");
    assert!(rows[2].starts_with("▸")); // 当前会话标记
    assert!(rows[2].contains("└ 2 · title-2")); // 分叉缩进
    picker.selected = 0;
}
