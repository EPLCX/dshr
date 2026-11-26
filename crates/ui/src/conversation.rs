//! 对话区：首页（标题 + 居中输入框）、消息列表、工具卡片、审批与输入框。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use dshr_agent::{Decision, PermissionMode, Preset, Session, SteerMessage, SteerQueue};
use dshr_config::{ModelRef, ReasoningEffort};
use dshr_core::TurnEndReason;
use futures::FutureExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{IndentInline, InputEvent, OutdentInline, Textarea, TextareaState};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{Disableable as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::design::ControlSize as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::announce::{announce, announce_after};
use crate::backend::{Backend, ModelChoice, TurnOptions, UiApprover};
use crate::design::motion::Edge;
use crate::design::{
    MenuFocus, Tone, Typography as _, badge, brand_mark, card, description, divider, heading, hint, hover_fill, menu_choice, menu_item,
    menu_popover, mono_font, motion, palette, pressable, radius, readable_with_id, size, space, with_focus_ring,
};
use crate::plugin::{ComposerControl, PanelContext, UiRegistry};
use crate::transcript::{PluginViewResolver, Row, ToolRow, ToolStatus, Transcript, TurnFooter, tail_lines};

/// 图片附件气泡的图标（完整 Lucide 目录在 `gpui_kit::assets`，组件兼容枚举里没有图片图标）。
const IMAGE_ICON: gpui_kit::assets::IconName = gpui_kit::assets::IconName::Image;

actions!(
    dshr_transcript,
    [
        /// 焦点移到上一条消息。
        MessagePrev,
        /// 焦点移到下一条消息。
        MessageNext,
        MessageFirst,
        MessageLast,
        /// 复制焦点所在消息的纯文本。
        MessageCopy,
        /// 回到输入框。
        MessageLeave,
        /// 消息列表里按 Tab：走到下一个控件，下一个还没画出来时先把它滚出来（见 `on_transcript_tab`）。
        TranscriptTabNext,
        /// 消息列表里按 Shift+Tab。
        TranscriptTabPrev,
    ]
);

actions!(
    dshr_approval,
    [
        /// 审批面板：焦点移到上一个选项。
        ApprovalPrev,
        /// 审批面板：焦点移到下一个选项。
        ApprovalNext,
        /// 审批面板：允许这一次。
        ApprovalAllow,
        /// 审批面板：允许，并在本会话内不再询问这个工具。
        ApprovalAlways,
        /// 审批面板：拒绝。
        ApprovalDeny,
    ]
);

/// 审批面板（在输入卡片的位置）。
const APPROVAL_CONTEXT: &str = "ApprovalPrompt";

const MESSAGE_CONTEXT: &str = "TranscriptMessage";
/// 消息列表（焦点在列表里的任何控件上时）。
const TRANSCRIPT_CONTEXT: &str = "Transcript";
/// 发送后等多久再播报“正在回复”。
const ANNOUNCE_AFTER_SEND: Duration = Duration::from_millis(400);
/// 输入卡片外面那一层的 id（首页和对话页共用，见 `render`）。
const COMPOSER_SLOT: &str = "composer-slot";
/// Tab 往前找有控件的行时，最多试这么多行（每试一行要等它画出来）。
const TAB_SEARCH_LIMIT: usize = 64;

/// 消息上的按键。比组件库根上的 Ctrl+C（复制选中文字）深一层，焦点在消息上时复制整条消息。
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", MessagePrev, Some(MESSAGE_CONTEXT)),
        KeyBinding::new("down", MessageNext, Some(MESSAGE_CONTEXT)),
        KeyBinding::new("home", MessageFirst, Some(MESSAGE_CONTEXT)),
        KeyBinding::new("end", MessageLast, Some(MESSAGE_CONTEXT)),
        KeyBinding::new(if cfg!(target_os = "macos") { "cmd-c" } else { "ctrl-c" }, MessageCopy, Some(MESSAGE_CONTEXT)),
        KeyBinding::new("escape", MessageLeave, Some(MESSAGE_CONTEXT)),
        // 比组件库根上的 Tab 深一层：焦点在消息列表里时先到这里。
        KeyBinding::new("tab", TranscriptTabNext, Some(TRANSCRIPT_CONTEXT)),
        KeyBinding::new("shift-tab", TranscriptTabPrev, Some(TRANSCRIPT_CONTEXT)),
        // 审批面板：方向键选、数字键直接选、Esc 拒绝；Enter / 空格按下有焦点的那个选项（按钮自带）。
        KeyBinding::new("up", ApprovalPrev, Some(APPROVAL_CONTEXT)),
        KeyBinding::new("down", ApprovalNext, Some(APPROVAL_CONTEXT)),
        KeyBinding::new("1", ApprovalAllow, Some(APPROVAL_CONTEXT)),
        KeyBinding::new("y", ApprovalAllow, Some(APPROVAL_CONTEXT)),
        KeyBinding::new("2", ApprovalAlways, Some(APPROVAL_CONTEXT)),
        KeyBinding::new("3", ApprovalDeny, Some(APPROVAL_CONTEXT)),
        KeyBinding::new("n", ApprovalDeny, Some(APPROVAL_CONTEXT)),
        KeyBinding::new("escape", ApprovalDeny, Some(APPROVAL_CONTEXT)),
    ]);
}

/// 审批涉及的插件能力：授予能力（`plugin_grant`）时是要授予的那几项；修改已授权的插件（必须确认的 `plugin_write`）
/// 时是它已经被允许的能力。别的审批是 `None`。
fn approval_capabilities(backend: &Backend, tool: &str, input: &serde_json::Value, mandatory: bool) -> Option<(String, Vec<String>)> {
    let plugin = input.get("id").and_then(serde_json::Value::as_str)?.to_owned();
    match tool {
        "plugin_grant" => {
            let names: Vec<String> = input
                .get("capabilities")
                .and_then(serde_json::Value::as_array)
                .map(|items| items.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            Some((plugin, names))
        }
        "plugin_write" if mandatory => {
            let names = backend.plugin_granted(&plugin);
            (!names.is_empty()).then_some((plugin, names))
        }
        _ => None,
    }
}

/// 审批面板上的选项，按显示顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalChoice {
    Allow,
    Always,
    Deny,
}

impl ApprovalChoice {
    const ALL: [ApprovalChoice; 3] = [ApprovalChoice::Allow, ApprovalChoice::Always, ApprovalChoice::Deny];
}

/// 插队的一条消息在界面上的样子：交给 agent 之前（附件还在后台读）`pushed` 为假。
struct SteeredMessage {
    id: u64,
    text: String,
    attachments: Vec<PendingAttachment>,
    pushed: bool,
}

/// 通知外层（侧边栏）。
pub enum ConversationEvent {
    /// 会话列表需要刷新（新建、标题变化、一轮结束）。
    SessionsChanged,
    /// 用户在输入卡片上方选了工作目录。
    WorkspaceChosen(PathBuf),
    /// 插件的命令要打开它的一页（`{ open: page }`）。
    OpenPluginPage { plugin: String, page: String, title: String },
    /// 新会话发出了第一条消息：它当时的模式、权限、模型，留给之后的新会话沿用（只在这次运行期间）。
    NewSessionStarted(TurnOptions),
}

/// 斜杠菜单里的一项：插件的斜杠命令（`/名字`）或命令（`/插件:id`）。
#[derive(Clone)]
struct SlashItem {
    plugin: String,
    /// 斜杠命令的名字，或命令的 id。
    id: String,
    /// 输入框里要打的字（不含 `/`）。
    trigger: String,
    description: String,
    slash: bool,
}

/// 斜杠菜单最多列几项。
const SLASH_ITEMS: usize = 8;
/// `@` 菜单最多列几项；停止输入多久之后才去找。
const MENTION_ITEMS: usize = 10;
const MENTION_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// `@` 菜单里的一项。
#[derive(Clone)]
struct MentionRow {
    source: Arc<dyn dshr_agent::MentionSource>,
    item: dshr_agent::MentionItem,
}

/// 输入框末尾正在打的 `@字`（`@` 在开头或空白之后，后面没有空白）。
fn mention_token(text: &str) -> Option<&str> {
    let at = text.rfind('@')?;
    let token = &text[at + 1..];
    let starts_word = text[..at].chars().next_back().is_none_or(char::is_whitespace);
    (starts_word && !token.contains(char::is_whitespace)).then_some(token)
}

