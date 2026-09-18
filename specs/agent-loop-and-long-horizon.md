# baiji 待开发任务清单：agent 循环优化 + 长程任务能力

> 日期：2026-09-18
> 来源：两轮分析的整合——
> ① pi（badlogic/pi-mono）agent 循环对比分析中值得借鉴的 4 项（P-A/P-G/P-F/P-B）；
> ② 长程任务能力评估中识别的 5 个缺口。
> 两份清单中「usage 锚定估算」「文件清单摘要」重叠，去重后共 **8 个任务**。
> 前置状态：P1-P4 上下文压缩栈、verbosity steer、pi 式分层设置均已落地
> （提交 984a509…369ebcf，285 测试全绿）。

## 总览

| ID | 任务 | 来源 | 价值 | 工作量 | 优先级 | 依赖 |
|----|------|------|------|--------|--------|------|
| T1 | todo 工具（任务状态外置） | 长程 | 高 | 中 | P0 | — |
| T2 | usage 锚定的压缩触发估算 | pi P-A + 长程 | 高 | 中 | P0 | — |
| T3 | 压缩摘要保留文件操作清单 | pi P-G + 长程 | 中 | 小 | P1 | — |
| T4 | 自动接力（自主循环 + 预算上限） | 长程 | 高 | 中 | P1 | T1, T2 |
| T5 | 截断的最终答案明示 | pi P-F | 中 | 小 | P1 | — |
| T6 | 运行中 elide 复用可逆 stub | pi P-B | 低 | 小 | P2 | — |
| T7 | 后台任务（bash 后台 + jobs 工具） | 长程 | 中 | 中 | P2 | — |
| T8 | 子代理 Task 工具 | 长程 | 高 | 大 | P3 | T1（复用其摘要结构可选） |

分阶段建议：
- **Phase 1（快赢，1-2 天）**：T3、T5、T6 —— 都是局部小改动，不动架构。
- **Phase 2（长程核心）**：T1 → T2 → T4 —— todo 外置是长程任务的第一性设计，
  usage 锚定让自动接力的预算判断可信，然后才敢放开自主循环。
- **Phase 3（体验扩展）**：T7、T8。

明确不做（前轮分析已否决）：follow-up 消息队列（baiji 的"非运行态输入=新 run"语义更清晰）、
并行工具执行（需重做事件流/HITL/steering/TUI，收益场景窄）、per-model 覆盖表、
pi 的交互式设置浏览器全量版。

---

## T1 · todo 工具（任务状态外置）【P0 · ✅ 已完成 2026-09-18】

### 动机
计划目前只活在对话流里，压缩摘要只保留"用户问题 + assistant 首句 + 工具名"，
长程任务中第 30 轮的决定会在压缩后丢失。Claude Code 的 TodoWrite、pi 的 todo 扩展、
lean-ctx 的 `ctx_task`/`ctx_workflow` 均为此存在：**把目标外置到稳定结构，而非靠上下文记忆**。
这是长程任务最大的单一增益，且天然解决"resume 后重新定位"问题。

### 设计
- `TodoItem { id, content, status: Pending | InProgress | Done, note: Option<String> }`，
  会话级 `TodoStore`（内存 + 持久化）。
- **持久化**：session JSONL 新增 `Record::Todo { items }`（append-only，每次变更追加一条，
  重放取最后状态；serde 兼容旧文件——`#[serde(default)]`）。参照 `Record::Summary` 的重放语义。
- **注入**：`AgentHarness::system_prompt()` 在 skills/memory 之后追加 `## Task list` 小节
  （渲染当前清单；空清单不注入）。**关键性质：todo 在系统提示里，永不参与压缩**。
- **工具**：`todo` 工具（add / update / list / clear），实现放 `baiji-harness`
  （跟随 `MemoryTool` 的模式：harness 持状态、工具注册进 ToolRegistry）。
- 系统提示（`DEFAULT_PROMPT`）加一条使用指引：开始多步任务前先建 todo，每完成一步更新。

