//! 插件页面：把插件给出的节点树（`dshr_agent::UiSurface`）用界面自己的组件画出来。
//!
//! 插件代码在后台线程上跑：打开页面、点了按钮之后去要新的树，拿到之前保留旧的那一页，只是按钮暂时不能点。
//! 输入框的状态在这边：节点里的 `value` 只在输入框第一次出现、或者插件换了一个值时写进去，用户正在打的字不会被覆盖。
//!
//! 只展示、不能操作的节点树（插件给工具画的卡片、插件事件在对话区的那一行）用 [`static_tree`] 画。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use serde_json::{Map, Value, json};

use crate::backend::Backend;
use crate::design::{
    ControlSize as _, HoverFill as _, Palette, RowColor as _, Tone, Typography as _, badge, card, divider, hint, mono_font, palette, panel,
    pressable, radius, readable_with_id, space,
};
use crate::dropdown::{DropdownChanged, DropdownState};
use crate::reveal::reveal_in;

pub struct PluginPageView {
    backend: Arc<Backend>,
    plugin: String,
    page: String,
    workspace: Option<PathBuf>,
    tree: Option<Value>,
    /// 最近一次画页面或执行动作的错误。
    error: Option<String>,
    /// 动作返回的一句话。
    message: Option<String>,
    /// 正在等插件（画页面或执行动作）。
    busy: bool,
    /// 页面上各个表单控件的状态：id → 状态。动作的 `inputs` 从这里取。
    controls: HashMap<String, Control>,
    /// 标签页选中的是哪一页：节点的 `id`（没有时是节点路径）→ 标签的 `id`。只在这边记，不回调插件。
    tabs: HashMap<String, String>,
    /// 折叠区（`collapsible`、可折叠的 `card`）展开了没有：键同上。
    open: HashMap<String, bool>,
    /// 点过一次、等第二次确认的链接。
    pending_link: Option<String>,
    /// 带 `confirm` 的按钮点过一次、等第二次确认：按钮的元素 id。
    pending_confirm: Option<String>,
    /// 控件自己的事件触发的动作：输入框的 `submit`，滑块、下拉框、标签输入的 `action`。控件 id → 动作名。
    events: HashMap<String, String>,
    /// 标签输入最多几个标签：控件 id → `max`。
    tag_limits: HashMap<String, usize>,
    /// 日期选择展开时显示的月份：控件 id → `2026-11`。
    months: HashMap<String, String>,
    /// 上次看到的 `host.ui.refresh` 次数。
    refresh_generation: u64,
    /// 页面正文的滚动（滚动容器在弹窗里，见 `WorkspaceView`）：键盘焦点移到看不见的控件时滚过去。
    scroll: ScrollHandle,
}

impl PluginPageView {
    /// 页面正文所在的滚动容器要 `track_scroll` 这个句柄。
    pub fn scroll_handle(&self) -> &ScrollHandle {
        &self.scroll
    }

