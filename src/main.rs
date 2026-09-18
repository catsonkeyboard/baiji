//! baiji — 终端 AI Agent 客户端（composition root）
//!
//! 装配流程：CLI 解析 → 配置（厂商预设 + API Key + 模型发现）→ Provider →
//! ExecutionEnv + 内置工具 + 插件 → AgentRuntime → AgentHarness（skills、
//! session、JSONL）→ TUI 或 headless 一次性执行。
//!
//! CLI：
//! - `baiji`                 交互式 TUI
//! - `baiji -e "<msg>"`      headless 执行（`--yes` 自动放行确认、`--session <id>` 续会话）
//! - `baiji --sessions`      列出会话

mod cli;
mod config;
mod headless;

use anyhow::{Context, Result};
use baiji_agent::{AgentRuntime, HookRegistry, ToolRegistry};
use baiji_harness::AgentHarness;
use baiji_telemetry::{NoopTelemetry, Telemetry};
use std::sync::Arc;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    let options = cli::parse(&std::env::args().skip(1).collect::<Vec<_>>());
    if options.help {
        print!("{}", cli::USAGE);
        return Ok(());
    }

    let baiji_dir = dirs::home_dir()
        .context("无法获取用户主目录")?
        .join(".baiji");
    // guard 持有到 main 返回，日志才会真正落盘
    let _log_guards = init_logging(&baiji_dir);

    // --sessions：无需 Provider/密钥，直接读存储（不创建新会话）
    if options.list_sessions {
        return headless::print_sessions(&baiji_dir.join("sessions"));
    }

    // ---- 配置与 Provider ----
    let app_config = match config::AppConfig::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("配置加载失败: {e:#}");
            let path = config::AppConfig::default_path()?;
            eprintln!("\n请编辑 {} 后重新启动。", path.display());
            return Err(e);
        }
    };

    let resolved = app_config.resolve()?;
    info!(
        "vendor: {} ({}) · endpoint: {} · protocol: {} · base_url: {} · key from: {}",
        resolved.vendor.display_name,
        resolved.vendor.id,
        app_config.endpoint.as_deref().unwrap_or("api"),
        resolved.config.protocol.as_str(),
        resolved.config.base_url,
        resolved.key_source
    );

    // 模型：显式配置 > 自动发现（按厂商偏好挑对话模型）
    let mut provider_config = resolved.config;
    let discovered: Option<baiji_ai::ModelInfo> = match app_config.model.as_deref() {
        Some(model) if !model.is_empty() => {
            provider_config.model = model.to_string();
            lookup_model_info(resolved.vendor, &provider_config).await
        }
        _ => {
            // 订阅制端点只接受固定模型 id（如 Kimi Code 的 kimi-for-coding）
            let variant_default = app_config
                .endpoint
                .as_deref()
                .and_then(|name| resolved.vendor.find_endpoint(name))
                .and_then(|variant| variant.default_model);
            match variant_default {
                Some(model) => {
                    info!("using endpoint default model '{model}'");
                    provider_config.model = model.to_string();
                    None
                }
                None => {
                    let picked = pick_model(resolved.vendor, &provider_config).await?;
                    provider_config.model = picked.id.clone();
                    Some(picked)
                }
            }
        }
    };

    // 聚合厂商（OpenCode Zen）按模型家族选协议；用户显式配置的 endpoint/protocol 优先
    if app_config.endpoint.is_none()
        && app_config.protocol.is_none()
        && let Some(endpoint) = baiji_ai::auto_endpoint(resolved.vendor, &provider_config.model)
    {
            let routed = baiji_ai::resolve_vendor(
                resolved.vendor,
                Some(endpoint),
                app_config.base_url.as_deref(),
                None,
            )?;
            if routed.protocol != provider_config.protocol {
                info!(
                    "model '{}' routed to endpoint '{endpoint}' ({})",
                    provider_config.model,
                    routed.protocol.as_str()
                );
            }
            provider_config.protocol = routed.protocol;
            provider_config.base_url = routed.base_url;
    }
    let limits = baiji_ai::model_limits(&provider_config.model, discovered.as_ref());
    info!(
        "model limits: context={} max_output={:?} ({})",
        limits.context_length,
        limits.max_output_tokens,
        if limits.discovered { "from vendor API" } else { "built-in fallback" }
    );
    let model_name = provider_config.model.clone();
    let api_key = provider_config.api_key.clone();
    let provider = baiji_ai::build_provider(provider_config)?;
    info!("model: {}", model_name);

    // ---- 执行环境与工具 ----
    let workdir = std::env::current_dir().context("无法获取当前目录")?;
    let mut env = baiji_tools::ExecutionEnv::new(&workdir)
        .with_ctx_store(baiji_dir.join("ctx-store"))
        .with_max_output_bytes(app_config.policy.max_tool_output_bytes)
        .with_command_timeout(std::time::Duration::from_secs(
            app_config.policy.bash_timeout_secs,
        ));
    for extra in &app_config.policy.allowed_paths {
        env = env.with_allowed_root(extra);
    }
    if !app_config.policy.compression_enabled {
        env = env.without_compression();
    }

    let mut tools = ToolRegistry::new();
    for tool in baiji_tools::builtin_tools(env) {
        tools.register(tool);
    }

    // ---- 跨会话项目记忆（memory 工具 + 系统提示注入）----
    let memory_store = Arc::new(baiji_harness::MemoryStore::open(baiji_dir.join("memory")));
    let memory_project = baiji_harness::project_key(&workdir);
    tools.register(Arc::new(baiji_harness::MemoryTool::new(
        memory_store.clone(),
        memory_project.clone(),
    )));
    info!("project memory enabled for '{memory_project}'");

    // skills：同名时项目级 ./.baiji/skills 覆盖用户级 ~/.baiji/skills
    // （load_skills 后者覆盖前者，故用户级在前）。系统提示只列清单，正文经 skill 工具按需加载
    let skills = baiji_harness::load_skills(&[
        baiji_dir.join("skills"),
        workdir.join(".baiji").join("skills"),
    ]);
    if !skills.is_empty() {
        info!(
            "loaded {} skills: {:?}",
            skills.len(),
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
        );
        tools.register(Arc::new(baiji_harness::SkillTool::new(skills.clone())));
    }

    // ---- MCP 工具（项目根存在 mcporter.json 时启用）----
    let mcporter_config = workdir.join("mcporter.json");
    match baiji_extensions::register_mcp_tools(&mut tools, mcporter_config.clone()).await {
        Ok(count) if count > 0 => {
            info!("registered {count} MCP tools from {}", mcporter_config.display())
        }
        Ok(_) => {}
        Err(e) => warn!("MCP discovery failed: {e}"),
    }

    // ---- 插件 ----
    let mut hooks = HookRegistry::new();
    let applied = baiji_extensions::PluginManager::new()
        .add(Box::new(baiji_extensions::ClockPlugin))
        .add(Box::new(baiji_extensions::SafetyPlugin))
        .apply(&mut tools, &mut hooks)?;
    info!(
        "tools: {} ({}) · plugins: {}",
        tools.len(),
        tools.names().join(", "),
        applied.join(", ")
    );

    // ---- 遥测（BAIJI_TELEMETRY=file 时落盘 JSONL，默认 Noop）----
    let telemetry: Arc<dyn Telemetry> = match std::env::var("BAIJI_TELEMETRY").as_deref() {
        Ok("file") | Ok("jsonl") => {
            let trace_file = baiji_dir
                .join("traces")
                .join(format!("trace_{}.jsonl", chrono_time_compact()));
            match baiji_telemetry::JsonlTelemetry::open(&trace_file) {
                Ok(t) => {
                    info!("telemetry → {}", trace_file.display());
                    Arc::new(t)
                }
                Err(e) => {
                    warn!("telemetry file backend unavailable ({e}), falling back to noop");
                    Arc::new(NoopTelemetry)
                }
            }
        }
        _ => Arc::new(NoopTelemetry),
    };

    // ---- HITL 确认策略（按模式选择）----
    // - TUI：配置了名单 → InteractiveApprover 弹框
    // - headless --yes：AutoApprover 自动放行
    // - headless（默认）：DenyAllApprover 拒绝（无人值守安全默认）
    let mut confirm_rx: Option<tokio::sync::mpsc::UnboundedReceiver<baiji_tui::ConfirmDialog>> =
        None;
    let approver: Arc<dyn baiji_agent::Approver> =
        if app_config.policy.require_confirmation_tools.is_empty() {
            Arc::new(baiji_agent::AutoApprover)
        } else if options.exec.is_some() {
            if options.yes {
                info!("headless mode: confirmations auto-approved (--yes)");
                Arc::new(baiji_agent::AutoApprover)
            } else {
                info!(
                    "headless mode: confirmations DENIED for {} (pass --yes to approve)",
                    app_config.policy.require_confirmation_tools.join(", ")
                );
                Arc::new(baiji_agent::DenyAllApprover)
            }
        } else {
            info!(
                "confirmation required for: {}",
                app_config.policy.require_confirmation_tools.join(", ")
            );
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            confirm_rx = Some(rx);
            Arc::new(baiji_tui::InteractiveApprover::new(tx))
        };

    // ---- Runtime + Harness ----
    let mut runtime_builder = AgentRuntime::new(provider)
        .with_tools(tools)
        .with_hooks(hooks)
        .with_telemetry(telemetry.clone());
    if let Some(max_tokens) = app_config.max_tokens {
        runtime_builder = runtime_builder.with_max_tokens(max_tokens);
    }
    // 不超过模型自身的输出上限（超出会被 API 直接拒绝）
    if let Some(cap) = limits.max_output_tokens.and_then(|c| u32::try_from(c).ok())
        && runtime_builder.max_tokens() > cap
    {
        warn!("max_tokens clamped to model limit {cap}");
        runtime_builder = runtime_builder.with_max_tokens(cap);
    }
    if !app_config.policy.require_confirmation_tools.is_empty() {
        runtime_builder = runtime_builder.with_confirmation(baiji_agent::ConfirmationGate::new(
            app_config.policy.require_confirmation_tools.clone(),
            approver,
        ));
    }
    if app_config.policy.verbosity_steer {
        info!("verbosity steer enabled (constant conciseness suffix on the last user turn)");
        runtime_builder = runtime_builder.with_verbosity_steer(true);
    }
    let runtime = Arc::new(runtime_builder);

    let mut harness = match &options.session {
        Some(session_id) if options.exec.is_some() => {
            info!("resuming session {session_id}");
            AgentHarness::load(runtime, baiji_dir.join("sessions"), session_id)?
        }
        _ => AgentHarness::new(runtime, baiji_dir.join("sessions"))?,
    };

    // 压缩阈值随模型上下文窗口而定（不再固定 48k）
    harness.set_context_window(limits.context_length);

    // 历史 tool result stub 化与 expand 工具共用同一 ctx store
    harness.set_ctx_store(baiji_dir.join("ctx-store"));

    // LLM 压缩摘要（可选）
    if app_config.llm_compaction.unwrap_or(false) {
        info!("LLM compaction enabled");
        harness.set_llm_compaction(true);
    }

    // 跨会话记忆注入系统提示
    harness.set_memory(memory_store, memory_project);

    if !skills.is_empty() {
        harness.set_skills(&skills);
    }

    // 自定义系统提示：项目级 ./.baiji/system.md 优先于用户级 ~/.baiji/system.md
    // （模板变量 {{cwd}} {{date}} {{os}} {{model}} 每次运行时渲染）
    for candidate in [workdir.join(".baiji").join("system.md"), baiji_dir.join("system.md")] {
        if let Ok(custom) = std::fs::read_to_string(&candidate)
            && !custom.trim().is_empty()
        {
            info!("using custom system prompt {}", candidate.display());
            harness.set_base_prompt(custom);
            break;
        }
    }

    // 用户 prompt 模板（/name 参数）：同名时项目级覆盖用户级
    let templates = baiji_harness::load_templates(&[
        baiji_dir.join("prompts"),
        workdir.join(".baiji").join("prompts"),
    ]);
    if !templates.is_empty() {
        info!(
            "loaded {} prompt templates: {:?}",
            templates.len(),
            templates.iter().map(|t| t.name.as_str()).collect::<Vec<_>>()
        );
        harness.set_templates(templates);
    }
    harness.set_telemetry(telemetry);

    // ---- headless 一次性执行 ----
    if let Some(input) = options.exec.clone() {
        // Ctrl-C：第一次优雅取消（杀掉正在跑的命令、落盘已完成的轮次），第二次强退
        let cancel = tokio_util::sync::CancellationToken::new();
        let on_signal = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                on_signal.cancel();
                if tokio::signal::ctrl_c().await.is_ok() {
                    std::process::exit(130);
                }
            }
        });

        let outcome = headless::run_once(harness, &input, false, cancel).await?;
        info!("headless run completed (exit code {})", outcome.exit_code());
        let code = outcome.exit_code();
        if code != 0 {
            // 先让日志 guard 析构落盘，再带退出码退出
            drop(_log_guards);
            std::process::exit(code);
        }
        return Ok(());
    }

    // ---- TUI ----
    let theme = baiji_tui::Theme::parse(
        &app_config
            .ui
            .as_ref()
            .map(|u| u.theme.clone())
            .unwrap_or_else(|| "dark".to_string()),
    );
    let tui_settings = baiji_tui::RuntimeSettings {
        vendor: resolved.vendor.id.to_string(),
        endpoint: app_config.endpoint.clone(),
        model: Some(model_name.clone()),
        api_key,
    };
    let config_path = config::AppConfig::default_path()?;

    let harness = Arc::new(tokio::sync::Mutex::new(harness));
    if let Err(e) = baiji_tui::run(harness, theme, confirm_rx, config_path, tui_settings).await {
        error!("TUI error: {e}");
        return Err(e);
    }
    info!("baiji exited normally");
    Ok(())
}

