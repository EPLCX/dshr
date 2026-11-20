//! agent 循环上的钩子。循环本身只管 turn / step、请求与工具调用；
//! 权限、压缩、标题、预设都是挂在这些钩子上的插件。

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use dshr_context::{ExtensionPoint, Hook, Snapshot};
use dshr_core::{EventBody, EventProjector, Projection, SessionEvent, derive_messages_with};
use dshr_llm::{ChatRequest, MessageAccumulator, Provider, StreamEvent};
use dshr_tools::{Access, ToolContext};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{AgentConfig, AgentError, Approver, Gate, LiveEvent, Session, TurnOutcome, collect_text};

/// 扩展点：插件自定义事件（`EventBody::Ext`）怎么投影进模型上下文。一种事件类型登记一个；
/// 写事件用 [`TurnContext::append_ext`]（钩子里）或 `ToolContext::emit_event`（工具里）。
/// 事件在界面上怎么显示是 UI 那一半的事（`dshr_ui::UiScope::transcript_renderer`）。
pub struct EventKinds;

impl ExtensionPoint for EventKinds {
    type Item = Arc<dyn EventProjector>;
}

/// 扩展点：插件自定义事件在对话区显示成的一行纯文本。给画不了界面的插件（JS 插件）用：
/// 界面那一半为这个事件类型登记了 `TranscriptRenderer` 时以它为准，这里的不用。
pub struct EventLabels;

impl ExtensionPoint for EventLabels {
    type Item = Arc<dyn EventLabel>;
}

pub trait EventLabel: Send + Sync {
    /// 负责的事件类型，如 `todo/item`。
    fn kind(&self) -> &str;

    /// 这条事件显示成什么；`None` 表示不显示。桌面版在后台线程上同步调用（不占界面线程），但打开会话时
    /// 每条这类事件都要问一次，要快。
    fn label(&self, data: &Value) -> Option<String>;
}

/// 用户消息进来、还没写进日志时（每轮最先调用）。按顺序问，第一个拦下的生效：拦下时这一轮什么都不写，
/// `run_turn` 返回 [`AgentError::Blocked`]。没拦下时，钩子补的事件排在用户消息和附件之后写入。
pub struct TurnStarted;

impl Hook for TurnStarted {
    type Handler = dyn TurnStartedHook;
}

/// 这一轮的输入。
pub struct TurnInput<'a> {
    /// 这一轮的编号（新会话的第一轮是 1）。
    pub turn: u32,
    pub text: &'a str,
    /// 附件的路径。
    pub attachments: Vec<String>,
}

/// [`TurnStartedHook`] 的结论。
#[derive(Debug, Default)]
pub struct TurnStart {
    /// 拦下这一轮的原因。
    pub block: Option<String>,
    /// 要补进日志的插件事件（类型，数据），排在用户消息之后。
    pub events: Vec<(String, Value)>,
}

#[async_trait::async_trait]
pub trait TurnStartedHook: Send + Sync {
    /// 拦下这一轮的插件叫什么（拦下时显示）。
    fn name(&self) -> &str;

    /// `turn` 里的会话还没有这一轮的任何事件；不要在这里往日志里写东西，要写的放进返回值。
    async fn turn_started(&self, turn: &TurnContext<'_>, input: &TurnInput<'_>) -> TurnStart;
}

/// 扩展点：会话里每写进一条事件就告诉它（插件订阅会话事件）。在写日志的那个线程上同步调用，所以**不能阻塞**：
/// 要做的事排进自己的队列，马上返回。只有 agent 循环里写的事件会通知（界面自己写的界面状态不算）。
pub struct EventObservers;

impl ExtensionPoint for EventObservers {
    type Item = Arc<dyn EventObserver>;
}

pub trait EventObserver: Send + Sync {
    fn observe(&self, session: &ObservedSession<'_>, event: &SessionEvent);
}

/// 写这条事件的会话。
pub struct ObservedSession<'a> {
    pub id: &'a str,
    /// 会话日志（子代理的日志在父会话的 `agents/` 下）。
    pub log_path: &'a std::path::Path,
    /// 这个会话的工作区。
    pub workspace: Option<&'a std::path::Path>,
}

/// 每个 step 发请求之前，可以往会话里写事件（如上下文压缩）。出错时本轮失败。
pub struct BeforeStep;

impl Hook for BeforeStep {
    type Handler = dyn BeforeStepHook;
}