    pub fn new(
        backend: Arc<Backend>,
        plugin: String,
        page: String,
        workspace: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let refresh_generation = backend.surface(&plugin).map_or(0, |surface| surface.refresh_generation(&page));
        let mut view = Self {
            backend,
            plugin,
            page,
            workspace,
            tree: None,
            error: None,
            message: None,
            busy: false,
            controls: HashMap::new(),
            tabs: HashMap::new(),
            open: HashMap::new(),
            pending_link: None,
            pending_confirm: None,
            events: HashMap::new(),
            tag_limits: HashMap::new(),
            months: HashMap::new(),
            scroll: ScrollHandle::new(),
            refresh_generation,
        };
        view.refresh(window, cx);
        // 工具、钩子改了插件的数据后可以要求重画（`host.ui.refresh(page)`）：有插件要求时才醒来比一次。
        let mut refreshes = dshr_agent::page_refreshes();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                if refreshes.changed().await.is_err() {
                    break;
                }
                let alive = this.update_in(cx, |this, window, cx| {
                    let generation = this.backend.surface(&this.plugin).map_or(0, |surface| surface.refresh_generation(&this.page));
                    if generation != this.refresh_generation {
                        this.refresh_generation = generation;
                        if !this.busy {
                            this.refresh(window, cx);
                        }
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        view
    }

    /// 重新要一次节点树。
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = true;
        let (backend, plugin, page, workspace) = (self.backend.clone(), self.plugin.clone(), self.page.clone(), self.workspace.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let surface = backend.surface(&plugin).ok_or_else(|| t!("plugin_page.gone", plugin = plugin).to_string())?;
                    surface.render(&page, workspace.as_deref())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(tree) => {
                        this.sync_inputs(&tree, window, cx);
                        this.tree = Some(tree);
                        this.error = None;
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// 执行一个动作，然后重画。交给插件的是 `{ value, inputs, checked? }`：节点上写的 `payload`、
    /// 各输入框现在的值，拨开关时还有新的开关状态。
    fn act(&mut self, action: String, value: Value, checked: Option<bool>, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let inputs: Map<String, Value> = self.controls.iter().map(|(id, control)| (id.clone(), control.value(cx))).collect();
        self.pending_confirm = None;
        let mut payload = json!({ "value": value, "inputs": inputs });
        if let Some(checked) = checked {
            payload["checked"] = Value::Bool(checked);
        }
        self.busy = true;
        self.message = None;
        let (backend, plugin, page, workspace) = (self.backend.clone(), self.plugin.clone(), self.page.clone(), self.workspace.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let surface = backend.surface(&plugin).ok_or_else(|| t!("plugin_page.gone", plugin = plugin).to_string())?;
                    surface.act(&page, &action, payload, workspace.as_deref())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(message) => {
                        this.message = message;
                        this.error = None;
                    }
                    Err(error) => this.error = Some(error),
                }
                this.refresh(window, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// 树里每个表单控件都要有一个状态；插件换了 `value` 时写进去（如提交后清空）。树里没有了的丢掉。
    fn sync_inputs(&mut self, tree: &Value, window: &mut Window, cx: &mut Context<Self>) {
        let mut found = Vec::new();
        let mut events = HashMap::new();
        let mut tag_limits = HashMap::new();
        visit_nodes(tree, &mut |node| {
            let Some(id) = node.get("id").and_then(Value::as_str) else { return };
            if let Some(limit) =
                node.get("max").and_then(Value::as_u64).filter(|_| node.get("type").and_then(Value::as_str) == Some("tags-input"))
            {
                tag_limits.insert(id.to_owned(), limit as usize);
            }
            if let Some(seed) = Seed::of(node) {
                found.push((id.to_owned(), seed));
            }
            // 由控件自己的事件（Enter、松开滑块、选下拉项、加删标签）触发的动作。
            let key = if node.get("type").and_then(Value::as_str) == Some("input") { "submit" } else { "action" };
            if let Some(action) = node.get(key).and_then(Value::as_str) {
                events.insert(id.to_owned(), action.to_owned());
            }
        });
        self.events = events;
        self.tag_limits = tag_limits;
        self.controls.retain(|id, control| found.iter().any(|(other, seed)| other == id && control.matches(seed)));
        for (id, seed) in found {
            match (self.controls.get_mut(&id), seed) {
                (Some(Control::Input { state, last, number: limits, .. }), Seed::Input { value, number, .. }) => {
                    if *last != value {
                        state.update(cx, |state, cx| state.set_value(value.clone(), window, cx));
                        *last = value;
                    }
                    *limits = number;
                }
                (Some(Control::Textarea { state, last }), Seed::Textarea { value, .. }) => {
                    if *last != value {
                        state.update(cx, |state, cx| state.set_value(value.clone(), window, cx));
                        *last = value;
                    }
                }
                (Some(Control::Choice { current, last }), Seed::Choice(value)) => {
                    if *last != value {
                        *current = value.clone();
                        *last = value;
                    }
                }
                (Some(Control::Slider { state, last, .. }), Seed::Slider { value, .. }) => {
                    if *last != value {
                        state.update(cx, |state, cx| state.set_value(value as f32, window, cx));
                        *last = value;
                    }
                }
                (Some(Control::Tags { tags, last, .. }), Seed::Tags { value, .. }) => {
                    if *last != value {
                        *tags = value.clone();
                        *last = value;
                    }
                }
                (
                    Some(Control::Dropdown { state, filter, options: current_options, shown, current, last, .. }),
                    Seed::Dropdown { options, value, .. },
                ) => {
                    if *last != value {
                        *current = value.clone();
                        *last = value;
                    }
                    *current_options = options;
                    let text = filter.as_ref().map(|filter| filter.read(cx).value().to_string()).unwrap_or_default();
                    *shown = filter_options(current_options, &text, current);
                    let (labels, selected) = dropdown_items(current_options, shown, current);
                    state.update(cx, |state, cx| state.set_items(labels, selected, cx));
                }
                (_, seed) => {
                    let control = self.new_control(&id, seed, window, cx);
                    self.controls.insert(id, control);
                }
            }
        }
    }

    /// 第一次见到一个控件（或者它的类型、建的时候定下的设置变了）时建它的状态，订阅它自己的事件。
    fn new_control(&mut self, id: &str, seed: Seed, window: &mut Window, cx: &mut Context<Self>) -> Control {
        let id = id.to_owned();
        match seed {
            Seed::Input { value, placeholder, number, masked, max_length } => {
                let state = cx.new(|cx| {
                    let state = InputState::new(window, cx).placeholder(placeholder).default_value(value.clone()).masked(masked);
                    match max_length {
                        Some(limit) => state.validate(move |text, _| text.chars().count() <= limit),
                        None => state,
                    }
                });
                let _events = cx.subscribe_in(&state, window, move |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event
                        && let Some(action) = this.events.get(&id).cloned()
                    {
                        this.act(action, Value::Null, None, window, cx);
                    }
                });
                Control::Input { state, last: value, number, masked, max_length, _events }
            }
            Seed::Textarea { value, placeholder } => {
                let state =
                    cx.new(|cx| TextareaState::new(window, cx).placeholder(placeholder).default_value(value.clone()).auto_grow(3, 12));
                Control::Textarea { state, last: value }
            }
            Seed::Choice(value) => Control::Choice { current: value.clone(), last: value },
            Seed::Slider { value, min, max, step } => {
                let state = cx.new(|_| SliderState::new().min(min as f32).max(max as f32).step(step as f32).default_value(value as f32));
                let _events = cx.subscribe_in(&state, window, move |this, _, event: &SliderEvent, window, cx| match event {
                    // 拖动时只重画（旁边显示的数值跟着变），松手时才交给插件。
                    SliderEvent::Change(_) => cx.notify(),
                    SliderEvent::Release(value) => match this.events.get(&id).cloned() {
                        Some(action) => this.act(action, json!(slider_value(*value)), None, window, cx),
                        None => cx.notify(),
                    },
                });
                Control::Slider { state, last: value, range: (min, max, step), _events }
            }
            Seed::Tags { value, placeholder } => {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                let _events = cx.subscribe_in(&input, window, move |this, input, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        let text = input.read(cx).value().trim().to_owned();
                        input.update(cx, |input, cx| input.set_value("", window, cx));
                        this.add_tag(&id, text, window, cx);
                    }
                });
                Control::Tags { input, tags: value.clone(), last: value, _events }
            }
            Seed::Dropdown { options, value, searchable } => {
                let shown: Vec<usize> = (0..options.len()).collect();
                let (labels, selected) = dropdown_items(&options, &shown, &value);
                let state = cx.new(|cx| DropdownState::new(labels, selected, cx).width(240.));
                let mut events = vec![{
                    let id = id.clone();
                    cx.subscribe_in(&state, window, move |this, _, event: &DropdownChanged, window, cx| {
                        this.pick_dropdown(&id, event.0, window, cx)
                    })
                }];
                let filter = searchable.then(|| {
                    let filter = cx.new(|cx| InputState::new(window, cx).placeholder(t!("plugin_page.filter").to_string()));
                    let id = id.clone();
                    events.push(cx.subscribe_in(&filter, window, move |this, filter, event: &InputEvent, _, cx| {
                        if let InputEvent::Change = event {
                            let text = filter.read(cx).value().to_string();
                            this.filter_dropdown(&id, &text, cx);
                        }
                    }));
                    filter
                });
                Control::Dropdown { state, filter, options, shown, current: value.clone(), last: value, _events: events }
            }
        }
    }

    /// 标签输入框里按了 Enter：加一个标签（去掉空白；空的、重复的、超过 `max` 的不加）。
    fn add_tag(&mut self, id: &str, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let limit = self.tag_limits.get(id).copied().unwrap_or(dshr_agent::MAX_TAGS);
        let Some(Control::Tags { tags, .. }) = self.controls.get_mut(id) else { return };
        if text.is_empty() || tags.contains(&text) || tags.len() >= limit {
            return cx.notify();
        }
        tags.push(text);
        self.tags_changed(id, window, cx);
    }

    fn remove_tag(&mut self, id: &str, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Control::Tags { tags, .. }) = self.controls.get_mut(id) else { return };
        if index < tags.len() {
            tags.remove(index);
        }
        self.tags_changed(id, window, cx);
    }

    /// 标签变了：有 `action` 时交给插件（`value` 是现在的标签），没有时只重画。
    fn tags_changed(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.controls.get(id).map(|control| control.value(cx)).unwrap_or(Value::Null);
        match self.events.get(id).cloned() {
            Some(action) => self.act(action, value, None, window, cx),
            None => cx.notify(),
        }
    }

    /// 在下拉框里选了第几项（筛选后的列表里）。
    fn pick_dropdown(&mut self, id: &str, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Control::Dropdown { options, shown, current, .. }) = self.controls.get_mut(id) else { return };
        let Some(value) = shown.get(index).and_then(|index| options.get(*index)).map(|(value, _)| value.clone()) else { return };
        *current = value.clone();
        match self.events.get(id).cloned() {
            Some(action) => self.act(action, Value::String(value), None, window, cx),
            None => cx.notify(),
        }
    }

    /// 下拉框的筛选框改了：只留下包含这些字的选项。
    fn filter_dropdown(&mut self, id: &str, text: &str, cx: &mut Context<Self>) {
        let Some(Control::Dropdown { state, options, shown, current, .. }) = self.controls.get_mut(id) else { return };
        *shown = filter_options(options, text, current);
        let (labels, selected) = dropdown_items(options, shown, current);
        state.update(cx, |state, cx| state.set_items(labels, selected, cx));
        cx.notify();
    }

    /// 选择类控件（`select`、`radio`、`checkbox`）现在的值。
    fn choice(&self, id: &str) -> Option<&Value> {
        match self.controls.get(id) {
            Some(Control::Choice { current, .. }) => Some(current),
            _ => None,
        }
    }

    /// 改选择类控件的值；有 `action` 时马上交给插件，没有时只是记下来，等别的动作一起提交。
    fn choose(&mut self, id: &str, value: Value, action: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Control::Choice { current, .. }) = self.controls.get_mut(id) {
            *current = value.clone();
        }
        match action {
            Some(action) => {
                let checked = value.as_bool();
                self.act(action, value, checked, window, cx)
            }
            None => cx.notify(),
        }
    }

    /// 画一个节点。能聚焦的控件包一层 [`reveal_in`]：Tab 走到页面下面看不见的地方时，页面跟着滚。
    /// `size` 是外层 `row` 给子控件的默认尺寸。
    fn node(&self, node: &Value, path: &str, size: Option<NodeSize>, cx: &mut Context<Self>) -> AnyElement {
        if node.get("hidden").and_then(Value::as_bool) == Some(true) {
            return div().into_any_element();
        }
        let element = self.node_content(node, path, size, cx);
        let element = match node.get("type").and_then(Value::as_str) {
            Some(
                "button" | "input" | "switch" | "textarea" | "select" | "link" | "calendar" | "checkbox" | "radio" | "number" | "tabs"
                | "collapsible" | "checkboxes" | "slider" | "date" | "tags-input",
            ) => reveal_in(&self.scroll, SharedString::from(format!("{}-{path}-reveal", self.plugin)), element).into_any_element(),
            _ => element,
        };
        with_tooltip(node, SharedString::from(format!("{}-{path}-tip", self.plugin)), element)
    }

    /// 折叠区、标签页在这边记状态用的键：节点的 `id`，没有时是节点路径。
    fn local_key(node: &Value, path: &str) -> String {
        node.get("id").and_then(Value::as_str).map_or_else(|| format!("@{path}"), str::to_owned)
    }

    fn node_content(&self, node: &Value, path: &str, size: Option<NodeSize>, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let id = |suffix: &str| SharedString::from(format!("{}-{path}{suffix}", self.plugin));
        let string = |key: &str| node.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let optional = |key: &str| node.get(key).and_then(Value::as_str).map(str::to_owned);
        // 插件画的文字都要进无障碍树，否则读屏软件看到的是一页空白。id 按节点路径给，保证唯一。
        if let Some(text) = node.as_str() {
            return div().text_body().child(readable_with_id(id(""), text.to_owned())).into_any_element();
        }
        // 插件让这个控件不能用，或者正在等插件。
        let off = self.busy || node.get("disabled").and_then(Value::as_bool) == Some(true);
        let children = |this: &Self, key: &str, inherited: Option<NodeSize>, cx: &mut Context<Self>| -> Vec<AnyElement> {
            node.get(key)
                .and_then(Value::as_array)
                .map(|children| {
                    children
                        .iter()
                        .enumerate()
                        .map(|(index, child)| this.node(child, &format!("{path}.{key}{index}"), inherited, cx))
                        .collect()
                })
                .unwrap_or_default()
        };
        let gap = |default: f32| gap_of(node, default);
        let kind = node.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "column" => layout(v_flex().gap(px(gap(space::M))), node).children(children(self, "children", size, cx)).into_any_element(),
            "row" => {
                let inherited = NodeSize::parse(node.get("size").and_then(Value::as_str)).or(size);
                let row = h_flex().when(node.get("wrap").and_then(Value::as_bool) != Some(false), Div::flex_wrap).items_center();
                layout(row.gap(px(gap(space::S))), node).children(children(self, "children", inherited, cx)).into_any_element()
            }
            "grid" => {
                let columns = node.get("columns").and_then(Value::as_u64).unwrap_or(2).clamp(1, 6) as u16;
                layout(div().w_full().grid().grid_cols(columns).gap(px(gap(space::M))), node)
                    .children(children(self, "children", size, cx))
                    .into_any_element()
            }
            "spacer" => spacer(node).into_any_element(),
            "section" => v_flex()
                .w_full()
                .gap(px(gap(space::M)))
                .child(section_heading(node, &id("-title"), &p))
                .children(children(self, "children", size, cx))
                .into_any_element(),
            "split" => split(node, children(self, "left", size, cx), children(self, "right", size, cx)).into_any_element(),
            "card" | "collapsible" => {
                let key = Self::local_key(node, path);
                let collapsible = kind == "collapsible" || node.get("collapsible").and_then(Value::as_bool) == Some(true);
                // 卡片默认展开，折叠区默认收起；点过之后按这边记的。
                let default_open = node.get("open").and_then(Value::as_bool).unwrap_or(kind == "card");
                let open = !collapsible || self.open.get(&key).copied().unwrap_or(default_open);
                let title = optional("title");
                let heading = section_heading(node, &id("-title"), &p);
                let header: Option<AnyElement> = match (collapsible, title) {
                    (true, title) => {
                        let title = title.unwrap_or_default();
                        Some(
                            pressable(id("-toggle"), &p)
                                .accessibility_label(title)
                                .aria_expanded(open)
                                .disabled(node.get("disabled").and_then(Value::as_bool) == Some(true))
                                .w_full()
                                .rounded(px(radius::MD))
                                .cursor_pointer()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap(px(space::S))
                                        .items_center()
                                        .child(
                                            Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                                                .size(px(crate::design::size::ICON_MD))
                                                .text_color(p.text_secondary),
                                        )
                                        .child(div().flex_1().min_w(px(0.)).child(heading)),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open.insert(key.clone(), !open);
                                    cx.notify();
                                }))
                                .into_any_element(),
                        )
                    }
                    (false, Some(_)) => Some(heading.into_any_element()),
                    (false, None) => None,
                };
                let actions = children(self, "actions", Some(NodeSize::Small), cx);
                let header = match (header, actions.is_empty()) {
                    (None, true) => None,
                    (header, _) => Some(
                        h_flex()
                            .gap(px(space::S))
                            .items_center()
                            .child(div().flex_1().min_w(px(0.)).children(header))
                            .children(actions)
                            .into_any_element(),
                    ),
                };
                let body = v_flex()
                    .gap(px(gap(if kind == "card" { space::M } else { space::S })))
                    .children(header)
                    .when(open, |body| body.children(children(self, "children", size, cx)));
                match kind {
                    "collapsible" => body.into_any_element(),
                    _ => card_frame(node, &p).child(body).into_any_element(),
                }
            }
            "tabs" => {
                let key = Self::local_key(node, path);
                let tabs = node.get("tabs").and_then(Value::as_array).cloned().unwrap_or_default();
                let tab_id = |tab: &Value| tab.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                let current = self
                    .tabs
                    .get(&key)
                    .filter(|current| tabs.iter().any(|tab| tab_id(tab) == **current))
                    .cloned()
                    .or_else(|| optional("value"))
                    .unwrap_or_else(|| tabs.first().map(tab_id).unwrap_or_default());
                let height = NodeSize::of(node, size, NodeSize::Medium).height();
                let action = optional("action");
                let bar: Vec<_> = tabs
                    .iter()
                    .enumerate()
                    .map(|(index, tab)| {
                        let (value, label) = (tab_id(tab), tab.get("label").and_then(Value::as_str).unwrap_or_default().to_owned());
                        let selected = value == current;
                        let (key, action) = (key.clone(), action.clone());
                        pressable(id(&format!("-tab-{index}")), &p)
                            .role(Role::Tab)
                            .accessibility_label(label.clone())
                            .aria_selected(selected)
                            .disabled(off && action.is_some())
                            .h(px(height))
                            .px(px(space::M))
                            .flex()
                            .items_center()
                            .gap(px(space::XS))
                            .cursor_pointer()
                            .border_b_2()
                            .border_color(if selected { p.accent } else { transparent_black() })
                            .when(selected, |tab| tab.text_label())
                            .when(!selected, |tab| tab.text_body().text_color(p.text_secondary))
                            .children(icon_of(tab).map(|icon| Icon::new(icon).size(px(crate::design::size::ICON_SM))))
                            .child(label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.tabs.insert(key.clone(), value.clone());
                                match &action {
                                    Some(action) => this.act(action.clone(), Value::String(value.clone()), None, window, cx),
                                    None => cx.notify(),
                                }
                            }))
                    })
                    .collect();
                let page = tabs.iter().position(|tab| tab_id(tab) == current).unwrap_or(0);
                let content: Vec<AnyElement> = tabs
                    .get(page)
                    .and_then(|tab| tab.get("children"))
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .enumerate()
                            .map(|(index, child)| self.node(child, &format!("{path}.tab{page}.{index}"), size, cx))
                            .collect()
                    })
                    .unwrap_or_default();
                v_flex()
                    .w_full()
                    .gap(px(space::M))
                    .child(
                        h_flex()
                            .id(id("-tablist"))
                            .role(Role::TabList)
                            .border_b_1()
                            .border_color(p.border)
                            .overflow_x_hidden()
                            .children(bar),
                    )
                    .child(v_flex().id(id("-tabpanel")).role(Role::TabPanel).gap(px(gap(space::M))).children(content))
                    .into_any_element()
            }
            "text" => text_node(node, &id(""), true, cx),
            "badge" => badge_node(node, size, &p).into_any_element(),
            "divider" => divider(&p).into_any_element(),
            "stat" => stat_node(node, &id(""), size, cx),
            "alert" | "empty" => {
                let button = optional("action").map(|action| {
                    let value = node.get("payload").cloned().unwrap_or(Value::Null);
                    let button = Button::new(id("-action")).control_sm().label(string("actionLabel")).disabled(off);
                    let button = if kind == "empty" { button } else { button.subtle(cx) };
                    crate::design::framed(
                        id("-action"),
                        button.on_click(cx.listener(move |this, _, window, cx| this.act(action.clone(), value.clone(), None, window, cx))),
                    )
                    .inert(off)
                    .into_any_element()
                });
                let button = button.map(|button| reveal_in(&self.scroll, id("-action-reveal"), button).into_any_element());
                if kind == "alert" { alert_node(node, &id(""), button, &p) } else { empty_node(node, &id(""), button, &p) }
            }
            "button" => {
                let (action, value) = (string("action"), node.get("payload").cloned().unwrap_or(Value::Null));
                let element_id = id("").to_string();
                let label = string("label");
                let confirm = optional("confirm");
                let confirming = confirm.is_some() && self.pending_confirm.as_deref() == Some(element_id.as_str());
                let icon_only = node.get("iconOnly").and_then(Value::as_bool) == Some(true);
                let full = node.get("full").and_then(Value::as_bool) == Some(true);
                let mut button = NodeSize::of(node, size, NodeSize::Small).apply(Button::new(id(""))).disabled(off);
                if let Some(icon) = icon_of(node) {
                    button = button.icon(icon);
                }
                button = match (&confirm, confirming, icon_only) {
                    // 第二次确认时把要确认的话写在按钮上，读屏软件也读这句。
                    (Some(confirm), true, _) => button.label(confirm.clone()).accessibility_label(confirm.clone()),
                    (_, _, true) => button.accessibility_label(label.clone()).tooltip(label.clone()),
                    _ => button.label(label.clone()),
                };
                let button = match (node.get("variant").and_then(Value::as_str), confirming) {
                    (_, true) | (Some("danger"), _) => button.danger(),
                    (Some("primary"), _) => button.primary(),
                    (Some("ghost"), _) => button.subtle(cx),
                    (Some("link"), _) => button.link(),
                    _ => button,
                };
                let button = button.when(full, |button| button.w_full()).on_click(cx.listener(move |this, _, window, cx| {
                    if confirm.is_some() && this.pending_confirm.as_deref() != Some(element_id.as_str()) {
                        this.pending_confirm = Some(element_id.clone());
                        cx.notify();
                    } else {
                        this.act(action.clone(), value.clone(), None, window, cx)
                    }
                }));
                // 外面是一个横向的 flex：在 `column` 里按钮按文字宽度，不被拉满（`full` 时才占满）。
                h_flex().when(full, |wrapper| wrapper.w_full()).child(crate::design::framed(id(""), button).inert(off)).into_any_element()
            }
            "input" | "number" => {
                let control_id = string("id");
                let label = optional("label");
                let Some(Control::Input { state, number, .. }) = self.controls.get(&control_id) else { return div().into_any_element() };
                let control_size = NodeSize::of(node, size, NodeSize::Medium);
                let name = label.clone().unwrap_or_else(|| string("placeholder"));
                let mut field = control_size.apply(Input::new(state)).disabled(off).aria_label(name.clone());
                // 密码框带“显示 / 隐藏”按钮；搜索框前面是放大镜；`prefix` / `suffix` 是框里两头的固定文字（如 https://、.com）。
                match node.get("kind").and_then(Value::as_str) {
                    Some("password") => field = field.mask_toggle(),
                    Some("search") => {
                        field =
                            field.prefix(Icon::new(IconName::Search).size(px(crate::design::size::ICON_SM)).text_color(p.text_secondary))
                    }
                    _ => {}
                }
                if node.get("kind").and_then(Value::as_str) != Some("search")
                    && let Some(prefix) = optional("prefix")
                {
                    field = field.prefix(div().text_color(p.text_secondary).child(prefix));
                }
                if let Some(suffix) = optional("suffix") {
                    field = field.suffix(div().text_color(p.text_secondary).child(suffix));
                }
                let input = crate::design::framed_handle(gpui_kit::Focusable::focus_handle(state.read(cx), cx), field);
                let control: AnyElement = match number {
                    None => input.into_any_element(),
                    Some(_) => {
                        let step = node.get("step").and_then(Value::as_f64).filter(|step| *step > 0.).unwrap_or(1.);
                        let stepper = |suffix: &str, icon: IconName, delta: f64, spoken: String| {
                            let control_id = control_id.clone();
                            crate::design::framed(
                                id(suffix),
                                control_size
                                    .apply(Button::new(id(suffix)))
                                    .subtle(cx)
                                    .icon(icon)
                                    .accessibility_label(spoken)
                                    .disabled(off)
                                    .on_click(cx.listener(move |this, _, window, cx| this.step_number(&control_id, delta, window, cx))),
                            )
                            .inert(off)
                        };
                        h_flex()
                            .gap(px(space::XS))
                            .items_center()
                            .child(stepper("-minus", IconName::Minus, -step, t!("plugin_page.decrease", name = name).to_string()))
                            .child(div().w(px(120.)).child(input))
                            .child(stepper("-plus", IconName::Plus, step, t!("plugin_page.increase", name = name).to_string()))
                            .children(optional("unit").map(|unit| control_size.text(div().text_color(p.text_secondary)).child(unit)))
                            .into_any_element()
                    }
                };
                v_flex()
                    .gap(px(space::XS))
                    .when(kind == "input", |field| field.min_w(px(200.)))
                    .children(label.map(|label| control_size.text(div()).child(readable_with_id(id("-label"), label))))
                    .child(control)
                    .into_any_element()
            }
            "switch" => {
                let action = string("action");
                let value = node.get("payload").cloned().unwrap_or(Value::Null);
                let checked = node.get("checked").and_then(Value::as_bool).unwrap_or(false);
                let label = string("label");
                let description = optional("description");
                let text_size = NodeSize::of(node, size, NodeSize::Medium);
                // 用应用自己的开关（和设置页一样）：组件库的开关没有高可见性轮廓的外圈。
                // 名字给开关本身，读屏软件读“<名字>，开关，开 / 关”；旁边的文字只给眼睛看。
                let switch = crate::design::switch(id(""), checked, &p)
                    .accessibility_label(label.clone())
                    .when_some(description.clone(), |switch, description| switch.aria_description(description))
                    .disabled(off)
                    .on_click(cx.listener(move |this, _, window, cx| this.act(action.clone(), value.clone(), Some(!checked), window, cx)));
                let text = v_flex()
                    .when(off, |text| text.opacity(0.5))
                    .child(text_size.text(div()).child(label))
                    .children(description.map(|description| div().text_caption().text_color(p.text_secondary).child(description)));
                match node.get("layout").and_then(Value::as_str) {
                    // 设置行的样子：文字在左、开关在右，占满一行。
                    Some("row") => h_flex()
                        .w_full()
                        .gap(px(space::L))
                        .items_center()
                        .child(text.flex_1().min_w(px(0.)))
                        .child(switch)
                        .into_any_element(),
                    _ => h_flex().gap(px(space::S)).items_center().child(switch).child(text).into_any_element(),
                }
            }
            "checkbox" => {
                let control_id = string("id");
                let checked = self.choice(&control_id).and_then(Value::as_bool).unwrap_or(false);
                let action = optional("action");
                let control_size = NodeSize::of(node, size, NodeSize::Medium);
                check_item(id(""), checked, string("label"), optional("description"), control_size, off, &p)
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.choose(&control_id, Value::Bool(!checked), action.clone(), window, cx)),
                    )
                    .into_any_element()
            }
            "checkboxes" => {
                let control_id = string("id");
                let current: Vec<String> = self
                    .choice(&control_id)
                    .and_then(Value::as_array)
                    .map(|values| values.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                    .unwrap_or_default();
                let action = optional("action");
                let label = optional("label");
                let control_size = NodeSize::of(node, size, NodeSize::Medium);
                let options = options_of(node);
                let items: Vec<AnyElement> = options
                    .iter()
                    .enumerate()
                    .map(|(index, (value, text))| {
                        let checked = current.contains(value);
                        // 勾选后的值按选项的顺序排，不按点的先后。
                        let next: Vec<String> = options
                            .iter()
                            .filter(|(other, _)| if other == value { !checked } else { current.contains(other) })
                            .map(|(other, _)| other.clone())
                            .collect();
                        let (control_id, action) = (control_id.clone(), action.clone());
                        check_item(id(&format!("-option-{index}")), checked, text.clone(), None, control_size, off, &p)
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.choose(&control_id, json!(next), action.clone(), window, cx)),
                            )
                            .into_any_element()
                    })
                    .collect();
                let group = match node.get("layout").and_then(Value::as_str) {
                    Some("horizontal") => h_flex().flex_wrap().gap(px(space::M)),
                    _ => v_flex().gap(px(space::XS)),
                };
                v_flex()
                    .gap(px(space::XS))
                    .children(label.clone().map(|label| control_size.text(div()).child(readable_with_id(id("-label"), label))))
                    .child(
                        group.id(id("-group")).role(Role::Group).when_some(label, |group, label| group.aria_label(label)).children(items),
                    )
                    .into_any_element()
            }
            "slider" => {
                let label = optional("label");
                let Some(Control::Slider { state, .. }) = self.controls.get(&string("id")) else { return div().into_any_element() };
                let value = slider_value(state.read(cx).value());
                let shown = match optional("unit") {
                    Some(unit) => format!("{} {unit}", format_number(value)),
                    None => format_number(value),
                };
                let show_value = node.get("showValue").and_then(Value::as_bool) != Some(false);
                v_flex()
                    .gap(px(space::XS))
                    .w_full()
                    .min_w(px(200.))
                    .child(
                        h_flex()
                            .justify_between()
                            .children(label.clone().map(|label| div().text_body().child(readable_with_id(id("-label"), label))))
                            .when(show_value, |row| row.child(div().text_caption().text_color(p.text_secondary).child(shown))),
                    )
                    .child(Slider::new(state).disabled(off).accessibility_label(label.unwrap_or_default()))
                    .into_any_element()
            }
            "date" => {
                let control_id = string("id");
                let value = self.choice(&control_id).and_then(Value::as_str).unwrap_or_default().to_owned();
                let label = optional("label");
                let action = optional("action");
                let control_size = NodeSize::of(node, size, NodeSize::Medium);
                let open_key = format!("date:{control_id}");
                let open = self.open.get(&open_key).copied().unwrap_or(false);
                let shown = if value.is_empty() {
                    optional("placeholder").unwrap_or_else(|| t!("plugin_page.pick_date").to_string())
                } else {
                    value.clone()
                };
                let spoken = [label.clone().unwrap_or_default(), shown.clone()]
                    .into_iter()
                    .filter(|text| !text.is_empty())
                    .collect::<Vec<_>>()
                    .join("，");
                let toggle_key = open_key.clone();
                let trigger = crate::design::framed(
                    id("-trigger"),
                    control_size
                        .apply(Button::new(id("-trigger")))
                        .icon(IconName::Calendar)
                        .label(shown)
                        .accessibility_label(spoken)
                        .disabled(off)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open.insert(toggle_key.clone(), !open);
                            cx.notify();
                        })),
                )
                .inert(off);
                let picker = open.then(|| {
                    let month = self.months.get(&control_id).cloned().unwrap_or_else(|| {
                        if value.len() >= 7 { value[..7].to_owned() } else { chrono::Local::now().format("%Y-%m").to_string() }
                    });
                    let nav = |suffix: &str, icon: IconName, delta: i32, spoken: String| {
                        let (control_id, month) = (control_id.clone(), month.clone());
                        crate::design::framed(
                            id(suffix),
                            Button::new(id(suffix)).control_sm().subtle(cx).icon(icon).accessibility_label(spoken).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.months.insert(control_id.clone(), shift_month(&month, delta));
                                    cx.notify();
                                },
                            )),
                        )
                    };
                    let view = cx.entity().downgrade();
                    let (pick_id, pick_key) = (control_id.clone(), open_key.clone());
                    let pick: PickDate = std::rc::Rc::new(move |date, window, cx| {
                        let (control_id, action, key) = (pick_id.clone(), action.clone(), pick_key.clone());
                        view.update(cx, |this, cx| {
                            this.open.insert(key, false);
                            this.choose(&control_id, Value::String(date), action, window, cx)
                        })
                        .ok();
                    });
                    let calendar_node = json!({ "month": month, "marks": { value.clone(): "" } });
                    v_flex().w(px(320.)).gap(px(space::XS)).p(px(space::S)).child(
                        card(&p).p(px(space::S)).child(
                            v_flex()
                                .gap(px(space::XS))
                                .child(
                                    h_flex()
                                        .justify_end()
                                        .gap(px(space::XS))
                                        .child(nav("-previous", IconName::ChevronLeft, -1, t!("plugin_page.previous_month").to_string()))
                                        .child(nav("-next", IconName::ChevronRight, 1, t!("plugin_page.next_month").to_string())),
                                )
                                .child(calendar(&calendar_node, &id("-calendar"), Some(pick), cx)),
                        ),
                    )
                });
                v_flex()
                    .gap(px(space::XS))
                    .items_start()
                    .children(label.map(|label| control_size.text(div()).child(readable_with_id(id("-label"), label))))
                    .child(trigger)
                    .children(picker)
                    .into_any_element()
            }
            "tags-input" => {
                let control_id = string("id");
                let label = optional("label");
                let Some(Control::Tags { input, tags, .. }) = self.controls.get(&control_id) else { return div().into_any_element() };
                let control_size = NodeSize::of(node, size, NodeSize::Medium);
                let chips: Vec<AnyElement> = tags
                    .iter()
                    .enumerate()
                    .map(|(index, tag)| {
                        let control_id = control_id.clone();
                        let remove = id(&format!("-remove-{index}"));
                        h_flex()
                            .gap(px(2.))
                            .items_center()
                            .pl(px(space::S))
                            .rounded_full()
                            .bg(p.accent_soft)
                            .text_color(p.accent)
                            .child(control_size.text(div()).child(readable_with_id(id(&format!("-tag-{index}")), tag.clone())))
                            .child(
                                crate::design::framed(
                                    remove.clone(),
                                    Button::new(remove)
                                        .control_xs()
                                        .subtle(cx)
                                        .icon(IconName::Close)
                                        .accessibility_label(t!("plugin_page.remove_tag", tag = tag).to_string())
                                        .disabled(off)
                                        .on_click(cx.listener(move |this, _, window, cx| this.remove_tag(&control_id, index, window, cx))),
                                )
                                .inert(off),
                            )
                            .into_any_element()
                    })
                    .collect();
                let name = label.clone().unwrap_or_else(|| string("placeholder"));
                v_flex()
                    .gap(px(space::XS))
                    .min_w(px(200.))
                    .children(label.map(|label| control_size.text(div()).child(readable_with_id(id("-label"), label))))
                    .when(!chips.is_empty(), |field| field.child(h_flex().flex_wrap().gap(px(space::XS)).children(chips)))
                    .child(crate::design::framed_handle(
                        gpui_kit::Focusable::focus_handle(input.read(cx), cx),
                        control_size.apply(Input::new(input)).disabled(off).aria_label(name),
                    ))
                    .into_any_element()
            }
            "select" if node.get("style").and_then(Value::as_str) == Some("dropdown") => {
                let label = optional("label");
                let Some(Control::Dropdown { state, filter, .. }) = self.controls.get(&string("id")) else {
                    return div().into_any_element();
                };
                let (state, filter) = (state.clone(), filter.clone());
                let name = label.clone().unwrap_or_else(|| string("placeholder"));
                state.update(cx, |state, cx| state.set_label(name.clone(), cx));
                v_flex()
                    .gap(px(space::XS))
                    .items_start()
                    .children(label.map(|label| div().text_body().child(readable_with_id(id("-label"), label))))
                    .children(filter.map(|filter| {
                        div().w(px(240.)).child(crate::design::framed_handle(
                            gpui_kit::Focusable::focus_handle(filter.read(cx), cx),
                            Input::new(&filter).small().aria_label(t!("plugin_page.filter_named", name = name).to_string()),
                        ))
                    }))
                    .child(state)
                    .into_any_element()
            }
            "radio" | "select" => {
                let control_id = string("id");
                let current = self.choice(&control_id).and_then(Value::as_str).unwrap_or_default().to_owned();
                let action = optional("action");
                let label = optional("label");
                let options = options_of(node);
                // `select` 的 `style: "radio"` 和单选的默认样式一样，是一列圆点。
                let style = match node.get("style").and_then(Value::as_str).unwrap_or(if kind == "select" { "buttons" } else { "list" }) {
                    "radio" => "list",
                    style => style,
                };
                // 一排按钮默认是小尺寸（和原来一样），分段控件和圆点默认是中等。
                let control_size = NodeSize::of(node, size, if style == "buttons" { NodeSize::Small } else { NodeSize::Medium });
                let items: Vec<AnyElement> = options
                    .into_iter()
                    .enumerate()
                    .map(|(index, (value, text))| {
                        let selected = value == current;
                        let (control_id, action) = (control_id.clone(), action.clone());
                        let item_id = id(&format!("-option-{index}"));
                        let pick = cx.listener(move |this, _, window, cx| {
                            this.choose(&control_id, Value::String(value.clone()), action.clone(), window, cx)
                        });
                        match style {
                            // 选择框：选项不多时一排按钮最直接，读屏软件也能逐个读出；选中的那个是主按钮。
                            "buttons" => {
                                let button = control_size.apply(Button::new(item_id.clone())).label(text).disabled(off);
                                let button = if selected { button.primary() } else { button };
                                crate::design::framed(item_id, button.on_click(pick)).inert(off).into_any_element()
                            }
                            // 分段控件：下沉底色里一排，选中的那段是表面色底。
                            "segmented" => pressable(item_id, &p)
                                .role(Role::RadioButton)
                                .aria_toggled(if selected { Toggled::True } else { Toggled::False })
                                .accessibility_label(text.clone())
                                .disabled(off)
                                .h(px(control_size.height() - space::XS * 2.))
                                .px(px(space::M))
                                .flex()
                                .items_center()
                                .rounded(px(radius::SM))
                                .cursor_pointer()
                                .when(selected, |item| control_size.text(item.bg(p.surface).shadow_xs()).font_weight(FontWeight::MEDIUM))
                                .when(!selected, |item| control_size.text(item).text_color(p.text_secondary))
                                .child(text)
                                .on_click(pick)
                                .into_any_element(),
                            _ => {
                                let indicator = control_size.indicator();
                                pressable(item_id, &p)
                                    .role(Role::RadioButton)
                                    .aria_toggled(if selected { Toggled::True } else { Toggled::False })
                                    .accessibility_label(text.clone())
                                    .disabled(off)
                                    .self_start()
                                    .rounded(px(radius::SM))
                                    .cursor_pointer()
                                    .when(off, |item| item.opacity(0.5))
                                    .child(
                                        h_flex()
                                            .gap(px(space::S))
                                            .items_center()
                                            .child(
                                                div()
                                                    .size(px(indicator))
                                                    .flex_none()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded_full()
                                                    .border_1()
                                                    .border_color(if selected { p.accent } else { p.text_secondary })
                                                    .when(selected, |dot| {
                                                        dot.child(div().size(px(indicator / 2.)).rounded_full().bg(p.accent))
                                                    }),
                                            )
                                            .child(control_size.text(div()).child(text)),
                                    )
                                    .on_click(pick)
                                    .into_any_element()
                            }
                        }
                    })
                    .collect();
                let group = match style {
                    "segmented" => h_flex().p(px(space::XS)).gap(px(2.)).bg(p.sunken).rounded(px(radius::MD)),
                    "buttons" => h_flex().flex_wrap().gap(px(space::XS)),
                    _ => v_flex().gap(px(space::XS)),
                };
                v_flex()
                    .gap(px(space::XS))
                    .when(style == "segmented", |field| field.items_start())
                    .children(label.clone().map(|label| control_size.text(div()).child(readable_with_id(id("-label"), label))))
                    .child(
                        group
                            .id(id("-group"))
                            .when(kind == "radio", |group| group.role(Role::RadioGroup))
                            .when_some(label, |group, label| group.aria_label(label))
                            .children(items),
                    )
                    .into_any_element()
            }
            "list" => {
                let items = node.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
                if items.is_empty() {
                    return hint(optional("empty").unwrap_or_default(), Tone::Neutral, &p).into_any_element();
                }
                let list_size = NodeSize::of(node, size, NodeSize::Medium);
                let dividers = node.get("dividers").and_then(Value::as_bool) == Some(true);
                let mut rows = Vec::new();
                for (index, item) in items.into_iter().enumerate() {
                    if dividers && index > 0 {
                        rows.push(divider(&p).into_any_element());
                    }
                    let title = item.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
                    let detail = item.get("detail").and_then(Value::as_str).map(str::to_owned);
                    let action = item.get("action").and_then(Value::as_str).map(str::to_owned);
                    let value = item.get("payload").cloned().unwrap_or(Value::Null);
                    let item_id = id(&format!("-item-{index}"));
                    let content = list_row(&item, list_size, &p);
                    rows.push(match action {
                        // 能点的行是按钮：Tab 能走到，Enter / 空格触发；读屏软件读标题，详情作补充说明。
                        Some(action) if !off => reveal_in(
                            &self.scroll,
                            SharedString::from(format!("{item_id}-reveal")),
                            pressable(item_id, &p)
                                .accessibility_label(title.clone())
                                .when_some(detail.clone(), |row, detail| row.aria_description(detail))
                                .relative()
                                .flex()
                                .items_center()
                                .rounded(px(radius::MD))
                                .cursor_pointer()
                                // 悬停、按下底色铺满整行，带圆角。
                                .hover_fill(&p, radius::MD, false)
                                .on_click(cx.listener(move |this, _, window, cx| this.act(action.clone(), value.clone(), None, window, cx)))
                                .child(content.flex_1().min_w(px(0.))),
                        )
                        .into_any_element(),
                        // 不能点的行：文字直接进无障碍树。
                        _ => {
                            let text = match &detail {
                                Some(detail) => format!("{title}，{detail}"),
                                None => title.clone(),
                            };
                            div().id(item_id).role(Role::Label).aria_value(text).child(content).into_any_element()
                        }
                    });
                }
                v_flex().gap(px(if dividers { 0. } else { 2. })).children(rows).into_any_element()
            }
            "table" => {
                let on_row: Option<PickRow> = optional("rowAction").filter(|_| !off).map(|action| {
                    let view = cx.entity().downgrade();
                    std::rc::Rc::new(move |index: usize, row: Value, window: &mut Window, cx: &mut App| {
                        let action = action.clone();
                        view.update(cx, |this, cx| this.act(action, json!({ "index": index, "row": row }), None, window, cx)).ok();
                    }) as PickRow
                });
                table(node, &id(""), size, on_row, cx)
            }
            "textarea" => {
                let label = optional("label");
                let Some(Control::Textarea { state, .. }) = self.controls.get(&string("id")) else { return div().into_any_element() };
                let control_size = NodeSize::of(node, size, NodeSize::Medium);
                v_flex()
                    .gap(px(space::XS))
                    .w_full()
                    .children(label.clone().map(|label| control_size.text(div()).child(readable_with_id(id("-label"), label))))
                    .child(crate::design::framed_handle(
                        gpui_kit::Focusable::focus_handle(state.read(cx), cx),
                        control_size.apply(Textarea::new(state)).disabled(off).aria_label(label.unwrap_or_else(|| string("placeholder"))),
                    ))
                    .into_any_element()
            }
            // 链接：点一次先显示要打开的地址，再点一次才用系统浏览器打开。
            "link" => {
                let url = string("url");
                let confirming = self.pending_link.as_deref() == Some(url.as_str());
                let label = if confirming { t!("plugin_page.open_link_confirm", url = url).to_string() } else { string("label") };
                crate::design::framed(
                    id(""),
                    NodeSize::of(node, size, NodeSize::Small)
                        .apply(Button::new(id("")))
                        .subtle(cx)
                        .icon(icon_of(node).unwrap_or(IconName::ExternalLink))
                        .label(label)
                        .disabled(off)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.pending_link.as_deref() == Some(url.as_str()) {
                                this.pending_link = None;
                                cx.open_url(&url);
                            } else {
                                this.pending_link = Some(url.clone());
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                )
                .into_any_element()
            }
            "calendar" if node.get("action").and_then(Value::as_str).is_some() => {
                let action = string("action");
                let view = cx.entity().downgrade();
                let pick: PickDate = std::rc::Rc::new(move |date, window, cx| {
                    let action = action.clone();
                    view.update(cx, |this, cx| this.act(action, Value::String(date), None, window, cx)).ok();
                });
                calendar(node, &id(""), Some(pick), cx)
            }
            kind => match display_node(node, &id(""), cx) {
                Some(element) => element,
                // 画之前已经检查过；这里只防万一。
                None => hint(t!("plugin_page.unknown_node", kind = kind).to_string(), Tone::Danger, &p).into_any_element(),
            },
        }
    }

    /// 数字框旁的加减按钮：按 `step` 加减，不超出 `min` / `max`。
    fn step_number(&mut self, id: &str, delta: f64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Control::Input { state, number: Some((min, max)), .. }) = self.controls.get(id) else { return };
        let (state, min, max) = (state.clone(), *min, *max);
        let current = state.read(cx).value().trim().parse::<f64>().ok().filter(|value| value.is_finite());
        let next = clamp(current.unwrap_or(min.unwrap_or(0.)) + if current.is_some() { delta } else { 0. }, min, max);
        // 去掉 0.1 + 0.2 这类浮点误差。
        let next = (next * 1e9).round() / 1e9;
        state.update(cx, |state, cx| state.set_value(format_number(next), window, cx));
    }
}

