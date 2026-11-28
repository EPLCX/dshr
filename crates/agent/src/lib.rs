//! agent 循环。
//!
//! 一个 turn 由若干 step 组成，每个 step 是一次模型请求加上它发起的工具调用。
//! 所有持久事件写入会话日志，同时连同流式增量一起推给订阅方（CLI 或桌面 UI）。
//!
//! 循环本身不懂权限、沙箱、压缩、标题、预设和子代理：它们是挂在 [`hooks`] 上的插件。

mod approval;
mod attachment;
mod catalog;
mod compaction;
mod delegate;
mod hooks;
mod permission;
mod preset;
mod prompt;
mod sandbox;
mod session;
mod steer;
mod surface;
mod system_prompt;
mod title;

use std::sync::Arc;
use std::time::Duration;

use dshr_context::{Context, ContextError, Options, PluginGroup, Snapshot};
use dshr_core::{ContentBlock, ContinueReason, CoreError, EventBody, StopReason, TurnEndReason};
use dshr_llm::{AssembledMessage, ChatRequest, LlmError, MessageAccumulator, Provider, ReasoningEffort, StreamEvent};
use dshr_tools::{EventSink, OutputSink, ToolContext, ToolOutput, ToolRegistry};
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub use approval::{ApprovalRequest, Approver, AutoApprove, Decision};
pub use attachment::UserAttachment;
pub use catalog::{
    CatalogError, SessionCatalog, SessionSummary, delete_session, fork_session, list_sessions, list_sessions_cached, rename_session,
    session_log_path,
};
pub use compaction::{CompactionPlugin, estimate_tokens};
pub use delegate::{DELEGATE, DelegatePlugin, SubagentRunner};
pub use hooks::{
    AfterTool, AfterToolCall, AfterToolHook, AgentIdentity, BeforeRequest, BeforeStep, BeforeStepHook, CallEnv, EventKinds, EventLabel,
    EventLabels, EventObserver, EventObservers, GateRequest, GateTool, LlmAccess, ModelChoices, ObservedSession, ReviewTool,
    ReviewToolHook, SessionInfo, TransformRequest, TransformRequestHook, TurnContext, TurnControl, TurnEnded, TurnEndedHook, TurnInput,
    TurnStart, TurnStarted, TurnStartedHook, TurnState,
};
pub use permission::{
    AutoReviewer, Gate, PermissionMode, PermissionPlugin, READ_ONLY_DENIED, REVIEW_FAILED, UserIntent, gate, read_only_turn,
};
pub use preset::{PluginPreset, PluginPresets, Preset, PresetDef, PresetsPlugin, Subagent};
pub use prompt::{EnvironmentPlugin, InstructionsPlugin, PromptContext, PromptContributor, PromptParts};
pub use sandbox::SandboxPlugin;
pub use session::{Session, SessionOptions};
pub use steer::{SteerMessage, SteerQueue};
pub use surface::{
    CommandEntry, CommandResult, EntryPlace, FileMentions, MAX_MENTION_BYTES, MAX_MENTION_ITEMS, MentionContent, MentionItem,
    MentionSource, MentionSources, NODE_ICONS, NODE_TONES, SlashEntry, ThemeDef, ToolCardView, UiEntry, UiSurface, UiSurfaces, hideable,
    notify_page_refresh, page_refreshes, validate_tree,
};
pub use system_prompt::DEFAULT_SYSTEM_PROMPT;
pub use title::{TitlePlugin, title_changes};

/// 工具执行被用户中断时记录的结果文本；界面据此把卡片显示为“已取消”而不是“失败”。
pub const INTERRUPTED_TOOL_OUTPUT: &str = "Interrupted by user.";

/// 一轮里最多连续自动续写几次（回复被输出上限截断时）。
const MAX_TRUNCATIONS: u32 = 3;

