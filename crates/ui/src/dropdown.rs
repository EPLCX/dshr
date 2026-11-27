//! 下拉选择框（组合框）。
//!
//! 组件库的 `Select` 把弹出列表放在延后绘制层里，列表打开时按 Tab，焦点按绘制顺序跳到页面别处，
//! 列表随之关闭，读屏用户会觉得焦点“飘走了”；它的内部状态也不公开，没法从外面修。
//! 这里自己实现一个，键盘行为按 Windows 标准组合框：
//!
//! - 收起时：↑ / ↓ / Home / End 直接改选中项（不展开），Alt+↓、F4、Enter、空格展开。
//!   macOS 上按系统弹出按钮的习惯，↑ / ↓ 是展开列表（VoiceOver 用户按 VO+空格 或 ↓ 打开）。
//! - 展开时：焦点在当前项上，↑ / ↓ / Home / End 移动，Enter / 空格选中并收起，Esc 收起不改，
//!   Tab / Shift+Tab 选中当前项、收起，再把焦点移到组合框的下一个 / 上一个控件——不会飘走。
//!
//! 无障碍：触发器是“组合框”，名称是调用方给的标签（带上所属区块），值是当前选项，读出展开状态；
//! 列表是“列表框”，每项是“列表项”，读出“第几项，共几项”和是否已选择。macOS 上列表是“菜单”、每项是
//! “单选菜单项”，原因见 [`LIST_ROLE`]。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::{Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::design::motion::{self, Edge};
use crate::design::{Typography as _, high_visibility_outlines, palette, radius, shadow, size, space};

actions!(
    dshr_dropdown,
    [
        /// 收起时：选上一项。展开时：高亮上一项。
        DropdownUp,
        /// 收起时：选下一项。展开时：高亮下一项。
        DropdownDown,
        DropdownFirst,
        DropdownLast,
        /// 展开列表（收起时），或选中高亮项（展开时）。
        DropdownConfirm,
        /// 收起列表，不改选中项。
        DropdownCancel,
        /// 展开时：选中高亮项，焦点移到下一个控件。
        DropdownTabNext,
        /// 展开时：选中高亮项，焦点移到上一个控件。
        DropdownTabPrev,
    ]
);

const TRIGGER_CONTEXT: &str = "AppDropdown";

/// 展开的列表在无障碍树里的角色。
///
/// macOS 上是“菜单”，选项是“单选菜单项”，用勾选状态（`aria_toggled`）表示当前选项，和系统弹出按钮
/// （`NSPopUpButton`）以及本程序其他弹出菜单（[`crate::design::menu_choice`]）一致。
/// 不能用“列表框 + `aria_selected`”：那样选项在 VoiceOver 眼里是可选择的列表行（accesskit 允许
/// `setAccessibilitySelected:`，并把它翻译成点击），VoiceOver 光标一落到某一项（鼠标碰到、或焦点移过去）
/// 就选中它，等于直接点了这一项：列表收起，值也改了。
///
/// 其他平台照旧是列表框和列表项：讲述人通过 UIA 的 SelectionItem 读出“已选择”，没有这个问题。
const LIST_ROLE: Role = if cfg!(target_os = "macos") { Role::Menu } else { Role::ListBox };

/// 选项的角色和“是否当前选项”（见 [`LIST_ROLE`]）。
fn option_semantics(option: Stateful<Div>, selected: bool) -> Stateful<Div> {
    if cfg!(target_os = "macos") {
        option.role(Role::MenuItemRadio).aria_toggled(if selected { Toggled::True } else { Toggled::False })
    } else {
        option.role(Role::ListBoxOption).aria_selected(selected)
    }
}
const LIST_CONTEXT: &str = "AppDropdownList";

/// 注册按键。比组件库在根上注册的 Tab 更深一层，所以列表展开时 Tab 先到这里。
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", DropdownUp, Some(TRIGGER_CONTEXT)),
        KeyBinding::new("down", DropdownDown, Some(TRIGGER_CONTEXT)),
        KeyBinding::new("home", DropdownFirst, Some(TRIGGER_CONTEXT)),
        KeyBinding::new("end", DropdownLast, Some(TRIGGER_CONTEXT)),
        KeyBinding::new("alt-down", DropdownConfirm, Some(TRIGGER_CONTEXT)),
        KeyBinding::new("f4", DropdownConfirm, Some(TRIGGER_CONTEXT)),
        KeyBinding::new("enter", DropdownConfirm, Some(TRIGGER_CONTEXT)),
        KeyBinding::new("space", DropdownConfirm, Some(TRIGGER_CONTEXT)),
        KeyBinding::new("up", DropdownUp, Some(LIST_CONTEXT)),
        KeyBinding::new("down", DropdownDown, Some(LIST_CONTEXT)),
        KeyBinding::new("home", DropdownFirst, Some(LIST_CONTEXT)),
        KeyBinding::new("end", DropdownLast, Some(LIST_CONTEXT)),
        KeyBinding::new("enter", DropdownConfirm, Some(LIST_CONTEXT)),
        KeyBinding::new("space", DropdownConfirm, Some(LIST_CONTEXT)),
        KeyBinding::new("alt-up", DropdownConfirm, Some(LIST_CONTEXT)),
        KeyBinding::new("escape", DropdownCancel, Some(LIST_CONTEXT)),
        KeyBinding::new("tab", DropdownTabNext, Some(LIST_CONTEXT)),
        KeyBinding::new("shift-tab", DropdownTabPrev, Some(LIST_CONTEXT)),
    ]);
}