/// 一个复选框：方框 + 文字（+ 说明）。`checkbox` 和 `checkboxes` 的每一项都用它；调用方再加 `on_click`。
fn check_item(
    item_id: SharedString,
    checked: bool,
    label: String,
    description: Option<String>,
    size: NodeSize,
    off: bool,
    p: &Palette,
) -> crate::design::Pressable {
    let indicator = size.indicator();
    let mark = div()
        .size(px(indicator))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .when(checked, |mark| {
            mark.bg(p.accent).border_color(p.accent).child(Icon::new(IconName::Check).size(px(indicator - 4.)).text_color(p.on_accent))
        })
        .when(!checked, |mark| mark.border_color(p.text_secondary));
    pressable(item_id, p)
        .role(Role::CheckBox)
        .aria_toggled(if checked { Toggled::True } else { Toggled::False })
        .accessibility_label(label.clone())
        .when_some(description.clone(), |row, description| row.aria_description(description))
        .disabled(off)
        .self_start()
        .rounded(px(radius::SM))
        .cursor_pointer()
        .when(off, |row| row.opacity(0.5))
        .child(
            h_flex().gap(px(space::S)).items_start().child(div().h(px(indicator + 4.)).flex().items_center().child(mark)).child(
                v_flex()
                    .child(size.text(div()).child(label))
                    .children(description.map(|text| div().text_caption().text_color(p.text_secondary).child(text))),
            ),
        )
}