pub struct ConversationView {
    backend: Arc<Backend>,
    /// 消息列表和输入卡片各是一个子视图，见 [`TranscriptView`]、[`ComposerView`]。
    transcript_view: Entity<TranscriptView>,
    composer_view: Entity<ComposerView>,
    /// 界面插件登记的工具卡片、事件行、面板和菜单。
    ui: Rc<UiRegistry>,
    workspace: Option<PathBuf>,
    /// 当前会话；一轮对话运行期间被移交给后台任务，结束后放回。
    session: Option<Session>,
    session_id: Option<String>,
    /// 正在后台读 `session_id` 的日志（见 [`Self::open_session`]）。
    loading: bool,
    transcript: Transcript,
    /// 消息列表的虚拟化状态：只有看得见的行会被渲染，行数与 `transcript.rows` 保持一致（[`Self::sync_rows`]）。
    list: Entity<MessageScrollerState>,
    input: Entity<TextareaState>,
    options: TurnOptions,
    /// 写入 `session/options` 事件时用的、挂在界面线程上的实时事件出口（事件先给对话区，再进日志）。
    ui_event_sink: Option<tokio::sync::mpsc::UnboundedSender<dshr_agent::LiveEvent>>,
    /// 模型菜单的内容与当前模型的显示名。只在设置变化或换模型时重算，不在每次渲染时读配置。
    model_choices: Rc<Vec<ModelChoice>>,
    model_label: SharedString,
    model_menu_open: bool,
    /// 模型菜单现在显示哪一页（关上时回到模型列表）。
    model_menu_page: ModelMenuPage,
    /// 模型菜单里可滚动的模型列表：打开菜单、用方向键移动时把当前项滚进视口。
    model_scroll: ScrollHandle,
    permission_menu_open: bool,
    preset_menu_open: bool,
    /// 折叠起来的面板（输入卡片上方，如计划面板），按面板 id。
    collapsed_panels: HashSet<&'static str>,
    /// JS 插件加在输入卡片上方的面板（`ui.panels`），和上次建它们时插件的组成。
    plugin_panels: Vec<PluginPanel>,
    plugin_panels_generation: usize,
    /// 折叠起来的插件面板，按 `<插件>/<id>`。
    collapsed_plugin_panels: HashSet<String>,
    /// 输入卡片上已选的附件，随下一条消息发送。
    attachments: Vec<PendingAttachment>,
    /// `@` 提及：正在找的字，找到的条目，以及找的任务（换了字就丢掉旧的）。
    mention_query: Option<String>,
    mention_results: Vec<MentionRow>,
    _mention_task: Option<Task<()>>,
    /// 文件被拖进窗口：输入卡片高亮，提示可以松手添加附件。
    file_drag: bool,
    /// 运行期间输入的消息（连同附件），本轮正常结束后依次发送（对应原版的排队输入）。
    queue: Vec<(String, Vec<PendingAttachment>)>,
    /// 这一轮的插队队列（运行期间才有），和界面上还在等 agent 取走的插队消息。
    steer: Option<SteerQueue>,
    steered: Vec<SteeredMessage>,
    next_steer_id: u64,
    /// 本会话里选了“允许，不再询问”的工具：再来审批时直接放行。
    always_allow: HashSet<String>,
    /// 审批面板上每个选项的焦点（顺序同 [`ApprovalChoice::ALL`]）。
    approval_focus: Vec<FocusHandle>,
    /// 上次画审批面板时是哪个调用：换成新的审批时把焦点移过去。
    approval_shown: RefCell<Option<String>>,
    cancel: Option<CancellationToken>,
    error: Option<String>,
    /// 重新打开会话之后仍要显示的错误（这一轮的后台任务崩溃后，从日志重新打开会话时用）。
    /// （会话 id，错误）：只在重新打开的正是这个会话时显示。
    error_after_open: Option<(String, String)>,
    /// 插件（`turnStarted`）拦下了刚发的消息：下次画输入框时把文字放回去。
    restore_input: Option<String>,
    /// 每条消息（用户消息、回复、提示）的焦点：消息可以用 Tab / 方向键走到，读屏软件读出全文。
    message_focus: HashMap<usize, FocusHandle>,
    /// 上次聚焦的消息：Tab 走进消息列表时回到这一条（没有时是最后一条），列表里只有它是 Tab 停靠点。
    current_message: Option<usize>,
    /// 最后一条可聚焦消息的行号，`sync_rows` 里算一次；渲染每行时直接比，不必整表扫。
    last_message_row: Option<usize>,
    /// 每一行外层的焦点（不是 Tab 停靠点）：用来知道焦点在哪一行里，以及 Tab 走到还没画出来的行时，
    /// 先把焦点放在那一行上，等它画出来再走到它里面的第一个控件。
    row_focus: HashMap<usize, FocusHandle>,
    /// 每一行上次画在哪里，和消息列表的可视区域：Tab 走到的控件在视口外时把那一行滚进来。
    row_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    viewport_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 等这一行画出来就把焦点给这条消息（从首页发出第一条消息之后）。
    pending_message_focus: Option<usize>,
    /// 刚从首页发出第一条消息，等它出现在记录里。
    focus_sent_message: bool,
    /// Tab 正要走进的那一行（已经滚过去、焦点先放在行上），等它画出来再走到它里面的控件。
    pending_tab: Option<PendingTab>,
    /// 键盘焦点刚走到这一行：等它这一帧画完，按画出来的位置判断要不要滚动（见 [`Self::reveal_row`]）。
    pending_reveal: Option<usize>,
    /// “跳到最新消息”按钮。自己画（不用消息列表自带的）：自带的按下之后马上消失，焦点跟着丢了。
    jump_focus: FocusHandle,
    /// 测试用：`render_row` 被调用的次数（看哪些操作会让消息列表重画）。
    #[cfg(test)]
    rows_rendered: usize,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ConversationEvent> for ConversationView {}

impl ConversationView {
    pub fn new(backend: Arc<Backend>, workspace: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config = backend.config();
        let options = TurnOptions {
            model: config.default_model().unwrap_or_else(|| ModelRef::new("deepseek", "deepseek-flash")),
            effort: config.model.effort,
            permission: default_permission(&config),
            preset: default_preset(&config),
            plugin_preset: None,
        };

        let input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(t!("conversation.placeholder").to_string()).submit_on_enter(true).auto_grow(2, 10)
        });
        let conversation = cx.weak_entity();
        let transcript_view = cx.new(|_| TranscriptView { conversation: conversation.clone() });
        let composer_view = cx.new(|_| ComposerView { conversation });
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
            // 运行中按 Ctrl+Enter（macOS 上 Cmd+Enter）：插队，不等这一轮结束。
            InputEvent::PressEnter { shift: false, secondary } => {
                if *secondary && this.is_running() {
                    this.submit_steer(window, cx);
                } else {
                    this.submit(window, cx);
                }
            }
            // 发送按钮的可用状态、斜杠菜单取决于输入的内容：重画输入卡片就够了，不必通知整个对话视图。
            InputEvent::Change => {
                this.update_mentions(cx);
                this.composer_view.update(cx, |_, cx| cx.notify());
            }
            _ => {}
        });

        let list = cx.new(|cx| MessageScrollerState::new(0, cx));
        // 本视图在窗口里是缓存的（见 `WorkspaceView::render`），只有它自己被通知时才重新生成。
        // 输入框和消息列表的状态是普通实体而不是视图，它们变了（光标移动、选区、滚动位置）
        // 不会自动让视图失效，所以在这里转成通知。
        //
        // 输入框（光标每 500 毫秒闪一次、每打一个字）只通知输入卡片：子视图被通知时对话视图作为祖先也会重画，
        // 但不会触发下面的 `observe_self`，缓存着的消息列表不跟着重新生成。以前这里通知整个对话视图，
        // 输入框有焦点时（默认就有）视口里的消息每秒重建两次，打字时每个键重建一次。
        let observe_input = cx.observe(&input, |this, _, cx| this.composer_view.update(cx, |_, cx| cx.notify()));
        let observe_list = cx.observe(&list, |_, _, cx| cx.notify());
        // 本视图的状态一变（事件到达、滚动、展开卡片、焦点……）就让消息列表重画：它是缓存的视图，
        // 不会因为本视图重画而重新生成（同 `WorkspaceView` 对侧边栏的做法）。
        let observe_self = cx.observe_self(|this, cx| this.transcript_view.update(cx, |_, cx| cx.notify()));
        let model_choices = Rc::new(backend.model_choices());
        let model_label = config.model_display_name(&options.model).into();
        let ui = UiRegistry::global(cx);
        let transcript = Transcript::new(ui.clone());

        Self {
            backend,
            transcript_view,
            composer_view,
            workspace,
            session: None,
            session_id: None,
            loading: false,
            transcript,
            ui,
            list,
            input,
            options,
            ui_event_sink: None,
            model_choices,
            model_label,
            model_menu_open: false,
            model_menu_page: ModelMenuPage::Models,
            model_scroll: ScrollHandle::new(),
            permission_menu_open: false,
            preset_menu_open: false,
            collapsed_panels: HashSet::new(),
            plugin_panels: Vec::new(),
            plugin_panels_generation: 0,
            collapsed_plugin_panels: HashSet::new(),
            attachments: Vec::new(),
            mention_query: None,
            mention_results: Vec::new(),
            _mention_task: None,
            file_drag: false,
            queue: Vec::new(),
            steer: None,
            steered: Vec::new(),
            next_steer_id: 0,
            always_allow: HashSet::new(),
            approval_focus: ApprovalChoice::ALL.iter().map(|_| cx.focus_handle()).collect(),
            approval_shown: RefCell::new(None),
            cancel: None,
            error: None,
            error_after_open: None,
            restore_input: None,
            message_focus: HashMap::new(),
            current_message: None,
            last_message_row: None,
            row_focus: HashMap::new(),
            row_bounds: Rc::default(),
            viewport_bounds: Rc::default(),
            jump_focus: cx.focus_handle(),
            pending_tab: None,
            pending_reveal: None,
            pending_message_focus: None,
            focus_sent_message: false,
            #[cfg(test)]
            rows_rendered: 0,
            _subscriptions: vec![subscription, observe_input, observe_list, observe_self],
        }
    }

    /// 把对话记录的变化告诉消息列表：行数变了就增删列表项，内容变了的行重新测量高度。
    /// 每次改动 `self.transcript` 之后调用。
    fn sync_rows(&mut self, cx: &mut Context<Self>) {
        let rows = self.transcript.rows.len();
        let dirty = self.transcript.take_dirty();
        self.last_message_row = self.transcript.rows.iter().rposition(is_message);
        // 从首页发出的第一条消息出现了：等它画出来就把焦点给它。
        if self.focus_sent_message
            && let Some(index) = self.transcript.rows.iter().rposition(|row| matches!(row, Row::User { .. }))
        {
            self.focus_sent_message = false;
            self.current_message = Some(index);
            self.pending_message_focus = Some(index);
        }
        // 行没了（换会话、重新载入）就丢掉它们的焦点句柄和位置。
        self.row_focus.retain(|index, _| *index < rows);
        self.message_focus.retain(|index, _| *index < rows);
        self.row_bounds.borrow_mut().retain(|index, _| *index < rows);
        self.list.update(cx, |list, cx| {
            let known = list.item_count();
            let kept = known.min(rows);
            if known != rows {
                list.splice(kept..known, rows - kept, cx);
            }
            for index in dirty.into_iter().filter(|index| *index < kept) {
                list.remeasure_items(index..index + 1, cx);
            }
        });
    }

    /// 回到最新内容并继续跟随（发送消息、出现审批请求、打开会话时）。
    fn scroll_to_latest(&mut self, cx: &mut Context<Self>) {
        self.list.update(cx, |list, cx| list.scroll_to_end(cx));
    }

    pub fn set_workspace(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.is_running() {
            self.workspace = Some(path);
            cx.notify();
        }
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut App) {
        // 有审批在等时输入框的位置是审批面板：焦点给它的第一个选项。
        if self.transcript.pending_approval().is_some() {
            self.approval_focus[0].focus(window, cx);
            return;
        }
        self.input.read(cx).focus_handle(cx).focus(window, cx);
    }

    pub fn is_running(&self) -> bool {
        self.cancel.is_some()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn workspace(&self) -> Option<&PathBuf> {
        self.workspace.as_ref()
    }

    /// 还没有开始任何对话的空白会话，可以直接拿来当“新会话”。
    pub fn is_blank(&self) -> bool {
        self.session_id.is_none() && !self.is_running() && self.transcript.rows.is_empty()
    }

    /// 当前的模式、权限、模型（[`WorkspaceView`](crate::WorkspaceView) 在空会话被切走又切回时用来恢复）。
    pub fn turn_options(&self) -> &TurnOptions {
        &self.options
    }

    /// 输入框里没发出去的原文（不去掉首尾空白）：切走时由 [`WorkspaceView`](crate::WorkspaceView) 记下。
    pub fn draft(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    /// 切回来时放回没发出去的字；输入框里已经有字（比如插件刚放进去的）就不覆盖。
    pub fn restore_draft(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !text.is_empty() && self.input.read(cx).value().is_empty() {
            self.set_composer_text(text, window, cx);
        }
    }

    /// 恢复空会话上次的模式、权限、模型；只更新界面状态，不写日志（会话还没开始）。
    pub fn set_turn_options(&mut self, options: TurnOptions, cx: &mut Context<Self>) {
        self.model_label = self.backend.config().model_display_name(&options.model).into();
        self.options = options;
        cx.notify();
    }

    /// 有工具调用在等用户审批。
    pub fn needs_attention(&self) -> bool {
        self.transcript.rows.iter().any(|row| matches!(row, Row::Tool(tool) if tool.approval.is_some()))
    }

    /// 设置保存后刷新：当前模型被删掉时回到默认模型，并更新随语言变化的占位文字。
    pub fn reload_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = self.backend.config();
        let still_available = config
            .provider(&self.options.model.provider)
            .is_some_and(|provider| provider.models.iter().any(|model| model.id == self.options.model.model));
        if !still_available && let Some(model) = config.default_model() {
            self.options.model = model;
        }
        self.model_choices = Rc::new(self.backend.model_choices());
        self.model_label = config.model_display_name(&self.options.model).into();
        self.input.update(cx, |input, cx| input.set_placeholder(t!("conversation.placeholder").to_string(), window, cx));
        cx.notify();
    }

    /// 打开会话时恢复上次选的模式、权限、模型（日志里最后一条 `session/options`）。
    /// 字符串认不出的部分保留当前值：比如记录里选过的插件预设后来被卸载了。
    fn apply_stored_options(&mut self, stored: &dshr_agent::SessionOptions) {
        let preset = preset_from_id(&stored.preset);
        match &stored.plugin_preset {
            Some(key) => {
                if let Some((plugin, id)) = key.split_once('/')
                    && let Some(def) = self.backend.plugin_presets().into_iter().find(|def| def.plugin == plugin && def.id == id)
                {
                    self.options.preset = Preset::Standard;
                    self.options.plugin_preset = Some(def);
                }
            }
            None => {
                self.options.plugin_preset = None;
                if let Some(preset) = preset {
                    self.options.preset = preset;
                }
            }
        }
        // 旧版本的“只读”已经去掉，按“手动模式”恢复（见 `PermissionMode::from_id`）。
        if let Some(mode) = PermissionMode::from_id(&stored.permission) {
            self.options.permission = mode;
        }
        let config = self.backend.config();
        let available =
            config.provider(&stored.model_provider).is_some_and(|provider| provider.models.iter().any(|model| model.id == stored.model));
        if available {
            self.options.model = ModelRef::new(&stored.model_provider, &stored.model);
            self.model_label = config.model_display_name(&self.options.model).into();
        }
    }

    /// 当前选项变化时写进会话日志（只在会话已有日志时；新会话的首轮在 [`Self::start_turn`] 里写）。
    fn persist_options(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let plugin_preset = self.options.plugin_preset.as_ref().map(|preset| format!("{}/{}", preset.plugin, preset.id));
        let body = dshr_core::EventBody::SessionOptions {
            preset: preset_id(self.options.preset).to_owned(),
            plugin_preset,
            permission: self.options.permission.id().to_owned(),
            model_provider: self.options.model.provider.clone(),
            model: self.options.model.model.clone(),
        };
        // 出口挂了的话说明这个视图的会话还没由界面线程关掉；写不进日志只影响下次恢复，不算错误。
        if let Some(sink) = &self.ui_event_sink {
            session.append_event(body, sink).ok();
        }
        cx.notify();
    }

    pub fn open_session(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.is_running() {
            return;
        }
        // 先记下会话 id（侧边栏靠它找到这个视图，重复点击不会再开一个），日志在后台读、解析。
        // 读完之前不能发消息：不然会新建一个会话。
        self.session_id = Some(id.to_owned());
        self.session = None;
        self.loading = true;
        // “不再询问”只对当时那个会话有效。
        self.always_allow.clear();
        let open = self.backend.open_session(id);
        // JS 插件给事件、卡片画的东西也在后台一起要好：重放时在界面线程上一条条去问插件，
        // 插件事件多的会话打开时会卡住，插件正忙时每条要等几百毫秒。
        let mut resolver = PluginViewResolver::new(self.backend.clone());
        let task = cx.background_executor().spawn(async move {
            open().map(|session| {
                let views = resolver.resolve(session.events());
                (session, views)
            })
        });
        let id = id.to_owned();
        cx.spawn(async move |this, cx| {
            let opened = task.await;
            this.update(cx, |this, cx| {
                // 读的时候换了别的会话（比如分叉后又切走）：这份结果作废。
                if this.session_id.as_deref() != Some(id.as_str()) {
                    return;
                }
                this.loading = false;
                match opened {
                    Ok((session, views)) => {
                        if let Some(stored) = session.stored_options() {
                            this.apply_stored_options(&stored);
                        }
                        // 打开已有会话后，再在输入卡片上改选项时直接写回日志（见 `persist_options`）。
                        let (event_tx, events) = mpsc::unbounded_channel();
                        this.ui_event_sink = Some(event_tx);
                        cx.spawn(async move |_, _cx| {
                            // 实时事件由界面线程丢掉：对话区已经整份重放过日志。
                            let mut events = events;
                            while events.recv().await.is_some() {}
                        })
                        .detach();
                        this.transcript = Transcript::new(this.ui.clone()).with_plugin_views(views).replay(session.events());
                        this.session = Some(session);
                        this.error = this.error_after_open.take().filter(|(session, _)| *session == id).map(|(_, error)| error);
                        this.message_focus.clear();
                        this.current_message = None;
                        let rows = this.transcript.rows.len();
                        this.list.update(cx, |list, cx| {
                            list.reset(rows, cx);
                            list.scroll_to_end(cx);
                        });
                    }
                    Err(error) => this.error = Some(t!("conversation.open_failed", error = error).to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// 从第 `turn` 轮结束处分叉出新会话并切换过去。
    fn fork_at(&mut self, turn: u32, cx: &mut Context<Self>) {
        let Some(id) = self.session_id.clone() else {
            return;
        };
        let fork = self.backend.fork_session(&id, turn);
        let task = cx.background_executor().spawn(async move { fork() });
        cx.spawn(async move |this, cx| {
            let forked = task.await;
            this.update(cx, |this, cx| match forked {
                // 复制期间用户切走了、或者开始了新的一轮：不再跳过去，新会话照样出现在侧边栏里。
                Ok(new_id) => {
                    if this.session_id.as_deref() == Some(id.as_str()) && !this.is_running() {
                        this.open_session(&new_id, cx);
                    }
                    cx.emit(ConversationEvent::SessionsChanged);
                }
                Err(error) => this.show_error(t!("conversation.fork_failed", error = error).to_string(), cx),
            })
            .ok();
        })
        .detach();
    }

    fn choose_workspace(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("conversation.choose_workspace").to_string().into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = picked.await
                && let Some(path) = paths.pop()
            {
                this.update(cx, |this, cx| {
                    this.set_workspace(path.clone(), cx);
                    cx.emit(ConversationEvent::WorkspaceChosen(path));
                })
                .ok();
            }
        })
        .detach();
    }

    /// 插件替用户发来的一条消息（`host.session.send`）：对话区先加一行“由插件 X 发送”，正在运行时排队。
    pub fn plugin_send(&mut self, text: String, plugin: &str, cx: &mut Context<Self>) {
        self.transcript.notice(t!("conversation.sent_by_plugin", plugin = plugin).to_string());
        self.sync_rows(cx);
        if self.is_running() {
            self.queue.push((text, Vec::new()));
        } else if !self.start_turn(text, Vec::new(), cx) {
            tracing::warn!(plugin, "插件发的消息没能开始一轮（可能缺少工作区或 API Key）");
        }
        cx.notify();
    }

    /// 停止正在跑的这一轮（插件的 `host.session.stop`）。
    pub fn plugin_stop(&mut self, cx: &mut Context<Self>) {
        if self.is_running() {
            self.stop(cx);
        }
    }

    /// 现在装着的插件提供的斜杠命令和命令。
    fn slash_items(&self, _cx: &App) -> Vec<SlashItem> {
        let mut items = Vec::new();
        for surface in self.backend.surfaces() {
            let plugin = surface.plugin().to_owned();
            items.extend(surface.slash_commands().iter().map(|command| SlashItem {
                plugin: plugin.clone(),
                id: command.name.clone(),
                trigger: command.name.clone(),
                description: command.description.clone(),
                slash: true,
            }));
            items.extend(surface.commands().iter().map(|command| SlashItem {
                plugin: plugin.clone(),
                id: command.id.clone(),
                trigger: format!("{plugin}:{}", command.id),
                description: command.title.clone(),
                slash: false,
            }));
        }
        items
    }

    /// 执行插件的一个命令（快捷键触发）。
    pub fn run_command(&mut self, plugin: String, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let item = SlashItem { trigger: format!("{plugin}:{id}"), plugin, id, description: String::new(), slash: false };
        self.run_plugin_command(item, String::new(), window, cx);
    }

    /// 执行插件的斜杠命令或命令（在后台线程上），按返回值改输入框、发消息、打开页面或显示一句话。
    fn run_plugin_command(&mut self, item: SlashItem, args: String, window: &mut Window, cx: &mut Context<Self>) {
        let (backend, workspace) = (self.backend.clone(), self.workspace.clone());
        let plugin = item.plugin.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let surface = backend.surface(&item.plugin).ok_or_else(|| t!("plugin_page.gone", plugin = item.plugin).to_string())?;
                    surface.run_command(&item.id, item.slash, &args, workspace.as_deref())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(dshr_agent::CommandResult::Nothing) => {}
                    Ok(dshr_agent::CommandResult::Message(text)) => {
                        announce(text.clone(), cx);
                        this.transcript.notice(text);
                        this.sync_rows(cx);
                    }
                    Ok(dshr_agent::CommandResult::Insert(text)) => this.set_composer_text(&text, window, cx),
                    Ok(dshr_agent::CommandResult::Send(text)) => {
                        if this.is_running() {
                            this.queue.push((text, Vec::new()));
                        } else {
                            this.start_turn(text, Vec::new(), cx);
                        }
                    }
                    Ok(dshr_agent::CommandResult::Open(page)) => {
                        cx.emit(ConversationEvent::OpenPluginPage { plugin: plugin.clone(), title: plugin.clone(), page });
                    }
                    Err(error) => this.show_error(error, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 输入框变了：末尾在打 `@字` 时，停一会儿到后台问各个来源（工作区文件、插件、MCP 资源）。
    fn update_mentions(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        let Some(query) = mention_token(&text).map(str::to_owned) else {
            if self.mention_query.take().is_some() {
                self.mention_results.clear();
                self._mention_task = None;
            }
            return;
        };
        if self.mention_query.as_deref() == Some(query.as_str()) {
            return;
        }
        self.mention_query = Some(query.clone());
        let (backend, workspace) = (self.backend.clone(), self.workspace.clone());
        self._mention_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MENTION_DEBOUNCE).await;
            let search = query.clone();
            let rows = cx
                .background_executor()
                .spawn(async move {
                    let mut rows = Vec::new();
                    for source in backend.mention_sources() {
                        match source.search(&search, workspace.as_deref()) {
                            Ok(items) => rows.extend(items.into_iter().map(|item| MentionRow { source: source.clone(), item })),
                            Err(error) => tracing::warn!(source = source.id(), error, "提及来源没有给出结果"),
                        }
                        if rows.len() >= MENTION_ITEMS * 3 {
                            break;
                        }
                    }
                    rows
                })
                .await;
            this.update(cx, |this, cx| {
                if this.mention_query.as_deref() == Some(query.as_str()) {
                    this.mention_results = rows;
                    this.composer_view.update(cx, |_, cx| cx.notify());
                }
            })
            .ok();
        }));
    }

    /// 选中一项提及：在后台解析成文字，变成一个附件，把输入框末尾的 `@字` 去掉。
    fn pick_mention(&mut self, row: MentionRow, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if let Some(token) = mention_token(&text) {
            let kept = text[..text.len() - token.len() - 1].to_owned();
            self.set_composer_text(&kept, window, cx);
        }
        self.mention_query = None;
        self.mention_results.clear();
        let workspace = self.workspace.clone();
        cx.spawn(async move |this, cx| {
            let resolving = row.clone();
            let resolved =
                cx.background_executor().spawn(async move { resolving.source.resolve(&resolving.item, workspace.as_deref()) }).await;
            this.update(cx, |this, cx| match resolved {
                Ok(content) => {
                    let path = format!("@{}:{}", row.source.id(), row.item.id.replace('\n', " "));
                    this.attachments.push(PendingAttachment::Mention { label: content.title, path, content: content.text });
                    cx.notify();
                }
                Err(error) => this.show_error(error, cx),
            })
            .ok();
        })
        .detach();
    }

    /// `@` 菜单：输入框末尾在打 `@字` 时列出找到的条目，点一下变成附件。
    fn render_mention_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.mention_query.as_ref()?;
        if self.mention_results.is_empty() {
            return None;
        }
        let p = palette(cx);
        let rows = self.mention_results.iter().take(MENTION_ITEMS).enumerate().map(|(index, row)| {
            let source = match row.source.id() {
                "file" => t!("conversation.mention_files").to_string(),
                "mcp" => "MCP".to_owned(),
                _ => row.source.label().to_owned(),
            };
            let detail = row.item.detail.clone().unwrap_or_default();
            let picked = row.clone();
            pressable(("mention-item", index), &p)
                .accessibility_label(format!("{source}: {}", row.item.title))
                .aria_description(detail.clone())
                .flex()
                .items_center()
                .relative()
                .gap(px(space::M))
                .min_h(px(size::LIST_ITEM))
                .px(px(space::S))
                .rounded(px(radius::SM))
                .cursor_pointer()
                .child(hover_fill(&p, radius::SM, false))
                .child(div().flex_none().text_caption().text_color(p.text_secondary).child(source))
                .child(div().flex_none().text_label().child(row.item.title.clone()))
                .child(div().flex_1().min_w(px(0.)).truncate().text_caption().text_color(p.text_secondary).child(detail))
                .on_click(cx.listener(move |this, _, window, cx| this.pick_mention(picked.clone(), window, cx)))
        });
        Some(suggestion_list("mention-menu", rows.collect(), &p))
    }

    /// 斜杠菜单：输入框以 `/` 开头、还在打名字时列出匹配的命令，点一下执行（有参数的先补全名字）。
    fn render_slash_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let text = self.input_text(cx);
        let typed = text.strip_prefix('/')?;
        if typed.contains(char::is_whitespace) {
            return None;
        }
        let items: Vec<SlashItem> =
            self.slash_items(cx).into_iter().filter(|item| item.trigger.starts_with(typed)).take(SLASH_ITEMS).collect();
        if items.is_empty() {
            return None;
        }
        let p = palette(cx);
        let rows = items.into_iter().enumerate().map(|(index, item)| {
            let label = format!("/{}", item.trigger);
            let picked = item.clone();
            pressable(("slash-item", index), &p)
                .accessibility_label(label.clone())
                // 说明放在描述里：读屏软件先读出命令名，停一下再读说明。
                .aria_description(item.description.clone())
                .flex()
                .items_center()
                .relative()
                .gap(px(space::M))
                .min_h(px(size::LIST_ITEM))
                .px(px(space::S))
                .rounded(px(radius::SM))
                .cursor_pointer()
                .child(hover_fill(&p, radius::SM, false))
                .child(div().flex_none().text_label().text_mono(mono_font(cx)).child(label))
                .child(div().flex_1().min_w(px(0.)).truncate().text_caption().text_color(p.text_secondary).child(item.description.clone()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.input.update(cx, |input, cx| input.set_value(String::new(), window, cx));
                    this.run_plugin_command(picked.clone(), String::new(), window, cx);
                }))
        });
        Some(suggestion_list("slash-menu", rows.collect(), &p))
    }

    /// 输入框里现在的字（插件的 `host.composer.get`）。
    pub fn composer_text(&self, cx: &App) -> String {
        self.input_text(cx)
    }

    /// 替换输入框里的字（插件的 `host.composer.set`、斜杠命令的 `{ insert }`）。
    pub fn set_composer_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        cx.notify();
    }

    /// 会话标题（侧边栏里那个）；还没有时是空的。
    pub fn title(&self) -> String {
        self.session.as_ref().and_then(|session| session.title().map(str::to_owned)).unwrap_or_default()
    }

    pub(crate) fn refresh_title(&mut self, cx: &mut Context<Self>) {
        if self.session.as_mut().is_some_and(Session::refresh_title) {
            cx.notify();
        }
    }

    fn input_text(&self, cx: &App) -> String {
        self.input.read(cx).value().trim().to_owned()
    }

    /// 输入框提交：空闲时立即开始一轮，运行中则加入队列。
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_text(cx);
        // `/名字 参数`：插件的斜杠命令或命令，不发给模型。
        if let Some(rest) = text.strip_prefix('/') {
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if let Some(item) = self.slash_items(cx).into_iter().find(|item| item.trigger == name) {
                self.input.update(cx, |input, cx| input.set_value(String::new(), window, cx));
                self.run_plugin_command(item, args.trim().to_owned(), window, cx);
                return;
            }
        }
        // 只发附件（比如一张图）不打字也可以。
        if (text.is_empty() && self.attachments.is_empty()) || self.workspace.is_none() {
            return;
        }
        let attachments = self.attachments.clone();
        let from_home = self.transcript.rows.is_empty();
        let accepted = if self.is_running() {
            self.queue.push((text, attachments));
            announce(t!("a11y.queued").to_string(), cx);
            cx.notify();
            true
        } else {
            self.start_turn(text, attachments, cx)
        };
        // 没能开始（如缺少 API Key）时保留输入和附件，方便用户改好设置后直接重发。
        if accepted {
            // 从新会话（首页）发出第一条消息：焦点放到刚发出的这句话上，接着就能用 ↑ / ↓ 在消息之间走、
            // 读回复；Esc 回到输入框。等这一行画出来再给（见 `render_row`），讲述人才收得到。
            // 这句话由会话事件加进记录（稍后才到），到了之后在 `sync_rows` 里接着处理。
            self.focus_sent_message = from_home;
            self.attachments.clear();
            self.input.update(cx, |input, cx| input.set_value(String::new(), window, cx));
        }
    }

    /// 运行中按 Ctrl+Enter：输入框里的消息插队，当前这一步结束后就交给模型。
    fn submit_steer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_text(cx);
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        if text.starts_with('/') {
            // 斜杠命令不发给模型，照常处理。
            self.submit(window, cx);
            return;
        }
        let attachments = std::mem::take(&mut self.attachments);
        self.steer_message(text, attachments, cx);
        self.input.update(cx, |input, cx| input.set_value(String::new(), window, cx));
    }

    /// 把排队的第 `index` 条消息改为插队。
    fn steer_queued(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.is_running() || index >= self.queue.len() {
            return;
        }
        let (text, attachments) = self.queue.remove(index);
        self.steer_message(text, attachments, cx);
    }

    /// 插队：交给这一轮的 [`SteerQueue`]。有附件时先在后台读好再交；这一轮已经结束（没有队列）时退回普通排队。
    fn steer_message(&mut self, text: String, attachments: Vec<PendingAttachment>, cx: &mut Context<Self>) {
        let Some(queue) = self.steer.clone().filter(|_| self.is_running()) else {
            self.queue.push((text, attachments));
            cx.notify();
            return;
        };
        if let Err(error) = check_attachments(&attachments) {
            self.queue.push((text, attachments));
            self.show_error(error, cx);
            return;
        }
        let id = self.next_steer_id;
        self.next_steer_id += 1;
        let pushed = attachments.is_empty();
        if pushed {
            queue.push(SteerMessage { id, text: text.clone(), attachments: Vec::new() });
        } else {
            let pending = attachments.clone();
            let message = text.clone();
            let read = cx.background_executor().spawn(async move { read_attachments(&pending) });
            cx.spawn(async move |this, cx| {
                let read = read.await;
                this.update(cx, |this, cx| {
                    let Some(index) = this.steered.iter().position(|steered| steered.id == id) else { return };
                    let live = this.steer.as_ref().is_some_and(|current| current.same(&queue));
                    match read {
                        Ok(read) if live => {
                            queue.push(SteerMessage { id, text: message, attachments: user_attachments(read) });
                            this.steered[index].pushed = true;
                        }
                        // 读附件期间这一轮已经结束：留给收尾时放回排队（见 `finish_steering`）。
                        Ok(_) => {}
                        Err(error) => {
                            let steered = this.steered.remove(index);
                            this.queue.insert(0, (steered.text, steered.attachments));
                            this.show_error(error, cx);
                        }
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        self.steered.push(SteeredMessage { id, text, attachments, pushed });
        announce(t!("a11y.steered").to_string(), cx);
        cx.notify();
    }

    /// 取消一条还没被 agent 取走的插队消息，放回排队的最前面。
    fn unsteer(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(index) = self.steered.iter().position(|steered| steered.id == id) else { return };
        let taken_back = !self.steered[index].pushed || self.steer.as_ref().is_some_and(|queue| queue.remove(id));
        if taken_back {
            let steered = self.steered.remove(index);
            self.queue.insert(0, (steered.text, steered.attachments));
        }
        cx.notify();
    }

    /// 丢掉已经被 agent 接进对话的插队消息（它们这时已经作为用户消息出现在记录里）。
    fn prune_steered(&mut self) {
        let queue = self.steer.clone();
        self.steered.retain(|steered| !steered.pushed || queue.as_ref().is_some_and(|queue| queue.contains(steered.id)));
    }

    /// 一轮结束：没赶上的插队消息放回排队的最前面，按原来的顺序。
    fn finish_steering(&mut self) {
        let left: Vec<u64> =
            self.steer.take().map(|queue| queue.take_all()).unwrap_or_default().into_iter().map(|message| message.id).collect();
        let back: Vec<_> = std::mem::take(&mut self.steered)
            .into_iter()
            .filter(|steered| !steered.pushed || left.contains(&steered.id))
            .map(|steered| (steered.text, steered.attachments))
            .collect();
        if !back.is_empty() {
            let rest = std::mem::take(&mut self.queue);
            self.queue = back.into_iter().chain(rest).collect();
        }
    }

    /// 开始一轮对话；返回是否成功启动。
    fn start_turn(&mut self, text: String, attachments: Vec<PendingAttachment>, cx: &mut Context<Self>) -> bool {
        let Some(workspace) = self.workspace.clone() else {
            return false;
        };
        if self.loading {
            return false;
        }
        // 这里只检查附件在不在、大小有没有超（读元数据，很快）；读文件、图片转 base64（最多 5 MB）
        // 放到下面的后台任务里，发送那一下界面不卡。
        if let Err(error) = check_attachments(&attachments) {
            self.show_error(error, cx);
            return false;
        }
        let new_session = self.session.is_none();
        let session = match self.session.take() {
            Some(session) => session,
            None => match self.backend.create_session() {
                Ok(session) => session,
                Err(error) => {
                    self.show_error(error, cx);
                    return false;
                }
            },
        };
        let (approval_tx, mut approvals) = mpsc::unbounded_channel();
        let approver = Arc::new(UiApprover { tx: approval_tx });
        // 用过的目录排到侧边栏工作区的最前面。
        cx.emit(ConversationEvent::WorkspaceChosen(workspace.clone()));
        let steer = SteerQueue::new();
        let agent = match self.backend.build_agent(workspace, approver, &self.options) {
            Ok(agent) => agent.with_steering(steer.clone()),
            Err(error) => {
                self.session = Some(session);
                self.show_error(error, cx);
                return false;
            }
        };

        self.error = None;
        // 发送后没有焦点变化，读屏用户听不到任何反馈：告诉他已经发出、正在回复。
        // 稍等一下再播报：发送引起的界面变化（首页换成对话页、按钮换成停止）里如果有焦点事件，
        // 讲述人读焦点时会打断正在读的实时区域；等它们过去再读，这句话就不会被挤掉。
        announce_after(t!("a11y.sending").to_string(), ANNOUNCE_AFTER_SEND, cx);
        let options_snapshot = self.options.clone();
        if new_session {
            cx.emit(ConversationEvent::NewSessionStarted(options_snapshot.clone()));
        }
        self.session_id = Some(session.id().to_owned());
        let (event_tx, mut events) = mpsc::unbounded_channel();
        self.ui_event_sink = Some(event_tx.clone());
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.steer = Some(steer);
        self.steered.clear();

        // turn 任务跑完时发的信号。不能靠事件通道关闭来判断：界面自己也持有一个发送端（`ui_event_sink`），
        // 通道永远不会关。任务 panic 时发送端被丢掉，接收端同样会就绪。
        let (done_tx, done_rx) = futures::channel::oneshot::channel::<()>();
        let turn = self.backend.runtime().spawn(async move {
            let mut session = session;
            // 检查过之后文件又被删掉、改大了：这一轮不开始，原因显示在输入卡片上。
            let attachments = match tokio::task::spawn_blocking(move || read_attachments(&attachments)).await {
                Ok(Ok(attachments)) => attachments,
                Ok(Err(error)) => {
                    let _ = done_tx.send(());
                    return (session, Err((error, None)));
                }
                Err(error) => {
                    let _ = done_tx.send(());
                    return (session, Err((error.to_string(), None)));
                }
            };
            // 首轮把当时选的模式、权限、模型写进日志，之后打开会话时恢复（见 `session/options`）。
            if session.stored_options().is_none() {
                session.append_event(options_event_body(&options_snapshot), &event_tx).ok();
            }
            let attachments = user_attachments(attachments);
            let original = text.clone();
            // 插件拦下的消息：文字还给输入框，原因显示出来；其余错误只显示。
            let outcome = agent.run_turn_with(&mut session, text, attachments, &event_tx, &cancel).await.map_err(|error| match error {
                dshr_agent::AgentError::Blocked { .. } => (error.to_string(), Some(original)),
                other => (other.to_string(), None),
            });
            // 本轮的事件都已发出；后台标题另行通知，不占用这一轮的事件通道。
            let _ = done_tx.send(());
            (session, outcome)
        });

        cx.spawn(async move |this, cx| {
            while let Some(pending) = approvals.recv().await {
                let applied = this.update(cx, |this, cx| {
                    // 本会话里说过“不再询问”的工具：直接放行，不打扰用户。必须亲自确认的（授予插件能力等）除外。
                    if !pending.mandatory && this.always_allow.contains(&pending.tool) {
                        let _ = pending.respond.send(Decision::Allow);
                        return;
                    }
                    let tool = tool_description(&pending.tool).unwrap_or_else(|| pending.tool.clone());
                    announce(t!("a11y.approval_needed", tool = tool).to_string(), cx);
                    let capabilities = approval_capabilities(&this.backend, &pending.tool, &pending.input, pending.mandatory);
                    this.transcript.attach_approval_with(&pending.call_id, pending.respond, pending.note, pending.mandatory, capabilities);
                    this.sync_rows(cx);
                    // 审批需要用户处理：即使正往回翻看，也回到最新位置。
                    this.scroll_to_latest(cx);
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
            }
        })
        .detach();

        // JS 插件给这一轮的事件、工具卡片画的东西：要问插件的那几批事件先到后台线程上问好，再交给界面。
        let mut resolver = PluginViewResolver::new(self.backend.clone());
        cx.spawn(async move |this, cx| {
            // 一次取走所有已到达的事件，合并成一次界面更新；逐条处理的话，事件来得比界面
            // 处理得快时（大量命令输出）这个循环会一直占着主线程，窗口失去响应。
            // 回合结束后等到 turn/end 再退出；异常路径收不到 turn/end 时，设一次兜底超时，不因新事件重置。
            // turn/end 到达时任务可能还在执行结束钩子，而界面持有发送端，通道不会关闭，
            // 所以还要监听任务结束信号 `done_rx`。
            let mut batch = Vec::with_capacity(EVENT_BATCH);
            let mut turn_ended = false;
            let mut done_rx = done_rx.fuse();
            let mut done = false;
            // turn 任务结束、且还没等到 turn/end 时，兜底退出的截止时间（Instant，只设一次）。
            let mut tail_deadline: Option<std::time::Instant> = None;
            let turn = loop {
                // 这个任务跑在界面执行器上（不在 tokio 运行时里），超时要用 gpui 的 timer。
                let finished = done || turn.is_finished();
                let received = if finished && !turn_ended {
                    let deadline = tail_deadline.get_or_insert_with(|| std::time::Instant::now() + TURN_TAIL_WAIT);
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        0
                    } else {
                        futures::select_biased! {
                            received = events.recv_many(&mut batch, EVENT_BATCH).fuse() => received,
                            _ = cx.background_executor().timer(remaining).fuse() => 0,
                        }
                    }
                } else if finished {
                    // turn/end 已收到：把通道里已经到达的收尾事件捞完就退，不空等。
                    let mut received = 0;
                    while received < EVENT_BATCH {
                        match events.try_recv() {
                            Ok(event) => {
                                batch.push(event);
                                received += 1;
                            }
                            Err(_) => break,
                        }
                    }
                    received
                } else {
                    // 等下一批事件，或者任务结束（`recv_many` 可以安全取消，不丢事件）。
                    futures::select_biased! {
                        received = events.recv_many(&mut batch, EVENT_BATCH).fuse() => received,
                        _ = done_rx => {
                            done = true;
                            continue;
                        }
                    }
                };
                if received == 0 {
                    break turn;
                }
                // 插件事件、插件画卡片的工具结果：先在后台线程上问插件（等插件回答时界面照常响应），
                // 其余的（绝大多数批次）不碰插件，直接在这里过一遍。
                let durable: Vec<Arc<dshr_core::SessionEvent>> = batch
                    .iter()
                    .filter_map(|event| match event {
                        dshr_agent::LiveEvent::Durable(event) => Some(event.clone()),
                        dshr_agent::LiveEvent::Chunk(_) => None,
                    })
                    .collect();
                resolver.record_calls(durable.iter().map(|event| &**event));
                let views = if resolver.needs_plugins(durable.iter().map(|event| &**event)) {
                    let (back, views) = cx
                        .background_executor()
                        .spawn(async move {
                            let views = resolver.resolve(durable.iter().map(|event| &**event));
                            (resolver, views)
                        })
                        .await;
                    resolver = back;
                    views
                } else {
                    resolver.resolve(durable.iter().map(|event| &**event))
                };
                let mut applied = true;
                let mut saw_end = false;
                this.update(cx, |this, cx| {
                    this.transcript.add_plugin_views(views);
                    for event in batch.drain(..) {
                        if matches!(&event, dshr_agent::LiveEvent::Durable(event) if matches!(event.body, dshr_core::EventBody::TurnEnd { .. })) {
                            saw_end = true;
                        }
                        this.transcript.apply(&event);
                    }
                    this.prune_steered();
                    this.sync_rows(cx);
                    cx.notify();
                })
                .unwrap_or_else(|_| applied = false);
                if !applied {
                    return;
                }
                turn_ended |= saw_end;
                if turn_ended && (done || turn.is_finished()) {
                    // 收尾事件已齐：直接结束，界面立刻回到“发送”样式。
                    break turn;
                }
                if received == EVENT_BATCH {
                    // 还有积压：先让主线程画一帧、处理输入（比如点“停止”），再继续。
                    cx.background_executor().timer(BACKLOG_PAUSE).await;
                }
            };
            // 事件循环结束后统一收尾：流式行定稿、清空“运行中”，再告诉界面重画。
            let result = turn.await;
            this.update(cx, |this, cx| {
                this.cancel = None;
                this.finish_steering();
                let mut continue_queue = false;
                match result {
                    Ok((mut session, outcome)) => {
                        // 标题通知可能在会话还由后台持有时到达。
                        session.refresh_title();
                        this.session = Some(session);
                        match outcome {
                            Ok(outcome) => {
                                continue_queue =
                                    matches!(outcome.reason, TurnEndReason::Completed | TurnEndReason::MaxSteps);
                                let done = match outcome.reason {
                                    TurnEndReason::Interrupted => t!("a11y.reply_stopped"),
                                    _ => t!("a11y.reply_done"),
                                };
                                announce(done.to_string(), cx);
                            }
                            Err((error, restore)) => {
                                this.error = Some(error);
                                if restore.is_some() {
                                    this.restore_input = restore;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        // 这一轮的后台任务崩溃了（panic）：会话对象随它没了，但日志里的事件都还在。
                        // 从日志重新打开，用户可以接着在这个会话里说话，而不是下一条消息新建一个会话。
                        let message = t!("conversation.task_crashed", error = error).to_string();
                        this.error = Some(message.clone());
                        if let Some(id) = this.session_id.clone() {
                            this.error_after_open = Some((id.clone(), message));
                            this.open_session(&id, cx);
                        }
                    }
                }
                if let Some(error) = &this.error {
                    announce(error.clone(), cx);
                }
                cx.emit(ConversationEvent::SessionsChanged);
                // 被中断或出错时保留队列，等用户决定。
                if continue_queue && !this.queue.is_empty() {
                    let (text, attachments) = this.queue.remove(0);
                    if !this.start_turn(text.clone(), attachments.clone(), cx) {
                        this.queue.insert(0, (text, attachments));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();

        // 自己刚发的消息总要看得到。
        self.scroll_to_latest(cx);
        cx.emit(ConversationEvent::SessionsChanged);
        cx.notify();
        true
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
            // 还没被取走的插队消息不会再被接进这一轮：现在就放回排队，免得停止后又被当成插队。
            self.finish_steering();
            // 立即退出“运行中”：停止按钮马上变回发送按钮、Esc/队列等不再被当成运行中；
            // 后台那一轮随后照常收尾（写 turn/end、恢复会话），不再动 self.cancel。
            cx.notify();
        }
    }

    fn show_error(&mut self, error: String, cx: &mut Context<Self>) {
        announce(error.clone(), cx);
        self.error = Some(error);
        cx.notify();
    }

    /// 画首页（标题和居中的输入卡片）：还没有任何消息，而且不是正在载入已有会话的日志。
    /// 载入期间画对话页的布局（空的消息列表 + 底部的输入卡片），首页不会先闪一下再被换掉。
    fn shows_home(&self) -> bool {
        self.transcript.rows.is_empty() && !self.loading
    }

    /// 首页标题：品牌标记 + 标语 + “预览版”徽标。
    fn render_hero(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let p = palette(cx);
        h_flex()
            .justify_center()
            .gap(px(space::M))
            .child(brand_mark(size::ICON_HERO, &p))
            .child(heading(t!("conversation.hero_title").to_string(), 1).text_display())
            .child(badge(t!("conversation.preview").to_string(), &p))
    }

    /// 输入卡片第一行左边的工作区选择（和附件标签同一行）。
    fn render_context_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let label = match &self.workspace {
            Some(path) => path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned()),
            None => t!("conversation.choose_workspace").to_string(),
        };
        let name = t!("a11y.choice", group = t!("a11y.workspace_chip"), value = label).to_string();
        h_flex().child(crate::design::framed(
            "workspace-chip",
            Button::new("workspace-chip")
                .ghost()
                .control_xs()
                .icon(IconName::Folder)
                .label(label)
                .accessibility_label(name)
                .child(Icon::new(IconName::ChevronDown).size(px(size::ICON_SM)))
                .when_some(self.workspace.as_ref(), |button, path| button.tooltip(path.display().to_string()))
                .disabled(self.is_running())
                .on_click(cx.listener(|this, _, _, cx| this.choose_workspace(cx))),
        ))
    }

    /// 权限模式菜单（输入卡片左下角，对应原版的“工作区内修改”）。
    fn render_permission_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let current = self.options.permission;
        let view = cx.entity().downgrade();
        // 菜单打开的那一刻焦点留在这个按钮上，菜单画出来再移进去（见 `menu_popover`）。
        let trigger_focus = crate::design::menu_trigger_focus("permission-menu", window, cx);
        let trigger = crate::design::framed_trigger_with(
            trigger_focus.clone(),
            Button::new("permission-trigger")
                .track_focus(&trigger_focus)
                .ghost()
                .control_sm()
                .icon(permission_icon(current))
                .label(permission_label(current))
                // 只读“工作区内修改”听不出这是什么：带上“修改策略”。
                .accessibility_label(t!("a11y.choice", group = t!("a11y.permission_menu"), value = permission_label(current)).to_string())
                .child(Icon::new(IconName::ChevronDown).size(px(size::ICON_SM))),
        );
        let modes = [PermissionMode::Manual, PermissionMode::Workspace, PermissionMode::AutoReview, PermissionMode::FullAccess];
        let initial = modes.iter().position(|mode| *mode == current).unwrap_or(0);
        // 菜单向上展开（锚在输入卡片底部）。
        let body = move |menu: &MenuFocus, _: &mut Window, cx: &mut App| {
            let p = palette(cx);
            v_flex()
                .w(px(size::MENU_WIDTH))
                .p(px(space::XS))
                .gap(px(2.))
                .children(modes.into_iter().enumerate().map(|(index, mode)| {
                    let view = view.clone();
                    // 用可聚焦的按钮：Tab 能走到、Enter / 空格能选，读屏软件读出名称、说明和是否勾选。
                    let item = menu_choice(
                        pressable(("permission-mode", mode as usize), &p).focus_on_hover().track_focus(&menu.item(index, cx)),
                        mode == current,
                    )
                    .accessibility_label(permission_label(mode))
                    .aria_description(permission_description(mode))
                    .relative()
                    .px(px(space::S))
                    .py(px(space::S))
                    .flex()
                    .gap(px(space::S))
                    .items_start()
                    .rounded(px(radius::SM))
                    .cursor_pointer()
                    .child(hover_fill(&p, radius::SM, false))
                    .child(Icon::new(permission_icon(mode)).size(px(size::ICON_MD)).mt(px(2.)))
                    .child(
                        // min_w(0) 让说明在菜单宽度内换行，而不是把菜单撑开。
                        v_flex().flex_1().min_w(px(0.)).child(div().text_label().child(permission_label(mode))).child(hint(
                            permission_description(mode),
                            Tone::Neutral,
                            &p,
                        )),
                    )
                    .when(mode == current, |item| item.child(Icon::new(IconName::Check).size(px(size::ICON_SM)).text_color(p.accent)))
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            this.options.permission = mode;
                            this.permission_menu_open = false;
                            this.persist_options(cx);
                            cx.notify();
                        })
                        .ok();
                    });
                    item.into_any_element()
                }))
                .into_any_element()
        };
        menu_popover(
            "permission-menu",
            self.permission_menu_open,
            Anchor::BottomLeft,
            trigger,
            cx.listener(|this, open: &bool, _, cx| {
                this.permission_menu_open = *open;
                cx.notify();
            }),
            initial,
            body,
            window,
            cx,
        )
    }

    /// Agent 预设菜单（对应原版的“标准模式”菜单）。
    fn render_preset_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let current = self.options.preset;
        let plugin_current = self.options.plugin_preset.clone();
        let plugin_presets = self.backend.plugin_presets();
        let trigger_label = plugin_current.as_ref().map_or_else(|| preset_label(current), |preset| preset.label.clone());
        let view = cx.entity().downgrade();
        // 菜单打开的那一刻焦点留在这个按钮上，菜单画出来再移进去（见 `menu_popover`）。
        let trigger_focus = crate::design::menu_trigger_focus("preset-menu", window, cx);
        let trigger = crate::design::framed_trigger_with(
            trigger_focus.clone(),
            Button::new("preset-trigger")
                .track_focus(&trigger_focus)
                .ghost()
                .control_sm()
                .icon(preset_icon(current))
                .label(trigger_label.clone())
                .accessibility_label(t!("a11y.choice", group = t!("a11y.mode_menu"), value = trigger_label).to_string())
                .child(Icon::new(IconName::ChevronDown).size(px(size::ICON_SM))),
        );
        let initial = match &plugin_current {
            Some(current) => plugin_presets
                .iter()
                .position(|preset| preset.plugin == current.plugin && preset.id == current.id)
                .map_or(0, |index| Preset::ALL.len() + index),
            None => Preset::ALL.iter().position(|preset| *preset == current).unwrap_or(0),
        };
        let body = move |menu: &MenuFocus, _: &mut Window, cx: &mut App| {
            let p = palette(cx);
            let builtin = Preset::ALL.len();
            v_flex()
                .w(px(size::MENU_WIDTH))
                .p(px(space::XS))
                .gap(px(2.))
                .children(Preset::ALL.into_iter().enumerate().map(|(index, preset)| {
                    let view = view.clone();
                    let chosen = preset == current && plugin_current.is_none();
                    let item = menu_choice(pressable(("preset", index), &p).focus_on_hover().track_focus(&menu.item(index, cx)), chosen)
                        .accessibility_label(preset_label(preset))
                        .aria_description(preset_description(preset))
                        .selected(chosen)
                        .relative()
                        .px(px(space::S))
                        .py(px(space::S))
                        .flex()
                        .gap(px(space::S))
                        .items_start()
                        .rounded(px(radius::SM))
                        .cursor_pointer()
                        .child(hover_fill(&p, radius::SM, false))
                        .child(Icon::new(preset_icon(preset)).size(px(size::ICON_MD)).mt(px(2.)))
                        .child(v_flex().flex_1().min_w(px(0.)).child(div().text_label().child(preset_label(preset))).child(hint(
                            preset_description(preset),
                            Tone::Neutral,
                            &p,
                        )))
                        .when(chosen, |item| item.child(Icon::new(IconName::Check).size(px(size::ICON_SM)).text_color(p.accent)))
                        .on_click(move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                this.options.preset = preset;
                                this.options.plugin_preset = None;
                                this.preset_menu_open = false;
                                this.persist_options(cx);
                                cx.notify();
                            })
                            .ok();
                        });
                    item.into_any_element()
                }))
                // 插件提供的预设排在内置的后面。
                .children(plugin_presets.iter().enumerate().map(|(index, preset)| {
                    let view = view.clone();
                    let chosen = plugin_current.as_ref().is_some_and(|current| current.plugin == preset.plugin && current.id == preset.id);
                    let picked = preset.clone();
                    let item = menu_choice(
                        pressable(("plugin-preset", index), &p).focus_on_hover().track_focus(&menu.item(builtin + index, cx)),
                        chosen,
                    )
                    .accessibility_label(preset.label.clone())
                    .aria_description(preset.description.clone())
                    .selected(chosen)
                    .relative()
                    .px(px(space::S))
                    .py(px(space::S))
                    .flex()
                    .gap(px(space::S))
                    .items_start()
                    .rounded(px(radius::SM))
                    .cursor_pointer()
                    .child(hover_fill(&p, radius::SM, false))
                    .child(Icon::new(IconName::Bot).size(px(size::ICON_MD)).mt(px(2.)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().text_label().child(preset.label.clone()))
                            .children((!preset.description.is_empty()).then(|| hint(preset.description.clone(), Tone::Neutral, &p))),
                    )
                    .when(chosen, |item| item.child(Icon::new(IconName::Check).size(px(size::ICON_SM)).text_color(p.accent)))
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            this.options.preset = Preset::Standard;
                            this.options.plugin_preset = Some(picked.clone());
                            this.preset_menu_open = false;
                            this.persist_options(cx);
                            cx.notify();
                        })
                        .ok();
                    });
                    item.into_any_element()
                }))
                .into_any_element()
        };
        menu_popover(
            "preset-menu",
            self.preset_menu_open,
            Anchor::BottomLeft,
            trigger,
            cx.listener(|this, open: &bool, _, cx| {
                this.preset_menu_open = *open;
                cx.notify();
            }),
            initial,
            body,
            window,
            cx,
        )
    }

    /// 输入卡片左下角的“+”：选择要随消息发送的文件。
    fn pick_attachments(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(t!("conversation.attach").to_string().into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                this.update(cx, |this, cx| {
                    this.add_paths(paths, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// 拖进窗口、打开对话框选出来的路径：去重后加进附件列表。
    fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        for path in paths {
            if !self.attachments.iter().any(|attachment| matches!(attachment, PendingAttachment::Path(existing) if *existing == path)) {
                self.attachments.push(PendingAttachment::Path(path));
            }
        }
        cx.notify();
    }

    /// Ctrl+V：剪贴板里是截图或文件时加为附件（返回 true 表示已处理，输入框不再当文本粘贴）。
    fn paste_clipboard(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else { return false };
        let mut handled = false;
        for entry in item.entries {
            match entry {
                ClipboardEntry::Image(image) => {
                    let media_type = image.format.mime_type().to_owned();
                    let base64 = {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD.encode(&image.bytes)
                    };
                    // 剪贴板截图没有文件名，给个能认出来的名字。
                    let name = t!("conversation.pasted_image", format = image.format.extension()).to_string();
                    self.attachments.push(PendingAttachment::Screenshot { name, media_type, data: base64 });
                    handled = true;
                }
                // 从文件管理器复制的文件：当成选了这些文件。
                ClipboardEntry::ExternalPaths(paths) => {
                    self.add_paths(paths.paths().to_vec(), cx);
                    handled = true;
                }
                ClipboardEntry::String(_) => {}
            }
        }
        if handled {
            cx.notify();
        }
        handled
    }

    /// 已选附件的标签，每个带移除按钮。
    fn render_attachment_chips(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = palette(cx);
        self.attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| {
                let name = attachment.label();
                let is_image = match attachment {
                    PendingAttachment::Path(path) => dshr_core::image_media_type(path).is_some(),
                    PendingAttachment::Screenshot { .. } => true,
                    PendingAttachment::Mention { .. } => false,
                };
                h_flex()
                    .gap(px(space::XS))
                    .pl(px(space::S))
                    .rounded(px(radius::MD))
                    .bg(p.sunken)
                    .text_caption()
                    .child(Icon::new(if is_image { IMAGE_ICON } else { IconName::File.into() }).size(px(size::ICON_SM)))
                    .child(readable_with_id(("attachment", index), name.clone()))
                    .child(crate::design::framed(
                        ("remove-attachment", index),
                        Button::new(("remove-attachment", index))
                            .ghost()
                            .control_xs()
                            .icon(IconName::Close)
                            .accessibility_label(t!("conversation.remove_attachment", name = name).to_string())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if index < this.attachments.len() {
                                    this.attachments.remove(index);
                                    cx.notify();
                                }
                            })),
                    ))
                    .into_any_element()
            })
            .collect()
    }

    /// 模型与思考菜单（对应原版的“模型和推理”菜单）。
    ///
    /// 一级是按提供商分组的模型列表（太长时在限定高度内滚动），最下面一项“思考等级”打开二级页：
    /// 点它、或焦点在它上面时按 →，换成思考等级的选项；二级页里点“返回”、按 ← 或 Esc 回到模型列表。
    fn render_model_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let choices = self.model_choices.clone();
        let current = self.options.model.clone();
        let label = self.model_label.clone();
        let effort = self.options.effort;
        let page = self.model_menu_page;
        let scroll = self.model_scroll.clone();
        let view = cx.entity().downgrade();
        // 模型列表最高这么多，再多就滚动；窗口矮时不超过窗口高度的一小半（菜单向上展开，上面还要留出空间）。
        let list_height = MODEL_LIST_MAX_HEIGHT.min(window.viewport_size().height.as_f32() * 0.45);
        let selected_model = choices.iter().position(|choice| choice.reference == current);
        let initial = match page {
            ModelMenuPage::Models => selected_model.unwrap_or(0),
            ModelMenuPage::Effort => effort_index(effort) + 1,
        };

        // 菜单打开的那一刻焦点留在这个按钮上，菜单画出来再移进去（见 `menu_popover`）。
        let trigger_focus = crate::design::menu_trigger_focus("model-menu", window, cx);
        let trigger = crate::design::framed_trigger_with(
            trigger_focus.clone(),
            Button::new("model-trigger")
                .track_focus(&trigger_focus)
                .ghost()
                .control_sm()
                .accessibility_label(t!("a11y.choice", group = t!("a11y.model_menu"), value = label).to_string())
                .label(label)
                .child(Icon::new(IconName::ChevronDown).size(px(size::ICON_SM))),
        );
        let body = move |menu: &MenuFocus, _: &mut Window, cx: &mut App| {
            let p = palette(cx);
            let effort_group = t!("conversation.effort").to_string();
            // 换页：改视图状态，焦点移到新页上对应的那一项（下一帧画出来时生效）。
            let show_page = {
                let (view, menu) = (view.clone(), menu.clone());
                move |page: ModelMenuPage, focus: usize, window: &mut Window, cx: &mut App| {
                    view.update(cx, |this, cx| {
                        this.model_menu_page = page;
                        cx.notify();
                    })
                    .ok();
                    menu.focus(focus, window, cx);
                }
            };
            let menu_body = v_flex().w(px(size::MENU_WIDTH)).p(px(space::XS)).gap(px(2.));
            match page {
                ModelMenuPage::Models => {
                    // 模型列表：可滚动容器里每个模型占一个子元素（第 i 个模型就是第 i 个子元素，滚动按它定位）。
                    // 每组的提供商标题和这组的第一个模型合成一个子元素：滚到这个模型时标题一起露出来。
                    let mut rows: Vec<AnyElement> = Vec::new();
                    let mut last_provider: Option<&str> = None;
                    for (index, choice) in choices.iter().enumerate() {
                        let header = (last_provider != Some(choice.provider_name.as_str())).then(|| {
                            div()
                                .px(px(space::S + 2.))
                                .pt(px(space::S))
                                .pb(px(space::XS))
                                .text_caption()
                                .text_color(p.text_secondary)
                                .child(readable_with_id(("model-provider", index), choice.provider_name.clone()))
                        });
                        last_provider = Some(&choice.provider_name);
                        menu.scrolls_in(index, &scroll, index, cx);
                        let selected = choice.reference == current;
                        let reference = choice.reference.clone();
                        let label: SharedString = choice.label.clone().into();
                        let view = view.clone();
                        let item =
                            // 当前模型只用对勾表示，勾选状态另外告诉读屏软件。
                            menu_choice(menu_item(("model-choice", index), None, choice.label.clone(), false, &p), selected)
                                .focus_on_hover().track_focus(&menu.item(index, cx))
                                // 读出提供商：同一个模型可能在两个提供商下都有（如截图里的 DeepSeek-V4.1-Flash）。
                                .accessibility_label(format!("{} · {}", choice.provider_name, choice.label))
                                .when(selected, |item| item.child(Icon::new(IconName::Check).size(px(size::ICON_SM)).text_color(p.accent)))
                                .on_click(move |_, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.options.model = reference.clone();
                                        this.model_label = label.clone();
                                        this.model_menu_open = false;
                                        this.persist_options(cx);
                                        cx.notify();
                                    })
                                    .ok();
                                });
                        let item = item.into_any_element();
                        // 不许收缩：列表限了高度，弹性布局会把每一行压到文字的高度，而不是滚动。
                        rows.push(match header {
                            Some(header) => v_flex().flex_none().gap(px(2.)).child(header).child(item).into_any_element(),
                            None => div().flex_none().child(item).into_any_element(),
                        });
                    }
                    if choices.is_empty() {
                        rows.push(
                            div().p(px(space::S)).child(description(t!("conversation.no_models").to_string(), &p)).into_any_element(),
                        );
                    }
                    let list = div()
                        .id("model-list")
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .max_h(px(list_height))
                        .overflow_y_scroll()
                        .track_scroll(&scroll)
                        .children(rows);
                    // 最下面一项打开二级页“思考等级”，右边写着现在的选择。
                    let entry_index = choices.len();
                    let to_effort = {
                        let show_page = show_page.clone();
                        move |window: &mut Window, cx: &mut App| show_page(ModelMenuPage::Effort, effort_index(effort) + 1, window, cx)
                    };
                    menu.on_forward(entry_index, to_effort.clone(), cx);
                    let entry = menu_item("model-effort-entry", None, effort_group.clone(), false, &p)
                        .focus_on_hover()
                        .track_focus(&menu.item(entry_index, cx))
                        .accessibility_label(t!("a11y.choice", group = effort_group, value = effort_label(effort)).to_string())
                        .aria_expanded(false)
                        .child(div().flex_none().text_caption().text_color(p.text_secondary).child(effort_label(effort)))
                        .child(Icon::new(IconName::ChevronRight).size(px(size::ICON_SM)).text_color(p.text_secondary))
                        .on_click(move |_, window, cx| to_effort(window, cx));
                    menu_body
                        .child(list)
                        .child(div().my(px(space::XS)).child(divider(&p)))
                        .child(entry.into_any_element())
                        .into_any_element()
                }
                ModelMenuPage::Effort => {
                    // 回到模型列表时焦点回到“思考等级”这一项。
                    let entry_index = choices.len();
                    let back = {
                        let show_page = show_page.clone();
                        move |window: &mut Window, cx: &mut App| show_page(ModelMenuPage::Models, entry_index, window, cx)
                    };
                    menu.on_back(back.clone(), cx);
                    let back_item = menu_item("model-effort-back", Some(IconName::ChevronLeft), effort_group.clone(), false, &p)
                        .focus_on_hover()
                        .track_focus(&menu.item(0, cx))
                        .accessibility_label(t!("conversation.effort_back").to_string())
                        .on_click({
                            let back = back.clone();
                            move |_, window, cx| back(window, cx)
                        });
                    // 竖排的单选菜单项：译文比中文长时（德语、希腊语）自动换行，不会撑出菜单。
                    let levels = EFFORT_CHOICES.into_iter().enumerate().map(|(index, level)| {
                        let (view, back) = (view.clone(), back.clone());
                        let checked = level == effort;
                        let name = t!("a11y.choice", group = effort_group, value = effort_label(level)).to_string();
                        let item = menu_choice(menu_item(("effort", index), None, effort_label(level), false, &p), checked)
                            .focus_on_hover()
                            .track_focus(&menu.item(index + 1, cx))
                            .accessibility_label(name)
                            // accesskit 的位置从 0 数，读屏时各平台的适配层再加 1。
                            .aria_position_in_set(index)
                            .aria_size_of_set(EFFORT_CHOICES.len())
                            .when(checked, |item| item.child(Icon::new(IconName::Check).size(px(size::ICON_SM)).text_color(p.accent)))
                            // 选好后回到模型列表，菜单不关：还可以接着换模型。
                            .on_click(move |_, window, cx| {
                                view.update(cx, |this, cx| {
                                    this.options.effort = level;
                                    cx.notify();
                                })
                                .ok();
                                back(window, cx);
                            });
                        item.into_any_element()
                    });
                    menu_body
                        .child(back_item.into_any_element())
                        .child(div().my(px(space::XS)).child(divider(&p)))
                        .children(levels)
                        .child(div().px(px(space::S)).pt(px(space::XS)).child(hint(
                            t!("conversation.effort_hint").to_string(),
                            Tone::Neutral,
                            &p,
                        )))
                        .into_any_element()
                }
            }
        };
        let open_scroll = self.model_scroll.clone();
        let open_choices = self.model_choices.clone();
        menu_popover(
            "model-menu",
            self.model_menu_open,
            Anchor::BottomRight,
            trigger,
            cx.listener(move |this, open: &bool, _, cx| {
                this.model_menu_open = *open;
                if *open {
                    // 打开时把当前模型滚进视口（焦点会落在它上面）。
                    if let Some(index) = open_choices.iter().position(|choice| choice.reference == this.options.model) {
                        open_scroll.scroll_to_item(index);
                    }
                } else {
                    // 下次打开从模型列表开始。
                    this.model_menu_page = ModelMenuPage::Models;
                }
                cx.notify();
            }),
            initial,
            body,
            window,
            cx,
        )
    }
    /// 圆形发送按钮；运行中且输入为空时变成停止按钮，有输入时把消息加入队列。
    ///
    /// 可用与不可用、发送与停止之间都有过渡：不透明度渐变，停止的底色交叉淡入，换图标时新图标轻轻浮上来。
    fn render_send_button(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let has_text = !self.input_text(cx).is_empty();
        let running = self.is_running();
        let stop = running && !has_text;
        let enabled = stop || (has_text && self.workspace.is_some());
        let p = palette(cx);
        let opacity = motion::tween("send-opacity", if enabled { 1. } else { 0.35 }, window, cx);
        let icon_enter =
            |icon: Icon, id: &'static str| motion::Entrance::new(motion::FAST).from(Edge::Bottom, motion::NUDGE).play(icon, id);
        let base = pressable("send", &p)
            .size(px(size::CONTROL_SM))
            .flex_none()
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .relative()
            .bg(p.accent)
            .opacity(opacity)
            // 用鼠标点发送：焦点留在输入框里，不跳到按钮上。跳过去再跳回来，讲述人会把输入框重读一遍，
            // 盖过“正在回复”；用键盘 Tab 到这个按钮上照常能按。
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .child(
                motion::highlight("send-stop-fill", p.text, size::CONTROL_SM / 2., stop).without_hover().animate(motion::FAST).outset(2.),
            )
            // 悬停提亮、按住压暗（可用时才有）。
            .when(enabled, |button| {
                button.child(
                    motion::highlight("send-state", transparent_black(), size::CONTROL_SM / 2., false)
                        .hover_color(white().opacity(0.14))
                        .pressed_color(black().opacity(0.14))
                        .outset(2.),
                )
            });
        if stop {
            return base
                .accessibility_label(t!("conversation.stop").to_string())
                .cursor_pointer()
                .child(icon_enter(Icon::new(IconName::Square).size(px(size::ICON_SM)).text_color(p.surface), "send-icon-stop"))
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                .into_any_element();
        }
        let label = if running { t!("conversation.add_to_queue") } else { t!("conversation.send") };
        base.accessibility_label(label.to_string())
            .disabled(!enabled)
            .child(icon_enter(Icon::new(IconName::ArrowUp).size(px(size::ICON_MD)).text_color(p.on_accent), "send-icon-send"))
            .when(enabled, |button| button.cursor_pointer().on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))))
            .into_any_element()
    }

    /// 输入卡片上方的面板（如计划面板），由界面插件提供；展开状态记在本视图里。
    fn render_panels(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let ui = self.ui.clone();
        ui.composer_panels()
            .iter()
            .filter_map(|panel| {
                let id = panel.id();
                let toggle = cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if !this.collapsed_panels.remove(id) {
                        this.collapsed_panels.insert(id);
                    }
                    cx.notify();
                });
                let context =
                    PanelContext { expanded: !self.collapsed_panels.contains(id), running: self.is_running(), toggle: Rc::new(toggle) };
                panel.render(self.transcript.state(), &context, cx)
            })
            .collect::<Vec<_>>()
            .into_iter()
            .chain(self.plugin_panels.iter().map(|panel| self.render_plugin_panel(panel, window, cx)).collect::<Vec<_>>())
            .collect()
    }

    /// 插件组成变了时，重建 JS 插件的面板（每个面板一个插件页面视图）。
    fn sync_plugin_panels(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let generation = self.backend.plugins_generation();
        if generation == self.plugin_panels_generation {
            return;
        }
        self.plugin_panels_generation = generation;
        let mut panels = Vec::new();
        for surface in self.backend.surfaces() {
            let plugin = surface.plugin().to_owned();
            for entry in surface.entries().iter().filter(|entry| entry.place == dshr_agent::EntryPlace::Panel) {
                let key = format!("{plugin}/{}", entry.id);
                // 已有的面板（同一个插件、同一页）接着用，输入框里打的字不丢。
                let view = match self.plugin_panels.iter().position(|panel| panel.key == key && panel.page == entry.page) {
                    Some(index) => self.plugin_panels.swap_remove(index).view,
                    None => {
                        let (backend, page, workspace) = (self.backend.clone(), entry.page.clone(), self.workspace.clone());
                        let plugin = plugin.clone();
                        cx.new(|cx| crate::plugin_page::PluginPageView::new(backend, plugin, page, workspace, window, cx))
                    }
                };
                panels.push(PluginPanel { key, page: entry.page.clone(), label: entry.label.clone(), view });
            }
        }
        self.plugin_panels = panels;
    }

    fn render_plugin_panel(&self, panel: &PluginPanel, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let expanded = !self.collapsed_plugin_panels.contains(&panel.key);
        let key = panel.key.clone();
        let header = pressable(SharedString::from(format!("plugin-panel-{}", panel.key)), &p)
            .accessibility_label(panel.label.clone())
            .aria_expanded(expanded)
            .flex()
            .items_center()
            .gap(px(space::S))
            .rounded(px(radius::SM))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed_plugin_panels.remove(&key) {
                    this.collapsed_plugin_panels.insert(key.clone());
                }
                cx.notify();
            }))
            .child(div().text_label().child(panel.label.clone()))
            .child(div().flex_1())
            .child(motion::turn(
                Icon::new(IconName::ChevronUp).size(px(size::ICON_SM)),
                SharedString::from(format!("plugin-panel-turn-{}", panel.key)),
                expanded,
                motion::HALF_TURN,
                window,
                cx,
            ));
        let body_id = SharedString::from(format!("plugin-panel-body-{}", panel.key));
        let view = panel.view.clone();
        let body = expanded
            .then(|| div().pt(px(space::S)).child(div().max_h(px(320.)).id(body_id).overflow_y_scroll().child(view)).into_any_element());
        card(&p).px(px(space::L)).py(px(space::M)).child(v_flex().child(header).children(body)).into_any_element()
    }

    fn render_queue(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let running = self.is_running();
        let steer_key = if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" };
        // 插队的消息排在最前面：当前步骤一结束就交给模型。
        let steered: Vec<_> = self
            .steered
            .iter()
            .map(|steered| {
                let id = steered.id;
                let row = h_flex()
                    .gap(px(space::S))
                    .px(px(space::M))
                    .py(px(space::XS))
                    .rounded(px(radius::MD))
                    .bg(p.accent_soft)
                    .child(
                        div()
                            .text_caption()
                            .text_color(p.accent)
                            .child(readable_with_id(("steering", id as usize), t!("conversation.steering").to_string())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_body()
                            .child(readable_with_id(("steering-text", id as usize), steered.text.clone())),
                    )
                    .child(crate::design::framed(
                        ("unsteer", id as usize),
                        Button::new(("unsteer", id as usize))
                            .ghost()
                            .control_xs()
                            .icon(IconName::Close)
                            .tooltip(t!("conversation.unsteer").to_string())
                            .accessibility_label(t!("conversation.unsteer").to_string())
                            .on_click(cx.listener(move |this, _, _, cx| this.unsteer(id, cx))),
                    ));
                row.into_any_element()
            })
            .collect();
        let rows: Vec<_> = self
            .queue
            .iter()
            .enumerate()
            .map(|(index, (text, _))| {
                let row = h_flex()
                    .gap(px(space::S))
                    .px(px(space::M))
                    .py(px(space::XS))
                    .rounded(px(radius::MD))
                    .bg(p.sunken)
                    .child(
                        div()
                            .text_caption()
                            .text_color(p.text_secondary)
                            .child(readable_with_id(("queued", index), t!("conversation.queued").to_string())),
                    )
                    .child(
                        div().flex_1().min_w(px(0.)).truncate().text_body().child(readable_with_id(("queued-text", index), text.clone())),
                    )
                    // 运行中可以把排队的消息改成插队：不等这一轮结束。
                    .when(running, |row| {
                        row.child(crate::design::framed(
                            ("steer", index),
                            Button::new(("steer", index))
                                .ghost()
                                .control_xs()
                                .icon(gpui_kit::assets::IconName::CornerDownLeft)
                                .label(t!("conversation.steer").to_string())
                                .tooltip(t!("conversation.steer_tooltip", key = steer_key).to_string())
                                .accessibility_label(t!("conversation.steer").to_string())
                                .on_click(cx.listener(move |this, _, _, cx| this.steer_queued(index, cx))),
                        ))
                    })
                    .child(crate::design::framed(
                        ("unqueue", index),
                        Button::new(("unqueue", index))
                            .ghost()
                            .control_xs()
                            .icon(IconName::Close)
                            .accessibility_label(t!("conversation.unqueue").to_string())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if index < this.queue.len() {
                                    this.queue.remove(index);
                                    cx.notify();
                                }
                            })),
                    ));
                row.into_any_element()
            })
            .collect();
        v_flex().gap(px(space::XS)).children(steered).children(rows).into_any_element()
    }

    /// 输入区：工作区选择、错误、排队消息与输入卡片。
    fn render_composer(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let context_bar = self.render_context_bar(cx);
        let queue = self.render_queue(cx);
        let panels = self.render_panels(window, cx);
        let slash_menu = self.render_slash_menu(cx).or_else(|| self.render_mention_menu(cx));
        // 这两个菜单属于各自的插件：插件停用时不显示（本轮按标准模式、默认权限进行）。
        // 插件可以把这两个菜单藏起来（`composer:permission`、`composer:presets`）。
        let hidden = self.backend.hidden_entries();
        let permission_menu = if self.ui.has_control(ComposerControl::PermissionMenu) && !hidden.contains("composer:permission") {
            Some(self.render_permission_menu(window, cx))
        } else {
            None
        };
        let preset_menu = if self.ui.has_control(ComposerControl::PresetMenu) && !hidden.contains("composer:presets") {
            Some(self.render_preset_menu(window, cx))
        } else {
            None
        };
        let chips = self.render_attachment_chips(cx);
        let attach = crate::design::framed(
            "attach",
            Button::new("attach")
                .ghost()
                .control_sm()
                .icon(IconName::Plus)
                .tooltip(t!("conversation.attach").to_string())
                .accessibility_label(t!("conversation.attach").to_string())
                .on_click(cx.listener(|this, _, _, cx| this.pick_attachments(cx))),
        );
        let model_menu = self.render_model_menu(window, cx);
        let send = self.render_send_button(window, cx);
        let p = palette(cx);

        let mut column = v_flex().w_full().gap(px(space::S));
        if let Some(error) = &self.error {
            // 需要立即处理的错误才用带描边的块。
            // 出错时淡入；原因写在文字里。
            column = column.child(motion::fade_in(
                div()
                    .px(px(space::M))
                    .py(px(space::S))
                    .rounded(px(radius::MD))
                    .border_1()
                    .border_color(p.danger)
                    .text_body()
                    .text_color(p.danger)
                    .child(readable_with_id("composer-error", error.clone())),
                "composer-error-enter",
            ));
        }
        if self.workspace.is_none() {
            column = column.child(div().px(px(space::XS)).child(hint(t!("conversation.no_workspace_hint").to_string(), Tone::Neutral, &p)));
        }
        // 有审批在等：输入卡片的位置换成审批面板（焦点自动移过去，键盘就能答）。
        if let Some(approval) = self.render_approval(window, cx) {
            return column.children(panels).child(queue).child(approval);
        }
        let drag_border = self.file_drag;
        let typing = gpui_kit::Focusable::focus_handle(self.input.read(cx), cx).is_focused(window);
        // 文件拖进窗口时描边用强调色（松手就加为附件）；输入框有焦点时描边略深一点。
        let border = if drag_border {
            p.accent
        } else if typing {
            p.text.opacity(0.25)
        } else {
            p.border
        };
        // `/`、`@` 的候选列表浮在卡片上沿之上，不把卡片往下推。
        let suggestions = slash_menu.map(|menu| div().absolute().left_0().right_0().bottom_full().pb(px(space::XS)).occlude().child(menu));
        let card = v_flex()
            .w_full()
            .rounded(px(radius::XL))
            .border_1()
            .border_color(border)
            .bg(p.surface)
            .shadow(crate::design::shadow::composer(&p))
            .p(px(space::S))
            .gap(px(space::XS))
            // 文件拖放（GPUI 把它翻译成内部拖拽）：悬停时高亮，松手把路径加进附件。
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.file_drag = false;
                this.add_paths(paths.paths().to_vec(), cx);
            }))
            .on_drag_move(cx.listener(|this, _: &DragMoveEvent<ExternalPaths>, _, cx| {
                if !this.file_drag {
                    this.file_drag = true;
                    cx.notify();
                }
            }))
            .on_file_drop_exit(cx.listener(|this, _, _, cx| {
                this.file_drag = false;
                cx.notify();
            }))
            // 第一行：工作区、附件。没选工作区时也能先输入，发送按钮在选好之前不可用。
            .child(h_flex().flex_wrap().items_center().gap(px(space::XS)).child(context_bar).children(chips))
            // 多行输入框默认把 Tab / Shift+Tab 当缩进、反缩进，键盘焦点就困在输入框里，
            // 走不到发送、菜单和侧边栏。消息里用不到缩进：在外层先截住这两个动作，改成移动焦点。
            .child(
                div()
                    .px(px(space::S))
                    .capture_action(|_: &IndentInline, window, cx| {
                        cx.stop_propagation();
                        window.focus_next(cx);
                    })
                    .capture_action(|_: &OutdentInline, window, cx| {
                        cx.stop_propagation();
                        window.focus_prev(cx);
                    })
                    // Ctrl+V 先问剪贴板：是图片或文件（比如截图、从文件管理器复制的文件）
                    // 就加为附件，不让输入框当成纯文本处理。
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        let paste = event.keystroke.key == "v"
                            && event.keystroke.modifiers.platform
                            && !event.keystroke.modifiers.shift
                            && !event.keystroke.modifiers.alt;
                        if paste && this.paste_clipboard(window, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .child(Textarea::new(&self.input).appearance(false).aria_label(t!("a11y.message_input").to_string()).text_content()),
            )
            .child(
                h_flex()
                    .gap(px(space::XS))
                    .child(attach)
                    .children(preset_menu)
                    .children(permission_menu)
                    .child(div().flex_1())
                    .child(model_menu)
                    .child(send),
            );
        column.children(panels).child(queue).child(div().relative().w_full().child(card).children(suggestions))
    }

    /// 审批面板：在输入卡片的位置问用户是否允许这次工具调用。出现时焦点落在“允许”上；
    /// ↑ / ↓ 在选项间移动，Enter / 空格选中，1–3 直接选，Esc 拒绝。
    fn render_approval(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tool = self.transcript.pending_approval()?;
        let p = palette(cx);
        let mono = mono_font(cx);
        let name = tool_description(&tool.name).unwrap_or_else(|| tool.name.to_string());
        // 新来的审批：把焦点移过去。等这一帧画完再移（选项这时才有位置），并按键盘焦点显示焦点框。
        let fresh = self.approval_shown.borrow().as_deref() != Some(tool.call_id.as_str());
        if fresh {
            *self.approval_shown.borrow_mut() = Some(tool.call_id.clone());
            let handle = self.approval_focus[0].clone();
            window.defer(cx, move |window, cx| {
                crate::design::set_focus_from_keyboard(true, window);
                handle.focus(window, cx);
            });
        }
        let focused = self.approval_focus.iter().position(|handle| handle.is_focused(window));
        let mandatory = tool.approval_mandatory;
        // 必须亲自确认的审批没有“本会话不再询问”。序号仍按 `ApprovalChoice::ALL` 的位置编，和快捷键 1 / 2 / 3 一致
        // （“拒绝”还是 3，不会因为少了一项变成 2，按 2 时误以为在拒绝）。
        let options =
            ApprovalChoice::ALL.into_iter().enumerate().filter(move |(_, choice)| !(mandatory && *choice == ApprovalChoice::Always)).map(
                |(index, choice)| {
                    let label = match choice {
                        ApprovalChoice::Allow => t!("tool.allow"),
                        ApprovalChoice::Always => t!("tool.approval_always"),
                        ApprovalChoice::Deny => t!("tool.deny"),
                    }
                    .to_string();
                    let active = focused == Some(index);
                    let color = if choice == ApprovalChoice::Deny { p.danger } else { p.text };
                    pressable(("approval-choice", index), &p)
                        .track_focus(&self.approval_focus[index])
                        .focus_on_hover()
                        .accessibility_label(format!("{} {label}", index + 1))
                        .relative()
                        .flex()
                        .items_center()
                        .gap(px(space::S))
                        .px(px(space::M))
                        .py(px(6.))
                        .min_h(px(size::LIST_ITEM))
                        .rounded(px(radius::MD))
                        .cursor_pointer()
                        .when(active, |item| item.bg(p.selected))
                        .child(hover_fill(&p, radius::MD, false))
                        .child(
                            div()
                                .w(px(size::ICON_MD))
                                .flex_none()
                                .text_caption()
                                .text_color(p.text_secondary)
                                .child(SharedString::from((index + 1).to_string())),
                        )
                        .child(div().flex_1().min_w(px(0.)).text_body().text_color(color).child(label))
                        .when(active, |item| {
                            item.child(
                                Icon::new(gpui_kit::assets::IconName::CornerDownLeft).size(px(size::ICON_SM)).text_color(p.text_secondary),
                            )
                        })
                        .on_click(cx.listener(move |this, _, window, cx| this.answer_approval(choice, window, cx)))
                },
            );
        // 必须亲自确认时：说明这一点，并把涉及的插件能力写成人能读懂的说法（敏感的用醒目的颜色）。
        let mandatory_note = mandatory.then(|| {
            let mut column = v_flex().gap(px(2.)).px(px(space::XS)).child(
                h_flex()
                    .gap(px(space::S))
                    .items_start()
                    .child(Icon::new(gpui_kit::assets::IconName::ShieldCheck).size(px(size::ICON_SM)).mt(px(2.)).text_color(p.warning))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_caption()
                            .text_color(p.text_secondary)
                            .child(readable_with_id("approval-mandatory", t!("tool.approval_mandatory").to_string())),
                    ),
            );
            if let Some((plugin, names)) = &tool.approval_capabilities {
                let heading = if tool.name.as_ref() == "plugin_write" {
                    t!("tool.approval_write_granted", name = plugin.as_ref())
                } else {
                    t!("tool.approval_grant_list", name = plugin.as_ref())
                };
                column = column.child(div().text_caption().text_color(p.text).child(SharedString::from(heading.to_string())));
                for (index, capability) in names.iter().enumerate() {
                    let sensitive = crate::permissions::capability_sensitive(capability);
                    let text = format!("• {}（{capability}）", crate::permissions::capability_text(capability));
                    column = column.child(
                        div()
                            .id(("approval-capability", index))
                            .text_caption()
                            .text_color(if sensitive { p.warning } else { p.text_secondary })
                            .child(SharedString::from(text)),
                    );
                }
            }
            column
        });
        // 必须确认时宿主给的说明（英文，给模型看的）已经由上面的文字代替；只有不认得的工具才原样显示。
        let raw_note = tool.approval_note.as_ref().filter(|_| !mandatory || tool.approval_capabilities.is_none());
        let note = raw_note.map(|note| {
            h_flex()
                .gap(px(space::S))
                .items_start()
                .child(Icon::new(gpui_kit::assets::IconName::ShieldCheck).size(px(size::ICON_SM)).mt(px(2.)).text_color(p.warning))
                .child(div().flex_1().min_w(px(0.)).text_caption().text_color(p.text_secondary).child(readable_with_id(
                    "approval-note",
                    if mandatory {
                        t!("tool.approval_mandatory_note", note = note).to_string()
                    } else {
                        t!("tool.approval_review_note", note = note).to_string()
                    },
                )))
        });
        let panel = v_flex()
            .id("approval-panel")
            .key_context(APPROVAL_CONTEXT)
            .role(Role::Group)
            .aria_label(t!("a11y.approval_dialog", tool = name).to_string())
            .on_action(cx.listener(|this, _: &ApprovalPrev, window, cx| this.step_approval(false, window, cx)))
            .on_action(cx.listener(|this, _: &ApprovalNext, window, cx| this.step_approval(true, window, cx)))
            .on_action(cx.listener(|this, _: &ApprovalAllow, window, cx| this.answer_approval(ApprovalChoice::Allow, window, cx)))
            .on_action(cx.listener(|this, _: &ApprovalAlways, window, cx| this.answer_approval(ApprovalChoice::Always, window, cx)))
            .on_action(cx.listener(|this, _: &ApprovalDeny, window, cx| this.answer_approval(ApprovalChoice::Deny, window, cx)))
            .w_full()
            .rounded(px(radius::XL))
            .border_1()
            .border_color(p.warning)
            .bg(p.surface)
            .shadow(crate::design::shadow::composer(&p))
            .p(px(space::S))
            .gap(px(space::S))
            .child(
                h_flex()
                    .gap(px(space::S))
                    .items_center()
                    .px(px(space::XS))
                    .child(Icon::new(IconName::TriangleAlert).size(px(size::ICON_MD)).text_color(p.warning))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_label()
                            .child(readable_with_id("approval-title", t!("tool.approval_title", tool = name).to_string())),
                    ),
            )
            .child(
                div()
                    .id("approval-input")
                    .max_h(px(160.))
                    .overflow_y_scroll()
                    .px(px(space::M))
                    .py(px(space::S))
                    .rounded(px(radius::MD))
                    .bg(p.sunken)
                    .text_mono(mono)
                    .text_color(p.text_secondary)
                    .child(readable_with_id("approval-input-text", SharedString::from(tool.input.clone()))),
            )
            .children(mandatory_note)
            .children(note)
            .child(v_flex().gap(px(2.)).children(options))
            .child(div().px(px(space::XS)).child(hint(t!("tool.approval_keys").to_string(), Tone::Neutral, &p)));
        Some(panel.into_any_element())
    }

    /// 审批面板里 ↑ / ↓：焦点在选项之间循环移动。
    fn step_approval(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.approval_focus.len();
        let mandatory = self.transcript.pending_approval().is_some_and(|tool| tool.approval_mandatory);
        // 必须亲自确认时“本会话不再询问”没有画出来，跳过它的焦点。
        let hidden = |index: usize| mandatory && ApprovalChoice::ALL[index] == ApprovalChoice::Always;
        let current = self.approval_focus.iter().position(|handle| handle.is_focused(window));
        let step = |index: usize| if forward { (index + 1) % count } else { (index + count - 1) % count };
        let mut next = match (current, forward) {
            (Some(index), _) => step(index),
            (None, true) => 0,
            (None, false) => count - 1,
        };
        while hidden(next) {
            next = step(next);
        }
        self.approval_focus[next].focus(window, cx);
        cx.notify();
    }

    /// 回答等着的审批；之后焦点回到输入框（下一个审批来了会再移过去）。
    fn answer_approval(&mut self, choice: ApprovalChoice, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tool) = self.transcript.pending_approval() else { return };
        let (call_id, name, mandatory) = (tool.call_id.clone(), tool.name.to_string(), tool.approval_mandatory);
        if choice == ApprovalChoice::Always {
            // 必须亲自确认的审批没有这一项：快捷键 2 不起作用，免得把它当成别的选项按下去。
            if mandatory {
                return;
            }
            self.always_allow.insert(name);
        }
        let decision = if choice == ApprovalChoice::Deny { Decision::Deny } else { Decision::Allow };
        self.transcript.resolve_approval(&call_id, decision);
        self.sync_rows(cx);
        self.focus_input(window, cx);
        cx.notify();
    }

    /// 复制这一轮的最后一条回复（Markdown 原文）。
    fn copy_turn(&mut self, footer: usize, cx: &mut Context<Self>) {
        if let Some(text) = self.transcript.turn_reply(footer) {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            announce(t!("a11y.copied").to_string(), cx);
        }
    }

    /// 分享这一轮：用户说的话和回复整理成 Markdown，存到用户选的位置。
    fn share_turn(&mut self, footer: usize, cx: &mut Context<Self>) {
        let Some(Row::TurnFooter(info)) = self.transcript.rows.get(footer) else { return };
        let name = share_file_name(info.turn);
        let Some(markdown) = self.transcript.turn_markdown(footer) else { return };
        let dir = self.workspace.clone().unwrap_or_else(|| self.backend.home().to_owned());
        let picked = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            let written = match picked.await {
                Ok(Ok(Some(path))) => std::fs::write(&path, markdown).map(|_| path).map_err(|error| error.to_string()),
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => Err(error.to_string()),
            };
            this.update(cx, |this, cx| match written {
                Ok(path) => {
                    let text = t!("conversation.shared", path = path.display()).to_string();
                    announce(text.clone(), cx);
                    this.transcript.notice(text);
                    this.sync_rows(cx);
                    cx.notify();
                }
                Err(error) => this.show_error(t!("conversation.share_failed", error = error).to_string(), cx),
            })
            .ok();
        })
        .detach();
    }

    /// 一轮结束处的一行：复制、分享、分叉，这一轮的用量和结束时间。
    fn render_footer(&self, index: usize, footer: &TurnFooter, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let turn = footer.turn;
        let icon_button = |id: &'static str, icon: gpui_kit::assets::IconName, label: String| {
            Button::new((id, index)).ghost().control_xs().icon(icon).tooltip(label.clone()).accessibility_label(label)
        };
        let usage = (footer.tokens > 0).then(|| {
            h_flex()
                .id(("turn-usage", index))
                .role(Role::Label)
                .aria_description(t!("conversation.usage_detail", tokens = footer.tokens).to_string())
                .ml(px(space::S))
                .gap(px(space::XS))
                .items_center()
                .child(Icon::new(gpui_kit::assets::IconName::Database).size(px(size::ICON_SM)))
                .child(readable_with_id(
                    ("turn-usage-text", index),
                    t!("conversation.usage", tokens = compact_tokens(footer.tokens)).to_string(),
                ))
        });
        let time = footer_time(footer.time).map(|time| div().ml(px(space::S)).child(readable_with_id(("turn-time", index), time)));
        h_flex()
            .gap(px(space::XS))
            .items_center()
            .text_caption()
            .text_color(p.text_secondary)
            .child(crate::design::framed(
                ("copy-turn", index),
                icon_button("copy-turn", IconName::Copy.into(), t!("conversation.copy_reply").to_string())
                    .on_click(cx.listener(move |this, _, _, cx| this.copy_turn(index, cx))),
            ))
            .child(crate::design::framed(
                ("share-turn", index),
                icon_button("share-turn", gpui_kit::assets::IconName::Share2, t!("conversation.share").to_string())
                    .on_click(cx.listener(move |this, _, _, cx| this.share_turn(index, cx))),
            ))
            .child(crate::design::framed(
                ("fork", index),
                icon_button("fork", gpui_kit::assets::IconName::GitBranch, t!("conversation.fork").to_string())
                    .disabled(self.is_running() || self.session_id.is_none())
                    .on_click(cx.listener(move |this, _, _, cx| this.fork_at(turn, cx))),
            ))
            .children(usage)
            .children(time)
            .into_any_element()
    }

    /// 消息列表那一块：列表、顶部淡出、“回到最新”按钮。由 [`TranscriptView`] 画。
    fn render_transcript_area(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let transcript = self.render_transcript(cx);
        // 往下看过之后，内容会从面板顶边滚出去：顶上淡出一小段，内容渐隐进标题栏下面，而不是被一刀切断。
        // 停在最上面时什么都不盖（那里本来就是留白）。它贴着面板的左上角，要跟着面板一起圆角，
        // 不然方角会用底色把面板圆角外面的框架色盖掉。
        let top = self.list.read(cx).logical_scroll_top();
        let scrolled = top.item_ix > 0 || top.offset_in_item > px(0.);
        // 用键盘在列表里走时不盖：滚到顶上的那一行的焦点框不能被它遮住。
        let keyboard_inside = crate::design::focus_from_keyboard() && self.focused_row(window, cx).is_some();
        let fade = motion::tween("transcript-top-fade", if scrolled && !keyboard_inside { 1. } else { 0. }, window, cx);
        let jump = self.render_jump_button(window, cx);
        let viewport = self.viewport_bounds.clone();
        v_flex()
            .relative()
            .flex_1()
            .size_full()
            .min_h(px(0.))
            .key_context(TRANSCRIPT_CONTEXT)
            .on_action(cx.listener(|this, _: &TranscriptTabNext, window, cx| this.on_transcript_tab(true, window, cx)))
            .on_action(cx.listener(|this, _: &TranscriptTabPrev, window, cx| this.on_transcript_tab(false, window, cx)))
            .child(transcript)
            .child(canvas(move |area, _, _| viewport.set(Some(area)), |_, _, _, _| {}).absolute().top_0().left_0().size_full())
            .children(jump)
            .when(fade > 0., |area| {
                area.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(space::XXL))
                        .rounded_tl(px(crate::title_bar::panel_inner_radius()))
                        .opacity(fade)
                        .bg(linear_gradient(180., linear_color_stop(p.surface, 0.), linear_color_stop(p.surface.opacity(0.), 1.))),
                )
            })
            .into_any_element()
    }

    /// 消息列表。虚拟化：只渲染视口附近的行，一帧的开销和会话长度无关。
    /// 停在底部时自动跟随新内容，往回翻看时不打扰，右下出现“回到最新”按钮。
    fn render_transcript(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        MessageScroller::new(
            "transcript",
            self.list.clone(),
            // 行在布局阶段才渲染（此时本视图的 render 已经返回），通过 processor 回到视图里取数据。
            cx.processor(|this, index: usize, window, cx| this.render_row(index, window, cx)),
        )
        .flex_1()
        .min_h(px(0.))
        // “跳到最新消息”自己画，见 `render_jump_button`。
        .jump_button(false)
        .with_bottom_fade(p.surface)
        // 行与行的间距、列表上下的留白都算在行里（列表按行高计算滚动范围）。
        .map(|scroller| {
            if crate::design::high_visibility_outlines() {
                // 高可见性轮廓：行间距一半放在行的上面。行被滚到视口顶上时（Tab、方向键走到它），
                // 上面留着画外轮廓的地方，不会被视口切掉。
                let half = space::L / 2.;
                scroller
                    .with_list_style(StyleRefinement::default().pt(px(space::XXL - half)).pb(px(space::XXL - space::L)))
                    .with_row_style(StyleRefinement::default().px(px(0.)).pt(px(half)).pb(px(half)))
            } else {
                scroller
                    .with_list_style(StyleRefinement::default().pt(px(space::XXL)).pb(px(space::XXL - space::L)))
                    .with_row_style(StyleRefinement::default().px(px(0.)).pb(px(space::L)))
            }
        })
        .into_any_element()
    }

    /// “跳到最新消息”：往回翻看时出现在列表底部。按下后回到底部，焦点放到最新一条消息上
    /// （读屏软件读出它）；没有消息时回到输入框。按钮有焦点时一直画着，回到底部之后焦点也不会丢。
    fn render_jump_button(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let scrolled_up = self.list.read(cx).is_scrolled_up();
        if !scrolled_up && !self.jump_focus.is_focused(window) {
            return None;
        }
        let p = palette(cx);
        let label = t!("conversation.jump_to_latest").to_string();
        let button = pressable("jump-to-latest", &p)
            .track_focus(&self.jump_focus)
            .accessibility_label(label.clone())
            .size(px(size::CONTROL_MD))
            .rounded_full()
            .border_color(p.border)
            .bg(p.surface)
            .text_color(p.text)
            .shadow(crate::design::shadow::menu(&p))
            .cursor_pointer()
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(label.clone()).build(window, cx))
            .relative()
            .child(hover_fill(&p, size::CONTROL_MD / 2., false))
            .child(Icon::new(IconName::ArrowDown).size(px(size::ICON_MD)))
            .on_click(cx.listener(|this, _, window, cx| this.jump_to_latest(window, cx)));
        let row = div().absolute().left_0().right_0().bottom(px(space::L)).flex().justify_center().child(button);
        Some(row.into_any_element())
    }

    /// 回到最新内容，焦点放到最后一条消息上（没有消息时放回输入框）。
    fn jump_to_latest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let last = self.message_rows().next_back();
        match last {
            Some(last) => self.focus_message(last, window, cx),
            None => {
                self.scroll_to_latest(cx);
                self.focus_input(window, cx);
            }
        }
    }

    /// 把焦点放到对话内容上（“跳到对话内容”）：上次停留的消息，没有时是最后一条；还没有消息时是输入框。
    pub fn focus_content(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_message.filter(|index| self.transcript.rows.get(*index).is_some_and(is_message));
        let target = current.or_else(|| self.message_rows().next_back());
        match target {
            Some(index) => self.focus_message(index, window, cx),
            None => self.focus_input(window, cx),
        }
    }

    fn row_handle(&mut self, index: usize, cx: &mut Context<Self>) -> FocusHandle {
        self.row_focus.entry(index).or_insert_with(|| cx.focus_handle().tab_stop(false)).clone()
    }

    /// 焦点在第几行里（行本身或行里的控件）。
    fn focused_row(&self, window: &Window, cx: &App) -> Option<usize> {
        self.row_focus.iter().find(|(_, handle)| handle.contains_focused(window, cx)).map(|(index, _)| *index)
    }

    /// 这一行里可能有 Tab 停靠点：工具卡片的标题、“从这里分叉”、插件画的行，和消息列表里那条当停靠点的消息。
    /// 只是猜：猜错了（比如分叉按钮正好不可用）就接着找下一行。
    fn row_may_have_tab_stop(&self, index: usize) -> bool {
        match self.transcript.rows.get(index) {
            Some(Row::Tool(_) | Row::TurnFooter { .. } | Row::Ext(_) | Row::Plugin { .. }) => true,
            Some(row) if is_message(row) => {
                let stop = self.current_message.filter(|current| *current < self.transcript.rows.len());
                stop.map_or(self.last_message_row == Some(index), |stop| stop == index)
            }
            _ => false,
        }
    }

    /// `from` 之后（`forward`）或之前第一行可能有 Tab 停靠点的行。
    fn next_tab_row(&self, from: usize, forward: bool) -> Option<usize> {
        if forward {
            (from + 1..self.transcript.rows.len()).find(|index| self.row_may_have_tab_stop(*index))
        } else {
            (0..from).rev().find(|index| self.row_may_have_tab_stop(*index))
        }
    }

    /// 第 `index` 行不完全在视口里时把它滚进来（停在顶上）。
    fn reveal_row(&mut self, index: usize, window: &Window, cx: &mut Context<Self>) {
        let (Some(row), Some(viewport)) = (self.row_bounds.borrow().get(&index).copied(), self.viewport_bounds.get()) else {
            return;
        };
        let on_message = self.message_focus.get(&index).is_some_and(|handle| handle.is_focused(window));
        if !row_is_revealed(row, viewport, on_message) {
            self.list.update(cx, |list, cx| list.scroll_to_item(index, cx));
        }
    }

    /// 键盘焦点走到了第 `index` 行：等这一帧画完再看它在不在视口里（用这一帧的位置，不是上一帧留下的）。
    fn request_reveal(&mut self, index: usize, cx: &mut Context<Self>) {
        self.pending_reveal = Some(index);
        cx.notify();
    }

    /// 第 `index` 行这一帧画好了（`render_row` 的绘制回调里登记）：需要的话把它滚进视口。
    fn finish_reveal(&mut self, index: usize, window: &Window, cx: &mut Context<Self>) {
        if self.pending_reveal.take_if(|pending| *pending == index).is_some() {
            self.reveal_row(index, window, cx);
        }
    }

    /// 消息列表里的 Tab / Shift+Tab。
    ///
    /// 列表是虚拟化的：只有视口附近的行画出来了，系统的 Tab 顺序里只有它们。照常走到下一个控件；
    /// 走到的控件还在列表里，就把它所在的行滚进视口。走出了列表、但后面（前面）还有没画出来的行时，
    /// 不出去，先把那一行滚出来，等它画好再走到它里面的第一个（最后一个）控件——整段对话都能用 Tab 走完，
    /// 不必把所有行都画出来。
    fn on_transcript_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.focused_row(window, cx);
        self.pending_reveal = None;
        if forward {
            window.focus_next(cx);
        } else {
            window.focus_prev(cx);
        }
        let Some(from) = from else { return };
        if let Some(to) = self.focused_row(window, cx) {
            self.request_reveal(to, cx);
            return;
        }
        let outside = window.focused(cx);
        if let Some(target) = self.next_tab_row(from, forward) {
            self.tab_into_row(target, forward, outside, 0, window, cx);
        }
    }

    /// 把第 `target` 行滚出来，焦点先放在这一行上；它画出来之后（`render_row` 里登记的绘制回调）
    /// 再走进去，见 [`Self::finish_tab_into_row`]。
    fn tab_into_row(
        &mut self,
        target: usize,
        forward: bool,
        outside: Option<FocusHandle>,
        attempts: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handle = self.row_handle(target, cx);
        self.pending_tab = Some(PendingTab { row: target, forward, outside, attempts });
        self.list.update(cx, |list, cx| list.scroll_to_item(target, cx));
        handle.focus(window, cx);
        cx.notify();
    }

    /// 第 `target` 行画出来了（这一帧画完之后调用，Tab 顺序里已经有它的控件）：走到它里面的第一个
    /// （Shift+Tab 时最后一个）控件；它里面没有控件就接着找下一行，都没有就照常走出列表。
    fn finish_tab_into_row(&mut self, target: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(PendingTab { forward, outside, attempts, .. }) = self.pending_tab.take_if(|pending| pending.row == target) else {
            return;
        };
        let handle = self.row_handle(target, cx);
        // 这期间焦点被别的操作移走了（点了别处、按了别的键）：不再接着走。
        if !handle.is_focused(window) {
            return;
        }
        window.focus_next(cx);
        let landed = self.focused_row(window, cx).filter(|_| !handle.is_focused(window));
        let landed = match landed {
            // Shift+Tab：走到这一行里的最后一个控件。
            Some(row) if !forward && row == target => {
                loop {
                    let current = window.focused(cx);
                    window.focus_next(cx);
                    if self.focused_row(window, cx) != Some(target) || window.focused(cx) == current {
                        if let Some(current) = current {
                            current.focus(window, cx);
                        }
                        break;
                    }
                }
                Some(target)
            }
            Some(row) if forward => Some(row),
            _ => None,
        };
        match landed {
            Some(row) => self.request_reveal(row, cx),
            None => match self.next_tab_row(target, forward).filter(|_| attempts + 1 < TAB_SEARCH_LIMIT) {
                Some(next) => self.tab_into_row(next, forward, outside, attempts + 1, window, cx),
                // 后面（前面）的行里都没有控件：照常走出列表。
                None => match outside {
                    Some(outside) => outside.focus(window, cx),
                    None => self.focus_input(window, cx),
                },
            },
        }
    }

    /// 能用键盘走到的消息（用户消息、回复、提示）在记录里的位置。
    fn message_rows(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        self.transcript.rows.iter().enumerate().filter(|(_, row)| is_message(row)).map(|(index, _)| index)
    }

    fn message_handle(&mut self, index: usize, cx: &mut Context<Self>) -> FocusHandle {
        self.message_focus.entry(index).or_insert_with(|| cx.focus_handle()).clone()
    }

    /// 把焦点移到第 `index` 行的消息上，并滚动到能看见它。
    fn focus_message(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.current_message = Some(index);
        let last = self.message_rows().next_back() == Some(index);
        self.list.update(cx, |list, cx| {
            if last {
                list.scroll_to_end(cx);
            } else {
                list.scroll_to_item(index, cx);
            }
        });
        self.message_handle(index, cx).focus(window, cx);
        cx.notify();
    }

    /// 等这次鼠标事件处理完，把焦点放到第 `index` 行的消息本身上（见 `render_row` 里的说明）。
    fn claim_message_focus(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            view.update(cx, |this, cx| {
                // 文字选择会把焦点给 Markdown 视图里面的一层（读屏软件读不到有用的东西）：
                // 焦点不在消息本身上就拿回来。
                let handle = this.message_handle(index, cx);
                if !handle.is_focused(window) {
                    handle.focus(window, cx);
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn on_message_prev(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.message_rows().rfind(|row| *row < index);
        if let Some(target) = target {
            self.focus_message(target, window, cx);
        }
    }

    fn on_message_next(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.message_rows().find(|row| *row > index);
        if let Some(target) = target {
            self.focus_message(target, window, cx);
        }
    }

    fn copy_message(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(text) = self.transcript.rows.get(index).and_then(message_plain) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            announce(t!("a11y.copied").to_string(), cx);
        }
    }

    /// 一行内容，居中并限制最大宽度。
    ///
    /// 消息（用户消息、回复、提示）可以获得焦点：读屏软件读出全文（Markdown 已去掉标记），
    /// ↑ / ↓ / Home / End 在消息之间移动，Ctrl+C 复制整条，Esc 回到输入框。
    /// 列表里只有一条消息是 Tab 停靠点（上次停留的那条，默认最后一条），Tab 不会在长对话里一条条走。
    fn render_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        #[cfg(test)]
        {
            self.rows_rendered += 1;
        }
        // 列表的行数在 `sync_rows` 里与记录对齐；万一不一致，多出来的行画成空的。
        let Some(row) = self.transcript.rows.get(index) else {
            return div().into_any_element();
        };
        // 读屏软件没连着时不拼名称：回复的名称是去掉 Markdown 标记的全文，正在输出的那一行每一帧都要重算一遍，
        // 没人读的时候白算（无障碍树也不建）。消息照样可以获得焦点。
        let label = if window.is_a11y_active() { message_text(row) } else { is_message(row).then(String::new) };
        let content = self.render_row_content(index, row, window, cx);
        let content = match label {
            Some(label) => {
                let p = palette(cx);
                let stop = self.current_message.filter(|current| *current < self.transcript.rows.len());
                let is_stop = stop.map_or_else(|| self.last_message_row == Some(index), |stop| stop == index);
                let focus = self.message_handle(index, cx).tab_stop(is_stop);
                let focused = focus.is_focused(window);
                div()
                    .id(("message", index))
                    .key_context(MESSAGE_CONTEXT)
                    .track_focus(&focus)
                    .role(Role::Article)
                    .aria_label(label)
                    // 操作说明只在第一次进入消息列表时读：之后每条都读一遍太吵。
                    .when(self.current_message.is_none(), |message| message.aria_description(t!("a11y.message_help").to_string()))
                    // 焦点框的圆角和用户气泡一样（不比里面的气泡小）。
                    .rounded(px(radius::XL))
                    .border_2()
                    .border_color(transparent_black())
                    .relative()
                    .map(|message| with_focus_ring(message, focused, radius::XL, window, &p))
                    // 点在消息的任何地方都让它获得焦点（读屏软件读出全文）。回复正文是可选择文字的 Markdown，
                    // 文字选择会把按下事件拿走，消息自己的“按下就聚焦”轮不到，以前只有点在思考那一段才行。
                    // 在捕获阶段登记、等这次按下处理完再聚焦：不打断文字选择，也不会被别的处理再抢走。
                    .capture_any_mouse_down(cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        if event.button == MouseButton::Left {
                            this.current_message = Some(index);
                            this.claim_message_focus(index, window, cx);
                        }
                    }))
                    // 松开时文字选择还会再把焦点给 Markdown 视图一次，同样拿回来。
                    .capture_any_mouse_up(cx.listener(move |this, event: &MouseUpEvent, window, cx| {
                        if event.button == MouseButton::Left {
                            this.claim_message_focus(index, window, cx);
                        }
                    }))
                    .on_action(cx.listener(move |this, _: &MessagePrev, window, cx| this.on_message_prev(index, window, cx)))
                    .on_action(cx.listener(move |this, _: &MessageNext, window, cx| this.on_message_next(index, window, cx)))
                    .on_action(cx.listener(|this, _: &MessageFirst, window, cx| {
                        let first = this.message_rows().next();
                        if let Some(first) = first {
                            this.focus_message(first, window, cx);
                        }
                    }))
                    .on_action(cx.listener(|this, _: &MessageLast, window, cx| {
                        let last = this.message_rows().next_back();
                        if let Some(last) = last {
                            this.focus_message(last, window, cx);
                        }
                    }))
                    .on_action(cx.listener(move |this, _: &MessageCopy, window, cx| {
                        // 用鼠标选了一段文字：复制选中的部分（交给组件库根上的复制），而不是整条消息。
                        if gpui_kit::base::TextSelection::has_selection(window, cx) {
                            cx.propagate();
                        } else {
                            this.copy_message(index, cx);
                        }
                    }))
                    .on_action(cx.listener(|this, _: &MessageLeave, window, cx| {
                        this.list.update(cx, |list, cx| list.scroll_to_end(cx));
                        this.focus_input(window, cx);
                    }))
                    .child(content)
                    .into_any_element()
            }
            None => content,
        };
        let row_focus = self.row_handle(index, cx);
        let bounds = self.row_bounds.clone();
        // Tab 正等着这一行画出来：画完这一帧再走进去。
        let pending = self.pending_tab.as_ref().is_some_and(|pending| pending.row == index).then(|| cx.entity().downgrade());
        let focus_message = (self.pending_message_focus == Some(index)).then(|| cx.entity().downgrade());
        let reveal = (self.pending_reveal == Some(index)).then(|| cx.entity().downgrade());
        let row = div()
            .id(("row", index))
            .track_focus(&row_focus)
            // 行本身能有焦点只是为了 Tab（见 `on_transcript_tab`）：点在行里的空白、文字上不把焦点抢过来。
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .relative()
            .w_full()
            .max_w(px(size::CONTENT_MAX_WIDTH))
            .mx_auto()
            .px(px(space::XXL))
            .child(content)
            .child(
                canvas(
                    move |area, _, _| {
                        bounds.borrow_mut().insert(index, area);
                    },
                    move |_, _, window, cx| {
                        if let Some(view) = focus_message.clone() {
                            window.defer(cx, move |window, cx| {
                                view.update(cx, |this, cx| {
                                    if this.pending_message_focus.take() == Some(index) {
                                        // 程序自己放的焦点（给读屏软件读）：默认不画焦点框，高可见性轮廓开启时照画。
                                        crate::design::set_focus_from_keyboard(false, window);
                                        this.message_handle(index, cx).focus(window, cx);
                                        cx.notify();
                                    }
                                })
                                .ok();
                            });
                        }
                        if let Some(view) = pending.clone() {
                            window.defer(cx, move |window, cx| {
                                view.update(cx, |this, cx| this.finish_tab_into_row(index, window, cx)).ok();
                            });
                        }
                        if let Some(view) = reveal.clone() {
                            window.defer(cx, move |window, cx| {
                                view.update(cx, |this, cx| this.finish_reveal(index, window, cx)).ok();
                            });
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        // 新行直接出现（不做进场动画：流式输出本身就在变化，再滑动只会拖慢阅读）。
        row.into_any_element()
    }

    fn render_row_content(&self, index: usize, row: &Row, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        match row {
            Row::User { text, attachments } => v_flex()
                .items_end()
                .gap(px(space::XS))
                .child(
                    div()
                        .max_w(px(size::CONTENT_MAX_WIDTH * 0.75))
                        .px(px(space::M))
                        .py(px(space::S))
                        .rounded(px(radius::XL))
                        .bg(p.selected)
                        .text_content()
                        .child(readable_with_id(("user", index), text.clone())),
                )
                .when(!attachments.is_empty(), |column| {
                    column.child(h_flex().flex_wrap().justify_end().gap(px(space::XS)).children(attachments.iter().enumerate().map(
                        |(attachment_index, path)| {
                            h_flex()
                                .gap(px(space::XS))
                                .px(px(space::S))
                                .rounded(px(radius::MD))
                                .bg(p.sunken)
                                .text_caption()
                                .text_color(p.text_secondary)
                                .child(Icon::new(IconName::File).size(px(size::ICON_SM)))
                                .child(readable_with_id(
                                    SharedString::from(format!("user-{index}-attachment-{attachment_index}")),
                                    file_label(path),
                                ))
                        },
                    )))
                })
                .into_any_element(),
            // 正在输出的那一行文本每帧都在变；纯文本在事件到达时已经算好（见 `Transcript::apply`）。
            Row::Streaming { text, thinking, .. } => {
                // 纯文本只给读屏软件用：没连着时不算（流式输出时每帧都得重算整段）。
                let plain = (!text.is_empty())
                    .then(|| if window.is_a11y_active() { row.plain().map(SharedString::from) } else { Some(SharedString::default()) })
                    .flatten();
                self.render_reply(index, thinking.clone().into(), text.clone().into(), plain, true, &p)
            }
            Row::Assistant { text, thinking, .. } => {
                let plain = if window.is_a11y_active() { row.plain().map(SharedString::from) } else { Some(SharedString::default()) };
                self.render_reply(index, thinking.clone().into(), text.clone().into(), plain, false, &p)
            }
            Row::Tool(tool) => self.render_tool(index, tool, window, cx),
            Row::Notice { text, error } => {
                // 同样的提示可能出现多次（如多次压缩），按行号给 id。
                let tone = if *error { Tone::Danger } else { Tone::Neutral };
                div().text_caption().text_color(tone.color(&p)).child(readable_with_id(("notice", index), text.clone())).into_any_element()
            }
            // 插件事件由登记了这个类型的界面插件画；能进到行里的都有 renderer（见 `Transcript`）。
            Row::Ext(row) => match self.ui.transcript_renderer(&row.kind).cloned() {
                Some(renderer) => renderer.render(row, index, cx),
                None => div().into_any_element(),
            },
            // JS 插件给事件画的节点树：只读地显示。
            Row::Plugin { tree } => crate::plugin_page::static_tree(tree, &format!("plugin-row-{index}"), cx),
            Row::TurnFooter(footer) => self.render_footer(index, footer, cx),
        }
    }

    /// 一条回复：思考（次要色、左侧竖线）加 Markdown 正文。`plain` 是给读屏软件的纯文本。
    fn render_reply(
        &self,
        index: usize,
        thinking: SharedString,
        text: SharedString,
        plain: Option<SharedString>,
        streaming: bool,
        p: &crate::design::Palette,
    ) -> AnyElement {
        let mut column = v_flex().gap(px(space::S));
        if !thinking.is_empty() {
            column = column.child(
                div()
                    .pl(px(space::M))
                    .border_l_2()
                    .border_color(p.border)
                    .text_body()
                    .text_color(p.text_secondary)
                    .child(readable_with_id(("thinking", index), thinking)),
            );
        }
        match plain {
            // Markdown 视图本身不进入无障碍树，外层给读屏软件一份去掉标记的纯文本。
            Some(plain) if !text.is_empty() => {
                column = column.child(
                    div()
                        .id(("assistant-reply", index))
                        .role(Role::Label)
                        .aria_value(plain)
                        .text_content()
                        .child(TextView::markdown(("assistant", index), text)),
                );
            }
            // 还没有文字：三个点依次亮起（减少动态效果时是三个一样淡的点）。读屏软件读“正在回复”。
            _ if streaming => {
                column = column.child(motion::typing_dots(p.text_secondary));
            }
            _ => {}
        }
        column.into_any_element()
    }

    fn render_tool(&self, index: usize, tool: &ToolRow, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let mono = mono_font(cx);
        let (status, tone) = match &tool.status {
            ToolStatus::Running if tool.approval.is_some() => (t!("tool.waiting"), Tone::Warning),
            ToolStatus::Running => (t!("tool.running"), Tone::Neutral),
            ToolStatus::Done { is_error: false, .. } => (t!("tool.done"), Tone::Success),
            ToolStatus::Done { is_error: true, .. } => (t!("tool.failed"), Tone::Danger),
            ToolStatus::Cancelled => (t!("tool.cancelled"), Tone::Neutral),
        };
        // 这个工具有自己的卡片时，标题里的附加信息和展开后的内容由它画（如文件改动的 diff）。
        let tool_view = self.ui.tool_card(&tool.name).cloned();
        // 没有界面插件的卡片时，工具所在的 JS 插件可能给它画了卡片（`cards` 导出）。
        let plugin_card = match &tool_view {
            None => tool.data.as_ref().and_then(|data| data.downcast_ref::<crate::transcript::PluginCard>()).map(|card| &card.0),
            Some(_) => None,
        };
        let summary = match plugin_card {
            Some(card) => card
                .title
                .clone()
                .map(|title| div().flex_none().text_caption().text_color(p.text_secondary).child(title).into_any_element()),
            None => tool_view.as_ref().and_then(|view| view.render_summary(tool, &p)),
        };
        let toggle_id = tool.call_id.clone();
        // 运行结束后标题才能展开、折叠输出；读屏软件读出状态，并把参数作为补充说明。
        let expandable = matches!(tool.status, ToolStatus::Done { .. });
        let header = pressable(("tool-header", index), &p)
            .accessibility_label(match tool_description(&tool.name) {
                Some(desc) => format!("{} {}", desc, status),
                None => format!("{} {}", tool.name, status),
            })
            // 参数可能是整段 JSON（写入的文件内容、长命令）：只读开头一段，全文展开后在输出区里。
            .when(!tool.input.is_empty(), |header| header.aria_description(brief(&tool.input)))
            .when(expandable, |header| header.aria_expanded(tool.expanded))
            .rounded(px(radius::LG))
            .flex()
            .items_center()
            .px(px(space::M))
            .py(px(6.))
            .min_h(px(size::LIST_ITEM))
            .gap(px(space::S))
            .cursor_pointer()
            .relative()
            .child(hover_fill(&p, radius::LG, false))
            .on_click(cx.listener(move |this, _, _, cx| {
                let expanded = this.transcript.toggle_tool(&toggle_id);
                this.sync_rows(cx);
                // 展开、折叠即时完成；列表缓存了行高，这一行要重新测量。
                this.list.update(cx, |list, cx| list.remeasure_items(index..index + 1, cx));
                // 列表停在底部时会跟着内容往上滚；展开的卡片变高，刚点的标题可能被顶出视口。
                // 这时改为停在这张卡片上，让它留在眼前。
                if expanded {
                    this.list.update(cx, |list, cx| {
                        if list.is_following_tail() {
                            list.scroll_to_item(index, cx);
                        }
                    });
                }
                cx.notify();
            }))
            .child(div().size(px(size::STATUS_DOT)).flex_none().rounded_full().bg(tone.color(&p)))
            .child(div().flex_none().text_label().child(match tool_description(&tool.name) {
                Some(desc) => SharedString::from(desc),
                None => SharedString::from(tool.name.clone()),
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_mono(mono.clone())
                    .text_color(p.text_secondary)
                    .child(SharedString::from(tool.input.clone())),
            )
            .children(summary)
            .child(hint(status.to_string(), tone, &p))
            // 能展开的卡片在最右边放一个箭头，展开时转向下。
            .when(expandable, |header| {
                header.child(motion::turn(
                    Icon::new(IconName::ChevronRight).size(px(size::ICON_SM)).text_color(p.text_secondary),
                    ("tool-turn", index),
                    tool.expanded,
                    motion::QUARTER_TURN,
                    window,
                    cx,
                ))
            });

        let mut tool_card = card(&p).overflow_hidden().child(header);

        // 审批在输入框的位置（见 `render_approval`）；卡片上只提示一句，状态由标题里的“等待审批”表达。
        if tool.approval.is_some() {
            tool_card = tool_card.child(divider(&p)).child(div().px(px(space::M)).py(px(space::S)).child(hint(
                t!("tool.approval_below").to_string(),
                Tone::Warning,
                &p,
            )));
        }

        // 工具交回的图片：缩略图，一直显示（模型看到的也是这些）。图片在事件到达时已经解码，这里直接画。
        if !tool.images.is_empty() {
            let thumbnails = tool.images.iter().enumerate().map(|(image_index, image)| {
                div()
                    .id(SharedString::from(format!("tool-image-{index}-{image_index}")))
                    .role(Role::Image)
                    .aria_label(t!("tool.image", index = image_index + 1).to_string())
                    .child(img(image.clone()).max_w_full().w(px(160.)))
            });
            tool_card = tool_card.child(divider(&p)).child(h_flex().flex_wrap().gap(px(space::S)).p(px(space::S)).children(thumbnails));
        }

        // 终端面板：命令运行期间显示实时输出的最后几行，结束后折叠成结果。
        if tool_shows_live(tool) {
            let tail = tail_lines(&tool.live, LIVE_LINES);
            let live = v_flex().child(divider(&p)).child(
                div()
                    .px(px(space::M))
                    .py(px(space::S))
                    .bg(p.sunken)
                    .text_mono(mono.clone())
                    .child(readable_with_id(("live-output", index), tail)),
            );
            tool_card = tool_card.child(live);
        }

        // 展开、折叠即时完成。
        if tool.expanded
            && let ToolStatus::Done { output, .. } = &tool.status
        {
            let body = div().id(("tool-output", index)).max_h(px(320.)).overflow_y_scroll().bg(p.sunken).text_mono(mono);
            let plugin_body = plugin_card
                .and_then(|card| card.body.as_ref())
                .map(|tree| crate::plugin_page::static_tree(tree, &format!("tool-card-{index}"), cx));
            // 界面插件画的输出（如 diff）自己管左右留白：着色的行要铺满整行宽度。
            let body = match (plugin_body, tool_view.as_ref().and_then(|view| view.render_output(tool, cx))) {
                (Some(content), _) => body.child(div().px(px(space::M)).py(px(space::S)).child(content)),
                (None, Some(content)) => body.child(content),
                (None, None) => body.px(px(space::M)).py(px(space::S)).child(readable_with_id("output", output.clone())),
            };
            tool_card = tool_card.child(v_flex().child(divider(&p)).child(body));
        }
        tool_card.into_any_element()
    }
}

/// 运行中、已经有实时输出时显示终端面板。
/// 读屏软件读的简短版本：合并空白，超过 [`BRIEF_CHARS`] 个字符截断并加省略号。
fn brief(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= BRIEF_CHARS {
        return flat;
    }
    let mut short: String = flat.chars().take(BRIEF_CHARS).collect();
    short.push('…');
    short
}

/// 工具参数读多少个字符。
const BRIEF_CHARS: usize = 80;

fn tool_shows_live(tool: &ToolRow) -> bool {
    tool.status == ToolStatus::Running && !tool.live.is_empty()
}

/// 内置工具的一句话说明（跟随界面语言）；别的工具（插件、MCP）没有说明，显示原名。
fn tool_description(name: &str) -> Option<String> {
    let desc = match name {
        "read_file" => t!("tool.desc_read_file"),
        "write_file" => t!("tool.desc_write_file"),
        "edit_file" => t!("tool.desc_edit_file"),
        "run_shell" => t!("tool.desc_run_shell"),
        "list_dir" => t!("tool.desc_list_dir"),
        "glob" => t!("tool.desc_glob"),
        "grep" => t!("tool.desc_grep"),
        "delegate" => t!("tool.desc_delegate"),
        "update_plan" => t!("tool.desc_update_plan"),
        _ => return None,
    };
    Some(desc.to_string())
}

/// 能用键盘走到的消息（用户消息、回复、提示）；工具卡片、分叉按钮、插件行有自己的可聚焦控件。
/// 只看行的种类，不拼名称：`sync_rows` 每次都要对整份记录判断一遍。
/// 键盘焦点所在的第几行（`row`）在消息列表的视口（`viewport`）里算不算“看得见”，看不见就要滚到它。
///
/// - 放得下的行：要整行都在视口里。
/// - 比视口高的行，焦点在消息本身上（长回复）：露出至少一行列表项高就行，不因为顶边在上面就跳回顶上打断阅读。
///   以前只要“顶边在视口顶边以下”就算看得见：长回复只在视口底边露出一小条时，焦点到了它上面列表却不动。
/// - 比视口高的行，焦点在行里的控件上（工具卡片的标题、按钮都在行的顶部）：顶边要在视口里。
fn row_is_revealed(row: Bounds<Pixels>, viewport: Bounds<Pixels>, on_message: bool) -> bool {
    // 至少露出这么多才算看得见（一行列表项的高度）。
    let margin = px(size::LIST_ITEM);
    if row.size.height <= viewport.size.height {
        row.top() >= viewport.top() && row.bottom() <= viewport.bottom()
    } else if on_message {
        row.top() < viewport.bottom() - margin && row.bottom() > viewport.top() + margin
    } else {
        row.top() >= viewport.top() && row.top() < viewport.bottom() - margin
    }
}

fn is_message(row: &Row) -> bool {
    matches!(row, Row::User { .. } | Row::Assistant { .. } | Row::Streaming { .. } | Row::Notice { .. })
}

/// 消息在读屏软件里的名称（带“你：”“回复：”前缀，Markdown 去掉标记）；不是消息的行为 `None`。
/// 只在画看得见的那几行时调用。
fn message_text(row: &Row) -> Option<String> {
    match row {
        Row::User { text, .. } => Some(t!("a11y.you_said", text = text).to_string()),
        Row::Streaming { text, .. } if text.is_empty() => Some(t!("a11y.replying").to_string()),
        Row::Assistant { .. } | Row::Streaming { .. } => Some(t!("a11y.reply", text = row.plain()?).to_string()),
        Row::Notice { text, .. } => Some(text.to_string()),
        _ => None,
    }
}

/// 复制时用的纯文本（不带前缀）。
fn message_plain(row: &Row) -> Option<String> {
    match row {
        Row::User { text, .. } => Some(text.to_string()),
        Row::Streaming { text, .. } if text.is_empty() => None,
        Row::Assistant { .. } | Row::Streaming { .. } => row.plain().map(|plain| plain.to_string()),
        Row::Notice { text, .. } => Some(text.to_string()),
        _ => None,
    }
}

/// 思考等级菜单的选项；`None` 是“默认”，不发送。
pub(crate) const EFFORT_CHOICES: [Option<ReasoningEffort>; 5] =
    [None, Some(ReasoningEffort::Off), Some(ReasoningEffort::Low), Some(ReasoningEffort::High), Some(ReasoningEffort::Max)];

/// 思考等级在 [`EFFORT_CHOICES`] 里的位置（认不出时当作“默认”）。
fn effort_index(effort: Option<ReasoningEffort>) -> usize {
    EFFORT_CHOICES.iter().position(|choice| *choice == effort).unwrap_or(0)
}

/// 模型菜单显示哪一页。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ModelMenuPage {
    /// 一级：模型列表和“思考等级”入口。
    #[default]
    Models,
    /// 二级：思考等级的选项。
    Effort,
}

/// 模型菜单里模型列表最高这么多（逻辑像素），再多就在里面滚动。
const MODEL_LIST_MAX_HEIGHT: f32 = 320.;

pub(crate) fn effort_label(effort: Option<ReasoningEffort>) -> String {
    match effort {
        None => t!("effort.default"),
        Some(ReasoningEffort::Off) => t!("effort.off"),
        Some(ReasoningEffort::Low) => t!("effort.low"),
        Some(ReasoningEffort::High) => t!("effort.high"),
        Some(ReasoningEffort::Max) => t!("effort.max"),
    }
    .to_string()
}

/// 终端面板显示的实时输出行数。
const LIVE_LINES: usize = 12;
/// 一次界面更新最多合并的事件数。
const EVENT_BATCH: usize = 256;
/// 事件积压时，两批之间让出主线程的时间（约半帧）。
const BACKLOG_PAUSE: Duration = Duration::from_millis(8);
/// 一轮结束后等收尾事件（turn/end 等）的兜底时长：正常情况下收到 turn/end 就结束，
/// 这个时长只兜底异常路径（turn 任务结束了但 turn/end 迟迟不来）。只计一次，不被后续事件推迟。
const TURN_TAIL_WAIT: Duration = Duration::from_millis(250);
/// 单个附件的大小上限。附件整段放进上下文，太大的文件应该让 agent 自己按需读取。
const MAX_ATTACHMENT_BYTES: u64 = 256 * 1024;
/// 图片附件比文本大得多，单独给一个上限（原始字节，不是 base64 后的长度）。
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// 输入卡片上已选的附件（附件气泡里的条目）。文件附件只记路径，发送时才读入；
/// 剪贴板截图没有路径，收到时就把字节记在这里。
#[derive(Clone)]
enum PendingAttachment {
    Path(PathBuf),
    Screenshot {
        name: String,
        media_type: String,
        data: String,
    },
    /// `@` 提及解析出的文字：`path` 是 `@<来源>:<条目>`，写进 `user/attachment`。
    Mention {
        label: String,
        path: String,
        content: String,
    },
}

impl PendingAttachment {
    /// 附件气泡上显示的名字。
    fn label(&self) -> String {
        match self {
            Self::Path(path) => file_label(&path.display().to_string()),
            Self::Screenshot { name, .. } => name.clone(),
            Self::Mention { label, .. } => format!("@{label}"),
        }
    }
}

/// 一条附件：文本（路径 + 内容）或图片（路径 + media type + base64）。
enum ReadAttachment {
    Text(String, String),
    Image { path: String, media_type: String, data: String },
}

/// 读入附件：文本文件只接受不太大的 UTF-8；图片按扩展名识别（png / jpeg / gif / webp），
/// base64 编码后发给能看图的模型。截图直接用粘贴时收到的字节。
/// 发送前在界面线程上检查附件：文件在不在、有没有超过大小上限。只读元数据。
fn check_attachments(paths: &[PendingAttachment]) -> Result<(), String> {
    for attachment in paths {
        let PendingAttachment::Path(path) = attachment else { continue };
        attachment_size_ok(path)?;
    }
    Ok(())
}

/// 附件的大小在上限以内；超了或读不到元数据时是错误原因。
fn attachment_size_ok(path: &std::path::Path) -> Result<(), String> {
    let name = path.display().to_string();
    let limit = if dshr_core::image_media_type(path).is_some() { MAX_IMAGE_BYTES } else { MAX_ATTACHMENT_BYTES };
    let size = std::fs::metadata(path).map_err(|error| t!("conversation.attach_failed", path = name, error = error).to_string())?.len();
    if size > limit {
        // 文本 256 KB，图片 5 MB：按数量级用 KB 或 MB 表示。
        let limit_str = if limit >= 1024 * 1024 { format!("{} MB", limit / 1024 / 1024) } else { format!("{} KB", limit / 1024) };
        return Err(t!("conversation.attach_too_large", path = name, limit = limit_str).to_string());
    }
    Ok(())
}

/// 读入附件内容（图片转成 base64）。要读文件：在后台线程上调用。
fn read_attachments(paths: &[PendingAttachment]) -> Result<Vec<ReadAttachment>, String> {
    paths
        .iter()
        .map(|attachment| {
            let path = match attachment {
                PendingAttachment::Path(path) => path,
                PendingAttachment::Screenshot { name, media_type, data } => {
                    return Ok(ReadAttachment::Image { path: name.clone(), media_type: media_type.clone(), data: data.clone() });
                }
                PendingAttachment::Mention { path, content, .. } => return Ok(ReadAttachment::Text(path.clone(), content.clone())),
            };
            let name = path.display().to_string();
            let is_image = dshr_core::image_media_type(path);
            attachment_size_ok(path)?;
            let bytes = std::fs::read(path).map_err(|error| t!("conversation.attach_failed", path = name, error = error).to_string())?;
            if let Some(media_type) = is_image {
                use base64::Engine as _;
                let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                Ok(ReadAttachment::Image { path: name, media_type: media_type.to_owned(), data })
            } else {
                let text = String::from_utf8(bytes).map_err(|_| t!("conversation.attach_not_text", path = name).to_string())?;
                Ok(ReadAttachment::Text(name, text))
            }
        })
        .collect()
}

/// 读好的附件交给 agent 的样子。
fn user_attachments(read: Vec<ReadAttachment>) -> Vec<dshr_agent::UserAttachment> {
    read.into_iter()
        .map(|attachment| match attachment {
            ReadAttachment::Text(path, content) => dshr_agent::UserAttachment::Text(path, content),
            ReadAttachment::Image { path, media_type, data } => dshr_agent::UserAttachment::Image { path, media_type, data },
        })
        .collect()
}

fn file_label(path: &str) -> String {
    std::path::Path::new(path).file_name().map_or_else(|| path.to_owned(), |name| name.to_string_lossy().into_owned())
}

/// 新会话的权限模式、预设：设置里“新会话默认设置”选的，没选（或认不出）时用内置默认值。
pub(crate) fn default_permission(config: &dshr_config::Config) -> PermissionMode {
    config.model.permission.as_deref().and_then(PermissionMode::from_id).unwrap_or_default()
}

pub(crate) fn default_preset(config: &dshr_config::Config) -> Preset {
    config.model.preset.as_deref().and_then(preset_from_id).unwrap_or_default()
}

/// [`preset_id`] 反过来。
pub(crate) fn preset_from_id(id: &str) -> Option<Preset> {
    Preset::ALL.into_iter().find(|preset| preset_id(*preset) == id)
}

/// 预设写进日志的名字（`session/options` 事件），也是配置里 `model.preset` 的写法。
pub(crate) fn preset_id(preset: Preset) -> &'static str {
    match preset {
        Preset::Standard => "standard",
        Preset::Plan => "plan",
        Preset::Review => "review",
        Preset::Create => "create",
    }
}

/// 把当前输入卡片上的选项变成一条 `session/options` 事件（首轮开始时、或选项变化时写入）。
fn options_event_body(options: &TurnOptions) -> dshr_core::EventBody {
    dshr_core::EventBody::SessionOptions {
        preset: preset_id(options.preset).to_owned(),
        plugin_preset: options.plugin_preset.as_ref().map(|preset| format!("{}/{}", preset.plugin, preset.id)),
        permission: options.permission.id().to_owned(),
        model_provider: options.model.provider.clone(),
        model: options.model.model.clone(),
    }
}

pub(crate) fn preset_label(preset: Preset) -> String {
    match preset {
        Preset::Standard => t!("preset.standard"),
        Preset::Plan => t!("preset.plan"),
        Preset::Review => t!("preset.review"),
        Preset::Create => t!("preset.create"),
    }
    .to_string()
}

fn preset_description(preset: Preset) -> String {
    match preset {
        Preset::Standard => t!("preset.standard_desc"),
        Preset::Plan => t!("preset.plan_desc"),
        Preset::Review => t!("preset.review_desc"),
        Preset::Create => t!("preset.create_desc"),
    }
    .to_string()
}

fn preset_icon(preset: Preset) -> IconName {
    match preset {
        Preset::Standard => IconName::SquareTerminal,
        Preset::Plan => IconName::Map,
        Preset::Review => IconName::Search,
        Preset::Create => IconName::Bot,
    }
}

pub(crate) fn permission_label(mode: PermissionMode) -> String {
    match mode {
        PermissionMode::Manual => t!("permission.manual"),
        PermissionMode::Workspace => t!("permission.workspace"),
        PermissionMode::AutoReview => t!("permission.auto_review"),
        PermissionMode::FullAccess => t!("permission.full_access"),
    }
    .to_string()
}

fn permission_description(mode: PermissionMode) -> String {
    match mode {
        PermissionMode::Manual => t!("permission.manual_desc"),
        PermissionMode::Workspace => t!("permission.workspace_desc"),
        PermissionMode::AutoReview => t!("permission.auto_review_desc"),
        PermissionMode::FullAccess => t!("permission.full_access_desc"),
    }
    .to_string()
}

fn permission_icon(mode: PermissionMode) -> gpui_kit::assets::IconName {
    match mode {
        PermissionMode::Manual => gpui_kit::assets::IconName::Hand,
        PermissionMode::Workspace => IconName::FolderOpen.into(),
        PermissionMode::AutoReview => gpui_kit::assets::IconName::ShieldCheck,
        PermissionMode::FullAccess => IconName::TriangleAlert.into(),
    }
}

/// 用量显示成几个有效数字：`950`、`12.3K`、`1M`、`1.2M`。
fn compact_tokens(tokens: u64) -> String {
    let scaled = |value: f64, unit: &str| {
        let text = if value >= 100. { format!("{value:.0}") } else { format!("{value:.1}") };
        format!("{}{unit}", text.trim_end_matches(".0"))
    };
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => scaled(tokens as f64 / 1_000., "K"),
        _ => scaled(tokens as f64 / 1_000_000., "M"),
    }
}

/// 一轮结束的时刻（Unix 毫秒），按本地时区和界面语言写成“11月12日 17:29”；不是今年时带上年份。
fn footer_time(millis: u64) -> Option<String> {
    use chrono::{Datelike as _, Local, TimeZone as _};
    let time = Local.timestamp_millis_opt(i64::try_from(millis).ok()?).single()?;
    let format = if time.year() == Local::now().year() { t!("conversation.time_same_year") } else { t!("conversation.time_other_year") };
    Some(time.format(&format).to_string())
}

/// 导出文件建议的名字：`对话-<轮次>.md`。
fn share_file_name(turn: u32) -> String {
    format!("{}-{turn}.md", t!("conversation.share_file"))
}

/// 消息列表。单独做成一个视图，在对话视图里缓存（见 `ConversationView::render`）：输入框的光标闪烁、打字，
/// 输入卡片上的悬停和循环动画只让输入卡片（[`ComposerView`]）和对话视图本身重画，视口里的每条消息、
/// 展开的工具输出不再跟着重新生成、重新布局。对话视图每次被通知时通知它（见 `ConversationView::new`）。
/// 行里的动画、悬停通知的是它自己，只重画消息列表。
struct TranscriptView {
    conversation: WeakEntity<ConversationView>,
}

impl Render for TranscriptView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.conversation.update(cx, |this, cx| this.render_transcript_area(window, cx)).unwrap_or_else(|_| div().into_any_element())
    }
}

