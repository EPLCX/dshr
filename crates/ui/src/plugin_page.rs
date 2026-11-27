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
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{Disableable as _, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::design::ControlSize as _;
use crate::design::HoverFill as _;
use crate::design::RowColor as _;
use serde_json::{Map, Value, json};

use crate::backend::Backend;
use crate::design::{Tone, Typography as _, badge, card, divider, hint, mono_font, palette, pressable, radius, readable_with_id, space};
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
    /// 输入框：id → (状态, 上次从节点里写进去的值)。
    inputs: HashMap<String, (Entity<InputState>, String)>,
    /// 多行输入框，同上。
    textareas: HashMap<String, (Entity<TextareaState>, String)>,
    /// 选择框现在选中的值：id → (值, 上次从节点里写进去的值)。
    selects: HashMap<String, (String, String)>,
    /// 点过一次、等第二次确认的链接。
    pending_link: Option<String>,
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
            inputs: HashMap::new(),
            textareas: HashMap::new(),
            selects: HashMap::new(),
            pending_link: None,
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
        let mut inputs: Map<String, Value> =
            self.inputs.iter().map(|(id, (input, _))| (id.clone(), Value::String(input.read(cx).value().to_string()))).collect();
        for (id, (area, _)) in &self.textareas {
            inputs.insert(id.clone(), Value::String(area.read(cx).value().to_string()));
        }
        for (id, (value, _)) in &self.selects {
            inputs.insert(id.clone(), Value::String(value.clone()));
        }
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

    /// 树里每个输入框都要有一个状态；插件换了 `value` 时写进去（如提交后清空）。树里没有了的丢掉。
    fn sync_inputs(&mut self, tree: &Value, window: &mut Window, cx: &mut Context<Self>) {
        let mut areas = Vec::new();
        collect_nodes(tree, "textarea", &mut areas);
        self.textareas.retain(|id, _| areas.iter().any(|(other, _, _)| other == id));
        for (id, value, placeholder) in areas {
            match self.textareas.get_mut(&id) {
                Some((area, last)) => {
                    if *last != value {
                        area.update(cx, |area, cx| area.set_value(value.clone(), window, cx));
                        *last = value;
                    }
                }
                None => {
                    let area =
                        cx.new(|cx| TextareaState::new(window, cx).placeholder(placeholder).default_value(value.clone()).auto_grow(3, 12));
                    self.textareas.insert(id, (area, value));
                }
            }
        }
        let mut selects = Vec::new();
        collect_nodes(tree, "select", &mut selects);
        self.selects.retain(|id, _| selects.iter().any(|(other, _, _)| other == id));
        for (id, value, _) in selects {
            match self.selects.get_mut(&id) {
                Some((current, last)) if *last != value => {
                    *current = value.clone();
                    *last = value;
                }
                Some(_) => {}
                None => {
                    self.selects.insert(id, (value.clone(), value));
                }
            }
        }
        let mut seen = Vec::new();
        collect_nodes(tree, "input", &mut seen);
        self.inputs.retain(|id, _| seen.iter().any(|(other, _, _)| other == id));
        for (id, value, placeholder) in seen {
            match self.inputs.get_mut(&id) {
                Some((input, last)) => {
                    if *last != value {
                        input.update(cx, |input, cx| input.set_value(value.clone(), window, cx));
                        *last = value;
                    }
                }
                None => {
                    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value.clone()));
                    self.inputs.insert(id, (input, value));
                }
            }
        }
    }

    /// 画一个节点。能聚焦的控件包一层 [`reveal_in`]：Tab 走到页面下面看不见的地方时，页面跟着滚。
    fn node(&self, node: &Value, path: &str, cx: &mut Context<Self>) -> AnyElement {
        let element = self.node_content(node, path, cx);
        match node.get("type").and_then(Value::as_str) {
            Some("button" | "input" | "switch" | "textarea" | "select" | "link" | "calendar") => {
                reveal_in(&self.scroll, SharedString::from(format!("{}-{path}-reveal", self.plugin)), element).into_any_element()
            }
            _ => element,
        }
    }

    fn node_content(&self, node: &Value, path: &str, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let id = |suffix: &str| SharedString::from(format!("{}-{path}{suffix}", self.plugin));
        let string = |key: &str| node.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        // 插件画的文字都要进无障碍树，否则读屏软件看到的是一页空白。id 按节点路径给，保证唯一。
        if let Some(text) = node.as_str() {
            return div().text_body().child(readable_with_id(id(""), text.to_owned())).into_any_element();
        }
        let children = |this: &Self, cx: &mut Context<Self>| -> Vec<AnyElement> {
            node.get("children")
                .and_then(Value::as_array)
                .map(|children| {
                    children.iter().enumerate().map(|(index, child)| this.node(child, &format!("{path}.{index}"), cx)).collect()
                })
                .unwrap_or_default()
        };
        let gap = |default: f32| match node.get("gap").and_then(Value::as_str) {
            Some("small") => space::XS,
            Some("medium") => space::S,
            Some("large") => space::L,
            _ => default,
        };
        match node.get("type").and_then(Value::as_str).unwrap_or_default() {
            "column" => v_flex().gap(px(gap(space::M))).children(children(self, cx)).into_any_element(),
            "row" => h_flex().flex_wrap().items_center().gap(px(gap(space::S))).children(children(self, cx)).into_any_element(),
            "card" => {
                let title = node.get("title").and_then(Value::as_str).map(|title| {
                    div()
                        .id(id("-title"))
                        .role(Role::Heading)
                        .aria_level(2)
                        .aria_label(title.to_owned())
                        .text_heading()
                        .child(title.to_owned())
                });
                card(&p)
                    .p(px(space::L))
                    .child(v_flex().gap(px(gap(space::M))).children(title).children(children(self, cx)))
                    .into_any_element()
            }
            "text" => {
                let text = string("text");
                let color = match node.get("tone").and_then(Value::as_str) {
                    Some("accent") => p.accent,
                    Some("success") => p.success,
                    Some("danger") => p.danger,
                    _ => p.text,
                };
                let style = node.get("style").and_then(Value::as_str);
                // 标题样式的文字同时是标题，读屏软件可以按标题跳转。
                let level = match style {
                    Some("title") => Some(1),
                    Some("heading") => Some(2),
                    _ => None,
                };
                let element = match level {
                    Some(level) => {
                        div().id(id("")).role(Role::Heading).aria_level(level).aria_label(text.clone()).child(text).into_any_element()
                    }
                    None => readable_with_id(id(""), text).into_any_element(),
                };
                // 颜色、字号放在外层，“说明”样式才能盖过默认颜色。
                let element = div().text_color(color).child(element);
                match style {
                    Some("title") => element.text_title(),
                    Some("heading") => element.text_heading(),
                    Some("caption") => element.text_caption().when(node.get("tone").is_none(), |e| e.text_color(p.text_secondary)),
                    Some("mono") => element.text_body().text_mono(mono_font(cx)),
                    _ => element.text_body(),
                }
                .into_any_element()
            }
            "badge" => badge(string("text"), &p).into_any_element(),
            "divider" => divider(&p).into_any_element(),
            "button" => {
                let (action, value) = (string("action"), node.get("payload").cloned().unwrap_or(Value::Null));
                let button = Button::new(id("")).control_sm().label(string("label")).disabled(self.busy);
                let button = match node.get("variant").and_then(Value::as_str) {
                    Some("primary") => button.primary(),
                    Some("ghost") => button.subtle(cx),
                    Some("danger") => button.danger(),
                    _ => button,
                };
                div()
                    .child(
                        crate::design::framed(
                            id(""),
                            button.on_click(
                                cx.listener(move |this, _, window, cx| this.act(action.clone(), value.clone(), None, window, cx)),
                            ),
                        )
                        .inert(self.busy),
                    )
                    .into_any_element()
            }
            "input" => {
                let label = node.get("label").and_then(Value::as_str).map(str::to_owned);
                let Some((input, _)) = self.inputs.get(&string("id")) else { return div().into_any_element() };
                v_flex()
                    .gap(px(space::XS))
                    .min_w(px(200.))
                    .children(label.clone().map(|label| div().text_body().child(readable_with_id(id("-label"), label))))
                    .child(crate::design::framed_handle(
                        gpui_kit::Focusable::focus_handle(input.read(cx), cx),
                        Input::new(input).aria_label(label.unwrap_or_else(|| string("placeholder"))),
                    ))
                    .into_any_element()
            }
            "switch" => {
                let action = string("action");
                let value = node.get("payload").cloned().unwrap_or(Value::Null);
                let checked = node.get("checked").and_then(Value::as_bool).unwrap_or(false);
                let label = string("label");
                // 用应用自己的开关（和设置页一样）：组件库的开关没有高可见性轮廓的外圈。
                // 名字给开关本身，读屏软件读“<名字>，开关，开 / 关”；旁边的文字只给眼睛看。
                h_flex()
                    .gap(px(space::S))
                    .items_center()
                    .child(crate::design::switch(id(""), checked, &p).accessibility_label(label.clone()).disabled(self.busy).on_click(
                        cx.listener(move |this, _, window, cx| this.act(action.clone(), value.clone(), Some(!checked), window, cx)),
                    ))
                    .child(div().text_body().when(self.busy, |text| text.opacity(0.5)).child(label))
                    .into_any_element()
            }
            "list" => {
                let items = node.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
                v_flex()
                    .gap(px(2.))
                    .children(items.into_iter().enumerate().map(|(index, item)| {
                        let title = item.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
                        let detail = item.get("detail").and_then(Value::as_str).map(str::to_owned);
                        let action = item.get("action").and_then(Value::as_str).map(str::to_owned);
                        let value = item.get("payload").cloned().unwrap_or(Value::Null);
                        let item_id = id(&format!("-item-{index}"));
                        let content = |row: Div| {
                            row.px(px(space::M))
                                .py(px(space::S))
                                .gap(px(space::M))
                                .rounded(px(radius::MD))
                                .child(div().flex_1().min_w(px(0.)).text_body().child(title.clone()))
                                .children(detail.clone().map(|detail| div().text_caption().row_color(p.text_secondary, &p).child(detail)))
                        };
                        match action {
                            // 能点的行是按钮：Tab 能走到，Enter / 空格触发；读屏软件读标题，详情作补充说明。
                            Some(action) if !self.busy => reveal_in(
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
                                    .on_click(
                                        cx.listener(move |this, _, window, cx| this.act(action.clone(), value.clone(), None, window, cx)),
                                    )
                                    .child(content(h_flex().flex_1().min_w(px(0.)))),
                            )
                            .into_any_element(),
                            // 不能点的行：文字直接进无障碍树。
                            _ => {
                                let text = match &detail {
                                    Some(detail) => format!("{title}，{detail}"),
                                    None => title.clone(),
                                };
                                div().id(item_id).role(Role::Label).aria_value(text).child(content(h_flex())).into_any_element()
                            }
                        }
                    }))
                    .into_any_element()
            }
            "textarea" => {
                let label = node.get("label").and_then(Value::as_str).map(str::to_owned);
                let Some((area, _)) = self.textareas.get(&string("id")) else { return div().into_any_element() };
                v_flex()
                    .gap(px(space::XS))
                    .w_full()
                    .children(label.clone().map(|label| div().text_body().child(readable_with_id(id("-label"), label))))
                    .child(crate::design::framed_handle(
                        gpui_kit::Focusable::focus_handle(area.read(cx), cx),
                        Textarea::new(area).aria_label(label.unwrap_or_else(|| string("placeholder"))),
                    ))
                    .into_any_element()
            }
            // 选择框：选项不多时一排按钮最直接，读屏软件也能逐个读出；选中的那个是主按钮。
            "select" => {
                let select = string("id");
                let current = self.selects.get(&select).map(|(value, _)| value.clone()).unwrap_or_default();
                let action = node.get("action").and_then(Value::as_str).map(str::to_owned);
                let options = node.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
                let label = node.get("label").and_then(Value::as_str).map(str::to_owned);
                let buttons = options.into_iter().enumerate().map(|(index, option)| {
                    let value = option.as_str().or_else(|| option.get("value").and_then(Value::as_str)).unwrap_or_default().to_owned();
                    let text = option.get("label").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| value.clone());
                    let button = Button::new(id(&format!("-option-{index}"))).control_sm().label(text).disabled(self.busy);
                    let button = if value == current { button.primary() } else { button };
                    let (select, action) = (select.clone(), action.clone());
                    crate::design::framed(
                        id(&format!("-option-{index}")),
                        button.on_click(cx.listener(move |this, _, window, cx| {
                            if let Some((selected, _)) = this.selects.get_mut(&select) {
                                *selected = value.clone();
                            }
                            match &action {
                                Some(action) => this.act(action.clone(), Value::String(value.clone()), None, window, cx),
                                None => cx.notify(),
                            }
                        })),
                    )
                    .inert(self.busy)
                });
                v_flex()
                    .gap(px(space::XS))
                    .children(label.map(|label| div().text_body().child(readable_with_id(id("-label"), label))))
                    .child(h_flex().flex_wrap().gap(px(space::XS)).children(buttons))
                    .into_any_element()
            }
            // 链接：点一次先显示要打开的地址，再点一次才用系统浏览器打开。
            "link" => {
                let url = string("url");
                let confirming = self.pending_link.as_deref() == Some(url.as_str());
                let label = if confirming { t!("plugin_page.open_link_confirm", url = url).to_string() } else { string("label") };
                crate::design::framed(
                    id(""),
                    Button::new(id(""))
                        .control_sm()
                        .subtle(cx)
                        .icon(IconName::ExternalLink)
                        .label(label)
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
        "table" => {
            let cell_text = |cell: &Value| match cell {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            let columns: Vec<String> =
                node.get("columns").and_then(Value::as_array).map(|columns| columns.iter().map(cell_text).collect()).unwrap_or_default();
            let rows = node.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
            let row = |cells: Vec<String>, header: bool, index: usize| {
                h_flex()
                    .w_full()
                    .gap(px(space::M))
                    .px(px(space::M))
                    .py(px(space::XS))
                    .when(header, |row| row.text_label().text_color(p.text_secondary))
                    .when(!header, |row| row.text_body())
                    .children(cells.into_iter().enumerate().map(move |(column, text)| {
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .child(readable_with_id(SharedString::from(format!("{id}-cell-{index}-{column}")), text))
                    }))
            };
            v_flex()
                .w_full()
                .rounded(px(radius::MD))
                .border_1()
                .border_color(p.border)
                .child(row(columns, true, 0))
                .children(rows.iter().enumerate().map(|(index, cells)| {
                    let cells = cells.as_array().map(|cells| cells.iter().map(cell_text).collect()).unwrap_or_default();
                    div().border_t_1().border_color(p.border).child(row(cells, false, index + 1))
                }))
                .into_any_element()
        }
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
            .children(mark.map(|mark| div().text_caption().truncate().row_color(p.accent, &p).child(mark)));
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
        let string = |key: &str| node.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        let children = || -> Vec<AnyElement> {
            node.get("children")
                .and_then(Value::as_array)
                .map(|children| children.iter().enumerate().map(|(index, child)| walk(child, &format!("{path}.{index}"), cx)).collect())
                .unwrap_or_default()
        };
        match node.get("type").and_then(Value::as_str).unwrap_or_default() {
            "column" => v_flex().gap(px(space::S)).children(children()).into_any_element(),
            "row" => h_flex().flex_wrap().items_center().gap(px(space::S)).children(children()).into_any_element(),
            "card" => card(&p)
                .p(px(space::M))
                .child(
                    v_flex()
                        .gap(px(space::S))
                        .children(node.get("title").and_then(Value::as_str).map(|title| div().text_label().child(title.to_owned())))
                        .children(children()),
                )
                .into_any_element(),
            "text" => {
                let color = match node.get("tone").and_then(Value::as_str) {
                    Some("accent") => p.accent,
                    Some("success") => p.success,
                    Some("danger") => p.danger,
                    _ => p.text,
                };
                let element = div().text_color(color).child(readable_with_id(id, string("text")));
                match node.get("style").and_then(Value::as_str) {
                    Some("title" | "heading") => element.text_heading(),
                    Some("caption") => element.text_caption(),
                    Some("mono") => element.text_body().text_mono(mono_font(cx)),
                    _ => element.text_body(),
                }
                .into_any_element()
            }
            "badge" => badge(string("text"), &p).into_any_element(),
            "divider" => divider(&p).into_any_element(),
            "list" => v_flex()
                .gap(px(2.))
                .children(node.get("items").and_then(Value::as_array).into_iter().flatten().enumerate().map(|(index, item)| {
                    let title = item.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
                    let detail = item.get("detail").and_then(Value::as_str).map(str::to_owned);
                    h_flex()
                        .gap(px(space::M))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_body()
                                .child(readable_with_id(SharedString::from(format!("{path}-item-{index}")), title)),
                        )
                        .children(detail.map(|detail| div().text_caption().text_color(p.text_secondary).child(detail)))
                }))
                .into_any_element(),
            // 能操作的节点：只读的地方只显示文字。
            "button" | "switch" | "link" => hint(string("label"), Tone::Neutral, &p).into_any_element(),
            "input" | "textarea" | "select" => {
                let text = [string("label"), string("value")].into_iter().filter(|text| !text.is_empty()).collect::<Vec<_>>().join("：");
                hint(text, Tone::Neutral, &p).into_any_element()
            }
            kind => display_node(node, &id, cx)
                .unwrap_or_else(|| hint(t!("plugin_page.unknown_node", kind = kind).to_string(), Tone::Danger, &p).into_any_element()),
        }
    }
    walk(node, prefix, cx)
}

/// 树里某种节点（`input`、`textarea`、`select`）：(id, value, placeholder)。
fn collect_nodes(node: &Value, kind: &str, found: &mut Vec<(String, String, String)>) {
    if node.get("type").and_then(Value::as_str) == Some(kind)
        && let Some(id) = node.get("id").and_then(Value::as_str)
    {
        let text = |key: &str| node.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
        found.push((id.to_owned(), text("value"), text("placeholder")));
    }
    for child in node.get("children").and_then(Value::as_array).into_iter().flatten() {
        collect_nodes(child, kind, found);
    }
}

/// 入口的图标名 → 图标；不认识的用默认的。
pub fn entry_icon(name: Option<&str>) -> IconName {
    match name.unwrap_or_default() {
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
        _ => IconName::LayoutDashboard,
    }
}

impl Render for PluginPageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let body = match &self.tree {
            Some(tree) => {
                let tree = tree.clone();
                self.node(&tree, "0", cx)
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