### 代码落点
- `crates/baiji-harness/src/todo.rs`（新）：TodoStore + TodoTool + 渲染。
- `crates/baiji-harness/src/lib.rs`：模块导出、system_prompt() 注入。
- `crates/baiji-harness/src/persist.rs`：`Record::Todo`（重放语义：取最后一条）。
- `crates/baiji-agent` 不动。

### 验收
- todo CRUD 工具测试；系统提示包含清单；重放会话后清单恢复；
- 压缩后清单仍完整（构造超预算会话触发压缩，断言 system_prompt 仍含全部条目）；
- TUI 无需改动（工具活动走既有 ToolFinished 事件）。

### 风险
无实质风险；注意 JSONL 重放语义（Title/Summary 的既有模式可照抄）。

---

## T2 · usage 锚定的压缩触发估算【P0】

### 动机
baiji 轮间压缩的触发估算是全量字符启发式（系统提示、工具定义、tokenizer 差异全部不可见），
偏差大——过早压缩浪费上下文，过迟有超窗风险。现状只有"观察值超限→强制压缩"的二值兜底。
pi 的 `estimateContextTokens`（`packages/agent/src/harness/compaction/compaction.ts:205`）：
**最近一次 assistant 的真实 usage + 其后消息的字符估算**，把不可见部分锚定在真实值上。

### 设计
- harness 在 run 事件循环（已 tap `UsageReported`）记录锚点：
  `(usage_context_tokens, 锚点后的消息数)`，存 `AgentHarness` 字段（会话内存态，不持久化）。
- `compaction::estimate_tokens` 增加锚定变体：
  `estimate_tokens_anchored(messages, anchor) = anchor.usage + estimate(messages[anchor.idx..])`。
  run() 的压缩触发判定（含"观察值强制"路径）改用锚定值。
- 锚点失效条件：压缩/stub 化改变了消息列表长度（此时退回全量估算并重建锚点）、
  switch_session/branch 后。
- 与运行中路径统一：runtime 的 `elide_old_tool_results` 触发已用
  `response.context_tokens.unwrap_or(rough_estimate)`，语义一致化。

### 代码落点
- `crates/baiji-harness/src/compaction.rs`（锚定估算函数 + 触发判定改造）。
- `crates/baiji-harness/src/lib.rs`（锚点字段维护、失效处理）。

### 验收
- 单测：给定锚点 + 追加消息，估算 = usage + 尾部估算；锚点越过消息尾部时回退全量；
- 集成：mock provider 上报 usage 后，触发精度不再依赖全量字符估算；
- 压缩/切换会话后锚点重建（防陈旧锚点导致误判）。

### 风险
锚点陈旧（消息列表被压缩缩短）→ 必须在 compact/stub/switch 后显式失效，测试覆盖。

---

## T3 · 压缩摘要保留文件操作清单【P1】

### 动机
压缩后模型不知道"读过/改过哪些文件"，恢复定位靠猜。pi 的 compaction 用
`extractFileOperations` 维护 readFiles/modifiedFiles 并跨压缩传递
（`compaction.ts` 的 `CompactionDetails`）。

### 设计
- `summarize_turns`（确定性摘要）在既有"每轮 user 问题 + assistant 首句 + 工具名"之上，
  汇总各轮 tool_calls 参数中的路径：`read/grep/find/ls` → read 集，`write/edit` → modified 集，
  `bash` 不解析（噪声大）。
- 集合去重、按首次出现排序、封顶（如各 ≤30 个，超出折叠为 `… +N more`）。
- **跨压缩传递**：摘要文本自带 `Files read: …` / `Files modified: …` 尾节，
  `previous_summary` 原样带入的既有机制自动保持连续性（无需结构化字段）。
- `compact_with_llm` 的 SUMMARIZE_PROMPT 加一句：保留文件清单。

### 代码落点
- `crates/baiji-harness/src/compaction.rs`（summarize_turns + 提取函数）。

### 验收
- 构造含 read/write 调用的多轮会话 → 摘要含两个清单；
- 二次压缩后清单仍在（previous_summary 传递）；
- 无文件操作时会话无清单行。