/// 选中项变了（用户操作才发，`set_items` / `set_selected` 不发）。
pub struct DropdownChanged(pub usize);

pub struct DropdownState {
    items: Vec<SharedString>,
    /// 每一项属于哪一组（如模型属于哪个提供商）；没有分组时为空。列表里每组上面画组名，
    /// 选项只写自己的名字——长的“提供商 · 模型”在窄的列表里会把真正区分它们的模型名截掉。
    groups: Vec<Option<SharedString>>,
    selected: Option<usize>,
    open: bool,
    /// 展开时高亮（带键盘焦点）的项。
    highlighted: usize,
    /// 读屏软件读出的名称，如“新会话默认设置，默认模型”。
    label: SharedString,
    placeholder: SharedString,
    width: Pixels,
    trigger: FocusHandle,
    options: Vec<FocusHandle>,
    /// 读屏软件在名称、值、展开状态之后读的说明（设置行的那句说明）。
    description: SharedString,
    /// 刚展开：等列表画出来再把焦点放到当前项上（见 `open`）。
    focus_pending: bool,
    /// 不能改时的原因（如“高对比度开启时由它决定配色”）。仍可聚焦，读屏用户能找到它、听到原因；
    /// 不展开、方向键不改选中项，看起来是淡的。
    disabled: Option<SharedString>,
    /// 上一帧触发框在窗口里的位置（绘制时记下），展开时据此决定列表往下还是往上。
    trigger_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 上一帧展开的列表在窗口里的位置（绘制时记下）。
    list_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl EventEmitter<DropdownChanged> for DropdownState {}

/// 列表最高这么多，再多就滚动。
const MAX_LIST_HEIGHT: f32 = 320.;
/// 列表最宽这么多（还不超过窗口）：至少和触发框一样宽，选项长就跟着变宽，不把选项截断。
const MAX_LIST_WIDTH: f32 = 520.;
/// 分组时组名那一行的高度（`caption` 一行加上下内边距）。
const GROUP_HEADER_HEIGHT: f32 = 18. + space::S + space::XS;
/// 列表和触发框之间的空隙。
const LIST_GAP: f32 = space::XS;
/// 列表离窗口边缘至少这么远。
const WINDOW_MARGIN: f32 = space::S;

/// 列表朝哪边展开、最高多少：下面放得下就往下；放不下、上面又更宽就往上；高度不超过那一侧剩下的空间，
/// 超出的部分滚动。列表不会盖住触发框（以前贴着窗口底边时会被整个推上去压住它）。
/// 参数都是窗口坐标；返回（是否往下，最大高度）。
pub(crate) fn place(trigger_top: f32, trigger_bottom: f32, window_height: f32, wanted: f32) -> (bool, f32) {
    let below = (window_height - trigger_bottom - LIST_GAP - WINDOW_MARGIN).max(0.);
    let above = (trigger_top - LIST_GAP - WINDOW_MARGIN).max(0.);
    if wanted <= below || below >= above { (true, wanted.min(below)) } else { (false, wanted.min(above)) }
}

impl DropdownState {
    pub fn new(items: Vec<SharedString>, selected: Option<usize>, cx: &mut Context<Self>) -> Self {
        let mut state = Self {
            items: Vec::new(),
            groups: Vec::new(),
            selected: None,
            open: false,
            highlighted: 0,
            label: SharedString::default(),
            placeholder: SharedString::default(),
            width: px(240.),
            trigger: cx.focus_handle().tab_stop(true),
            options: Vec::new(),
            disabled: None,
            description: SharedString::default(),
            focus_pending: false,
            trigger_bounds: Rc::default(),
            list_bounds: Rc::default(),
        };
        state.set_items(items, selected, cx);
        state
    }

    pub fn width(mut self, width: f32) -> Self {
        self.width = px(width);
        self
    }

    /// 换标签（切换语言之后）。
    pub fn set_label(&mut self, label: impl Into<SharedString>, cx: &mut Context<Self>) {
        let label = label.into();
        if self.label != label {
            self.label = label;
            cx.notify();
        }
    }

    pub fn set_items(&mut self, items: Vec<SharedString>, selected: Option<usize>, cx: &mut Context<Self>) {
        while self.options.len() < items.len() {
            self.options.push(cx.focus_handle());
        }
        self.options.truncate(items.len());
        self.items = items;
        self.groups.clear();
        self.selected = selected.filter(|index| *index < self.items.len());
        self.highlighted = self.selected.unwrap_or(0);
        if self.items.is_empty() {
            self.open = false;
        }
        cx.notify();
    }