/// 推给订阅方的实时事件。
#[derive(Debug, Clone, PartialEq)]
pub enum LiveEvent {
    /// 已写入日志的持久事件；用 `Arc` 共享，推送时不用深拷贝。
    Durable(std::sync::Arc<dshr_core::SessionEvent>),
    /// 模型流式输出的增量，只用于展示（对应原版的 `assistant/live-chunk`）。
    Chunk(Chunk),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    Text(String),
    Thinking(String),
    /// 工具执行中的实时输出（如命令的 stdout/stderr）。
    ToolOutput {
        call_id: String,
        text: String,
    },
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    /// 思考等级；`None` 用模型的默认值。输出长度与工具步数不设上限。
    pub effort: Option<ReasoningEffort>,
    /// 模型上下文窗口（tokens）；压缩插件据此判断上下文是不是快满了。
    pub context_window: u32,
    pub system_prompt: String,
    pub max_attempts: u32,
    pub retry_delay: Duration,
    /// 本轮的选项，由插件按类型读取：权限模式（[`PermissionMode`]）、预设（[`Preset`]）等。
    pub options: Options,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "deepseek-flash".into(),
            effort: None,
            context_window: 128_000,
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            max_attempts: 4,
            retry_delay: Duration::from_secs(1),
            options: Options::new(),
        }
    }
}

impl AgentConfig {
    /// 加一个给插件看的选项，如 `config.with(PermissionMode::Manual)`。
    pub fn with<T: Send + Sync + 'static>(mut self, option: T) -> Self {
        self.options.insert(option);
        self
    }

    /// 同 [`Self::with`]，`None` 时什么也不加。
    pub fn with_option<T: Send + Sync + 'static>(self, option: Option<T>) -> Self {
        match option {
            Some(option) => self.with(option),
            None => self,
        }
    }
}

/// agent 自带、主代理和子代理都用的插件：权限、沙箱、上下文压缩、预设、运行环境说明。
/// 标题（[`TitlePlugin`]）和子代理（[`DelegatePlugin`]）只给主代理，由主程序另外添加。
pub struct AgentPlugins;

impl PluginGroup for AgentPlugins {
    fn add_to(self, cx: &Context) -> Result<(), ContextError> {
        cx.add(PermissionPlugin)?;
        cx.add(SandboxPlugin)?;
        cx.add(CompactionPlugin)?;
        cx.add(PresetsPlugin)?;
        // 排在项目说明、Skills 这些追加内容前面（它们由主程序在这之后装上）。
        cx.add(EnvironmentPlugin)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("会话日志写入失败: {0}")]
    Log(#[from] CoreError),
    #[error(transparent)]
    EventKind(#[from] dshr_core::ExtKindError),
    /// 插件的 `turnStarted` 钩子拦下了这条消息：日志里什么都没写，界面把文字还给用户。
    #[error("插件 {plugin} 拦下了这条消息：{reason}")]
    Blocked { plugin: String, reason: String },
    /// 同一个会话已经在另一处（另一个 [`Session`]）跑着一轮：日志里什么都没写。
    #[error("这个会话正在另一处运行，等它结束后再发")]
    Busy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub reason: TurnEndReason,
    /// 最后一条 assistant 消息中的文本。
    pub final_text: String,
    pub error: Option<String>,
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    /// 工具和钩子从这里取：每个 step 开始时取一份快照，插件的增删从下一个 step 起生效。
    plugins: Context,
    approver: Arc<dyn Approver>,
    tool_ctx: ToolContext,
    config: Arc<AgentConfig>,
    /// 运行中插队的消息（[`Self::with_steering`]）：每一步开始前、以及本来要结束这一轮时取出来接进对话。
    steer: Option<SteerQueue>,
}

enum ModelResult {
    Done(AssembledMessage),
    Interrupted(AssembledMessage),
    Failed(LlmError),
}

/// 这一轮里插件追加到系统提示词后面的内容；外层的 `None` 表示还没算过。
#[derive(Default)]
struct PromptExtra(Option<Option<String>>);

/// 一个 step 里用的插件快照，以及从中取出的工具。
struct Step {
    plugins: Arc<Snapshot>,
    tools: ToolRegistry,
}

/// 一批里最多同时执行几个只读的工具调用。
const MAX_PARALLEL_TOOLS: usize = 8;

/// 已经放行、等着执行的一次工具调用。
struct Job<'a> {
    call_id: &'a str,
    name: &'a str,
    input: Arc<serde_json::Value>,
    tool: Arc<dyn dshr_tools::Tool>,
    ctx: ToolContext,
    emitted: Emitted,
}

/// 一次工具调用。
struct ToolCall<'a> {
    id: &'a str,
    /// 子代理日志所在目录。
    agents_dir: &'a std::path::Path,
    emitted: &'a Emitted,
    session: &'a SessionInfo,
    /// 用户最近说的话（自动审查用）。
    intent: &'a Arc<str>,
    /// 插件要求这一步之后结束本轮时置位。
    control: &'a TurnControl,
}

