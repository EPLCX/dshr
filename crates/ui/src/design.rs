//! 设计规范的实现，见 `docs/design.md`。
//!
//! 视图只使用这里的 token 与基础组件：不直接写颜色值、圆角或零散的字号。
//! [`Palette`] 是按当前主题算好的颜色角色，`Copy`，取出后不再借用 `cx`。

use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// 间距阶梯（4px 基准）。
pub mod space {
    pub const XS: f32 = 4.;
    pub const S: f32 = 8.;
    pub const M: f32 = 12.;
    pub const L: f32 = 16.;
    pub const XL: f32 = 20.;
    pub const XXL: f32 = 24.;
    pub const XXXL: f32 = 32.;
}

/// 圆角阶梯。
pub mod radius {
    pub const SM: f32 = 6.;
    pub const MD: f32 = 8.;
    pub const LG: f32 = 12.;
    pub const XL: f32 = 16.;
    pub const XXL: f32 = 24.;
}

/// 控件、图标尺寸与布局常量。
pub mod size {
    pub const CONTROL_MD: f32 = 36.;
    pub const ICON_SM: f32 = 14.;
    pub const ICON_MD: f32 = 16.;
    pub const ICON_LG: f32 = 20.;
    pub const ICON_HERO: f32 = 32.;
    pub const STATUS_DOT: f32 = 8.;
    pub const LIST_ITEM: f32 = 32.;
    pub const NAV_ITEM: f32 = 40.;
    pub const SIDEBAR_WIDTH: f32 = 260.;
    /// 收起后的侧边栏：一列图标按钮。
    pub const SIDEBAR_RAIL_WIDTH: f32 = 56.;
    pub const CONTENT_MAX_WIDTH: f32 = 860.;
    pub const MODAL_WIDTH: f32 = 1000.;
    pub const MODAL_HEIGHT: f32 = 820.;
    pub const MODAL_NAV_WIDTH: f32 = 240.;
    pub const MODAL_CONTENT_MAX_WIDTH: f32 = 720.;
    pub const MENU_WIDTH: f32 = 300.;
}

/// 按当前主题算好的颜色角色。
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub text: Hsla,
    pub text_secondary: Hsla,
    pub surface: Hsla,
    /// 浮在页面之上的表面（弹窗、菜单）；深色模式下比页面略亮，才能和背景分开。
    pub raised: Hsla,
    pub sidebar: Hsla,
    pub sunken: Hsla,
    pub selected: Hsla,
    pub border: Hsla,
    pub accent: Hsla,
    pub accent_soft: Hsla,
    pub on_accent: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub scrim: Hsla,
    /// 键盘焦点框。
    pub focus_ring: Hsla,
}

pub fn palette(cx: &App) -> Palette {
    let theme = cx.theme();
    let dark = theme.is_dark();
    let mut palette = base_palette(cx, dark);
    if let Some(over) = cx.try_global::<PaletteOverride>() {
        over.apply(&mut palette, dark);
    }
    palette
}

/// 最近一次输入是不是来自触摸屏，由窗口的根视图每帧开头按 `Window::last_input_was_touch` 更新
/// （[`update_touch_mode`]）。输入方式一变 GPUI 会整窗重画，缓存的视图也会重新生成。
#[derive(Clone, Copy, Default)]
struct TouchMode(bool);

impl Global for TouchMode {}

/// 触屏模式：最近一次输入来自触摸屏。这时悬停才出现的东西直接显示，悬停底色不会留在手指点过的地方。
/// 鼠标、键盘一用就恢复。
pub fn touch_mode(cx: &App) -> bool {
    cx.try_global::<TouchMode>().is_some_and(|mode| mode.0)
}

/// 在窗口根视图的 `render` 开头调用（在取调色板之前）。
pub fn update_touch_mode(window: &Window, cx: &mut App) {
    let touch = window.last_input_was_touch();
    if touch_mode(cx) != touch {
        cx.set_global(TouchMode(touch));
    }
}

/// 配色方案（`<数据目录>/themes/<id>.toml` 或插件的 `themes`）：只覆盖给出的颜色，浅色、深色各一套。
#[derive(Debug, Clone, Default)]
pub struct PaletteOverride {
    pub light: Vec<(String, Hsla)>,
    pub dark: Vec<(String, Hsla)>,
}

impl Global for PaletteOverride {}

impl PaletteOverride {
    /// 从 `#rrggbb` 的表建；颜色写错、正文和背景的对比度不到 4.5:1 时拒绝整套。
    pub fn parse(
        light: &std::collections::BTreeMap<String, String>,
        dark: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let parse_set = |set: &std::collections::BTreeMap<String, String>| -> Result<Vec<(String, Hsla)>, String> {
            set.iter()
                .map(|(key, value)| {
                    if !PALETTE_KEYS.contains(&key.as_str()) {
                        return Err(format!("unknown color {key} (available: {})", PALETTE_KEYS.join(", ")));
                    }
                    let hex = value.strip_prefix('#').filter(|hex| hex.len() == 6).and_then(|hex| u32::from_str_radix(hex, 16).ok());
                    hex.map(|hex| (key.clone(), Hsla::from(rgb(hex)))).ok_or_else(|| format!("{key}: {value} is not a #rrggbb color"))
                })
                .collect()
        };
        let over = Self { light: parse_set(light)?, dark: parse_set(dark)? };
        for (mode, set) in [("light", &over.light), ("dark", &over.dark)] {
            let color = |key: &str| set.iter().find(|(name, _)| name == key).map(|(_, color)| *color);
            if let (Some(text), Some(surface)) = (color("text"), color("surface"))
                && contrast(text, surface) < 4.5
            {
                return Err(format!("{mode}: the text color has too little contrast with the surface (needs 4.5:1)"));
            }
        }
        Ok(over)
    }

    /// 这一种明暗下有没有改任何颜色。
    pub fn is_empty(&self, dark: bool) -> bool {
        if dark { self.dark.is_empty() } else { self.light.is_empty() }
    }

    fn apply(&self, palette: &mut Palette, dark: bool) {
        for (key, color) in if dark { &self.dark } else { &self.light } {
            let slot = match key.as_str() {
                "text" => &mut palette.text,
                "text_secondary" => &mut palette.text_secondary,
                "surface" => &mut palette.surface,
                "raised" => &mut palette.raised,
                "sidebar" => &mut palette.sidebar,
                "sunken" => &mut palette.sunken,
                "selected" => &mut palette.selected,
                "border" => &mut palette.border,
                "accent" => &mut palette.accent,
                "accent_soft" => &mut palette.accent_soft,
                "on_accent" => &mut palette.on_accent,
                "success" => &mut palette.success,
                "warning" => &mut palette.warning,
                "danger" => &mut palette.danger,
                "focus_ring" => &mut palette.focus_ring,
                _ => continue,
            };
            *slot = *color;
        }
    }
}

/// 用了配色方案（包括高对比度）时，把它套到组件库的主题颜色上：组件库自己画的控件（开关、输入框、
/// 组件库按钮、滚动条、Markdown 回复）和我们自己画的用同一套颜色。
pub fn apply_palette_to_theme(p: &Palette, colors: &mut gpui_kit::component::ThemeColor) {
    colors.background = p.surface;
    colors.foreground = p.text;
    colors.border = p.border;
    colors.input = p.border;
    colors.caret = p.text;
    colors.muted = p.sunken;
    colors.muted_foreground = p.text_secondary;
    colors.popover = p.raised;
    colors.popover_foreground = p.text;
    colors.sidebar = p.sidebar;
    colors.sidebar_foreground = p.text;
    colors.sidebar_border = p.border;
    colors.sidebar_accent = p.selected;
    colors.sidebar_accent_foreground = p.text;
    colors.title_bar = p.sidebar;
    colors.title_bar_border = p.border;
    colors.window_border = p.border;
    colors.secondary = p.selected;
    colors.secondary_hover = p.selected;
    colors.secondary_active = p.selected;
    colors.secondary_foreground = p.text;
    colors.accent = p.selected;
    colors.accent_foreground = p.text;
    colors.list_hover = p.selected;
    colors.list_active = p.selected;
    colors.primary = p.accent;
    colors.primary_hover = p.accent;
    colors.primary_active = p.accent;
    colors.primary_foreground = p.on_accent;
    colors.button_primary = p.accent;
    colors.button_primary_hover = p.accent;
    colors.button_primary_active = p.accent;
    colors.button_primary_foreground = p.on_accent;
    colors.link = p.accent;
    colors.link_hover = p.accent;
    colors.link_active = p.accent;
    colors.danger = p.danger;
    colors.success = p.success;
    colors.warning = p.warning;
    colors.selection = p.accent_soft;
    // 关着的开关：轨道要和底色分得开（非文字元素至少 3:1）；开着的是强调色。
    colors.switch = p.text_secondary;
    colors.switch_thumb = p.surface;
    colors.scrollbar_thumb = p.text_secondary;
    colors.scrollbar_thumb_hover = p.text;
    colors.ring = p.focus_ring;
}

