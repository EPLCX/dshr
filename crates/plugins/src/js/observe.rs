//! `export const on = { "tool/result": fn, "todo/*": fn, "*": fn }`：订阅会话事件。
//!
//! 事件写进日志的那一刻，宿主在 agent 循环的线程上把它排进插件的队列，**不等**脚本处理：每个插件最多排
//! [`MAX_PENDING`] 件，满了就丢（插件日志里记一笔）。脚本的处理函数可以读工作区（有读能力时）、联网（有 `net:` 时）、
//! 读写自己的数据目录、刷新页面；不能写工作区、不能启动程序、不能调模型、不能再写会话事件。
//!
//! 能收到哪些事件：自己命名空间（`<插件 id>/…`）里的不需要能力；其余的要 `dshr:sessions:read`
//! （和读全部会话同一级）。
//!
//! 模型的流式输出（`stream/text`、`stream/thinking`、`stream/end`）不进日志，由 [`JsStreamObserver`] 单独转交：
//! 要 `session:read`（或 `dshr:sessions:read`），只交给明确订阅了 `stream/…` 的处理函数（`*` 收不到）。
//! 增量按会话攒起来，每 [`STREAM_FLUSH`] 或攒够 [`STREAM_CHUNK_CHARS`] 个字符交一次，免得逐字排满队列。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dshr_agent::{EventObserver, ObservedSession, StreamChunk, StreamObserver};
use dshr_core::SessionEvent;
use dshr_tools::ToolContext;
use serde_json::{Value, json};

use super::host::Permits;
use super::runtime::{Command, Instance, Job, Pending, Reply};
use crate::manifest::PluginManifest;

/// 每个插件最多排这么多件没处理的事件通知。
const MAX_PENDING: usize = 256;
/// 一条事件交给脚本的数据最多这么多字符。
const MAX_DATA_CHARS: usize = 16 * 1024;
/// 流式输出攒多久交一次、攒到多少字符马上交。
const STREAM_FLUSH: Duration = Duration::from_millis(100);
const STREAM_CHUNK_CHARS: usize = 4 * 1024;

pub(super) struct JsObserver {
    plugin: &'static str,
    instance: Arc<Instance>,
    patterns: Vec<String>,
    /// 能收别的插件、内核的事件（`dshr:sessions:read`）。
    all: bool,
    timeout: Duration,
    permits: Permits,
    pending: Arc<AtomicUsize>,
    dropped: AtomicU64,
}

impl JsObserver {
    pub fn new(plugin: &'static str, manifest: &PluginManifest, timeout: Duration, instance: Arc<Instance>) -> Self {
        let capabilities = &manifest.capabilities;
        Self {
            plugin,
            patterns: instance.exports.on.clone(),
            instance,
            all: capabilities.sessions_read,
            timeout,
            // 只读：能读工作区（按能力）、联网（按能力）；不写、不启动程序、不调模型、不替用户做事。
            permits: Permits { write: false, net: !capabilities.net.is_empty(), exec: false, llm: false, act: false, config: false },
            pending: Arc::default(),
            dropped: AtomicU64::new(0),
        }
    }

    fn wants(&self, kind: &str) -> bool {
        let namespace = kind.split('/').next().unwrap_or_default();
        if namespace != self.plugin && !self.all {
            return false;
        }
        self.patterns
            .iter()
            .any(|pattern| pattern == "*" || pattern == kind || pattern.strip_suffix("/*").is_some_and(|prefix| prefix == namespace))
    }
}

impl EventObserver for JsObserver {
    fn observe(&self, session: &ObservedSession<'_>, event: &SessionEvent) {
        let kind = event.body.kind();
        if !self.wants(kind) {
            return;
        }
        if self.pending.fetch_add(1, Ordering::Relaxed) >= MAX_PENDING {
            self.pending.fetch_sub(1, Ordering::Relaxed);
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped.is_power_of_two() {
                tracing::warn!(plugin = self.plugin, dropped, "插件处理会话事件跟不上，丢掉了一些通知");
            }
            return;
        }
        // 取出 `data`（不再复制一份），超长时换成截断的文本：大事件（图片、整份文件）只序列化到上限为止。
        let data =
            serde_json::to_value(&event.body).ok().and_then(|mut value| value.get_mut("data").map(Value::take)).unwrap_or(Value::Null);
        let data = match super::hooks::clipped_json(&data, MAX_DATA_CHARS) {
            Some(clipped) => Value::String(clipped),
            None => data,
        };
        let agent = session.log_path.parent().and_then(|dir| dir.file_name()).is_some_and(|name| name == "agents");
        let payload = json!({
            "type": kind,
            "session": session.id,
            "agent": agent.then_some(session.id),
            "seq": event.seq,
            "time": event.time,
            "data": data,
        });
        let ctx = session.workspace.map(|workspace| ToolContext::new(workspace.to_owned()));
        let command = Command {
            job: Job::Dispatch { kind: "on", name: kind.to_string(), payload: payload.to_string() },
            ctx,
            permits: self.permits,
            timeout: self.timeout,
            cancelled: Arc::default(),
            reply: Reply::Done(Pending(self.pending.clone())),
        };
        // 线程已经停了（插件刚被换下）：命令被丢掉，计数随之减回去。
        let _ = self.instance.send(command);
    }
}

