//! JS 插件的 `hooks` 与 `schedules` 导出：把脚本挂到 agent 循环的钩子上，以及在后台定时运行。
//!
//! | 导出 | 接到 | 怎么等 | 能做什么 |
//! |---|---|---|---|
//! | `beforeTool` | `GateTool` | 同步，最多 300 ms | 拒绝、要求询问；放行要 `hooks:gate` |
//! | `afterTool` | `AfterTool` | 异步 | 看结果、写事件；改写结果要 `hooks:tool-output` |
//! | `beforeStep` | `BeforeStep` | 异步 | 写事件 |
//! | `beforeRequest` | `BeforeRequest` | 同步，最多 300 ms | 往系统提示词后面追加文本、往消息末尾补一段 user 文本 |
//! | `turnEnded` | `TurnEnded` | 异步 | 写事件、写数据、联网、调模型 |
//! | `turnStarted` | `TurnStarted` | 异步，最多 5 秒 | 拦下这条消息、补事件 |
//! | `beforeWrite` | `WriteIntent` | 同步，最多 1 秒 | 拒绝写入；改写内容要 `hooks:write` |
//!
//! “同步”指 agent 循环在原地等结果：钩子本身照样可以是 async 函数、`await` 宿主操作（读写数据目录、`host.db`），
//! 只要在时限内落定。所有钩子和工具跑在插件的同一个运行时里，模块变量彼此共享。
//! 每个钩子的参数里都有 `session`（顶层会话 id）和 `agent`（`main` 或子代理名），插件据此按会话、按代理区分状态。
//!
//! 钩子都不能写工作区、不能启动程序：这些要经过权限把关，而钩子不在把关的范围里。
//! 超时、抛异常都只当作“没有结论”，记进插件日志，对话照常进行。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use dshr_agent::{
    AfterTool, AfterToolCall, AfterToolHook, AgentConfig, AgentError, AgentIdentity, BeforeRequest, BeforeStep, BeforeStepHook, Gate,
    GateRequest, GateTool, ModelChoices, ReviewTool, ReviewToolHook, TransformRequest, TransformRequestHook, TurnContext, TurnEnded,
    TurnEndedHook, TurnInput, TurnOutcome, TurnStart, TurnStarted, TurnStartedHook,
};
use dshr_context::{ExtensionPoint, Scope};
use dshr_llm::ChatRequest;
use dshr_tools::{Access, EventSink, ToolContext, ToolOutput, WriteDecision, WriteIntent, WriteRequest};
use serde_json::{Value, json};

use super::host::Permits;
use super::runtime::{Instance, Job};
use crate::manifest::PluginManifest;

/// 同步钩子（`beforeTool`、`beforeRequest`）最多等这么久：它们卡在每次工具调用、每次请求的路上。
const SYNC_WAIT: Duration = Duration::from_millis(300);
/// 钩子里交给脚本的工具输入、输出最多这么多字符。
const MAX_PAYLOAD_CHARS: usize = 16 * 1024;
/// `beforeRequest` 追加的文本（系统提示词、user 消息）各自最多这么多字节。
const MAX_APPEND_BYTES: usize = 8 * 1024;
/// 排在预设（-100）之后、权限（0）之前：脚本能在内置权限放行之前拒绝，但预设的只读规则先生效。
const GATE_PRIORITY: i32 = -50;
/// 插件提供的权限模式：排在 `beforeTool`（-50）之后、内置权限（0）之前。选中它时由它给结论。
const PERMISSION_PRIORITY: i32 = -10;
/// 其余钩子排在内置的后面。
const HOOK_PRIORITY: i32 = 100;
/// `approve` 最多等这么久：插件可以把审批转到别处（手机、IM），等人回答。
const APPROVE_WAIT: Duration = Duration::from_secs(600);
/// `reviewTool` 最多等这么久（可以调模型、起子代理）；`transformRequest` 最多等这么久。
const REVIEW_WAIT: Duration = Duration::from_secs(20);
const TRANSFORM_WAIT: Duration = Duration::from_secs(5);
/// `turnStarted` 最多等这么久：用户在等回复。
const TURN_STARTED_WAIT: Duration = Duration::from_secs(5);
/// `beforeWrite` 最多等这么久：它卡在每次写文件的路上。
const WRITE_WAIT: Duration = Duration::from_secs(1);

/// 扩展点：插件在某个 `Context` 里激活着的记号。条目被撤销（插件停用、换版本、卸掉）时计数减一，
/// 后台任务据此知道这个版本还在不在用。
pub(super) struct Activity;

