//! 输入框 / 粘贴 / 提示词历史 / Esc / 滚动 / steering 兜底的 App 级测试

use super::*;
use crossterm::event::{KeyCode as K, KeyModifiers as M};

struct Echo;

#[async_trait::async_trait]
impl baiji_ai::Provider for Echo {
    async fn chat(&self, _: baiji_ai::ChatRequest) -> anyhow::Result<baiji_ai::ChatResponse> {
        unreachable!()
    }
    async fn chat_stream(
        &self,
        _: baiji_ai::ChatRequest,
    ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<baiji_ai::StreamChunk>>>
    {
        use futures::StreamExt as _;
        Ok(futures::stream::iter(vec![Ok(baiji_ai::StreamChunk::Done)]).boxed())
    }
    fn protocol(&self) -> baiji_ai::Protocol {
        baiji_ai::Protocol::OpenAIChat
    }
    fn model(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// 英文包 App（纯内存历史：不碰真实家目录）
fn test_app(dir: &std::path::Path) -> App {
    let runtime = Arc::new(
        baiji_agent::AgentRuntime::new(Arc::new(Echo)).with_tools(baiji_agent::ToolRegistry::new()),
    );
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

fn tx() -> UnboundedSender<UiEvent> {
    mpsc::unbounded_channel().0
}

async fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    app.handle_key(KeyEvent::new(code, mods), &tx()).await
}

async fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        press(app, K::Char(c), M::NONE).await;
    }
}

fn user_lines(app: &App) -> Vec<String> {
    app.lines
        .iter()
        .filter_map(|l| match l {
            ChatLine::User(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn test_composer_editing_keys() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());

    type_str(&mut app, "hello 世界").await;
    assert_eq!(app.input(), "hello 世界");
    // 光标编辑：←、插入、Home/End、Ctrl+A/E
    press(&mut app, K::Left, M::NONE).await;
    press(&mut app, K::Left, M::NONE).await;
    type_str(&mut app, "新").await;
    assert_eq!(app.input(), "hello 新世界");
    press(&mut app, K::Home, M::NONE).await;
    assert_eq!(app.input.cursor(), 0);
    press(&mut app, K::Delete, M::NONE).await;
    assert_eq!(app.input(), "ello 新世界");
    press(&mut app, K::Char('e'), M::CONTROL).await;
    assert_eq!(app.input.cursor(), app.input().len());
    // Ctrl+字母是快捷键，不插字
    press(&mut app, K::Char('x'), M::CONTROL).await;
    assert_eq!(app.input(), "ello 新世界");
    // 跳词 + Ctrl+W / Ctrl+U / Ctrl+K
    press(&mut app, K::Left, M::ALT).await;
    assert_eq!(app.input.cursor(), "ello ".len());
    press(&mut app, K::Char('k'), M::CONTROL).await;
    assert_eq!(app.input(), "ello ");
    press(&mut app, K::Char('w'), M::CONTROL).await;
    assert_eq!(app.input(), "");
    type_str(&mut app, "abc def").await;
    press(&mut app, K::Char('b'), M::ALT).await; // macOS Option+← = ESC b
    assert_eq!(app.input.cursor(), "abc ".len());
    press(&mut app, K::Char('u'), M::CONTROL).await;
    assert_eq!(app.input(), "def");
    press(&mut app, K::Backspace, M::ALT).await; // 光标在行首：无可删
    assert_eq!(app.input(), "def");
    press(&mut app, K::End, M::NONE).await;
    press(&mut app, K::Backspace, M::ALT).await;
    assert_eq!(app.input(), "");
}

#[tokio::test]
async fn test_newline_triggers_do_not_submit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());
    type_str(&mut app, "a").await;
    press(&mut app, K::Enter, M::SHIFT).await;
    type_str(&mut app, "b").await;
    press(&mut app, K::Enter, M::ALT).await;
    type_str(&mut app, "c").await;
    press(&mut app, K::Char('j'), M::CONTROL).await;
    type_str(&mut app, "d\\").await;
    press(&mut app, K::Enter, M::NONE).await; // 行尾反斜杠续行
    assert_eq!(app.input(), "a\nb\nc\nd\n");
    assert!(
        app.lines.is_empty() && !app.agent_running,
        "nothing submitted"
    );

    // 多行内容里 ↑↓ 在行间移动光标（不召回历史）
    type_str(&mut app, "e").await;
    press(&mut app, K::Up, M::NONE).await;
    assert_eq!(app.input.cursor(), "a\nb\nc\nd".len());
    press(&mut app, K::Up, M::NONE).await;
    press(&mut app, K::Up, M::NONE).await;
    press(&mut app, K::Up, M::NONE).await;
    assert_eq!(app.input.cursor(), 1, "top line, column kept");
    press(&mut app, K::Down, M::NONE).await;
    assert_eq!(app.input.cursor(), "a\nb".len());
    assert_eq!(app.input(), "a\nb\nc\nd\ne");

    // 普通 Enter 提交（多行原样作为用户消息）
    press(&mut app, K::Enter, M::NONE).await;
    assert_eq!(user_lines(&app), vec!["a\nb\nc\nd\ne".to_string()]);
    assert!(app.input().is_empty());
}

#[tokio::test]
async fn test_paste_keeps_newlines_placeholder_and_guard() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());
    app.agent_running = true; // 提交走 steering 路径：便于直接检查提交内容

    // 少于 5 行：换行保留（\r\n 归一），尾随换行去掉
    app.handle_paste("fn main() {\r\n    hi();\r\n}\r\n".to_string());
    assert_eq!(app.input(), "fn main() {\n    hi();\n}");
    // 粘贴守卫：紧随粘贴的 Enter 是粘贴内容的一部分 → 换行，不提交
    press(&mut app, K::Enter, M::NONE).await;
    assert_eq!(app.input(), "fn main() {\n    hi();\n}\n");
    assert!(app.steering.is_empty(), "guarded Enter must not submit");
    app.input.clear();

    // ≥5 行：占位符显示，提交时展开回原文
    let trace: String = (1..=23)
        .map(|i| format!("at frame {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    type_str(&mut app, "why? ").await;
    app.handle_paste(trace.clone());
    assert_eq!(app.input(), "why? [pasted 23 lines]");
    // 守卫过期（>150ms）后的 Enter 正常提交
    app.last_paste = Some(Instant::now() - Duration::from_secs(1));
    press(&mut app, K::Enter, M::NONE).await;
    let submitted = app.steering.drain();
    assert_eq!(submitted, vec![format!("why? {trace}")]);
    assert!(app.input().is_empty());
    // 历史里存展开后的全文
    assert_eq!(
        app.history.entries().last().unwrap(),
        &format!("why? {trace}")
    );

    // 守卫只吞一次：粘贴后第二个 Enter 照常提交
    app.handle_paste("x".to_string());
    press(&mut app, K::Enter, M::NONE).await;
    press(&mut app, K::Enter, M::NONE).await;
    assert_eq!(app.steering.drain(), vec!["x".to_string()]);
}

#[tokio::test]
async fn test_history_recall_and_draft_restore() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());
    let path = dir.path().join("prompt-history.jsonl");
    app.set_prompt_history(PromptHistory::load(path.clone()));
    app.agent_running = true; // steering 路径同样记入历史，且不发起后台 run

    type_str(&mut app, "first").await;
    press(&mut app, K::Enter, M::NONE).await;
    type_str(&mut app, "second").await;
    press(&mut app, K::Enter, M::SHIFT).await;
    type_str(&mut app, "line2").await;
    press(&mut app, K::Enter, M::NONE).await;
    // 落盘：每条一个 JSON 对象
    let raw = std::fs::read_to_string(&path).unwrap();
    assert_eq!(raw.lines().count(), 2);
    assert!(raw.contains("\"text\":\"second\\nline2\""), "{raw}");

    // 空输入 ↑：最新一条（光标在末尾）
    press(&mut app, K::Up, M::NONE).await;
    assert_eq!(app.input(), "second\nline2");
    // 多行历史条目：先在行间移动，到首行后再 ↑ 才换条目
    press(&mut app, K::Up, M::NONE).await;
    assert_eq!(app.input(), "second\nline2");
    assert!(app.input.cursor() < "second\n".len());
    press(&mut app, K::Up, M::NONE).await;
    assert_eq!(app.input(), "first");
    press(&mut app, K::Up, M::NONE).await;
    assert_eq!(app.input(), "first", "stays at the oldest");
    press(&mut app, K::Down, M::NONE).await;
    assert_eq!(app.input(), "second\nline2");
    press(&mut app, K::Down, M::NONE).await;
    assert_eq!(
        app.input(),
        "",
        "past the newest: back to the (empty) draft"
    );

    // 非历史草稿：普通 ↑ 不召回（光标到开头）；Alt+↑ 召回并暂存草稿，↓ 越过最新时恢复
    type_str(&mut app, "my draft").await;
    press(&mut app, K::Up, M::NONE).await;
    assert_eq!(app.input(), "my draft");
    assert_eq!(app.input.cursor(), 0);
    press(&mut app, K::Up, M::ALT).await;
    assert_eq!(app.input(), "second\nline2");
    press(&mut app, K::Down, M::NONE).await;
    assert_eq!(app.input(), "my draft", "draft restored");

    // 改动召回的条目后它就是普通草稿：↑ 不再换条目
    app.input.clear();
    press(&mut app, K::Up, M::NONE).await;
    press(&mut app, K::Up, M::NONE).await;
    press(&mut app, K::Up, M::NONE).await;
    assert_eq!(app.input(), "first");
    type_str(&mut app, "!").await;
    press(&mut app, K::Up, M::NONE).await;
    assert_eq!(app.input(), "first!");

    // 重启后从文件恢复
    let reloaded = PromptHistory::load(path);
    assert_eq!(reloaded.entries(), ["first", "second\nline2"]);
}

