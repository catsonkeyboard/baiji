//! 终端输入模式：bracketed paste + kitty 键盘协议（Shift+Enter 换行所需）。
//!
//! 传统终端里 Enter / Shift+Enter 都是同一个字节 `0x0d`，应用无从区分；
//! kitty 键盘协议的 DISAMBIGUATE_ESCAPE_CODES 让带修饰键的 Enter 以 `CSI 13;2u`
//! 送达，crossterm 解码为 Enter+SHIFT。只在终端确认支持时推入（查询而非盲推），
//! 退出与 panic 路径都会弹出——否则退出后 shell 会停留在 CSI u 模式。
//! 不支持的终端（如 Terminal.app）仍可用 Alt+Enter / Ctrl+J / 行尾 `\` 换行。

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

/// 本进程是否推入了键盘增强标志（只弹自己推的那一层，且只弹一次）
static KEYBOARD_PUSHED: AtomicBool = AtomicBool::new(false);
static PASTE_ENABLED: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

/// 进入 TUI 后调用（raw 模式已开、事件读取线程尚未启动——支持度查询要独占 stdin）
pub(crate) fn enable_input_modes() {
    let mut out = std::io::stdout();
    if crossterm::execute!(out, EnableBracketedPaste).is_ok() {
        PASTE_ENABLED.store(true, Ordering::Release);
    }
    let supported = matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    );
    if supported
        && crossterm::execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok()
    {
        KEYBOARD_PUSHED.store(true, Ordering::Release);
    }
    tracing::info!("terminal input modes: bracketed paste on, keyboard enhancement {supported}");
    install_panic_hook();
}

/// 退出 TUI 前调用（幂等：panic 钩子与正常退出路径可能都会走到）
pub(crate) fn restore_input_modes() {
    let mut out = std::io::stdout();
    if KEYBOARD_PUSHED.swap(false, Ordering::AcqRel) {
        let _ = crossterm::execute!(out, PopKeyboardEnhancementFlags);
    }
    if PASTE_ENABLED.swap(false, Ordering::AcqRel) {
        let _ = crossterm::execute!(out, DisableBracketedPaste);
    }
}

/// 包裹现有 panic 钩子（ratatui::init 装的那个负责 raw 模式/备用屏）：先恢复输入模式
fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_input_modes();
            previous(info);
        }));
    });
}