/// 两栏：左栏占 `ratio`（默认一半）。每栏至少 240 宽，放不下时右栏换到下面。
fn split(node: &Value, left: Vec<AnyElement>, right: Vec<AnyElement>) -> Div {
    let ratio = node.get("ratio").and_then(Value::as_f64).unwrap_or(0.5).clamp(0.2, 0.8) as f32;
    let column = |share: f32| v_flex().flex_basis(relative(share)).flex_grow(share).min_w(px(240.)).gap(px(gap_of(node, space::M)));
    h_flex()
        .w_full()
        .flex_wrap()
        .items_start()
        .child(column(ratio).pr(px(space::L)).children(left))
        .child(column(1. - ratio).children(right))
}

/// `2026-11` 往前（负数）或往后数几个月。
fn shift_month(month: &str, delta: i32) -> String {
    let year = month.get(..4).and_then(|year| year.parse::<i32>().ok()).unwrap_or(2026);
    let number = month.get(5..7).and_then(|number| number.parse::<i32>().ok()).unwrap_or(1);
    let total = year * 12 + (number - 1) + delta;
    format!("{:04}-{:02}", total.div_euclid(12), total.rem_euclid(12) + 1)
}

/// 节点有 `tooltip` 时包一层：鼠标停上去显示这句话。
fn with_tooltip(node: &Value, id: SharedString, element: AnyElement) -> AnyElement {
    match node.get("tooltip").and_then(Value::as_str) {
        Some(tip) => {
            let tip = tip.to_owned();
            div()
                .id(id)
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                .child(element)
                .into_any_element()
        }
        None => element,
    }
}

/// `gap`：small / medium / large，没写时用各自的默认值。
fn gap_of(node: &Value, default: f32) -> f32 {
    match node.get("gap").and_then(Value::as_str) {
        Some("small") => space::XS,
        Some("medium") => space::S,
        Some("large") => space::L,
        _ => default,
    }
}

/// `column` / `row` / `grid` 的对齐（`align`、`justify`）和内边距（`padding`）。
fn layout(element: Div, node: &Value) -> Div {
    let element = match node.get("align").and_then(Value::as_str) {
        Some("start") => element.items_start(),
        Some("center") => element.items_center(),
        Some("end") => element.items_end(),
        Some("stretch") => element.items_stretch(),
        _ => element,
    };
    let element = match node.get("justify").and_then(Value::as_str) {
        Some("start") => element.justify_start(),
        Some("center") => element.justify_center(),
        Some("end") => element.justify_end(),
        Some("between") => element.justify_between(),
        _ => element,
    };
    match node.get("padding").and_then(Value::as_str) {
        Some("small") => element.p(px(space::S)),
        Some("medium") => element.p(px(space::M)),
        Some("large") => element.p(px(space::L)),
        _ => element,
    }
}

/// 空白：没写 `size` 时占满剩下的地方（在 `row` 里把后面的东西推到最右）。
fn spacer(node: &Value) -> Div {
    match node.get("size").and_then(Value::as_str) {
        Some("small") => div().flex_none().size(px(space::S)),
        Some("medium") => div().flex_none().size(px(space::L)),
        Some("large") => div().flex_none().size(px(space::XXL)),
        _ => div().flex_1(),
    }
}

/// 卡片、折叠区的标题：可选图标 + 标题（读屏软件能按标题跳转）+ 可选说明。
fn section_heading(node: &Value, id: &SharedString, p: &Palette) -> Div {
    let title = node.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
    let description = node.get("description").and_then(Value::as_str).map(str::to_owned);
    v_flex()
        .gap(px(2.))
        .child(
            h_flex()
                .gap(px(space::S))
                .items_center()
                .children(icon_of(node).map(|icon| Icon::new(icon).size(px(crate::design::size::ICON_MD)).text_color(p.text_secondary)))
                .child(div().id(id.clone()).role(Role::Heading).aria_level(2).aria_label(title.clone()).text_heading().child(title)),
        )
        .children(description.map(|text| div().text_caption().text_color(p.text_secondary).child(text)))
}

/// 卡片的外框：`outlined`（默认，描边）、`filled`（面板，下沉底色）、`plain`（只有内边距）。
fn card_frame(node: &Value, p: &Palette) -> Div {
    match node.get("variant").and_then(Value::as_str) {
        Some("filled") => panel(p).p(px(space::L)),
        Some("plain") => div().p(px(space::L)),
        _ => card(p).p(px(space::L)),
    }
}

/// 文字节点。`heading_roles` 时标题样式的文字同时是标题，读屏软件可以按标题跳转（只读的卡片里不需要）。
fn text_node(node: &Value, id: &SharedString, heading_roles: bool, cx: &App) -> AnyElement {
    let p = palette(cx);
    let text = node.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
    let tone = node.get("tone").and_then(Value::as_str);
    let style = node.get("style").and_then(Value::as_str);
    let level = match style {
        Some("title") if heading_roles => Some(1),
        Some("heading") if heading_roles => Some(2),
        _ => None,
    };
    let truncate = node.get("truncate").and_then(Value::as_bool) == Some(true);
    let lines = node.get("lines").and_then(Value::as_u64).map(|lines| lines.clamp(1, 10) as usize);
    let element = match level {
        Some(level) => div().id(id.clone()).role(Role::Heading).aria_level(level).aria_label(text.clone()).child(text).into_any_element(),
        None => readable_with_id(id.clone(), text).into_any_element(),
    };
    // 颜色、字号放在外层，“说明”样式才能盖过默认颜色。
    let element = div().text_color(tone_color(tone, &p).unwrap_or(p.text)).child(element);
    let element = match style {
        Some("title") => element.text_title(),
        Some("heading") => element.text_heading(),
        Some("caption") => element.text_caption().when(tone.is_none(), |e| e.text_color(p.text_secondary)),
        Some("mono") => element.text_body().text_mono(mono_font(cx)),
        _ => element.text_body(),
    };
    let element = match node.get("weight").and_then(Value::as_str) {
        Some("regular") => element.font_weight(FontWeight::NORMAL),
        Some("medium") => element.font_weight(FontWeight::MEDIUM),
        Some("bold") => element.font_weight(FontWeight::BOLD),
        _ => element,
    };
    let element = match node.get("align").and_then(Value::as_str) {
        Some("center") => element.w_full().text_center(),
        Some("end") => element.w_full().text_right(),
        _ => element,
    };
    element
        .when(truncate, |element| element.min_w(px(0.)).truncate())
        .when_some(lines.filter(|_| !truncate), |element, lines| element.line_clamp(lines))
        .into_any_element()
}