/// 订阅了 `stream/…` 的插件：把模型的流式增量合并后交给 `on` 的处理函数。
pub(super) struct JsStreamObserver {
    plugin: &'static str,
    instance: Arc<Instance>,
    timeout: Duration,
    permits: Permits,
    pending: Arc<AtomicUsize>,
    dropped: AtomicU64,
    /// 每个会话还没交出去的那一段：类型（`text` / `thinking`）、文字、开始攒的时间。
    buffers: Mutex<HashMap<String, StreamBuffer>>,
}

struct StreamBuffer {
    kind: &'static str,
    text: String,
    since: Instant,
}

impl JsStreamObserver {
    /// 插件订阅了流式输出、也有读会话的能力时才有。
    pub fn new(plugin: &'static str, manifest: &PluginManifest, timeout: Duration, instance: Arc<Instance>) -> Option<Self> {
        let capabilities = &manifest.capabilities;
        let wants = instance.exports.on.iter().any(|pattern| pattern.starts_with("stream/"));
        if !wants {
            return None;
        }
        if !(capabilities.session_read || capabilities.sessions_read) {
            tracing::warn!(plugin, "插件订阅了流式输出，但没有声明 session:read，收不到");
            return None;
        }
        Some(Self {
            plugin,
            instance,
            timeout,
            permits: Permits { write: false, net: !capabilities.net.is_empty(), exec: false, llm: false, act: false, config: false },
            pending: Arc::default(),
            dropped: AtomicU64::new(0),
            buffers: Mutex::default(),
        })
    }

    fn wants(&self, kind: &str) -> bool {
        self.instance.exports.on.iter().any(|pattern| pattern == "stream/*" || pattern == kind)
    }

    fn dispatch(&self, session: &ObservedSession<'_>, kind: &str, text: Option<String>) {
        if !self.wants(kind) {
            return;
        }
        if self.pending.fetch_add(1, Ordering::Relaxed) >= MAX_PENDING {
            self.pending.fetch_sub(1, Ordering::Relaxed);
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped.is_power_of_two() {
                tracing::warn!(plugin = self.plugin, dropped, "插件处理流式输出跟不上，丢掉了一些增量");
            }
            return;
        }
        let agent = session.log_path.parent().and_then(|dir| dir.file_name()).is_some_and(|name| name == "agents");
        let mut payload = json!({
            "type": kind,
            "stream": true,
            "session": session.id,
            "agent": agent.then_some(session.id),
        });
        if let Some(text) = text {
            payload["text"] = Value::String(text);
        }
        let command = Command {
            job: Job::Dispatch { kind: "on", name: kind.to_owned(), payload: payload.to_string() },
            ctx: session.workspace.map(|workspace| ToolContext::new(workspace.to_owned())),
            permits: self.permits,
            timeout: self.timeout,
            cancelled: Arc::default(),
            reply: Reply::Done(Pending(self.pending.clone())),
        };
        let _ = self.instance.send(command);
    }

    fn flush(&self, session: &ObservedSession<'_>, buffer: StreamBuffer) {
        if !buffer.text.is_empty() {
            self.dispatch(session, &format!("stream/{}", buffer.kind), Some(buffer.text));
        }
    }
}

impl StreamObserver for JsStreamObserver {
    fn chunk(&self, session: &ObservedSession<'_>, chunk: StreamChunk<'_>) {
        let (kind, text) = match chunk {
            StreamChunk::Text(text) => ("text", text),
            StreamChunk::Thinking(text) => ("thinking", text),
            StreamChunk::End => {
                let left = self.buffers.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(session.id);
                if let Some(buffer) = left {
                    self.flush(session, buffer);
                }
                self.dispatch(session, "stream/end", None);
                return;
            }
        };
        let ready = {
            let mut buffers = self.buffers.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut ready = Vec::new();
            // 换了类型（思考 → 正文）：先把前一段交出去，保持先后顺序。
            if buffers.get(session.id).is_some_and(|buffer| buffer.kind != kind) {
                ready.extend(buffers.remove(session.id));
            }
            let buffer =
                buffers.entry(session.id.to_owned()).or_insert_with(|| StreamBuffer { kind, text: String::new(), since: Instant::now() });
            buffer.text.push_str(text);
            if buffer.since.elapsed() >= STREAM_FLUSH || buffer.text.len() >= STREAM_CHUNK_CHARS {
                ready.extend(buffers.remove(session.id));
            }
            ready
        };
        for buffer in ready {
            self.flush(session, buffer);
        }
    }
}