### 风险
无；纯摘要文本增强。

---

## T4 · 自动接力（自主循环 + 预算上限）【P1，依赖 T1+T2】

### 动机
`run()` 结束 = 任务暂停，必须用户再按一次回车。长程任务需要"todo 未完成则继续，
直到完成或触及总预算"的自主循环（同时是 T1/T2 价值的兑现点）。

### 设计
- 配置：`policy.auto_continue: false`（默认关，项目白名单可覆盖）+
  `auto_continue_max_turns`（默认 96，累计值）、可选 token 预算上限。
- 判定（harness.run 返回后，由调用方执行——TUI/headless 各自策略）：
  自主模式开 && todo 存在未完成项 && 累计轮次/预算未超 → 以固定接力输入
  （如 `继续，按 todo 清单推进`）自动发起下一次 run。
- TUI：状态栏显示自主模式剩余预算；Esc 随时可停（既有 CancellationToken 链路）；
  每次接力在聊天区插入 System 行提示。
- headless（`-e`）：加 `--continue-until-done` 开关。
- 安全：接力输入是用户可见的固定文本；HITL 确认不受影响（ConfirmationGate 照常弹窗）。

### 代码落点
- `src/config.rs`（policy 字段）、`src/main.rs`/`src/headless.rs`（接线）、
  `crates/baiji-tui/src/app.rs`（RunCompleted 后的接力判定 + 状态展示）。
- harness 可提供 `has_open_todos()` 访问器。

### 验收
- 集成：todo 3 项 + mock provider 每轮完成一项 → 自动跑 3 次 run 后停止；
- 超过 max_turns 上限强制停止并明示；Esc 中断后不再接力；
- 默认关闭时行为与现状完全一致。

### 风险
失控循环 → 硬上限 + Esc + 默认关；接力上下文膨胀 → 由 T2 锚定的压缩兜底。

---

## T5 · 截断的最终答案明示【P1】

### 动机
`stop == MaxTokens` 且无工具调用时，截断文本被当作最终答案返回，只有 warn 日志，
用户与模型都不知道答案不完整（pi 的启示：stopReason 应显式影响行为）。

### 设计
- runtime 最终答案分支：`stop == Some(StopReason::MaxTokens)` 时，
  在答案末尾追加固定标记行：
  `[答案因 max_tokens={N} 被截断 — 可发送"继续"获取剩余部分]`。
- 标记仅进本次答案与会话历史（模型下一轮自然看到并续写），不改 verbosity steer 的请求副本原则。
- TUI 无需改动（跟随答案文本）。

### 代码落点
- `crates/baiji-agent/src/runtime.rs` 最终答案分支（现仅 `warn!`）。

### 验收
- mock provider 返回 stop=MaxTokens → 答案含标记；正常完成 → 无标记。

### 风险
无。

---

## T6 · 运行中 elide 复用可逆 stub【P2】

### 动机
`elide_old_tool_results`（runtime.rs，85% 窗口紧急路径）把旧工具结果替换为**不可逆**的
`[elided …]` 占位，与 P3 的可逆 stub 基础设施（spill + 句柄 + expand）不一致。

### 设计
- runtime 获得可选 spill 能力：`with_ctx_store(PathBuf)` 存目录，
  elide 时优先 `baiji_tools::spill_to_store` 拿句柄，占位文本改为
  `[ctx stub: … ctx:<handle> — expand 可取回]`；spill 失败回退现有不可逆占位（fail-open 不变）。
- main.rs 在构造 runtime 时传入与 harness/expand 同一目录（与 `harness.set_ctx_store` 同源）。

### 代码落点
- `crates/baiji-agent/src/runtime.rs`（字段 + elide_old_tool_results 改造）、
  `crates/baiji-agent/Cargo.toml`（依赖 baiji-tools，仅用 spill_to_store —— 注意保持
  依赖图文档同步）、`src/main.rs`（接线）。

### 验收
- 超预算触发 elide 后，占位含句柄且 expand 可取回原文；无 store 时行为不变。