impl ExtensionPoint for Activity {
    type Item = Arc<ActiveGuard>;
}

pub(super) struct ActiveGuard(Arc<AtomicUsize>);

impl ActiveGuard {
    pub fn new(count: Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Self(count)
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 把模块导出的钩子登记到 `cx` 上。
pub(super) fn register(cx: &mut Scope, plugin: &'static str, manifest: &PluginManifest, timeout: Duration, instance: &Arc<Instance>) {
    let capabilities = &manifest.capabilities;
    let js = Arc::new(JsHooks {
        plugin,
        instance: instance.clone(),
        timeout,
        may_allow: capabilities.hooks_gate,
        may_rewrite: capabilities.hooks_tool_output,
        may_review: capabilities.hooks_review,
        may_compact: capabilities.context_compact,
        may_rewrite_request: capabilities.context_rewrite,
        may_route: capabilities.model_route,
        may_rewrite_writes: capabilities.hooks_write,
        // 异步钩子可以联网、调模型（按插件的能力），不能写工作区、不能启动程序。
        // 异步钩子还可以替用户做事（发消息、起子代理），按插件的能力。
        permits: Permits {
            write: false,
            exec: false,
            net: !capabilities.net.is_empty(),
            llm: capabilities.llm,
            act: capabilities.acts_for_user(),
        },
    });
    for name in &instance.exports.hooks {
        match name.as_str() {
            "beforeTool" => {
                let js = js.clone();
                cx.hook::<GateTool>(GATE_PRIORITY, Arc::new(move |request: &GateRequest<'_>| js.gate(request)));
            }
            "beforeRequest" => {
                let js = js.clone();
                cx.hook::<BeforeRequest>(
                    HOOK_PRIORITY,
                    Arc::new(move |request: &mut ChatRequest, config: &AgentConfig, ctx: &ToolContext| {
                        js.before_request(request, config, ctx)
                    }),
                );
            }
            "afterTool" => cx.hook::<AfterTool>(HOOK_PRIORITY, js.clone()),
            "beforeStep" => cx.hook::<BeforeStep>(HOOK_PRIORITY, js.clone()),
            "turnEnded" => cx.hook::<TurnEnded>(HOOK_PRIORITY, js.clone()),
            "reviewTool" => cx.hook::<ReviewTool>(HOOK_PRIORITY, js.clone()),
            "transformRequest" => cx.hook::<TransformRequest>(HOOK_PRIORITY, js.clone()),
            "turnStarted" => cx.hook::<TurnStarted>(HOOK_PRIORITY, js.clone()),
            "approve" if capabilities.permissions_approve => cx.hook::<dshr_agent::ApproveTool>(HOOK_PRIORITY, js.clone()),
            "beforeWrite" => {
                let js = js.clone();
                cx.hook::<WriteIntent>(HOOK_PRIORITY, Arc::new(move |request: &WriteRequest<'_>| js.before_write(request)));
            }
            _ => {}
        }
    }
    // 插件提供的权限模式：选中时每次工具调用由 `decide` 下结论（同步，和 `beforeTool` 一样等 300 ms）。
    if capabilities.permissions_mode {
        for decl in &instance.exports.permissions {
            let (js, id) = (js.clone(), decl.id.clone());
            cx.hook::<GateTool>(PERMISSION_PRIORITY, Arc::new(move |request: &GateRequest<'_>| js.decide(&id, request)));
        }
    }
}

struct JsHooks {
    plugin: &'static str,
    instance: Arc<Instance>,
    timeout: Duration,
    may_allow: bool,
    may_rewrite: bool,
    may_review: bool,
    may_compact: bool,
    may_rewrite_request: bool,
    may_route: bool,
    may_rewrite_writes: bool,
    permits: Permits,
}

/// 钩子返回的结论：`"allow"` / `"deny"` / `"ask"`，或 `{ decision, reason }`。
fn decision(answer: &Value) -> Option<(&str, Option<String>)> {
    match answer {
        Value::String(decision) => Some((decision.as_str(), None)),
        Value::Object(object) => Some((
            object.get("decision").and_then(Value::as_str).unwrap_or_default(),
            object.get("reason").and_then(Value::as_str).map(str::to_owned),
        )),
        _ => None,
    }
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_PAYLOAD_CHARS) {
        Some((cut, _)) => format!("{}… [truncated]", &text[..cut]),
        None => text.to_owned(),
    }
}

fn clip_value(value: &Value) -> Value {
    match clipped_json(value, MAX_PAYLOAD_CHARS) {
        Some(clipped) => Value::String(clipped),
        None => value.clone(),
    }
}

/// `value` 的 JSON 文本超过 `max_chars` 个字符时，返回截断后的文本（末尾带 `… [truncated]`）；没超过时返回 `None`。
///
/// 序列化写到一个有上限的缓冲区里，写满就停：几 MB 的参数（整份文件、图片）不用先完整序列化一遍再数字符。
pub(super) fn clipped_json(value: &Value, max_chars: usize) -> Option<String> {
    struct Capped {
        buf: Vec<u8>,
        cap: usize,
    }
    impl std::io::Write for Capped {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let room = self.cap - self.buf.len();
            if data.len() > room {
                self.buf.extend_from_slice(&data[..room]);
                return Err(std::io::Error::other("full"));
            }
            self.buf.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // 一个字符最多 4 个字节：写满时缓冲区里一定有超过 `max_chars` 个字符。
    let mut capped = Capped { buf: Vec::new(), cap: max_chars * 4 + 4 };
    let complete = serde_json::to_writer(&mut capped, value).is_ok();
    if complete && capped.buf.len() <= max_chars {
        return None;
    }
    // 写满时最后一个字符可能被切开，有损转换只影响截断点之后的部分。
    let text = String::from_utf8_lossy(&capped.buf);
    text.char_indices().nth(max_chars).map(|(cut, _)| format!("{}… [truncated]", &text[..cut]))
}

/// 给钩子的参数补上 `session`（顶层会话 id）和 `agent`（`main` 或子代理名）。
fn with_identity(mut payload: Value, options: &dshr_context::Options, session: Option<&str>) -> Value {
    let identity = AgentIdentity::of(options, session);
    if let Value::Object(object) = &mut payload {
        object.insert("session".into(), Value::String(identity.session));
        object.insert("agent".into(), Value::String(identity.agent));
    }
    payload
}

/// 同步钩子的工具环境：工作区（按能力只读）、选项（会话、身份）和插件快照，不带实时输出和事件通道。
fn sync_context(ctx: &ToolContext) -> ToolContext {
    let mut sync = ToolContext::new(ctx.workspace.clone());
    sync.options = ctx.options.clone();
    sync.plugins = ctx.plugins.clone();
    sync
}

/// 截到 `limit` 字节以内（落在字符边界上）。
fn clip_bytes(text: &str, limit: usize) -> String {
    let mut text = text.to_owned();
    if let Some((cut, _)) = text.char_indices().find(|(index, _)| *index >= limit) {
        text.truncate(cut);
    }
    text
}

fn access_json(access: &Access) -> Value {
    match access {
        Access::Read => json!({ "type": "read" }),
        // 仍然是 `read`（按类型判断的钩子照旧认得），多一个路径和 `outside` 标记。
        Access::ReadOutside(path) => json!({ "type": "read", "path": path.display().to_string(), "outside": true }),
        Access::Write(path) => json!({ "type": "write", "path": path.display().to_string() }),
        Access::Execute => json!({ "type": "execute" }),
        Access::External => json!({ "type": "external" }),
    }
}

/// 钩子写的事件先攒着，钩子返回后由 `TurnContext::append_ext` 写进日志。
#[derive(Clone, Default)]
struct Collected(Arc<Mutex<Vec<(String, Value)>>>);

impl Collected {
    fn sink(&self) -> EventSink {
        let events = self.0.clone();
        EventSink(Arc::new(move |kind, data| events.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((kind, data))))
    }

    fn write(&self, plugin: &str, turn: &mut TurnContext<'_>) {
        let events = std::mem::take(&mut *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        for (kind, data) in events {
            if let Err(error) = turn.append_ext(&kind, data) {
                tracing::warn!(plugin, %error, "钩子写的事件没能写进日志");
            }
        }
    }
}

impl JsHooks {
    fn hook(name: &str, payload: Value) -> Job {
        Job::Dispatch { kind: "hook", name: name.to_owned(), payload: payload.to_string() }
    }

    fn answer(&self, name: &str, result: Result<Option<String>, String>) -> Value {
        match result {
            Ok(text) => text.and_then(|text| serde_json::from_str(&text).ok()).unwrap_or(Value::Null),
            Err(error) => {
                tracing::warn!(plugin = self.plugin, hook = name, error, "插件的钩子没有给出结果");
                Value::Null
            }
        }
    }

    /// `beforeTool`：返回 `"deny"`、`"ask"`、`"allow"`，或 `{ decision, reason }`；其余（含没有返回）是没有结论。
    fn gate(&self, request: &GateRequest<'_>) -> Option<Gate> {
        let payload = json!({ "tool": request.tool, "input": clip_value(request.input), "access": access_json(request.access) });
        let payload = with_identity(payload, &request.ctx.options, None);
        // 只读：能读工作区（按能力）、读写数据目录，不能写工作区、不能联网。
        let ctx = sync_context(request.ctx);
        let answer = self.answer("beforeTool", self.instance.ask(Self::hook("beforeTool", payload), Some(ctx), SYNC_WAIT));
        let (decision, reason) = decision(&answer)?;
        match decision {
            "deny" => Some(Gate::Deny(reason.unwrap_or_else(|| format!("Blocked by the {} plugin.", self.plugin)))),
            // “询问”不能比内置的权限更松：内置的会拒绝时仍然拒绝。
            "ask" => match builtin_gate(request) {
                Gate::Deny(reason) => Some(Gate::Deny(reason)),
                _ => Some(Gate::Ask),
            },
            "allow" if self.may_allow => Some(Gate::Allow),
            "allow" => {
                tracing::warn!(plugin = self.plugin, "beforeTool 想直接放行，但插件没有声明 `hooks:gate` 能力，忽略");
                None
            }
            _ => None,
        }
    }

    /// 插件权限模式 `id` 的 `decide`：只在这个模式被选中（且插件还在）时调用。返回 `"allow"`、`"deny"`、`"ask"`，
    /// 或 `{ decision, reason }`（`ask` 的 `reason` 附在审批处）；其余是没有结论，按手动确认处理。
    fn decide(&self, id: &str, request: &GateRequest<'_>) -> Option<Gate> {
        let chosen = dshr_agent::PluginPermission::active(&request.ctx.options, &request.ctx.plugins)?;
        if chosen.plugin != self.plugin || chosen.id != id {
            return None;
        }
        let payload = json!({ "tool": request.tool, "input": clip_value(request.input), "access": access_json(request.access) });
        let payload = with_identity(payload, &request.ctx.options, None);
        let job = Job::Dispatch { kind: "permission", name: id.to_owned(), payload: payload.to_string() };
        let answer = self.answer("permissions.decide", self.instance.ask(job, Some(sync_context(request.ctx)), SYNC_WAIT));
        let (decision, reason) = decision(&answer)?;
        match decision {
            "allow" => Some(Gate::Allow),
            "deny" => Some(Gate::Deny(reason.unwrap_or_else(|| format!("Blocked by the {} plugin's permission mode.", self.plugin)))),
            "ask" => Some(reason.map_or(Gate::Ask, Gate::Confirm)),
            _ => None,
        }
    }

    /// `beforeRequest`：返回的文本追加到这次请求的系统提示词后面（不进日志）；返回 `{ append, user, model, effort }` 时
    /// `user` 作为一段 user 文本接在消息末尾（同样只在这次请求里），还可以换模型和思考等级（要 `model:route`，
    /// 模型只能是主程序放进 [`ModelChoices`] 的那些）。
    fn before_request(&self, request: &mut ChatRequest, config: &AgentConfig, ctx: &ToolContext) {
        let effort = request.effort.map(|effort| effort.as_str());
        let payload = json!({
            "model": request.model,
            "effort": effort,
            "messages": request.messages.len(),
            "tools": request.tools.len(),
            "workspace": ctx.workspace.display().to_string(),
        });
        let payload = with_identity(payload, &ctx.options, None);
        let answer =
            self.answer("beforeRequest", self.instance.ask(Self::hook("beforeRequest", payload), Some(sync_context(ctx)), SYNC_WAIT));
        let (append, user) = match &answer {
            Value::Object(object) => {
                self.route(request, config, object);
                (object.get("append").and_then(Value::as_str), object.get("user").and_then(Value::as_str))
            }
            other => (other.as_str(), None),
        };
        if let Some(text) = append.map(str::trim).filter(|text| !text.is_empty()) {
            let text = clip_bytes(text, MAX_APPEND_BYTES);
            request.system = Some(match request.system.take() {
                Some(system) => format!("{system}\n\n{text}"),
                None => text,
            });
        }
        if let Some(text) = user.map(str::trim).filter(|text| !text.is_empty()) {
            append_user_text(&mut request.messages, clip_bytes(text, MAX_APPEND_BYTES));
        }
    }

    fn route(&self, request: &mut ChatRequest, config: &AgentConfig, answer: &serde_json::Map<String, Value>) {
        let model = answer.get("model").and_then(Value::as_str);
        let effort = answer.get("effort").and_then(Value::as_str);
        if model.is_none() && effort.is_none() {
            return;
        }
        if !self.may_route {
            tracing::warn!(plugin = self.plugin, "beforeRequest 想换模型，但插件没有声明 `model:route` 能力，忽略");
            return;
        }
        if let Some(model) = model {
            let allowed = config.options.get::<ModelChoices>().is_some_and(|choices| choices.0.iter().any(|choice| choice == model));
            if allowed {
                request.model = model.to_owned();
            } else {
                tracing::warn!(plugin = self.plugin, model, "插件要换的模型不在当前提供商的模型目录里，忽略");
            }
        }
        if let Some(effort) = effort {
            match dshr_llm::ReasoningEffort::ALL.into_iter().find(|known| known.as_str() == effort) {
                Some(effort) => request.effort = Some(effort),
                None => tracing::warn!(plugin = self.plugin, effort, "插件给的思考等级不认识，忽略"),
            }
        }
    }

    /// `beforeWrite`：`{ decision: "deny", reason }`（或 `"deny"`）拒绝；`{ content }` 改写（要 `hooks:write`）。
    fn before_write(&self, request: &WriteRequest<'_>) -> WriteDecision {
        let payload = json!({ "tool": request.tool, "path": request.path, "before": request.before, "after": request.after });
        let payload = with_identity(payload, &request.ctx.options, None);
        // 只读：能读工作区（按能力）、读写数据目录，不能写工作区、不能联网。
        let ctx = sync_context(request.ctx);
        let answer = self.answer("beforeWrite", self.instance.ask(Self::hook("beforeWrite", payload), Some(ctx), WRITE_WAIT));
        if let Some(("deny", reason)) = decision(&answer) {
            return WriteDecision::Deny(reason.unwrap_or_else(|| format!("Blocked by the {} plugin.", self.plugin)));
        }
        match answer.get("content").and_then(Value::as_str) {
            Some(content) if content != request.after => {
                if self.may_rewrite_writes {
                    WriteDecision::Rewrite { content: content.to_owned(), by: self.plugin.to_owned() }
                } else {
                    tracing::warn!(plugin = self.plugin, "beforeWrite 想改写内容，但插件没有声明 `hooks:write` 能力，忽略");
                    WriteDecision::Keep
                }
            }
            _ => WriteDecision::Keep,
        }
    }

    /// 异步钩子的公共部分：带上会话、模型，收集写的事件。
    async fn run(&self, name: &str, payload: Value, ctx: ToolContext) -> Value {
        let job = Self::hook(name, payload);
        self.answer(name, self.instance.call(job, Some(ctx), self.timeout, self.permits).await)
    }
}

#[async_trait::async_trait]
impl ReviewToolHook for JsHooks {
    /// `reviewTool`：本来要询问用户时先问插件。超时、出错、没有结论都照常询问；放行要 `hooks:review`。
    async fn review(&self, request: &GateRequest<'_>) -> Option<Gate> {
        let payload = json!({ "tool": request.tool, "input": clip_value(request.input), "access": access_json(request.access) });
        let payload = with_identity(payload, &request.ctx.options, None);
        let job = Self::hook("reviewTool", payload);
        let answer = self.answer("reviewTool", self.instance.call(job, Some(request.ctx.clone()), REVIEW_WAIT, self.permits).await);
        let (decision, reason) = decision(&answer)?;
        let gate = match decision {
            "deny" => Gate::Deny(reason.clone().unwrap_or_else(|| format!("Blocked by the {} plugin's review.", self.plugin))),
            "allow" if self.may_review => Gate::Allow,
            "allow" => {
                tracing::warn!(plugin = self.plugin, "reviewTool 想直接放行，但插件没有声明 `hooks:review` 能力，照常询问");
                return None;
            }
            _ => return None,
        };
        // 审查结论写进日志，对话区里看得到、事后能查。
        request
            .ctx
            .emit_event("plugins/reviewed", json!({ "plugin": self.plugin, "tool": request.tool, "decision": decision, "reason": reason }));
        Some(gate)
    }
}

#[async_trait::async_trait]
impl dshr_agent::ApproveToolHook for JsHooks {
    /// `approve`：和界面上的审批同时等，返回 `"allow"` / `"deny"`（或 `{ decision, reason }`）就以它为准；
    /// 其余（含没有返回、超时）表示放弃，继续等界面。结论写进日志（`plugins/approved`）。
    async fn approve(&self, request: &GateRequest<'_>, note: Option<&str>) -> Option<dshr_agent::Decision> {
        let payload = json!({
            "tool": request.tool,
            "input": clip_value(request.input),
            "access": access_json(request.access),
            "note": note,
        });
        let payload = with_identity(payload, &request.ctx.options, None);
        let job = Self::hook("approve", payload);
        let answer = self.answer("approve", self.instance.call(job, Some(request.ctx.clone()), APPROVE_WAIT, self.permits).await);
        let (decision, reason) = decision(&answer)?;
        let verdict = match decision {
            "allow" => dshr_agent::Decision::Allow,
            "deny" => dshr_agent::Decision::Deny,
            _ => return None,
        };
        request
            .ctx
            .emit_event("plugins/approved", json!({ "plugin": self.plugin, "tool": request.tool, "decision": decision, "reason": reason }));
        Some(verdict)
    }
}

#[async_trait::async_trait]
impl TransformRequestHook for JsHooks {
    /// `transformRequest`：返回 `{ system?, messages? }` 替换这次请求的系统提示词和消息（要 `context:rewrite`）。
    async fn transform(&self, request: &mut ChatRequest, ctx: &ToolContext) {
        if !self.may_rewrite_request {
            return;
        }
        let payload = json!({ "model": request.model, "system": request.system, "messages": request.messages });
        let payload = with_identity(payload, &ctx.options, None);
        let job = Self::hook("transformRequest", payload);
        let mut answer = self.answer("transformRequest", self.instance.call(job, Some(ctx.clone()), TRANSFORM_WAIT, self.permits).await);
        if let Some(system) = answer.get("system") {
            request.system = system.as_str().map(str::to_owned);
        }
        if let Some(messages) = answer.get_mut("messages").map(Value::take) {
            match serde_json::from_value::<Vec<dshr_core::Message>>(messages) {
                Ok(messages) if !messages.is_empty() => request.messages = messages,
                Ok(_) => tracing::warn!(plugin = self.plugin, "transformRequest 返回了空的消息列表，忽略"),
                Err(error) => tracing::warn!(plugin = self.plugin, %error, "transformRequest 返回的消息格式不对，忽略"),
            }
        }
    }
}

/// 把一段 user 文本接在消息末尾：最后一条是 user 消息（用户的话、工具结果）时并进去，否则新起一条。
/// 两条 user 消息相邻时有的接口会拒绝请求，所以优先并进去。
fn append_user_text(messages: &mut Vec<dshr_core::Message>, text: String) {
    match messages.last_mut() {
        Some(last) if last.role == dshr_core::Role::User => last.content.push(dshr_core::ContentBlock::text(text)),
        _ => messages.push(dshr_core::Message { role: dshr_core::Role::User, content: vec![dshr_core::ContentBlock::text(text)] }),
    }
}

/// 内置的权限规则（预设的只读模式另算）：`"ask"` 用它来保证不比内置的更松。
fn builtin_gate(request: &GateRequest<'_>) -> Gate {
    let confined = request.ctx.plugins.service::<dyn dshr_sandbox::Sandbox>().is_some_and(|sandbox| sandbox.confines_writes());
    dshr_agent::gate(dshr_agent::effective_mode(&request.ctx.options, &request.ctx.plugins), request.access, request.ctx, confined)
}

#[async_trait::async_trait]
impl AfterToolHook for JsHooks {
    async fn after_tool(&self, call: &AfterToolCall<'_>, output: &mut ToolOutput) {
        let payload = json!({
            "tool": call.tool,
            "input": clip_value(call.input),
            "output": clip(&output.content),
            "isError": output.is_error,
        });
        let payload = with_identity(payload, &call.ctx.options, None);
        // 写的事件直接走这次调用的通道，排在 `tool/result` 之后。
        let answer = self.run("afterTool", payload, call.ctx.clone()).await;
        let replacement = match &answer {
            Value::String(text) => Some((text.clone(), output.is_error)),
            Value::Object(object) => object
                .get("output")
                .and_then(Value::as_str)
                .map(|text| (text.to_owned(), object.get("isError").and_then(Value::as_bool).unwrap_or(output.is_error))),
            _ => None,
        };
        if let Some((content, is_error)) = replacement {
            if self.may_rewrite {
                *output = ToolOutput { content: dshr_tools::truncate(content), is_error };
            } else {
                tracing::warn!(plugin = self.plugin, "afterTool 想改写结果，但插件没有声明 `hooks:tool-output` 能力，忽略");
            }
        }
    }
}

#[async_trait::async_trait]
impl BeforeStepHook for JsHooks {
    async fn before_step(&self, turn: &mut TurnContext<'_>) -> Result<(), AgentError> {
        let events = turn.session().events();
        let (turn_number, step) = events
            .iter()
            .rev()
            .find_map(|event| match event.body {
                dshr_core::EventBody::StepStart { turn, step } => Some((turn, step)),
                _ => None,
            })
            .unwrap_or_default();
        let last_seq = events.last().map_or(0, |event| event.seq);
        let estimated = dshr_agent::estimate_tokens(turn.projection());
        let payload = json!({
            "session": turn.session().id(),
            "turn": turn_number,
            "step": step,
            "events": turn.session().events().len(),
            "lastSeq": last_seq,
            "contextWindow": turn.config.context_window,
            "estimatedTokens": estimated,
        });
        let payload = with_identity(payload, &turn.config.options, Some(turn.session().id()));
        let collected = Collected::default();
        let mut ctx = turn.tool_context();
        ctx.events = Some(collected.sink());
        let answer = self.run("beforeStep", payload, ctx).await;
        collected.write(self.plugin, turn);
        // `{ compact: { summary, uptoSeq } }`：把 `uptoSeq` 及之前的对话换成摘要（要 `context:compact`）。
        if let Some(compact) = answer.get("compact") {
            let summary = compact.get("summary").and_then(Value::as_str).map(str::trim).unwrap_or_default();
            let upto = compact.get("uptoSeq").and_then(Value::as_u64);
            match (self.may_compact, upto) {
                (false, _) => tracing::warn!(plugin = self.plugin, "beforeStep 想压缩上下文，但插件没有声明 `context:compact` 能力，忽略"),
                (true, Some(upto))
                    if !summary.is_empty() && upto <= last_seq && turn.session().events().iter().any(|event| event.seq == upto) =>
                {
                    turn.append(dshr_core::EventBody::Compaction { summary: summary.into(), upto_seq: upto })?;
                }
                _ => tracing::warn!(plugin = self.plugin, "beforeStep 给的压缩不合规（摘要为空，或 uptoSeq 不是日志里已有的事件），忽略"),
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl TurnStartedHook for JsHooks {
    fn name(&self) -> &str {
        self.plugin
    }

    /// `turnStarted`：返回 `{ block: "原因" }` 拦下这条消息；`host.emitEvent` 写的事件排在用户消息之后。
    async fn turn_started(&self, turn: &TurnContext<'_>, input: &TurnInput<'_>) -> TurnStart {
        let payload = json!({
            "session": turn.session().id(),
            "turn": input.turn,
            "text": clip(input.text),
            "attachments": input.attachments,
        });
        let payload = with_identity(payload, &turn.config.options, Some(turn.session().id()));
        let collected = Collected::default();
        let mut ctx = turn.tool_context();
        ctx.events = Some(collected.sink());
        let job = Self::hook("turnStarted", payload);
        let answer = self.answer("turnStarted", self.instance.call(job, Some(ctx), TURN_STARTED_WAIT, self.permits).await);
        let block = match answer.get("block") {
            Some(Value::String(reason)) if !reason.trim().is_empty() => Some(reason.trim().to_owned()),
            Some(Value::Bool(true)) => Some(format!("Blocked by the {} plugin.", self.plugin)),
            _ => None,
        };
        let events = std::mem::take(&mut *collected.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        // 拦下时补的事件也不要：这一轮什么都不写。
        let events = if block.is_some() { Vec::new() } else { events };
        TurnStart { block, events }
    }
}

#[async_trait::async_trait]
impl TurnEndedHook for JsHooks {
    async fn turn_ended(&self, turn: &mut TurnContext<'_>, outcome: &TurnOutcome) -> Result<(), AgentError> {
        let reason = serde_json::to_value(outcome.reason).unwrap_or(Value::Null);
        let payload = json!({
            "session": turn.session().id(),
            "reason": reason,
            "finalText": clip(&outcome.final_text),
            "error": outcome.error,
        });
        let payload = with_identity(payload, &turn.config.options, Some(turn.session().id()));
        let collected = Collected::default();
        let mut ctx = turn.tool_context();
        ctx.events = Some(collected.sink());
        self.run("turnEnded", payload, ctx).await;
        collected.write(self.plugin, turn);
        Ok(())
    }
}

/// 后台任务：每个 `schedules` 条目一个循环，跑在共用的 IO 运行时上。只持有运行时的弱引用：
/// 这个版本被换下、卸掉后循环自己结束；插件被停用（没有处于激活状态的 `Context`）时跳过。
pub(super) fn start_schedules(plugin: &'static str, manifest: &PluginManifest, instance: &Arc<Instance>, active: Arc<AtomicUsize>) {
    let Ok(runtime) = super::ops::io() else {
        tracing::error!(plugin, "没有 IO 运行时，后台任务没有启动");
        return;
    };
    let data_dir = super::surface::data_dir(manifest);
    let permits = Permits {
        write: false,
        exec: false,
        net: !manifest.capabilities.net.is_empty(),
        llm: false,
        act: manifest.capabilities.session_control,
    };
    let timeout = manifest.timeout;
    for schedule in &instance.exports.schedules {
        let weak: Weak<Instance> = Arc::downgrade(instance);
        let (id, every) = (schedule.id.clone(), Duration::from_millis(schedule.every_ms));
        let (active, data_dir) = (active.clone(), data_dir.clone());
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(instance) = weak.upgrade() else { return };
                if active.load(Ordering::Relaxed) == 0 {
                    continue;
                }
                if std::fs::create_dir_all(&data_dir).is_err() {
                    continue;
                }
                // 后台任务没有工作区：文件函数指向插件自己的数据目录。
                let ctx = ToolContext::new(data_dir.clone());
                let job = Job::Dispatch { kind: "schedule", name: id.clone(), payload: "null".into() };
                if let Err(error) = instance.call(job, Some(ctx), timeout, permits).await {
                    tracing::warn!(plugin, schedule = id, error, "插件的后台任务失败");
                }
            }
        });
    }
}

/// 宿主替插件写的事件（`plugins/reviewed`：`reviewTool` 的审查结论）在对话区怎么显示，以及怎么进上下文。
pub(super) struct HostEvents;

impl dshr_context::Plugin for HostEvents {
    fn manifest(&self) -> dshr_context::Manifest {
        dshr_context::Manifest::new("plugin-host-events")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<dshr_agent::EventLabels>(Arc::new(ReviewedLabel));
        cx.contribute::<dshr_agent::EventLabels>(Arc::new(ApprovedLabel));
        Ok(())
    }
}

struct ReviewedLabel;

impl dshr_agent::EventLabel for ReviewedLabel {
    fn kind(&self) -> &str {
        "plugins/reviewed"
    }

    fn label(&self, data: &Value) -> Option<String> {
        let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default();
        let verdict = if text("decision") == "allow" { "放行" } else { "拒绝" };
        let reason = data.get("reason").and_then(Value::as_str).map(|reason| format!("：{reason}")).unwrap_or_default();
        Some(format!("插件 {} 审查后{verdict}了 {}{reason}", text("plugin"), text("tool")))
    }
}

struct ApprovedLabel;

impl dshr_agent::EventLabel for ApprovedLabel {
    fn kind(&self) -> &str {
        "plugins/approved"
    }

    fn label(&self, data: &Value) -> Option<String> {
        let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default();
        let verdict = if text("decision") == "allow" { "批准" } else { "拒绝" };
        let reason = data.get("reason").and_then(Value::as_str).map(|reason| format!("：{reason}")).unwrap_or_default();
        Some(format!("插件 {} 代为{verdict}了 {}{reason}", text("plugin"), text("tool")))
    }
}

#[cfg(test)]
mod tests {
    use super::clipped_json;
    use serde_json::json;

    #[test]
    fn clips_long_json_and_keeps_short_json() {
        assert_eq!(clipped_json(&json!({"a": "xy"}), 10), None);
        assert_eq!(clipped_json(&json!("0123456789"), 12), None);
        assert_eq!(clipped_json(&json!("0123456789"), 5).as_deref(), Some("\"0123… [truncated]"));
        // 多字节字符：按字符数截，不按字节。
        let wide = json!("汉".repeat(100));
        assert_eq!(clipped_json(&wide, 102), None);
        assert_eq!(clipped_json(&wide, 4).as_deref(), Some("\"汉汉汉… [truncated]"));
        let huge = json!({ "text": "x".repeat(10_000_000) });
        assert_eq!(clipped_json(&huge, 8).as_deref(), Some("{\"text\":… [truncated]"));
    }
}