/// 徽标：`tone` 定颜色，`variant` 是浅底（默认）/ 描边 / 实底，`size` 定字号。默认样式和设计规范的徽标一样。
fn badge_node(node: &Value, inherited: Option<NodeSize>, p: &Palette) -> Div {
    let text = node.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
    badge_view(
        text,
        node.get("tone").and_then(Value::as_str),
        node.get("variant").and_then(Value::as_str),
        icon_of(node),
        NodeSize::of(node, inherited, NodeSize::Small),
        p,
    )
}

fn badge_view(text: String, tone: Option<&str>, variant: Option<&str>, icon: Option<IconName>, size: NodeSize, p: &Palette) -> Div {
    if tone.is_none() && variant.is_none_or(|variant| variant == "soft") && icon.is_none() && size == NodeSize::Small {
        return badge(text, p);
    }
    let color = tone_color(tone, p).unwrap_or(p.accent);
    let element = div().flex().flex_none().items_center().gap(px(space::XS)).rounded_full().py(px(2.));
    let element = match size {
        NodeSize::Small => element.px(px(space::S)).text_caption(),
        NodeSize::Medium => element.px(px(space::M)).text_body(),
        NodeSize::Large => element.px(px(space::M)).py(px(space::XS)).text_content(),
    };
    let element = match variant {
        Some("outline") => element.border_1().border_color(color).text_color(color),
        Some("solid") => element.bg(color).text_color(p.on_accent),
        _ if tone.is_none() || tone == Some("accent") => element.bg(p.accent_soft).text_color(p.accent),
        _ => element.bg(color.opacity(0.12)).text_color(color),
    };
    element
        .children(icon.map(|icon| Icon::new(icon).size(px(size.indicator() - 2.))))
        .child(readable_with_id(ElementId::Name(SharedString::from(format!("badge-{text}"))), text))
}

/// 指标卡：名称、数值、变化量（正数绿、负数红）、说明，可选一条迷你折线。
fn stat_node(node: &Value, id: &SharedString, inherited: Option<NodeSize>, cx: &App) -> AnyElement {
    let p = palette(cx);
    let scalar = |key: &str| match node.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => number.as_f64().map(format_number),
        _ => None,
    };
    let label = node.get("label").and_then(Value::as_str).unwrap_or_default().to_owned();
    let value = scalar("value").unwrap_or_default();
    let delta = node.get("delta").map(|delta| match delta {
        Value::Number(number) => {
            let number = number.as_f64().unwrap_or(0.);
            let text = if number > 0. { format!("+{}", format_number(number)) } else { format_number(number) };
            (text, number.partial_cmp(&0.).unwrap_or(std::cmp::Ordering::Equal))
        }
        other => {
            let text = other.as_str().unwrap_or_default().to_owned();
            let sign = match text.chars().next() {
                Some('+') => std::cmp::Ordering::Greater,
                Some('-' | '−') => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Equal,
            };
            (text, sign)
        }
    });
    let caption = node.get("caption").and_then(Value::as_str).map(str::to_owned);
    let size = NodeSize::of(node, inherited, NodeSize::Medium);
    let value_color = tone_color(node.get("tone").and_then(Value::as_str), &p).unwrap_or(p.text);
    let spoken = [Some(label.clone()), Some(value.clone()), delta.as_ref().map(|(text, _)| text.clone()), caption.clone()]
        .into_iter()
        .flatten()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("，");
    let number = div().text_color(value_color).child(value);
    let number = match size {
        NodeSize::Small => number.text_heading(),
        NodeSize::Medium => number.text_title(),
        NodeSize::Large => number.text_display(),
    };
    let spark: Vec<f64> =
        node.get("spark").and_then(Value::as_array).map(|spark| spark.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
    card(&p)
        .id(SharedString::from(format!("{id}-stat")))
        .role(Role::Label)
        .aria_value(spoken)
        .p(px(space::L))
        .min_w(px(140.))
        .child(
            v_flex()
                .gap(px(space::XS))
                .child(
                    h_flex()
                        .gap(px(space::XS))
                        .items_center()
                        .text_caption()
                        .text_color(p.text_secondary)
                        .children(icon_of(node).map(|icon| Icon::new(icon).size(px(crate::design::size::ICON_SM))))
                        .child(label),
                )
                .child(h_flex().gap(px(space::S)).items_end().child(number).children(delta.map(|(text, sign)| {
                    let color = match sign {
                        std::cmp::Ordering::Greater => p.success,
                        std::cmp::Ordering::Less => p.danger,
                        std::cmp::Ordering::Equal => p.text_secondary,
                    };
                    div().pb(px(2.)).text_caption().text_color(color).child(text)
                })))
                .children(caption.map(|text| div().text_caption().text_color(p.text_secondary).child(text)))
                .when(spark.len() > 1, |column| column.child(sparkline(spark, SharedString::from(format!("{id}-spark")), &p))),
        )
        .into_any_element()
}

/// 指标卡里的迷你折线。
fn sparkline(values: Vec<f64>, id: SharedString, p: &Palette) -> AnyElement {
    use gpui_kit::component::chart::LineChart;
    let points: Vec<(SharedString, f64)> = values.into_iter().enumerate().map(|(index, value)| (index.to_string().into(), value)).collect();
    div()
        .w_full()
        .h(px(32.))
        .child(
            LineChart::new(points)
                .id(id)
                .x(|point: &(SharedString, f64)| point.0.clone())
                .y(|point: &(SharedString, f64)| point.1)
                .stroke(p.accent)
                .x_axis(false)
                .y_axis(false)
                .grid(false)
                .interactive(false),
        )
        .into_any_element()
}

/// 提示条。按设计规范，只有需要注意（`warning`）和错误（`danger`）用浅色底，其余只描边。
fn alert_node(node: &Value, id: &SharedString, action: Option<AnyElement>, p: &Palette) -> AnyElement {
    let tone = node.get("tone").and_then(Value::as_str);
    let color = tone_color(tone, p).unwrap_or(p.text_secondary);
    let icon = icon_of(node).unwrap_or(match tone {
        Some("success") => IconName::CircleCheck,
        Some("warning") => IconName::TriangleAlert,
        Some("danger") => IconName::CircleAlert,
        _ => IconName::Info,
    });
    let title = node.get("title").and_then(Value::as_str).map(str::to_owned);
    let text = node.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
    let urgent = matches!(tone, Some("warning" | "danger"));
    h_flex()
        .id(SharedString::from(format!("{id}-alert")))
        .role(if urgent { Role::Alert } else { Role::Status })
        .w_full()
        .gap(px(space::S))
        .items_start()
        .px(px(space::M))
        .py(px(space::S))
        .rounded(px(radius::MD))
        .when(urgent, |alert| alert.bg(color.opacity(0.1)))
        .when(!urgent, |alert| alert.border_1().border_color(p.border))
        .child(div().pt(px(3.)).child(Icon::new(icon).size(px(crate::design::size::ICON_MD)).text_color(color)))
        .child(
            v_flex()
                .flex_1()
                .min_w(px(0.))
                .gap(px(2.))
                .children(
                    title.map(|title| div().text_label().child(readable_with_id(SharedString::from(format!("{id}-alert-title")), title))),
                )
                .child(div().text_body().child(readable_with_id(SharedString::from(format!("{id}-alert-text")), text))),
        )
        .children(action)
        .into_any_element()
}

/// 空状态：居中的图标、标题、说明和一个操作。
fn empty_node(node: &Value, id: &SharedString, action: Option<AnyElement>, p: &Palette) -> AnyElement {
    let title = node.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
    let description = node.get("description").and_then(Value::as_str).map(str::to_owned);
    v_flex()
        .w_full()
        .items_center()
        .gap(px(space::S))
        .py(px(space::XXL))
        .child(Icon::new(icon_of(node).unwrap_or(IconName::Inbox)).size(px(crate::design::size::ICON_HERO)).text_color(p.text_secondary))
        .child(div().text_heading().child(readable_with_id(SharedString::from(format!("{id}-empty-title")), title)))
        .children(description.map(|text| {
            div()
                .text_body()
                .text_center()
                .text_color(p.text_secondary)
                .child(readable_with_id(SharedString::from(format!("{id}-empty-text")), text))
        }))
        .children(action.map(|action| div().pt(px(space::S)).child(action)))
        .into_any_element()
}

/// `select` / `radio` 的选项：(值, 显示的文字)。
fn options_of(node: &Value) -> Vec<(String, String)> {
    node.get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|option| {
            let value = option.as_str().or_else(|| option.get("value").and_then(Value::as_str)).unwrap_or_default().to_owned();
            let label = option.get("label").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| value.clone());
            (value, label)
        })
        .collect()
}

/// 列表的一行：图标、标题（按 `tone` 着色）、详情、徽标、右侧文字。行高按 `size`。
fn list_row(item: &Value, size: NodeSize, p: &Palette) -> Div {
    let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned);
    let color = tone_color(item.get("tone").and_then(Value::as_str), p).unwrap_or(p.text);
    let padding = match size {
        NodeSize::Small => space::XS,
        NodeSize::Medium => space::S,
        NodeSize::Large => space::M,
    };
    h_flex()
        .px(px(space::M))
        .py(px(padding))
        .gap(px(space::M))
        .items_center()
        .rounded(px(radius::MD))
        .children(icon_of(item).map(|icon| Icon::new(icon).size(px(size.indicator())).text_color(p.text_secondary)))
        .child(size.text(div().flex_1().min_w(px(0.)).text_color(color)).child(text("title").unwrap_or_default()))
        .children(text("detail").map(|detail| div().text_caption().row_color(p.text_secondary, p).child(detail)))
        .children(text("badge").map(|text| badge(text, p)))
        .children(text("trailing").map(|text| size.text(div().flex_none().text_color(p.text_secondary)).child(text)))
}

/// 点了表格的某一行：(行号, 这一行的单元格)。
type PickRow = std::rc::Rc<dyn Fn(usize, Value, &mut Window, &mut App)>;

/// 表格：每列可以有对齐（`align`）和相对宽度（`widths`）；`striped` 隔行底色；`size` 定行高和字号；
/// 单元格可以是 `{ text, tone, badge }`。有 `on_row` 时每行能点（`rowAction`）。
fn table(node: &Value, id: &SharedString, inherited: Option<NodeSize>, on_row: Option<PickRow>, cx: &App) -> AnyElement {
    let p = palette(cx);
    let cell_text = |cell: &Value| match cell {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Object(object) => object.get("text").and_then(Value::as_str).unwrap_or_default().to_owned(),
        other => other.to_string(),
    };
    let columns: Vec<String> =
        node.get("columns").and_then(Value::as_array).map(|columns| columns.iter().map(cell_text).collect()).unwrap_or_default();
    let rows = node.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
    let aligns: Vec<String> = node
        .get("align")
        .and_then(Value::as_array)
        .map(|aligns| aligns.iter().map(|align| align.as_str().unwrap_or("left").to_owned()).collect())
        .unwrap_or_default();
    let widths: Vec<f32> = node
        .get("widths")
        .and_then(Value::as_array)
        .map(|widths| widths.iter().map(|width| width.as_f64().unwrap_or(1.) as f32).collect())
        .unwrap_or_default();
    let size = NodeSize::of(node, inherited, NodeSize::Medium);
    let striped = node.get("striped").and_then(Value::as_bool) == Some(true);
    let max_height = node.get("maxHeight").and_then(Value::as_f64).map(|height| height as f32);
    let padding = match size {
        NodeSize::Small => 2.,
        NodeSize::Medium => space::XS,
        NodeSize::Large => space::S,
    };
    let cell = |column: usize, content: AnyElement| {
        let element = div().flex_basis(px(0.)).flex_grow(widths.get(column).copied().unwrap_or(1.)).min_w(px(0.)).truncate();
        match aligns.get(column).map(String::as_str) {
            Some("center") => element.text_center(),
            Some("right") => element.text_right(),
            _ => element,
        }
        .child(content)
    };
    let row = |cells: Vec<AnyElement>| h_flex().w_full().gap(px(space::M)).px(px(space::M)).py(px(padding)).children(cells);
    let header = row(columns
        .iter()
        .enumerate()
        .map(|(column, text)| {
            cell(column, readable_with_id(SharedString::from(format!("{id}-head-{column}")), text.clone()).into_any_element())
                .into_any_element()
        })
        .collect())
    .text_label()
    .text_color(p.text_secondary);
    let body: Vec<AnyElement> = rows
        .iter()
        .enumerate()
        .map(|(index, cells)| {
            let values = cells.as_array().cloned().unwrap_or_default();
            let spoken = values.iter().map(cell_text).collect::<Vec<_>>().join("，");
            let content = row(values
                .iter()
                .enumerate()
                .map(|(column, value)| {
                    let tone = value.get("tone").and_then(Value::as_str);
                    let content = if value.get("badge").and_then(Value::as_bool) == Some(true) {
                        // 徽标按内容宽度，跟着这一列的对齐方式摆。
                        let badge = badge_view(cell_text(value), tone, None, None, NodeSize::Small, &p);
                        match aligns.get(column).map(String::as_str) {
                            Some("center") => h_flex().justify_center().child(badge),
                            Some("right") => h_flex().justify_end().child(badge),
                            _ => h_flex().child(badge),
                        }
                        .into_any_element()
                    } else {
                        div()
                            .when_some(tone_color(tone, &p), |cell, color| cell.text_color(color))
                            .child(readable_with_id(SharedString::from(format!("{id}-cell-{index}-{column}")), cell_text(value)))
                            .into_any_element()
                    };
                    cell(column, content).into_any_element()
                })
                .collect());
            let content = size.text(content).when(striped && index % 2 == 1, |row| row.bg(p.sunken));
            let line = div().border_t_1().border_color(p.border);
            match &on_row {
                Some(pick) => {
                    let (pick, row_value) = (pick.clone(), Value::Array(values.clone()));
                    line.child(
                        pressable(SharedString::from(format!("{id}-row-{index}")), &p)
                            .accessibility_label(spoken)
                            .relative()
                            .w_full()
                            .cursor_pointer()
                            .hover_fill(&p, 0., false)
                            .on_click(move |_, window, cx| pick(index, row_value.clone(), window, cx))
                            .child(content),
                    )
                    .into_any_element()
                }
                None => line.child(content).into_any_element(),
            }
        })
        .collect();
    let empty = node.get("empty").and_then(Value::as_str).filter(|_| rows.is_empty()).map(|text| {
        div().border_t_1().border_color(p.border).px(px(space::M)).py(px(space::S)).child(hint(text.to_owned(), Tone::Neutral, &p))
    });
    let body = v_flex().id(SharedString::from(format!("{id}-rows"))).children(body).children(empty);
    let body = match max_height {
        Some(height) => body.max_h(px(height)).overflow_y_scroll().into_any_element(),
        None => body.into_any_element(),
    };
    v_flex()
        .w_full()
        .rounded(px(radius::MD))
        .border_1()
        .border_color(p.border)
        .overflow_hidden()
        .child(header)
        .child(body)
        .into_any_element()
}

