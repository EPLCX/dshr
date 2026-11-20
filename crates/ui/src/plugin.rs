//! 界面这一半的插件（详见 `docs/architecture.md` 的“UI 插件”）。
//!
//! GPUI 的 `App`、`Window`、`Entity` 都只能在主线程用，不能和后端的 `dshr_context::Context` 放在一起，
//! 所以一个插件分两半、按 id 对应：后端那一半（`Plugin`）注册工具、钩子；界面这一半（[`UiPlugin`]）
//! 注册这些东西怎么显示。后端那一半没有激活（停用、缺依赖、激活失败）时，界面这一半也不装。
//!
//! 界面里原来按工具名、按页面写死的分支都改成查这里的表：
//!
//! | 扩展点 | 内容 | 现在谁在用 |
//! |---|---|---|
//! | [`ToolCard`] | 某个工具的调用在对话区怎么显示 | `fs`（改动行数与 diff）、`plan`（不显示成卡片） |
//! | [`TranscriptRenderer`] | 插件自定义事件（`EventBody::Ext`）在对话区占的一行 | 还没有内置的 |
//! | [`ComposerPanel`] | 输入卡片上方的面板 | `plan`（计划面板） |
//! | [`ComposerControl`] | 输入卡片底部那一排里的菜单 | `presets`、`permission` |
//! | [`SettingsPage`] | 设置弹窗的一页 | 通用、模型、扩展 |
//! | [`SidebarItem`] | 侧边栏底部的入口 | 还没有内置的 |
//! | [`StatusItem`] | 侧边栏底部的状态行 | 账号状态 |

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use dshr_context::Status;
use gpui_kit::component::IconName;
use gpui_kit::{AnyElement, AnyView, App, ClickEvent, Context, Global, Window};
use serde_json::Value;

use crate::backend::Backend;
use crate::design::Palette;
use crate::settings::SettingsView;
use crate::transcript::{ExtRow, ToolRow};

/// 界面这一半的插件。
pub trait UiPlugin: 'static {
    /// 与后端 `Plugin` 的 id 相同。只有界面、没有后端那一半的插件（如设置页）起一个不重复的名字即可。
    fn id(&self) -> &'static str;

    fn apply_ui(&self, cx: &mut UiScope<'_>);
}

/// 卡片、事件行算好后存下来的数据：事件到达时算一次，之后每帧渲染直接取用。
pub type RowData = Rc<dyn Any>;

/// 工具卡片。没登记的工具用通用卡片：标题里是参数预览，展开后是原始输出。
pub trait ToolCard: 'static {
    /// 这个工具的调用要不要显示成卡片。内容显示在别处的工具（`update_plan`）返回 `false`。
    fn visible(&self) -> bool {
        true
    }

    /// 调用到达时（含从历史重建）：可以更新这个会话的界面状态，如计划面板里的计划。
    fn on_call(&self, _input: &Value, _state: &mut ViewState) {}

    /// 结果到达时把输出解析成卡片要用的数据，存进 [`ToolRow::data`]。
    fn on_result(&self, _output: &str, _is_error: bool) -> Option<RowData> {
        None
    }

    /// 标题右侧、状态文字之前的附加信息（如 `+3 −1`）。
    fn render_summary(&self, _tool: &ToolRow, _p: &Palette) -> Option<AnyElement> {
        None
    }

    /// 展开后的内容，放在可滚动的输出区里；`None` 时显示原始输出。
    fn render_output(&self, _tool: &ToolRow, _cx: &mut App) -> Option<AnyElement> {
        None
    }
}

/// 插件自定义事件在对话区的显示。没登记的事件类型不显示。
pub trait TranscriptRenderer: 'static {
    /// 事件到达时（含从历史重建）算好这一行要显示的东西；返回 `None` 表示这条事件不占一行
    /// （可以只更新界面状态）。日志只追加、不改写，旧版本写下的数据也会走到这里，要兼容。
    fn on_event(&self, data: &Value, state: &mut ViewState) -> Option<RowData>;

    /// 画这一行。`index` 是它在对话区里的行号，元素 id 用它保证不重复。
    fn render(&self, row: &ExtRow, index: usize, cx: &mut App) -> AnyElement;
}

/// 输入卡片上方的面板。
pub trait ComposerPanel: 'static {
    fn id(&self) -> &'static str;

    /// 没有内容可显示时返回 `None`。
    fn render(&self, state: &ViewState, panel: &PanelContext, cx: &mut App) -> Option<AnyElement>;
}

/// 点击的处理函数。
pub type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// 面板的展开状态由对话视图保管。
pub struct PanelContext {
    pub expanded: bool,
    /// 这一轮还在运行。循环动画（转圈）只在运行时播：停下来时还在播的话，每一帧都会让整个对话区重画。
    pub running: bool,
    /// 点击标题时调用：切换展开 / 折叠。
    pub toggle: ClickHandler,
}

