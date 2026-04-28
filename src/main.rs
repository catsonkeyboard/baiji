mod agent;
mod app;
mod config;
mod event;
mod llm;
mod mcp;
mod ui;

use anyhow::Result;
use std::sync::Arc;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志：写入程序同目录 ./logs/，分 info.log（INFO 及以上）和 error.log（仅 ERROR）
    let log_dir = std::path::Path::new("logs");
    std::fs::create_dir_all(log_dir).unwrap_or_default();

    let info_appender = tracing_appender::rolling::daily(log_dir, "info.log");
    let error_appender = tracing_appender::rolling::daily(log_dir, "error.log");
    let (info_nb, _info_guard) = tracing_appender::non_blocking(info_appender);
    let (error_nb, _error_guard) = tracing_appender::non_blocking(error_appender);

    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let info_writer = info_nb.with_max_level(tracing::Level::INFO);
    let error_writer = error_nb.with_max_level(tracing::Level::ERROR);

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(info_writer.and(error_writer))
        .init();

    info!("Starting baiji...");

    // 加载配置
    let config = match config::Config::load() {
        Ok(config) => {
            info!("Configuration loaded successfully");
            info!("LLM Provider: {}", config.llm.provider);
            info!("LLM Model: {}", config.llm.model);
            config
        }
        Err(e) => {
            warn!("Failed to load configuration: {}", e);
            let config_path = config::Config::default_config_path()?;
            eprintln!("配置加载失败: {}", e);
            eprintln!("\n请按照以下步骤创建配置文件:");
            eprintln!("1. 创建配置目录:");
            eprintln!("   mkdir -p '{}'", config_path.parent().unwrap().display());
            eprintln!("2. 复制示例配置:");
            eprintln!("   cp config.example.json '{}'", config_path.display());
            eprintln!("3. 编辑配置，设置你的 API 密钥");
            return Err(e);
        }
    };

    // 创建 LLM Provider
    let llm = match llm::ProviderFactory::create(
        &config.llm.provider,
        config.llm.base_url.clone(),
        config.llm.api_key.clone(),
        config.llm.model.clone(),
    ) {
        Ok(provider) => {
            info!("LLM provider '{}' created", config.llm.provider);
            provider
        }
        Err(e) => {
            error!("Failed to create LLM provider: {}", e);
            return Err(e);
        }
    };

    // --- mcporter bridge（不在启动时 discover，TUI 内后台异步进行）---
    let mcporter_config = std::path::PathBuf::from("mcporter.json");
    let mcporter = if mcporter_config.exists() {
        info!("Found mcporter.json, MCP discovery will run in background");
        Some(Arc::new(mcp::McporterBridge::new(mcporter_config)))
    } else {
        info!("No mcporter.json found, skipping MCP");
        None
    };

    // 内置工具
    let builtin = agent::builtin_tools::builtin_tool_definitions();
    let builtin_count = builtin.len();
    info!("Loaded {} builtin tools", builtin_count);

    // 创建工具策略引擎（Harness Engineering 架构约束）
    let policy = agent::ToolPolicy::from_config(&config.policy);
    info!("Tool policy loaded: {} allowed paths, {} blocked tools",
        config.policy.allowed_paths.len(),
        config.policy.blocked_tools.len(),
    );

    // 创建 ReAct Agent（仅含内置工具，MCP 工具后台追加）
    let mut agent_builder = agent::ReActAgent::new(llm)
        .with_tools(builtin)
        .with_policy(policy);
    if let Some(bridge) = &mcporter {
        agent_builder = agent_builder.with_mcporter(bridge.clone());
    }
    let agent = Arc::new(agent_builder);

    // 创建并运行应用
    let mut app = app::App::new(config, Some(agent), mcporter, builtin_count);

    if let Err(e) = app.run().await {
        error!("Application error: {}", e);
        eprintln!("应用运行出错: {}", e);
        return Err(e);
    }

    info!("baiji exited normally");
    Ok(())
}
