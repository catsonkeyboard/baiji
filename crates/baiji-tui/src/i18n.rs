//! TUI 文案双语（en 默认 / zh）：`ui.language` 配置选择。
//!
//! Strings 持有全部用户可见文案（模板用位置 `{}`，调用点 `format!` 填参）；
//! 斜杠命令用法在 [`crate::app::SLASH_COMMANDS`] 三元组（name, en, zh）里。

use serde::{Deserialize, Serialize};

/// 界面语言
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    En,
    Zh,
}

impl Lang {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "en" | "english" => Some(Self::En),
            "zh" | "cn" | "chinese" | "中文" => Some(Self::Zh),
            _ => None,
        }
    }
}

/// 全部界面文案（按区域分组；字段都是静态模板）。
/// `lang` 冗余存放选择的语言（调用点取用法时免再传参）
#[derive(Debug, Clone, Copy)]
pub struct Strings {
    pub lang: Lang,
    // ---- 输入框 ----
    pub input_placeholder: &'static str,
    pub input_running_title: &'static str,
    pub input_plan_title: &'static str,
    pub input_plan_await_title: &'static str,
    // ---- 状态栏 ----
    pub status_ready: &'static str,
    pub status_plan_readonly: &'static str,
    pub status_plan_await: &'static str,
    /// "saved {bytes} (~{tokens} tok)"
    pub status_saved_tpl: &'static str,
    /// "auto {used}/{max}"
    pub status_auto_tpl: &'static str,
    /// "session {id}"
    pub status_session_tpl: &'static str,
    // ---- 欢迎屏 ----
    pub welcome_tagline: &'static str,
    pub welcome_hint1: &'static str,
    pub welcome_hint2: &'static str,
    // ---- todo 悬浮面板 ----
    /// "… +{extra} more"
    pub todo_overflow_tpl: &'static str,
    // ---- HITL 确认弹窗 ----
    pub confirm_title: &'static str,
    /// "⚠ Run tool {name}"
    pub confirm_warn_tpl: &'static str,
    /// "… {n} lines hidden (window too small); press n if unsure"
    pub confirm_hidden_tpl: &'static str,
    pub confirm_keys: &'static str,
    // ---- 会话选择器 ----
    pub picker_title: &'static str,
    pub picker_no_title: &'static str,
    // ---- 子代理面板 ----
    pub subagents_title: &'static str,
    pub subagents_inherit: &'static str,
    pub subagents_default: &'static str,
    pub subagents_all: &'static str,
    /// "Role dirs: {dirs} (edit then press r to hot-reload)"
    pub subagents_dirs_tpl: &'static str,
    pub subagents_empty1: &'static str,
    pub subagents_empty2: &'static str,
    /// "Reloaded {count} subagent role(s) (effective on the next task call)"
    pub subagents_reloaded_tpl: &'static str,
    pub subagents_busy: &'static str,
    /// "    prompt: {p}…"
    pub subagents_prompt_preview_tpl: &'static str,
    // ---- 运行事件 ----
    pub run_cancelled: &'static str,
    /// "✗ error: {error}"
    pub run_error_tpl: &'static str,
    pub run_cancel_requested: &'static str,
    /// "（steering）{text}" — 前缀模板
    pub steering_prefix_tpl: &'static str,
    // ---- 自动接力 ----
    /// "⏹ auto-continue stopped: turn limit {max} reached (todos still open; continue manually)"
    pub auto_stopped_tpl: &'static str,
    /// "⏩ auto-continue ({used}/{max}): todos open, continuing (Esc stops)"
    pub auto_continue_tpl: &'static str,
    // ---- 计划模式 ----
    pub plan_on: &'static str,
    pub plan_off: &'static str,
    pub plan_awaiting: &'static str,
    /// "⏩ executing plan: {prompt}"
    pub plan_execute_tpl: &'static str,
    pub plan_confirm_cancelled: &'static str,
    /// "（steering·plan）{goal}"
    pub plan_steering_tpl: &'static str,
    pub plan_on_label: &'static str,
    pub plan_off_label: &'static str,
    // ---- 命令输出 ----
    /// "Commands (ghost hint while typing /, Tab accepts): {list}"
    pub cmd_help_tpl: &'static str,
    /// "prompt templates: {list}"
    pub cmd_templates_tpl: &'static str,
    /// "unknown command /{cmd} (available: {list} · Tab completes)"
    pub cmd_unknown_tpl: &'static str,
    pub cmd_blocked_config: &'static str,
    pub cmd_blocked_running: &'static str,
    /// "usage: /fork or /fork <turns-to-rewind>"
    pub cmd_fork_usage: &'static str,
    /// "branched → {id} (rewound {turns} turns; original kept, switch back via the picker)"
    pub cmd_fork_done_tpl: &'static str,
    /// "✗ fork failed: {err}"
    pub cmd_fork_fail_tpl: &'static str,
    /// "switched session {id} · {title}"
    pub cmd_session_switched_tpl: &'static str,
    /// "✗ switch failed: {err}"
    pub cmd_session_fail_tpl: &'static str,
    /// "branched → {id} (history inherited)"
    pub cmd_branch_done_tpl: &'static str,
    /// "new session {id} started (original kept; Ctrl+O to switch back)"
    pub cmd_new_done_tpl: &'static str,
    /// "✗ failed to start a new session: {err}"
    pub cmd_new_fail_tpl: &'static str,
    /// "compacted: {chars}-char summary · {stubbed} old tool result(s) stubbed (recover via expand)"
    pub cmd_compact_done_tpl: &'static str,
    /// "compacted: {stubbed} old tool result(s) stubbed (no summary needed)"
    pub cmd_compact_stubbed_tpl: &'static str,
    pub cmd_compact_nothing: &'static str,
    /// "session busy (running), try again later"
    pub cmd_busy: &'static str,
    /// "session {id}\ncreated: {created}\ntitle: {title}\nproject: {project}\nfrom: {parent}\nmessages: {msgs} · todos: {todos}"
    pub cmd_session_info_tpl: &'static str,
    pub cmd_todos_empty: &'static str,
    /// "todos:\n{rows}"
    pub cmd_todos_tpl: &'static str,
    /// "context: {ctx} tokens · tool calls: {calls}\nsaved by compression: {saved} · auto-continue: {used}/{max}\nhistory: {messages} msgs · model: {model}"
    pub cmd_usage_tpl: &'static str,
    pub cmd_no_data: &'static str,
    pub cmd_tasks_disabled: &'static str,
    pub cmd_tasks_empty: &'static str,
    /// "background jobs:\n{rows}"
    pub cmd_tasks_tpl: &'static str,
    /// "stopped job #{id}"
    pub cmd_kill_done_tpl: &'static str,
    /// "no running job #{id} (see /tasks)"
    pub cmd_kill_missing_tpl: &'static str,
    /// "usage: /kill <id> (see /tasks)"
    pub cmd_kill_usage: &'static str,
    /// "current thinking level: {level}\nusage: /thinking <minimal|low|medium|high> or /thinking off\nAnthropic maps to thinking.budget_tokens; OpenAI to reasoning_effort / reasoning.effort"
    pub cmd_thinking_status_tpl: &'static str,
    /// "thinking set to {level} (effective on the next request, saved to config)"
    pub cmd_thinking_set_tpl: &'static str,
    pub cmd_thinking_usage: &'static str,
    /// "✗ failed to save config: {err}"
    pub cmd_config_save_fail_tpl: &'static str,
    /// "✗ switch failed (config saved; restart to apply): {err}"
    pub cmd_swap_fail_tpl: &'static str,
    /// "✓ config applied: {vendor} · endpoint={endpoint} · model={model} (written to config)"
    pub cmd_applied_tpl: &'static str,
    /// "usage: /{name} <task> (delegate to the external {name} agent CLI)"
    pub cmd_external_usage_tpl: &'static str,
    pub cmd_btw_stub: &'static str,
    // ---- 外部 agent ----
    /// "Delegate a task to the external {name} agent: /{name} <task>"
    pub external_usage_tpl: &'static str,
    pub external_interrupted: &'static str,
    // ---- 运行中不可用 ----
    pub busy_picker: &'static str,
    pub busy_subagents: &'static str,
    // ---- 向导 ----
    pub wizard_vendor_title: &'static str,
    /// "Choose an endpoint · {vendor} (Enter=select Esc=back)"
    pub wizard_endpoint_title_tpl: &'static str,
    /// "Enter the API key ({hint}; empty keeps the current one) · confirm in the input box"
    pub wizard_key_title_tpl: &'static str,
    pub wizard_model_title: &'static str,
    pub wizard_model_manual_title: &'static str,
    /// "API key (recommended env var {env}; paste & Enter, empty keeps the current)"
    pub wizard_key_prompt_tpl: &'static str,
    pub wizard_manual_entry: &'static str,
    /// "model name (e.g. glm-4.7)"
    pub wizard_manual_prompt: &'static str,
    /// "→ pick the last entry “{manual}” to type it directly (Coding Plan endpoints often expose no model list)"
    pub wizard_fallback_hint_tpl: &'static str,
    /// "api — default endpoint (pay-as-you-go) · {note}"
    pub wizard_api_endpoint_tpl: &'static str,
    /// "failed to fetch models: {err}"
    pub wizard_models_error_tpl: &'static str,
}

