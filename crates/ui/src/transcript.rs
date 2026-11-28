//! 对话区的视图模型：把 agent 推来的事件归并成一行行可渲染的内容。
//!
//! 这里不碰窗口和元素，便于单独测试。历史会话用同一套规则从持久事件重建。
//! 某个工具、某种插件事件怎么显示不写在这里，而是查界面插件的注册表（[`UiRegistry`]）。
//!
//! 渲染每帧都会发生，所以凡是能在事件到达时算好的东西都在这里算一次存下来：定稿的文本存成
//! `Arc<str>`（界面克隆的只是引用），给读屏软件的纯文本、文件改动报告的解析结果也一并缓存。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use dshr_agent::{Chunk, Decision, INTERRUPTED_TOOL_OUTPUT, LiveEvent};
use dshr_core::{ContentBlock, EventBody, SessionEvent, TurnEndReason};
use gpui_kit::{Image, ImageFormat};
use rust_i18n::t;
use tokio::sync::oneshot;

use crate::plugin::{RowData, UiRegistry, ViewState};

/// 工具参数在卡片标题里最多显示的字符数。
const INPUT_PREVIEW_CHARS: usize = 160;
/// 终端面板保留的实时输出（字节）。
const MAX_LIVE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum Row {
    User {
        text: Arc<str>,
        attachments: Vec<Arc<str>>,
    },
    /// 模型正在输出的回复正文：文本还在变，每来一个增量追加一次。
    /// 纯文本（给读屏软件）在要用时才算，文本没变就沿用上次的结果，见 [`Row::plain`]。
    Streaming {
        text: String,
        plain: PlainText,
    },
    /// 定稿的回复正文。直接共用事件里的 `Arc<str>`（只有一个文本块时不拷贝）；
    /// 去掉 Markdown 标记的纯文本同样在要用时才算。
    Assistant {
        text: Arc<str>,
        plain: PlainText,
    },
    /// 一段连续的思考与工具调用，默认收起成一行；正文不在里面（见 [`WorkGroup`]）。
    Work(WorkGroup),
    Notice {
        text: Arc<str>,
        error: bool,
    },
    /// 一轮正常结束：复制、分享、分叉，以及这一轮的用量和结束时间。
    TurnFooter(TurnFooter),
    /// 插件自定义事件占的一行，由登记了这个事件类型的 `TranscriptRenderer` 画。
    Ext(ExtRow),
    /// 插件事件画成的节点树（JS 插件 `events` 里的 `render`），只读地显示。
    Plugin {
        tree: Arc<serde_json::Value>,
    },
}

/// 一轮结束处的那一行（[`Row::TurnFooter`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFooter {
    pub turn: u32,
    /// 这一轮第一行（用户消息）的下标：复制、分享时取这一段。
    pub start: usize,
    /// 这一轮所有模型请求用掉的 tokens（输入含缓存，加上输出）。
    pub tokens: u64,
    /// 结束时刻，Unix 毫秒。
    pub time: u64,
}

#[derive(Debug)]
pub struct ExtRow {
    /// 事件类型，如 `todo/item`。
    pub kind: Arc<str>,
    /// 事件到达时 renderer 算好的数据。
    pub data: RowData,
}

impl Row {
    fn assistant(text: Arc<str>) -> Self {
        Self::Assistant { text, plain: PlainText::default() }
    }

    /// 回复的纯文本（去掉 Markdown 标记，给读屏软件、复制用）；不是回复的行为 `None`。
    /// 第一次要时才算并记住：长会话里大多数回复从来不会滚进视口，不必为它们各存一份纯文本。
    pub fn plain(&self) -> Option<Arc<str>> {
        match self {
            Row::Assistant { text, plain, .. } => Some(plain.get(text, Some(text))),
            Row::Streaming { text, plain, .. } => Some(plain.get(text, None)),
            _ => None,
        }
    }

    fn notice(text: impl Into<Arc<str>>, error: bool) -> Self {
        Self::Notice { text: text.into(), error }
    }
}

/// 一段连续的思考与工具调用（[`Row::Work`]）。思考或工具调用到来时，末尾已经是这样一组就接着往里放，
/// 否则新开一组；回复正文总是组外单独的一行，所以记录里是“分组、正文、分组、正文”交替出现。
#[derive(Debug, Default)]
pub struct WorkGroup {
    pub items: Vec<WorkItem>,
    /// 展开后逐项列出思考与工具卡片；默认收起。
    pub expanded: bool,
}

#[derive(Debug)]
pub enum WorkItem {
    Thinking(Thought),
    Tool(ToolRow),
}

/// 一段思考：流式输出时逐段追加，最终消息到达（或一轮结束）时定稿。
#[derive(Debug)]
pub enum Thought {
    Live(String),
    Done(Arc<str>),
}

impl Thought {
    pub fn text(&self) -> &str {
        match self {
            Thought::Live(text) => text,
            Thought::Done(text) => text,
        }
    }

    fn is_live(&self) -> bool {
        matches!(self, Thought::Live(_))
    }
}

impl WorkGroup {
    /// 组里的工具调用。
    pub fn tools(&self) -> impl DoubleEndedIterator<Item = &ToolRow> {
        self.items.iter().filter_map(|item| match item {
            WorkItem::Tool(tool) => Some(tool),
            WorkItem::Thinking(_) => None,
        })
    }

    /// 还在进行：有正在输出的思考或还在运行的工具。
    pub fn is_active(&self) -> bool {
        self.items.iter().any(|item| match item {
            WorkItem::Thinking(thought) => thought.is_live(),
            WorkItem::Tool(tool) => tool.status == ToolStatus::Running,
        })
    }

    pub fn awaiting_approval(&self) -> bool {
        self.tools().any(ToolRow::awaiting_approval)
    }

    /// 有没有失败的工具调用。
    pub fn has_error(&self) -> bool {
        self.tools().any(|tool| matches!(tool.status, ToolStatus::Done { is_error: true, .. }))
    }

    /// 去掉还没定稿的思考（被最终消息取代或因重试作废）；返回第一段原来所在的位置。
    fn drop_live_thoughts(&mut self) -> Option<usize> {
        let first = self.items.iter().position(|item| matches!(item, WorkItem::Thinking(thought) if thought.is_live()));
        self.items.retain(|item| !matches!(item, WorkItem::Thinking(thought) if thought.is_live()));
        first
    }
}

/// 正在做什么，显示在消息列表末尾的状态行里（运行期间常驻）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Activity {
    /// 等模型回应（刚发出请求、工具都跑完了）。
    #[default]
    Waiting,
    Thinking,
    Writing,
    /// 正在执行的工具（名称）。
    Tool(Arc<str>),
    /// 请求失败，第几次重试。
    Retrying(u32),
}

#[derive(Debug)]
pub struct ToolRow {
    pub call_id: String,
    pub name: Arc<str>,
    pub input: Arc<str>,
    pub status: ToolStatus,
    pub expanded: bool,
    /// 等待用户审批时的回复通道。
    pub approval: Option<oneshot::Sender<Decision>>,
    /// 审批附带的说明（自动审查认为有风险的理由；必须确认时是宿主给的说明）。
    pub approval_note: Option<Arc<str>>,
    /// 这次审批必须由用户亲自确认（`Gate::Require`）：不显示“本会话不再询问”。
    pub approval_mandatory: bool,
    /// 审批涉及的插件能力：(插件 id, 能力名)。授予插件能力、修改已授权的插件时，审批面板把它们写成人能读懂的说法。
    pub approval_capabilities: Option<(Arc<str>, Vec<String>)>,
    /// 执行中的实时输出（终端面板），结束后以结果为准。
    pub live: String,
    /// 结果到达时这个工具的卡片（`ToolCard::on_result`）解析好的数据，如文件改动报告；没有就是通用卡片。
    pub data: Option<RowData>,
    /// 工具交回的图片（`media/images` 事件），到达时解码一次；不认识、解不开的跳过。
    pub images: Vec<Arc<Image>>,
}

impl ToolRow {
    /// 还在等用户审批：有审批请求，而且没有被别处先答复（插件的 `hooks.approve` 先给了结论时，agent 那边已经不再等）。
    pub fn awaiting_approval(&self) -> bool {
        self.approval.as_ref().is_some_and(|respond| !respond.is_closed())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done { output: Arc<str>, is_error: bool },
    Cancelled,
}

/// [`ChangeReport`] 的自有版本，存在工具行里供每帧渲染直接使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSummary {
    pub added: usize,
    pub removed: usize,
    /// 连续同类的行合成一段（行之间用 `\n` 连起来）。展开的卡片每段画一个元素，而不是每行一个：
    /// 新写一个上千行的文件时 diff 就是一整段新增，以前要为每一行建一个元素、在每次重画时逐个布局。
    pub blocks: Vec<(DiffLineKind, Arc<str>)>,
}

/// 回复的纯文本缓存：记着算它时原文的长度，原文变长（流式输出）后下次要时重算。
/// 纯文本和原文一样（没有 Markdown 标记）时直接共用原文的 `Arc`。
#[derive(Debug, Default)]
pub struct PlainText(RefCell<Option<(usize, Arc<str>)>>);