/// 只展示的节点（`markdown`、`code`、`diff`、`table`、`progress`）。别的类型返回 `None`。
fn display_node(node: &Value, id: &SharedString, cx: &App) -> Option<AnyElement> {
    let p = palette(cx);
    let string = |key: &str| node.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let element = match node.get("type").and_then(Value::as_str).unwrap_or_default() {
        "markdown" => div()
            .id(SharedString::from(format!("{id}-md")))
            .role(Role::Label)
            .aria_value(crate::transcript::plain_markdown(&string("text")))
            .text_content()
            .child(TextView::markdown(id.clone(), string("text")))
            .into_any_element(),
        "code" => div()
            .w_full()
            .px(px(space::M))
            .py(px(space::S))
            .rounded(px(radius::MD))
            .bg(p.sunken)
            .text_mono(mono_font(cx))
            .child(readable_with_id(SharedString::from(format!("{id}-code")), string("text")))
            .into_any_element(),
        "diff" => {
            let text = match node.get("text").and_then(Value::as_str) {
                Some(text) => text.to_owned(),
                None => dshr_tools::unified_diff(&string("path"), &string("before"), &string("after")),
            };
            let lines = text.lines().enumerate().map(|(index, line)| {
                let row = div()
                    .px(px(space::M))
                    .whitespace_nowrap()
                    .child(readable_with_id(SharedString::from(format!("{id}-line-{index}")), line.to_owned()));
                if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
                    row.text_color(p.text_secondary)
                } else if line.starts_with('+') {
                    row.bg(p.success.opacity(0.12))
                } else if line.starts_with('-') {
                    row.bg(p.danger.opacity(0.12))
                } else {
                    row
                }
            });
            div()
                .w_full()
                .py(px(space::XS))
                .rounded(px(radius::MD))
                .bg(p.sunken)
                .text_mono(mono_font(cx))
                .children(lines)
                .into_any_element()
        }
        "table" => table(node, id, None, None, cx),
        "progress" => {
            let value = node.get("value").and_then(Value::as_f64).unwrap_or(0.).clamp(0., 1.);
            let label = node.get("label").and_then(Value::as_str).map(str::to_owned);
            let percent = format!("{}%", (value * 100.).round());
            v_flex()
                .w_full()
                .gap(px(space::XS))
                .child(
                    h_flex()
                        .justify_between()
                        .text_caption()
                        .text_color(p.text_secondary)
                        .child(readable_with_id(SharedString::from(format!("{id}-label")), label.unwrap_or_default()))
                        .child(readable_with_id(SharedString::from(format!("{id}-percent")), percent)),
                )
                .child(
                    div()
                        .w_full()
                        .h(px(6.))
                        .rounded_full()
                        .bg(p.sunken)
                        .child(div().h_full().rounded_full().bg(p.accent).w(relative(value as f32))),
                )
                .into_any_element()
        }
        "chart" => chart(node, id, cx),
        "heatmap" => heatmap(node, id, cx),
        "calendar" => calendar(node, id, None, cx),
        "kv" => {
            let items = node.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            v_flex()
                .w_full()
                .gap(px(2.))
                .children(items.into_iter().enumerate().map(|(index, item)| {
                    let key = item.get("key").and_then(Value::as_str).unwrap_or_default().to_owned();
                    let value = match item.get("value") {
                        Some(Value::String(text)) => text.clone(),
                        Some(Value::Null) | None => String::new(),
                        Some(other) => other.to_string(),
                    };
                    h_flex()
                        .id(SharedString::from(format!("{id}-kv-{index}")))
                        .role(Role::Label)
                        .aria_value(format!("{key}：{value}"))
                        .gap(px(space::M))
                        .child(div().w(px(140.)).flex_none().text_caption().text_color(p.text_secondary).child(key))
                        .child(div().flex_1().min_w(px(0.)).text_body().child(value))
                }))
                .into_any_element()
        }
        "image" => image(node, id, cx),
        _ => return None,
    };
    Some(element)
}