#[tokio::test]
async fn test_esc_clears_draft_before_exit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());
    type_str(&mut app, "half-written prompt").await;
    assert!(
        !press(&mut app, K::Esc, M::NONE).await,
        "first Esc only clears the draft"
    );
    assert!(app.input().is_empty());
    assert!(
        press(&mut app, K::Esc, M::NONE).await,
        "Esc on empty input exits"
    );

    // 运行中：Esc 取消 run，草稿保留
    let mut app = test_app(dir.path());
    app.agent_running = true;
    type_str(&mut app, "keep me").await;
    assert!(!press(&mut app, K::Esc, M::NONE).await);
    assert!(app.cancel.is_cancelled());
    assert_eq!(app.input(), "keep me");
}

#[tokio::test]
async fn test_scroll_follow_and_paging() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    for i in 0..100 {
        app.lines.push(ChatLine::assistant(format!("row {i}")));
    }
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    // 24 行 = 顶栏 1 + 聊天 19 + 输入框 3 + 状态栏 1 → 底部偏移 100 − 19
    assert_eq!(app.chat_viewport, 19);
    assert_eq!(app.scroll, 81, "sentinel clamped to the bottom");
    assert!(app.follow_bottom);

    // Shift+↑ 一行：离开底部；流式输出不再把视图拽回底部
    press(&mut app, K::Up, M::SHIFT).await;
    assert_eq!(app.scroll, 80);
    assert!(!app.follow_bottom);
    app.handle_agent_event(
        AgentEvent::TextDelta {
            text: "streaming…".to_string(),
        },
        &tx(),
    );
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert_eq!(
        app.scroll, 80,
        "reading position kept while a reply streams"
    );

    // PageUp/PageDown 一页（视口 − 2）
    press(&mut app, K::PageUp, M::NONE).await;
    assert_eq!(app.scroll, 63);
    press(&mut app, K::PageDown, M::NONE).await;
    assert_eq!(app.scroll, 80);
    // 翻过底部：渲染钳制 → 恢复跟随，新内容重新贴底
    press(&mut app, K::PageDown, M::NONE).await;
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert!(app.follow_bottom);
    app.handle_agent_event(
        AgentEvent::TextDelta {
            text: " more".to_string(),
        },
        &tx(),
    );
    assert_eq!(app.scroll, usize::MAX);

    // Ctrl+Home 顶部 / Ctrl+End 回底
    press(&mut app, K::Home, M::CONTROL).await;
    assert_eq!(app.scroll, 0);
    assert!(!app.follow_bottom);
    press(&mut app, K::End, M::CONTROL).await;
    assert_eq!(app.scroll, usize::MAX);
    assert!(app.follow_bottom);

    // 内容不足一屏时上翻无处可去：保持跟随
    let mut app = test_app(dir.path());
    app.lines.push(ChatLine::assistant("short"));
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    press(&mut app, K::Up, M::SHIFT).await;
    assert!(app.follow_bottom);
}