/// 配色方案能改的颜色（`Palette` 的字段名）。
pub const PALETTE_KEYS: [&str; 15] = [
    "text",
    "text_secondary",
    "surface",
    "raised",
    "sidebar",
    "sunken",
    "selected",
    "border",
    "accent",
    "accent_soft",
    "on_accent",
    "success",
    "warning",
    "danger",
    "focus_ring",
];

/// WCAG 对比度。
fn contrast(a: Hsla, b: Hsla) -> f32 {
    let luminance = |color: Hsla| {
        let rgba = color.to_rgb();
        let channel = |value: f32| if value <= 0.03928 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) };
        0.2126 * channel(rgba.r) + 0.7152 * channel(rgba.g) + 0.0722 * channel(rgba.b)
    };
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn base_palette(cx: &App, dark: bool) -> Palette {
    let theme = cx.theme();
    let accent: Hsla = if dark { rgb(0x7088FF) } else { rgb(0x4D6BFE) }.into();
    Palette {
        text: theme.foreground,
        text_secondary: theme.muted_foreground,
        surface: theme.background,
        raised: if dark { theme.popover } else { theme.background },
        sidebar: theme.sidebar,
        sunken: theme.muted.opacity(0.6),
        selected: theme.secondary,
        border: theme.border,
        accent,
        accent_soft: accent.opacity(if dark { 0.2 } else { 0.12 }),
        on_accent: white(),
        success: theme.success,
        warning: if dark { rgb(0xF59E0B) } else { rgb(0xB45309) }.into(),
        danger: theme.danger,
        scrim: black().opacity(if dark { 0.55 } else { 0.35 }),
        focus_ring: accent,
    }
}

/// 深色主题的底色：组件库默认的深色几乎是纯黑（`#0a0a0a`），对比太硬、层次也分不出来。
/// 换成带一点冷调的深灰，分出四层：页面（对话区，最深）< 侧边栏 < 卡片里的下沉区 < 浮在上面的菜单和弹窗。
/// 对话区比侧边栏深，视线落在内容上；侧边栏和标题栏左段用同一个颜色。
/// 主色按钮在默认深色里是白底，改用品牌蓝，和浅色模式里的强调色一致。
pub fn soften_dark(theme: &mut gpui_kit::component::Theme) {
    let c = |hex: u32| -> Hsla { rgb(hex).into() };
    let colors = &mut theme.colors;
    let page = c(0x141518);
    let sidebar = c(0x1D1E22);
    let raised = c(0x2A2B30);
    let hover = c(0x313338);
    let border = c(0x303237);
    let text = c(0xE6E7EA);
    let muted_text = c(0x9DA0A8);
    let accent = c(0x7088FF);

    colors.background = page;
    colors.foreground = text;
    colors.border = border;
    colors.input = c(0x3C3E45);
    colors.ring = accent;
    colors.caret = text;

    colors.sidebar = sidebar;
    colors.sidebar_foreground = text;
    colors.sidebar_border = c(0x2A2B30);
    colors.sidebar_accent = hover;
    colors.sidebar_accent_foreground = text;

    colors.popover = raised;
    colors.popover_foreground = text;
    colors.muted = c(0x2B2C31);
    colors.muted_foreground = muted_text;
    colors.secondary = hover;
    colors.secondary_hover = c(0x383A40);
    colors.secondary_active = c(0x2E3035);
    colors.secondary_foreground = text;
    colors.accent = hover;
    colors.accent_foreground = text;

    colors.primary = accent;
    colors.primary_hover = c(0x8197FF);
    colors.primary_active = c(0x5F78F0);
    colors.primary_foreground = c(0xFFFFFF);
    colors.button_primary = accent;
    colors.button_primary_hover = c(0x8197FF);
    colors.button_primary_active = c(0x5F78F0);
    colors.button_primary_foreground = c(0xFFFFFF);
    colors.selection = accent.opacity(0.45);

    for list in [&mut colors.list, &mut colors.table, &mut colors.accordion, &mut colors.group_box, &mut colors.tab_active] {
        *list = page;
    }
    colors.list_hover = hover;
    colors.list_even = c(0x1A1B1F);
    colors.list_head = c(0x1A1B1F);
    colors.table_hover = hover;
    colors.table_even = c(0x1A1B1F);
    colors.table_head = c(0x1A1B1F);
    colors.table_row_border = border;
    colors.skeleton = c(0x2B2C31);
    colors.switch = c(0x45474E);
    colors.tab_bar = sidebar;
    colors.tab_bar_segmented = c(0x2B2C31);
    colors.title_bar = sidebar;
    colors.title_bar_border = c(0x2A2B30);
    colors.status_bar = sidebar;
    colors.status_bar_border = c(0x2A2B30);
    colors.window_border = border;
    colors.description_list_label = c(0x2B2C31);
    colors.overlay = c(0x000000).opacity(0.4);
}

/// 等宽字体（工具参数、命令输出）。
pub fn mono_font(cx: &App) -> SharedString {
    cx.theme().mono_font_family.clone()
}

/// 界面文字大小（设置 → 辅助功能），1.0 是默认。[`Typography`] 拿不到 `cx`，所以存在这里。
static TEXT_SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(100);

/// 可选的文字大小（百分比）。再大的话，固定高度的列表项、按钮里放不下一行字。
pub const TEXT_SCALES: [u16; 4] = [90, 100, 115, 130];

pub fn text_scale() -> f32 {
    TEXT_SCALE.load(std::sync::atomic::Ordering::Relaxed) as f32 / 100.
}

/// 设成 `percent`%（限制在 [`TEXT_SCALES`] 的范围里）。之后要调用 `sync_theme` 并刷新窗口。
pub fn set_text_scale(percent: u16) {
    let percent = percent.clamp(TEXT_SCALES[0], TEXT_SCALES[TEXT_SCALES.len() - 1]);
    TEXT_SCALE.store(percent as u32, std::sync::atomic::Ordering::Relaxed);
}

/// 按文字大小设置缩放后的字号、行高（行高取整，避免半像素的文字发虚）。
fn scaled<S: Styled>(style: S, size: f32, line: f32) -> S {
    let scale = text_scale();
    style.text_size(px(size * scale)).line_height(px((line * scale).round()))
}

/// 字体样式。字号、行高随设置里的文字大小缩放。
pub trait Typography: Styled + Sized {
    /// 中文大字号用常规字重更清爽，层级靠字号拉开。
    fn text_display(self) -> Self {
        scaled(self, 28., 36.).font_weight(FontWeight::NORMAL)
    }
    fn text_title(self) -> Self {
        scaled(self, 20., 28.).font_weight(FontWeight::SEMIBOLD)
    }
    fn text_heading(self) -> Self {
        scaled(self, 16., 24.).font_weight(FontWeight::SEMIBOLD)
    }
    fn text_label(self) -> Self {
        scaled(self, 14., 20.).font_weight(FontWeight::MEDIUM)
    }
    fn text_body(self) -> Self {
        scaled(self, 14., 22.).font_weight(FontWeight::NORMAL)
    }
    fn text_content(self) -> Self {
        scaled(self, 15., 24.).font_weight(FontWeight::NORMAL)
    }
    fn text_caption(self) -> Self {
        scaled(self, 12., 18.).font_weight(FontWeight::NORMAL)
    }
    fn text_mono(self, family: SharedString) -> Self {
        scaled(self.font_family(family), 13., 20.)
    }
}

impl<T: Styled> Typography for T {}

/// 提示的语气。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Success,
    Warning,
    Danger,
}

impl Tone {
    pub fn color(self, p: &Palette) -> Hsla {
        match self {
            Tone::Neutral => p.text_secondary,
            Tone::Success => p.success,
            Tone::Warning => p.warning,
            Tone::Danger => p.danger,
        }
    }
}

/// 屏幕阅读器能读到的文字。
///
/// 直接作为子元素的字符串不进入无障碍树，读屏软件看不到。这里用文字本身作 id：
/// 内容变了就是新节点，读屏软件会当作新内容播报。同一容器里出现两段完全相同的文字时，
/// 用 [`readable_with_id`] 指定 id，避免重复。
pub fn readable(text: impl Into<SharedString>) -> Text {
    let text = text.into();
    Text::new(ElementId::Name(text.clone()), text)
}