impl PlainText {
    fn get(&self, markdown: &str, original: Option<&Arc<str>>) -> Arc<str> {
        if let Some((len, plain)) = &*self.0.borrow()
            && *len == markdown.len()
        {
            return plain.clone();
        }
        let computed = plain_markdown(markdown);
        let plain = match original {
            Some(original) if **original == *computed => original.clone(),
            _ => Arc::from(computed),
        };
        *self.0.borrow_mut() = Some((markdown.len(), plain.clone()));
        plain
    }
}

/// 向插件要对话区里的显示内容（JS 插件 `events` 里的 `render`、`label`，以及 `cards`）。桌面版里是 `Backend`。
///
/// 问 JS 插件要跨到插件的线程上等它回答（每次最多等几百毫秒，插件正忙时等满）。所以这些方法只在后台线程上调用
/// （见 [`PluginViewResolver`]），界面线程上的 [`Transcript`] 只查算好的结果。`handles_*` 只看插件声明了什么，
/// 不问插件线程，可以在界面线程上调用。
pub trait PluginViewSource: Send + Sync {
    /// 有没有插件给这种事件画节点树或一行字。
    fn handles_event(&self, kind: &str) -> bool;
    /// 有没有插件给这个工具画卡片。
    fn handles_tool(&self, tool: &str) -> bool;
    /// 事件画成的节点树（JS 插件 `events` 里的 `render`）。
    fn event_tree(&self, kind: &str, data: &serde_json::Value) -> Option<serde_json::Value>;
    /// 事件显示成的一行字（JS 插件的 `label`）。
    fn event_label(&self, kind: &str, data: &serde_json::Value) -> Option<String>;
    /// 插件给自己的工具画的卡片（JS 插件的 `cards`）。
    fn tool_card(&self, tool: &str, input: &serde_json::Value, output: &str, is_error: bool) -> Option<dshr_agent::ToolCardView>;
}

/// 插件事件在对话区里的样子。
#[derive(Debug)]
pub enum ExtView {
    Tree(serde_json::Value),
    Label(String),
}

/// 在后台线程上向插件要好的显示内容，交给 [`Transcript`] 在事件到达时取用。
#[derive(Debug, Default)]
pub struct PluginViews {
    /// 按事件的 `seq`：插件事件画成的节点树或一行字。
    events: std::collections::HashMap<u64, ExtView>,
    /// 按调用号：插件给自己的工具画的卡片。
    cards: std::collections::HashMap<String, dshr_agent::ToolCardView>,
}

impl PluginViews {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.cards.is_empty()
    }
}

/// 在后台线程上替 [`Transcript`] 问插件。一轮对话（或一次打开会话）用一个：要画卡片的工具，
/// 它的调用参数先在这里记着，等结果到了一起交给插件。
pub struct PluginViewResolver {
    source: Arc<dyn PluginViewSource>,
    /// 还没出结果、有插件给它画卡片的工具调用：调用号 → (工具名, 参数)。
    inputs: std::collections::HashMap<String, (String, std::sync::Arc<serde_json::Value>)>,
}

impl PluginViewResolver {
    pub fn new(source: Arc<dyn PluginViewSource>) -> Self {
        Self { source, inputs: std::collections::HashMap::new() }
    }

    /// 记下有插件给它画卡片的工具调用的参数。只看插件的声明，不问插件：可以在界面线程上调用。
    pub fn record_calls<'a>(&mut self, events: impl IntoIterator<Item = &'a SessionEvent>) {
        for event in events {
            if let EventBody::ToolCall { call_id, name, input } = &event.body
                && self.source.handles_tool(name)
            {
                self.inputs.insert(call_id.clone(), (name.clone(), input.clone()));
            }
        }
    }

    /// 这些事件里有没有要问插件的（先 [`Self::record_calls`]）。没有时 [`Self::resolve`] 不碰插件，
    /// 可以直接在界面线程上调用；有时放到后台线程上。
    pub fn needs_plugins<'a>(&self, events: impl IntoIterator<Item = &'a SessionEvent>) -> bool {
        events.into_iter().any(|event| match &event.body {
            EventBody::Ext { kind, .. } => kind != dshr_core::MEDIA_IMAGES && self.source.handles_event(kind),
            EventBody::ToolResult { call_id, output, is_error } => !is_cancelled(output, *is_error) && self.inputs.contains_key(call_id),
            _ => false,
        })
    }

    /// 向插件要这些事件的显示内容。要等插件回答：除非 [`Self::needs_plugins`] 是 `false`，都在后台线程上调用。
    pub fn resolve<'a>(&mut self, events: impl IntoIterator<Item = &'a SessionEvent> + Clone) -> PluginViews {
        self.record_calls(events.clone());
        let mut views = PluginViews::default();
        for event in events {
            match &event.body {
                EventBody::Ext { kind, data } if kind != dshr_core::MEDIA_IMAGES && self.source.handles_event(kind) => {
                    let view = match self.source.event_tree(kind, data) {
                        Some(tree) => Some(ExtView::Tree(tree)),
                        None => self.source.event_label(kind, data).map(ExtView::Label),
                    };
                    if let Some(view) = view {
                        views.events.insert(event.seq, view);
                    }
                }
                EventBody::ToolResult { call_id, output, is_error } => {
                    let Some((tool, input)) = self.inputs.remove(call_id) else { continue };
                    if is_cancelled(output, *is_error) {
                        continue;
                    }
                    if let Some(card) = self.source.tool_card(&tool, &input, output, *is_error) {
                        views.cards.insert(call_id.clone(), card);
                    }
                }
                _ => {}
            }
        }
        views
    }
}

/// 被用户中断的调用：结果是 [`INTERRUPTED_TOOL_OUTPUT`]。卡片显示成“已取消”，不交给卡片解析。
fn is_cancelled(output: &str, is_error: bool) -> bool {
    is_error && output == INTERRUPTED_TOOL_OUTPUT
}

/// 存在 [`ToolRow::data`] 里的插件卡片：标题里的附加信息和展开后的节点树。
#[derive(Debug)]
pub struct PluginCard(pub dshr_agent::ToolCardView);

#[derive(Default)]
pub struct Transcript {
    pub rows: Vec<Row>,
    /// 不在行里的界面状态，按类型存取：如最近一次 `update_plan` 的计划，显示在计划面板里。
    state: ViewState,
    /// 界面插件登记的工具卡片与事件行。
    ui: Rc<UiRegistry>,
    /// 没有界面插件认领的事件和工具，JS 插件给它们画的东西：在后台算好（见 [`PluginViewResolver`]），
    /// 事件到达时取走。
    views: PluginViews,
    /// 工具调用号 → (行下标, 组内下标)：工具输出增量很高频，不能每次都从头扫一遍行。
    /// 行只往后追加（`drop_streaming` 只弹末尾的流式行），组内只在末尾的组里删掉未定稿的思考，
    /// 位置变了时查找会核对调用号、对不上就整表扫，所以存下标是安全的。
    tools: std::collections::HashMap<String, (usize, usize)>,
    /// 内容变过、还没告诉界面的行。界面只渲染看得见的行，行高变了要靠它通知列表重新测量。
    dirty: Vec<usize>,
    /// 这之前的行都已经定稿（上一轮结束时处理过）：轮次结束时只看这之后的行，
    /// 重放长会话时不必每一轮都把整份记录扫一遍。
    settled: usize,
    /// 正在进行的这一轮从哪一行开始、到现在用了多少 tokens（结束时写进 [`TurnFooter`]）。
    turn_start: usize,
    turn_tokens: u64,
    /// 正在做什么（状态行）。
    activity: Activity,
}

impl std::fmt::Debug for Transcript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transcript").field("rows", &self.rows).field("state", &self.state).finish_non_exhaustive()
    }
}

impl Transcript {
    pub fn new(ui: Rc<UiRegistry>) -> Self {
        Self { ui, ..Self::default() }
    }

    /// 同 [`Self::add_plugin_views`]，在 [`Self::replay`] 之前用。
    pub fn with_plugin_views(mut self, views: PluginViews) -> Self {
        self.add_plugin_views(views);
        self
    }

    /// 插件给接下来要放进来的事件画好的东西（在后台线程上向插件要的，见 [`PluginViewResolver`]）。
    pub fn add_plugin_views(&mut self, views: PluginViews) {
        self.views.events.extend(views.events);
        self.views.cards.extend(views.cards);
    }