#[async_trait::async_trait]
pub trait BeforeStepHook: Send + Sync {
    async fn before_step(&self, turn: &mut TurnContext<'_>) -> Result<(), AgentError>;
}

/// 请求发出之前改写它（如预设往系统提示词后追加说明）。改动不进日志。
pub struct BeforeRequest;

impl Hook for BeforeRequest {
    type Handler = dyn Fn(&mut ChatRequest, &AgentConfig) + Send + Sync;
}

/// 工具调用执行之前的把关。按顺序问，第一个给出结论的生效；都没有结论时询问用户。
pub struct GateTool;

impl Hook for GateTool {
    type Handler = dyn Fn(&GateRequest<'_>) -> Option<Gate> + Send + Sync;
}

pub struct GateRequest<'a> {
    pub tool: &'a str,
    pub input: &'a Value,
    pub access: &'a Access,
    pub ctx: &'a ToolContext,
}

/// 工具调用本来要询问用户时（[`GateTool`] 链的结论是“询问”），先让这些钩子审查：第一个给出结论的生效，
/// 都没有结论才真的询问。只在“本来要问”时调用，所以不会放松“拒绝”这类结论。可以花几秒（比如让另一个模型判断）。
pub struct ReviewTool;

impl Hook for ReviewTool {
    type Handler = dyn ReviewToolHook;
}

#[async_trait::async_trait]
pub trait ReviewToolHook: Send + Sync {
    async fn review(&self, request: &GateRequest<'_>) -> Option<Gate>;
}

/// 请求发出之前（[`BeforeRequest`] 之后）异步地改写它：压缩、裁剪消息等。改动不进日志。
pub struct TransformRequest;

impl Hook for TransformRequest {
    type Handler = dyn TransformRequestHook;
}

#[async_trait::async_trait]
pub trait TransformRequestHook: Send + Sync {
    /// `ctx` 是这一轮的工具环境（工作区、选项、会话、模型）。
    async fn transform(&self, request: &mut ChatRequest, ctx: &ToolContext);
}

/// [`TurnContext::complete_text`] 的最长等待时间。
pub const COMPLETE_TEXT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 这一轮允许插件切换到的模型（`model:route`），由主程序按当前提供商的模型目录放进 [`AgentConfig::options`]；
/// 没放时插件不能换模型。
#[derive(Debug, Clone, Default)]
pub struct ModelChoices(pub Vec<String>);

/// 工具执行完、结果写进日志之前。可以看到结果，也可以改写它（JS 插件要 `hooks:tool-output` 能力）；
/// 在 `call.ctx` 上写的事件（`emit_event`）照常排在这次调用的 `tool/result` 之后。出错只记日志。
pub struct AfterTool;

impl Hook for AfterTool {
    type Handler = dyn AfterToolHook;
}

pub struct AfterToolCall<'a> {
    pub tool: &'a str,
    pub input: &'a Value,
    pub ctx: &'a ToolContext,
}

#[async_trait::async_trait]
pub trait AfterToolHook: Send + Sync {
    async fn after_tool(&self, call: &AfterToolCall<'_>, output: &mut dshr_tools::ToolOutput);
}

/// 当前会话，放在 [`ToolContext::options`] 里：需要读会话的工具（JS 插件的 `host.session`）用它。
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    /// 会话日志文件。
    pub log_path: PathBuf,
}

/// 当前会话用的模型，放在 [`ToolContext::options`] 里：插件借它发一次性的请求（JS 插件的 `host.llm`）。
#[derive(Clone)]
pub struct LlmAccess {
    pub provider: Arc<dyn Provider>,
    pub model: String,
}

/// 一轮结束（`turn/end` 已写入）之后。出错只记日志，不影响这一轮的结果。
pub struct TurnEnded;

impl Hook for TurnEnded {
    type Handler = dyn TurnEndedHook;
}

#[async_trait::async_trait]
pub trait TurnEndedHook: Send + Sync {
    async fn turn_ended(&self, turn: &mut TurnContext<'_>, outcome: &TurnOutcome) -> Result<(), AgentError>;
}

/// 钩子里能看到、能改的东西：会话、模型、本轮的配置与状态。
pub struct TurnContext<'a> {
    pub(crate) session: &'a mut Session,
    pub(crate) sink: &'a UnboundedSender<LiveEvent>,
    /// 会话的上下文投影，用到时才算；写入事件后作废。
    pub(crate) projection: Option<Projection>,
    /// 这一步（或一轮结束时）的插件快照；投影插件事件要用其中的 [`EventKinds`]。
    pub(crate) plugins: &'a Snapshot,
    pub cancel: &'a CancellationToken,
    /// 工作区。
    pub workspace: &'a std::path::Path,
    pub provider: &'a Arc<dyn Provider>,
    pub config: &'a AgentConfig,
    /// 插件在这一轮里的临时状态。
    pub state: &'a mut TurnState,
}

impl TurnContext<'_> {
    pub fn session(&self) -> &Session {
        self.session
    }

