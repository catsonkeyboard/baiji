//! 运行中用户引导（Steering）消息队列
//!
//! Agent 运行期间用户发送的新消息进入该队列；
//! Runtime 在每轮开始与每个工具执行后检查队列，
//! 将新指令注入上下文（不打断当前流，但可跳过剩余工具调用）。

use std::collections::VecDeque;
use std::sync::Mutex;

/// 线程安全的 steering 消息队列
#[derive(Debug, Default)]
pub struct SteeringQueue {
    inner: Mutex<VecDeque<String>>,
}

impl SteeringQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用户侧：压入一条引导消息
    pub fn push(&self, message: impl Into<String>) {
        self.inner.lock().unwrap().push_back(message.into());
    }

    /// Runtime 侧：取出并清空所有待处理消息
    pub fn drain(&self) -> Vec<String> {
        let mut queue = self.inner.lock().unwrap();
        queue.drain(..).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_drain_order() {
        let queue = SteeringQueue::new();
        assert!(queue.is_empty());

        queue.push("first");
        queue.push("second");
        assert_eq!(queue.len(), 2);

        assert_eq!(queue.drain(), vec!["first".to_string(), "second".to_string()]);
        assert!(queue.is_empty());
        assert!(queue.drain().is_empty());
    }
}