/// 输入卡片底部那一排里、由某个插件负责的菜单。菜单本身画在对话视图里（它们要改这一轮的选项），
/// 插件的界面那一半只决定它出不出现：停用 `presets`，预设菜单就不显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComposerControl {
    PresetMenu,
    PermissionMenu,
}

/// 设置弹窗的一页。
#[derive(Clone)]
pub struct SettingsPage {
    pub id: &'static str,
    /// 导航里的图标：组件库的 `IconName`、完整 Lucide 目录的 `IconName`、`design::AppIcon` 都能 `.into()` 过来。
    pub icon: gpui_kit::component::Icon,
    /// 导航里的名字。每次渲染都取，切换语言后跟着变。
    pub label: Rc<dyn Fn() -> String>,
    pub content: PageContent,
}

#[derive(Clone)]
pub enum PageContent {
    /// 设置弹窗自带的页面，状态在 [`SettingsView`] 里。
    Builtin(fn(&SettingsView, &mut Context<SettingsView>) -> AnyElement),
    /// 插件自己的页面：第一次切到这一页时建一个视图，用到弹窗关闭。
    View(PageBuilder),
}

/// 建一个设置页的视图。
pub type PageBuilder = Rc<dyn Fn(&Arc<Backend>, &mut Window, &mut App) -> AnyView>;

/// 侧边栏底部、“设置”上面的入口。
#[derive(Clone)]
pub struct SidebarItem {
    pub id: &'static str,
    pub icon: IconName,
    pub label: Rc<dyn Fn() -> String>,
    pub on_click: SidebarAction,
}

/// 点了侧边栏入口之后做的事。
pub type SidebarAction = Rc<dyn Fn(&mut Window, &mut App)>;

/// 侧边栏底部的一行状态：圆点加文字。
#[derive(Clone)]
pub struct StatusItem {
    pub id: &'static str,
    /// 读当前状态；窗口打开和设置保存后各调一次，不在每帧调用。
    pub read: Rc<dyn Fn(&Backend) -> StatusLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub label: String,
    /// 圆点的颜色：正常 / 需要处理。文字里也要说明状态，不能只靠颜色。
    pub ok: bool,
}

/// 一个会话的界面状态，按类型存取（如计划面板的 `Plan`）。由 [`ToolCard::on_call`]、
/// [`TranscriptRenderer::on_event`] 更新，[`ComposerPanel::render`] 读取；从历史重建时重新算出来。
#[derive(Default)]
pub struct ViewState(HashMap<TypeId, Box<dyn Any>>);

impl ViewState {
    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.0.get(&TypeId::of::<T>())?.downcast_ref()
    }

    pub fn get_or_default<T: Default + 'static>(&mut self) -> &mut T {
        self.0.entry(TypeId::of::<T>()).or_insert_with(|| Box::new(T::default())).downcast_mut().expect("按类型存取")
    }

    /// 放入一项；同一类型已有时替换。
    pub fn insert<T: 'static>(&mut self, value: T) {
        self.0.insert(TypeId::of::<T>(), Box::new(value));
    }
}

impl std::fmt::Debug for ViewState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ViewState({})", self.0.len())
    }
}

/// 插件注册时拿到的句柄。
pub struct UiScope<'a> {
    registry: &'a mut UiRegistry,
}

impl UiScope<'_> {
    /// 登记工具 `tool` 的卡片；同一个工具后登记的生效。
    pub fn tool_card(&mut self, tool: &str, card: impl ToolCard) {
        self.registry.tool_cards.insert(tool.to_owned(), Rc::new(card));
    }

    /// 登记事件类型 `kind`（如 `todo/item`）在对话区的显示。
    pub fn transcript_renderer(&mut self, kind: &str, renderer: impl TranscriptRenderer) {
        self.registry.renderers.insert(kind.to_owned(), Rc::new(renderer));
    }

    pub fn composer_panel(&mut self, panel: impl ComposerPanel) {
        self.registry.panels.push(Rc::new(panel));
    }

    pub fn composer_control(&mut self, control: ComposerControl) {
        if !self.registry.controls.contains(&control) {
            self.registry.controls.push(control);
        }
    }

    pub fn settings_page(&mut self, page: SettingsPage) {
        self.registry.settings_pages.push(page);
    }

    pub fn sidebar_item(&mut self, item: SidebarItem) {
        self.registry.sidebar_items.push(item);
    }

    pub fn status_item(&mut self, item: StatusItem) {
        self.registry.status_items.push(item);
    }
}

