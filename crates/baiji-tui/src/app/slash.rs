//! 斜杠命令分发：App::handle_slash 巨型 match（从 mod.rs 拆出）。
//! 19 个静态命令 + 外部 agent 动态命令。

use super::helpers::{format_bytes, slash_commands};
use super::pickers::SubagentsPanel;
use super::wizard::{ConfigWizard, WizardStep};
use super::{App, ChatLine, UiEvent};
use crate::settings;
use tokio::sync::mpsc::UnboundedSender;

impl App {
    pub(crate) async fn handle_slash(
        &mut self,
        cmd: &str,
        args: &str,
        ui_tx: &UnboundedSender<UiEvent>,
    ) -> bool {
        match cmd {
            "quit" => return true,
            "help" => {
                let list: Vec<String> = slash_commands(self.strings.lang)
                    .iter()
                    .map(|(name, _)| format!("/{name}"))
                    .collect();
                self.lines.push(ChatLine::System(crate::i18n::fill(
                    &self.strings.cmd_help_tpl,
                    &[&list.join(" · ")],
                )));
                if !self.templates.is_empty() {
                    let list: Vec<String> = self
                        .templates
                        .iter()
                        .map(|(name, desc)| {
                            if desc.is_empty() {
                                format!("/{name}")
                            } else {
                                format!("/{name}（{desc}）")
                            }
                        })
                        .collect();
                    self.lines.push(ChatLine::System(crate::i18n::fill(
                        &self.strings.cmd_templates_tpl,
                        &[&list.join(" · ")],
                    )));
                }
            }
            "fork" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        self.strings.cmd_blocked_running.to_string(),
                    ));
                } else {
                    match args.trim() {
                        "" => self.fork_session(0).await,
                        n => match n.parse::<usize>() {
                            Ok(turns) => self.fork_session(turns).await,
                            Err(_) => self
                                .lines
                                .push(ChatLine::System(self.strings.cmd_fork_usage.to_string())),
                        },
                    }
                }
            }
            "status" => {
                let s = &self.settings;
                let workflow = {
                    let w = self
                        .harness
                        .try_lock()
                        .map(|h| h.workflow_summary())
                        .unwrap_or_default();
                    if w.is_empty() {
                        String::new()
                    } else {
                        format!("\nworkflow: {w}")
                    }
                };
                let external = if s.external_agents.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\nexternal agents: {}",
                        s.external_agents
                            .iter()
                            .map(|a| format!("/{}", a.name))
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                };
                self.lines.push(ChatLine::System(format!(
                    "vendor: {} · endpoint: {} · model: {} · thinking: {} · session: {}\nplan mode: {} · {}{workflow}{external}",
                    s.vendor,
                    s.endpoint.as_deref().unwrap_or("api"),
                    s.model.as_deref().unwrap_or("auto"),
                    s.thinking.map(|l| l.effort()).unwrap_or("off"),
                    self.session_id,
                    if self.plan_mode { self.strings.plan_on_label } else { self.strings.plan_off_label },
                    self.settings_summary
                )));
            }
            "config" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        self.strings.cmd_blocked_config.to_string(),
                    ));
                    return false;
                }
                let s = &self.settings;
                self.wizard = Some(ConfigWizard::new(&s.vendor, s.endpoint.as_deref()));
            }
            "model" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        self.strings.cmd_blocked_config.to_string(),
                    ));
                    return false;
                }
                if args.is_empty() {
                    // 直接进入模型选择（沿用当前厂商与 Key）
                    let s = &self.settings;
                    let mut wizard = ConfigWizard::new(&s.vendor, s.endpoint.as_deref());
                    wizard.step = WizardStep::Model;
                    wizard.fetching = true;
                    self.wizard = Some(wizard);
                    self.spawn_model_discovery();
                } else {
                    let mut new_settings = self.settings.clone();
                    new_settings.model = Some(args.to_string());
                    // 复用向导生效路径
                    self.wizard = Some(ConfigWizard {
                        step: WizardStep::ModelManual,
                        vendor: new_settings.vendor.clone(),
                        endpoint: new_settings.endpoint.clone(),
                        api_key: String::new(),
                        model: new_settings.model.clone(),
                        models: Vec::new(),
                        models_error: None,
                        fetching: false,
                        selected: 0,
                    });
                    // 直接应用
                    self.apply_wizard().await;
                }
            }
            "resume" => {
                if self.agent_running {
                    self.lines
                        .push(ChatLine::System(self.strings.busy_picker.to_string()));
                } else {
                    self.open_picker().await;
                }
            }
            "new" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        self.strings.cmd_blocked_running.to_string(),
                    ));
                } else {
                    let result = self.harness.lock().await.start_new_session();
                    match result {
                        Ok(id) => {
                            self.session_id = id.clone();
                            self.rebuild_lines(&[]);
                            // 会话级台账归零
                            self.context_tokens = 0;
                            self.bytes_saved = 0;
                            self.tokens_saved = 0;
                            self.tools_total = 0;
                            self.lines.push(ChatLine::System(crate::i18n::fill(
                                &self.strings.cmd_new_done_tpl,
                                &[&id],
                            )));
                        }
                        Err(e) => {
                            self.lines.push(ChatLine::System(crate::i18n::fill(
                                &self.strings.cmd_new_fail_tpl,
                                &[&e],
                            )));
                        }
                    }
                }
            }
            "session" => {
                let info = self.harness.try_lock().ok().map(|h| {
                    let meta = &h.session().meta;
                    (
                        meta.id.clone(),
                        meta.parent_id.clone(),
                        meta.created_at.clone(),
                        meta.title.clone(),
                        meta.project.clone(),
                        h.session().messages.len(),
                        h.todos_snapshot().len(),
                    )
                });
                match info {
                    Some((id, parent, created, title, project, msgs, todos)) => {
                        self.lines.push(ChatLine::System(crate::i18n::fill(
                            &self.strings.cmd_session_info_tpl,
                            &[
                                &id,
                                &created,
                                &title.unwrap_or_else(|| "-".into()),
                                &project.unwrap_or_else(|| "-".into()),
                                &parent.unwrap_or_else(|| "-".into()),
                                &msgs.to_string(),
                                &todos.to_string(),
                            ],
                        )));
                    }
                    None => self
                        .lines
                        .push(ChatLine::System(self.strings.cmd_busy.to_string())),
                }
            }
            "compact" => {
                if self.agent_running {
                    self.lines.push(ChatLine::System(
                        self.strings.cmd_blocked_running.to_string(),
                    ));
                } else {
                    let mut harness = self.harness.lock().await;
                    let (stubbed, summary) = harness.compact_now().await;
                    let messages = harness.session().messages.clone();
                    drop(harness);
                    self.rebuild_lines(&messages);
                    match summary {
                        Some(s) => {
                            let chars = s.chars().count().to_string();
                            let stubbed = stubbed.to_string();
                            self.lines.push(ChatLine::System(crate::i18n::fill(
                                &self.strings.cmd_compact_done_tpl,
                                &[&chars, &stubbed],
                            )))
                        }
                        None if stubbed > 0 => {
                            let stubbed = stubbed.to_string();
                            self.lines.push(ChatLine::System(crate::i18n::fill(
                                &self.strings.cmd_compact_stubbed_tpl,
                                &[&stubbed],
                            )))
                        }
                        None => self.lines.push(ChatLine::System(
                            self.strings.cmd_compact_nothing.to_string(),
                        )),
                    }
                }
            }
            "todos" => {
                let items = self.todo_items();
                if items.is_empty() {
                    self.lines
                        .push(ChatLine::System(self.strings.cmd_todos_empty.to_string()));
                } else {
                    let rows: Vec<String> = items
                        .iter()
                        .map(|t| format!("{} {}", t.status.marker(), t.content))
                        .collect();
                    self.lines.push(ChatLine::System(crate::i18n::fill(
                        &self.strings.cmd_todos_tpl,
                        &[&rows.join("\n")],
                    )));
                }
            }
            "usage" => {
                let messages = self
                    .harness
                    .try_lock()
                    .map(|h| h.session().messages.len())
                    .unwrap_or(0);
                let saved = if self.bytes_saved > 0 {
                    format!(
                        "{} (~{} tok)",
                        format_bytes(self.bytes_saved),
                        self.tokens_saved
                    )
                } else {
                    "0B".to_string()
                };
                let ctx = if self.context_tokens > 0 {
                    self.context_tokens.to_string()
                } else {
                    self.strings.cmd_no_data.to_string()
                };
                let model = self.settings.model.clone().unwrap_or_else(|| "auto".into());
                self.lines.push(ChatLine::System(crate::i18n::fill(
                    &self.strings.cmd_usage_tpl,
                    &[
                        &ctx,
                        &self.tools_total.to_string(),
                        &saved,
                        &self.auto_turns.to_string(),
                        &self.auto.max_turns.to_string(),
                        &messages.to_string(),
                        &model,
                    ],
                )));
            }
            "tasks" => match self.jobs.as_ref() {
                None => self.lines.push(ChatLine::System(
                    self.strings.cmd_tasks_disabled.to_string(),
                )),
                Some(registry) => {
                    let snapshot = registry.snapshot();
                    if snapshot.is_empty() {
                        self.lines
                            .push(ChatLine::System(self.strings.cmd_tasks_empty.to_string()));
                    } else {
                        let rows: Vec<String> = snapshot
                            .iter()
                            .map(|(id, command, log, state)| {
                                format!(" #{id} [{}] {command}（{log}）", state.label())
                            })
                            .collect();
                        self.lines.push(ChatLine::System(crate::i18n::fill(
                            &self.strings.cmd_tasks_tpl,
                            &[&rows.join("\n")],
                        )));
                    }
                }
            },
            "kill" => {
                let Some(registry) = self.jobs.as_ref() else {
                    self.lines.push(ChatLine::System(
                        self.strings.cmd_tasks_disabled.to_string(),
                    ));
                    return false;
                };
                match args.trim().parse::<u32>() {
                    Ok(id) => {
                        let id_s = id.to_string();
                        if registry.stop(id) {
                            self.lines.push(ChatLine::System(crate::i18n::fill(
                                &self.strings.cmd_kill_done_tpl,
                                &[&id_s],
                            )));
                        } else {
                            self.lines.push(ChatLine::System(crate::i18n::fill(
                                &self.strings.cmd_kill_missing_tpl,
                                &[&id_s],
                            )));
                        }
                    }
                    Err(_) => self
                        .lines
                        .push(ChatLine::System(self.strings.cmd_kill_usage.to_string())),
                }
            }
            // 思考级别：/thinking <minimal|low|medium|high|off>，热生效（下一次请求）
            // 并落盘；不带参数显示当前级别
            "thinking" => {
                let arg = args.trim().to_ascii_lowercase();
                if arg.is_empty() {
                    let current = self
                        .settings
                        .thinking
                        .map(|l| l.effort().to_string())
                        .unwrap_or_else(|| "off".to_string());
                    self.lines.push(ChatLine::System(crate::i18n::fill(
                        &self.strings.cmd_thinking_status_tpl,
                        &[&current],
                    )));
                } else {
                    let level = if arg == "off" || arg == "none" {
                        None
                    } else {
                        match baiji_ai::ThinkingLevel::parse(&arg) {
                            Some(level) => Some(level),
                            None => {
                                self.lines.push(ChatLine::System(
                                    self.strings.cmd_thinking_usage.to_string(),
                                ));
                                return false;
                            }
                        }
                    };
                    self.settings.thinking = level;
                    if let Err(e) =
                        settings::save(&self.config_path, &self.settings, settings::KeyUpdate::Keep)
                    {
                        self.lines.push(ChatLine::System(crate::i18n::fill(
                            &self.strings.cmd_config_save_fail_tpl,
                            &[&e],
                        )));
                    } else {
                        let shown = level
                            .map(|l| l.effort().to_string())
                            .unwrap_or_else(|| "off".to_string());
                        self.harness.lock().await.set_thinking(level);
                        self.lines.push(ChatLine::System(crate::i18n::fill(
                            &self.strings.cmd_thinking_set_tpl,
                            &[&shown],
                        )));
                    }
                }
            }
            // 计划模式：/plan 开关（热切换，运行中亦可）；/plan <目标> = 开启并开始规划。
            // 开关真值在 runtime（工具门控 + 系统提示段随之生效），此处镜像仅驱动渲染
            "plan" => {
                let arg = args.trim();
                match arg.to_ascii_lowercase().as_str() {
                    "" => self.set_plan_mode(!self.plan_mode).await,
                    "on" => self.set_plan_mode(true).await,
                    "off" => self.set_plan_mode(false).await,
                    goal => {
                        if !self.plan_mode {
                            self.set_plan_mode(true).await;
                        }
                        if self.agent_running {
                            // 运行中：目标作为 steering 注入当前 run
                            // （写工具立即被计划模式门控拒绝，本轮即可转入规划）
                            self.steering.push(goal);
                            self.lines.push(ChatLine::System(crate::i18n::fill(
                                &self.strings.plan_steering_tpl,
                                &[&goal],
                            )));
                        } else {
                            self.lines.push(ChatLine::user(goal));
                            self.auto_turns = 0;
                            self.spawn_run(goal.to_string(), ui_tx.clone());
                        }
                    }
                }
            }
            // 子代理角色管理面板：查看角色（agent 文件定义）+ r 热重载
            "subagents" => {
                if self.agent_running {
                    self.lines
                        .push(ChatLine::System(self.strings.busy_subagents.to_string()));
                } else {
                    self.subagents_panel = Some(SubagentsPanel { selected: 0 });
                }
            }
            // spec 驱动：<描述> 起草 → approve（任务种子进 todo 并开跑）→ done
            "spec" => {
                let arg = args.trim();
                match arg.to_ascii_lowercase().as_str() {
                    "" => self.lines.push(ChatLine::System(
                        self.strings.cmd_spec_usage.to_string(),
                    )),
                    "approve" => {
                        let mut harness = self.harness.lock().await;
                        match harness.spec_approve() {
                            Ok(true) => {
                                let slug = harness
                                    .spec_active()
                                    .map(|s| s.slug.clone())
                                    .unwrap_or_default();
                                drop(harness);
                                self.lines.push(ChatLine::System(crate::i18n::fill(
                                    &self.strings.cmd_spec_approved_tpl,
                                    &[&slug],
                                )));
                                self.auto_turns = 0;
                                self.spawn_run(
                                    "Implement the active spec following its task list.".to_string(),
                                    ui_tx.clone(),
                                );
                            }
                            Ok(false) => self
                                .lines
                                .push(ChatLine::System(self.strings.cmd_spec_no_tasks.to_string())),
                            Err(e) => self.lines.push(ChatLine::System(crate::i18n::fill(
                                &self.strings.cmd_spec_approve_fail_tpl,
                                &[&e.to_string()],
                            ))),
                        }
                    }
                    "show" => {
                        let harness = self.harness.lock().await;
                        match harness
                            .spec_active()
                            .map(|s| std::fs::read_to_string(&s.path).ok())
                            .flatten()
                        {
                            Some(content) => {
                                let head: String =
                                    content.lines().take(60).collect::<Vec<_>>().join("\n");
                                self.lines.push(ChatLine::System(crate::i18n::fill(
                                    &self.strings.cmd_spec_show_tpl,
                                    &[&head],
                                )));
                            }
                            None => self.lines.push(ChatLine::System(
                                self.strings.cmd_spec_none.to_string(),
                            )),
                        }
                    }
                    "list" => {
                        let list = self.harness.lock().await.spec_list();
                        self.lines.push(ChatLine::System(if list.is_empty() {
                            self.strings.cmd_spec_list_empty.to_string()
                        } else {
                            list.join(" · ")
                        }));
                    }
                    "done" | "off" => {
                        self.harness.lock().await.spec_end();
                        self.lines
                            .push(ChatLine::System(self.strings.cmd_spec_cleared.to_string()));
                    }
                    description => {
                        // slug：前三个词 kebab-case
                        let slug: String = description
                            .split_whitespace()
                            .take(3)
                            .collect::<Vec<_>>()
                            .join("-")
                            .chars()
                            .map(|c| {
                                if c.is_ascii_alphanumeric() || c == '-' {
                                    c.to_ascii_lowercase()
                                } else {
                                    '-'
                                }
                            })
                            .collect();
                        let slug = slug.trim_matches('-').to_string();
                        let result = self.harness.lock().await.spec_start(&slug, description);
                        match result {
                            Ok(_) => {
                                self.lines
                                    .push(ChatLine::user(format!("/spec {description}")));
                                self.lines.push(ChatLine::System(crate::i18n::fill(
                                    &self.strings.cmd_spec_drafting_tpl,
                                    &[&slug, &slug],
                                )));
                                self.auto_turns = 0;
                                self.spawn_run(
                                    format!("Draft the specification for: {description}"),
                                    ui_tx.clone(),
                                );
                            }
                            Err(e) => self.lines.push(ChatLine::System(crate::i18n::fill(
                                &self.strings.cmd_spec_start_fail_tpl,
                                &[&e.to_string()],
                            ))),
                        }
                    }
                }
            }
            // goal 驱动：<目标> 自主推进（todo + 自动接力直到完成）
            "goal" => {
                let arg = args.trim();
                if arg.is_empty() {
                    let active = self
                        .harness
                        .try_lock()
                        .ok()
                        .and_then(|h| h.goal_objective().map(str::to_string));
                    match active {
                        Some(goal) => self.lines.push(ChatLine::System(crate::i18n::fill(
                            &self.strings.cmd_goal_active_tpl,
                            &[&goal],
                        ))),
                        None => self
                            .lines
                            .push(ChatLine::System(self.strings.cmd_goal_usage.to_string())),
                    }
                } else if arg.eq_ignore_ascii_case("off") || arg.eq_ignore_ascii_case("done") {
                    self.harness.lock().await.goal_end();
                    self.lines
                        .push(ChatLine::System(self.strings.cmd_goal_cleared.to_string()));
                } else {
                    self.harness.lock().await.goal_set(arg);
                    self.lines.push(ChatLine::user(format!("/goal {arg}")));
                    self.auto_turns = 0;
                    self.spawn_run(format!("Pursue this goal: {arg}"), ui_tx.clone());
                }
            }
            // experts 编排：主代理作为 orchestrator 委派专家子代理
            "experts" => {
                let on = match args.trim().to_ascii_lowercase().as_str() {
                    "on" => true,
                    "off" => false,
                    _ => !self
                        .harness
                        .try_lock()
                        .map(|h| h.experts_active())
                        .unwrap_or(false),
                };
                self.harness.lock().await.experts_set(on);
                self.lines.push(ChatLine::System(
                    if on {
                        self.strings.cmd_experts_on
                    } else {
                        self.strings.cmd_experts_off
                    }
                    .to_string(),
                ));
            }
            "btw" => self
                .lines
                .push(ChatLine::System(self.strings.cmd_btw_stub.to_string())),
            other => {
                // 外部 coding agent 动态命令：/codex <任务> /claude <任务> …
                if let Some(spec) = self
                    .settings
                    .external_agents
                    .iter()
                    .find(|a| a.name == other)
                    .cloned()
                {
                    let prompt = args.trim().to_string();
                    if prompt.is_empty() {
                        self.lines.push(ChatLine::System(crate::i18n::fill(
                            &self.strings.cmd_external_usage_tpl,
                            &[&spec.name, &spec.name],
                        )));
                    } else {
                        self.lines
                            .push(ChatLine::user(format!("/{} {prompt}", spec.name)));
                        self.spawn_external_agent(spec, prompt, ui_tx.clone());
                    }
                    return false;
                }
                let mut list: Vec<String> = slash_commands(self.strings.lang)
                    .iter()
                    .map(|(name, _)| format!("/{name}"))
                    .collect();
                for agent in &self.settings.external_agents {
                    list.push(format!("/{}", agent.name));
                }
                self.lines.push(ChatLine::System(crate::i18n::fill(
                    &self.strings.cmd_unknown_tpl,
                    &[&other.to_string(), &list.join(" ")],
                )));
            }
        }
        false
    }

}