    /// 分了组的选项：(组名, 选项名)。列表按组画组名，选项和触发框里模型名优先显示，组名放不下时先截组名；
    /// 读屏软件读“组名 · 选项名”。内容没变时什么也不做，可以在渲染时每次调用。
    pub fn set_grouped_items(&mut self, items: Vec<(SharedString, SharedString)>, selected: Option<usize>, cx: &mut Context<Self>) {
        let (groups, labels): (Vec<_>, Vec<_>) = items.into_iter().map(|(group, label)| (Some(group), label)).unzip();
        if self.groups == groups && self.items == labels && self.selected == selected {
            return;
        }
        self.set_items(labels, selected, cx);
        self.groups = groups;
    }

    /// 第 `index` 项的完整名字：有分组时是“组名 · 选项名”。读屏软件读它。
    fn full_label(&self, index: usize) -> Option<SharedString> {
        let item = self.items.get(index)?;
        Some(match self.groups.get(index).cloned().flatten() {
            Some(group) => format!("{group} · {item}").into(),
            None => item.clone(),
        })
    }

    /// 第 `index` 项上面要不要画组名（它是这一组的第一项）。
    fn starts_group(&self, index: usize) -> Option<SharedString> {
        let group = self.groups.get(index).cloned().flatten()?;
        let previous = index.checked_sub(1).and_then(|previous| self.groups.get(previous).cloned().flatten());
        (previous.as_ref() != Some(&group)).then_some(group)
    }

    /// 换选项的文字（切换语言后），选中项不变。文字没变时什么也不做，可以在渲染时每次调用。
    pub fn set_item_labels(&mut self, items: Vec<SharedString>, cx: &mut Context<Self>) {
        if self.items != items {
            let selected = self.selected;
            self.set_items(items, selected, cx);
        }
    }

    /// 选中某一项（来自配置，不发事件）。没变时什么也不做。
    pub fn set_selected(&mut self, selected: Option<usize>, cx: &mut Context<Self>) {
        let selected = selected.filter(|index| *index < self.items.len());
        if self.selected != selected {
            self.selected = selected;
            self.highlighted = selected.unwrap_or(0);
            cx.notify();
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected_item(&self) -> Option<&SharedString> {
        self.selected.and_then(|index| self.items.get(index))
    }

    /// 换说明（切换语言之后）。没变时什么也不做，可以在渲染时每次调用。
    pub fn set_description(&mut self, description: impl Into<SharedString>, cx: &mut Context<Self>) {
        let description = description.into();
        if self.description != description {
            self.description = description;
            cx.notify();
        }
    }

    /// 设为不能改（`Some(原因)`）或恢复。展开着时收起。没变时什么也不做，可以在渲染时每次调用。
    pub fn set_disabled(&mut self, reason: Option<SharedString>, cx: &mut Context<Self>) {
        if self.disabled != reason {
            self.disabled = reason;
            if self.disabled.is_some() {
                self.open = false;
            }
            cx.notify();
        }
    }

    /// 触发器的焦点（用来把焦点放到这个组合框上）。
    pub fn focus_handle(&self) -> FocusHandle {
        self.trigger.clone()
    }

    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.items.is_empty() || self.disabled.is_some() {
            return;
        }
        self.open = true;
        self.highlighted = self.selected.unwrap_or(0).min(self.items.len() - 1);
        // 不马上把焦点给当前项：那一项这时还没画出来，无障碍树里没有它，讲述人收不到这次焦点变化，
        // 第一项读不出来，按 ↓ 才从第二项开始读。等列表画出来再给（`render_list` 里登记）。
        self.focus_pending = true;
        let _ = window;
        cx.notify();
    }

    /// 列表画出来了：把焦点放到当前项上。
    fn focus_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open && std::mem::take(&mut self.focus_pending) {
            if let Some(option) = self.options.get(self.highlighted) {
                option.focus(window, cx);
            }
            cx.notify();
        }
    }

    /// 收起并把焦点放回触发器。
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self.focus_pending = false;
        self.trigger.focus(window, cx);
        cx.notify();
    }

    fn choose(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.items.len() && self.selected != Some(index) {
            self.selected = Some(index);
            cx.emit(DropdownChanged(index));
        }
        cx.notify();
    }

    /// 鼠标移到展开列表的某一项上：它成为高亮项并获得焦点（读屏软件读出它）。
    /// 已经是高亮项时什么也不做，鼠标在同一项里移动不会重复播报。触屏上没有悬停。
    fn hover(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open || index >= self.options.len() || window.last_input_was_touch() {
            return;
        }
        if self.highlighted != index || !self.options[index].is_focused(window) {
            self.highlighted = index;
            self.options[index].focus(window, cx);
            cx.notify();
        }
    }

