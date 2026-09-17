//! baiji-telemetry — 遥测契约层
//!
//! 定义 Span / Event 的最小契约，默认提供 Noop 实现（零开销）。
//! 上层 crate（agent / harness / tools）只依赖本契约，
//! 具体的导出后端（OTel、日志、JSONL）可在外部替换注入。

use std::sync::{Arc, Mutex};

/// 属性值
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Uint(u64),
    Float(f64),
    Bool(bool),
}

impl From<&str> for AttrValue {
    fn from(v: &str) -> Self {
        AttrValue::Str(v.to_string())
    }
}

impl From<String> for AttrValue {
    fn from(v: String) -> Self {
        AttrValue::Str(v)
    }
}

impl From<u64> for AttrValue {
    fn from(v: u64) -> Self {
        AttrValue::Uint(v)
    }
}

impl From<i64> for AttrValue {
    fn from(v: i64) -> Self {
        AttrValue::Int(v)
    }
}

impl From<bool> for AttrValue {
    fn from(v: bool) -> Self {
        AttrValue::Bool(v)
    }
}

/// 属性集合的简写构造
pub fn attrs(pairs: &[(&str, AttrValue)]) -> Vec<(String, AttrValue)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// 一个 Span 的生命周期句柄。`end` 消费自身并结束 span。
pub trait Span: Send + Sync {
    /// 追加/覆盖属性
    fn set_attribute(&self, key: &str, value: AttrValue);
    /// 结束 span
    fn end(self: Box<Self>);
    /// 标记失败并结束 span
    fn end_with_error(self: Box<Self>, message: &str) {
        self.set_attribute("error", AttrValue::Bool(true));
        self.set_attribute("error.message", AttrValue::Str(message.to_string()));
        self.end();
    }
}

/// 遥测契约
pub trait Telemetry: Send + Sync {
    /// 开始一个 span
    fn span(&self, name: &str, attributes: Vec<(String, AttrValue)>) -> Box<dyn Span>;
    /// 记录一个瞬时事件
    fn event(&self, name: &str, attributes: Vec<(String, AttrValue)>);
}

/// 空实现（默认）
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopTelemetry;

impl Telemetry for NoopTelemetry {
    fn span(&self, _name: &str, _attributes: Vec<(String, AttrValue)>) -> Box<dyn Span> {
        Box::new(NoopSpan)
    }

    fn event(&self, _name: &str, _attributes: Vec<(String, AttrValue)>) {}
}

struct NoopSpan;

impl Span for NoopSpan {
    fn set_attribute(&self, _key: &str, _value: AttrValue) {}
    fn end(self: Box<Self>) {}
}

/// 记录型实现：把 span/event 收进内存，用于测试与调试。
#[derive(Debug, Default)]
pub struct RecordingTelemetry {
    inner: Arc<Mutex<RecordLog>>,
}

#[derive(Debug, Default)]
pub struct RecordLog {
    pub spans: Vec<SpanRecord>,
    pub events: Vec<EventRecord>,
}

#[derive(Debug, Clone)]
pub struct SpanRecord {
    pub name: String,
    pub attributes: Vec<(String, AttrValue)>,
    pub ended: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EventRecord {
    pub name: String,
    pub attributes: Vec<(String, AttrValue)>,
}

impl RecordingTelemetry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 共享同一份日志（跨模块注入同一实例）
    pub fn shared(&self) -> Arc<Self> {
        let log = Arc::clone(&self.inner);
        Arc::new(RecordingTelemetry { inner: log })
    }

    pub fn log(&self) -> std::sync::MutexGuard<'_, RecordLog> {
        self.inner.lock().unwrap()
    }

    pub fn span_names(&self) -> Vec<String> {
        self.log().spans.iter().map(|s| s.name.clone()).collect()
    }

    pub fn event_names(&self) -> Vec<String> {
        self.log().events.iter().map(|e| e.name.clone()).collect()
    }
}

