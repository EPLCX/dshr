//! 插件之间互相调用：提供方导出 `api`，调用方在清单里写 `uses = ["<id>"]`（派生能力 `plugins:call:<id>`，
//! 要用户允许），然后 `await host.plugins.call("<id>", "method", args)`。
//!
//! - 被调用方能做的事 = 调用方这次调用的场合许可 ∩ 被调用方自己的能力：借别的插件不能做到自己本来做不到的事
//!   （比如只读的工具借一个能写文件的插件去写）；而“A 能联网 + B 能读文件”这种组合要用户先允许 A 调用 B。
//! - 工作区沿用调用方的；时限是调用方这次调用剩下的时间。只传 JSON，结果最多 2 MB。
//! - 防死锁：同一个插件的调用是排队执行的，A→B→A 会互相等到超时。调用链随调用传下去（`ToolContext::options`
//!   里的 [`CallChain`]），出现环时立刻报错；链最长 [`MAX_DEPTH`]。

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use dshr_context::{ExtensionPoint, Scope};
use serde_json::{Value, json};

use super::host::{Permits, State};
use super::runtime::{Instance, Job};
use crate::manifest::Capabilities;

/// 调用链最长几层（A→B→C→D）。
const MAX_DEPTH: usize = 4;
/// 结果最多多大。
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;

/// 一个导出了 `api` 的、激活着的插件版本。
pub(super) struct ApiEntry {
    pub instance: Arc<Instance>,
    pub capabilities: Capabilities,
}

/// 扩展点：只用来让 [`ApiEntry`] 跟着插件的 Scope 活着（插件停用、换版本时随之撤销）。
pub(super) struct ApiProviders;

impl ExtensionPoint for ApiProviders {
    type Item = Arc<ApiEntry>;
}

/// 插件 id → 它激活着的版本（后装的在后面）。
static REGISTRY: LazyLock<Mutex<HashMap<String, Vec<Weak<ApiEntry>>>>> = LazyLock::new(Mutex::default);

pub(super) fn register(cx: &mut Scope, plugin: &'static str, entry: ApiEntry) {
    let entry = Arc::new(entry);
    let mut registry = REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let versions = registry.entry(plugin.to_owned()).or_default();
    versions.retain(|version| version.strong_count() > 0);
    versions.push(Arc::downgrade(&entry));
    drop(registry);
    cx.contribute::<ApiProviders>(entry);
}

fn lookup(plugin: &str) -> Option<Arc<ApiEntry>> {
    let registry = REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.get(plugin)?.iter().rev().find_map(Weak::upgrade)
}

/// 这次调用经过的插件（最先发起的在前）。
#[derive(Debug, Clone, Default)]
pub(crate) struct CallChain(pub Vec<String>);

impl State {
    /// `host.plugins.call(id, method, args)`：结果是被调用方返回的 JSON。
    pub(super) fn plugins_api(&self, target: &str, method: &str, args: &str) -> Result<u64, String> {
        if !self.capabilities.plugins_call.iter().any(|allowed| allowed == target) {
            return Err(format!("this plugin may not call {target}: add `uses = [\"{target}\"]` to plugin.toml"));
        }
        let args: Value = serde_json::from_str(args).map_err(|error| format!("invalid arguments: {error}"))?;
        let (ctx, permits) = self.with_call_pub(|call| {
            let mut chain = call.ctx.options.get::<CallChain>().cloned().unwrap_or_default();
            if chain.0.is_empty() {
                chain.0.push(self.plugin.to_owned());
            }
            if chain.0.iter().any(|plugin| plugin == target) {
                return Err(format!("circular plugin call: {} → {target}", chain.0.join(" → ")));
            }
            if chain.0.len() >= MAX_DEPTH {
                return Err(format!("plugin calls are nested too deeply ({} → {target})", chain.0.join(" → ")));
            }
            chain.0.push(target.to_owned());
            let mut ctx = call.ctx.clone();
            ctx.options.insert(chain);
            Ok((ctx, call.permits))
        })?;
        let entry =
            lookup(target).ok_or_else(|| format!("the plugin {target} is not active (not installed, disabled or failed to load)"))?;
        if !entry.instance.exports.api.iter().any(|name| name == method) {
            return Err(format!("the plugin {target} has no api method {method}"));
        }
        let permits = intersect(permits, &entry.capabilities);
        let timeout = self.remaining();
        let job = Job::Dispatch { kind: "api", name: method.to_owned(), payload: json!({ "args": args }).to_string() };
        let (target, method) = (target.to_owned(), method.to_owned());
        let work = async move {
            let result =
                entry.instance.call(job, Some(ctx), timeout, permits).await.map_err(|error| format!("{target}.{method}: {error}"))?;
            let text = result.unwrap_or_else(|| "null".into());
            if text.len() > MAX_RESULT_BYTES {
                return Err(format!("{target}.{method} returned more than {} MB", MAX_RESULT_BYTES / 1024 / 1024));
            }
            Ok(text)
        };
        self.ops.start(work, timeout)
    }
}

/// 被调用方的许可：调用方这次调用的场合许可，再按被调用方自己的能力收紧。
fn intersect(permits: Permits, callee: &Capabilities) -> Permits {
    Permits {
        write: permits.write && callee.fs_write,
        net: permits.net && !callee.net.is_empty(),
        exec: permits.exec && !callee.exec.is_empty(),
        llm: permits.llm && callee.llm,
        act: permits.act && callee.acts_for_user(),
    }
}