/// 工具执行期间通过 `ToolContext::emit_event` 写的自定义事件（类型名，数据），调用结束后写进日志。
#[derive(Clone, Default)]
struct Emitted(Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>);

impl Emitted {
    fn push(&self, kind: String, data: serde_json::Value) {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((kind, data));
    }

    fn take(&self) -> Vec<(String, serde_json::Value)> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        plugins: Context,
        approver: Arc<dyn Approver>,
        tool_ctx: ToolContext,
        config: AgentConfig,
    ) -> Self {
        let mut tool_ctx = tool_ctx;
        tool_ctx.options = config.options.clone();
        Self { provider, plugins, approver, tool_ctx, config: Arc::new(config), steer: None }
    }

    /// 运行中可以插队：界面往 `queue` 里放的消息，在当前这一步（模型回复和它发起的工具调用）结束后、
    /// 下一次请求模型之前接进对话，模型马上就能看到；本来要结束这一轮时也会先看一眼，有就接着做。
    pub fn with_steering(mut self, queue: SteerQueue) -> Self {
        self.steer = Some(queue);
        self
    }

    /// 把插队的消息写进日志（用户消息加附件），返回有没有写。
    fn take_steering(&self, session: &mut Session, sink: &UnboundedSender<LiveEvent>, intent: &mut Arc<str>) -> Result<bool, AgentError> {
        let Some(queue) = &self.steer else { return Ok(false) };
        let messages = queue.take_all();
        let injected = !messages.is_empty();
        for message in messages {
            *intent = message.text.as_str().into();
            session.append(EventBody::UserMessage { text: message.text.into() }, sink)?;
            for attachment in message.attachments {
                session.append(attachment_event(attachment), sink)?;
            }
        }
        Ok(injected)
    }

    pub async fn run_turn(
        &self,
        session: &mut Session,
        input: String,
        sink: &UnboundedSender<LiveEvent>,
        cancel: &CancellationToken,
    ) -> Result<TurnOutcome, AgentError> {
        self.run_turn_with(session, input, Vec::new(), sink, cancel).await
    }

    /// 带附件的一轮：附件跟在用户消息后面写进日志；图片附件在能看图的模型那里投影成图片。
    pub async fn run_turn_with(
        &self,
        session: &mut Session,
        input: String,
        attachments: Vec<UserAttachment>,
        sink: &UnboundedSender<LiveEvent>,
        cancel: &CancellationToken,
    ) -> Result<TurnOutcome, AgentError> {
        // 同一份日志同时只跑一轮；顺带补上别处写进日志的事件，轮次编号、上下文都从日志的最新状态算。
        let _claim = session.claim_turn().ok_or(AgentError::Busy)?;
        let mut state = TurnState::default();
        let outcome = self.run_steps(session, input, attachments, sink, cancel, &mut state).await?;

        let plugins = self.plugins.snapshot();
        session.set_observers(plugins.items::<EventObservers>().to_vec(), &self.tool_ctx.workspace);
        let mut turn = self.turn_context(session, sink, cancel, &mut state, &plugins);
        for hook in plugins.hooks::<TurnEnded>() {
            if let Err(error) = hook.turn_ended(&mut turn, &outcome).await {
                tracing::warn!(%error, "turn 结束后的钩子失败");
            }
        }
        Ok(outcome)
    }

    async fn run_steps(
        &self,
        session: &mut Session,
        input: String,
        attachments: Vec<UserAttachment>,
        sink: &UnboundedSender<LiveEvent>,
        cancel: &CancellationToken,
        state: &mut TurnState,
    ) -> Result<TurnOutcome, AgentError> {
        // 先问插件（`turnStarted`）：拦下时什么都不写。
        let plugins = self.plugins.snapshot();
        session.set_observers(plugins.items::<EventObservers>().to_vec(), &self.tool_ctx.workspace);
        let mut extra_events = Vec::new();
        let starters = plugins.hooks::<TurnStarted>();
        if !starters.is_empty() {
            let paths = attachments
                .iter()
                .map(|attachment| match attachment {
                    UserAttachment::Text(path, _) | UserAttachment::Image { path, .. } => path.clone(),
                })
                .collect();
            let input = TurnInput { turn: session.peek_next_turn(), text: &input, attachments: paths };
            let turn = self.turn_context(session, sink, cancel, state, &plugins);
            for hook in starters {
                let start = hook.turn_started(&turn, &input).await;
                if let Some(reason) = start.block {
                    return Err(AgentError::Blocked { plugin: hook.name().to_owned(), reason });
                }
                extra_events.extend(start.events);
            }
        }
        if !session.has_workspace() {
            let path = self.tool_ctx.workspace.display().to_string();
            session.append(EventBody::SessionWorkspace { path }, sink)?;
        }
        if !session.has_system_message() && !self.config.system_prompt.is_empty() {
            session.append(EventBody::SystemMessage { text: self.config.system_prompt.as_str().into() }, sink)?;
        }
        let turn = session.next_turn();
        session.append(EventBody::TurnStart { turn }, sink)?;
        let mut intent: Arc<str> = input.as_str().into();
        session.append(EventBody::UserMessage { text: input.into() }, sink)?;
        for attachment in attachments {
            session.append(attachment_event(attachment), sink)?;
        }
        // `turnStarted` 补的事件排在用户消息和附件之后。
        for (kind, data) in extra_events {
            match EventBody::ext(&kind, data) {
                Ok(body) => session.append(body, sink)?,
                Err(error) => tracing::warn!(kind, %error, "turnStarted 写的事件名字不合规，丢弃"),
            }
        }

        if turn == 1 && !plugins.items::<title::AutoTitle>().is_empty() && !session.has_title() && !cancel.is_cancelled() {
            title::spawn_title(session.log_path().to_owned(), &intent, self.config.model.clone(), self.provider.clone(), cancel.clone());
        }

        let mut final_text = String::new();
        // 连续被输出上限截断的次数；超过上限说明模型停不下来，结束本轮。
        let mut truncations = 0;
        // 子代理的日志放在本会话目录的 `agents/` 下。
        let agents_dir = session.agents_dir();
        let session_info = SessionInfo { id: session.id().to_owned(), log_path: session.log_path().to_owned() };
        // 不限制步数：模型不再调用工具、出错或被用户中断时结束。
        for step in 1.. {
            if cancel.is_cancelled() {
                return end_turn(session, sink, turn, TurnEndReason::Interrupted, final_text, None);
            }
            // 上一步运行期间插队的消息：接在工具结果后面，这一步的请求里就有。
            if step > 1 {
                self.take_steering(session, sink, &mut intent)?;
            }
            session.append(EventBody::StepStart { turn, step }, sink)?;
            let plugins = self.plugins.snapshot();
            session.set_observers(plugins.items::<EventObservers>().to_vec(), &self.tool_ctx.workspace);
            let current = Step { tools: ToolRegistry::load(&plugins).await, plugins };
            // 这一步里插件（工具、`afterTool`）可以要求本轮在这一步之后结束。
            let control = TurnControl::default();

            let message = match self.request_with_retry(session, &current, sink, cancel, state).await? {
                ModelResult::Done(message) => message,
                ModelResult::Interrupted(partial) => {
                    // 中断时只保留已输出的文本，未完成的思考与工具调用丢弃。
                    let content: Vec<_> = partial
                        .content
                        .into_iter()
                        .filter(|block| matches!(block, ContentBlock::Text { text } if !text.is_empty()))
                        .collect();
                    if !content.is_empty() {
                        final_text = collect_text(&content);
                        self.record_assistant(session, sink, content, None, partial.usage)?;
                    }
                    session.append(EventBody::StepEnd { turn, step }, sink)?;
                    return end_turn(session, sink, turn, TurnEndReason::Interrupted, final_text, None);
                }
                ModelResult::Failed(error) => {
                    session.append(EventBody::StepEnd { turn, step }, sink)?;
                    let error = Some(error.to_string());
                    return end_turn(session, sink, turn, TurnEndReason::Error, final_text, error);
                }
            };

            let stop_reason = message.stop_reason.clone();
            // 输出达到上限时，最后一个工具调用的参数多半不完整（解析不成 JSON 时保留为字符串），丢掉它，
            // 执行完整的调用后提示模型接着做，而不是把这一轮当作已经结束。
            let truncated = stop_reason == Some(StopReason::MaxTokens);
            let mut content = message.content;
            if truncated {
                content.retain(|block| !matches!(block, ContentBlock::ToolUse { input, .. } if input.is_string()));
                truncations += 1;
            } else {
                truncations = 0;
            }
            final_text = collect_text(&content);
            let tool_uses: Vec<_> = content
                .iter()
                .filter_map(|block| match block {
                    // `input` 是 `Arc`：这里只加引用计数，不复制参数（常常是整份文件内容）。
                    ContentBlock::ToolUse { id, name, input } => Some((id.clone(), name.clone(), input.clone())),
                    _ => None,
                })
                .collect();
            self.record_assistant(session, sink, content, stop_reason.clone(), message.usage)?;

            let continues = truncated && truncations <= MAX_TRUNCATIONS;
            if !continues && (tool_uses.is_empty() || stop_reason != Some(StopReason::ToolUse)) {
                session.append(EventBody::StepEnd { turn, step }, sink)?;
                // 模型说完了，但这期间有人插队：接着回答插队的消息，不结束这一轮。
                if self.take_steering(session, sink, &mut intent)? {
                    continue;
                }
                return end_turn(session, sink, turn, TurnEndReason::Completed, final_text, None);
            }

            // 连续的只读调用（读文件、搜索、子代理……）一起执行；会改东西的调用各自一批、按顺序执行。
            let mut next = 0;
            while next < tool_uses.len() {
                let end = self.batch_end(&current, &tool_uses, next);
                let batch = &tool_uses[next..end];
                next = end;
                // 先按顺序写调用事件、逐个把关（审批要一个一个问用户）。
                let mut jobs = Vec::new();
                for (call_id, name, input) in batch {
                    session.append(
                        EventBody::ToolCall { call_id: call_id.clone(), name: name.clone(), input: serde_json::Value::clone(input) },
                        sink,
                    )?;
                    // 工具执行期间写的自定义事件先收着：`tool/result` 必须紧跟在调用后面，事件排在它之后。
                    let emitted = Emitted::default();
                    let Some(tool) = current.tools.get(name) else {
                        finish_call(session, sink, call_id, name, ToolOutput::error(format!("Unknown tool: {name}")), &emitted)?;
                        continue;
                    };
                    let call = ToolCall {
                        id: call_id,
                        agents_dir: &agents_dir,
                        emitted: &emitted,
                        session: &session_info,
                        intent: &intent,
                        control: &control,
                    };
                    let ctx = self.call_context(&current, &call, sink, cancel);
                    let refused = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => Some(ToolOutput::error(INTERRUPTED_TOOL_OUTPUT)),
                        refused = self.authorize(&current, tool.as_ref(), call_id, name, input, &ctx, cancel) => refused,
                    };
                    match refused {
                        Some(output) => finish_call(session, sink, call_id, name, output, &emitted)?,
                        None => jobs.push(Job { call_id, name, input: Arc::clone(input), tool, ctx, emitted }),
                    }
                    if cancel.is_cancelled() {
                        break;
                    }
                }
                // 放行的调用同时执行，谁先完成先写结果（投影和界面都按 call_id 对应，不看先后）。
                let mut finished = vec![false; jobs.len()];
                {
                    let current = &current;
                    let mut running: futures::stream::FuturesUnordered<_> =
                        jobs.iter().enumerate().map(|(index, job)| async move { (index, self.run_tool(current, job).await) }).collect();
                    while !running.is_empty() {
                        let done = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => None,
                            done = running.next() => done,
                        };
                        let Some((index, output)) = done else { break };
                        finished[index] = true;
                        let job = &jobs[index];
                        finish_call(session, sink, job.call_id, job.name, output, &job.emitted)?;
                    }
                }
                // 被中断时没跑完的调用也要有结果。
                for (job, _) in jobs.iter().zip(&finished).filter(|(_, finished)| !**finished) {
                    finish_call(session, sink, job.call_id, job.name, ToolOutput::error(INTERRUPTED_TOOL_OUTPUT), &job.emitted)?;
                }
                if cancel.is_cancelled() {
                    session.append(EventBody::StepEnd { turn, step }, sink)?;
                    return end_turn(session, sink, turn, TurnEndReason::Interrupted, final_text, None);
                }
            }
            // 插件要求结束本轮（如工具已经给出了最终结果）：不再请求模型。
            if control.end_requested() {
                session.append(EventBody::StepEnd { turn, step }, sink)?;
                return end_turn(session, sink, turn, TurnEndReason::Completed, final_text, None);
            }
            if continues {
                session.append(EventBody::TurnContinue { reason: ContinueReason::MaxTokens }, sink)?;
            }
            session.append(EventBody::StepEnd { turn, step }, sink)?;
        }
        unreachable!("步数循环只会从内部返回")
    }

    fn turn_context<'a>(
        &'a self,
        session: &'a mut Session,
        sink: &'a UnboundedSender<LiveEvent>,
        cancel: &'a CancellationToken,
        state: &'a mut TurnState,
        plugins: &'a Snapshot,
    ) -> TurnContext<'a> {
        TurnContext {
            session,
            sink,
            projection: None,
            plugins,
            cancel,
            workspace: &self.tool_ctx.workspace,
            provider: &self.provider,
            config: &self.config,
            state,
        }
    }

    fn record_assistant(
        &self,
        session: &mut Session,
        sink: &UnboundedSender<LiveEvent>,
        content: Vec<ContentBlock>,
        stop_reason: Option<StopReason>,
        usage: dshr_core::Usage,
    ) -> Result<(), AgentError> {
        session.append(EventBody::AssistantMessage { model: self.config.model.clone(), content, stop_reason, usage: Some(usage) }, sink)?;
        Ok(())
    }

    async fn request_with_retry(
        &self,
        session: &mut Session,
        step: &Step,
        sink: &UnboundedSender<LiveEvent>,
        cancel: &CancellationToken,
        state: &mut TurnState,
    ) -> Result<ModelResult, AgentError> {
        let mut turn = self.turn_context(session, sink, cancel, state, &step.plugins);
        for hook in step.plugins.hooks::<BeforeStep>() {
            hook.before_step(&mut turn).await?;
        }
        let projection = turn.into_projection();
        // 插件追加的提示词一轮只算一次：少读几次文件，请求的前缀在一轮之内也保持不变。
        let extra = state.get_or_default::<PromptExtra>().0.get_or_insert_with(|| self.prompt_parts(&step.plugins)).clone();
        // 老会话的日志里是旧版默认提示词：换成新版。
        let system = match (projection.system.map(|system| system_prompt::upgrade_legacy(system.to_string())), extra) {
            (Some(system), Some(extra)) => Some(format!("{system}\n\n{extra}")),
            (system, extra) => system.or(extra),
        };
        // 只读的一轮（只读权限、规划模式、只读的插件预设）：会改东西的工具反正会被拒绝，干脆不发给模型，
        // 省得它白试一次。模型硬要调用时 `GateTool` 照样拦下。
        let tools = if read_only_turn(&self.config.options) { step.tools.read_only_specs(&self.tool_ctx) } else { step.tools.specs() };
        let mut request = ChatRequest {
            model: self.config.model.clone(),
            system,
            messages: projection.messages,
            tools,
            max_tokens: None,
            effort: self.config.effort,
        };
        let mut ctx = self.tool_ctx.clone();
        ctx.plugins = step.plugins.clone();
        ctx.options.insert(SessionInfo { id: session.id().to_owned(), log_path: session.log_path().to_owned() });
        ctx.options.insert(LlmAccess { provider: self.provider.clone(), model: self.config.model.clone() });
        for hook in step.plugins.hooks::<BeforeRequest>() {
            hook(&mut request, &self.config, &ctx);
        }
        for hook in step.plugins.hooks::<TransformRequest>() {
            hook.transform(&mut request, &ctx).await;
        }
        session.append(EventBody::RequestHeader { provider: self.provider.id().to_owned(), model: request.model.clone() }, sink)?;

        let mut attempt = 1;
        loop {
            match self.call_model(request.clone(), sink, cancel).await {
                ModelResult::Failed(error) if error.is_retryable() && attempt < self.config.max_attempts => {
                    tracing::warn!(attempt, %error, "模型请求失败，准备重试");
                    session.append(EventBody::AssistantAttempt { attempt, error: error.to_string() }, sink)?;
                    let wait = retry_wait(&error, self.config.retry_delay, attempt);
                    tokio::select! {
                        _ = cancel.cancelled() => return Ok(ModelResult::Interrupted(AssembledMessage::default())),
                        _ = tokio::time::sleep(wait) => {}
                    }
                    attempt += 1;
                }
                result => return Ok(result),
            }
        }
    }

    fn prompt_parts(&self, plugins: &Snapshot) -> Option<String> {
        let cx = PromptContext { workspace: &self.tool_ctx.workspace, options: &self.config.options, plugins: Some(plugins) };
        let parts: Vec<String> = plugins.items::<PromptParts>().iter().filter_map(|part| part.contribute(&cx)).collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    async fn call_model(&self, request: ChatRequest, sink: &UnboundedSender<LiveEvent>, cancel: &CancellationToken) -> ModelResult {
        let mut stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return ModelResult::Interrupted(AssembledMessage::default()),
            stream = self.provider.stream(request) => match stream {
                Ok(stream) => stream,
                Err(error) => return ModelResult::Failed(error),
            },
        };
        let mut accumulator = MessageAccumulator::default();
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => return ModelResult::Interrupted(accumulator.finish()),
                next = stream.next() => next,
            };
            let event = match next {
                None => break,
                Some(Err(error)) => return ModelResult::Failed(error),
                Some(Ok(event)) => event,
            };
            match &event {
                StreamEvent::TextDelta { text, .. } => {
                    let _ = sink.send(LiveEvent::Chunk(Chunk::Text(text.clone())));
                }
                StreamEvent::ThinkingDelta { thinking, .. } => {
                    let _ = sink.send(LiveEvent::Chunk(Chunk::Thinking(thinking.clone())));
                }
                _ => {}
            }
            if let Err(error) = accumulator.push(&event) {
                return ModelResult::Failed(error);
            }
            // 消息已经结束：不再等连接关闭（有的服务发完最后一个事件不断开，会一直卡在“生成中”）。
            if event == StreamEvent::MessageStop {
                break;
            }
        }
        ModelResult::Done(accumulator.finish())
    }

    /// 从 `start` 起的这一批调用到哪里为止（不含）：只读的调用和后面连续的只读调用一批（最多
    /// [`MAX_PARALLEL_TOOLS`] 个），其余的调用自己一批。
    fn batch_end(&self, step: &Step, calls: &[(String, String, Arc<serde_json::Value>)], start: usize) -> usize {
        let read_only = |(_, name, input): &(String, String, Arc<serde_json::Value>)| {
            step.tools.get(name).is_some_and(|tool| tool.access(input, &self.tool_ctx).is_read())
        };
        let mut end = start + 1;
        if read_only(&calls[start]) {
            while end < calls.len() && end - start < MAX_PARALLEL_TOOLS && read_only(&calls[end]) {
                end += 1;
            }
        }
        end
    }

    /// 这次调用的环境：实时输出带上 call_id 推给界面，自定义事件收进 `call.emitted`。
    fn call_context(&self, step: &Step, call: &ToolCall<'_>, sink: &UnboundedSender<LiveEvent>, cancel: &CancellationToken) -> ToolContext {
        let call_id = call.id;
        let mut ctx = self.tool_ctx.clone();
        ctx.plugins = step.plugins.clone();
        ctx.options.insert(CallEnv {
            call_id: call_id.to_owned(),
            identity: AgentIdentity::of(&self.config.options, Some(&call.session.id)),
            provider: self.provider.clone(),
            approver: self.approver.clone(),
            config: self.config.clone(),
            agents_dir: call.agents_dir.to_owned(),
            cancel: cancel.clone(),
        });
        ctx.options.insert(call.session.clone());
        ctx.options.insert(call.control.clone());
        ctx.options.insert(UserIntent(call.intent.clone()));
        ctx.options.insert(LlmAccess { provider: self.provider.clone(), model: self.config.model.clone() });
        let (sink, output_call) = (sink.clone(), call_id.to_owned());
        ctx.output = Some(OutputSink(Arc::new(move |text: &str| {
            let chunk = Chunk::ToolOutput { call_id: output_call.clone(), text: text.to_owned() };
            let _ = sink.send(LiveEvent::Chunk(chunk));
        })));
        let emitted = call.emitted.clone();
        ctx.events = Some(EventSink(Arc::new(move |kind, data| emitted.push(kind, data))));
        ctx
    }

    /// 执行前把关。放行时返回 `None`；被拒绝、用户不允许、等审批时被中断时返回交给模型的结果。
    #[allow(clippy::too_many_arguments)]
    async fn authorize(
        &self,
        step: &Step,
        tool: &dyn dshr_tools::Tool,
        call_id: &str,
        name: &str,
        input: &serde_json::Value,
        ctx: &ToolContext,
        cancel: &CancellationToken,
    ) -> Option<ToolOutput> {
        let access = tool.access(input, ctx);
        let request = GateRequest { tool: name, input, access: &access, ctx };
        // 第一个给出结论的钩子生效；都没有结论时询问。
        let mut gate = step.plugins.hooks::<GateTool>().iter().find_map(|hook| hook(&request)).unwrap_or(Gate::Ask);
        // 本来要问用户：先让审查钩子看一眼（如另一个模型判断），都没有结论才真的问。
        // 必须由用户亲自确认的（`Gate::Require`）不交给审查：模型或插件都不能替用户点头。
        if gate.asks() && !gate.mandatory() {
            for hook in step.plugins.hooks::<ReviewTool>() {
                if let Some(verdict) = hook.review(&request).await {
                    gate = verdict;
                    break;
                }
            }
        }
        match gate {
            Gate::Allow => None,
            Gate::Deny(reason) => Some(ToolOutput::error(reason)),
            Gate::Ask | Gate::Confirm(_) | Gate::Require(_) => {
                let mandatory = gate.mandatory();
                let note = match gate {
                    Gate::Confirm(note) => Some(note),
                    Gate::Require(note) => note,
                    _ => None,
                };
                let request = ApprovalRequest { call_id: call_id.to_owned(), tool: name.to_owned(), input: input.clone(), note, mandatory };
                // 等审批期间点停止：立刻当作中断返回，不再等用户点允许/拒绝。
                let decision = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Some(ToolOutput::error(INTERRUPTED_TOOL_OUTPUT)),
                    decision = self.approver.approve(&request) => decision,
                };
                (decision == Decision::Deny).then(|| ToolOutput::error("User denied this tool call."))
            }
        }
    }

    /// 执行一个已经放行的调用，再交给 [`AfterTool`] 钩子。
    async fn run_tool(&self, step: &Step, job: &Job<'_>) -> ToolOutput {
        let mut output = job.tool.execute(serde_json::Value::clone(&job.input), &job.ctx).await;
        let after = AfterToolCall { tool: job.name, input: &job.input, ctx: &job.ctx };
        for hook in step.plugins.hooks::<AfterTool>() {
            hook.after_tool(&after, &mut output).await;
        }
        output
    }
}