    /// 从历史事件重建。
    #[cfg(test)]
    pub fn from_events<'a>(ui: Rc<UiRegistry>, events: impl IntoIterator<Item = &'a SessionEvent>) -> Self {
        Self::new(ui).replay(events)
    }

    /// 把历史事件依次放进来；之后没有待通知的变化。界面上打开会话时分段放（见 `ConversationView::open_session`）。
    #[cfg(test)]
    pub fn replay<'a>(mut self, events: impl IntoIterator<Item = &'a SessionEvent>) -> Self {
        self.replay_part(events);
        self.finish_replay();
        self
    }

    /// 分段重放：放进一段历史事件（长会话分几帧放完，界面不卡）。全部放完后调用 [`Self::finish_replay`]。
    pub fn replay_part<'a>(&mut self, events: impl IntoIterator<Item = &'a SessionEvent>) {
        for event in events {
            self.apply_durable(event);
        }
    }

    /// 分段重放结束：重放过程中记下的变化不用通知（界面拿到的是完整的结果）。
    pub fn finish_replay(&mut self) {
        self.dirty.clear();
    }

    /// 不在行里的界面状态（面板据此渲染）。
    pub fn state(&self) -> &ViewState {
        &self.state
    }

    /// 取走自上次调用以来内容变过的行的下标（升序、去重）。行数的增减由调用方比较 `rows.len()` 得到。
    pub fn take_dirty(&mut self) -> Vec<usize> {
        let mut dirty = std::mem::take(&mut self.dirty);
        dirty.sort_unstable();
        dirty.dedup();
        dirty
    }

    fn mark(&mut self, index: usize) {
        if self.dirty.last() != Some(&index) {
            self.dirty.push(index);
        }
    }

    /// 正在做什么，状态行据此显示（只在运行期间有意义）。
    pub fn activity(&self) -> &Activity {
        &self.activity
    }

    /// 界面自己加的一行提示（不进日志），如“由插件 X 发送”。
    pub fn notice(&mut self, text: impl Into<Arc<str>>) {
        self.push(Row::notice(text, false));
    }

    fn push(&mut self, row: Row) {
        self.mark(self.rows.len());
        self.rows.push(row);
    }

    /// 丢掉末尾还在流式输出的行（被最终消息或重试取代）。
    fn drop_streaming(&mut self) {
        if matches!(self.rows.last(), Some(Row::Streaming { .. })) {
            self.rows.pop();
        }
    }

    /// 末尾那一组里去掉还没定稿的思考，组空了就整行去掉；返回第一段原来的位置。
    fn drop_live_thoughts(&mut self) -> Option<usize> {
        let index = self.rows.len().checked_sub(1)?;
        let Some(Row::Work(group)) = self.rows.last_mut() else { return None };
        let first = group.drop_live_thoughts()?;
        if group.items.is_empty() {
            self.rows.pop();
        } else {
            self.mark(index);
        }
        Some(first)
    }

    /// 末尾的思考与工具分组，没有则新开一组。返回 (行下标, 分组)。
    fn work_group(&mut self) -> (usize, &mut WorkGroup) {
        if matches!(self.rows.last(), Some(Row::Work(_))) {
            self.mark(self.rows.len() - 1);
        } else {
            self.push(Row::Work(WorkGroup::default()));
        }
        let index = self.rows.len() - 1;
        match self.rows.last_mut() {
            Some(Row::Work(group)) => (index, group),
            _ => unreachable!("刚刚确保了末尾是分组"),
        }
    }

    pub fn apply(&mut self, event: &LiveEvent) {
        match event {
            LiveEvent::Durable(event) => self.apply_durable(event),
            // 纯文本不在这里算：每个增量都把整段重算一遍，长回复就是平方级的开销；渲染时按需算（见 `Row::plain`）。
            LiveEvent::Chunk(Chunk::Text(delta)) => {
                self.activity = Activity::Writing;
                self.streaming_row().push_str(delta);
            }
            LiveEvent::Chunk(Chunk::Thinking(delta)) => {
                self.activity = Activity::Thinking;
                let (_, group) = self.work_group();
                match group.items.last_mut() {
                    Some(WorkItem::Thinking(Thought::Live(text))) => text.push_str(delta),
                    _ => group.items.push(WorkItem::Thinking(Thought::Live(delta.clone()))),
                }
            }
            LiveEvent::Chunk(Chunk::ToolOutput { call_id, text }) => {
                if let Some(row) = self.tool_mut(call_id) {
                    row.live.push_str(text);
                    // 只留最后一段，长时间运行的命令不会无限占内存；完整输出在结果里。
                    if row.live.len() > MAX_LIVE_BYTES {
                        let mut cut = row.live.len() - MAX_LIVE_BYTES;
                        while !row.live.is_char_boundary(cut) {
                            cut += 1;
                        }
                        row.live.drain(..cut);
                    }
                }
            }
        }
    }

    fn apply_durable(&mut self, event: &SessionEvent) {
        match &event.body {
            EventBody::TurnStart { .. } => {
                self.turn_start = self.rows.len();
                self.turn_tokens = 0;
                self.activity = Activity::Waiting;
            }
            EventBody::StepStart { .. } => self.activity = Activity::Waiting,
            EventBody::UserMessage { text } => self.push(Row::User { text: text.clone(), attachments: Vec::new() }),
            EventBody::UserAttachment { path, .. } => {
                if let Some(Row::User { attachments, .. }) = self.rows.last_mut() {
                    attachments.push(path.as_str().into());
                    self.mark(self.rows.len() - 1);
                }
            }
            EventBody::AssistantMessage { content, usage, .. } => {
                if let Some(usage) = usage {
                    self.turn_tokens += usage.total_input_tokens() + usage.output_tokens;
                }
                let text = join_blocks(content, |block| match block {
                    ContentBlock::Text { text } => Some(text),
                    _ => None,
                });
                let thinking = join_blocks(content, |block| match block {
                    ContentBlock::Thinking { thinking, .. } => Some(thinking),
                    _ => None,
                });
                // 以最终消息为准，覆盖流式拼出来的内容：正文行换成定稿的，分组里的思考也换成定稿的。
                self.drop_streaming();
                let at = self.drop_live_thoughts();
                if !thinking.is_empty() {
                    let (_, group) = self.work_group();
                    let at = at.unwrap_or(group.items.len()).min(group.items.len());
                    group.items.insert(at, WorkItem::Thinking(Thought::Done(thinking)));
                }
                if !text.is_empty() {
                    self.push(Row::assistant(text));
                }
            }
            EventBody::AssistantAttempt { attempt, error } => {
                self.drop_streaming();
                self.drop_live_thoughts();
                self.activity = Activity::Retrying(*attempt);
                self.push(Row::notice(t!("notice.retry", attempt = attempt, error = error), true));
            }
            EventBody::ToolCall { call_id, name, input } => {
                // 这个工具有自己的卡片时先让它看一眼调用；它可以不显示成卡片（`update_plan` 显示在计划面板里）。
                let card = self.ui.tool_card(name).cloned();
                if let Some(card) = &card {
                    card.on_call(input, &mut self.state);
                    if !card.visible() {
                        return;
                    }
                }
                self.activity = Activity::Tool(name.as_str().into());
                let tool = ToolRow {
                    call_id: call_id.clone(),
                    name: name.as_str().into(),
                    input: preview(&json_prefix(input, INPUT_PREVIEW_CHARS * 4)).into(),
                    status: ToolStatus::Running,
                    expanded: false,
                    approval: None,
                    approval_note: None,
                    approval_mandatory: false,
                    approval_capabilities: None,
                    live: String::new(),
                    data: None,
                    images: Vec::new(),
                };
                let (index, group) = self.work_group();
                group.items.push(WorkItem::Tool(tool));
                let item = group.items.len() - 1;
                self.tools.insert(call_id.clone(), (index, item));
            }
            EventBody::ToolResult { call_id, output, is_error } => {
                let ui = self.ui.clone();
                let plugin_card = self.views.cards.remove(call_id);
                if let Some(row) = self.tool_mut(call_id) {
                    let cancelled = is_cancelled(output, *is_error);
                    row.status =
                        if cancelled { ToolStatus::Cancelled } else { ToolStatus::Done { output: output.clone(), is_error: *is_error } };
                    row.data = match ui.tool_card(&row.name) {
                        Some(card) if !cancelled => card.on_result(output, *is_error),
                        Some(_) => None,
                        // 没有界面插件的卡片：用工具所在的 JS 插件给它画的卡片（在后台算好的）。
                        None => plugin_card.filter(|_| !cancelled).map(|view| Rc::new(PluginCard(view)) as RowData),
                    };
                    row.approval = None;
                    row.approval_note = None;
                    row.approval_mandatory = false;
                    row.approval_capabilities = None;
                    // 结果到了，实时输出不再显示，不必继续占着内存。
                    row.live = String::new();
                }
                // 同一组里还有在跑的工具（并行的只读调用）就显示它，否则回到等模型。
                self.activity = match self.rows.last() {
                    Some(Row::Work(group)) => group
                        .tools()
                        .rev()
                        .find(|tool| tool.status == ToolStatus::Running)
                        .map_or(Activity::Waiting, |tool| Activity::Tool(tool.name.clone())),
                    _ => Activity::Waiting,
                };
            }
            EventBody::Ext { kind, data } if kind == dshr_core::MEDIA_IMAGES => {
                let call_id = data.get("call_id").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
                // 到达时解码一次存下来；以前存成 `data:` URI 字符串（base64 再拷一份），每帧画的时候还要再解码。
                let images: Vec<Arc<Image>> = data
                    .get("images")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|image| decode_image(image.get("media_type")?.as_str()?, image.get("data")?.as_str()?))
                    .collect();
                if let Some(row) = self.tool_mut(&call_id) {
                    row.images.extend(images);
                }
            }
            EventBody::Ext { kind, data } => {
                // 界面插件认领的类型由它画；否则看后端有没有给出一行字（JS 插件的 `label`）；
                // 都没有就不显示（插件停用后，它写过的事件就只留在日志里）。
                let view = self.views.events.remove(&event.seq);
                if let Some(renderer) = self.ui.transcript_renderer(kind).cloned() {
                    if let Some(data) = renderer.on_event(data, &mut self.state) {
                        self.push(Row::Ext(ExtRow { kind: kind.as_str().into(), data }));
                    }
                } else {
                    match view {
                        Some(ExtView::Tree(tree)) => self.push(Row::Plugin { tree: Arc::new(tree) }),
                        Some(ExtView::Label(text)) => self.push(Row::notice(text, false)),
                        None => {}
                    }
                }
            }
            EventBody::TurnEnd { turn, reason, error } => self.end_turn(*turn, *reason, error.as_deref(), event.time),
            EventBody::Compaction { .. } => self.push(Row::notice(t!("notice.compacted"), false)),
            EventBody::TurnContinue { .. } => self.push(Row::notice(t!("notice.continued"), false)),
            EventBody::SessionForked { turn, .. } => self.push(Row::notice(t!("notice.forked", turn = turn), false)),
            _ => {}
        }
    }

    fn end_turn(&mut self, turn: u32, reason: TurnEndReason, error: Option<&str>, time: u64) {
        for index in self.settled.min(self.rows.len())..self.rows.len() {
            let changed = match &mut self.rows[index] {
                // 没等到最终消息的半截回复（出错时）：按已收到的内容定稿。
                Row::Streaming { text, .. } => {
                    self.rows[index] = Row::assistant(std::mem::take(text).into());
                    true
                }
                Row::Work(group) if group.is_active() => {
                    for item in &mut group.items {
                        match item {
                            WorkItem::Thinking(thought) => {
                                if let Thought::Live(text) = thought {
                                    *thought = Thought::Done(std::mem::take(text).into());
                                }
                            }
                            WorkItem::Tool(tool) if tool.status == ToolStatus::Running => {
                                tool.status = ToolStatus::Cancelled;
                                tool.approval = None;
                                tool.approval_note = None;
                                tool.approval_mandatory = false;
                                tool.approval_capabilities = None;
                                tool.live = String::new();
                            }
                            WorkItem::Tool(_) => {}
                        }
                    }
                    true
                }
                _ => false,
            };
            if changed {
                self.mark(index);
            }
        }
        let (text, error) = match reason {
            TurnEndReason::Completed => {
                let start = self.turn_start.min(self.rows.len());
                self.push(Row::TurnFooter(TurnFooter { turn, start, tokens: self.turn_tokens, time }));
                self.settled = self.rows.len();
                return;
            }
            TurnEndReason::Interrupted => (t!("notice.interrupted"), false),
            TurnEndReason::MaxSteps => (t!("notice.max_steps"), true),
            TurnEndReason::Error => {
                let detail = error.map_or_else(|| t!("notice.unknown_error").to_string(), str::to_owned);
                (t!("notice.error", error = detail), true)
            }
        };
        self.push(Row::notice(text, error));
        self.settled = self.rows.len();
    }

    /// 末尾的流式正文行，没有则新建。返回正文缓冲。
    fn streaming_row(&mut self) -> &mut String {
        if matches!(self.rows.last(), Some(Row::Streaming { .. })) {
            self.mark(self.rows.len() - 1);
        } else {
            self.push(Row::Streaming { text: String::new(), plain: PlainText::default() });
        }
        match self.rows.last_mut() {
            Some(Row::Streaming { text, .. }) => text,
            _ => unreachable!("刚刚确保了末尾是流式行"),
        }
    }

    /// 工具调用在第几行、组里第几项。
    fn locate_tool(&self, call_id: &str) -> Option<(usize, usize)> {
        let is_it = |row: &Row, item: usize| matches!(row, Row::Work(group) if matches!(group.items.get(item), Some(WorkItem::Tool(tool)) if tool.call_id == call_id));
        // 先查缓存的下标，对不上再整表扫（兼容直接改 rows 的调用方）。
        if let Some(&(index, item)) = self.tools.get(call_id)
            && self.rows.get(index).is_some_and(|row| is_it(row, item))
        {
            return Some((index, item));
        }
        self.rows.iter().enumerate().rev().find_map(|(index, row)| match row {
            Row::Work(group) => group
                .items
                .iter()
                .position(|item| matches!(item, WorkItem::Tool(tool) if tool.call_id == call_id))
                .map(|item| (index, item)),
            _ => None,
        })
    }

    /// 找到工具调用并把它所在的行记为“内容变过”（调用方拿到可变引用就是要改它）。
    pub fn tool_mut(&mut self, call_id: &str) -> Option<&mut ToolRow> {
        let (index, item) = self.locate_tool(call_id)?;
        self.tools.insert(call_id.to_owned(), (index, item));
        self.mark(index);
        match &mut self.rows[index] {
            Row::Work(group) => match &mut group.items[item] {
                WorkItem::Tool(tool) => Some(tool),
                WorkItem::Thinking(_) => None,
            },
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn attach_approval(&mut self, call_id: &str, respond: oneshot::Sender<Decision>, note: Option<String>) {
        self.attach_approval_with(call_id, respond, note, false, None);
    }

    /// 把审批请求挂到对应的工具卡片上；找不到卡片（已结束）时直接拒绝。
    /// 同时记录“必须亲自确认”和涉及的插件能力（见 [`ToolRow::approval_capabilities`]）。
    pub fn attach_approval_with(
        &mut self,
        call_id: &str,
        respond: oneshot::Sender<Decision>,
        note: Option<String>,
        mandatory: bool,
        capabilities: Option<(String, Vec<String>)>,
    ) {
        match self.tool_mut(call_id) {
            Some(row) if row.status == ToolStatus::Running => {
                row.approval = Some(respond);
                row.approval_note = note.map(Arc::from);
                row.approval_mandatory = mandatory;
                row.approval_capabilities = capabilities.map(|(plugin, names)| (Arc::from(plugin), names));
            }
            _ => {
                let _ = respond.send(Decision::Deny);
            }
        }
    }

    pub fn resolve_approval(&mut self, call_id: &str, decision: Decision) {
        if let Some(row) = self.tool_mut(call_id) {
            row.approval_note = None;
            row.approval_mandatory = false;
            row.approval_capabilities = None;
            if let Some(respond) = row.approval.take() {
                let _ = respond.send(decision);
            }
        }
    }

    /// 第一个等待审批的工具调用（审批按顺序一个一个问，同一时刻最多一个）。
    pub fn pending_approval(&self) -> Option<&ToolRow> {
        self.rows.iter().rev().find_map(|row| match row {
            Row::Work(group) => group.tools().find(|tool| tool.awaiting_approval()),
            _ => None,
        })
    }

    /// 第 `footer` 行（[`Row::TurnFooter`]）那一轮里最后一条回复的原文（Markdown）。
    pub fn turn_reply(&self, footer: usize) -> Option<Arc<str>> {
        let Some(Row::TurnFooter(info)) = self.rows.get(footer) else { return None };
        self.rows[info.start.min(footer)..footer].iter().rev().find_map(|row| match row {
            Row::Assistant { text, .. } if !text.is_empty() => Some(text.clone()),
            _ => None,
        })
    }

    /// 第 `footer` 行那一轮整理成 Markdown：用户说的话（含插队的消息）和回复，按出现的顺序。
    pub fn turn_markdown(&self, footer: usize) -> Option<String> {
        let Some(Row::TurnFooter(info)) = self.rows.get(footer) else { return None };
        let mut parts = Vec::new();
        for row in &self.rows[info.start.min(footer)..footer] {
            match row {
                Row::User { text, attachments } => {
                    let mut part = format!("## {}\n\n{text}", t!("conversation.share_user"));
                    for path in attachments {
                        part.push_str(&format!("\n\n> {}", t!("conversation.share_attachment", path = path)));
                    }
                    parts.push(part);
                }
                Row::Assistant { text, .. } if !text.is_empty() => {
                    parts.push(format!("## {}\n\n{text}", t!("conversation.share_assistant")))
                }
                _ => {}
            }
        }
        (!parts.is_empty()).then(|| parts.join("\n\n") + "\n")
    }

    /// 展开或收起第 `index` 行的思考与工具分组，返回它是不是展开了。
    pub fn toggle_group(&mut self, index: usize) -> bool {
        let Some(Row::Work(group)) = self.rows.get_mut(index) else { return false };
        group.expanded = !group.expanded;
        let expanded = group.expanded;
        self.mark(index);
        expanded
    }

    /// 展开或折叠工具卡片，返回它是不是展开了。
    pub fn toggle_tool(&mut self, call_id: &str) -> bool {
        self.tool_mut(call_id).is_some_and(|row| {
            row.expanded = !row.expanded;
            row.expanded
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Added,
    Removed,
    Hunk,
    Context,
}

/// 文件改动报告：`edit_file` / `write_file` 的输出首行是 `Edited path (+a -r)`，空一行后是统一 diff。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeReport<'a> {
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<(DiffLineKind, &'a str)>,
}

pub fn parse_change_report(output: &str) -> Option<ChangeReport<'_>> {
    let (header, rest) = output.split_once('\n')?;
    let counts = header.rsplit_once(" (+")?.1.strip_suffix(')')?;
    let (added, removed) = counts.split_once(" -")?;
    let lines = rest
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with("---") && !line.starts_with("+++"))
        .map(|line| {
            let kind = match line.as_bytes()[0] {
                b'+' => DiffLineKind::Added,
                b'-' => DiffLineKind::Removed,
                b'@' => DiffLineKind::Hunk,
                _ => DiffLineKind::Context,
            };
            (kind, line)
        })
        .collect();
    Some(ChangeReport { added: added.parse().ok()?, removed: removed.parse().ok()?, lines })
}

impl ChangeReport<'_> {
    pub fn to_owned(&self) -> ChangeSummary {
        let mut blocks: Vec<(DiffLineKind, String)> = Vec::new();
        for (kind, line) in &self.lines {
            match blocks.last_mut() {
                Some((last, text)) if last == kind => {
                    text.push('\n');
                    text.push_str(line);
                }
                _ => blocks.push((*kind, (*line).to_owned())),
            }
        }
        ChangeSummary {
            added: self.added,
            removed: self.removed,
            blocks: blocks.into_iter().map(|(kind, text)| (kind, Arc::from(text))).collect(),
        }
    }
}

/// 文本的最后 `count` 行（末尾的换行不算一行），行尾统一成 `\n`。
/// 从末尾往前找，不必把整段文本先拆成行。
pub fn tail_lines(text: &str, count: usize) -> String {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    let start = text.rmatch_indices('\n').nth(count.saturating_sub(1)).map_or(0, |(index, _)| index + 1);
    text[start..].replace("\r\n", "\n")
}

/// 给读屏软件的纯文本：去掉 Markdown 标记（标题井号、强调、行内代码、代码围栏、链接地址），保留文字与换行。
pub fn plain_markdown(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            continue;
        }
        let line = trimmed.trim_start_matches('#').trim_start_matches('>');
        let line = if line.len() == trimmed.len() { trimmed } else { line.trim_start() };
        lines.push(strip_links(&line.replace("**", "").replace("__", "").replace("~~", "").replace('`', "")));
    }
    lines.join("\n")
}

