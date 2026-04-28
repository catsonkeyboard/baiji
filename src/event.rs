use anyhow::Result;
use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, MouseEvent};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error};

/// 应用事件
#[derive(Debug, Clone)]
pub enum Event {
    /// 定时刷新
    Tick,
    /// 键盘事件
    Key(KeyEvent),
    /// 鼠标事件
    Mouse(MouseEvent),
}

/// 事件处理器
pub struct EventHandler {
    /// 事件接收通道
    receiver: mpsc::UnboundedReceiver<Event>,
    /// 关机标志：Drop 时置 true，通知后台线程退出
    shutdown: Arc<AtomicBool>,
}

impl Drop for EventHandler {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl EventHandler {
    /// 创建新的事件处理器
    pub fn new(tick_rate: Duration) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        // 启动 Tick 定时任务
        let tick_sender = sender.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick_rate);
            loop {
                interval.tick().await;
                if tick_sender.send(Event::Tick).is_err() {
                    break;
                }
            }
        });

        // 启动键盘事件监听任务
        // 用 event::poll(200ms) 替代 event::read() 裸阻塞：
        // 每 200ms 超时返回一次，让 tokio runtime 能在退出时干净结束，
        // 避免进程卡在永久阻塞的 spawn_blocking 上。
        let shutdown_flag = Arc::clone(&shutdown);
        tokio::spawn(async move {
            loop {
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }

                let flag = Arc::clone(&shutdown_flag);
                let result = tokio::task::spawn_blocking(move || {
                    // 最多等 200ms，超时返回 Ok(None)
                    match event::poll(Duration::from_millis(200)) {
                        Ok(true) => event::read().map(Some),
                        Ok(false) => Ok(None),
                        Err(e) => Err(e),
                    }
                })
                .await;

                // 如果关机标志已被设置，立即退出（不再发送任何事件）
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }

                match result {
                    Ok(Ok(Some(CrosstermEvent::Key(key)))) => {
                        debug!("Key event received: {:?}", key);
                        if sender.send(Event::Key(key)).is_err() {
                            break;
                        }
                    }
                    Ok(Ok(Some(CrosstermEvent::Mouse(mouse)))) => {
                        if sender.send(Event::Mouse(mouse)).is_err() {
                            break;
                        }
                    }
                    Ok(Ok(Some(CrosstermEvent::Resize(_, _)))) | Ok(Ok(None)) => {}
                    Ok(Err(e)) => {
                        error!("Error reading event: {:?}", e);
                    }
                    Err(e) => {
                        error!("Task join error: {:?}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });

        Self { receiver, shutdown }
    }

    /// 获取下一个事件
    pub async fn next(&mut self) -> Result<Event> {
        self.receiver
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("Event channel closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_handler_creation() {
        let _handler = EventHandler::new(Duration::from_millis(100));
        assert!(true);
    }
}