    /// 按方向键：收起时直接改选中项，展开时移动高亮。
    fn step(&mut self, target: impl Fn(usize, usize) -> usize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.items.len();
        if count == 0 || self.disabled.is_some() {
            return;
        }
        if self.open {
            self.highlighted = target(self.highlighted, count).min(count - 1);
            self.focus_pending = false;
            self.options[self.highlighted].focus(window, cx);
            cx.notify();
        } else if cfg!(target_os = "macos") {
            self.open(window, cx);
        } else {
            let current = self.selected.unwrap_or(0);
            let next = target(current, count).min(count - 1);
            if self.selected.is_none() {
                self.choose(0, cx);
            } else {
                self.choose(next, cx);
            }
        }
    }

    fn on_up(&mut self, _: &DropdownUp, window: &mut Window, cx: &mut Context<Self>) {
        self.step(|index, _| index.saturating_sub(1), window, cx);
    }

    fn on_down(&mut self, _: &DropdownDown, window: &mut Window, cx: &mut Context<Self>) {
        self.step(|index, count| (index + 1).min(count - 1), window, cx);
    }

    fn on_first(&mut self, _: &DropdownFirst, window: &mut Window, cx: &mut Context<Self>) {
        self.step(|_, _| 0, window, cx);
    }

    fn on_last(&mut self, _: &DropdownLast, window: &mut Window, cx: &mut Context<Self>) {
        self.step(|_, count| count - 1, window, cx);
    }

    fn on_confirm(&mut self, _: &DropdownConfirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.choose(self.highlighted, cx);
            self.close(window, cx);
        } else {
            self.open(window, cx);
        }
    }

    fn on_cancel(&mut self, _: &DropdownCancel, window: &mut Window, cx: &mut Context<Self>) {
        self.close(window, cx);
    }

    fn on_tab_next(&mut self, _: &DropdownTabNext, window: &mut Window, cx: &mut Context<Self>) {
        self.choose(self.highlighted, cx);
        self.close(window, cx);
        window.focus_next(cx);
    }

    fn on_tab_prev(&mut self, _: &DropdownTabPrev, window: &mut Window, cx: &mut Context<Self>) {
        self.choose(self.highlighted, cx);
        self.close(window, cx);
        window.focus_prev(cx);
    }

    /// 展开的列表。`phase` 是进场进度：从展开的方向淡入并移过来一点；收起时立即移除。
    fn render_list(&self, phase: motion::Phase, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let p = palette(cx);
        let count = self.items.len();
        // 列表本来要多高：每项一行，加上行间距、内边距和边框。
        let rows = count as f32;
        let row_height = size::LIST_ITEM;
        let headers = (0..count).filter(|index| self.starts_group(*index).is_some()).count() as f32;
        let wanted = (rows * row_height + headers * GROUP_HEADER_HEIGHT + space::XS * 2. + 2.).min(MAX_LIST_HEIGHT);
        let max_width = px(MAX_LIST_WIDTH).min(window.viewport_size().width - px(WINDOW_MARGIN * 2.)).max(self.width);
        let (down, max_height) = match self.trigger_bounds.get() {
            Some(bounds) => place(bounds.top().as_f32(), bounds.bottom().as_f32(), window.viewport_size().height.as_f32(), wanted),
            None => (true, wanted),
        };
        let trigger_height = self.trigger_bounds.get().map_or(size::CONTROL_MD, |bounds| bounds.size.height.as_f32());
        let view = cx.entity().downgrade();
        let options = self
            .items
            .iter()
            .enumerate()
            .flat_map(|(index, item)| {
                let selected = self.selected == Some(index);
                let highlighted = self.highlighted == index;
                let hover_bg = p.selected;
                let header = self.starts_group(index).map(|group| {
                    // 组名只是给眼睛看的（每一项的名字里已经有它），不进无障碍树。
                    div()
                        .flex_none()
                        // 和选项文字左对齐（选项左右内边距 8，加 2px 的焦点框）。
                        .px(px(space::S + 2.))
                        .pt(px(space::S))
                        .pb(px(space::XS))
                        .text_caption()
                        .text_color(p.text_secondary)
                        .truncate()
                        .child(group.to_string())
                });
                let option = div()
                    .id(("dropdown-option", index))
                    .flex_none()
                    .track_focus(&self.options[index])
                    .map(|option| option_semantics(option, selected))
                    .aria_label(self.full_label(index).unwrap_or_else(|| item.clone()))
                    // accesskit 的位置从 0 数，读屏时各平台的适配层再加 1（以前在这里先加了 1，读出来多一个：第一项读成“项 2”）。
                    .aria_position_in_set(index)
                    .aria_size_of_set(count)
                    .flex()
                    .items_center()
                    .gap(px(space::S))
                    .min_h(px(row_height))
                    .px(px(space::S))
                    .rounded(px(radius::SM))
                    .text_body()
                    .when(selected, |option| option.font_weight(FontWeight::MEDIUM))
                    .cursor_pointer()
                    .border_2()
                    .border_color(transparent_black())
                    .relative()
                    // 高可见性轮廓：点击聚焦也显示轮廓，外面再画一圈（默认只随键盘焦点出现）。
                    .map(|option| crate::design::with_focus_ring(option, self.options[index].is_focused(window), radius::SM, window, &p))
                    // 高亮项的底色有过渡（铺到焦点框外沿）。悬停就是高亮（见 `hover`），只有一项亮着。
                    .child(motion::highlight("option-fill", hover_bg, radius::SM, highlighted).without_hover().outset(2.))
                    // 和系统组合框一样：鼠标移到哪一项，哪一项就是高亮项并获得焦点，读屏软件随之读出它
                    // （不只是键盘）。之后按 Enter、方向键也从这一项接着走。
                    .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, window, cx| this.hover(index, window, cx)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.choose(index, cx);
                        this.close(window, cx);
                    }))
                    // 按文字本身的宽度排（不用 `flex_1`：那样列表算不出选项要多宽，只能截断）。
                    .child(div().flex_grow(1.).min_w(px(0.)).truncate().child(item.clone()))
                    // 刚展开时，当前项画出来之后再给它焦点（见 `open`）。
                    .when(highlighted && self.focus_pending, |option| {
                        let view = view.clone();
                        option.child(crate::design::focus_after_paint(move |window, cx| {
                            view.update(cx, |this, cx| this.focus_highlighted(window, cx)).ok();
                        }))
                    })
                    .when(selected, |option| option.child(Icon::new(IconName::Check).size(px(size::ICON_SM)).text_color(p.accent)));
                [header.map(IntoElement::into_any_element), Some(option.into_any_element())]
            })
            .flatten();
        // 以触发框左上角为基准，让菜单与触发框之间留出 LIST_GAP。
        let anchor = if down {
            anchored().anchor(Anchor::TopLeft).offset(point(px(0.), px(trigger_height + LIST_GAP)))
        } else {
            anchored().anchor(Anchor::BottomLeft).offset(point(px(0.), px(-LIST_GAP)))
        };
        deferred(
            anchor.snap_to_window_with_margin(px(WINDOW_MARGIN)).child(
                v_flex()
                    .id("dropdown-list")
                    .key_context(LIST_CONTEXT)
                    .role(LIST_ROLE)
                    .aria_label(self.label.clone())
                    .occlude()
                    .min_w(self.width)
                    .max_w(max_width)
                    .max_h(px(max_height))
                    .overflow_y_scroll()
                    .p(px(space::XS))
                    .rounded(px(radius::LG))
                    .border_1()
                    .border_color(p.border)
                    .bg(p.raised)
                    .text_color(p.text)
                    .map(|list| motion::pop(list, if down { Edge::Top } else { Edge::Bottom }, motion::NUDGE, phase, shadow::menu(&p)))
                    .on_action(cx.listener(Self::on_up))
                    .on_action(cx.listener(Self::on_down))
                    .on_action(cx.listener(Self::on_first))
                    .on_action(cx.listener(Self::on_last))
                    .on_action(cx.listener(Self::on_confirm))
                    .on_action(cx.listener(Self::on_cancel))
                    .on_action(cx.listener(Self::on_tab_next))
                    .on_action(cx.listener(Self::on_tab_prev))
                    .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                        if this.open {
                            this.close(window, cx);
                        }
                    }))
                    .children(options)
                    .child({
                        let bounds = self.list_bounds.clone();
                        canvas(move |area, _, _| bounds.set(Some(area)), |_, _, _, _| {}).absolute().top_0().left_0().size_full()
                    }),
            ),
        )
        .with_priority(1)
    }
}