/// 同 [`readable`]，但由调用方给出 id（列表里的消息、逐行的输出等）。
pub fn readable_with_id(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Text {
    Text::new(id.into(), text.into())
}

/// 标题：屏幕阅读器可以按标题跳转。
pub fn heading(text: impl Into<SharedString>, level: usize) -> Stateful<Div> {
    let text = text.into();
    div().id(ElementId::Name(text.clone())).role(Role::Heading).aria_level(level).aria_label(text.clone()).child(text)
}

/// 一行提示文字。
pub fn hint(text: impl Into<SharedString>, tone: Tone, p: &Palette) -> Div {
    div().text_caption().text_color(tone.color(p)).child(readable(text))
}

/// 正文说明（次要色）。
pub fn description(text: impl Into<SharedString>, p: &Palette) -> Div {
    div().text_body().text_color(p.text_secondary).child(readable(text))
}

/// 读屏软件读到的控件名称带上所属区块，如“MCP 服务器，名称”。
///
/// Windows 讲述人按 Tab 到一个控件时只读控件自己的名称，不读旁边的标题；只写“名称”，
/// 听的人不知道这是哪一块的设置。
pub fn in_section(section: impl AsRef<str>, field: impl AsRef<str>) -> String {
    rust_i18n::t!("a11y.in_section", section = section.as_ref(), field = field.as_ref()).to_string()
}

/// 表单项：标签 → 控件 → 可选提示。
///
/// 不设成有名字的分组：控件自己的名称已经带着标签（最好用 [`in_section`] 再带上区块），
/// 分组再报一遍标签，讲述人每到一个控件就把同一个词读两遍。
pub fn field(label: impl Into<SharedString>, control: impl IntoElement) -> Stateful<Div> {
    let label = label.into();
    v_flex()
        .id(ElementId::Name(format!("field-{label}").into()))
        .gap(px(space::S))
        .child(div().text_label().child(readable(label.clone())))
        .child(crate::reveal::reveal(ElementId::Name(format!("reveal-field-{label}").into()), control))
}

/// 设置行：左边标题和一句说明，右边控件（下拉框、开关、按钮）。多行放进 [`setting_list`]。
///
/// 控件自己的名称由调用方设（带上标题和区块）。整行不设成有名字、带描述的分组：
/// 讲述人进到一行就先读一遍分组名和整段说明，再读控件，听半天才听到控件和当前值。
/// 说明仍是可读的文字，扫描模式下能读到。
pub fn setting_row(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    desc: Option<SharedString>,
    control: impl IntoElement,
    p: &Palette,
) -> Stateful<Div> {
    let title = title.into();
    h_flex()
        .id(id)
        .py(px(space::L))
        .gap(px(space::L))
        .items_center()
        .child(
            v_flex()
                .flex_1()
                .min_w(px(0.))
                .gap(px(2.))
                .child(div().text_label().child(readable(title)))
                .when_some(desc, |column, desc| column.child(hint(desc, Tone::Neutral, p))),
        )
        .child(div().flex_none().child(crate::reveal::reveal("reveal-control", control)))
}

/// 一组设置行：卡片里一行一行排，行间有分隔线（见设置弹窗“常规”页）。
pub fn setting_list(rows: Vec<AnyElement>, p: &Palette) -> Div {
    let mut list = v_flex();
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            list = list.child(divider(p));
        }
        list = list.child(row);
    }
    card(p).px(px(space::XL)).py(px(space::XS)).child(list)
}

/// 区块标题与可选说明。
pub fn section_header(title: impl Into<SharedString>, desc: Option<SharedString>, p: &Palette) -> Div {
    v_flex().gap(px(space::XS)).child(heading(title, 2).text_heading()).when_some(desc, |header, desc| header.child(description(desc, p)))
}

/// 卡片：底色 + 描边。
pub fn card(p: &Palette) -> Div {
    div().bg(p.surface).border_1().border_color(p.border).rounded(px(radius::LG))
}

/// 面板：下沉底色、无描边，用于成组的编辑区。
pub fn panel(p: &Palette) -> Div {
    div().bg(p.sunken).rounded(px(radius::XL)).p(px(space::XL))
}

pub fn divider(p: &Palette) -> Div {
    div().h(px(1.)).bg(p.border)
}

pub fn status_dot(ready: bool, p: &Palette) -> Div {
    div().size(px(size::STATUS_DOT)).flex_none().rounded_full().bg(if ready { p.success } else { p.text_secondary })
}

pub fn badge(text: impl Into<SharedString>, p: &Palette) -> Div {
    div().px(px(space::S)).py(px(2.)).rounded_full().text_caption().bg(p.accent_soft).text_color(p.accent).child(readable(text))
}

/// 品牌标记。
pub fn brand_mark(icon_size: f32, p: &Palette) -> Icon {
    Icon::new(IconName::SquareTerminal).size(px(icon_size)).text_color(p.accent)
}

/// 是否开启“高可见性控件轮廓”（设置里切换；[`pressable`] 拿不到 `cx`，所以存在这里）。
static HIGH_VISIBILITY_OUTLINES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn high_visibility_outlines() -> bool {
    HIGH_VISIBILITY_OUTLINES.load(std::sync::atomic::Ordering::Relaxed)
}