/// 输入卡片，连同上面的工作区选择、面板、斜杠菜单和排队的消息。单独做成一个视图（不缓存，大小随内容）：
/// 输入框的状态变化和卡片里的动画（如计划面板里的转圈）只通知它，不通知对话视图，所以不会连带消息列表重画。
struct ComposerView {
    conversation: WeakEntity<ConversationView>,
}

impl Render for ComposerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.conversation
            .update(cx, |this, cx| this.render_composer(window, cx).into_any_element())
            .unwrap_or_else(|_| div().into_any_element())
    }
}

impl Render for ConversationView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_plugin_panels(window, cx);
        if let Some(text) = self.restore_input.take() {
            self.set_composer_text(&text, window, cx);
        }
        let composer = self.composer_view.clone();
        // `size_full`：本视图被缓存时按外层给的尺寸独立布局（见 `WorkspaceView::render`），根元素要自己撑满。
        // 不设底色：底色由外面的对话面板画（自绘标题栏时它左上角是圆角，这里再铺一层方的就盖住了）。
        let root = v_flex().flex_1().size_full().min_w(px(0.)).relative();

        // 首页：标题与输入卡片一起，在可用区域中偏上居中。打开已有会话、日志还在加载时不画首页
        // （否则首页先闪一下再换成对话内容），只画空的对话区和底部的输入卡片。
        if self.shows_home() {
            let hero = self.render_hero(cx);
            return root.child(
                v_flex().flex_1().items_center().justify_center().pb(px(space::XXXL * 2.)).child(
                    // 和对话页的输入区一样：最大宽度里含左右内边距，发出第一条消息时输入卡片不变宽窄。
                    v_flex()
                        .w_full()
                        .max_w(px(size::CONTENT_MAX_WIDTH))
                        .px(px(space::XXL))
                        .gap(px(space::XXXL))
                        .child(hero)
                        .child(div().id(COMPOSER_SLOT).w_full().child(composer)),
                ),
            );
        }
        // 读屏软件连着时不缓存（原因同 `WorkspaceView::render`）。
        let transcript = if window.is_a11y_active() {
            ViewElement::new(self.transcript_view.clone()).into_any_element()
        } else {
            self.transcript_view.clone().cached(StyleRefinement::default().flex_1().min_h(px(0.)).w_full()).into_any_element()
        };
        root.child(transcript).child(
            // 和首页包着输入卡片的那一层用同一个 id：发出第一条消息、首页换成对话页时，输入框在元素树里的
            // 路径不变，无障碍节点也就不变。路径一变，讲述人会把输入框当成新获得焦点的控件再读一遍，
            // 正好盖过“已发送，正在回复”。
            div()
                .id(COMPOSER_SLOT)
                .w_full()
                .max_w(px(size::CONTENT_MAX_WIDTH))
                .mx_auto()
                .px(px(space::XXL))
                .pb(px(space::XL))
                .child(composer),
        )
    }
}