/// 图表：每个序列一张（多序列时上面写序列名）。点的第一项是横轴标签，其余是数值。
fn chart(node: &Value, id: &SharedString, cx: &App) -> AnyElement {
    use gpui_kit::component::chart::{AreaChart, BarChart, CandlestickChart, LineChart, PieChart};
    #[derive(Clone)]
    struct Point {
        x: SharedString,
        values: [f64; 4],
        index: usize,
    }
    let p = palette(cx);
    let kind = node.get("kind").and_then(Value::as_str).unwrap_or("line").to_owned();
    let height = node.get("height").and_then(Value::as_u64).unwrap_or(220) as f32;
    let series = node.get("series").and_then(Value::as_array).cloned().unwrap_or_default();
    let multiple = series.len() > 1;
    let colors = [p.accent, p.success, p.warning, p.danger, p.text_secondary];
    v_flex()
        .w_full()
        .gap(px(space::S))
        .children(series.into_iter().enumerate().map(|(series_index, one)| {
            let name = one.get("name").and_then(Value::as_str).map(str::to_owned);
            let points: Vec<Point> = one
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(|(index, point)| {
                    let point = point.as_array()?;
                    let x = match point.first()? {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    let mut values = [0.0; 4];
                    for (slot, value) in values.iter_mut().zip(point.iter().skip(1)) {
                        *slot = value.as_f64().unwrap_or(0.0);
                    }
                    Some(Point { x: x.into(), values, index })
                })
                .collect();
            let color = colors[series_index % colors.len()];
            let chart_id = SharedString::from(format!("{id}-chart-{series_index}"));
            let element: AnyElement = match kind.as_str() {
                "bar" => BarChart::new(points)
                    .id(chart_id)
                    .band(|point: &Point| point.x.clone())
                    .value(|point: &Point| point.values[0])
                    .into_any_element(),
                "area" => AreaChart::new(points)
                    .id(chart_id)
                    .x(|point: &Point| point.x.clone())
                    .y(|point: &Point| point.values[0])
                    .stroke(color)
                    .fill(color.opacity(0.2))
                    .into_any_element(),
                "pie" => {
                    let palette_colors = colors;
                    PieChart::new(points)
                        .id(chart_id)
                        .value(|point: &Point| point.values[0] as f32)
                        .color(move |point: &Point| palette_colors[point.index % palette_colors.len()])
                        .into_any_element()
                }
                "candlestick" => CandlestickChart::new(points)
                    .id(chart_id)
                    .x(|point: &Point| point.x.clone())
                    .open(|point: &Point| point.values[0])
                    .high(|point: &Point| point.values[1])
                    .low(|point: &Point| point.values[2])
                    .close(|point: &Point| point.values[3])
                    .into_any_element(),
                _ => LineChart::new(points)
                    .id(chart_id)
                    .x(|point: &Point| point.x.clone())
                    .y(|point: &Point| point.values[0])
                    .stroke(color)
                    .dot()
                    .into_any_element(),
            };
            v_flex()
                .gap(px(space::XS))
                .children(name.filter(|_| multiple).map(|name| div().text_caption().text_color(p.text_secondary).child(name)))
                .child(div().w_full().h(px(height)).child(element))
        }))
        .into_any_element()
}

/// 热力图：每格的颜色按值在最小、最大值之间插值强调色的深浅；没有值的格子留空。
fn heatmap(node: &Value, id: &SharedString, cx: &App) -> AnyElement {
    let p = palette(cx);
    let labels = |key: &str| -> Vec<String> {
        node.get(key)
            .and_then(Value::as_array)
            .map(|labels| labels.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let (rows, columns) = (labels("rows"), labels("columns"));
    let values: Vec<Vec<Option<f64>>> = node
        .get("values")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().map(|row| row.as_array().map(|row| row.iter().map(Value::as_f64).collect()).unwrap_or_default()).collect())
        .unwrap_or_default();
    let all: Vec<f64> = values.iter().flatten().flatten().copied().collect();
    let (min, max) = all.iter().fold((f64::MAX, f64::MIN), |(min, max), value| (min.min(*value), max.max(*value)));
    let label = node.get("label").and_then(Value::as_str).map(str::to_owned);
    v_flex()
        .gap(px(2.))
        .children(label.map(|label| div().text_caption().text_color(p.text_secondary).child(label)))
        .child(h_flex().gap(px(2.)).child(div().w(px(64.)).flex_none()).children(columns.iter().map(|column| {
            div().w(px(16.)).flex_none().text_caption().text_color(p.text_secondary).overflow_hidden().child(column.clone())
        })))
        .children(rows.iter().enumerate().map(|(row_index, row)| {
            h_flex()
                .gap(px(2.))
                .child(div().w(px(64.)).flex_none().truncate().text_caption().text_color(p.text_secondary).child(row.clone()))
                .children(columns.iter().enumerate().map(|(column_index, column)| {
                    let value = values.get(row_index).and_then(|row| row.get(column_index)).copied().flatten();
                    let strength = match value {
                        Some(value) if max > min => ((value - min) / (max - min)) as f32,
                        Some(_) => 1.0,
                        None => 0.0,
                    };
                    let spoken = match value {
                        Some(value) => format!("{row} {column}：{value}"),
                        None => format!("{row} {column}"),
                    };
                    div()
                        .id(SharedString::from(format!("{id}-heat-{row_index}-{column_index}")))
                        .role(Role::Label)
                        .aria_value(spoken)
                        .size(px(16.))
                        .flex_none()
                        .rounded(px(3.))
                        .bg(if value.is_some() { p.accent.opacity(0.12 + strength * 0.88) } else { p.sunken })
                }))
        }))
        .into_any_element()
}

/// 点了日历上的某一天（`YYYY-MM-DD`）。
pub(crate) type PickDate = std::rc::Rc<dyn Fn(String, &mut Window, &mut App)>;

/// 某月的日历：7 列，周一开头；`marks` 里有的日子下面显示一小段文字。`on_pick` 有的时候日子能点。
pub(crate) fn calendar(node: &Value, id: &SharedString, on_pick: Option<PickDate>, cx: &App) -> AnyElement {
    let p = palette(cx);
    let month = node.get("month").and_then(Value::as_str).unwrap_or("2026-01").to_owned();
    let (year, number) = (month[..4].parse::<i32>().unwrap_or(2026), month[5..].parse::<u32>().unwrap_or(1));
    let days = days_in_month(year, number);
    // 这个月 1 号是星期几（0 是周一）。
    let first = (weekday(year, number, 1) + 6) % 7;
    let marks = node.get("marks").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut cells: Vec<AnyElement> = (0..first).map(|_| div().flex_1().into_any_element()).collect();
    for day in 1..=days {
        let date = format!("{month}-{day:02}");
        let mark = marks.get(&date).and_then(Value::as_str).map(str::to_owned);
        let spoken = match &mark {
            Some(mark) => format!("{date}：{mark}"),
            None => date.clone(),
        };
        let cell = div()
            .id(SharedString::from(format!("{id}-day-{day}")))
            .role(Role::Label)
            .aria_value(spoken)
            .flex_1()
            .min_h(px(40.))
            .p(px(space::XS))
            .rounded(px(radius::SM))
            .when(mark.is_some(), |cell| cell.bg(p.accent_soft))
            .child(div().text_caption().child(day.to_string()))
            // 空的标记只用底色表示（日期选择里选中的那天），不占一行。
            .children(mark.filter(|mark| !mark.is_empty()).map(|mark| div().text_caption().truncate().row_color(p.accent, &p).child(mark)));
        let cell = match &on_pick {
            Some(pick) => {
                let (pick, date) = (pick.clone(), date.clone());
                cell.cursor_pointer()
                    .relative()
                    .hover_fill(&p, radius::SM, false)
                    .on_click(move |_, window, cx| pick(date.clone(), window, cx))
                    .into_any_element()
            }
            None => cell.into_any_element(),
        };
        cells.push(cell);
    }
    while !cells.len().is_multiple_of(7) {
        cells.push(div().flex_1().into_any_element());
    }
    let mut weeks = Vec::new();
    let mut cells = cells.into_iter();
    loop {
        let week: Vec<AnyElement> = cells.by_ref().take(7).collect();
        if week.is_empty() {
            break;
        }
        weeks.push(h_flex().gap(px(2.)).children(week));
    }
    v_flex()
        .w_full()
        .gap(px(2.))
        .child(div().text_label().child(month.clone()))
        .child(h_flex().gap(px(2.)).children(
            ["一", "二", "三", "四", "五", "六", "日"].map(|name| div().flex_1().text_caption().text_color(p.text_secondary).child(name)),
        ))
        .children(weeks)
        .into_any_element()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 星期几（0 是周日）：Sakamoto 的算法。
fn weekday(year: i32, month: u32, day: u32) -> u32 {
    const OFFSETS: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let year = if month < 3 { year - 1 } else { year };
    ((year + year / 4 - year / 100 + year / 400 + OFFSETS[(month - 1) as usize] + day as i32).rem_euclid(7)) as u32
}

/// 图片节点：宿主已经把路径换成了 `data:` URI。
fn image(node: &Value, id: &SharedString, cx: &App) -> AnyElement {
    let p = palette(cx);
    let src = node.get("src").and_then(Value::as_str).unwrap_or_default();
    let alt = node.get("alt").and_then(Value::as_str).unwrap_or_default().to_owned();
    let width = node.get("width").and_then(Value::as_u64).map(|width| width as f32);
    let decoded = src
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
        .and_then(|(media_type, data)| decoded_image(media_type, data));
    match decoded {
        Some(image) => div()
            .id(SharedString::from(format!("{id}-image")))
            .role(Role::Image)
            .aria_label(alt)
            .child(img(image).max_w_full().when_some(width, |image, width| image.w(px(width))))
            .into_any_element(),
        None => hint(if alt.is_empty() { "[image]".to_owned() } else { alt }, Tone::Neutral, &p).into_any_element(),
    }
}

/// 解码过的 `data:` 图片，按内容记住最近用过的几张。插件的节点树每帧重建，
/// 不记住的话每帧都要把 base64 再解一遍、分配一份新的图片数据。
fn decoded_image(media_type: &str, data: &str) -> Option<Arc<Image>> {
    use std::hash::{Hash as _, Hasher as _};
    /// 最多记这么多张；比一屏能同时出现的图片多就够了。
    const RECENT: usize = 16;
    thread_local! {
        static RECENT_IMAGES: std::cell::RefCell<std::collections::VecDeque<(u64, Arc<Image>)>> = Default::default();
    }
    let mut hasher = std::hash::DefaultHasher::new();
    (media_type, data).hash(&mut hasher);
    let key = hasher.finish();
    if let Some(image) = RECENT_IMAGES.with_borrow(|recent| recent.iter().find(|(seen, _)| *seen == key).map(|(_, image)| image.clone())) {
        return Some(image);
    }
    let image = crate::transcript::decode_image(media_type, data)?;
    RECENT_IMAGES.with_borrow_mut(|recent| {
        if recent.len() >= RECENT {
            recent.pop_front();
        }
        recent.push_back((key, image.clone()));
    });
    Some(image)
}

/// 只读地画一棵节点树：插件给工具画的卡片、插件事件在对话区的那一行。能操作的节点（按钮、输入框、开关……）
/// 在这里只显示它们的文字，不能点。`prefix` 让同一屏里的元素 id 不重复。
pub fn static_tree(node: &Value, prefix: &str, cx: &App) -> AnyElement {
    fn walk(node: &Value, path: &str, cx: &App) -> AnyElement {
        let p = palette(cx);
        let id = SharedString::from(path.to_owned());
        if let Some(text) = node.as_str() {
            return div().text_body().child(readable_with_id(id, text.to_owned())).into_any_element();
        }
        if node.get("hidden").and_then(Value::as_bool) == Some(true) {
            return div().into_any_element();
        }
        let string = |key: &str| match node.get(key) {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Number(number)) => number.as_f64().map(format_number).unwrap_or_default(),
            _ => String::new(),
        };
        let nodes = |list: Option<&Value>, at: &str| -> Vec<AnyElement> {
            list.and_then(Value::as_array)
                .map(|children| children.iter().enumerate().map(|(index, child)| walk(child, &format!("{path}.{at}{index}"), cx)).collect())
                .unwrap_or_default()
        };
        let children = || nodes(node.get("children"), "");
        let kind = node.get("type").and_then(Value::as_str).unwrap_or_default();
        let element = match kind {
            "column" => layout(v_flex().gap(px(gap_of(node, space::S))), node).children(children()).into_any_element(),
            "row" => {
                layout(h_flex().flex_wrap().items_center().gap(px(gap_of(node, space::S))), node).children(children()).into_any_element()
            }
            "grid" => {
                let columns = node.get("columns").and_then(Value::as_u64).unwrap_or(2).clamp(1, 6) as u16;
                layout(div().w_full().grid().grid_cols(columns).gap(px(gap_of(node, space::S))), node)
                    .children(children())
                    .into_any_element()
            }
            "spacer" => spacer(node).into_any_element(),
            // 只读的地方折叠区按插件写的 `open` 画（卡片默认展开，折叠区默认收起）。
            "card" | "collapsible" => {
                let open = node.get("open").and_then(Value::as_bool).unwrap_or(kind == "card")
                    || (kind == "card" && node.get("collapsible").and_then(Value::as_bool) != Some(true));
                let header = node
                    .get("title")
                    .and_then(Value::as_str)
                    .map(|_| section_heading(node, &SharedString::from(format!("{path}-title")), &p));
                let body = v_flex().gap(px(space::S)).children(header).when(open, |body| body.children(children()));
                match kind {
                    "collapsible" => body.into_any_element(),
                    _ => card_frame(node, &p).child(body).into_any_element(),
                }
            }
            "tabs" => {
                let tabs = node.get("tabs").and_then(Value::as_array).cloned().unwrap_or_default();
                let value = node.get("value").and_then(Value::as_str);
                let tab = tabs.iter().find(|tab| tab.get("id").and_then(Value::as_str) == value).or(tabs.first());
                let label = tab.and_then(|tab| tab.get("label")).and_then(Value::as_str).unwrap_or_default().to_owned();
                v_flex()
                    .gap(px(space::S))
                    .child(div().text_label().child(label))
                    .children(nodes(tab.and_then(|tab| tab.get("children")), "tab"))
                    .into_any_element()
            }
            "text" => text_node(node, &id, false, cx),
            "badge" => badge_node(node, None, &p).into_any_element(),
            "divider" => divider(&p).into_any_element(),
            "stat" => stat_node(node, &id, None, cx),
            "alert" => alert_node(node, &id, None, &p),
            "empty" => empty_node(node, &id, None, &p),
            "list" => v_flex()
                .gap(px(2.))
                .children(node.get("items").and_then(Value::as_array).into_iter().flatten().enumerate().map(|(index, item)| {
                    let title = item.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
                    div().id(SharedString::from(format!("{path}-item-{index}"))).role(Role::Label).aria_value(title).child(list_row(
                        item,
                        NodeSize::of(node, None, NodeSize::Medium),
                        &p,
                    ))
                }))
                .into_any_element(),
            // 能操作的节点：只读的地方只显示文字。
            "button" | "switch" | "link" | "checkbox" => hint(string("label"), Tone::Neutral, &p).into_any_element(),
            "input" | "textarea" | "select" | "radio" | "number" | "slider" | "date" => {
                let text = [string("label"), string("value")].into_iter().filter(|text| !text.is_empty()).collect::<Vec<_>>().join("：");
                hint(text, Tone::Neutral, &p).into_any_element()
            }
            "checkboxes" | "tags-input" => {
                let values = node
                    .get("value")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("、");
                let text = [string("label"), values].into_iter().filter(|text| !text.is_empty()).collect::<Vec<_>>().join("：");
                hint(text, Tone::Neutral, &p).into_any_element()
            }
            "section" => v_flex()
                .gap(px(space::S))
                .child(section_heading(node, &SharedString::from(format!("{path}-title")), &p))
                .children(children())
                .into_any_element(),
            "split" => split(node, nodes(node.get("left"), "left"), nodes(node.get("right"), "right")).into_any_element(),
            kind => display_node(node, &id, cx)
                .unwrap_or_else(|| hint(t!("plugin_page.unknown_node", kind = kind).to_string(), Tone::Danger, &p).into_any_element()),
        };
        with_tooltip(node, SharedString::from(format!("{path}-tip")), element)
    }
    walk(node, prefix, cx)
}

/// 一个表单控件的状态。
enum Control {
    /// 单行输入框；`number` 节点也用它，`number` 是 (最小值, 最大值)。按 Enter 时执行节点的 `submit`。
    Input {
        state: Entity<InputState>,
        last: String,
        number: Option<(Option<f64>, Option<f64>)>,
        /// 密码框（`kind: "password"`）和字数上限建的时候定下，变了要重建。
        masked: bool,
        max_length: Option<usize>,
        _events: Subscription,
    },
    Textarea {
        state: Entity<TextareaState>,
        last: String,
    },
    /// 选择框、单选、日期（字符串）、复选框（布尔）和多选（字符串数组）：现在的值、上次从节点里写进去的值。
    Choice {
        current: Value,
        last: Value,
    },
    /// 滑块：松手时执行节点的 `action`。
    Slider {
        state: Entity<SliderState>,
        last: f64,
        /// (最小值, 最大值, 步长)：建的时候定下，变了要重建。
        range: (f64, f64, f64),
        _events: Subscription,
    },
    /// 标签输入：已有的标签，输入框里按 Enter 加一个。
    Tags {
        input: Entity<InputState>,
        tags: Vec<String>,
        last: Vec<String>,
        _events: Subscription,
    },
    /// 下拉框（`select` 的 `style: "dropdown"`），可带筛选框。`shown` 是筛选后列表里每一项对应 `options` 的第几项。
    Dropdown {
        state: Entity<DropdownState>,
        filter: Option<Entity<InputState>>,
        options: Vec<(String, String)>,
        shown: Vec<usize>,
        current: String,
        last: String,
        _events: Vec<Subscription>,
    },
}

impl Control {
    /// 交给插件的值：输入框是文字，数字框、滑块是数字（数字框空的或不是数字时是 `null`，超出范围时取边界），
    /// 标签输入是字符串数组，选择类按原样。
    fn value(&self, cx: &App) -> Value {
        match self {
            Control::Input { state, number: None, .. } => Value::String(state.read(cx).value().to_string()),
            Control::Input { state, number: Some((min, max)), .. } => {
                match state.read(cx).value().trim().parse::<f64>().ok().filter(|value| value.is_finite()) {
                    Some(value) => json!(clamp(value, *min, *max)),
                    None => Value::Null,
                }
            }
            Control::Textarea { state, .. } => Value::String(state.read(cx).value().to_string()),
            Control::Choice { current, .. } => current.clone(),
            Control::Slider { state, .. } => json!(slider_value(state.read(cx).value())),
            Control::Tags { tags, .. } => json!(tags),
            Control::Dropdown { current, .. } => Value::String(current.clone()),
        }
    }

    /// 节点换了类型（同一个 id 从输入框变成选择框等），或者换了建的时候定下的设置时，状态要重建。
    fn matches(&self, seed: &Seed) -> bool {
        match (self, seed) {
            (Control::Input { masked, max_length, .. }, Seed::Input { masked: wanted, max_length: wanted_length, .. }) => {
                masked == wanted && max_length == wanted_length
            }
            (Control::Dropdown { filter, .. }, Seed::Dropdown { searchable, .. }) => filter.is_some() == *searchable,
            (Control::Slider { range, .. }, Seed::Slider { min, max, step, .. }) => *range == (*min, *max, *step),
            (Control::Textarea { .. }, Seed::Textarea { .. })
            | (Control::Choice { .. }, Seed::Choice(_))
            | (Control::Tags { .. }, Seed::Tags { .. }) => true,
            _ => false,
        }
    }
}

/// 从节点里读出的控件初值。
enum Seed {
    Input { value: String, placeholder: String, number: Option<(Option<f64>, Option<f64>)>, masked: bool, max_length: Option<usize> },
    Textarea { value: String, placeholder: String },
    Choice(Value),
    Slider { value: f64, min: f64, max: f64, step: f64 },
    Tags { value: Vec<String>, placeholder: String },
    Dropdown { options: Vec<(String, String)>, value: String, searchable: bool },
}

impl Seed {
    fn of(node: &Value) -> Option<Self> {
        let text = |key: &str| node.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let number = |key: &str| node.get(key).and_then(Value::as_f64);
        let strings = |key: &str| -> Vec<String> {
            node.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
        };
        Some(match node.get("type").and_then(Value::as_str)? {
            "input" => Seed::Input {
                value: text("value"),
                placeholder: text("placeholder"),
                number: None,
                masked: node.get("kind").and_then(Value::as_str) == Some("password"),
                max_length: node.get("maxLength").and_then(Value::as_u64).map(|length| length as usize),
            },
            "number" => Seed::Input {
                value: number("value").map(format_number).unwrap_or_default(),
                placeholder: text("placeholder"),
                number: Some((number("min"), number("max"))),
                masked: false,
                max_length: None,
            },
            "textarea" => Seed::Textarea { value: text("value"), placeholder: text("placeholder") },
            "select" if node.get("style").and_then(Value::as_str) == Some("dropdown") => Seed::Dropdown {
                options: options_of(node),
                value: text("value"),
                searchable: node.get("searchable").and_then(Value::as_bool) == Some(true),
            },
            "select" | "radio" | "date" => Seed::Choice(Value::String(text("value"))),
            "checkbox" => Seed::Choice(Value::Bool(node.get("checked").and_then(Value::as_bool).unwrap_or(false))),
            "checkboxes" => Seed::Choice(json!(strings("value"))),
            "slider" => {
                let (min, max) = (number("min").unwrap_or(0.), number("max").unwrap_or(100.));
                Seed::Slider {
                    value: clamp(number("value").unwrap_or(min), Some(min), Some(max)),
                    min,
                    max,
                    step: number("step").filter(|step| *step > 0.).unwrap_or(1.),
                }
            }
            "tags-input" => Seed::Tags { value: strings("value"), placeholder: text("placeholder") },
            _ => return None,
        })
    }
}

/// 滑块的值（去掉浮点误差）。
fn slider_value(value: SliderValue) -> f64 {
    let value = match value {
        SliderValue::Single(value) => value,
        SliderValue::Range(_, end) => end,
    } as f64;
    (value * 1e6).round() / 1e6
}

/// 下拉框显示的文字，和当前值在其中是第几项（筛掉了就是 `None`）。
fn dropdown_items(options: &[(String, String)], shown: &[usize], current: &str) -> (Vec<SharedString>, Option<usize>) {
    let labels = shown.iter().map(|index| SharedString::from(options[*index].1.clone())).collect();
    let selected = shown.iter().position(|index| options[*index].0 == current);
    (labels, selected)
}

/// 下拉框筛选后还剩哪些选项（`options` 里的第几项）：显示的文字里包含筛选的字（不分大小写）。
/// 当前选中的那项总是留着，否则下拉框看起来像是没选。
fn filter_options(options: &[(String, String)], filter: &str, current: &str) -> Vec<usize> {
    let filter = filter.trim().to_lowercase();
    (0..options.len())
        .filter(|index| {
            let (value, label) = &options[*index];
            filter.is_empty() || value == current || label.to_lowercase().contains(&filter)
        })
        .collect()
}

fn clamp(value: f64, min: Option<f64>, max: Option<f64>) -> f64 {
    let value = min.map_or(value, |min| value.max(min));
    max.map_or(value, |max| value.min(max))
}

/// 数字写成输入框里的文字：整数不带小数点。
fn format_number(value: f64) -> String {
    if value.fract() == 0. && value.abs() < 1e15 { format!("{}", value as i64) } else { value.to_string() }
}

/// 依次访问树里的每个节点：`children`、卡片的 `actions`、标签页各页的 `children`。
fn visit_nodes(node: &Value, visit: &mut impl FnMut(&Value)) {
    visit(node);
    let lists = ["children", "actions", "left", "right"].into_iter().filter_map(|key| node.get(key).and_then(Value::as_array));
    let tabs =
        node.get("tabs").and_then(Value::as_array).into_iter().flatten().filter_map(|tab| tab.get("children").and_then(Value::as_array));
    for child in lists.chain(tabs).flatten() {
        visit_nodes(child, visit);
    }
}

/// 控件、文字的尺寸（`size`）：对应设计规范的控件高度 28 / 32 / 40。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeSize {
    Small,
    Medium,
    Large,
}

impl NodeSize {
    fn parse(value: Option<&str>) -> Option<Self> {
        match value? {
            "small" => Some(Self::Small),
            "medium" => Some(Self::Medium),
            "large" => Some(Self::Large),
            _ => None,
        }
    }

    /// 节点自己写的，否则外层 `row` 给的，否则这种控件现在的默认尺寸。
    fn of(node: &Value, inherited: Option<Self>, default: Self) -> Self {
        Self::parse(node.get("size").and_then(Value::as_str)).or(inherited).unwrap_or(default)
    }

    /// 组件库的控件（按钮、输入框）：`small` 是 `SM` 28，`medium` 是 `MD` 32（组件库的默认），`large` 是 `LG` 40。
    fn apply<T: Sizable + Styled>(self, element: T) -> T {
        match self {
            Self::Small => element.small().min_h(px(crate::design::size::CONTROL_SM)),
            Self::Medium => element.with_size(gpui_kit::component::Size::Medium).min_h(px(crate::design::size::CONTROL_MD)),
            Self::Large => element.large().min_h(px(crate::design::size::CONTROL_LG)),
        }
    }

    /// 控件旁的文字：说明、正文、稍大的正文。
    fn text<E: Styled>(self, element: E) -> E {
        match self {
            Self::Small => element.text_caption(),
            Self::Medium => element.text_body(),
            Self::Large => element.text_content(),
        }
    }

    fn height(self) -> f32 {
        match self {
            Self::Small => crate::design::size::CONTROL_SM,
            Self::Medium => crate::design::size::CONTROL_MD,
            Self::Large => crate::design::size::CONTROL_LG,
        }
    }

    /// 勾选框、单选圆点、图标。
    fn indicator(self) -> f32 {
        match self {
            Self::Small => crate::design::size::ICON_SM,
            Self::Medium => crate::design::size::ICON_MD,
            Self::Large => crate::design::size::ICON_LG,
        }
    }
}

/// 节点的 `tone` → 颜色；没写时是 `None`（用各自的默认色）。
fn tone_color(tone: Option<&str>, p: &Palette) -> Option<Hsla> {
    Some(match tone? {
        "neutral" => p.text,
        "secondary" => p.text_secondary,
        "accent" => p.accent,
        "success" => p.success,
        "warning" => p.warning,
        "danger" => p.danger,
        _ => return None,
    })
}

/// 节点的 `icon` → 图标（名单见 `dshr_agent::NODE_ICONS`）。
fn node_icon(name: &str) -> Option<IconName> {
    Some(match name {
        "star" => IconName::Star,
        "heart" => IconName::Heart,
        "bell" => IconName::Bell,
        "book" | "book-open" => IconName::BookOpen,
        "bot" => IconName::Bot,
        "calendar" => IconName::Calendar,
        "chart" | "chart-pie" => IconName::ChartPie,
        "cpu" => IconName::Cpu,
        "file" => IconName::File,
        "file-text" | "note" => IconName::FileText,
        "folder" => IconName::Folder,
        "globe" | "web" => IconName::Globe,
        "inbox" => IconName::Inbox,
        "map" => IconName::Map,
        "mic" => IconName::Mic,
        "moon" => IconName::Moon,
        "sun" => IconName::Sun,
        "network" => IconName::Network,
        "palette" => IconName::Palette,
        "search" => IconName::Search,
        "settings" => IconName::Settings2,
        "terminal" => IconName::SquareTerminal,
        "user" => IconName::User,
        "info" => IconName::Info,
        "check" => IconName::Check,
        "x" => IconName::Close,
        "plus" => IconName::Plus,
        "minus" => IconName::Minus,
        "trash" => IconName::Delete,
        "copy" => IconName::Copy,
        "refresh" => IconName::RefreshCw,
        "external" => IconName::ExternalLink,
        "warning" => IconName::TriangleAlert,
        "alert" => IconName::CircleAlert,
        "success" => IconName::CircleCheck,
        "error" => IconName::CircleX,
        "play" => IconName::Play,
        "pause" => IconName::Pause,
        "eye" => IconName::Eye,
        "chevron-right" => IconName::ChevronRight,
        "chevron-down" => IconName::ChevronDown,
        "arrow-up" => IconName::ArrowUp,
        "arrow-down" => IconName::ArrowDown,
        _ => return None,
    })
}

fn icon_of(node: &Value) -> Option<IconName> {
    node.get("icon").and_then(Value::as_str).and_then(node_icon)
}

/// 入口的图标名 → 图标；不认识的用默认的。
pub fn entry_icon(name: Option<&str>) -> IconName {
    name.and_then(node_icon).unwrap_or(IconName::LayoutDashboard)
}

impl Render for PluginPageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let body = match &self.tree {
            Some(tree) => {
                let tree = tree.clone();
                self.node(&tree, "0", None, cx)
            }
            None if self.error.is_none() => hint(t!("plugin_page.loading").to_string(), Tone::Neutral, &p).into_any_element(),
            None => div().into_any_element(),
        };
        v_flex()
            .gap(px(space::M))
            .children(self.error.clone().map(|error| {
                h_flex().gap(px(space::S)).child(div().flex_1().min_w(px(0.)).child(hint(error, Tone::Danger, &p))).child(
                    crate::design::framed(
                        "plugin-page-retry",
                        Button::new("plugin-page-retry")
                            .subtle(cx)
                            .control_xs()
                            .label(t!("plugin_page.retry").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                    ),
                )
            }))
            .children(self.message.clone().map(|message| hint(message, Tone::Success, &p)))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NodeSize, Seed, clamp, dropdown_items, entry_icon, filter_options, format_number, node_icon, options_of, shift_month, visit_nodes,
    };
    use gpui_kit::component::IconName;
    use serde_json::{Value, json};

    #[test]
    fn every_icon_the_validator_accepts_has_an_icon() {
        for name in dshr_agent::NODE_ICONS {
            assert!(node_icon(name).is_some(), "no icon for `{name}`");
        }
        assert!(matches!(entry_icon(Some("rocket")), IconName::LayoutDashboard));
        assert!(matches!(entry_icon(None), IconName::LayoutDashboard));
        assert!(matches!(entry_icon(Some("trash")), IconName::Delete));
    }

    #[test]
    fn controls_are_seeded_with_their_natural_types() {
        let seed = |node: Value| Seed::of(&node);
        assert!(matches!(seed(json!({ "type": "checkbox", "id": "a", "checked": true })), Some(Seed::Choice(Value::Bool(true)))));
        assert!(matches!(seed(json!({ "type": "checkbox", "id": "a" })), Some(Seed::Choice(Value::Bool(false)))));
        assert!(
            matches!(seed(json!({ "type": "radio", "id": "a", "value": "x" })), Some(Seed::Choice(Value::String(value))) if value == "x")
        );
        match seed(json!({ "type": "number", "id": "n", "value": 4, "min": 1, "max": 9 })) {
            Some(Seed::Input { value, number, .. }) => {
                assert_eq!(value, "4");
                assert_eq!(number, Some((Some(1.), Some(9.))));
            }
            _ => panic!("a number is an input with limits"),
        }
        assert!(matches!(seed(json!({ "type": "input", "id": "a" })), Some(Seed::Input { number: None, .. })));
        assert!(seed(json!({ "type": "text", "text": "a" })).is_none());
    }

    #[test]
    fn numbers_are_clamped_and_printed_without_noise() {
        assert_eq!(clamp(12., Some(0.), Some(10.)), 10.);
        assert_eq!(clamp(-1., Some(0.), None), 0.);
        assert_eq!(clamp(5., None, None), 5.);
        assert_eq!(format_number(3.), "3");
        assert_eq!(format_number(2.5), "2.5");
        assert_eq!(format_number(-0.25), "-0.25");
    }

    #[test]
    fn nested_controls_are_found_in_actions_and_tabs() {
        let tree = json!({ "type": "column", "children": [
            { "type": "card", "actions": [{ "type": "checkbox", "id": "in-actions", "label": "a" }], "children": [] },
            { "type": "tabs", "tabs": [
                { "id": "one", "label": "One", "children": [{ "type": "number", "id": "in-tab" }] },
                { "id": "two", "label": "Two" }
            ]},
            { "type": "input", "id": "plain" }
        ]});
        let mut ids = Vec::new();
        visit_nodes(&tree, &mut |node| {
            if let Some(id) = node.get("id").and_then(Value::as_str) {
                ids.push(id.to_owned());
            }
        });
        assert_eq!(ids, ["in-actions", "in-tab", "plain"]);
    }

    #[test]
    fn sizes_fall_back_to_the_row_then_the_default() {
        let node = json!({ "type": "button", "size": "large" });
        assert_eq!(NodeSize::of(&node, Some(NodeSize::Small), NodeSize::Medium), NodeSize::Large);
        let node = json!({ "type": "button" });
        assert_eq!(NodeSize::of(&node, Some(NodeSize::Small), NodeSize::Medium), NodeSize::Small);
        assert_eq!(NodeSize::of(&node, None, NodeSize::Medium), NodeSize::Medium);
        assert_eq!(NodeSize::Small.height(), 28.);
        assert_eq!(NodeSize::Medium.height(), 32.);
        assert_eq!(NodeSize::Large.height(), 40.);
    }

    #[test]
    fn options_accept_strings_and_objects() {
        let node = json!({ "options": ["a", { "value": "b", "label": "Bee" }, { "value": "c" }] });
        assert_eq!(
            options_of(&node),
            [("a".to_owned(), "a".to_owned()), ("b".to_owned(), "Bee".to_owned()), ("c".to_owned(), "c".to_owned())]
        );
    }

    #[test]
    fn second_round_controls_are_seeded() {
        assert!(
            matches!(Seed::of(&json!({ "type": "checkboxes", "id": "c", "value": ["a", "b"] })), Some(Seed::Choice(Value::Array(values))) if values.len() == 2)
        );
        assert!(matches!(Seed::of(&json!({ "type": "date", "id": "d" })), Some(Seed::Choice(Value::String(value))) if value.is_empty()));
        match Seed::of(&json!({ "type": "slider", "id": "s", "min": 0, "max": 10, "value": 20 })) {
            Some(Seed::Slider { value, min, max, step }) => assert_eq!((value, min, max, step), (10., 0., 10., 1.)),
            _ => panic!("a slider"),
        }
        assert!(
            matches!(Seed::of(&json!({ "type": "tags-input", "id": "t", "value": ["x"] })), Some(Seed::Tags { value, .. }) if value == ["x"])
        );
        assert!(matches!(
            Seed::of(&json!({ "type": "input", "id": "i", "kind": "password", "maxLength": 8 })),
            Some(Seed::Input { masked: true, max_length: Some(8), .. })
        ));
        assert!(matches!(
            Seed::of(&json!({ "type": "select", "id": "s", "options": ["a"], "style": "dropdown", "searchable": true })),
            Some(Seed::Dropdown { searchable: true, .. })
        ));
        assert!(matches!(Seed::of(&json!({ "type": "select", "id": "s", "options": ["a"] })), Some(Seed::Choice(_))));
    }

    #[test]
    fn dropdown_filters_keep_the_current_value() {
        let options =
            vec![("cn".to_owned(), "China".to_owned()), ("ch".to_owned(), "Switzerland".to_owned()), ("cl".to_owned(), "Chile".to_owned())];
        assert_eq!(filter_options(&options, "", "cn"), [0, 1, 2]);
        assert_eq!(filter_options(&options, " ch ", "cn"), [0, 2]);
        // 选中的那项筛不掉。
        assert_eq!(filter_options(&options, "ch", "ch"), [0, 1, 2]);
        assert_eq!(filter_options(&options, "zzz", "cl"), [2]);
        let shown = filter_options(&options, "ch", "cl");
        let (labels, selected) = dropdown_items(&options, &shown, "cl");
        assert_eq!(labels.len(), 2);
        assert_eq!(selected, Some(1));
        assert_eq!(dropdown_items(&options, &shown, "de").1, None);
    }

    #[test]
    fn months_shift_across_years() {
        assert_eq!(shift_month("2026-11", 1), "2026-12");
        assert_eq!(shift_month("2026-12", 1), "2027-01");
        assert_eq!(shift_month("2026-01", -1), "2025-12");
        assert_eq!(shift_month("2026-03", -14), "2025-01");
    }

    #[test]
    fn split_columns_are_visited() {
        let tree = json!({ "type": "split", "left": [{ "type": "input", "id": "l" }], "right": [{ "type": "slider", "id": "r", "min": 0, "max": 1 }] });
        let mut ids = Vec::new();
        visit_nodes(&tree, &mut |node| ids.extend(node.get("id").and_then(Value::as_str).map(str::to_owned)));
        assert_eq!(ids, ["l", "r"]);
    }
}