// 焦点是不是用键盘挪过来的：决定默认样式下画不画焦点框。
//
// 不用 GPUI 的 `last_input_was_keyboard`：它在鼠标一动时就变回 `false`，用键盘走到一个控件后碰一下鼠标，
// 焦点框就消失了。这里只看真正“触发”的设备：按键时为真，按下鼠标（或手指）时为假，移动鼠标不算。
// 只在界面线程上读写（按键、鼠标事件和绘制都在那里），所以是线程局部的。
thread_local! {
    static FOCUS_FROM_KEYBOARD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn focus_from_keyboard() -> bool {
    FOCUS_FROM_KEYBOARD.with(std::cell::Cell::get)
}

/// 记下焦点来自哪种设备；变了就整窗重画（缓存的视图也要换上 / 去掉焦点框）。
/// 程序自己挪焦点（如发出消息后把焦点给新消息）时传 `false`：这类焦点默认不显示焦点框。
pub fn set_focus_from_keyboard(keyboard: bool, window: &mut Window) {
    if FOCUS_FROM_KEYBOARD.with(|cell| cell.replace(keyboard)) != keyboard {
        window.refresh();
    }
}

/// 窗口根元素上调用：按下鼠标（或手指）时把 [`focus_from_keyboard`] 记成假。
/// 按键那一半在 [`init`] 里用 `intercept_keystrokes` 记：绑定了动作的键（Tab、方向键）派发成动作后
/// 不再走元素的按键监听，在根元素上捕获不到。
pub fn track_focus_source<E: InteractiveElement>(element: E) -> E {
    element.capture_any_mouse_down(|_, window, _| set_focus_from_keyboard(false, window))
}

pub fn set_high_visibility_outlines(enabled: bool) {
    HIGH_VISIBILITY_OUTLINES.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// [`pressable`] 的焦点框宽度：透明的边框，聚焦时才上色。
const FOCUS_BORDER: f32 = 2.;
/// 高可见性轮廓：焦点框外面空出这么宽，再画一圈外轮廓。两圈之间留着背景色，
/// 不管底下是什么颜色，内圈（焦点色）和外圈（正文色）都分得开。
const HIGH_VISIBILITY_GAP: f32 = 7.;
/// 高可见性轮廓的外圈宽度。
const HIGH_VISIBILITY_WIDTH: f32 = 3.;
/// 外圈四周的阴影：几圈越来越淡的细线，一圈 1.5px（不透明度从里到外）。不用 GPUI 的投影：它是实心的，
/// 画在控件上面会把控件整块压暗。
const HALO: [f32; 4] = [0.30, 0.18, 0.09, 0.04];
const HALO_STEP: f32 = 1.5;
/// 外圈连阴影伸出控件边框外沿多远。
const OUTLINE_EXTENT: f32 = HIGH_VISIBILITY_GAP + HIGH_VISIBILITY_WIDTH + HALO_STEP * HALO.len() as f32;

/// 外圈（正文色描边）加四周的阴影。调用方定位、定大小；`radius` 是外圈外沿的圆角。
fn outline_ring(radius: Corners<Pixels>, p: &Palette) -> Div {
    let halo = HALO.iter().enumerate().map(|(index, alpha)| {
        let offset = px(HALO_STEP * (index + 1) as f32);
        div()
            .absolute()
            // 绝对定位从边框内侧算起：先退出外圈的宽度，再往外一圈一圈扩。
            .top(-px(HIGH_VISIBILITY_WIDTH) - offset)
            .left(-px(HIGH_VISIBILITY_WIDTH) - offset)
            .right(-px(HIGH_VISIBILITY_WIDTH) - offset)
            .bottom(-px(HIGH_VISIBILITY_WIDTH) - offset)
            .border(px(HALO_STEP))
            .border_color(black().opacity(*alpha))
            .rounded_tl(radius.top_left + offset)
            .rounded_tr(radius.top_right + offset)
            .rounded_bl(radius.bottom_left + offset)
            .rounded_br(radius.bottom_right + offset)
    });
    div()
        .border(px(HIGH_VISIBILITY_WIDTH))
        .border_color(p.text)
        .rounded_tl(radius.top_left)
        .rounded_tr(radius.top_right)
        .rounded_bl(radius.bottom_left)
        .rounded_br(radius.bottom_right)
        .children(halo)
}

/// 高可见性轮廓的外圈：一个只有描边、中间透明的框，绝对定位在控件外面。
///
/// - 不能用投影画：GPUI 的投影是实心的，控件自己没有底色时整块会被涂成正文色。
/// - 画在延后绘制层（[`OutlineLayer`]）：在所有内容之后画，不被控件所在的卡片、列表裁掉；
///   只限制在控件所在的可见区域往外扩出外圈的范围内，滚出视口的控件不会把外圈留在别处。
///
/// `radius` 是控件自己的圆角，`border` 是控件的边框宽度（绝对定位从边框内侧算起）。
pub fn focus_outline(radius: Corners<Pixels>, border: f32, p: &Palette) -> OutlineLayer {
    let reach = border + HIGH_VISIBILITY_GAP + HIGH_VISIBILITY_WIDTH;
    let grow = px(HIGH_VISIBILITY_GAP + HIGH_VISIBILITY_WIDTH);
    let ring = outline_ring(radius.map(|corner| *corner + grow), p)
        .absolute()
        .top(px(-reach))
        .left(px(-reach))
        .right(px(-reach))
        .bottom(px(-reach));
    OutlineLayer { ring: Some(ring.into_any_element()) }
}

/// 外圈在延后绘制层里的次序：比所有弹出层（下拉列表 1、组件库的弹出菜单 100）高，比工具提示（200）低。
/// 菜单、列表里有焦点的项的外圈画在菜单上面，不会被菜单盖住。
const OUTLINE_PRIORITY: usize = 150;

/// 见 [`focus_outline`]。排版时和普通子元素一样（绝对定位在控件外面），绘制时交给延后绘制层。
pub struct OutlineLayer {
    ring: Option<AnyElement>,
}

impl IntoElement for OutlineLayer {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for OutlineLayer {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.ring.as_mut().expect("外圈只排版一次").request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) {
        let grow = px(HIGH_VISIBILITY_GAP + HIGH_VISIBILITY_WIDTH);
        let visible = window.content_mask().bounds;
        // 控件已经整个滚出了可见区域：不画。
        if !visible.intersects(&bounds.dilate(-grow)) {
            return;
        }
        let Some(ring) = self.ring.take() else { return };
        let mask = ContentMask { bounds: visible.dilate(px(OUTLINE_EXTENT)) };
        window.defer_draw(ring, window.element_offset(), OUTLINE_PRIORITY, Some(mask));
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }
}

/// 组件库的控件（按钮、输入框）外面套一层：高可见性轮廓开启、控件有焦点时，在它外面画和 [`pressable`]
/// 一样的外圈（组件库自己只画一圈半透明的焦点框）。排版和绘制都原样交给里面的控件。
///
/// 怎么知道控件有没有焦点：
/// - 组件库按钮：[`framed`]，传和 `Button::new` 一样的 id。按钮按这个 id 在元素状态里存焦点句柄（在 GPUI 给无状态
///   组件加的、以类型名为 id 的那一层里），这一层不占 id，在同样的作用域按同一个 id 取到的就是同一个句柄；
/// - 输入框和给了 `track_focus` 的按钮：[`framed_handle`]，直接给句柄。
pub struct FocusFrame {
    key: Option<ElementId>,
    handle: Option<FocusHandle>,
    child: AnyElement,
}

pub fn framed(id: impl Into<ElementId>, child: impl IntoElement) -> FocusFrame {
    FocusFrame { key: Some(id.into()), handle: None, child: child.into_any_element() }
}

pub fn framed_handle(handle: FocusHandle, child: impl IntoElement) -> FocusFrame {
    FocusFrame { key: None, handle: Some(handle), child: child.into_any_element() }
}

/// 弹出菜单的触发按钮用的 [`framed`]：菜单（`menu_popover` / 组件库的 `Popover`）要求触发器实现 `Selectable`，
/// 用它同步“已展开”的状态。这一层把 `Selectable` 原样转给里面的按钮，交给菜单渲染时再变成 [`FocusFrame`]。
/// 按钮在菜单里渲染时和这一层在同一层，按 id 取到的焦点句柄还是同一个。
// 应用里的弹出菜单都改用了 [`framed_trigger_with`]（见 [`menu_trigger_focus`]）；留给不需要自己焦点句柄的弹出层。
#[cfg_attr(not(test), allow(dead_code))]
pub fn framed_trigger<E: gpui_kit::component::Selectable + IntoElement + 'static>(
    id: impl Into<ElementId>,
    trigger: E,
) -> FramedTrigger<E> {
    FramedTrigger { id: id.into(), handle: None, trigger }
}

/// 同 [`framed_trigger`]，按钮用的是自己给的焦点句柄（`Button::track_focus`，如 [`menu_trigger_focus`]）。
pub fn framed_trigger_with<E: gpui_kit::component::Selectable + IntoElement + 'static>(
    handle: FocusHandle,
    trigger: E,
) -> FramedTrigger<E> {
    FramedTrigger { id: ElementId::from("framed-trigger"), handle: Some(handle), trigger }
}

pub struct FramedTrigger<E> {
    id: ElementId,
    handle: Option<FocusHandle>,
    trigger: E,
}

impl<E: gpui_kit::component::Selectable + IntoElement + 'static> gpui_kit::component::Selectable for FramedTrigger<E> {
    fn selected(mut self, selected: bool) -> Self {
        self.trigger = self.trigger.selected(selected);
        self
    }

    fn is_selected(&self) -> bool {
        self.trigger.is_selected()
    }

    fn secondary_selected(mut self, selected: bool) -> Self {
        self.trigger = self.trigger.secondary_selected(selected);
        self
    }

    fn open(mut self, open: bool) -> Self {
        self.trigger = self.trigger.open(open);
        self
    }

    fn is_open(&self) -> bool {
        self.trigger.is_open()
    }
}

impl<E: IntoElement + 'static> IntoElement for FramedTrigger<E> {
    type Element = FocusFrame;

    fn into_element(self) -> FocusFrame {
        framed_with(self.id, self.handle, self.trigger)
    }
}

/// 按钮可能给了 `track_focus`（`Some`），也可能没有（按 id 取）。
pub fn framed_with(id: impl Into<ElementId>, handle: Option<FocusHandle>, child: impl IntoElement) -> FocusFrame {
    match handle {
        Some(handle) => framed_handle(handle, child),
        None => framed(id, child),
    }
}