/// `/`、`@` 候选列表的外观：和弹出菜单一样（圆角 `LG`、内边距 4、描边、浮层阴影），最多 8 行高，再多就滚动。
fn suggestion_list(id: &'static str, rows: Vec<crate::design::Pressable>, p: &crate::design::Palette) -> AnyElement {
    v_flex()
        .id(id)
        .max_h(px(size::LIST_ITEM * 8. + space::XS * 2. + 2.))
        .overflow_y_scroll()
        .p(px(space::XS))
        .rounded(px(radius::LG))
        .border_1()
        .border_color(p.border)
        .bg(p.raised)
        .shadow(crate::design::shadow::menu(p))
        .children(rows)
        .into_any_element()
}

/// Tab 正要走进、还没画出来的一行（见 `ConversationView::tab_into_row`）。
struct PendingTab {
    row: usize,
    forward: bool,
    /// 系统 Tab 顺序本来要去的地方（列表外面）：后面的行里都没有控件时回到这里。
    outside: Option<FocusHandle>,
    /// 已经试过几行。
    attempts: usize,
}

/// JS 插件加在输入卡片上方的一个面板。
struct PluginPanel {
    /// `<插件>/<id>`。
    key: String,
    page: String,
    label: String,
    view: Entity<crate::plugin_page::PluginPageView>,
}