fn chrono_time_compact() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}", d.as_secs()))
        .unwrap_or_else(|_| "0".to_string())
}

/// 模型自动发现：按厂商偏好从模型列表里挑一个对话模型；厂商不支持发现或
/// 发现失败时报错并引导用户显式配置。
async fn pick_model(
    vendor: &baiji_ai::VendorPreset,
    provider_config: &baiji_ai::ProviderConfig,
) -> Result<baiji_ai::ModelInfo> {
    let vendor_hint = "请在配置中显式设置 model 字段";
    if !vendor.model_discovery {
        warn!(
            "model not configured; vendor '{}' does not expose a models API, {vendor_hint}",
            vendor.id
        );
        anyhow::bail!("无法自动发现模型（{vendor_hint}）");
    }

    match baiji_ai::discover_models(vendor, provider_config).await {
        Ok(models) => match baiji_ai::pick_default_model(vendor, &models) {
            Some(picked) => {
                info!("discovered {} models via API, using '{}'", models.len(), picked.id);
                eprintln!(
                    "未配置 model，已自动选择 '{}'（共 {} 个可用；可用 /model 或配置文件更改）",
                    picked.id,
                    models.len()
                );
                Ok(picked.clone())
            }
            None => {
                warn!("no chat-capable model in list of {}, {vendor_hint}", models.len());
                anyhow::bail!("模型列表中没有可用的对话模型（{vendor_hint}）")
            }
        },
        Err(e) => {
            warn!("model discovery failed: {e}");
            anyhow::bail!("模型自动发现失败: {e}（{vendor_hint}）")
        }
    }
}