impl IntoElement for FocusFrame {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for FocusFrame {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        if let Some(key) = self.key.take() {
            // 按钮是无状态组件：GPUI 渲染它时先压一层以类型名为 id 的作用域，按钮在那一层里按自己的 id 存焦点句柄。
            // 这里在同样的作用域里按同一个 id 取，拿到的就是同一个句柄（必须在下面的 `request_layout` 渲染按钮之前）。
            let scope = ElementId::from(std::any::type_name::<gpui_kit::component::button::Button>());
            let handle = window.with_id(scope, |window| window.use_keyed_state(key, cx, |_, cx| cx.focus_handle()).read(cx).clone());
            self.handle = Some(handle);
        }
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
        if high_visibility_outlines() && self.handle.as_ref().is_some_and(|handle| handle.is_focused(window)) {
            let radius = cx.theme().radius;
            defer_outline(bounds, Corners::all(radius), window, cx);
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

#[cfg(test)]
thread_local! {
    /// 测试用：[`defer_outline`] 画了几次外圈。
    pub(crate) static OUTLINES_DRAWN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 在 `bounds`（控件的边框外沿）外面隔一段空隙画外圈，交给延后绘制层（见 [`focus_outline`]）。在 prepaint 里调用。
fn defer_outline(bounds: Bounds<Pixels>, radius: Corners<Pixels>, window: &mut Window, cx: &mut App) {
    let grow = px(HIGH_VISIBILITY_GAP + HIGH_VISIBILITY_WIDTH);
    let visible = window.content_mask().bounds;
    if !visible.intersects(&bounds) {
        return;
    }
    let outer = bounds.dilate(grow);
    #[cfg(test)]
    OUTLINES_DRAWN.with(|count| count.set(count.get() + 1));
    let p = palette(cx);
    let mut ring = outline_ring(radius.map(|corner| *corner + grow), &p).w(outer.size.width).h(outer.size.height).into_any_element();
    ring.layout_as_root(AvailableSpace::min_size(), window, cx);
    window.defer_draw(ring, outer.origin, OUTLINE_PRIORITY, Some(ContentMask { bounds: visible.dilate(px(OUTLINE_EXTENT)) }));
}

/// 程序自己画的图标（不在 Lucide 目录里的），SVG 由 [`crate::AppAssets`] 提供。用法和 `IconName` 一样：`Icon::new(AppIcon::…)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppIcon {
    /// 通用无障碍标志：圆圈里张开双臂的人形（设置 → 辅助功能）。
    UniversalAccess,
}

impl gpui_kit::assets::IconNamed for AppIcon {
    fn path(self) -> SharedString {
        match self {
            AppIcon::UniversalAccess => crate::assets::UNIVERSAL_ACCESS_PATH.into(),
        }
    }
}

/// 开关（设置里的开/关项）。读屏软件读作“开关，开 / 关”；名称、说明由调用方设
/// （`accessibility_label`、`aria_description`），再加 `on_click`（参数里没有新状态，自己取反）。
///
/// 不用组件库的开关：它不能带说明，焦点框也只是一圈半透明的光晕，开了高可见性轮廓也没有外圈。
pub fn switch(id: impl Into<ElementId>, checked: bool, p: &Palette) -> Pressable {
    const TRACK_W: f32 = 36.;
    const TRACK_H: f32 = 20.;
    const THUMB: f32 = 16.;
    const INSET: f32 = 2.;
    pressable(id, p)
        .role(Role::Switch)
        .aria_toggled(if checked { Toggled::True } else { Toggled::False })
        .flex_none()
        // 2px 透明焦点框在轨道外面：焦点框贴着轨道，不压在轨道上。
        .w(px(TRACK_W + FOCUS_BORDER * 2.))
        .h(px(TRACK_H + FOCUS_BORDER * 2.))
        .rounded_full()
        .cursor_pointer()
        .child(motion::SwitchTrack {
            // 在开关自己的 id 下面，不会和别的开关混。
            id: "switch-track".into(),
            checked,
            track: (p.text_secondary, p.accent),
            thumb: (p.surface, p.on_accent),
            width: px(TRACK_W),
            height: px(TRACK_H),
            thumb_size: px(THUMB),
            inset: px(INSET),
        })
}

/// 自己画焦点框的元素（不是 [`pressable`]，如会话消息、下拉框）用：按是否聚焦给它加上焦点框。
///
/// 默认只在键盘聚焦时把 2px 的边框涂成焦点色；高可见性轮廓开启后，键盘、鼠标聚焦都显示，
/// 外面再隔一段空隙画一圈正文色的外轮廓（[`focus_outline`]）。元素要先设好 2px 的透明边框和圆角。
pub fn with_focus_ring<E: Styled + ParentElement>(element: E, focused: bool, radius: f32, _window: &Window, p: &Palette) -> E {
    let ring = p.focus_ring;
    if high_visibility_outlines() {
        if !focused {
            return element;
        }
        element.border_color(ring).child(focus_outline(Corners::all(px(radius)), FOCUS_BORDER, p))
    } else if focused && focus_from_keyboard() {
        element.border_color(ring)
    } else {
        element
    }
}

/// 可交互元素的基础：无样式按钮（自带 Enter / 空格激活、Tab 遍历、无障碍角色），
/// 加上统一的焦点框——默认只在键盘导航时显示，鼠标点击不出现；
/// 设置里打开“高可见性控件轮廓”后，聚焦（键盘或点击）都会显示，并在外面隔一段空隙再画一圈外轮廓。
pub fn pressable(id: impl Into<ElementId>, p: &Palette) -> Pressable {
    let ring = p.focus_ring;
    let id = id.into();
    let inner = gpui_kit::base::Button::new(id.clone()).border_2().border_color(transparent_black());
    let inner = if high_visibility_outlines() {
        inner.focus(move |style| style.border_color(ring))
    } else {
        // 不用 `focus_visible`（鼠标一动就藏起来），按 [`focus_from_keyboard`] 决定。
        inner.when(focus_from_keyboard(), |inner| inner.focus(move |style| style.border_color(ring)))
    };
    Pressable { id, inner, focus: None, outline: high_visibility_outlines(), focus_on_hover: false, palette: *p }
}

/// [`pressable`] 返回的按钮：组件库的无样式按钮，外加高可见性轮廓的外圈。
///
/// 外圈要在渲染时知道按钮是否聚焦，所以包了一层：焦点句柄在这里取（调用方没给时，
/// 和组件库一样按 id 存在元素状态里），再交给里面的按钮。用法和组件库的按钮完全一样。
#[derive(IntoElement)]
pub struct Pressable {
    id: ElementId,
    inner: gpui_kit::base::Button,
    focus: Option<FocusHandle>,
    /// 高可见性轮廓开启时（创建按钮那一刻的设置）：聚焦时画外圈。
    outline: bool,
    /// 鼠标移上来就获得焦点（菜单项、列表项）。
    focus_on_hover: bool,
    palette: Palette,
}

impl Pressable {
    pub fn selected(mut self, selected: bool) -> Self {
        self.inner = self.inner.selected(selected);
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.inner = self.inner.disabled(disabled);
        self
    }

    pub fn accessibility_label(mut self, label: impl Into<SharedString>) -> Self {
        self.inner = self.inner.accessibility_label(label);
        self
    }

    pub fn role(mut self, role: impl Into<gpui_kit::base::RoleOverride>) -> Self {
        self.inner = self.inner.role(role);
        self
    }

    pub fn track_focus(mut self, focus_handle: &FocusHandle) -> Self {
        self.focus = Some(focus_handle.clone());
        self
    }

    pub fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.inner = self.inner.on_click(handler);
        self
    }

    pub fn tab_index(mut self, tab_index: isize) -> Self {
        self.inner = self.inner.tab_index(tab_index);
        self
    }

    /// 鼠标移到这一项上就把焦点给它，读屏软件随之读出（和系统菜单、组合框列表一样，不只靠键盘）。
    /// 用在弹出菜单的项上；之后按方向键从这一项接着走。触屏上没有悬停，不起作用。
    pub fn focus_on_hover(mut self) -> Self {
        self.focus_on_hover = true;
        self
    }
}

impl RenderOnce for Pressable {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Pressable { id, mut inner, focus, outline, focus_on_hover, palette } = self;
        // 和组件库的按钮取句柄的方式一样（同一个 id、同一层），换成这里取不会改变焦点行为。
        let focus = focus.unwrap_or_else(|| window.use_keyed_state(id, cx, |_, cx| cx.focus_handle()).read(cx).clone());
        let focused = outline && focus.is_focused(window);
        if focused {
            let rem = window.rem_size();
            let radii = &inner.style().corner_radii;
            let radius = |value: Option<AbsoluteLength>| value.map(|value| value.to_pixels(rem)).unwrap_or_default();
            let corners = Corners {
                top_left: radius(radii.top_left),
                top_right: radius(radii.top_right),
                bottom_left: radius(radii.bottom_left),
                bottom_right: radius(radii.bottom_right),
            };
            inner = inner.child(focus_outline(corners, FOCUS_BORDER, &palette));
        }
        if focus_on_hover {
            let focus = focus.clone();
            inner = inner.on_mouse_move(move |_: &MouseMoveEvent, window, cx| {
                if !focus.is_focused(window) && !window.last_input_was_touch() {
                    focus.focus(window, cx);
                }
            });
        }
        inner.track_focus(&focus)
    }
}

impl Styled for Pressable {
    fn style(&mut self) -> &mut StyleRefinement {
        self.inner.style()
    }
}

impl ParentElement for Pressable {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.inner.extend(elements);
    }
}

impl InteractiveElement for Pressable {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.inner.interactivity()
    }
}

impl StatefulInteractiveElement for Pressable {}

impl gpui_kit::component::Selectable for Pressable {
    fn selected(self, selected: bool) -> Self {
        Pressable::selected(self, selected)
    }

    fn is_selected(&self) -> bool {
        gpui_kit::component::Selectable::is_selected(&self.inner)
    }
}

/// 基于 [`pressable`] 的控件的悬停、选中底色（带过渡）。作为控件的第一个子元素，控件本身不再设
/// `bg` / `hover`；铺满整个控件（含 2px 透明焦点框），和直接设 `bg` 一样大。`active` 时一直显示（选中、菜单打开）。
pub fn hover_fill(color: Hsla, corner: f32, active: bool) -> motion::Highlight {
    motion::highlight("hover-fill", color, corner, active).outset(FOCUS_BORDER)
}

