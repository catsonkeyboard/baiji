//! read 工具：多模式读取（JIT disclosure）
//!
//! - `full`（默认）：带行号完整内容（offset/limit 分页）
//! - `signatures`：符号大纲（函数/类型/类声明 + 行号区间），LLM 先看结构
//!   再用 `lines`/offset+limit 精准展开目标区域
//! - `map`：目录紧凑树（带文件大小）
//!
//! 截断的完整内容 spill 到 ctx store，可用 expand 工具取回。

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput, estimate_text_tokens};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::fs;

use crate::env::ExecutionEnv;

mod density;
mod outline_fallback;
mod walk_map;

pub use density::density_view;
pub(crate) use density::MIN_DENSITY;
pub(crate) use outline_fallback::is_signature_line;
pub(crate) use walk_map::{MAX_MAP_ENTRIES, walk_map};

/// signatures 视图单文件最多输出的符号数
const MAX_SIGNATURES: usize = 200;
/// 缓存重读的下限：更小的输出重发本来就便宜，stub + expand 往返反而贵
const MIN_CACHE_BYTES: usize = 2048;
/// 缓存条目上限（条目极小，超过整体清空防病态增长）
const MAX_CACHE_ENTRIES: usize = 512;

/// 一次已交付的读取：文件指纹 + 恢复句柄（重读命中时替换为短引用）
struct CachedRead {
    mtime: SystemTime,
    size: u64,
    /// 上次交付文本的 ctx 句柄（expand 可逐字取回）
    handle: String,
    delivered_bytes: u64,
    delivered_tokens: u64,
}

pub struct ReadTool {
    env: Arc<ExecutionEnv>,
    /// 缓存重读：(路径, 请求指纹) → 上次交付的指纹与恢复句柄。
    /// 进程内状态：TUI 长跑会话全程有效。同参数重读且 (mtime,size)
    /// 未变 → 返回 `[unchanged]` 短引用而非重发全文。
    cache: Mutex<HashMap<(PathBuf, String), CachedRead>>,
}