#[cfg(test)]
mod menu_tests {
    // 不用 `super::*`：那样会把 GPUI 的 `test` 宏一起引进来，盖过标准的 `#[test]`。
    use std::sync::{Arc, Mutex};

    use gpui::TestAppContext;
    use gpui_kit::{AnyWindowHandle, App, AppContext as _, Bounds, Entity, Window, WindowBounds, WindowOptions, point, px, size};

    use super::{ConversationView, ModelMenuPage};
    use crate::Backend;
    use dshr_config::ReasoningEffort;

    /// 后端从 `DSHR_HOME` 读配置：并行的测试改这个环境变量时排队。
    static HOME: Mutex<()> = Mutex::new(());

    struct Fixture {
        window: AnyWindowHandle,
        view: Entity<ConversationView>,
        _home: tempfile::TempDir,
    }

    /// 一个只有一个提供商、`models` 个模型的对话视图。
    fn open(cx: &mut TestAppContext, models: usize) -> Fixture {
        open_with(cx, models, false)
    }

    /// 同 [`open`]；`demo` 时用内置的演示模型（不联网，能真的发出消息）。
    fn open_with(cx: &mut TestAppContext, models: usize, demo: bool) -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let entries: Vec<String> = (0..models).map(|index| format!(r#"{{ id = "m{index}", name = "Model {index}" }}"#)).collect();
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                "[model]\ndefault = \"gw/m0\"\n\n[[providers]]\nid = \"gw\"\nadapter = \"custom\"\nname = \"Gateway\"\nprotocol = \"openai\"\nbase_url = \"https://example.invalid/v1\"\nmodels = [{}]\n",
                entries.join(", ")
            ),
        )
        .unwrap();
        let workspace = home.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let backend = {
            let _guard = HOME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            // SAFETY: 改环境变量和读它（`Backend::new`）都在锁里，别的测试不会同时读写它。
            unsafe { std::env::set_var("DSHR_HOME", home.path()) };
            // SAFETY: 同上，在锁里。
            unsafe {
                if demo {
                    std::env::set_var("DSHR_PROVIDER", "demo");
                } else {
                    std::env::remove_var("DSHR_PROVIDER");
                }
            }
            let paths = dshr_config::Paths::from_env().unwrap();
            let config = dshr_config::Config::load(&paths).unwrap();
            let runtime = Backend::make_runtime().unwrap();
            Arc::new(Backend::from_parts(dshr_context::Context::new(), paths, config, runtime))
        };
        let (window, view) = cx.update(|cx| {
            gpui_kit::init(cx);
            crate::UiRegistry::assemble(&crate::builtin_ui_plugins(), |id| backend.plugin_status(id)).install(cx);
            crate::design::init(cx);
            super::init(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds { origin: point(px(0.), px(0.)), size: size(px(1000.), px(800.)) })),
                ..Default::default()
            };
            let (window, view) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| ConversationView::new(backend.clone(), Some(workspace), window, cx))
            })
            .unwrap();
            (window, view)
        });
        draw(cx, window);
        Fixture { window, view, _home: home }
    }

    fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
        step(cx, window, |_, _| {});
    }

    fn step(cx: &mut TestAppContext, window: AnyWindowHandle, f: impl FnOnce(&mut Window, &mut App)) {
        cx.update_window(window, |_, window, cx| {
            f(window, cx);
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    /// 按下再松开每个键（按上一帧的派发树走），每个键后画两帧：焦点挪到下一页的项上要等那一项画出来。
    /// 要松开：GPUI 在松开 Enter / 空格时才算“点击”了有焦点的按钮。
    fn press(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &'static str) {
        for key in keys.split(' ') {
            cx.simulate_keystrokes(window, key);
            let keystroke = gpui_kit::Keystroke::parse(key).unwrap();
            cx.update_window(window, |_, window, cx| {
                window.dispatch_event(gpui_kit::PlatformInput::KeyUp(gpui_kit::KeyUpEvent { keystroke }), cx);
            })
            .unwrap();
            draw(cx, window);
            draw(cx, window);
        }
    }

    fn open_menu(cx: &mut TestAppContext, fixture: &Fixture) {
        let view = fixture.view.clone();
        step(cx, fixture.window, |_, cx| {
            view.update(cx, |view, cx| {
                view.model_menu_open = true;
                cx.notify();
            })
        });
        draw(cx, fixture.window);
    }

    fn read<R>(cx: &mut TestAppContext, fixture: &Fixture, f: impl FnOnce(&ConversationView) -> R) -> R {
        cx.update(|cx| f(fixture.view.read(cx)))
    }

    #[gpui::test]
    fn arrow_keys_move_through_the_model_menu(cx: &mut TestAppContext) {
        let fixture = open(cx, 3);
        open_menu(cx, &fixture);
        // 打开时焦点在当前模型（m0）上：↓ 到 m1，Enter 选中并关上菜单。
        press(cx, fixture.window, "down enter");
        assert_eq!(read(cx, &fixture, |view| view.options.model.model.clone()), "m1");
        assert!(!read(cx, &fixture, |view| view.model_menu_open));

        // ↓ 从最后一个模型走到“思考等级”，再 ↓ 回到第一个模型；↑ 又回到“思考等级”。
        open_menu(cx, &fixture);
        press(cx, fixture.window, "down down down up enter");
        assert_eq!(read(cx, &fixture, |view| view.model_menu_page), ModelMenuPage::Effort, "Enter 打开二级页");
        assert!(read(cx, &fixture, |view| view.model_menu_open));
    }

    #[gpui::test]
    fn effort_is_a_second_level_page(cx: &mut TestAppContext) {
        let fixture = open(cx, 3);
        open_menu(cx, &fixture);
        // ↑ 从第一个模型绕到最后一项“思考等级”，→ 打开二级页，焦点在当前等级（默认）上。
        press(cx, fixture.window, "up right");
        assert_eq!(read(cx, &fixture, |view| view.model_menu_page), ModelMenuPage::Effort);
        // ↓ 到“关闭”，Enter 选中：回到模型列表，菜单不关。
        press(cx, fixture.window, "down enter");
        assert_eq!(read(cx, &fixture, |view| view.options.effort), Some(ReasoningEffort::Off));
        assert_eq!(read(cx, &fixture, |view| view.model_menu_page), ModelMenuPage::Models);
        assert!(read(cx, &fixture, |view| view.model_menu_open));
        // 焦点回到“思考等级”上：→ 再进去，Esc 回到上一级而不是关菜单，再按一次 Esc 才关上。
        press(cx, fixture.window, "right");
        assert_eq!(read(cx, &fixture, |view| view.model_menu_page), ModelMenuPage::Effort);
        press(cx, fixture.window, "escape");
        assert_eq!(read(cx, &fixture, |view| view.model_menu_page), ModelMenuPage::Models);
        assert!(read(cx, &fixture, |view| view.model_menu_open));
        press(cx, fixture.window, "escape");
        assert!(!read(cx, &fixture, |view| view.model_menu_open));
        // ← 在二级页里也是返回。
        open_menu(cx, &fixture);
        press(cx, fixture.window, "end right left");
        assert_eq!(read(cx, &fixture, |view| view.model_menu_page), ModelMenuPage::Models);
        assert!(read(cx, &fixture, |view| view.model_menu_open));
    }

    #[gpui::test]
    fn arrow_keys_move_through_the_mode_and_permission_menus(cx: &mut TestAppContext) {
        use dshr_agent::{PermissionMode, Preset};
        let fixture = open(cx, 1);
        // 模式菜单：打开时焦点在当前模式上，↓ 到下一个，Enter 选中。
        let current = read(cx, &fixture, |view| view.options.preset);
        let position = Preset::ALL.iter().position(|preset| *preset == current).unwrap();
        let view = fixture.view.clone();
        step(cx, fixture.window, |_, cx| {
            view.update(cx, |view, cx| {
                view.preset_menu_open = true;
                cx.notify();
            })
        });
        press(cx, fixture.window, "down enter");
        assert_eq!(read(cx, &fixture, |view| view.options.preset), Preset::ALL[(position + 1) % Preset::ALL.len()]);
        assert!(!read(cx, &fixture, |view| view.preset_menu_open));

        // 权限菜单：默认是“工作区内修改”（第二项），↑ 到“手动模式”。
        assert_eq!(read(cx, &fixture, |view| view.options.permission), PermissionMode::Workspace);
        let view = fixture.view.clone();
        step(cx, fixture.window, |_, cx| {
            view.update(cx, |view, cx| {
                view.permission_menu_open = true;
                cx.notify();
            })
        });
        press(cx, fixture.window, "up enter");
        assert_eq!(read(cx, &fixture, |view| view.options.permission), PermissionMode::Manual);
    }

    /// 按触摸桥（`touch.rs`）的样子派发一根手指：从 `from` 拖到 `to`，分几步，每步隔一帧。
    fn touch_drag(cx: &mut TestAppContext, window: AnyWindowHandle, from: (f32, f32), to: (f32, f32)) {
        use gpui_kit::{PlatformInput, TouchEvent, TouchId, TouchPhase};
        const STEPS: usize = 12;
        let at = |t: f32| point(px(from.0 + (to.0 - from.0) * t), px(from.1 + (to.1 - from.1) * t));
        let send = |cx: &mut TestAppContext, phase, position| {
            cx.update_window(window, |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::Touch(TouchEvent { id: TouchId(7), phase, position, predicted_position: None, force: None }),
                    cx,
                );
            })
            .unwrap();
            cx.executor().advance_clock(std::time::Duration::from_millis(16));
            draw(cx, window);
        };
        send(cx, TouchPhase::Started, at(0.));
        for step in 1..=STEPS {
            send(cx, TouchPhase::Moved, at(step as f32 / STEPS as f32));
        }
        send(cx, TouchPhase::Ended, at(1.));
        // 抬起后的惯性滚动。
        for _ in 0..60 {
            cx.executor().advance_clock(std::time::Duration::from_millis(16));
            draw(cx, window);
        }
    }

    #[gpui::test]
    fn a_finger_drag_scrolls_the_transcript(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        let view = fixture.view.clone();
        step(cx, fixture.window, |_, cx| {
            view.update(cx, |view, cx| {
                for index in 0..200 {
                    view.transcript.notice(format!("第 {index} 行"));
                }
                view.sync_rows(cx);
                cx.notify();
            })
        });
        draw(cx, fixture.window);
        let top = |cx: &mut TestAppContext| cx.update(|cx| fixture.view.read(cx).list.read(cx).logical_scroll_top().item_ix);
        let before = top(cx);
        assert!(before > 100, "停在最新的内容上：{before}");
        // 手指往下拖：内容跟着往下走，看到更早的行。
        touch_drag(cx, fixture.window, (500., 150.), (500., 550.));
        let after = top(cx);
        // 拖了 400px（一行约 34px），至少回滚 8 行。以前写的是 10 行，正好落在行的边界上，输入区高度一变就时过时不过。
        assert!(after + 8 < before, "应该往回滚了：{before} → {after}");
        assert!(cx.update_window(fixture.window, |_, window, _| window.last_input_was_touch()).unwrap());
    }

    /// 一段长对话：每轮三条提示加一行结束处的按钮（复制、分享、分叉，都是 Tab 停靠点）。停在最上面。
    fn long_transcript(cx: &mut TestAppContext, fixture: &Fixture, turns: u32) {
        let view = fixture.view.clone();
        step(cx, fixture.window, |_, cx| {
            view.update(cx, |view, cx| {
                view.session_id = Some("test".into());
                for turn in 0..turns {
                    for line in 0..3 {
                        view.transcript.notice(format!("第 {turn} 轮第 {line} 行"));
                    }
                    view.transcript.rows.push(super::Row::TurnFooter(super::TurnFooter { turn, start: 0, tokens: 0, time: 0 }));
                }
                view.sync_rows(cx);
                view.list.update(cx, |list, cx| list.scroll_to_item(0, cx));
                cx.notify();
            })
        });
        draw(cx, fixture.window);
    }

    /// 画一帧，但不像 [`step`] 那样先 `refresh`（那会让所有缓存的视图都重画）：和平时一样，只重画被通知过的视图。
    fn draw_frame(cx: &mut TestAppContext, window: AnyWindowHandle) {
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
        cx.run_until_parked();
    }

    #[gpui::test]
    fn keys_bound_to_actions_count_as_keyboard_focus(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        long_transcript(cx, &fixture, 3);
        // 程序自己放的焦点（发出第一条消息后）不画焦点框。
        cx.update_window(fixture.window, |_, window, _| crate::design::set_focus_from_keyboard(false, window)).unwrap();
        assert!(!crate::design::focus_from_keyboard());
        // Tab、方向键派发成动作，不经过元素的按键监听，也要算键盘焦点（焦点框要显示回来）。
        press(cx, fixture.window, "tab");
        assert!(crate::design::focus_from_keyboard());
        // 方向键把焦点从一条消息挪到下一条：算键盘焦点。
        let view = fixture.view.clone();
        let first = cx.update(|cx| view.read(cx).message_rows().next()).unwrap();
        let handle = cx.update(|cx| view.read(cx).message_focus.get(&first).cloned()).unwrap();
        step(cx, fixture.window, |window, cx| handle.focus(window, cx));
        cx.update_window(fixture.window, |_, window, _| crate::design::set_focus_from_keyboard(false, window)).unwrap();
        press(cx, fixture.window, "down");
        let still_first = cx.update_window(fixture.window, |_, window, _| handle.is_focused(window)).unwrap();
        assert!(!still_first, "焦点应该移到了下一条消息");
        assert!(crate::design::focus_from_keyboard());
    }

    #[gpui::test]
    fn typing_does_not_count_as_keyboard_focus(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        // 鼠标点进输入框（焦点不是键盘挪过来的），然后打字：焦点没动，不该冒出焦点框。
        let view = fixture.view.clone();
        step(cx, fixture.window, |window, cx| {
            let input = view.read(cx).input.clone();
            gpui_kit::Focusable::focus_handle(input.read(cx), cx).focus(window, cx);
        });
        cx.update_window(fixture.window, |_, window, _| crate::design::set_focus_from_keyboard(false, window)).unwrap();
        press(cx, fixture.window, "a b");
        assert!(!crate::design::focus_from_keyboard(), "打字不挪焦点，不算键盘焦点");
        press(cx, fixture.window, "tab");
        assert!(crate::design::focus_from_keyboard(), "Tab 总算");
    }

    #[gpui::test]
    fn typing_does_not_rebuild_the_transcript(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        long_transcript(cx, &fixture, 5);
        let view = fixture.view.clone();
        draw(cx, fixture.window);
        draw_frame(cx, fixture.window);
        let before = read(cx, &fixture, |view| view.rows_rendered);
        assert!(before > 0);

        // 在输入框里打字：只重画输入卡片，消息列表沿用上一帧。
        let input = read(cx, &fixture, |view| view.input.clone());
        cx.update_window(fixture.window, |_, window, cx| {
            input.update(cx, |input, cx| input.set_value("你好".to_owned(), window, cx));
        })
        .unwrap();
        draw_frame(cx, fixture.window);
        draw_frame(cx, fixture.window);
        assert_eq!(read(cx, &fixture, |view| view.rows_rendered), before, "打字不该让消息列表重画");
        assert_eq!(cx.update(|cx| view.read(cx).composer_text(cx)), "你好");

        // 记录变了（对话视图被通知）：消息列表照常重画。
        cx.update(|cx| {
            view.update(cx, |view, cx| {
                view.transcript.notice("新的一行");
                view.sync_rows(cx);
                cx.notify();
            })
        });
        draw_frame(cx, fixture.window);
        assert!(read(cx, &fixture, |view| view.rows_rendered) > before, "记录变了要重画消息列表");
    }

    fn focused_row(cx: &mut TestAppContext, fixture: &Fixture) -> Option<usize> {
        let view = fixture.view.clone();
        cx.update_window(fixture.window, |_, window, cx| view.read(cx).focused_row(window, cx)).unwrap()
    }

    /// 按一下键，再多画几帧：Tab 走到还没画出来的行要等它画出来（两帧）。
    fn press_and_settle(cx: &mut TestAppContext, window: AnyWindowHandle, key: &'static str) {
        press(cx, window, key);
        for _ in 0..4 {
            draw(cx, window);
        }
    }

    #[test]
    fn rows_taller_than_the_viewport_are_revealed_only_when_on_screen() {
        use super::row_is_revealed;
        let bounds = |top: f32, height: f32| Bounds { origin: point(px(0.), px(top)), size: size(px(100.), px(height)) };
        let viewport = bounds(0., 500.);
        // 放得下的行：整行在里面才算。
        assert!(row_is_revealed(bounds(100., 200.), viewport, true));
        assert!(!row_is_revealed(bounds(400., 200.), viewport, true));
        // 比视口高、整行都在视口下面：以前算“看得见”，Tab 往下走时列表不动。
        assert!(!row_is_revealed(bounds(600., 900.), viewport, true));
        assert!(!row_is_revealed(bounds(600., 900.), viewport, false));
        // 比视口高、正在读它的中间：不跳回顶上。
        assert!(row_is_revealed(bounds(-300., 900.), viewport, true));
        // 焦点在行里的控件（在行的顶部）上：顶边要露出来。
        assert!(!row_is_revealed(bounds(-300., 900.), viewport, false));
        assert!(row_is_revealed(bounds(100., 900.), viewport, false));
    }

    #[gpui::test]
    fn tab_walks_the_whole_transcript_not_just_the_rows_on_screen(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        long_transcript(cx, &fixture, 60);
        // 焦点放在第一行上，按 Tab 走到第一个“从这里分叉”。
        let view = fixture.view.clone();
        step(cx, fixture.window, |window, cx| {
            view.update(cx, |view, cx| view.row_handle(0, cx).focus(window, cx));
        });
        press_and_settle(cx, fixture.window, "tab");
        assert_eq!(focused_row(cx, &fixture), Some(3));
        // 每一轮结束处有三个按钮（复制、分享、分叉）：在同一行里再走两下。
        let through_footer = |cx: &mut TestAppContext, key: &'static str, row: usize| {
            for _ in 0..2 {
                press_and_settle(cx, fixture.window, key);
                assert_eq!(focused_row(cx, &fixture), Some(row), "同一行里的下一个按钮");
            }
        };
        through_footer(cx, "tab", 3);
        // 一开始只画了最上面几轮；一直按 Tab，每一轮结束处的按钮都走得到，列表跟着往下滚。
        for turn in 1..60 {
            press_and_settle(cx, fixture.window, "tab");
            // 最后一条提示是列表里那条当停靠点的消息：走到最后一轮之前先经过它。
            if focused_row(cx, &fixture) == Some(238) {
                press_and_settle(cx, fixture.window, "tab");
            }
            assert_eq!(focused_row(cx, &fixture), Some(turn * 4 + 3), "第 {turn} 轮");
            through_footer(cx, "tab", turn * 4 + 3);
        }
        let top = cx.update(|cx| fixture.view.read(cx).list.read(cx).logical_scroll_top().item_ix);
        assert!(top > 150, "列表应该滚到了最后几轮：{top}");
        // Shift+Tab 往回也一样：回到最上面那一轮。
        through_footer(cx, "shift-tab", 59 * 4 + 3);
        for turn in (0..59).rev() {
            press_and_settle(cx, fixture.window, "shift-tab");
            let row = focused_row(cx, &fixture);
            // 最后一条提示是列表里那条当停靠点的消息：往回走时先经过它。
            if row == Some(238) {
                press_and_settle(cx, fixture.window, "shift-tab");
            }
            assert_eq!(focused_row(cx, &fixture), Some(turn * 4 + 3), "往回第 {turn} 轮");
            through_footer(cx, "shift-tab", turn * 4 + 3);
        }
        let top = cx.update(|cx| fixture.view.read(cx).list.read(cx).logical_scroll_top().item_ix);
        assert!(top < 10, "列表应该滚回了最上面：{top}");
    }

    /// 比视口还高的一行（长回复）只在视口底边露出一小条时，Tab 走到它上面要把它滚出来。以前把“顶边在
    /// 视口顶边以下、又比视口高”当作看得见，焦点到了它上面列表却不动，看起来焦点丢了。
    #[gpui::test]
    fn tab_scrolls_to_a_row_taller_than_the_viewport(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        long_transcript(cx, &fixture, 4);
        let view = fixture.view.clone();
        step(cx, fixture.window, |_, cx| {
            view.update(cx, |view, cx| {
                let long: Vec<String> = (0..120).map(|line| format!("很长的一条回复，第 {line} 段")).collect();
                view.transcript.notice(long.join("\n\n"));
                view.sync_rows(cx);
                view.list.update(cx, |list, cx| list.scroll_to_item(0, cx));
                cx.notify();
            })
        });
        draw(cx, fixture.window);
        // 往下滚一点：这条长回复的顶边刚好在视口底边露出一小条（不到一行列表项高）。
        let (viewport, footer) = read(cx, &fixture, |view| (view.viewport_bounds.get().unwrap(), view.row_bounds.borrow()[&15]));
        let sliver = px(crate::design::size::LIST_ITEM / 2.);
        let tall_top = footer.bottom() + px(17.);
        let distance = tall_top - (viewport.bottom() - sliver);
        cx.update_window(fixture.window, |_, window, cx| {
            window.dispatch_event(
                gpui_kit::PlatformInput::Scroll(gpui_kit::ScrollEvent {
                    position: point(px(500.), px(300.)),
                    delta: gpui_kit::ScrollDelta::Pixels(point(px(0.), -distance)),
                    ..Default::default()
                }),
                cx,
            );
        })
        .unwrap();
        draw(cx, fixture.window);
        draw(cx, fixture.window);
        let tall = read(cx, &fixture, |view| view.row_bounds.borrow().get(&16).copied()).expect("长回复露出来了，画了它");
        assert!(
            tall.top() < viewport.bottom() && tall.top() > viewport.bottom() - px(crate::design::size::LIST_ITEM),
            "只露出一小条：{tall:?}"
        );
        let last = read(cx, &fixture, |view| view.transcript.rows.len() - 1);
        // 从最后一轮的分叉按钮往后走一下：到那条长回复上（它是列表里当停靠点的消息）。
        step(cx, fixture.window, |window, cx| {
            view.update(cx, |view, cx| view.row_handle(15, cx).focus(window, cx));
        });
        press_and_settle(cx, fixture.window, "tab");
        for _ in 0..3 {
            if focused_row(cx, &fixture) == Some(last) {
                break;
            }
            press_and_settle(cx, fixture.window, "tab");
        }
        assert_eq!(focused_row(cx, &fixture), Some(last));
        let (row, viewport) = read(cx, &fixture, |view| (view.row_bounds.borrow().get(&last).copied(), view.viewport_bounds.get()));
        let (row, viewport) = (row.unwrap(), viewport.unwrap());
        assert!(row.size.height > viewport.size.height, "这一行比视口高：{row:?} {viewport:?}");
        assert!(
            row.top() < viewport.bottom() - px(crate::design::size::LIST_ITEM) && row.bottom() > viewport.top(),
            "焦点所在的行滚进了视口：{row:?} {viewport:?}"
        );
    }

    #[gpui::test]
    fn clicking_anywhere_in_a_reply_focuses_it(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        let view = fixture.view.clone();
        step(cx, fixture.window, |_, cx| {
            view.update(cx, |view, cx| {
                view.transcript.rows.push(super::Row::Assistant {
                    text: "第一段 **加粗** 的正文。\n\n第二段正文，比较长一些，用来点在中间。".into(),
                    thinking: "先想一想。".into(),
                    plain: Default::default(),
                });
                view.sync_rows(cx);
                cx.notify();
            })
        });
        draw(cx, fixture.window);
        draw(cx, fixture.window);
        let bounds = cx.update(|cx| view.read(cx).row_bounds.borrow().get(&0).copied()).unwrap();
        // 点在正文（可选择文字的 Markdown）上，而不是思考那一段。
        let position = gpui_kit::point(bounds.center().x, bounds.bottom() - px(20.));
        cx.update_window(fixture.window, |_, window, cx| {
            window.dispatch_event(
                gpui_kit::PlatformInput::MouseDown(gpui_kit::MouseDownEvent {
                    button: gpui_kit::MouseButton::Left,
                    position,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            window.dispatch_event(
                gpui_kit::PlatformInput::MouseUp(gpui_kit::MouseUpEvent {
                    button: gpui_kit::MouseButton::Left,
                    position,
                    modifiers: Default::default(),
                    click_count: 1,
                }),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();
        draw(cx, fixture.window);
        let focused = cx
            .update_window(fixture.window, |_, window, cx| {
                view.read(cx).message_focus.get(&0).is_some_and(|handle| handle.is_focused(window))
            })
            .unwrap();
        assert!(focused, "点在回复正文上也应该让这条消息获得焦点");
    }

    #[gpui::test]
    fn restore_draft_keeps_existing_text(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        let view = fixture.view.clone();
        step(cx, fixture.window, |window, cx| view.update(cx, |view, cx| view.restore_draft("  草稿 ", window, cx)));
        assert_eq!(cx.update(|cx| view.read(cx).draft(cx)), "  草稿 ", "空输入框放回草稿，原样保留空白");
        step(cx, fixture.window, |window, cx| view.update(cx, |view, cx| view.restore_draft("别的", window, cx)));
        assert_eq!(cx.update(|cx| view.read(cx).draft(cx)), "  草稿 ", "已有字时不覆盖");
    }

    /// 打开已有会话、日志还在后台读时不画首页（以前首页会先出现一下再换成对话内容）。
    #[gpui::test]
    fn loading_a_session_does_not_show_the_home_page(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        let view = fixture.view.clone();
        assert!(cx.update(|cx| view.read(cx).shows_home()), "新会话是首页");
        cx.update(|cx| view.update(cx, |view, _| view.loading = true));
        assert!(!cx.update(|cx| view.read(cx).shows_home()), "载入日志期间不画首页");
    }

    #[gpui::test]
    fn first_message_from_home_gets_focus(cx: &mut TestAppContext) {
        let fixture = open_with(cx, 1, true);
        let view = fixture.view.clone();
        step(cx, fixture.window, |window, cx| {
            view.update(cx, |view, cx| {
                view.set_composer_text("你好", window, cx);
                view.submit(window, cx);
            })
        });
        draw(cx, fixture.window);
        draw(cx, fixture.window);
        let user_row = cx.update(|cx| view.read(cx).transcript.rows.iter().position(|row| matches!(row, super::Row::User { .. }))).unwrap();
        let focused = cx
            .update_window(fixture.window, |_, window, cx| {
                view.read(cx).message_focus.get(&user_row).is_some_and(|handle| handle.is_focused(window))
            })
            .unwrap();
        assert!(focused, "从首页发出第一条消息后，焦点应该在这句话上");
    }

    #[gpui::test]
    fn jump_to_latest_moves_focus_to_the_last_message(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        long_transcript(cx, &fixture, 40);
        let view = fixture.view.clone();
        let scrolled_up = |cx: &mut TestAppContext| cx.update(|cx| view.read(cx).list.read(cx).is_scrolled_up());
        assert!(scrolled_up(cx));
        // 键盘走到“跳到最新消息”上按 Enter：回到底部，焦点落在最后一条消息上，而不是随按钮一起消失。
        let jump = cx.update(|cx| view.read(cx).jump_focus.clone());
        step(cx, fixture.window, |window, cx| jump.focus(window, cx));
        press_and_settle(cx, fixture.window, "enter");
        assert!(!scrolled_up(cx));
        let last = cx.update(|cx| view.read(cx).message_rows().next_back()).unwrap();
        let focused = cx
            .update_window(fixture.window, |_, window, cx| {
                view.read(cx).message_focus.get(&last).is_some_and(|handle| handle.is_focused(window))
            })
            .unwrap();
        assert!(focused, "焦点应该在最后一条消息上");
    }

    #[gpui::test]
    fn long_model_lists_scroll_to_the_focused_model(cx: &mut TestAppContext) {
        let fixture = open(cx, 40);
        open_menu(cx, &fixture);
        let offset = |cx: &mut TestAppContext| read(cx, &fixture, |view| view.model_scroll.offset().y);
        assert_eq!(offset(cx), px(0.));
        // 最后一个模型在列表最下面，看不见：↑ 两次（绕到“思考等级”再往上）走到它，列表跟着滚下去。
        press(cx, fixture.window, "up up");
        assert!(offset(cx) < px(0.), "列表应该滚到最后一个模型：{:?}", offset(cx));
        let max_height = read(cx, &fixture, |view| view.model_scroll.bounds().size.height);
        assert!(max_height <= px(super::MODEL_LIST_MAX_HEIGHT) + px(0.5), "列表高度受限：{max_height:?}");
        // Home 回到第一个模型，列表滚回顶上。
        press(cx, fixture.window, "home");
        assert_eq!(offset(cx), px(0.));
        // Enter 选中的正是第一个模型。
        press(cx, fixture.window, "end up enter");
        assert_eq!(read(cx, &fixture, |view| view.options.model.model.clone()), "m39");
    }

    /// 对话区里加一张等审批的工具卡片（前面一条用户消息，免得停在首页），返回审批的回复通道。
    fn ask_approval(
        cx: &mut TestAppContext,
        fixture: &Fixture,
        call_id: &'static str,
    ) -> tokio::sync::oneshot::Receiver<dshr_agent::Decision> {
        use dshr_core::EventBody;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let view = fixture.view.clone();
        step(cx, fixture.window, |_, cx| {
            view.update(cx, |view, cx| {
                let event = |body| dshr_agent::LiveEvent::Durable(Arc::new(dshr_core::SessionEvent { seq: 0, time: 0, body }));
                view.transcript.apply(&event(EventBody::UserMessage { text: "清理一下".into() }));
                view.transcript.apply(&event(EventBody::ToolCall {
                    call_id: call_id.into(),
                    name: "run_shell".into(),
                    input: serde_json::json!({ "command": "rm -rf build" }),
                }));
                view.transcript.attach_approval(call_id, tx, Some("会删除目录".into()));
                view.sync_rows(cx);
                cx.notify();
            })
        });
        // 第一帧画出审批面板并安排把焦点移过去，第二帧焦点到位。
        draw(cx, fixture.window);
        draw(cx, fixture.window);
        rx
    }

    fn approval_focus(cx: &mut TestAppContext, fixture: &Fixture) -> Option<usize> {
        let view = fixture.view.clone();
        cx.update_window(fixture.window, |_, window, cx| view.read(cx).approval_focus.iter().position(|handle| handle.is_focused(window)))
            .unwrap()
    }

    #[gpui::test]
    fn approval_takes_the_composer_and_answers_from_the_keyboard(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        let mut rx = ask_approval(cx, &fixture, "c1");
        // 出现时焦点自动落在“允许”上。
        assert_eq!(approval_focus(cx, &fixture), Some(0));
        // ↓ 到“允许，不再询问”，↑↓ 循环；Enter 选中。
        press(cx, fixture.window, "down");
        assert_eq!(approval_focus(cx, &fixture), Some(1));
        press(cx, fixture.window, "up up");
        assert_eq!(approval_focus(cx, &fixture), Some(2));
        press(cx, fixture.window, "down down enter");
        assert_eq!(rx.try_recv(), Ok(dshr_agent::Decision::Allow));
        assert!(read(cx, &fixture, |view| view.always_allow.contains("run_shell")));
        assert!(read(cx, &fixture, |view| view.transcript.pending_approval().is_none()));
        // 答完焦点回到输入框。
        let view = fixture.view.clone();
        let input_focused = cx
            .update_window(fixture.window, |_, window, cx| {
                gpui_kit::Focusable::focus_handle(view.read(cx).input.read(cx), cx).is_focused(window)
            })
            .unwrap();
        assert!(input_focused);
    }

    #[gpui::test]
    fn escape_and_number_keys_answer_approvals(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        let mut rx = ask_approval(cx, &fixture, "c1");
        press(cx, fixture.window, "escape");
        assert_eq!(rx.try_recv(), Ok(dshr_agent::Decision::Deny));
        assert!(read(cx, &fixture, |view| view.always_allow.is_empty()));

        let mut rx = ask_approval(cx, &fixture, "c2");
        assert_eq!(approval_focus(cx, &fixture), Some(0), "新的审批又拿到焦点");
        press(cx, fixture.window, "1");
        assert_eq!(rx.try_recv(), Ok(dshr_agent::Decision::Allow));
        assert!(read(cx, &fixture, |view| view.always_allow.is_empty()), "只允许这一次");
    }

    #[gpui::test]
    fn queued_messages_can_jump_the_queue(cx: &mut TestAppContext) {
        let fixture = open(cx, 1);
        let steer = dshr_agent::SteerQueue::new();
        let view = fixture.view.clone();
        step(cx, fixture.window, |_, cx| {
            view.update(cx, |view, cx| {
                // 模拟一轮正在运行。
                view.cancel = Some(tokio_util::sync::CancellationToken::new());
                view.steer = Some(steer.clone());
                view.queue = vec![("第一条".into(), Vec::new()), ("第二条".into(), Vec::new())];
                view.steer_queued(1, cx);
            })
        });
        assert_eq!(read(cx, &fixture, |view| view.queue.len()), 1);
        assert_eq!(read(cx, &fixture, |view| view.steered.len()), 1);
        assert!(steer.contains(0));

        // 取消插队：放回排队最前面。
        step(cx, fixture.window, |_, cx| view.update(cx, |view, cx| view.unsteer(0, cx)));
        assert!(steer.is_empty());
        assert_eq!(read(cx, &fixture, |view| view.queue.iter().map(|(text, _)| text.clone()).collect::<Vec<_>>()), ["第二条", "第一条"]);

        // 再插队一次，agent 取走之后界面上就不再显示。
        step(cx, fixture.window, |_, cx| view.update(cx, |view, cx| view.steer_queued(0, cx)));
        assert_eq!(steer.take_all().len(), 1);
        step(cx, fixture.window, |_, cx| view.update(cx, |view, _| view.prune_steered()));
        assert!(read(cx, &fixture, |view| view.steered.is_empty()));

        // 没赶上这一轮的插队消息：一轮结束时放回排队最前面。
        step(cx, fixture.window, |_, cx| {
            view.update(cx, |view, cx| {
                view.steer_queued(0, cx);
                view.finish_steering();
            })
        });
        assert!(read(cx, &fixture, |view| view.steer.is_none() && view.steered.is_empty()));
        assert_eq!(read(cx, &fixture, |view| view.queue.iter().map(|(text, _)| text.clone()).collect::<Vec<_>>()), ["第一条"]);
    }
}