impl Telemetry for RecordingTelemetry {
    fn span(&self, name: &str, attributes: Vec<(String, AttrValue)>) -> Box<dyn Span> {
        self.inner.lock().unwrap().spans.push(SpanRecord {
            name: name.to_string(),
            attributes,
            ended: false,
            error: None,
        });
        Box::new(RecordingSpan {
            log: Arc::clone(&self.inner),
            index: {
                // 刚压入的 span 位于末尾；索引在 end 前保持有效，
                // 因为 span 结束只会修改既有条目而不会移除
                self.inner.lock().unwrap().spans.len() - 1
            },
        })
    }

    fn event(&self, name: &str, attributes: Vec<(String, AttrValue)>) {
        self.inner.lock().unwrap().events.push(EventRecord {
            name: name.to_string(),
            attributes,
        });
    }
}

struct RecordingSpan {
    log: Arc<Mutex<RecordLog>>,
    index: usize,
}

impl Span for RecordingSpan {
    fn set_attribute(&self, key: &str, value: AttrValue) {
        let mut log = self.log.lock().unwrap();
        if let Some(span) = log.spans.get_mut(self.index) {
            span.attributes.push((key.to_string(), value));
        }
    }

    fn end(self: Box<Self>) {
        let mut log = self.log.lock().unwrap();
        if let Some(span) = log.spans.get_mut(self.index) {
            span.ended = true;
        }
    }

    fn end_with_error(self: Box<Self>, message: &str) {
        let mut log = self.log.lock().unwrap();
        if let Some(span) = log.spans.get_mut(self.index) {
            span.ended = true;
            span.error = Some(message.to_string());
        }
    }
}

// ========== JSONL 文件后端 ==========
//
// 每条 span_start / span_end / event 追加一行 JSON（时间戳为 Unix 毫秒，
// 零额外依赖）。用于离线分析 Agent 运行（BAIJI_TELEMETRY=file 启用）。

/// JSONL 落盘后端
pub struct JsonlTelemetry {
    file: Arc<Mutex<std::fs::File>>,
}