#[tokio::test]
async fn test_leftover_steering_is_resubmitted() {
    let dir = tempfile::tempdir().unwrap();
    let ends: Vec<AgentEvent> = vec![
        AgentEvent::RunCompleted {
            answer: "done".to_string(),
        },
        AgentEvent::RunFailed {
            error: "boom".to_string(),
        },
        AgentEvent::Interrupted,
    ];
    for end in ends {
        let mut app = test_app(dir.path());
        app.agent_running = true;
        // 最终回答流式输出期间输入的两条 steering（runtime 已不会再消费）
        app.steering.push("also update the docs");
        app.steering.push("and the tests");
        let old_queue = Arc::clone(&app.steering);
        app.on_agent_event(end, &tx());
        assert!(app.agent_running, "a follow-up run starts");
        assert_eq!(
            user_lines(&app),
            vec!["also update the docs\n\nand the tests".to_string()],
            "joined by blank lines, visible like a normal submit"
        );
        assert!(old_queue.is_empty());
        assert!(
            !Arc::ptr_eq(&old_queue, &app.steering),
            "the follow-up run has a fresh queue"
        );
    }

    // 队列为空：不额外发起 run
    let mut app = test_app(dir.path());
    app.agent_running = true;
    app.on_agent_event(
        AgentEvent::RunCompleted {
            answer: "ok".to_string(),
        },
        &tx(),
    );
    assert!(!app.agent_running);
    assert!(user_lines(&app).is_empty());

    // 新 run 紧接着开始（自动接力 / 计划批准等）时，遗留消息转入新队列
    let mut app = test_app(dir.path());
    app.steering.push("carry me");
    app.spawn_run("next".to_string(), tx());
    assert_eq!(app.steering.drain(), vec!["carry me".to_string()]);
}