impl Render for DropdownState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let value = self.selected_item().cloned();
        let group = self.selected.and_then(|index| self.groups.get(index).cloned().flatten());
        // 读屏软件读完整的“组名 · 选项名”；眼睛看到的组名放不下时先截组名，选项名尽量完整。
        let shown = self.selected.and_then(|index| self.full_label(index)).unwrap_or_else(|| self.placeholder.clone());
        let ring = p.focus_ring;
        let bounds = self.trigger_bounds.clone();
        let trigger = h_flex()
            .id("dropdown-trigger")
            .relative()
            .key_context(TRIGGER_CONTEXT)
            .track_focus(&self.trigger)
            .role(Role::ComboBox)
            .aria_label(self.label.clone())
            .aria_value(shown.clone())
            .aria_expanded(self.open)
            .when(!self.description.is_empty(), |trigger| trigger.aria_description(self.description.clone()))
            .when_some(self.disabled.clone(), |trigger, reason| trigger.aria_description(reason).opacity(0.5).cursor_default())
            .w(self.width)
            .max_w_full()
            .min_h(px(size::CONTROL_MD))
            .px(px(10.))
            .gap(px(space::S))
            .items_center()
            .rounded(px(radius::MD))
            .border_1()
            .border_color(p.control_border)
            .bg(p.surface)
            .text_body()
            .when(self.disabled.is_none(), |trigger| {
                // 悬停、按下时叠一层底色（画在内容下面），和按钮一样即时生效。
                trigger.cursor_pointer().child(
                    motion::highlight("trigger-fill", transparent_black(), radius::MD, false).hover_color(p.hover).pressed_color(p.pressed),
                )
            })
            .when(self.open, |trigger| trigger.border_color(ring))
            .when(high_visibility_outlines() && self.trigger.is_focused(window), |trigger| {
                // 触发框本来有 1px 的边框：聚焦时加粗成焦点框，外面再画一圈。
                trigger.border_2().border_color(ring).child(crate::design::focus_outline(Corners::all(px(radius::MD)), 2., &p))
            })
            .when(!high_visibility_outlines() && crate::design::focus_from_keyboard(), |trigger| {
                // 描边保持 1px（加粗会让内容挪动），外面再用 1px 的投影补成 2px 的焦点框。
                trigger
                    .focus(move |style| style.border_color(ring).shadow(vec![BoxShadow::new(px(0.), px(0.), ring).spread_radius(px(1.))]))
            })
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_first))
            .on_action(cx.listener(Self::on_last))
            .on_action(cx.listener(Self::on_confirm))
            .on_click(cx.listener(|this, _, window, cx| {
                if this.open {
                    this.close(window, cx);
                } else {
                    this.open(window, cx);
                }
            }))
            .child(match (group, value) {
                (Some(group), Some(value)) => h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .child(div().flex_shrink(1.).min_w(px(0.)).truncate().text_color(p.text_secondary).child(group.to_string()))
                    .child(div().flex_none().text_color(p.text_secondary).child("\u{00a0}·\u{00a0}"))
                    .child(div().flex_none().max_w_full().truncate().child(value.to_string()))
                    .into_any_element(),
                (_, value) => div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .when(value.is_none(), |text| text.text_color(p.text_secondary))
                    .child(shown.to_string())
                    .into_any_element(),
            })
            .child(motion::turn(
                Icon::new(IconName::ChevronDown).size(px(size::ICON_SM)).text_color(p.text_secondary),
                "dropdown-turn",
                self.open,
                motion::HALF_TURN,
                window,
                cx,
            ));
        // 记下触发框的位置（不触发重绘：只在下次展开时用）。
        let trigger =
            trigger.child(canvas(move |area, _, _| bounds.set(Some(area)), |_, _, _, _| {}).absolute().top_0().left_0().size_full());
        // 展开时淡入；收起时立即移除。
        let phase = motion::presence("dropdown-list-presence", self.open, motion::BASE, window, cx);
        v_flex().child(trigger).children(phase.map(|phase| self.render_list(phase, window, cx)))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    // 不用 `super::*`：那样会把 GPUI 的 `test` 宏一起引进来，盖过标准的 `#[test]`。
    use gpui::TestAppContext;
    use gpui_kit::component::v_flex;
    use gpui_kit::{
        AnyWindowHandle, App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _,
        Render, Styled as _, Subscription, Window, div, px,
    };

    use super::{DropdownChanged, DropdownState, LIST_GAP, MAX_LIST_WIDTH, WINDOW_MARGIN, init, place};

    #[test]
    fn list_opens_below_unless_only_above_fits() {
        // 触发框在 100–136，窗口高 800：下面放得下，往下，原样高度。
        assert_eq!(place(100., 136., 800., 320.), (true, 320.));
        // 贴近底部（图一的情况）：下面只剩 40 多，上面宽，往上，不再压住触发框。
        let (down, height) = place(700., 736., 800., 320.);
        assert!(!down && height == 320.);
        // 两边都不够：选宽的一侧，高度缩到放得下。
        let (down, height) = place(200., 236., 420., 320.);
        assert!(!down);
        assert_eq!(height, 200. - LIST_GAP - WINDOW_MARGIN);
        // 短列表即使在下方空间不大时也往下。
        assert_eq!(place(700., 736., 800., 40.), (true, 40.));
        // 不管朝哪边，列表都在触发框之外：往下时顶边在触发框下方，往上时底边在触发框上方。
        for (top, bottom, window) in [(100., 136., 800.), (700., 736., 800.), (200., 236., 420.), (10., 46., 60.)] {
            let (down, height) = place(top, bottom, window, 320.);
            if down {
                assert!(bottom + LIST_GAP + height <= window - WINDOW_MARGIN + 0.01);
            } else {
                assert!(top - LIST_GAP - height >= WINDOW_MARGIN - 0.01);
            }
        }
    }

    /// 组合框前后各有一个可以 Tab 到的控件。
    struct Host {
        before: FocusHandle,
        dropdown: Entity<DropdownState>,
        after: FocusHandle,
        changes: Rc<RefCell<Vec<usize>>>,
        _subscription: Subscription,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            v_flex()
                .size_full()
                .child(div().id("before").track_focus(&self.before).size(px(20.)))
                .child(self.dropdown.clone())
                .child(div().id("after").track_focus(&self.after).size(px(20.)))
        }
    }

    fn open_host(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Host>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx);
            let (window, host) = gpui_kit::open_window(Default::default(), cx, |_, cx| {
                cx.new(|cx| {
                    let dropdown = cx.new(|cx| DropdownState::new(vec!["甲".into(), "乙".into(), "丙".into()], Some(0), cx));
                    let changes = Rc::new(RefCell::new(Vec::new()));
                    let recorded = changes.clone();
                    let subscription = cx.subscribe(&dropdown, move |_, _, DropdownChanged(index), _| {
                        recorded.borrow_mut().push(*index);
                    });
                    Host {
                        before: cx.focus_handle().tab_stop(true),
                        dropdown,
                        after: cx.focus_handle().tab_stop(true),
                        changes,
                        _subscription: subscription,
                    }
                })
            })
            .expect("open test window");
            (window, host)
        })
    }

    /// 在窗口里做一步，然后画一帧（按键要按上一帧的派发树走）。
    fn step(cx: &mut TestAppContext, window: AnyWindowHandle, f: impl FnOnce(&mut Window, &mut App)) {
        cx.update_window(window, |_, window, cx| {
            f(window, cx);
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    /// 按一个键（按上一帧的派发树走），再画一帧。
    fn press(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &str) {
        cx.simulate_keystrokes(window, keys);
        step(cx, window, |_, _| {});
    }

    fn focus_trigger(cx: &mut TestAppContext, window: AnyWindowHandle, host: &Entity<Host>) {
        let handle = cx.update(|cx| host.read(cx).dropdown.read(cx).focus_handle());
        step(cx, window, |window, cx| handle.focus(window, cx));
    }

    fn is_focused(cx: &mut TestAppContext, window: AnyWindowHandle, handle: impl Fn(&App) -> FocusHandle) -> bool {
        cx.update_window(window, |_, window, cx| handle(cx).is_focused(window)).unwrap()
    }

    /// 分组的选项（提供商 · 模型）：列表按组画组名，选项只写模型名；列表跟着最长的选项变宽（不超过上限），
    /// 不再把“DeepSeek OAuth · DeepSeek-V4-…”截成几项一样的样子。读屏软件读完整的“组名 · 选项名”。
    #[gpui::test]
    fn grouped_items_widen_the_list_instead_of_truncating(cx: &mut TestAppContext) {
        let (window, host) = open_host(cx);
        let long = "DeepSeek-V4-Flash-Thinking-Preview-Long-Model-Name";
        cx.update(|cx| {
            let dropdown = host.read(cx).dropdown.clone();
            dropdown.update(cx, |state, cx| {
                state.width = px(120.);
                state.set_grouped_items(
                    vec![("DeepSeek".into(), "DeepSeek-V4-Flash".into()), ("DeepSeek OAuth".into(), long.into())],
                    Some(1),
                    cx,
                );
            });
        });
        step(cx, window, |_, _| {});
        focus_trigger(cx, window, &host);
        press(cx, window, "enter");
        step(cx, window, |_, _| {});
        let (list, full, header) = cx.update(|cx| {
            let state = host.read(cx).dropdown.read(cx);
            (state.list_bounds.get(), state.full_label(1), (state.starts_group(0), state.starts_group(1)))
        });
        let list = list.expect("列表画出来了");
        assert!(list.size.width > px(120.), "列表比触发框宽：{list:?}");
        assert!(list.size.width <= px(MAX_LIST_WIDTH), "不超过上限：{list:?}");
        assert_eq!(full.as_deref(), Some(format!("DeepSeek OAuth · {long}").as_str()));
        assert_eq!(header, (Some("DeepSeek".into()), Some("DeepSeek OAuth".into())));
    }

    #[gpui::test]
    fn tab_in_open_list_commits_and_moves_to_next_control(cx: &mut TestAppContext) {
        let (window, host) = open_host(cx);
        step(cx, window, |_, _| {});
        focus_trigger(cx, window, &host);

        press(cx, window, "enter");
        let dropdown = cx.update(|cx| host.read(cx).dropdown.clone());
        assert!(cx.update(|cx| dropdown.read(cx).open), "Enter 展开列表");
        let first = dropdown.clone();
        assert!(is_focused(cx, window, move |cx| first.read(cx).options[0].clone()), "焦点在当前项上");

        press(cx, window, "down");
        let second = dropdown.clone();
        assert!(is_focused(cx, window, move |cx| second.read(cx).options[1].clone()), "↓ 移到下一项");

        press(cx, window, "tab");
        assert!(!cx.update(|cx| dropdown.read(cx).open), "Tab 收起列表");
        assert_eq!(cx.update(|cx| dropdown.read(cx).selected_index()), Some(1), "Tab 选中高亮项");
        let host_after = host.clone();
        assert!(is_focused(cx, window, move |cx| host_after.read(cx).after.clone()), "焦点到下一个控件，不飘走");
        assert_eq!(*cx.update(|cx| host.read(cx).changes.clone()).borrow(), vec![1]);
    }

    /// 鼠标移到窗口里的 (x, y)，再画一帧。
    fn move_mouse(cx: &mut TestAppContext, window: AnyWindowHandle, x: f32, y: f32) {
        step(cx, window, |window, cx| {
            window.dispatch_event(
                gpui_kit::PlatformInput::MouseMove(gpui_kit::MouseMoveEvent {
                    position: gpui_kit::point(px(x), px(y)),
                    pressed_button: None,
                    modifiers: Default::default(),
                }),
                cx,
            );
        });
    }

    #[gpui::test]
    fn hovering_an_option_focuses_it_like_a_system_combobox(cx: &mut TestAppContext) {
        let (window, host) = open_host(cx);
        step(cx, window, |_, _| {});
        focus_trigger(cx, window, &host);
        press(cx, window, "enter");
        let dropdown = cx.update(|cx| host.read(cx).dropdown.clone());
        // 各项的位置取决于平台的字体和控件高度（Linux 上触发框更高，列表整体往下移了约 30 px），
        // 不写死坐标：鼠标从上往下逐点移动，直到第三项在鼠标下面。
        let mut y = 0.;
        while cx.update(|cx| dropdown.read(cx).highlighted) != 2 {
            assert!(y < 600., "没找到第三项");
            move_mouse(cx, window, 40., y);
            y += 2.;
        }
        let third = dropdown.clone();
        assert!(is_focused(cx, window, move |cx| third.read(cx).options[2].clone()), "鼠标移上去就获得焦点（读屏软件读出它）");
        assert_eq!(cx.update(|cx| dropdown.read(cx).highlighted), 2, "也成为高亮项");
        // 接着用键盘：从鼠标停的那一项往上走。
        press(cx, window, "up");
        let second = dropdown.clone();
        assert!(is_focused(cx, window, move |cx| second.read(cx).options[1].clone()));
        // 只是悬停不改选中项。
        assert_eq!(cx.update(|cx| dropdown.read(cx).selected_index()), Some(0));
    }

    #[gpui::test]
    fn shift_tab_in_open_list_moves_to_previous_control(cx: &mut TestAppContext) {
        let (window, host) = open_host(cx);
        step(cx, window, |_, _| {});
        focus_trigger(cx, window, &host);
        press(cx, window, "alt-down");
        press(cx, window, "shift-tab");
        let host_before = host.clone();
        assert!(is_focused(cx, window, move |cx| host_before.read(cx).before.clone()));
    }

    #[gpui::test]
    fn escape_closes_without_changing_and_returns_to_trigger(cx: &mut TestAppContext) {
        let (window, host) = open_host(cx);
        step(cx, window, |_, _| {});
        focus_trigger(cx, window, &host);
        let dropdown = cx.update(|cx| host.read(cx).dropdown.clone());
        press(cx, window, "space");
        press(cx, window, "end");
        press(cx, window, "escape");
        assert!(!cx.update(|cx| dropdown.read(cx).open));
        assert_eq!(cx.update(|cx| dropdown.read(cx).selected_index()), Some(0));
        let trigger = dropdown.clone();
        assert!(is_focused(cx, window, move |cx| trigger.read(cx).focus_handle()));
        assert!(cx.update(|cx| host.read(cx).changes.borrow().is_empty()));
    }

    #[gpui::test]
    fn enter_on_option_chooses_it(cx: &mut TestAppContext) {
        let (window, host) = open_host(cx);
        step(cx, window, |_, _| {});
        focus_trigger(cx, window, &host);
        let dropdown = cx.update(|cx| host.read(cx).dropdown.clone());
        press(cx, window, "enter");
        press(cx, window, "end");
        press(cx, window, "enter");
        assert!(!cx.update(|cx| dropdown.read(cx).open));
        assert_eq!(cx.update(|cx| dropdown.read(cx).selected_index()), Some(2));
        let trigger = dropdown.clone();
        assert!(is_focused(cx, window, move |cx| trigger.read(cx).focus_handle()));
    }

    /// Windows：收起时方向键直接改值，不展开。
    #[cfg(not(target_os = "macos"))]
    #[gpui::test]
    fn arrows_on_closed_trigger_change_the_value(cx: &mut TestAppContext) {
        let (window, host) = open_host(cx);
        step(cx, window, |_, _| {});
        focus_trigger(cx, window, &host);
        let dropdown = cx.update(|cx| host.read(cx).dropdown.clone());
        press(cx, window, "down");
        press(cx, window, "down");
        press(cx, window, "down");
        assert!(!cx.update(|cx| dropdown.read(cx).open));
        assert_eq!(cx.update(|cx| dropdown.read(cx).selected_index()), Some(2), "停在最后一项");
        press(cx, window, "home");
        assert_eq!(cx.update(|cx| dropdown.read(cx).selected_index()), Some(0));
        assert_eq!(*cx.update(|cx| host.read(cx).changes.clone()).borrow(), vec![1, 2, 0]);
    }
}