/// 导航项（设置弹窗左侧）。调用方再加 `on_click`。
///
/// 读屏软件上是一组标签页（外层用 [`Role::TabList`]），当前页读作“已选择”。
/// `Pressable::selected` 只管样式，选中状态要另外用 `aria_selected` 告诉读屏软件。
pub fn nav_item(id: impl Into<ElementId>, icon: impl Into<Icon>, label: impl Into<SharedString>, selected: bool, p: &Palette) -> Pressable {
    let selected_bg = p.selected;
    let label = label.into();
    pressable(id, p)
        .role(Role::Tab)
        .accessibility_label(label.clone())
        .selected(selected)
        .aria_selected(selected)
        .min_h(px(size::NAV_ITEM))
        .px(px(space::M))
        .flex()
        .items_center()
        .gap(px(space::S))
        .rounded(px(radius::MD))
        .cursor_pointer()
        .text_label()
        .relative()
        .when(!selected, |item| item.font_weight(FontWeight::NORMAL))
        .child(hover_fill(selected_bg, radius::MD, selected))
        .child(Icon::new(icon).size(px(size::ICON_MD)))
        .child(label)
}

/// 列表项（侧边栏、菜单）。调用方再加 `on_click`；`icon` 可选。
///
/// 默认是按钮。`selected` 只改样式：需要读屏软件读出“已选择 / 已勾选”时，调用方另设角色
/// （[`Role::ListItem`] 配 `aria_selected`，或 [`menu_choice`] 用的单选菜单项）。
pub fn list_item(
    id: impl Into<ElementId>,
    icon: Option<IconName>,
    label: impl Into<SharedString>,
    selected: bool,
    p: &Palette,
) -> Pressable {
    let selected_bg = p.selected;
    let label = label.into();
    pressable(id, p)
        .accessibility_label(label.clone())
        .selected(selected)
        .min_h(px(size::LIST_ITEM))
        .px(px(space::M))
        .flex()
        .items_center()
        .gap(px(space::S))
        .rounded(px(radius::MD))
        .cursor_pointer()
        .text_body()
        .relative()
        .when(selected, |item| item.font_weight(FontWeight::MEDIUM))
        .child(hover_fill(selected_bg, radius::MD, selected))
        .when_some(icon, |item, icon| item.child(Icon::new(icon).size(px(size::ICON_MD))))
        .child(div().flex_1().min_w(px(0.)).truncate().child(label))
}

actions!(
    dshr_menu,
    [
        /// 焦点移到上一项（到头了回到最后一项）。
        MenuPrev,
        /// 焦点移到下一项（到底了回到第一项）。
        MenuNext,
        MenuFirst,
        MenuLast,
        /// 二级菜单里：回到上一级。一级菜单里按 Esc 照常关上菜单。
        MenuBack,
        /// 焦点在有二级菜单的项上时：打开它。
        MenuForward,
    ]
);

/// [`menu_popover`] 里的按键（比组件库弹出层的 Esc 深一层，所以二级菜单里 Esc 先回到上一级）。
const MENU_CONTEXT: &str = "AppMenu";

/// 注册菜单的按键。启动时调用一次。
pub fn init(cx: &mut App) {
    // 每一次按键（包括绑定成动作的 Tab、方向键）在派发之前都经过这里：之后挪过去的焦点算键盘的。
    cx.intercept_keystrokes(|_, window, _| set_focus_from_keyboard(true, window)).detach();
    cx.bind_keys([
        KeyBinding::new("up", MenuPrev, Some(MENU_CONTEXT)),
        KeyBinding::new("down", MenuNext, Some(MENU_CONTEXT)),
        KeyBinding::new("home", MenuFirst, Some(MENU_CONTEXT)),
        KeyBinding::new("end", MenuLast, Some(MENU_CONTEXT)),
        KeyBinding::new("left", MenuBack, Some(MENU_CONTEXT)),
        KeyBinding::new("escape", MenuBack, Some(MENU_CONTEXT)),
        KeyBinding::new("right", MenuForward, Some(MENU_CONTEXT)),
    ]);
}

type MenuHandler = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

/// 一个弹出菜单里各项的键盘焦点：每项一个焦点句柄（按项的序号，跨帧保留），
/// ↑ / ↓ / Home / End 在这些项之间移动，打开菜单时焦点直接落在当前选中的那一项上。
///
/// 菜单的内容每帧重建：画每一项时用 [`MenuFocus::item`] 取它的句柄交给 `Pressable::track_focus`；
/// 在可滚动的列表里的项再用 [`MenuFocus::scrolls_in`] 登记，焦点移过去时把它滚进视口。
/// 有二级菜单时用 [`MenuFocus::on_forward`]、[`MenuFocus::on_back`] 登记 → / ← 做什么。
#[derive(Clone)]
pub struct MenuFocus(Entity<MenuFocusState>);

#[derive(Default)]
struct MenuFocusState {
    handles: Vec<FocusHandle>,
    /// 这一帧画了几项。
    count: usize,
    /// 第几项在哪个滚动容器里、是它的第几个子元素。
    scroll: Vec<Option<(ScrollHandle, usize)>>,
    /// 二级菜单：← / Esc 回到上一级。
    back: Option<MenuHandler>,
    /// 有二级菜单的项：→ 打开它。
    forward: Vec<(usize, MenuHandler)>,
    /// 这次打开后已经把焦点交给了初始项（见 [`menu_popover`]）；关上时清掉。
    initial_given: bool,
}

impl MenuFocus {
    fn new(id: &ElementId, window: &mut Window, cx: &mut App) -> Self {
        Self(window.use_keyed_state((id.clone(), "menu-focus"), cx, |_, _| MenuFocusState::default()))
    }

    /// 开始画这一帧的菜单内容：清掉上一帧登记的项数、滚动容器和二级菜单。
    fn begin(&self, cx: &mut App) {
        self.0.update(cx, |state, _| {
            state.count = 0;
            state.scroll.clear();
            state.back = None;
            state.forward.clear();
        });
    }

    /// 第 `index` 项的焦点句柄（项按画出来的先后从 0 数）。
    pub fn item(&self, index: usize, cx: &mut App) -> FocusHandle {
        self.0.update(cx, |state, cx| {
            while state.handles.len() <= index {
                state.handles.push(cx.focus_handle());
            }
            state.count = state.count.max(index + 1);
            state.handles[index].clone()
        })
    }

    /// 第 `index` 项是滚动容器 `scroll` 的第 `child` 个子元素：焦点移过去时把它滚进视口。
    pub fn scrolls_in(&self, index: usize, scroll: &ScrollHandle, child: usize, cx: &mut App) {
        self.0.update(cx, |state, _| {
            if state.scroll.len() <= index {
                state.scroll.resize(index + 1, None);
            }
            state.scroll[index] = Some((scroll.clone(), child));
        });
    }

    /// 这一页是二级菜单：← / Esc 时调用 `back`（回到上一级），而不是关上菜单。
    pub fn on_back(&self, back: impl Fn(&mut Window, &mut App) + 'static, cx: &mut App) {
        self.0.update(cx, |state, _| state.back = Some(std::rc::Rc::new(back)));
    }

    /// 第 `index` 项打开一个二级菜单：焦点在它上面时按 → 调用 `forward`。
    pub fn on_forward(&self, index: usize, forward: impl Fn(&mut Window, &mut App) + 'static, cx: &mut App) {
        self.0.update(cx, |state, _| state.forward.push((index, std::rc::Rc::new(forward))));
    }

    /// 把焦点移到第 `index` 项（在滚动容器里时滚进视口）。项还没画出来也可以：下一帧画出来时它就有焦点。
    pub fn focus(&self, index: usize, window: &mut Window, cx: &mut App) {
        let handle = self.item(index, cx);
        if let Some(Some((scroll, child))) = self.0.read(cx).scroll.get(index) {
            scroll.scroll_to_item(*child);
        }
        handle.focus(window, cx);
        window.refresh();
    }

    /// 现在有焦点的是第几项。
    fn focused(&self, window: &Window, cx: &App) -> Option<usize> {
        let state = self.0.read(cx);
        state.handles[..state.count.min(state.handles.len())].iter().position(|handle| handle.is_focused(window))
    }

    fn step(&self, forward: bool, window: &mut Window, cx: &mut App) {
        let count = self.0.read(cx).count;
        if count == 0 {
            return;
        }
        let next = match (self.focused(window, cx), forward) {
            (Some(index), true) => (index + 1) % count,
            (Some(index), false) => (index + count - 1) % count,
            (None, true) => 0,
            (None, false) => count - 1,
        };
        self.focus(next, window, cx);
    }