    /// 给钩子用的工具环境：工作区、本轮的选项，以及当前会话和模型（[`SessionInfo`]、[`LlmAccess`]）。
    /// 在它上面 `emit_event` 不会写进日志；钩子要写事件用 [`Self::append_ext`]。
    pub fn tool_context(&self) -> ToolContext {
        let mut ctx = ToolContext::new(self.workspace.to_owned());
        ctx.options = self.config.options.clone();
        ctx.options.insert(SessionInfo { id: self.session.id().to_owned(), log_path: self.session.log_path().to_owned() });
        ctx.options.insert(LlmAccess { provider: self.provider.clone(), model: self.config.model.clone() });
        ctx
    }

    /// 写入一个事件，并推给订阅方。
    pub fn append(&mut self, body: EventBody) -> Result<(), AgentError> {
        self.projection = None;
        self.session.append(body, self.sink)?;
        Ok(())
    }

    /// 写入一条插件自定义事件。`kind` 写成 `命名空间/名字`（如 `todo/item`），不合规时报错、什么也不写。
    pub fn append_ext(&mut self, kind: &str, data: Value) -> Result<(), AgentError> {
        self.append(EventBody::ext(kind, data)?)
    }

    /// 当前的上下文投影。几个钩子和随后的请求共用同一份，不重复投影。
    pub fn projection(&mut self) -> &Projection {
        self.projection.get_or_insert_with(|| derive_messages_with(self.session.events(), self.plugins.items::<EventKinds>()))
    }

    /// 把一段事件投影成模型上下文（如压缩时只投影较早的那部分），规则和 [`Self::projection`] 一样。
    pub fn derive<'e>(&self, events: impl IntoIterator<Item = &'e SessionEvent>) -> Projection {
        derive_messages_with(events, self.plugins.items::<EventKinds>())
    }

    pub(crate) fn into_projection(self) -> Projection {
        match self.projection {
            Some(projection) => projection,
            None => derive_messages_with(self.session.events(), self.plugins.items::<EventKinds>()),
        }
    }

    /// 不带工具、不推送增量的一次性请求，返回文本；任何失败都返回 `None`。
    ///
    /// 响应本轮取消，最多等 [`COMPLETE_TEXT_TIMEOUT`]。
    pub async fn complete_text(&self, request: ChatRequest) -> Option<String> {
        complete_text(self.provider.as_ref(), request, self.cancel).await
    }
}

pub(crate) async fn complete_text(provider: &dyn Provider, request: ChatRequest, cancel: &CancellationToken) -> Option<String> {
    let work = async {
        let mut stream = provider.stream(request).await.ok()?;
        let mut accumulator = MessageAccumulator::default();
        while let Some(event) = stream.next().await {
            let event = event.ok()?;
            accumulator.push(&event).ok()?;
            if event == StreamEvent::MessageStop {
                break;
            }
        }
        Some(collect_text(&accumulator.finish().content))
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        result = tokio::time::timeout(COMPLETE_TEXT_TIMEOUT, work) => result.ok().flatten(),
    }
}

/// 一轮之内按类型存取的临时状态，下一轮从头开始。
#[derive(Default)]
pub struct TurnState(HashMap<TypeId, Box<dyn Any + Send + Sync>>);

impl TurnState {
    pub fn get_or_default<T: Default + Send + Sync + 'static>(&mut self) -> &mut T {
        self.0.entry(TypeId::of::<T>()).or_insert_with(|| Box::new(T::default())).downcast_mut().expect("按类型存取")
    }
}

/// 一次工具调用的环境，放在 [`ToolContext::options`] 里。需要再起一个 agent 的工具（`delegate`）用它。
#[derive(Clone)]
pub struct CallEnv {
    pub call_id: String,
    pub provider: Arc<dyn Provider>,
    pub approver: Arc<dyn Approver>,
    pub config: Arc<AgentConfig>,
    /// 子代理日志所在目录。
    pub agents_dir: PathBuf,
    pub cancel: CancellationToken,
}
