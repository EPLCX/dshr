//! `export const on = { "tool/result": fn, "todo/*": fn, "*": fn }`：订阅会话事件。
//!
//! 事件写进日志的那一刻，宿主在 agent 循环的线程上把它排进插件的队列，**不等**脚本处理：每个插件最多排
//! [`MAX_PENDING`] 件，满了就丢（插件日志里记一笔）。脚本的处理函数可以读工作区（有读能力时）、联网（有 `net:` 时）、
//! 读写自己的数据目录、刷新页面；不能写工作区、不能启动程序、不能调模型、不能再写会话事件。
//!
//! 能收到哪些事件：自己命名空间（`<插件 id>/…`）里的不需要能力；其余的要 `dshr:sessions:read`
//! （和读全部会话同一级）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use dshr_agent::{EventObserver, ObservedSession};
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
            permits: Permits { write: false, net: !capabilities.net.is_empty(), exec: false, llm: false, act: false },
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
        let data = serde_json::to_value(&event.body).ok().and_then(|value| value.get("data").cloned()).unwrap_or(Value::Null);
        let text = data.to_string();
        let data = if text.chars().count() > MAX_DATA_CHARS {
            Value::String(format!("{}… [truncated]", text.chars().take(MAX_DATA_CHARS).collect::<String>()))
        } else {
            data
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