    /// 包住菜单内容：挂上菜单的按键。
    fn container(&self, content: AnyElement) -> Div {
        let menu = self.clone();
        let (prev, next, first, last, back) = (menu.clone(), menu.clone(), menu.clone(), menu.clone(), menu.clone());
        div()
            .key_context(MENU_CONTEXT)
            .on_action(move |_: &MenuPrev, window, cx| prev.step(false, window, cx))
            .on_action(move |_: &MenuNext, window, cx| next.step(true, window, cx))
            .on_action(move |_: &MenuFirst, window, cx| first.focus(0, window, cx))
            .on_action(move |_: &MenuLast, window, cx| {
                let count = last.0.read(cx).count;
                if count > 0 {
                    last.focus(count - 1, window, cx);
                }
            })
            .on_action(move |_: &MenuBack, window, cx| match back.0.read(cx).back.clone() {
                Some(handler) => handler(window, cx),
                // 一级菜单：交给弹出层（Esc 关上菜单；← 什么也不做）。
                None => cx.propagate(),
            })
            .on_action(move |_: &MenuForward, window, cx| {
                let focused = menu.focused(window, cx);
                let handler = menu.0.read(cx).forward.iter().find(|(index, _)| Some(*index) == focused).map(|(_, handler)| handler.clone());
                match handler {
                    Some(handler) => handler(window, cx),
                    None => cx.propagate(),
                }
            })
            .child(content)
    }
}

/// 弹出菜单 `id`（和 [`menu_popover`] 的 `id` 相同）的触发按钮用的焦点句柄。触发按钮要
/// `Button::track_focus` 它、再用 [`framed_trigger_with`] 包起来：菜单打开的那一刻焦点留在这个按钮上，
/// 等菜单画出来再移到当前项。
pub fn menu_trigger_focus(id: impl Into<ElementId>, window: &mut Window, cx: &mut App) -> FocusHandle {
    window.use_keyed_state((id.into(), "menu-trigger"), cx, |_, cx| cx.focus_handle()).read(cx).clone()
}

/// 弹出菜单（权限、预设、模型、会话的“…”）。
///
/// 打开、关闭、焦点、Esc 和点外面关闭都交给组件库的 `Popover`；外观和动画在这里画：
/// 菜单从触发按钮那一侧带一点过冲地浮出来，阴影随之渐显；里面的选项错峰出现（`body` 的第二个参数为
/// `true` 时由调用方套上 [`menu_item_enter`]）。
///
/// 键盘：打开时焦点落在第 `initial` 项（当前选中的那一项），↑ / ↓ / Home / End 在各项之间移动，
/// Enter / 空格选中。`body` 用第一个参数（[`MenuFocus`]）给每一项取焦点句柄，见那里的说明。
///
/// 关上时 `Popover` 立刻不再画内容（焦点也已经回到触发按钮），这里在同一位置再画一小段不响应操作的
/// “残影”退场。不能让 `Popover` 多开一会儿：它重新打开会把焦点抢回菜单，读屏软件会再报一次。
#[allow(clippy::too_many_arguments)] // 和 `Popover` 的构造一一对应，拆成结构体反而不好读。
pub fn menu_popover<T: gpui_kit::component::Selectable + IntoElement + 'static>(
    id: impl Into<ElementId>,
    open: bool,
    anchor: Anchor,
    trigger: T,
    on_open_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
    initial: usize,
    body: impl Fn(&MenuFocus, bool, &mut Window, &mut App) -> AnyElement + 'static,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    use gpui_kit::component::popover::Popover;

    let id = id.into();
    let body = std::rc::Rc::new(body);
    let menu = MenuFocus::new(&id, window, cx);
    // 打开的那一刻不把焦点交给菜单项：组件库的 `Popover` 在按下鼠标（或 Enter）的同一个事件里就移焦点，
    // 那时菜单还没画出来，无障碍树里没有这一项，讲述人的焦点落到窗口上，把窗口标题再读一遍。
    // 所以让它“移”到触发按钮自己（焦点不变），菜单画出来之后再移到当前项（见下面 `content`）。
    let hold_focus = menu_trigger_focus(id.clone(), window, cx);
    if !open {
        menu.0.update(cx, |state, _| state.initial_given = false);
    }
    let phase = motion::presence((id.clone(), "presence"), open, motion::BASE, motion::FAST, window, cx);
    // 向上展开的菜单从下面浮上来，向下展开的从上面落下来。
    let from = match anchor {
        Anchor::BottomLeft | Anchor::BottomRight | Anchor::BottomCenter => motion::Edge::Bottom,
        _ => motion::Edge::Top,
    };
    let surface = move |content: AnyElement, phase: motion::Phase, p: &Palette| {
        motion::pop(
            div().rounded(px(radius::LG)).border_1().border_color(p.border).bg(p.raised).text_color(p.text).child(content),
            from,
            motion::SHORT,
            phase,
            shadow::menu(),
        )
    };
    let entering = phase.filter(|phase| !phase.leaving).unwrap_or(motion::Phase::SETTLED);
    let popover = Popover::new(id)
        .anchor(anchor)
        .appearance(false)
        .open(open)
        // 打开时焦点落在当前选中的项上（而不是菜单本身），方向键马上就能用，读屏软件读出当前选项。
        .track_focus(&hold_focus)
        .on_open_change(on_open_change)
        .trigger(trigger)
        .content({
            let (body, menu) = (body.clone(), menu.clone());
            move |_, window, cx| {
                let p = palette(cx);
                menu.begin(cx);
                let content = body(&menu, true, window, cx);
                let give_initial = !menu.0.read(cx).initial_given;
                let content = menu.container(content).when(give_initial, |container| {
                    let (menu, hold) = (menu.clone(), hold_focus.clone());
                    container.child(focus_after_paint(move |window, cx| {
                        if menu.0.read(cx).initial_given {
                            return;
                        }
                        menu.0.update(cx, |state, _| state.initial_given = true);
                        // 这期间焦点被别的操作移走了（比如鼠标已经移到了另一项上）：不抢。
                        if hold.is_focused(window) {
                            menu.focus(initial, window, cx);
                        }
                    }))
                });
                surface(content.into_any_element(), entering, &p)
            }
        });
    let ghost = phase.filter(|phase| phase.leaving).map(|phase| {
        let p = palette(cx);
        // 和 `Popover` 一样：贴着触发按钮的对应角，留 0.25rem 的空隙。
        let gap = window.rem_size() * 0.25;
        let (corner, offset) = match anchor {
            Anchor::BottomLeft => (div().top_0().left_0(), -gap),
            Anchor::BottomRight => (div().top_0().right_0(), -gap),
            Anchor::TopRight => (div().bottom_0().right_0(), gap),
            _ => (div().bottom_0().left_0(), gap),
        };
        corner.absolute().child(
            deferred(
                anchored()
                    .anchor(anchor)
                    .offset(point(px(0.), offset))
                    .snap_to_window_with_margin(px(space::S))
                    .child(motion::inert(surface(body(&menu, false, window, cx), phase, &p))),
            )
            .with_priority(1),
        )
    });
    div().relative().flex_none().child(popover).children(ghost).into_any_element()
}

/// 一个不占位置的元素：它画出来之后（这一帧画完）调用一次 `then`。用来把焦点交给刚出现的东西——
/// 在它画出来之前给焦点，无障碍树里还没有它，讲述人收不到这次焦点变化（见规范 5.3）。
pub fn focus_after_paint(then: impl FnOnce(&mut Window, &mut App) + 'static) -> impl IntoElement {
    let then = std::cell::Cell::new(Some(then));
    canvas(
        |_, _, _| {},
        move |_, _, window, cx| {
            if let Some(then) = then.take() {
                window.defer(cx, then);
            }
        },
    )
    .absolute()
    .size_0()
}

/// 弹出菜单里第 `index` 个选项的进场（错峰、从菜单展开的方向轻轻移入）。见 [`menu_popover`]。
pub fn menu_item_enter<E: IntoElement + Styled + 'static>(item: E, id: impl Into<ElementId>, index: usize, animate: bool) -> AnyElement {
    if !animate {
        return item.into_any_element();
    }
    motion::Entrance::new(motion::BASE)
        .from(motion::Edge::Bottom, motion::NUDGE)
        .stagger(index, motion::STAGGER_ITEM)
        .play(item, id)
        .into_any_element()
}

/// 单选菜单里的一项（权限、预设、模型）：读屏软件读作“单选菜单项，已勾选 / 未勾选”。
/// 只设语义，样式由调用方决定。
pub fn menu_choice(item: Pressable, checked: bool) -> Pressable {
    item.role(Role::MenuItemRadio).aria_toggled(if checked { Toggled::True } else { Toggled::False })
}