### 风险
agent → tools 新依赖边（与 harness → tools 同理，须在 AGENTS.md 依赖图注明理由）。

---

## T7 · 后台任务（bash 后台 + jobs 工具）【P2】

### 动机
bash 默认 30s、硬上限 300s，无后台进程管理——`npm run dev` 起了管不了，
长时间观测类任务做不了。pi 有 background-tasks 扩展。

### 设计
- bash 工具新参数 `run_in_background: bool`：spawn 后立即返回任务 id
  （进程句柄存进程内注册表：`Mutex<HashMap<id, Child + started + label>>`）。
- 新工具 `jobs`：`list`（id/命令/状态/运行时长）、`output <id>`（读截止当前的输出，
  走 ctx store spill + 截断，复用现有管道）、`stop <id>`。
- 输出捕获：后台任务各自的缓冲写临时文件（进程退出后内容 spill 到 ctx store，句柄返回）。
- 生命周期策略：TUI 退出时**默认击杀**全部后台任务（与前台 bash 的 GroupKillGuard 一致），
  避免孤儿进程；文档明示。
- HITL：`run_in_background` 命中确认名单照常弹窗。

### 代码落点
- `crates/baiji-tools/src/tools/bash.rs`（参数 + 注册表）、
  `crates/baiji-tools/src/tools/jobs.rs`（新工具）、`lib.rs` 注册。

### 验收
- 后台起 `sleep` → jobs list 可见 → output 可读 → stop 可停；
- TUI 退出后无孤儿进程；确认弹窗路径不回归。

### 风险
进程生命周期管理（孤儿、僵尸）→ 复用既有 process_group + kill_on_drop 模式；
输出缓冲无上限 → 沿用 MAX_CAPTURE_BYTES 思路封顶。

---

## T8 · 子代理 Task 工具【P3】

### 动机
探索性子任务（"找出所有调用点并总结"）的中间输出污染主上下文。subagent 用独立上下文
干脏活、只回传结论（Claude Code 的 Task 工具、pi-subagents）。
对主上下文的保护效果与 P3 的 stub 是同一哲学的放大。

### 设计
- `task` 工具：`task(prompt, tools: Option<Vec<name>>)`——spawn 一个嵌套
  `AgentRuntime`（独立的 convo 与压缩生命周期；**不含** HITL 确认门控——子代理只读为宜，
  默认工具集 = read/grep/find/ls/search/imports，写操作需显式列入）。
- 返回：子代理最终答案（走既有 truncate 管道）+ 子会话摘要一行。
- 递归深度上限 1（子代理内不可再调 task——注册表剔除 task 工具即可）。
- 预算：子代理独立 max_turns（默认 12）；ledger 照常记录（事件流汇入父流，
  `agent.tool` span 标注 `subagent=true`）。
- 并发：首版顺序执行即可（一次一个 task 调用）；并行留作后续。

### 代码落点
- `crates/baiji-agent/src/runtime.rs` 或新 `crates/baiji-agent/src/subagent.rs`
  （构造子 runtime 需要 Provider 克隆 —— `Arc<dyn Provider>` 已可克隆）。
- 注册：`baiji-tools::builtin_tools` 层面无法访问 registry → 由 main.rs 装配后注册
  （或 harness 提供 `spawn_subagent` 闭包注入工具）。

### 验收
- 集成：task 工具跑探索型 mock，父上下文只收到摘要，中间工具输出不进父历史；
- 递归调用被拒；子代理超 max_turns 返回错误结果（is_error）不炸父 run。

### 风险
工程量最大：事件流嵌套、遥测归属、取消传播（父 Esc 应取消子代理 —— CancellationToken 透传）。
建议独立设计评审后再动工。

---

## 附：与前序工作的关系

- T2/T3 直接强化 P1-P4 建成的压缩栈（触发更准、摘要更有用）；
- T1/T4 在压缩栈之上补齐长程任务的编排层；
- T6 统一"可逆压缩"的最后一处不一致；
- T7/T8 是独立扩展，不阻塞其它任务。