/// 一条 assistant 消息里的文字（不含思考和工具调用），如命令行输出最终回复时用。
pub fn assistant_text(content: &[ContentBlock]) -> String {
    collect_text(content)
}

pub(crate) fn collect_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(&**text),
            _ => None,
        })
        .collect()
}

/// 重试前最多等这么久（服务端要求得更久时也只等这么久，用户随时可以停止）。
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// 第 `attempt` 次失败后等多久再试：服务端给了 `retry-after` 就照它来；否则指数退避（`base`、2×、4×……），
/// 再乘上 0.75–1.25 的随机系数，免得多个会话（子代理）被同一次限流打断后又同时重试。
fn retry_wait(error: &LlmError, base: Duration, attempt: u32) -> Duration {
    if let Some(wait) = error.retry_after() {
        return wait.min(MAX_RETRY_WAIT);
    }
    let backoff = base.saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.subsec_nanos());
    let jitter = 0.75 + f64::from(nanos % 1000) / 2000.0;
    backoff.mul_f64(jitter).min(MAX_RETRY_WAIT)
}

/// 用户附件写进日志的样子。
fn attachment_event(attachment: UserAttachment) -> EventBody {
    match attachment {
        UserAttachment::Text(path, content) => {
            EventBody::UserAttachment { path, content: Some(content.into()), media_type: None, data: None }
        }
        UserAttachment::Image { path, media_type, data } => {
            EventBody::UserAttachment { path, content: None, media_type: Some(media_type), data: Some(data.into()) }
        }
    }
}

/// 写下一次调用的结果，以及它执行期间写的自定义事件（排在结果后面）。
fn finish_call(
    session: &mut Session,
    sink: &UnboundedSender<LiveEvent>,
    call_id: &str,
    name: &str,
    output: ToolOutput,
    emitted: &Emitted,
) -> Result<(), AgentError> {
    session
        .append(EventBody::ToolResult { call_id: call_id.to_owned(), output: output.content.into(), is_error: output.is_error }, sink)?;
    for (kind, data) in emitted.take() {
        match EventBody::ext(kind, data) {
            Ok(body) => session.append(body, sink)?,
            Err(error) => tracing::warn!(tool = name, %error, "工具写的事件被丢弃"),
        }
    }
    Ok(())
}

fn end_turn(
    session: &mut Session,
    sink: &UnboundedSender<LiveEvent>,
    turn: u32,
    reason: TurnEndReason,
    final_text: String,
    error: Option<String>,
) -> Result<TurnOutcome, AgentError> {
    session.append(EventBody::TurnEnd { turn, reason, error: error.clone() }, sink)?;
    Ok(TurnOutcome { reason, final_text, error })
}

#[cfg(test)]
mod tests;