/// 动效：时长、缓动、进场与退场、展开、悬停过渡、加载指示。见 `design/motion.rs`。
pub mod motion;

/// 浮层的阴影。按层级取（见规范 2.5）；动画里由 [`motion::pop`] 按不透明度渐显。
pub mod shadow {
    use gpui_kit::{BoxShadow, hsla, px};

    /// 层级 1：菜单、下拉列表。
    pub fn menu() -> Vec<BoxShadow> {
        vec![
            BoxShadow::new(px(0.), px(10.), hsla(0., 0., 0., 0.1)).blur_radius(px(15.)).spread_radius(px(-3.)),
            BoxShadow::new(px(0.), px(4.), hsla(0., 0., 0., 0.1)).blur_radius(px(6.)).spread_radius(px(-4.)),
        ]
    }

    /// 层级 2：弹窗。
    pub fn modal() -> Vec<BoxShadow> {
        vec![BoxShadow::new(px(0.), px(25.), hsla(0., 0., 0., 0.25)).blur_radius(px(50.)).spread_radius(px(-12.))]
    }
}

#[cfg(test)]
mod tests {
    use super::{contrast, soften_dark};

    #[test]
    fn dark_conversation_is_darkest_layer() {
        let mut theme = gpui_kit::component::Theme::default();
        soften_dark(&mut theme);
        let colors = &theme.colors;
        let lightness = |color: gpui_kit::Hsla| color.l;
        // 对话区（页面）< 侧边栏 < 弹出层。
        assert!(lightness(colors.background) < lightness(colors.sidebar));
        assert!(lightness(colors.sidebar) < lightness(colors.popover));
        assert_eq!(colors.title_bar, colors.sidebar);
        assert!(contrast(colors.foreground, colors.background) >= 4.5);
        assert!(contrast(colors.muted_foreground, colors.background) >= 4.5);
        assert!(contrast(colors.foreground, colors.sidebar) >= 4.5);
    }

    /// 组件库按钮套上 `framed` / `framed_trigger`（菜单的触发按钮）后，Tab 到它上面时画外圈：
    /// 这一层取到的焦点句柄和按钮自己的是同一个。
    #[gpui::test]
    fn framed_component_buttons_get_the_outer_ring(cx: &mut gpui::TestAppContext) {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::popover::Popover;
        use gpui_kit::{AppContext as _, Context, IntoElement, ParentElement as _, Render, Styled as _, Window, div, px};

        struct View;
        impl Render for View {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().p(px(40.)).child(super::framed("plain", Button::new("plain").outline().label("plain"))).child(
                    Popover::new("menu")
                        .trigger(super::framed_trigger("trigger", Button::new("trigger").ghost().label("menu")))
                        .content(|_, _, _| div()),
                )
            }
        }

        super::set_high_visibility_outlines(true);
        let window = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| View)).unwrap().0
        });
        let draw = |cx: &mut gpui::TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap();
        };
        draw(cx);
        let drawn = || super::OUTLINES_DRAWN.with(|count| count.get());
        let before = drawn();
        draw(cx);
        assert_eq!(drawn(), before, "没有焦点时不画");
        for name in ["plain", "trigger"] {
            cx.update_window(window, |_, window, cx| window.focus_next(cx)).unwrap();
            let before = drawn();
            draw(cx);
            assert_eq!(drawn(), before + 1, "{name} 有焦点时应该画一圈外轮廓");
        }
        super::set_high_visibility_outlines(false);
    }

    /// 打开菜单的那一刻焦点不能落到还没画出来的菜单项上（讲述人会把焦点报成窗口、再读一遍窗口标题）：
    /// 先留在触发按钮上，菜单画出来之后才到当前项；关上后回到触发按钮。
    #[gpui::test]
    fn menu_focus_moves_in_only_after_the_menu_is_painted(cx: &mut gpui::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::{AppContext as _, Context, FocusHandle, IntoElement, ParentElement as _, Render, Styled as _, Window, div, px};

        #[derive(Default)]
        struct Seen {
            trigger: Option<FocusHandle>,
            items: Vec<FocusHandle>,
            /// 每一帧开始画时谁有焦点，这一帧有没有画菜单项。
            frames: Vec<(Option<FocusHandle>, bool)>,
        }
        struct View {
            open: bool,
            seen: Rc<RefCell<Seen>>,
        }
        impl Render for View {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                self.seen.borrow_mut().frames.push((window.focused(cx), false));
                let trigger_focus = super::menu_trigger_focus("menu", window, cx);
                self.seen.borrow_mut().trigger = Some(trigger_focus.clone());
                let trigger = super::framed_trigger_with(
                    trigger_focus.clone(),
                    Button::new("trigger").track_focus(&trigger_focus).ghost().label("menu"),
                );
                let seen = self.seen.clone();
                let view = cx.entity().downgrade();
                div().size_full().p(px(40.)).child(super::menu_popover(
                    "menu",
                    self.open,
                    gpui_kit::Anchor::TopLeft,
                    trigger,
                    move |open, _, cx| {
                        view.update(cx, |view, cx| {
                            view.open = *open;
                            cx.notify();
                        })
                        .ok();
                    },
                    1,
                    move |menu, _, _, cx| {
                        let p = super::palette(cx);
                        let items: Vec<FocusHandle> = (0..3).map(|index| menu.item(index, cx)).collect();
                        let mut seen = seen.borrow_mut();
                        seen.items = items.clone();
                        if let Some(frame) = seen.frames.last_mut() {
                            frame.1 = true;
                        }
                        gpui_kit::component::v_flex()
                            .children(items.iter().enumerate().map(|(index, handle)| {
                                super::list_item(("item", index), None, format!("item {index}"), false, &p).track_focus(handle)
                            }))
                            .into_any_element()
                    },
                    window,
                    cx,
                ))
            }
        }

        let seen = Rc::new(RefCell::new(Seen::default()));
        let (window, view) = cx.update(|cx| {
            gpui_kit::init(cx);
            super::init(cx);
            let seen = seen.clone();
            gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| View { open: false, seen })).unwrap()
        });
        let draw = |cx: &mut gpui::TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap();
            cx.run_until_parked();
        };
        let focused = |cx: &mut gpui::TestAppContext| cx.update_window(window, |_, window, cx| window.focused(cx)).unwrap();
        draw(cx);
        let trigger = seen.borrow().trigger.clone().unwrap();
        cx.update_window(window, |_, window, cx| trigger.focus(window, cx)).unwrap();
        draw(cx);

        cx.simulate_keystrokes(window, "enter");
        assert!(cx.update(|cx| view.read(cx).open), "Enter 打开了菜单");
        draw(cx);
        draw(cx);
        let item = seen.borrow().items.get(1).cloned().expect("菜单画出来了");
        assert_eq!(focused(cx).as_ref(), Some(&item), "画出来之后焦点到当前项");
        // 第一次画菜单项的那一帧开始时，焦点还在触发按钮上（不是还没进无障碍树的菜单项）。
        let first = seen.borrow().frames.iter().find(|(_, items)| *items).map(|(focused, _)| focused.clone());
        assert_eq!(first, Some(Some(trigger.clone())), "菜单还没画出来时焦点留在触发按钮上");

        cx.simulate_keystrokes(window, "escape");
        draw(cx);
        assert_eq!(focused(cx).as_ref(), Some(&trigger), "关上后回到触发按钮");
    }

    /// 内置高对比度配色按 WCAG AAA：文字在每种底色上不低于 7:1，描边和焦点框不低于 3:1，
    /// 强调色按钮上的字不低于 7:1。
    #[test]
    fn high_contrast_meets_aaa() {
        let theme = crate::backend::builtin_high_contrast_theme();
        let over = super::PaletteOverride::parse(&theme.light, &theme.dark).unwrap();
        for (mode, set) in [("light", &over.light), ("dark", &over.dark)] {
            let color = |key: &str| set.iter().find(|(name, _)| name == key).map(|(_, color)| *color).unwrap();
            let backgrounds = ["surface", "raised", "sidebar", "sunken", "selected", "accent_soft"];
            for text in ["text", "text_secondary", "accent", "success", "warning", "danger"] {
                for background in backgrounds {
                    let ratio = contrast(color(text), color(background));
                    assert!(ratio >= 7., "{mode}: {text} on {background} is {ratio:.2}:1");
                }
            }
            for line in ["border", "focus_ring"] {
                for background in backgrounds {
                    let ratio = contrast(color(line), color(background));
                    assert!(ratio >= 3., "{mode}: {line} on {background} is {ratio:.2}:1");
                }
            }
            let ratio = contrast(color("on_accent"), color("accent"));
            assert!(ratio >= 7., "{mode}: on_accent on accent is {ratio:.2}:1");
        }
    }
}