/// 已显式配置 model 时，尽力查询其元数据（上下文窗口/输出上限）。
/// 限时 4 秒、失败静默：拿不到就用内置兜底表，不拖慢也不阻断启动。
async fn lookup_model_info(
    vendor: &baiji_ai::VendorPreset,
    provider_config: &baiji_ai::ProviderConfig,
) -> Option<baiji_ai::ModelInfo> {
    if !vendor.model_discovery {
        return None;
    }
    let lookup = baiji_ai::discover_models(vendor, provider_config);
    match tokio::time::timeout(std::time::Duration::from_secs(4), lookup).await {
        Ok(Ok(models)) => models.into_iter().find(|m| m.id == provider_config.model),
        Ok(Err(e)) => {
            warn!("model metadata lookup failed: {e}");
            None
        }
        Err(_) => {
            warn!("model metadata lookup timed out");
            None
        }
    }
}

/// 日志：写入 `~/.baiji/logs/`（info.log + error.log），不落终端（stdout 归答案流）。
///
/// 返回的 guard 必须存活到进程结束：non_blocking 的后台写线程随 guard 析构而停止，
/// 提前丢弃会让之后的日志全部丢失。
/// 不写到当前目录：那会在用户项目里留下 logs/，还会被 agent 自己的 grep/find 扫到。
fn init_logging(baiji_dir: &std::path::Path) -> Vec<tracing_appender::non_blocking::WorkerGuard> {
    let log_dir = baiji_dir.join("logs");
    std::fs::create_dir_all(&log_dir).ok();

    let info_appender = tracing_appender::rolling::daily(&log_dir, "info.log");
    let error_appender = tracing_appender::rolling::daily(&log_dir, "error.log");
    let (info_nb, info_guard) = tracing_appender::non_blocking(info_appender);
    let (error_nb, error_guard) = tracing_appender::non_blocking(error_appender);

    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let info_writer = info_nb.with_max_level(tracing::Level::INFO);
    let error_writer = error_nb.with_max_level(tracing::Level::ERROR);

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(info_writer.and(error_writer))
        .init();
    vec![info_guard, error_guard]
}