/// `[文字](地址)` 与 `![说明](地址)` 只留文字。
fn strip_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|i| open + i) else { break };
        let Some(end) = rest[close..].find(')').map(|i| close + i) else { break };
        out.push_str(rest[..open].strip_suffix('!').unwrap_or(&rest[..open]));
        out.push_str(&rest[open + 1..close]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// 把挑出来的几块文本用空行连起来。只有一块（最常见）时直接共用它的 `Arc`，不拷贝。
fn join_blocks<'a>(content: &'a [ContentBlock], pick: impl Fn(&'a ContentBlock) -> Option<&'a Arc<str>>) -> Arc<str> {
    let mut picked = content.iter().filter_map(pick);
    let Some(first) = picked.next() else { return Arc::from("") };
    let Some(second) = picked.next() else { return first.clone() };
    let mut joined = format!("{first}\n\n{second}");
    for block in picked {
        joined.push_str("\n\n");
        joined.push_str(block);
    }
    joined.into()
}

/// 解码一张 base64 图片（工具交回的、插件页面里的）。只认 png / jpeg / gif / webp / svg；解不开时为 `None`。
pub(crate) fn decode_image(media_type: &str, data: &str) -> Option<Arc<Image>> {
    use base64::Engine as _;
    let format = match media_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        "image/svg+xml" => ImageFormat::Svg,
        _ => return None,
    };
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

/// 值序列化成 JSON 的前 `limit` 个字节左右。卡片标题只显示开头一段，不必把整个参数（写入的整份文件内容）
/// 都序列化出来：写够了就让序列化停下。
fn json_prefix(value: &serde_json::Value, limit: usize) -> String {
    struct Bounded {
        buffer: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.buffer.len() >= self.limit {
                return Err(std::io::Error::other("够了"));
            }
            self.buffer.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Bounded { buffer: Vec::new(), limit };
    let _ = serde_json::to_writer(&mut out, value);
    // 停下的位置可能在一个多字节字符中间：丢掉不完整的尾巴。
    match String::from_utf8(out.buffer) {
        Ok(text) => text,
        Err(error) => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
    }
}

fn preview(text: &str) -> String {
    match text.char_indices().nth(INPUT_PREVIEW_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cards::{FsUi, PlanUi};
    use crate::plugin::{TranscriptRenderer, UiPlugin, UiScope};
    use dshr_tools::{Plan, UPDATE_PLAN};
    use serde_json::json;

    /// 装了内置工具卡片的注册表。
    fn builtin_ui() -> Rc<UiRegistry> {
        let plugins: Vec<Box<dyn UiPlugin>> = vec![Box::new(FsUi), Box::new(PlanUi)];
        Rc::new(UiRegistry::assemble(&plugins, |_| None))
    }

    fn durable(body: EventBody) -> LiveEvent {
        LiveEvent::Durable(std::sync::Arc::new(SessionEvent { seq: 0, time: 0, body }))
    }

    fn assistant(content: Vec<ContentBlock>) -> LiveEvent {
        durable(EventBody::AssistantMessage { model: "m".into(), content, stop_reason: None, usage: None })
    }

    /// 第 `row` 行分组里的第 `item` 项工具调用。
    #[track_caller]
    fn tool(row: &Row, item: usize) -> &ToolRow {
        match row {
            Row::Work(group) => match &group.items[item] {
                WorkItem::Tool(tool) => tool,
                other => panic!("应当是工具调用：{other:?}"),
            },
            other => panic!("应当是分组：{other:?}"),
        }
    }

    /// 第 `row` 行分组里的第 `item` 项思考。
    #[track_caller]
    fn thought(row: &Row, item: usize) -> &Thought {
        match row {
            Row::Work(group) => match &group.items[item] {
                WorkItem::Thinking(thought) => thought,
                other => panic!("应当是思考：{other:?}"),
            },
            other => panic!("应当是分组：{other:?}"),
        }
    }

    #[test]
    fn streaming_text_is_replaced_by_final_message() {
        let mut t = Transcript::default();
        t.apply(&durable(EventBody::UserMessage { text: "hi".into() }));
        t.apply(&LiveEvent::Chunk(Chunk::Thinking("嗯".into())));
        t.apply(&LiveEvent::Chunk(Chunk::Text("你".into())));
        t.apply(&LiveEvent::Chunk(Chunk::Text("好".into())));
        // 思考进分组，正文是组外单独的一行。
        assert!(matches!(thought(&t.rows[1], 0), Thought::Live(text) if text == "嗯"));
        assert!(matches!(&t.rows[2], Row::Streaming { text, .. } if text == "你好"));
        assert_eq!(t.activity(), &Activity::Writing);

        t.apply(&assistant(vec![ContentBlock::Thinking { thinking: "嗯嗯".into(), signature: None }, ContentBlock::text("你好！")]));
        assert_eq!(t.rows.len(), 3);
        assert!(matches!(thought(&t.rows[1], 0), Thought::Done(text) if &**text == "嗯嗯"));
        assert!(matches!(&t.rows[1], Row::Work(group) if group.items.len() == 1 && !group.is_active() && !group.expanded));
        assert!(matches!(&t.rows[2], Row::Assistant { text, .. } if &**text == "你好！"));
    }

    #[test]
    fn tool_only_message_leaves_no_empty_bubble() {
        let mut t = Transcript::default();
        t.apply(&assistant(vec![ContentBlock::ToolUse { id: "c1".into(), name: "x".into(), input: json!({}).into() }]));
        t.apply(&durable(EventBody::ToolCall { call_id: "c1".into(), name: "list_dir".into(), input: json!({"path": "."}).into() }));
        assert_eq!(t.rows.len(), 1);
        assert_eq!(&*tool(&t.rows[0], 0).input, r#"{"path":"."}"#);
        assert_eq!(t.activity(), &Activity::Tool("list_dir".into()));

        t.apply(&durable(EventBody::ToolResult { call_id: "c1".into(), output: "a.txt".into(), is_error: false }));
        let done = tool(&t.rows[0], 0);
        assert!(done.status == ToolStatus::Done { output: "a.txt".into(), is_error: false } && done.data.is_none());
        assert_eq!(t.activity(), &Activity::Waiting);
    }

    #[tokio::test]
    async fn approval_round_trip_and_cancel() {
        let mut t = Transcript::default();
        t.apply(&durable(EventBody::ToolCall { call_id: "c1".into(), name: "run_shell".into(), input: json!({}).into() }));

        let (tx, rx) = oneshot::channel();
        t.attach_approval("c1", tx, Some("有风险".into()));
        assert!(matches!(t.pending_approval(), Some(tool) if tool.approval_note.as_deref() == Some("有风险")));
        t.resolve_approval("c1", Decision::Allow);
        assert_eq!(rx.await.unwrap(), Decision::Allow);
        assert!(t.pending_approval().is_none());

        // 未知卡片直接拒绝。
        let (tx, rx) = oneshot::channel();
        t.attach_approval("nope", tx, None);
        assert_eq!(rx.await.unwrap(), Decision::Deny);

        // 中断时 agent 记下的中断结果显示为已取消，并给出提示。
        let (tx, rx) = oneshot::channel();
        t.attach_approval("c1", tx, None);
        t.apply(&durable(EventBody::ToolResult { call_id: "c1".into(), output: INTERRUPTED_TOOL_OUTPUT.into(), is_error: true }));
        t.apply(&durable(EventBody::TurnEnd { turn: 1, reason: TurnEndReason::Interrupted, error: None }));
        assert!(rx.await.is_err());
        assert_eq!(tool(&t.rows[0], 0).status, ToolStatus::Cancelled);
        assert!(matches!(&t.rows[1], Row::Notice { text, error: false } if **text == *t!("notice.interrupted")));
    }

    #[test]
    fn footers_carry_usage_time_and_the_turn_text() {
        let event = |time, body| LiveEvent::Durable(std::sync::Arc::new(SessionEvent { seq: 0, time, body }));
        let usage = |input, output| dshr_core::Usage { input_tokens: input, output_tokens: output, ..Default::default() };
        let reply = |text: &str, tokens: (u64, u64)| {
            event(
                0,
                EventBody::AssistantMessage {
                    model: "m".into(),
                    content: vec![ContentBlock::text(text)],
                    stop_reason: None,
                    usage: Some(usage(tokens.0, tokens.1)),
                },
            )
        };
        let mut t = Transcript::default();
        t.apply(&event(0, EventBody::TurnStart { turn: 1 }));
        t.apply(&event(0, EventBody::UserMessage { text: "第一问".into() }));
        t.apply(&reply("一", (10, 2)));
        t.apply(&event(5, EventBody::TurnEnd { turn: 1, reason: TurnEndReason::Completed, error: None }));
        t.apply(&event(0, EventBody::TurnStart { turn: 2 }));
        t.apply(&event(0, EventBody::UserMessage { text: "第二问".into() }));
        t.apply(&reply("先看看", (100, 20)));
        // 中途插队的消息也算这一轮的。
        t.apply(&event(0, EventBody::UserMessage { text: "插队".into() }));
        t.apply(&reply("二", (200, 30)));
        t.apply(&event(9, EventBody::TurnEnd { turn: 2, reason: TurnEndReason::Completed, error: None }));

        let footer = t.rows.len() - 1;
        assert!(matches!(&t.rows[footer], Row::TurnFooter(TurnFooter { turn: 2, start: 3, tokens: 350, time: 9 })));
        assert_eq!(t.turn_reply(footer).as_deref(), Some("二"));
        let markdown = t.turn_markdown(footer).unwrap();
        assert!(markdown.contains("第二问") && markdown.contains("插队") && markdown.contains("先看看") && markdown.contains("二"));
        assert!(!markdown.contains("第一问"));
        assert!(matches!(&t.rows[2], Row::TurnFooter(TurnFooter { tokens: 12, .. })));
    }

    #[test]
    fn plan_calls_update_panel_and_turns_get_footers() {
        let plan_call = durable(EventBody::ToolCall {
            call_id: "p1".into(),
            name: UPDATE_PLAN.into(),
            input: json!({"steps": [{"title": "读代码", "status": "in_progress"}]}).into(),
        });
        let mut t = Transcript::new(builtin_ui());
        t.apply(&plan_call);
        t.apply(&durable(EventBody::ToolResult { call_id: "p1".into(), output: "ok".into(), is_error: false }));
        assert!(t.rows.is_empty());
        assert_eq!(t.state().get::<Plan>().unwrap().steps[0].title, "读代码");

        t.apply(&durable(EventBody::TurnEnd { turn: 3, reason: TurnEndReason::Completed, error: None }));
        assert!(matches!(t.rows.last(), Some(Row::TurnFooter(TurnFooter { turn: 3, .. }))));

        // `plan` 插件的界面那一半没装时，`update_plan` 就是一张普通的工具卡片。
        let mut t = Transcript::default();
        t.apply(&plan_call);
        assert_eq!(t.rows.len(), 1);
        assert_eq!(&*tool(&t.rows[0], 0).name, UPDATE_PLAN);
        assert!(t.state().get::<Plan>().is_none());
    }

    /// 测试用的事件行：`notes/add` 显示成一行，`notes/count` 只计数。
    struct NoteRows;

    impl TranscriptRenderer for NoteRows {
        fn on_event(&self, data: &serde_json::Value, state: &mut ViewState) -> Option<RowData> {
            *state.get_or_default::<usize>() += 1;
            let text = data.get("text")?.as_str()?;
            Some(Rc::new(text.to_owned()))
        }

        fn render(&self, _row: &ExtRow, _index: usize, _cx: &mut gpui_kit::App) -> gpui_kit::AnyElement {
            unreachable!("测试里不渲染")
        }
    }

    type TreeFn = Box<dyn Fn(&str, &serde_json::Value) -> Option<serde_json::Value> + Send + Sync>;
    type LabelFn = Box<dyn Fn(&str, &serde_json::Value) -> Option<String> + Send + Sync>;
    type CardFn = Box<dyn Fn(&str, &serde_json::Value, &str, bool) -> Option<dshr_agent::ToolCardView> + Send + Sync>;

    /// 代替 JS 插件：几个函数，外加数一数被问了几次。
    struct FakePlugins {
        tree: TreeFn,
        label: LabelFn,
        card: CardFn,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl Default for FakePlugins {
        fn default() -> Self {
            Self { tree: Box::new(|_, _| None), label: Box::new(|_, _| None), card: Box::new(|_, _, _, _| None), asked: Default::default() }
        }
    }

    impl PluginViewSource for FakePlugins {
        fn handles_event(&self, kind: &str) -> bool {
            kind.starts_with("notes/") || kind.starts_with("chart/")
        }

        fn handles_tool(&self, tool: &str) -> bool {
            tool == "count"
        }

        fn event_tree(&self, kind: &str, data: &serde_json::Value) -> Option<serde_json::Value> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (self.tree)(kind, data)
        }

        fn event_label(&self, kind: &str, data: &serde_json::Value) -> Option<String> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (self.label)(kind, data)
        }

        fn tool_card(&self, tool: &str, input: &serde_json::Value, output: &str, is_error: bool) -> Option<dshr_agent::ToolCardView> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (self.card)(tool, input, output, is_error)
        }
    }

    #[test]
    fn resolver_only_asks_plugins_about_what_they_declared() {
        let plugins = Arc::new(FakePlugins::default());
        let mut resolver = PluginViewResolver::new(plugins.clone());
        let event = |seq, body| SessionEvent { seq, time: 0, body };
        // 普通事件、没人声明的插件事件、没人画卡片的工具：不用问插件，可以留在界面线程上。
        let plain = [
            event(1, EventBody::UserMessage { text: "hi".into() }),
            event(2, EventBody::ext("other/thing", json!({})).unwrap()),
            event(3, EventBody::ToolCall { call_id: "c1".into(), name: "read_file".into(), input: json!({}).into() }),
            event(4, EventBody::ToolResult { call_id: "c1".into(), output: "x".into(), is_error: false }),
        ];
        resolver.record_calls(&plain);
        assert!(!resolver.needs_plugins(&plain));
        assert!(resolver.resolve(&plain).is_empty());
        assert_eq!(plugins.asked.load(std::sync::atomic::Ordering::Relaxed), 0);

        // 有插件画卡片的工具：调用和结果分在两批到达也行；结果到了才需要问插件。
        let call = [event(5, EventBody::ToolCall { call_id: "c2".into(), name: "count".into(), input: json!({"text": "a"}).into() })];
        resolver.record_calls(&call);
        assert!(!resolver.needs_plugins(&call));
        resolver.resolve(&call);
        let result = [event(6, EventBody::ToolResult { call_id: "c2".into(), output: "1".into(), is_error: false })];
        resolver.record_calls(&result);
        assert!(resolver.needs_plugins(&result));
        resolver.resolve(&result);
        assert_eq!(plugins.asked.load(std::sync::atomic::Ordering::Relaxed), 1);

        // 被中断的调用不交给卡片。
        let cancelled = [
            event(7, EventBody::ToolCall { call_id: "c3".into(), name: "count".into(), input: json!({}).into() }),
            event(8, EventBody::ToolResult { call_id: "c3".into(), output: INTERRUPTED_TOOL_OUTPUT.into(), is_error: true }),
        ];
        resolver.record_calls(&cancelled);
        assert!(!resolver.needs_plugins(&cancelled));
        resolver.resolve(&cancelled);
        assert_eq!(plugins.asked.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    struct NotesUi;

    impl UiPlugin for NotesUi {
        fn id(&self) -> &'static str {
            "notes"
        }

        fn apply_ui(&self, cx: &mut UiScope<'_>) {
            cx.transcript_renderer("notes/add", NoteRows);
            cx.transcript_renderer("notes/count", NoteRows);
        }
    }

    #[test]
    fn plugin_events_become_rows_only_with_a_renderer() {
        let events: Vec<SessionEvent> = [
            EventBody::UserMessage { text: "记一下".into() },
            EventBody::ext("notes/add", json!({"text": "周五发版"})).unwrap(),
            EventBody::ext("notes/count", json!({})).unwrap(),
            EventBody::ext("other/thing", json!({"text": "没人认领"})).unwrap(),
        ]
        .into_iter()
        .zip(1..)
        .map(|(body, seq)| SessionEvent { seq, time: 0, body })
        .collect();

        let plugins: Vec<Box<dyn UiPlugin>> = vec![Box::new(NotesUi)];
        let ui = Rc::new(UiRegistry::assemble(&plugins, |_| Some(dshr_context::Status::Active)));
        let t = Transcript::from_events(ui, &events);
        assert_eq!(t.rows.len(), 2);
        let Row::Ext(row) = &t.rows[1] else { panic!("应当是插件事件行") };
        assert_eq!((&*row.kind, row.data.downcast_ref::<String>().map(String::as_str)), ("notes/add", Some("周五发版")));
        // 不占一行的事件也更新了界面状态。
        assert_eq!(t.state().get::<usize>(), Some(&2));

        // 后端那一半停用时界面这一半不装：事件留在日志里，对话区不显示。
        let ui = Rc::new(UiRegistry::assemble(&plugins, |_| Some(dshr_context::Status::Disabled)));
        assert_eq!(Transcript::from_events(ui.clone(), &events).rows.len(), 1);

        // 没有界面插件认领时，后端给出的一行字（JS 插件的 `label`）显示成一条提示；给不出就不显示。
        let labels = FakePlugins {
            label: Box::new(|kind, data| (kind == "notes/add").then(|| format!("记下：{}", data["text"].as_str().unwrap_or_default()))),
            ..FakePlugins::default()
        };
        let views = PluginViewResolver::new(Arc::new(labels)).resolve(&events);
        let t = Transcript::new(ui).with_plugin_views(views).replay(&events);
        assert_eq!(t.rows.len(), 2);
        let Row::Notice { text, error: false } = &t.rows[1] else { panic!("应当是提示行：{:?}", t.rows[1]) };
        assert_eq!(&**text, "记下：周五发版");
    }

    #[test]
    fn js_plugins_draw_event_trees_and_tool_cards() {
        let events: Vec<SessionEvent> = [
            EventBody::ext("chart/point", json!({"value": 0.5})).unwrap(),
            EventBody::ToolCall { call_id: "c1".into(), name: "count".into(), input: json!({"text": "a\nb"}).into() },
            EventBody::ToolResult { call_id: "c1".into(), output: "2".into(), is_error: false },
            EventBody::ToolCall { call_id: "c2".into(), name: "other".into(), input: json!({}).into() },
            EventBody::ToolResult { call_id: "c2".into(), output: "x".into(), is_error: false },
        ]
        .into_iter()
        .zip(1..)
        .map(|(body, seq)| SessionEvent { seq, time: 0, body })
        .collect();
        let plugins = FakePlugins {
            tree: Box::new(|kind, data| (kind == "chart/point").then(|| json!({"type": "progress", "value": data["value"]}))),
            card: Box::new(|tool, input, output, _| {
                (tool == "count").then(|| dshr_agent::ToolCardView {
                    title: Some(format!("{} lines", input["text"].as_str().unwrap_or_default().lines().count())),
                    body: Some(json!({"type": "text", "text": output})),
                })
            }),
            ..FakePlugins::default()
        };
        let views = PluginViewResolver::new(Arc::new(plugins)).resolve(&events);
        let t = Transcript::new(Rc::default()).with_plugin_views(views).replay(&events);
        let Row::Plugin { tree } = &t.rows[0] else { panic!("应当是插件画的行：{:?}", t.rows[0]) };
        assert_eq!(tree["type"], "progress");
        let count = tool(&t.rows[1], 0);
        let card = count.data.as_ref().and_then(|data| data.downcast_ref::<PluginCard>()).expect("应当有插件卡片");
        assert_eq!((card.0.title.as_deref(), card.0.body.as_ref().map(|body| body["text"].clone())), (Some("2 lines"), Some(json!("2"))));
        let other = tool(&t.rows[1], 1);
        assert!(other.data.is_none());
    }

    #[test]
    fn tool_input_preview_matches_full_serialization() {
        let long = serde_json::json!({"path": "a.rs", "content": "中文".repeat(10_000)});
        assert_eq!(preview(&json_prefix(&long, INPUT_PREVIEW_CHARS * 4)), preview(&long.to_string()));
        let short = serde_json::json!({"path": "a.rs"});
        assert_eq!(json_prefix(&short, INPUT_PREVIEW_CHARS * 4), short.to_string());
    }

    #[test]
    fn parses_change_reports() {
        let output = "Edited src/a.rs (+1 -1)\n\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n fn main() {\n-    old();\n+    new();\n";
        let report = parse_change_report(output).unwrap();
        assert_eq!((report.added, report.removed), (1, 1));
        assert_eq!(
            report.lines,
            vec![
                (DiffLineKind::Hunk, "@@ -1,2 +1,2 @@"),
                (DiffLineKind::Context, " fn main() {"),
                (DiffLineKind::Removed, "-    old();"),
                (DiffLineKind::Added, "+    new();"),
            ]
        );
        assert!(parse_change_report("exit code: 0\n--- stdout ---").is_none());
    }

    #[test]
    fn change_summary_groups_consecutive_lines_of_a_kind() {
        let output = "Wrote a.rs (+3 -1)\n\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,4 @@\n keep\n-old\n+one\n+two\n+three\n";
        let summary = parse_change_report(output).unwrap().to_owned();
        assert_eq!(
            summary.blocks,
            vec![
                (DiffLineKind::Hunk, Arc::from("@@ -1,2 +1,4 @@")),
                (DiffLineKind::Context, Arc::from(" keep")),
                (DiffLineKind::Removed, Arc::from("-old")),
                (DiffLineKind::Added, Arc::from("+one\n+two\n+three")),
            ]
        );
    }

    #[test]
    fn dirty_rows_track_what_the_list_must_remeasure() {
        let mut t = Transcript::default();
        t.apply(&durable(EventBody::UserMessage { text: "hi".into() }));
        assert_eq!(t.take_dirty(), [0]);
        assert!(t.take_dirty().is_empty());

        // 流式增量反复改同一行，只记一次。
        for piece in ["你", "好", "！"] {
            t.apply(&LiveEvent::Chunk(Chunk::Text(piece.into())));
        }
        assert_eq!(t.take_dirty(), [1]);

        // 最终消息取代流式行：行数不变，但那一行要重新测量。
        t.apply(&assistant(vec![ContentBlock::text("你好！")]));
        assert_eq!((t.rows.len(), t.take_dirty()), (2, vec![1]));

        // 工具卡片的输出、审批、展开，分组的展开都算内容变化。
        t.apply(&durable(EventBody::ToolCall { call_id: "c1".into(), name: "run_shell".into(), input: json!({}).into() }));
        t.take_dirty();
        t.apply(&LiveEvent::Chunk(Chunk::ToolOutput { call_id: "c1".into(), text: "building…\n".into() }));
        assert_eq!(t.take_dirty(), [2]);
        t.toggle_tool("c1");
        assert_eq!(t.take_dirty(), [2]);
        assert!(t.toggle_group(2));
        assert_eq!(t.take_dirty(), [2]);

        // 一轮结束时还在运行的卡片变成已取消。
        t.apply(&durable(EventBody::TurnEnd { turn: 1, reason: TurnEndReason::Interrupted, error: None }));
        assert_eq!(t.take_dirty(), [2, 3]);

        // 从历史重建的记录没有待通知的变化。
        let events = [SessionEvent { seq: 1, time: 0, body: EventBody::UserMessage { text: "hi".into() } }];
        assert!(Transcript::from_events(Rc::default(), &events).take_dirty().is_empty());
    }

    /// 打开会话时分段重放（每段长短不一，可能把一轮从中间切开），结果和一次放完一样。
    #[test]
    fn replaying_in_parts_matches_replaying_at_once() {
        let mut events = Vec::new();
        for turn in 0..20u32 {
            let seq = |offset: u64| u64::from(turn) * 4 + offset;
            events
                .push(SessionEvent { seq: seq(0), time: 0, body: EventBody::UserMessage { text: format!("第 {turn} 个问题").into() } });
            events.push(SessionEvent {
                seq: seq(1),
                time: 0,
                body: EventBody::ToolCall { call_id: format!("c{turn}"), name: "run_shell".into(), input: json!({}).into() },
            });
            events.push(SessionEvent {
                seq: seq(2),
                time: 0,
                body: EventBody::TurnEnd { turn, reason: TurnEndReason::Completed, error: None },
            });
        }
        let whole = Transcript::from_events(builtin_ui(), &events);
        for step in [1, 2, 5, 7, 64] {
            let mut parts = Transcript::new(builtin_ui());
            for chunk in events.chunks(step) {
                parts.replay_part(chunk);
            }
            parts.finish_replay();
            assert_eq!(format!("{:?}", parts.rows), format!("{:?}", whole.rows), "每段 {step} 个事件");
            assert!(parts.take_dirty().is_empty());
        }
    }

    #[test]
    fn results_cache_reports_and_release_live_output() {
        let mut t = Transcript::new(builtin_ui());
        t.apply(&durable(EventBody::ToolCall { call_id: "e1".into(), name: "edit_file".into(), input: json!({}).into() }));
        t.apply(&LiveEvent::Chunk(Chunk::ToolOutput { call_id: "e1".into(), text: "x".into() }));
        let output = "Edited a.rs (+1 -1)\n\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
        t.apply(&durable(EventBody::ToolResult { call_id: "e1".into(), output: output.into(), is_error: false }));
        let edited = tool(&t.rows[0], 0);
        assert!(edited.live.is_empty());
        let report = edited.data.as_ref().and_then(|data| data.downcast_ref::<ChangeSummary>()).expect("应当解析出改动报告");
        assert_eq!((report.added, report.removed, report.blocks.len()), (1, 1, 3));

        // 失败的调用不解析报告，即使输出长得像。
        t.apply(&durable(EventBody::ToolCall { call_id: "e2".into(), name: "edit_file".into(), input: json!({}).into() }));
        t.apply(&durable(EventBody::ToolResult { call_id: "e2".into(), output: output.into(), is_error: true }));
        let failed = tool(&t.rows[0], 1);
        assert!(failed.data.is_none() && matches!(failed.status, ToolStatus::Done { is_error: true, .. }));

        // 别的工具的输出即使长得像改动报告，也按普通输出显示。
        t.apply(&durable(EventBody::ToolCall { call_id: "s1".into(), name: "run_shell".into(), input: json!({}).into() }));
        t.apply(&durable(EventBody::ToolResult { call_id: "s1".into(), output: output.into(), is_error: false }));
        assert!(tool(&t.rows[0], 2).data.is_none());
    }

    #[test]
    fn failed_stream_keeps_partial_text_as_final_row() {
        let mut t = Transcript::default();
        t.apply(&LiveEvent::Chunk(Chunk::Text("**半截**".into())));
        t.apply(&durable(EventBody::TurnEnd { turn: 1, reason: TurnEndReason::Error, error: Some("boom".into()) }));
        assert!(matches!(&t.rows[0], Row::Assistant { text, .. } if &**text == "**半截**"));
        assert_eq!(t.rows[0].plain().as_deref(), Some("半截"));
        assert!(matches!(&t.rows[1], Row::Notice { error: true, .. }));
    }

    #[test]
    fn replay_shares_event_text_instead_of_copying() {
        let reply: std::sync::Arc<str> = "**你好**".into();
        let output: std::sync::Arc<str> = "x".repeat(1000).into();
        let mut t = Transcript::default();
        t.apply(&assistant(vec![ContentBlock::Text { text: reply.clone() }]));
        t.apply(&durable(EventBody::ToolCall { call_id: "c".into(), name: "run_shell".into(), input: json!({}).into() }));
        t.apply(&durable(EventBody::ToolResult { call_id: "c".into(), output: output.clone(), is_error: false }));
        assert!(matches!(&t.rows[0], Row::Assistant { text, .. } if std::sync::Arc::ptr_eq(text, &reply)));
        assert!(matches!(&tool(&t.rows[1], 0).status, ToolStatus::Done { output: shown, .. } if std::sync::Arc::ptr_eq(shown, &output)));
        // 纯文本要时才算；没有标记的回复直接共用原文。
        assert_eq!(t.rows[0].plain().as_deref(), Some("你好"));
        let mut plain_only = Transcript::default();
        let text: std::sync::Arc<str> = "没有标记".into();
        plain_only.apply(&assistant(vec![ContentBlock::Text { text: text.clone() }]));
        assert!(std::sync::Arc::ptr_eq(&plain_only.rows[0].plain().unwrap(), &text));
        // 多块文本仍按空行拼起来。
        let mut joined = Transcript::default();
        joined.apply(&assistant(vec![ContentBlock::text("甲"), ContentBlock::text("乙")]));
        assert!(matches!(&joined.rows[0], Row::Assistant { text, .. } if &**text == "甲\n\n乙"));
    }

    #[test]
    fn streaming_plain_text_follows_the_text() {
        let mut t = Transcript::default();
        t.apply(&LiveEvent::Chunk(Chunk::Text("一".into())));
        assert_eq!(t.rows[0].plain().as_deref(), Some("一"));
        // 文本变长后不会沿用旧的纯文本。
        t.apply(&LiveEvent::Chunk(Chunk::Text("**二**".into())));
        assert_eq!(t.rows[0].plain().as_deref(), Some("一二"));
    }

    #[test]
    fn tool_images_are_decoded_once() {
        // 1x1 的透明 PNG。
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
        let mut t = Transcript::default();
        t.apply(&durable(EventBody::ToolCall { call_id: "c".into(), name: "run_shell".into(), input: json!({}).into() }));
        t.apply(&durable(EventBody::Ext {
            kind: dshr_core::MEDIA_IMAGES.into(),
            data: json!({ "call_id": "c", "images": [
                { "media_type": "image/png", "data": png },
                { "media_type": "image/bmp", "data": png },
                { "media_type": "image/png", "data": "not base64!" },
            ] }),
        }));
        let shell = tool(&t.rows[0], 0);
        assert_eq!(shell.images.len(), 1);
        assert_eq!(shell.images[0].format, gpui_kit::ImageFormat::Png);
    }

    #[test]
    fn tail_lines_takes_the_last_lines() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(tail_lines("a\r\nb\r\n", 5), "a\nb");
        assert_eq!(tail_lines("only", 3), "only");
        assert_eq!(tail_lines("", 3), "");
        let long: String = (0..1000).map(|n| format!("{n}\n")).collect();
        let expected = long.lines().collect::<Vec<_>>()[988..].join("\n");
        assert_eq!(tail_lines(&long, 12), expected);
    }

    #[test]
    fn plain_markdown_drops_markup() {
        let text = "# 标题\n\n**加粗** 与 `code`，见 [文档](https://x.y/z)。\n```rust\nlet a = 1;\n```\n> 引用\n- 列表 ![图](a.png)";
        assert_eq!(plain_markdown(text), "标题\n\n加粗 与 code，见 文档。\nlet a = 1;\n引用\n- 列表 图");
    }

    #[test]
    fn retry_drops_partial_stream() {
        let mut t = Transcript::default();
        t.apply(&LiveEvent::Chunk(Chunk::Text("半截".into())));
        t.apply(&durable(EventBody::AssistantAttempt { attempt: 1, error: "HTTP 503".into() }));
        assert_eq!(t.rows.len(), 1);
        assert!(matches!(&t.rows[0], Row::Notice { error: true, .. }));
        assert_eq!(t.activity(), &Activity::Retrying(1));

        // 重试时没定稿的思考也作废；分组里只剩它时整组去掉。
        let mut t = Transcript::default();
        t.apply(&LiveEvent::Chunk(Chunk::Thinking("想".into())));
        t.apply(&durable(EventBody::AssistantAttempt { attempt: 1, error: "HTTP 503".into() }));
        assert!(matches!(&t.rows[..], [Row::Notice { .. }]));
    }

    /// 连续的思考与工具调用归进同一组；正文把它们隔开，之后的思考、调用另起一组。
    #[test]
    fn thinking_and_tools_are_grouped_between_replies() {
        let call = |id: &str| durable(EventBody::ToolCall { call_id: id.into(), name: "read_file".into(), input: json!({}).into() });
        let think = |text: &str| ContentBlock::Thinking { thinking: text.into(), signature: None };
        let mut t = Transcript::default();
        t.apply(&durable(EventBody::UserMessage { text: "看看".into() }));
        t.apply(&assistant(vec![think("先读文件")]));
        t.apply(&call("a"));
        t.apply(&call("b"));
        t.apply(&assistant(vec![think("再想想"), ContentBlock::text("读完了，接着改。")]));
        t.apply(&call("c"));
        assert_eq!(t.rows.len(), 4, "{:?}", t.rows);
        assert!(matches!(&t.rows[1], Row::Work(group) if group.items.len() == 4 && group.tools().count() == 2));
        assert!(matches!(thought(&t.rows[1], 3), Thought::Done(text) if &**text == "再想想"));
        assert!(matches!(&t.rows[2], Row::Assistant { .. }));
        assert_eq!(tool(&t.rows[3], 0).call_id, "c");
        assert!(matches!(&t.rows[3], Row::Work(group) if group.is_active()));

        // 结果按调用号找回各自所在的组。
        t.apply(&durable(EventBody::ToolResult { call_id: "b".into(), output: "ok".into(), is_error: true }));
        assert!(matches!(&t.rows[1], Row::Work(group) if group.has_error()));
        assert_eq!(t.activity(), &Activity::Tool("read_file".into()));

        // 一轮结束：还在跑的调用取消，分组不再处于进行中。
        t.apply(&durable(EventBody::TurnEnd { turn: 1, reason: TurnEndReason::Interrupted, error: None }));
        assert_eq!(tool(&t.rows[3], 0).status, ToolStatus::Cancelled);
        assert!(matches!(&t.rows[3], Row::Work(group) if !group.is_active()));
    }

    /// 流式输出的思考在最终消息到达时定稿，结果和直接重放日志一样。
    #[test]
    fn streamed_groups_match_replayed_groups() {
        let message = EventBody::AssistantMessage {
            model: "m".into(),
            content: vec![ContentBlock::Thinking { thinking: "想好了".into(), signature: None }, ContentBlock::text("好")],
            stop_reason: None,
            usage: None,
        };
        let call = EventBody::ToolCall { call_id: "c".into(), name: "list_dir".into(), input: json!({}).into() };
        let mut live = Transcript::default();
        live.apply(&LiveEvent::Chunk(Chunk::Thinking("想".into())));
        live.apply(&LiveEvent::Chunk(Chunk::Thinking("好了".into())));
        assert_eq!(live.activity(), &Activity::Thinking);
        live.apply(&LiveEvent::Chunk(Chunk::Text("好".into())));
        live.apply(&durable(message.clone()));
        live.apply(&durable(call.clone()));
        let events = [SessionEvent { seq: 1, time: 0, body: message }, SessionEvent { seq: 2, time: 0, body: call }];
        let replayed = Transcript::from_events(Rc::default(), &events);
        assert_eq!(format!("{:?}", live.rows), format!("{:?}", replayed.rows));
    }
}