/// 位置 {} 模板填充（format! 宏只接受字面量，动态模板走这里）
pub fn fill(tpl: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = tpl.to_string();
    for a in args {
        if let Some(pos) = out.find("{}") {
            out.replace_range(pos..pos + 2, &a.to_string());
        }
    }
    out
}

impl Strings {
    pub fn for_lang(lang: Lang) -> Self {
        match lang {
            Lang::En => Self::en(),
            Lang::Zh => Self::zh(),
        }
    }

    pub fn en() -> Self {
        Self {
            lang: Lang::En,
            input_placeholder: "Type a message; / for commands…",
            input_running_title: " Esc to cancel · typing steers ",
            input_plan_title: " ⏸ Plan mode (read-only) · /plan off exits ",
            input_plan_await_title: " ⏸ Plan ready: Enter executes · typing refines ",
            status_ready: "ready",
            status_plan_readonly: "plan·RO",
            status_plan_await: "plan·go?",
            status_saved_tpl: "saved {} (~{} tok)",
            status_auto_tpl: "auto {}/{}",
            status_session_tpl: "session {}",
            welcome_tagline: "terminal AI coding agent",
            welcome_hint1: "Enter send · typing during a run steers · Esc cancel · Ctrl+C quit",
            welcome_hint2: "Ctrl+O sessions · /help · /config · PgUp/PgDn scroll",
            todo_overflow_tpl: "… +{extra} more",
            confirm_title: " Confirm execution? ",
            confirm_warn_tpl: "⚠ Run tool {}",
            confirm_hidden_tpl: "… {} more lines hidden (window too small); press n if unsure",
            confirm_keys: "y=allow  a=allow all this run  n=deny",
            picker_title: " Sessions (Enter=switch b=fork Esc=close) ",
            picker_no_title: "(untitled)",
            subagents_title: " Subagents (↑↓ select · r reload · Esc close) ",
            subagents_inherit: "inherit",
            subagents_default: "default",
            subagents_all: "all",
            subagents_dirs_tpl: "Role dirs: {} (edit, then press r to hot-reload)",
            subagents_empty1: "(no roles yet) create <name>.md in the dirs below; frontmatter:",
            subagents_empty2: "name/description/tools/model/thinking/max_turns; body = system prompt",
            subagents_reloaded_tpl: "Reloaded {} subagent role(s) (effective on the next task call)",
            subagents_busy: "harness busy, try later (cannot reload while running)",
            subagents_prompt_preview_tpl: "    prompt: {}…",
            run_cancelled: "cancelled",
            run_error_tpl: "✗ error: {}",
            run_cancel_requested: "cancelling…",
            steering_prefix_tpl: "(steering) {}",
            auto_stopped_tpl: "⏹ auto-continue stopped: turn limit {} reached (todos open; continue manually)",
            auto_continue_tpl: "⏩ auto-continue ({}/{}): todos open, continuing (Esc stops)",
            plan_on: "⏸ plan mode ON (read-only) — write/edit/bash disabled; /plan off exits",
            plan_off: "▶ plan mode OFF, full tools restored",
            plan_awaiting: "⏸ plan ready — empty Enter exits plan mode and executes · type to refine · /plan off to just exit",
            plan_execute_tpl: "⏩ executing the plan: {}",
            plan_confirm_cancelled: "approval cancelled (still in plan mode)",
            plan_steering_tpl: "(steering·plan) {}",
            plan_on_label: "on (read-only)",
            plan_off_label: "off",
            cmd_help_tpl: "Commands (ghost hint after /, Tab accepts): {}",
            cmd_templates_tpl: "prompt templates: {}",
            cmd_unknown_tpl: "unknown command /{} (available: {} · Tab completes)",
            cmd_blocked_config: "cannot change config while running — wait or press Esc",
            cmd_blocked_running: "cannot do this while running — press Esc first",
            cmd_fork_usage: "usage: /fork or /fork <turns-to-rewind>",
            cmd_fork_done_tpl: "branched → {} (rewound {} turns; original kept, switch back via the picker)",
            cmd_fork_fail_tpl: "✗ fork failed: {}",
            cmd_session_switched_tpl: "switched session {} · {}",
            cmd_session_fail_tpl: "✗ switch failed: {}",
            cmd_branch_done_tpl: "branched → {} (history inherited)",
            cmd_new_done_tpl: "new session {} started (original kept; Ctrl+O switches back)",
            cmd_new_fail_tpl: "✗ failed to start a new session: {}",
            cmd_compact_done_tpl: "compacted: {}-char summary · {} old tool result(s) stubbed (recover via expand)",
            cmd_compact_stubbed_tpl: "compacted: {} old tool result(s) stubbed (no summary needed)",
            cmd_compact_nothing: "history too short, nothing to compact",
            cmd_busy: "session busy (running), try again later",
            cmd_session_info_tpl: "session {}\ncreated: {}\ntitle: {}\nproject: {}\nforked from: {}\nmessages: {} · todos: {}",
            cmd_todos_empty: "no todo list (the model can create one with the todo tool)",
            cmd_todos_tpl: "todos:\n{}",
            cmd_usage_tpl: "context: {} tokens · tool calls: {}\nsaved by compression: {} · auto-continue: {}/{}\nhistory: {} msgs · model: {}",
            cmd_no_data: "no data yet",
            cmd_tasks_disabled: "background jobs not enabled (no JobRegistry wired)",
            cmd_tasks_empty: "no background jobs",
            cmd_tasks_tpl: "background jobs:\n{}",
            cmd_kill_done_tpl: "stopped job #{}",
            cmd_kill_missing_tpl: "no running job #{} (see /tasks)",
            cmd_kill_usage: "usage: /kill <id> (see /tasks)",
            cmd_thinking_status_tpl: "current thinking level: {}\nusage: /thinking <minimal|low|medium|high> or /thinking off\nAnthropic maps to thinking.budget_tokens; OpenAI to reasoning_effort / reasoning.effort",
            cmd_thinking_set_tpl: "thinking set to {} (effective on the next request, saved to config)",
            cmd_thinking_usage: "usage: /thinking <minimal|low|medium|high|off>",
            cmd_config_save_fail_tpl: "✗ failed to save config: {}",
            cmd_swap_fail_tpl: "✗ switch failed (config saved; restart to apply): {}",
            cmd_applied_tpl: "✓ config applied: {} · endpoint={} · model={} (written to config)",
            cmd_external_usage_tpl: "usage: /{} <task> (delegates to the external {} agent CLI)",
            cmd_btw_stub: "side-channel questions are not implemented yet (planned: one-off Q&A outside session history)",
            external_usage_tpl: "Delegate a task to the external {} agent: /{} <task>",
            external_interrupted: "[cancelled] external agent interrupted by the user",
            busy_picker: "cannot open the session picker while running — press Esc first",
            busy_subagents: "cannot open the subagents panel while running — press Esc first",
            wizard_vendor_title: " Choose a vendor (Enter=select Esc=cancel) ",
            wizard_endpoint_title_tpl: " Choose an endpoint · {} (Enter=select Esc=back) ",
            wizard_key_title_tpl: " Enter the API key ({} hint; empty keeps the current) · confirm in the input box ",
            wizard_model_title: " Choose a model (Enter=use last=manual input Esc=back) ",
            wizard_model_manual_title: " Type a model name (confirm with Enter in the input box) ",
            wizard_key_prompt_tpl: "API key (recommended env var {}; paste and Enter, empty keeps the current)",
            wizard_manual_entry: "✏ type a model name…",
            wizard_manual_prompt: "model name (e.g. glm-4.7)",
            wizard_fallback_hint_tpl: "→ pick the last entry “{}” to type it directly (Coding Plan endpoints often expose no model list)",
            wizard_api_endpoint_tpl: "api — default endpoint (pay-as-you-go) · {}",
            wizard_models_error_tpl: "failed to fetch models: {}",
        }
    }