/// 界面扩展点的注册表。启动时装配一次，之后只读；视图各自拿一份 `Rc`。
#[derive(Default)]
pub struct UiRegistry {
    /// 装上了的插件，按装配顺序。
    plugins: Vec<&'static str>,
    tool_cards: HashMap<String, Rc<dyn ToolCard>>,
    renderers: HashMap<String, Rc<dyn TranscriptRenderer>>,
    panels: Vec<Rc<dyn ComposerPanel>>,
    controls: Vec<ComposerControl>,
    settings_pages: Vec<SettingsPage>,
    sidebar_items: Vec<SidebarItem>,
    status_items: Vec<StatusItem>,
}

impl std::fmt::Debug for UiRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiRegistry").field("plugins", &self.plugins).finish_non_exhaustive()
    }
}

struct GlobalRegistry(Rc<UiRegistry>);

impl Global for GlobalRegistry {}

impl UiRegistry {
    /// 按顺序装配。`backend_status` 给出后端那一半的状态：`None` 表示这个插件没有后端那一半，
    /// 界面这一半照装；有后端那一半时，只有它处于激活状态才装。
    pub fn assemble(plugins: &[Box<dyn UiPlugin>], backend_status: impl Fn(&str) -> Option<Status>) -> Self {
        let mut registry = Self::default();
        for plugin in plugins {
            let id = plugin.id();
            if registry.plugins.contains(&id) || !matches!(backend_status(id), None | Some(Status::Active)) {
                continue;
            }
            plugin.apply_ui(&mut UiScope { registry: &mut registry });
            registry.plugins.push(id);
        }
        registry
    }

    /// 装进应用，视图通过 [`UiRegistry::global`] 取。
    pub fn install(self, cx: &mut App) {
        cx.set_global(GlobalRegistry(Rc::new(self)));
    }

    /// 应用里装好的注册表；还没装（测试里）时是空的。
    pub fn global(cx: &App) -> Rc<Self> {
        cx.try_global::<GlobalRegistry>().map(|global| global.0.clone()).unwrap_or_default()
    }

    /// 装上了的插件。
    pub fn plugins(&self) -> &[&'static str] {
        &self.plugins
    }

    pub fn tool_card(&self, tool: &str) -> Option<&Rc<dyn ToolCard>> {
        self.tool_cards.get(tool)
    }

    pub fn transcript_renderer(&self, kind: &str) -> Option<&Rc<dyn TranscriptRenderer>> {
        self.renderers.get(kind)
    }

    pub fn composer_panels(&self) -> &[Rc<dyn ComposerPanel>] {
        &self.panels
    }

    pub fn has_control(&self, control: ComposerControl) -> bool {
        self.controls.contains(&control)
    }

    pub fn settings_pages(&self) -> &[SettingsPage] {
        &self.settings_pages
    }

    pub fn sidebar_items(&self) -> &[SidebarItem] {
        &self.sidebar_items
    }

    pub fn status_items(&self) -> &[StatusItem] {
        &self.status_items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Marker(&'static str);

    impl UiPlugin for Marker {
        fn id(&self) -> &'static str {
            self.0
        }

        fn apply_ui(&self, cx: &mut UiScope<'_>) {
            cx.composer_control(ComposerControl::PresetMenu);
            cx.status_item(StatusItem { id: self.0, read: Rc::new(|_| StatusLine { label: String::new(), ok: true }) });
        }
    }

    #[test]
    fn ui_half_follows_the_backend_half() {
        let plugins: Vec<Box<dyn UiPlugin>> = vec![
            Box::new(Marker("active")),
            Box::new(Marker("disabled")),
            Box::new(Marker("pending")),
            Box::new(Marker("failed")),
            Box::new(Marker("ui-only")),
            // 重复的 id 只装第一个。
            Box::new(Marker("active")),
        ];
        let registry = UiRegistry::assemble(&plugins, |id| match id {
            "active" => Some(Status::Active),
            "disabled" => Some(Status::Disabled),
            "pending" => Some(Status::Pending(vec!["svc"])),
            "failed" => Some(Status::Failed("boom".into())),
            _ => None,
        });
        assert_eq!(registry.plugins(), ["active", "ui-only"]);
        assert_eq!(registry.status_items().len(), 2);
        assert!(registry.has_control(ComposerControl::PresetMenu));
        assert!(!registry.has_control(ComposerControl::PermissionMenu));
        assert!(registry.tool_card("anything").is_none());
    }

    #[test]
    fn view_state_is_keyed_by_type() {
        let mut state = ViewState::default();
        assert_eq!(state.get::<u32>(), None);
        *state.get_or_default::<u32>() += 2;
        state.insert("plan".to_owned());
        state.insert("newer".to_owned());
        assert_eq!((state.get::<u32>(), state.get::<String>().map(String::as_str)), (Some(&2), Some("newer")));
    }
}
