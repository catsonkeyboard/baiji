//! app 辅助：字节格式化、粘贴净化、git 分支读取、斜杠命令注册表与补全。
//! 从 app.rs 拆出的自由函数与常量。

/// 字节数人性化展示
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

/// UI 事件：键盘输入 / 粘贴 / Agent 事件
/// （模型发现结果走专用通道，见事件循环 models_rx）

pub fn sanitize_paste(text: &str) -> String {
    text.trim()
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// 从 `dir`（含父目录，≤8 层）读取 git 分支名。
/// worktree 的 `.git` 是文件，`join(".git/HEAD")` 读取失败按"此层无仓库"继续向上。
pub(crate) fn read_git_branch(dir: &std::path::Path) -> Option<String> {
    let mut current = Some(dir);
    for _ in 0..8 {
        let dir = current?;
        current = dir.parent();
        let Ok(head) = std::fs::read_to_string(dir.join(".git/HEAD")) else {
            continue;
        };
        let head = head.trim();
        if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
            return Some(branch.to_string());
        }
        if head.starts_with("gitdir:") {
            return None; // worktree：不追 gitdir 文件
        }
        return head
            .split_whitespace()
            .next()
            .map(|sha| sha.chars().take(7).collect::<String>()); // detached HEAD
    }
    None
}

/// 斜杠命令注册表：(名称, 英文用法, 中文用法)。字典序——ghost 补全取首个前缀匹配
pub const SLASH_COMMANDS: &[(&str, &str, &str)] = &[
    (
        "btw",
        "Side-channel one-off question, not written to session history (planned)",
        "旁路提问：一次性问答，不写入当前会话历史（规划中）",
    ),
    (
        "compact",
        "Manually compact the session context (stub old results + summary)",
        "手动压缩当前会话上下文（旧结果 stub 化 + 摘要）",
    ),
    (
        "config",
        "Open the config wizard: vendor → endpoint → API key → model (hot-applied)",
        "打开配置向导：选厂商 → 端点 → API Key → 模型（热生效）",
    ),
    (
        "experts",
        "Experts mode: orchestrate expert subagents (architect/developer/reviewer)",
        "Experts 模式：主代理编排专家子代理（架构/实现/评审）",
    ),
    (
        "fork",
        "Fork the session: /fork inherits all history; /fork <n> rewinds n turns",
        "分叉会话：/fork 继承全部历史；/fork <n> 回到 n 轮之前重来（原会话保留）",
    ),
    (
        "goal",
        "Goal mode: /goal <objective> drives autonomously until the todos complete",
        "Goal 模式：/goal <目标> 自主推进直到 todo 完成",
    ),
    ("help", "Show command help", "显示命令帮助"),
    (
        "kill",
        "Stop a background job: /kill <id> (list via /tasks)",
        "停止后台任务：/kill <id>（列表见 /tasks）",
    ),
    (
        "model",
        "Switch model: /model <name>, or no argument opens the picker",
        "切换模型：/model <名称>，或不带参数打开模型选择器",
    ),
    (
        "new",
        "Start a new session (the current one is kept on disk)",
        "开新会话（当前会话完整保留在磁盘）",
    ),
    (
        "plan",
        "Plan mode: /plan toggles read-only planning; /plan <goal> starts planning; Enter approves",
        "计划模式：/plan 切换只读规划态；/plan <目标> 开始规划；计划给出后 Enter 批准执行",
    ),
    ("quit", "Exit baiji", "退出 baiji"),
    (
        "resume",
        "Resume another session (opens the session picker)",
        "恢复其它会话（打开会话选择器）",
    ),
    (
        "session",
        "Show current session info and stats",
        "显示当前会话信息与统计",
    ),
    (
        "spec",
        "Spec-driven: /spec <feature> drafts a spec; /spec approve seeds tasks and implements",
        "Spec 驱动：/spec <特性> 起草规格；/spec approve 生成任务并开始实施",
    ),
    (
        "status",
        "Show current vendor / endpoint / model / session",
        "查看当前 vendor / endpoint / model / session",
    ),
    (
        "subagents",
        "Manage subagent roles: inspect / hot-reload (agents/*.md)",
        "管理子代理角色：查看/热重载（agents/*.md 定义的角色）",
    ),
    ("tasks", "List background jobs", "查看后台任务列表"),
    (
        "thinking",
        "Set the thinking level: /thinking <minimal|low|medium|high|off>, hot-applied",
        "设置思考级别：/thinking <minimal|low|medium|high|off>，热生效",
    ),
    ("todos", "Show the current todo list", "显示当前任务清单"),
    (
        "usage",
        "Usage stats: context / compression savings / tool calls",
        "显示用量统计：上下文 / 压缩节省 / 工具调用",
    ),
];


/// 按语言取 (名称, 用法) 列表
pub fn slash_commands(lang: crate::i18n::Lang) -> Vec<(&'static str, &'static str)> {
    SLASH_COMMANDS
        .iter()
        .map(|(n, en, zh)| {
            (
                *n,
                if lang == crate::i18n::Lang::Zh {
                    *zh
                } else {
                    *en
                },
            )
        })
        .collect()
}

/// 输入以 `/` 开头时的命令提示（按前缀过滤，按语言取用法）。
/// 返回 None = 非斜杠输入；Some(vec) = 匹配的命令（可能为空 = 无匹配）。
pub fn slash_hints(
    input: &str,
    lang: crate::i18n::Lang,
) -> Option<Vec<(&'static str, &'static str)>> {
    let rest = input.strip_prefix('/')?.trim_start();
    let commands = slash_commands(lang);
    // 空格后进入参数区：不再列命令，但保留精确命令的用法提示
    if rest.contains(' ') {
        let name = rest.split(' ').next().unwrap_or("");
        return Some(
            commands
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(n, u)| vec![(*n, *u)])
                .unwrap_or_default(),
        );
    }
    let lower = rest.to_ascii_lowercase();
    Some(
        commands
            .iter()
            .filter(|(n, _)| n.starts_with(lower.as_str()))
            .copied()
            .collect(),
    )
}

/// 斜杠命令解析：("/cmd", "rest args") 或 None
pub fn split_slash(input: &str) -> Option<(&str, &str)> {
    let rest = input.strip_prefix('/')?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let (cmd, a) = match rest.split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (rest, ""),
    };
    Some((cmd, a))
}

/// 输入框灰色补全（fish-style ghost）：命令输入态（`/` 开头、未进参数区）时
/// 取首个前缀匹配。Tab 键接受补全
pub fn ghost_completion(
    input: &str,
    lang: crate::i18n::Lang,
) -> Option<(&'static str, &'static str)> {
    let rest = input.strip_prefix('/')?;
    if rest.is_empty() || rest.contains(' ') {
        return None;
    }
    slash_hints(input, lang)?.first().copied()
}