#[test]
fn test_multiline_input_render_and_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(dir.path());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 24)).unwrap();
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

    // 三行内容：输入框增高到 3 行可见，续行 2 列缩进；光标落在末行末尾
    app.input.set("line1\nline2\nline3");
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    let first = rows.iter().position(|r| r.contains("> line1")).unwrap();
    assert!(
        rows[first + 1].contains("│  line2"),
        "{:?}",
        rows[first + 1]
    );
    assert!(rows[first + 2].contains("│  line3"));
    let pos = terminal.get_cursor_position().unwrap();
    assert_eq!((pos.x, pos.y), (1 + 2 + 5, first as u16 + 2));

    // 12 行：最多 8 行可见，视口跟随光标（末行可见、首行滚出）
    let long: Vec<String> = (1..=12).map(|i| format!("L{i:02}")).collect();
    app.input.set(long.join("\n"));
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    let visible: Vec<&String> = rows
        .iter()
        .filter(|r| r.contains("│  L") || r.contains("> L"))
        .collect();
    assert_eq!(visible.len(), 8, "{rows:?}");
    assert!(rows.iter().any(|r| r.contains("L12")));
    assert!(!rows.iter().any(|r| r.contains("L01")));
    // 光标移到开头：视口滚回顶部
    app.input.move_buffer_start();
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&terminal);
    assert!(rows.iter().any(|r| r.contains("> L01")));
    assert!(!rows.iter().any(|r| r.contains("L12")));

    // 长单行按显示宽度折行（CJK 2 列），光标随折行换到下一可视行
    app.input.set("中".repeat(30));
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let pos = terminal.get_cursor_position().unwrap();
    assert!(pos.x < 39, "cursor stays inside the box: {pos:?}");

    // 多行内容不出斜杠 ghost；占位符行渲染正常
    app.input.set("/mo\nmore");
    assert!(app.slash_ghost().is_none());
    app.input.clear();
    app.handle_paste(
        (1..=6)
            .map(|i| format!("p{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    terminal.clear().unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert!(
        screen(&terminal)
            .iter()
            .any(|r| r.contains("> [pasted 6 lines]"))
    );

    // 覆盖层打开时不放置输入框光标（交给覆盖层）
    app.wizard = Some(ConfigWizard::new("glm", None));
    assert!(app.overlay_active());
}