    pub fn zh() -> Self {
        Self {
            lang: Lang::Zh,
            input_placeholder: "输入消息，/ 开头为命令…",
            input_running_title: " Esc 取消 · 输入即 steering ",
            input_plan_title: " ⏸ 计划模式（只读） · /plan off 退出 ",
            input_plan_await_title: " ⏸ 计划待批准：空回车=开始执行 · 输入=继续改计划 ",
            status_ready: "就绪",
            status_plan_readonly: "计划·只读",
            status_plan_await: "计划·待执行",
            status_saved_tpl: "省 {} (~{} tok)",
            status_auto_tpl: "自动 {}/{}",
            status_session_tpl: "session {}",
            welcome_tagline: "terminal AI coding agent · 终端 AI 编程助手",
            welcome_hint1: "Enter 发送 · 运行中输入为 steering · Esc 取消 · Ctrl+C 退出",
            welcome_hint2: "Ctrl+O 会话 · /help 帮助 · /config 配置 · PgUp/PgDn 滚动",
            todo_overflow_tpl: "… 还有 {extra} 项",
            confirm_title: " 确认执行? ",
            confirm_warn_tpl: "⚠ 执行工具 {}",
            confirm_hidden_tpl: "… 还有 {} 行未显示（窗口太小）；不确定请按 n 拒绝",
            confirm_keys: "y=允许  a=本次全部允许  n=拒绝",
            picker_title: " 会话 (Enter=切换 b=分叉 Esc=关闭) ",
            picker_no_title: "(无标题)",
            subagents_title: " Subagents (↑↓ 选择 · r 热重载 · Esc 关闭) ",
            subagents_inherit: "继承",
            subagents_default: "默认",
            subagents_all: "全部",
            subagents_dirs_tpl: "角色目录: {}（编辑后按 r 热重载）",
            subagents_empty1: "（尚无角色）在下列目录创建 <name>.md：frontmatter 声明",
            subagents_empty2: "name/description/tools/model/thinking/max_turns，正文为该角色的系统提示",
            subagents_reloaded_tpl: "已重载子代理角色：{} 个（下一次 task 调用生效）",
            subagents_busy: "harness 忙，稍后再试（运行中不可重载）",
            subagents_prompt_preview_tpl: "    提示词: {}…",
            run_cancelled: "已取消",
            run_error_tpl: "✗ 出错: {}",
            run_cancel_requested: "已请求取消…",
            steering_prefix_tpl: "（steering）{}",
            auto_stopped_tpl: "⏹ 自动接力停止：达到轮次上限 {}（todo 仍有未完成项，可手动继续）",
            auto_continue_tpl: "⏩ 自动接力（轮次 {}/{}）：todo 未完成，继续任务（Esc 可停）",
            plan_on: "⏸ 计划模式已开启（只读）— 写/编辑/bash 被禁用；/plan off 退出",
            plan_off: "▶ 计划模式已关闭，恢复完整工具",
            plan_awaiting: "⏸ 计划已给出 — 空回车退出计划模式并开始执行 · 或直接输入继续修改计划 · /plan off 仅退出",
            plan_execute_tpl: "⏩ 执行计划：{}",
            plan_confirm_cancelled: "已取消执行确认（仍处于计划模式）",
            plan_steering_tpl: "（steering·规划）{}",
            plan_on_label: "开启（只读）",
            plan_off_label: "关闭",
            cmd_help_tpl: "命令（输入 / 后输入框灰色提示补全，Tab 接受）：{}",
            cmd_templates_tpl: "prompt 模板：{}",
            cmd_unknown_tpl: "未知命令 /{}（可用: {} · Tab 可补全）",
            cmd_blocked_config: "运行中不可修改配置，请先等待或 Esc 取消",
            cmd_blocked_running: "运行中无法操作，先按 Esc 取消",
            cmd_fork_usage: "用法：/fork 或 /fork <回退轮数>",
            cmd_fork_done_tpl: "已分叉 → {}（回退 {} 轮；原会话保留，可用会话选择器切回）",
            cmd_fork_fail_tpl: "✗ 分叉失败: {}",
            cmd_session_switched_tpl: "已切换会话 {} · {}",
            cmd_session_fail_tpl: "✗ 切换会话失败: {}",
            cmd_branch_done_tpl: "已从当前会话分叉 → {}（历史已继承）",
            cmd_new_done_tpl: "已开新会话 {}（原会话保留，Ctrl+O 可切回）",
            cmd_new_fail_tpl: "✗ 开新会话失败: {}",
            cmd_compact_done_tpl: "压缩完成：摘要 {} 字符 · stub 化 {} 条旧工具结果（可用 expand 取回）",
            cmd_compact_stubbed_tpl: "压缩完成：stub 化 {} 条旧工具结果（无需摘要）",
            cmd_compact_nothing: "历史太短，没有可压缩的内容",
            cmd_busy: "会话忙（运行中），稍后再试",
            cmd_session_info_tpl: "session {}\n创建: {}\n标题: {}\n项目: {}\n分叉自: {}\n消息: {} 条 · todo: {} 条",
            cmd_todos_empty: "当前没有任务清单（模型可用 todo 工具创建）",
            cmd_todos_tpl: "任务清单：\n{}",
            cmd_usage_tpl: "上下文: {} tokens · 工具调用: {} 次\n压缩节省: {} · 自动接力: {}/{}\n历史消息: {} 条 · 模型: {}",
            cmd_no_data: "尚无数据",
            cmd_tasks_disabled: "后台任务未启用（未接入 JobRegistry）",
            cmd_tasks_empty: "没有后台任务",
            cmd_tasks_tpl: "后台任务：\n{}",
            cmd_kill_done_tpl: "已停止后台任务 #{}",
            cmd_kill_missing_tpl: "未找到运行中的任务 #{}（/tasks 查看列表）",
            cmd_kill_usage: "用法：/kill <id>（id 见 /tasks）",
            cmd_thinking_status_tpl: "当前思考级别: {}\n用法：/thinking <minimal|low|medium|high>，或 /thinking off 关闭\nAnthropic 端点映射 thinking.budget_tokens，OpenAI 系映射 reasoning_effort / reasoning.effort",
            cmd_thinking_set_tpl: "思考级别已设为 {}（下一次请求生效，已写入配置）",
            cmd_thinking_usage: "用法：/thinking <minimal|low|medium|high|off>",
            cmd_config_save_fail_tpl: "✗ 配置保存失败: {}",
            cmd_swap_fail_tpl: "✗ 切换失败（配置已保存，重启后生效）: {}",
            cmd_applied_tpl: "✓ 配置已生效：{} · endpoint={} · model={}（已写入配置文件）",
            cmd_external_usage_tpl: "用法：/{} <任务描述>（委托给外部 {} agent CLI）",
            cmd_btw_stub: "旁路提问尚未实现（规划中：不写入当前会话历史的一次性问答）",
            external_usage_tpl: "委托任务给外部 {} agent：/{} <任务描述>",
            external_interrupted: "[已取消] 外部 agent 被用户中断",
            busy_picker: "运行中无法打开会话选择器，先按 Esc 取消",
            busy_subagents: "运行中无法打开子代理面板，先按 Esc 取消",
            wizard_vendor_title: " 选择厂商 (Enter=选择 Esc=取消) ",
            wizard_endpoint_title_tpl: " 选择端点 · {} (Enter=选择 Esc=返回) ",
            wizard_key_title_tpl: " 输入 API Key（{} 提示；留空沿用现有）· 输入框回车确认 ",
            wizard_model_title: " 选择模型 (Enter=使用 末项=手动输入 Esc=返回) ",
            wizard_model_manual_title: " 手动输入模型名（输入框回车确认） ",
            wizard_key_prompt_tpl: "API Key（推荐环境变量 {}；直接粘贴回车，留空沿用现有）",
            wizard_manual_entry: "✏ 手动输入模型名…",
            wizard_manual_prompt: "模型名称（如 glm-4.7）",
            wizard_fallback_hint_tpl: "→ 请选末项「{}」直接指定（Coding Plan 端点常无列表接口）",
            wizard_api_endpoint_tpl: "api — 默认端点（按量付费）· {}",
            wizard_models_error_tpl: "模型列表获取失败：{}",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lang_parse_and_default() {
        assert_eq!(
            Lang::default(),
            Lang::En,
            "English is the default UI language"
        );
        assert_eq!(Lang::parse("en"), Some(Lang::En));
        assert_eq!(Lang::parse("ZH"), Some(Lang::Zh));
        assert_eq!(Lang::parse("中文"), Some(Lang::Zh));
        assert_eq!(Lang::parse("xx"), None);
    }

    #[test]
    fn test_strings_cover_both_languages() {
        // 两个语言包字段一一对应（同结构体天然保证）；抽查关键差异
        let en = Strings::en();
        let zh = Strings::zh();
        assert_eq!(en.status_ready, "ready");
        assert_eq!(zh.status_ready, "就绪");
        assert_ne!(en.input_placeholder, zh.input_placeholder);
        assert!(en.cmd_help_tpl.contains("Commands"));
        assert!(zh.cmd_help_tpl.contains("命令"));
    }
}