impl ReadTool {
    pub fn new(env: Arc<ExecutionEnv>) -> Self {
        Self {
            env,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 命中且文件未变（mtime+size 校验）→ 返回可恢复的短引用 stub。
    /// None = 未命中/已变/太小/无 ctx store（fail-open：正常重读）。
    fn cache_stub(
        &self,
        resolved: &Path,
        key: &str,
        mtime: SystemTime,
        size: u64,
    ) -> Option<ToolOutput> {
        // 无 ctx store 无法 spill → 不启用缓存（fail-open：正常重读）
        self.env.ctx_store()?;
        let cache = self.cache.lock().unwrap();
        let entry = cache.get(&(resolved.to_path_buf(), key.to_string()))?;
        if entry.mtime != mtime
            || entry.size != size
            || (entry.delivered_bytes as usize) < MIN_CACHE_BYTES
        {
            return None;
        }
        Some(
            ToolOutput::ok(format!(
                "[unchanged: '{}' ({key}) was read earlier in this session and has not been \
                 modified since (verified mtime+size). Previous output ({} bytes): ctx:{} — \
                 call the expand tool with this handle to view it again.]",
                resolved.display(),
                entry.delivered_bytes,
                entry.handle
            ))
            .with_original_bytes(entry.delivered_bytes)
            // 反事实口径：重发上次交付文本需要的 token
            .with_original_tokens(entry.delivered_tokens),
        )
    }

    /// 交付成功后记录。输出足够大才值得（阈值与 stub 相同）；spill 失败不记录。
    fn cache_store(
        &self,
        resolved: &Path,
        key: &str,
        mtime: SystemTime,
        size: u64,
        delivered: &str,
    ) {
        let Some(dir) = self.env.ctx_store() else {
            return;
        };
        if delivered.len() < MIN_CACHE_BYTES {
            return;
        }
        let Some(handle) = crate::env::spill_to_store(dir, delivered) else {
            return;
        };
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(
            (resolved.to_path_buf(), key.to_string()),
            CachedRead {
                mtime,
                size,
                handle,
                delivered_bytes: delivered.len() as u64,
                delivered_tokens: estimate_text_tokens(delivered) as u64,
            },
        );
    }

    async fn read_full(
        &self,
        resolved: &std::path::Path,
        offset: usize,
        limit: Option<usize>,
        density: Option<f32>,
    ) -> ToolOutput {
        let meta = match fs::metadata(resolved).await {
            Ok(meta) if meta.len() > self.env.max_file_size => {
                return ToolOutput::err(format!(
                    "[Error] file '{}' is too large ({} bytes, max {}). \
                     Try mode=signatures first, then read line ranges.",
                    resolved.display(),
                    meta.len(),
                    self.env.max_file_size
                ));
            }
            Ok(meta) => meta,
            Err(e) => {
                return ToolOutput::err(format!("[Error] reading '{}': {e}", resolved.display()));
            }
        };
        // 文件指纹（mtime 不可得则不参与缓存）
        let stamp = meta.modified().ok().map(|t| (t, meta.len()));
        let key = format!("full:{offset}:{limit:?}:{density:?}");
        if let Some((mtime, size)) = stamp
            && let Some(stub) = self.cache_stub(resolved, &key, mtime, size)
        {
            return stub;
        }

        let content = match fs::read_to_string(resolved).await {
            Ok(content) => content,
            Err(e) => {
                return ToolOutput::err(format!(
                    "[Error] reading '{}' (binary or unreadable): {e}",
                    resolved.display()
                ));
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let start = (offset - 1).min(lines.len());
        let end = limit
            .map(|l| start.saturating_add(l).min(lines.len()))
            .unwrap_or(lines.len());
        let slice = &lines[start..end];
        if slice.is_empty() {
            return ToolOutput::ok(format!(
                "[Empty] '{}' has no content in the requested range",
                resolved.display()
            ));
        }

        // 显式 density 参数：按信息熵保留 ratio 比例的行
        if let Some(ratio) = density.filter(|r| *r < 1.0) {
            let (view, kept) = density_view(slice, start, ratio);
            let header = format!(
                "[density {:.0}% — kept {}/{} lines by entropy; skipped ranges marked]",
                ratio * 100.0,
                kept,
                slice.len()
            );
            let body = format!("{header}\n{view}");
            return self.deliver_cached(body, slice, resolved, &key, stamp);
        }

        // 预算降级：预计超限且未显式指定 density → 自动按预算比例熵选行
        let estimated: usize = slice.iter().map(|l| l.len() + 8).sum();
        if self.env.density_fallback && estimated > self.env.max_output_bytes {
            let ratio =
                (self.env.max_output_bytes as f32 / estimated as f32).clamp(MIN_DENSITY, 0.95);
            let (view, kept) = density_view(slice, start, ratio);
            let header = format!(
                "[auto-density {:.0}% — {}/{} lines kept by entropy to fit the {} byte budget; \
                 re-read with offset/limit or mode=signatures for other views]",
                ratio * 100.0,
                kept,
                slice.len(),
                self.env.max_output_bytes
            );
            let body = format!("{header}\n{view}");
            return self.deliver_cached(body, slice, resolved, &key, stamp);
        }

        let numbered = slice
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}\t{}", start + i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        self.deliver_cached(numbered, slice, resolved, &key, stamp)
    }

    /// 输出落账（截断保护 + 台账，字节与 token 双口径）+ 缓存记录
    fn deliver_cached(
        &self,
        text: String,
        slice: &[&str],
        resolved: &Path,
        key: &str,
        stamp: Option<(SystemTime, u64)>,
    ) -> ToolOutput {
        let out = self.deliver(text, slice);
        if let Some((mtime, size)) = stamp {
            self.cache_store(resolved, key, mtime, size, &out.content);
        }
        out
    }

    /// 输出落账（截断保护 + 台账，字节与 token 双口径）
    fn deliver(&self, text: String, slice: &[&str]) -> ToolOutput {
        let original_estimate: u64 = slice.iter().map(|l| l.len() + 8).sum::<usize>() as u64;
        let token_estimate: u64 = slice.iter().map(|l| estimate_text_tokens(l) as u64).sum();
        let (delivered, truncated_at, truncated_tokens) = self.env.truncate_with_meta(&text);
        let original = truncated_at.unwrap_or(original_estimate);
        let original_tokens = truncated_tokens.unwrap_or(token_estimate);
        if (delivered.len() as u64) < original {
            ToolOutput::ok(delivered)
                .with_original_bytes(original)
                .with_original_tokens(original_tokens)
        } else {
            ToolOutput::ok(delivered)
        }
    }

    /// 符号大纲：正则匹配常见语言的声明行（Rust/TS/JS/Go/Python/Java/C 系）
    fn read_signatures(&self, resolved: &std::path::Path, content: &str) -> ToolOutput {
        // AST 优先（带精确行区间），未知语言/解析失败自动回退正则
        let symbols =
            crate::signatures::outline(content, crate::signatures::Lang::detect(resolved));
        let symbols: Vec<crate::signatures::Symbol> =
            symbols.into_iter().take(MAX_SIGNATURES).collect();

        if symbols.is_empty() {
            return ToolOutput::ok(format!(
                "[No symbols detected in '{}' — fall back to mode=full]",
                resolved.display()
            ));
        }

        let span_width = symbols
            .iter()
            .map(|s| format!("L{}-{}", s.line_start, s.line_end).len())
            .max()
            .unwrap_or(6);
        let mut out: Vec<String> = symbols
            .iter()
            .map(|s| {
                let span = format!("L{}-{}", s.line_start, s.line_end);
                let label = if s.name.is_empty() {
                    String::new()
                } else {
                    format!("{} {}", s.kind, s.name)
                };
                format!(
                    "{span:<width$}  {label}  {}",
                    s.signature,
                    width = span_width
                )
            })
            .collect();
        let header = format!(
            "== signatures of {} ({} lines, {} symbols; read spans with offset/limit) ==",
            resolved.display(),
            content.lines().count(),
            symbols.len()
        );
        out.insert(0, header);
        ToolOutput::ok(out.join("\n"))
    }

    /// 目录紧凑树
    fn read_map(&self, root: &std::path::Path, content: &str) -> ToolOutput {
        // map 作用对象是文件：返回单行摘要 + signatures 提示
        let lines = content.lines().count();
        ToolOutput::ok(format!(
            "'{}' is a file ({} bytes, {} lines). Use mode=signatures for its outline.",
            root.display(),
            content.len(),
            lines
        ))
    }

    fn map_dir(&self, root: &std::path::Path) -> ToolOutput {
        let mut entries = Vec::new();
        walk_map(root, "", 0, self.env.max_search_depth, &mut entries);
        if entries.is_empty() {
            return ToolOutput::ok(format!("(empty directory '{}')", root.display()));
        }
        let header = format!("== map of {} ==", root.display());
        let mut lines = vec![header];
        lines.extend(entries.iter().take(MAX_MAP_ENTRIES).cloned());
        if entries.len() > MAX_MAP_ENTRIES {
            lines.push(format!(
                "[... {} more entries omitted — use find for targeted search]",
                entries.len() - MAX_MAP_ENTRIES
            ));
        }
        ToolOutput::ok(lines.join("\n"))
    }
}

/// density 视图：按行信息熵选行，保留原顺序与行号，跳过的连续段
/// 以 `[La-Lb skipped]` 标记（行号锚点不丢，可继续 offset/limit 精读）。

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read files or directory structure. Modes: 'signatures' (symbol outline with line \
         numbers — best first look), 'map' (compact directory tree), 'full' (numbered lines, \
         default). 'density' (0.05-1.0) keeps the highest-entropy lines in full mode. \
         Workflow: signatures first, then read the exact line range with offset/limit. \
         Re-reading an unmodified file with identical arguments returns a short '[unchanged]' \
         stub with a ctx: handle (use expand to view the previous output). \
         Truncated output can be recovered via the expand tool."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path (or directory for mode=map)"},
                "mode": {"type": "string", "enum": ["full", "signatures", "map"], "description": "Read mode (default full)"},
                "offset": {"type": "integer", "description": "Starting line number (1-based, full mode)"},
                "limit": {"type": "integer", "description": "Maximum lines to return (full mode)"},
                "density": {"type": "number", "description": "Fraction of lines to keep, selected by information entropy (full mode, e.g. 0.4)"}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(path) = args["path"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'path'"));
        };
        let mode = args["mode"].as_str().unwrap_or("full");
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().map(|l| l as usize);
        let density = args["density"].as_f64().map(|d| d.clamp(0.0, 1.0) as f32);

        let resolved = match self.env.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("[Policy denied] {e}"))),
        };

        let is_dir = resolved.is_dir();

        match mode {
            "map" => {
                if is_dir {
                    Ok(self.map_dir(&resolved))
                } else {
                    match fs::read_to_string(&resolved).await {
                        Ok(content) => Ok(self.read_map(&resolved, &content)),
                        Err(e) => Ok(ToolOutput::err(format!(
                            "[Error] reading '{}': {e}",
                            resolved.display()
                        ))),
                    }
                }
            }
            "signatures" => {
                if is_dir {
                    return Ok(ToolOutput::err(format!(
                        "[Error] '{}' is a directory; use mode=map for directories",
                        resolved.display()
                    )));
                }
                let meta = match fs::metadata(&resolved).await {
                    Ok(meta) if meta.len() > self.env.max_file_size => {
                        return Ok(ToolOutput::err(format!(
                            "[Error] file too large ({} bytes) — use grep to locate regions",
                            meta.len()
                        )));
                    }
                    Ok(meta) => meta,
                    Err(e) => {
                        return Ok(ToolOutput::err(format!(
                            "[Error] reading '{}': {e}",
                            resolved.display()
                        )));
                    }
                };
                let stamp = meta.modified().ok().map(|t| (t, meta.len()));
                if let Some((mtime, size)) = stamp
                    && let Some(stub) = self.cache_stub(&resolved, "signatures", mtime, size)
                {
                    return Ok(stub);
                }
                match fs::read_to_string(&resolved).await {
                    Ok(content) => {
                        let out = self.read_signatures(&resolved, &content);
                        if let Some((mtime, size)) = stamp {
                            self.cache_store(&resolved, "signatures", mtime, size, &out.content);
                        }
                        Ok(out)
                    }
                    Err(e) => Ok(ToolOutput::err(format!(
                        "[Error] reading '{}': {e}",
                        resolved.display()
                    ))),
                }
            }
            _ => {
                if is_dir {
                    return Ok(ToolOutput::err(format!(
                        "[Error] '{}' is a directory; use mode=map",
                        resolved.display()
                    )));
                }
                Ok(self.read_full(&resolved, offset, limit, density).await)
            }
        }
    }
}

#[cfg(test)]
mod tests;
