# TUI 滚动 Bug 修复摘要

## Bug 1 — 滚动卡死（键盘 Up/Down 无效）

**文件**：`src/app.rs`、`src/ui/mod.rs`

**根因**：`scroll_to_bottom()` 将 `self.scroll` 设为 `usize::MAX` 作为哨兵值。渲染时虽然在局部变量上做了 clamp（`let scroll = app.scroll.min(max_scroll)`），但结果从未写回 `app.scroll`。键盘事件直接对内存中的 `usize::MAX` 做加减，视觉上无任何变化：
- 按 Up：`usize::MAX - 1` 仍远大于 `max_scroll`，渲染还是钉在底部
- 按 Down：`usize::MAX.saturating_add(1) == usize::MAX`，完全不变

**修复**：
- `UI::draw` 和 `render_chat_area` 签名改为接收 `&mut App`
- 渲染计算完 `scroll` 后，立即写回：`app.scroll = scroll`

---

## Bug 2 — 滚动条无法滚到底部

**文件**：`src/ui/mod.rs`

**根因**：Ratatui 0.29 的 `part_lengths()` 内部以 `content_length - 1` 作为 thumb 的最大位置（`max_position`），即 thumb 只有在 `position == content_length - 1` 时才贴底。

旧代码：
```rust
ScrollbarState::new(total_lines).position(scroll)
```
`position` 最大值为 `max_scroll = total_lines - visible_lines`，比 `total_lines - 1` 小 `visible_lines - 1`，导致 thumb 底部始终留有缺口（缺口大小 ≈ `visible_lines / total_lines * track_height`）。

**修复**：
```rust
// 旧
ScrollbarState::new(total_lines).position(scroll).viewport_content_length(visible_lines)

// 新
ScrollbarState::new(max_scroll + 1).position(scroll).viewport_content_length(visible_lines)
```
`content_length = max_scroll + 1`，使得 `content_length - 1 == max_scroll`，position 到达最大值时 thumb 恰好贴底。
