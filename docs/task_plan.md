# Task Plan: Rust Agent - 终端 AI 对话程序

## Goal
构建一个基于 Ratatui 的终端交互式 AI 对话程序，支持 ReAct Agent 架构，可对接多种 LLM 提供商（优先 Anthropic），并支持通过配置文件集成 MCP Servers 作为工具。

## Phases

### Phase 1: 项目初始化和架构设计
- [x] 创建 Rust 项目结构
- [x] 设计整体架构和模块划分
- [x] 定义核心数据结构和接口

### Phase 2: 配置系统实现 ✅
- [x] 设计配置文件格式（JSON）
- [x] 实现配置读取和验证
- [x] 支持 LLM 配置和 MCP Servers 配置（config.json）

### Phase 3: TUI 界面框架 ✅
- [x] 集成 Ratatui 框架
- [x] 实现主界面布局（对话区、输入区、状态栏）
- [x] 实现键盘事件处理

### Phase 4: LLM 接口层 ✅
- [x] 设计 Provider trait 抽象接口
- [x] 实现 Anthropic Claude API 客户端
- [x] 实现流式响应（SSE）支持

### Phase 5: ReAct Agent 核心 ✅
- [x] 实现 Agent 循环逻辑（Thought → Action → Observation）
- [x] 实现提示词模板（ReAct pattern）
- [x] 集成 LLM 调用和响应解析

### Phase 6: MCP 工具集成 ✅
- [x] 集成 rmcp SDK
- [x] 实现 MCP 客户端管理
- [x] 将 MCP tools 转换为 LLM 工具描述
- [x] 实现工具调用执行

### Phase 7: 功能整合与优化 ✅
- [x] 整合所有模块（MCP + LLM + Agent + App + main.rs）
- [x] 实现真正的 MCP JSON-RPC 通信（修复 AsyncWriteExt import）
- [x] 实现真正执行 MCP 工具（MCPClientManager.execute_tool → McpClient.call_tool）
- [x] 修复 Anthropic tool_result 序列化 bug（id → tool_use_id）
- [x] 使用原生 Anthropic tool_use API 替代脆弱的文本解析
- [x] App 集成 ReActAgent（tokio::select! 事件循环）
- [x] 错误处理和日志完善

## Key Questions

1. **配置文件放在哪里？**
   - 决策：使用平台标准配置目录
     - Linux: `~/.config/baiji/config.json`
     - macOS: `~/Library/Application Support/baiji/config.json`
     - Windows: `%APPDATA%/baiji/config.json`

2. **如何设计 Provider trait？**
   - 决策：定义 `LLMProvider` trait，包含 `chat` 和 `chat_stream` 方法
   - 支持 `serde_json::Value` 作为消息格式，便于多厂商适配

3. **ReAct 提示词如何设计？**
   - 决策：使用结构化提示词，明确分隔 Thought/Action/Observation
   - 支持 JSON 格式工具调用，便于解析

4. **MCP 工具如何注册到 LLM？**
   - 决策：将 MCP tool schema 转换为 OpenAI function 格式
   - 使用工具名称作为 MCP server + tool 的组合标识

## Decisions Made

- **配置文件**: `~/.config/baiji/config.json`（JSON 格式）
- **配置内容**: LLM 配置 + MCP Servers 配置合一
- **环境变量支持**: 配置值支持 `$VAR_NAME` 格式引用环境变量
- **异步运行时**: Tokio（标准选择）
- **HTTP 客户端**: reqwest（配合 eventsource 实现 SSE）
- **MCP SDK**: rmcp（官方 Rust SDK）
- **UI 框架**: ratatui + crossterm
- **错误处理**: thiserror + anyhow

## Status

**Phase 7 (Completed)** - 功能整合完成，真实 MCP 工具执行已实现

`cargo build` 编译通过，`cargo test` 26/26 测试通过。