impl JsonlTelemetry {
    /// 打开（追加模式）一个轨迹文件；文件不存在时创建
    pub fn open(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
        })
    }

    fn write(file: &Mutex<std::fs::File>, mut record: serde_json::Value) {
        use std::io::Write;
        record["ts_ms"] = serde_json::json!(now_ms());
        let Ok(mut file) = file.lock() else { return };
        let _ = writeln!(file, "{record}");
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn attrs_to_json(attributes: &[(String, AttrValue)]) -> serde_json::Value {
    serde_json::Map::from_iter(
        attributes
            .iter()
            .map(|(k, v)| (k.clone(), attr_to_json(v))),
    )
    .into()
}

fn attr_to_json(value: &AttrValue) -> serde_json::Value {
    match value {
        AttrValue::Str(s) => serde_json::json!(s),
        AttrValue::Int(i) => serde_json::json!(i),
        AttrValue::Uint(u) => serde_json::json!(u),
        AttrValue::Float(f) => serde_json::json!(f),
        AttrValue::Bool(b) => serde_json::json!(b),
    }
}

struct JsonlSpan {
    file: Arc<Mutex<std::fs::File>>,
    name: String,
    started_ms: u128,
    /// span 生命周期内追加的属性（end 时随结束记录一并落盘）
    attrs: Mutex<Vec<(String, AttrValue)>>,
}

impl Span for JsonlSpan {
    fn set_attribute(&self, key: &str, value: AttrValue) {
        self.attrs
            .lock()
            .unwrap()
            .push((key.to_string(), value));
    }

    fn end(self: Box<Self>) {
        self.finish(None);
    }

    fn end_with_error(self: Box<Self>, message: &str) {
        self.finish(Some(message));
    }
}

impl JsonlSpan {
    fn finish(self: Box<Self>, error: Option<&str>) {
        let attrs = self.attrs.lock().unwrap().clone();
        let record = serde_json::json!({
            "kind": "span_end",
            "name": self.name,
            "duration_ms": now_ms().saturating_sub(self.started_ms),
            "attrs": attrs_to_json(&attrs),
            "error": error,
        });
        JsonlTelemetry::write(&self.file, record);
    }
}

impl Telemetry for JsonlTelemetry {
    fn span(&self, name: &str, attributes: Vec<(String, AttrValue)>) -> Box<dyn Span> {
        // start 行携带构造属性；运行期追加属性在 span_end 行
        let record = serde_json::json!({
            "kind": "span_start",
            "name": name,
            "attrs": attrs_to_json(&attributes),
        });
        Self::write(&self.file, record);
        Box::new(JsonlSpan {
            file: Arc::clone(&self.file),
            name: name.to_string(),
            started_ms: now_ms(),
            attrs: Mutex::new(Vec::new()),
        })
    }

    fn event(&self, name: &str, attributes: Vec<(String, AttrValue)>) {
        let record = serde_json::json!({
            "kind": "event",
            "name": name,
            "attrs": attrs_to_json(&attributes),
        });
        Self::write(&self.file, record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_telemetry_is_silent() {
        let tel = NoopTelemetry;
        let span = tel.span("test", attrs(&[("k", AttrValue::from(1u64))]));
        span.end();
        tel.event("e", vec![]);
    }

    #[test]
    fn recording_telemetry_captures() {
        let tel = RecordingTelemetry::new();
        {
            let span = tel.span("agent.turn", attrs(&[("turn", AttrValue::Uint(1))]));
            span.set_attribute("extra", AttrValue::from(true));
            span.end();
        }
        tel.event("tool.call", attrs(&[("name", AttrValue::from("bash"))]));

        let log = tel.log();
        assert_eq!(log.spans.len(), 1);
        assert!(log.spans[0].ended);
        assert_eq!(log.spans[0].attributes.len(), 2);
        assert_eq!(log.events.len(), 1);
        assert_eq!(log.events[0].name, "tool.call");
    }

    #[test]
    fn recording_span_error() {
        let tel = RecordingTelemetry::new();
        let span = tel.span("op", vec![]);
        Box::new(span).end_with_error("boom");
        let log = tel.log();
        assert_eq!(log.spans[0].error.as_deref(), Some("boom"));
        assert!(log.spans[0].ended);
    }

    #[test]
    fn shared_instances_share_log() {
        let tel = RecordingTelemetry::new();
        let other = tel.shared();
        Telemetry::event(&*other, "ping", vec![]);
        assert_eq!(tel.event_names(), vec!["ping".to_string()]);
    }

    #[test]
    fn jsonl_backend_writes_records() {
        let dir = std::env::temp_dir().join(format!(
            "baiji-telemetry-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("trace.jsonl");
        {
            let tel = JsonlTelemetry::open(&path).expect("open");
            let span = tel.span(
                "agent.tool",
                attrs(&[("name", AttrValue::from("bash"))]),
            );
            span.set_attribute("duration_ms", AttrValue::Uint(42));
            span.set_attribute("is_error", AttrValue::Bool(false));
            span.end();
            tel.event("agent.event", attrs(&[("turn", AttrValue::Uint(1))]));

            let failed = tel.span("agent.run", vec![]);
            Box::new(failed).end_with_error("boom");
        }

        let content = std::fs::read_to_string(&path).expect("read back");
        let records: Vec<serde_json::Value> = content
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid json line"))
            .collect();
        let _ = std::fs::remove_dir_all(&dir);

        // span_start + span_end + event + span_start + span_end(error)
        assert_eq!(records.len(), 5);
        assert_eq!(records[0]["kind"], "span_start");
        assert_eq!(records[0]["name"], "agent.tool");
        assert_eq!(records[0]["attrs"]["name"], "bash");
        assert!(records[0]["ts_ms"].as_u64().is_some());

        assert_eq!(records[1]["kind"], "span_end");
        assert_eq!(records[1]["attrs"]["duration_ms"], 42);
        assert_eq!(records[1]["attrs"]["is_error"], false);
        assert!(records[1]["error"].is_null());

        assert_eq!(records[2]["kind"], "event");
        assert_eq!(records[4]["kind"], "span_end");
        assert_eq!(records[4]["error"], "boom");
    }

    #[test]
    fn jsonl_backend_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!(
            "baiji-telemetry-nested-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("a/b/trace.jsonl");
        let tel = JsonlTelemetry::open(&path).expect("open with nested dirs");
        tel.event("boot", vec![]);
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
