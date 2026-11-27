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

/// 圆角阶梯（参照 Windows 11、VS Code、Linear 等桌面工具：控件 6、浮层与卡片 8、大容器 12）。
/// 嵌套时内层圆角 = 外层圆角 − 间距，且不小于 `SM`。
pub mod radius {
    /// 菜单项、下拉选项、键帽、紧凑的小元素。
    pub const SM: f32 = 4.;
    /// 按钮、输入框、下拉触发框、列表项、导航项。
    pub const MD: f32 = 6.;
    /// 卡片、菜单、下拉列表、面板、代码块。
    pub const LG: f32 = 8.;
    /// 弹窗、用户消息气泡。
    pub const XL: f32 = 12.;
    /// 只用于输入卡片和占用同一位置的审批面板：整个窗口里最显眼的输入区，圆角比卡片、弹窗大一档。
    pub const COMPOSER: f32 = 18.;
}

/// 控件、图标尺寸与布局常量。
pub mod size {
    /// 行内的图标按钮（列表行、卡片标题里的操作）。
    pub const CONTROL_XS: f32 = 24.;
    /// 工具栏、输入卡片底栏、卡片内的操作。
    pub const CONTROL_SM: f32 = 28.;
    /// 表单、弹窗里的按钮、输入框、下拉框（默认）。
    pub const CONTROL_MD: f32 = 32.;
    pub const ICON_SM: f32 = 14.;
    pub const ICON_MD: f32 = 16.;
    pub const ICON_LG: f32 = 20.;
    pub const ICON_HERO: f32 = 32.;
    pub const STATUS_DOT: f32 = 8.;
    pub const LIST_ITEM: f32 = 32.;
    pub const NAV_ITEM: f32 = 32.;
    /// 标题栏和侧边栏顶上的品牌行（两者对齐）。
    pub const TITLE_BAR: f32 = 40.;
    pub const SIDEBAR_WIDTH: f32 = 260.;
    /// 收起后的侧边栏：一列图标按钮。
    pub const SIDEBAR_RAIL_WIDTH: f32 = 56.;
    /// 窗口比这窄时侧边栏自动收成图标栏（不改用户的设置），对话区至少留出约 580。
    pub const SIDEBAR_AUTO_COLLAPSE: f32 = 840.;
    /// 消息列与输入卡片的最大宽度（含左右各 24 的内边距，正文约 752）。
    pub const CONTENT_MAX_WIDTH: f32 = 800.;
    pub const MODAL_WIDTH: f32 = 960.;
    pub const MODAL_HEIGHT: f32 = 720.;
    pub const MODAL_NAV_WIDTH: f32 = 220.;
    pub const MODAL_CONTENT_MAX_WIDTH: f32 = 680.;
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
    /// 悬停时叠在控件上的底色（正文色 5%，半透明，任何底色上都看得出来）。由其他颜色派生，配色方案不能直接改。
    pub hover: Hsla,
    /// 按住时叠在控件上的底色（正文色 9%）。由其他颜色派生。
    pub pressed: Hsla,
    /// 输入框、下拉触发框、描边按钮的描边（正文色 16%，比卡片描边 `border` 清楚）。高对比度时等于 `border`。
    pub control_border: Hsla,
    /// 选中项（和高对比度下悬停的行）上的文字色，配在 `selected` 上。配色方案不设时等于 `text`；
    /// 跟随系统的高对比度主题时是系统的“选定文本”文字色（如高对比白里的白字配紫底）。
    pub on_selected: Hsla,
    /// 用的是高对比度配色：行的悬停、选中铺 `selected`、文字取 `on_selected`，按钮改为加粗描边，见 [`HoverFill`]、`FocusFrame`。
    pub high_contrast: bool,
}

pub fn palette(cx: &App) -> Palette {
    let theme = cx.theme();
    let dark = theme.is_dark();
    let mut palette = base_palette(cx, dark);
    let over = cx.try_global::<PaletteOverride>();
    if let Some(over) = over {
        over.apply(&mut palette, dark);
    }
    derive_roles(&mut palette, over.is_some_and(|over| over.high_contrast));
    palette
}

/// 由其他颜色算出来的角色：配色方案（包括高对比度）改了正文色，它们跟着变。
fn derive_roles(p: &mut Palette, high_contrast: bool) {
    p.hover = p.text.opacity(0.05);
    p.pressed = p.text.opacity(0.09);
    // 高对比度的 `border` 已经按 3:1 校验过，直接用它。
    p.control_border = if high_contrast { p.border } else { p.text.opacity(0.16) };
    p.high_contrast = high_contrast;
    if p.on_selected.a == 0. {
        p.on_selected = p.text;
    }
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
    /// 这是内置的高对比度配色（由调用方在套用前设置）。
    pub high_contrast: bool,
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
        let over = Self { light: parse_set(light)?, dark: parse_set(dark)?, high_contrast: false };
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
                "on_selected" => &mut palette.on_selected,
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
    colors.secondary_hover = p.selected.blend(p.hover);
    colors.secondary_foreground = p.text;
    colors.list_hover = p.selected;
    colors.list_active = p.selected;
    // 按钮的常态、悬停、按下色和圆角由 `control_theme` 统一设（所有主题都走它）。
    colors.link = p.accent;
    colors.link_hover = p.accent;
    colors.link_active = p.accent;
    colors.danger = p.danger;
    colors.success = p.success;
    colors.warning = p.warning;
    // 文本选区：高对比度时用选定色的 40%（跟随系统主题时 `accent_soft` 就是背景色，选区看不出来），文字仍可读。
    colors.selection = if p.high_contrast { p.selected.opacity(0.4) } else { p.accent_soft };
    // 关着的开关：轨道要和底色分得开（非文字元素至少 3:1）；开着的是强调色。
    colors.switch = p.text_secondary;
    colors.switch_thumb = p.surface;
    colors.scrollbar_thumb = p.text_secondary;
    colors.scrollbar_thumb_hover = p.text;
    colors.ring = p.focus_ring;
}

/// 配色方案能改的颜色（`Palette` 的字段名）。
pub const PALETTE_KEYS: [&str; 16] = [
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
    "on_selected",
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
        // 由 `derive_roles` 算出。
        hover: transparent_black(),
        pressed: transparent_black(),
        control_border: transparent_black(),
        // 透明表示配色方案没设，`derive_roles` 里换成正文色。
        on_selected: transparent_black(),
        high_contrast: false,
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

    // 主要按钮、ghost 按钮的悬停与按下由 `control_theme` 按调色板统一设置。
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
    // 中性的说明在高对比度下跟着所在行的文字色（见 `RowColor`）；成功、警告、错误保留颜色。
    let hint = div().text_caption();
    let hint = if tone == Tone::Neutral { hint.row_color(tone.color(p), p) } else { hint.text_color(tone.color(p)) };
    hint.child(readable(text))
}

/// 正文说明（次要色）。
pub fn description(text: impl Into<SharedString>, p: &Palette) -> Div {
    div().text_body().row_color(p.text_secondary, p).child(readable(text))
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
    // 放不下时控件换到标题下面一行（窗口窄时），标题至少留 160，不会被挤成一字一行。
    h_flex()
        .id(id)
        .flex_wrap()
        .py(px(space::M))
        .gap_x(px(space::L))
        .gap_y(px(space::S))
        .items_center()
        .child(
            v_flex()
                .flex_1()
                .min_w(px(160.))
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
    card(p).px(px(space::L)).py(px(space::XS)).child(list)
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
    div().bg(p.sunken).rounded(px(radius::LG)).p(px(space::L))
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

thread_local! {
    /// 最近一次（不是 Tab 的）按键之前焦点在哪、什么时候按的：之后不久焦点变了，就算键盘挪过去的。
    static PENDING_KEY_FOCUS: std::cell::RefCell<Option<(Option<FocusHandle>, std::time::Instant)>> =
        const { std::cell::RefCell::new(None) };
}

/// 按键之后这么久之内焦点变了，都算这次按键挪的（要等一两帧才移焦点的菜单、弹窗）。
const KEY_FOCUS_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// 看最近一次按键之后焦点有没有变：变了就记成键盘焦点。按键派发完调一次；窗口的根视图每次渲染时
/// 也调一次（在子视图之前），接住要等画完一帧才移过去的焦点。
pub fn settle_focus_source(window: &mut Window, cx: &mut App) {
    let Some((before, at)) = PENDING_KEY_FOCUS.with(|pending| pending.borrow().clone()) else {
        return;
    };
    if at.elapsed() > KEY_FOCUS_WINDOW {
        PENDING_KEY_FOCUS.with(|pending| pending.borrow_mut().take());
        return;
    }
    if window.focused(cx) != before {
        PENDING_KEY_FOCUS.with(|pending| pending.borrow_mut().take());
        set_focus_from_keyboard(true, window);
    }
}

/// 窗口根元素上调用：按下鼠标（或手指）时把 [`focus_from_keyboard`] 记成假。
/// 按键那一半在 [`init`] 里用 `intercept_keystrokes` 记：绑定了动作的键（Tab、方向键）派发成动作后
/// 不再走元素的按键监听，在根元素上捕获不到。
pub fn track_focus_source<E: InteractiveElement>(element: E) -> E {
    element.capture_any_mouse_down(|_, window, _| {
        // 鼠标点下去挪的焦点不算前面那次按键的。
        PENDING_KEY_FOCUS.with(|pending| pending.borrow_mut().take());
        set_focus_from_keyboard(false, window)
    })
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
///
/// 包着的是按钮时，这一层还在按钮上面叠悬停层、按下层，以 `FAST` 渐变（见 [`motion::Tint`]）：组件库按钮自己的
/// 悬停、按下是即时换色，没法改成渐变，所以 [`control_theme`] 把它们设成与常态相同、ghost 按钮改用 [`ControlSize::subtle`]，
/// 变色都由这一层画。[`framed`]、[`framed_with`]、菜单触发按钮默认叠；[`framed_handle`] 包的多是输入框，要叠时调 [`FocusFrame::tint`]。
pub struct FocusFrame {
    key: Option<ElementId>,
    handle: Option<FocusHandle>,
    child: AnyElement,
    /// 悬停层、按下层的状态存在哪个 id 下；`None` 表示不叠。
    tint: Option<ElementId>,
    /// 按钮不可用：不叠悬停层、按下层。
    inert: bool,
    /// 菜单打开着：一直铺一层按下色，表示“展开中”。
    open: bool,
    tint_state: Option<Entity<motion::Tint>>,
}

fn tint_id(id: &ElementId) -> ElementId {
    ElementId::NamedChild(std::sync::Arc::new(id.clone()), "tint".into())
}

pub fn framed(id: impl Into<ElementId>, child: impl IntoElement) -> FocusFrame {
    let id = id.into();
    FocusFrame {
        tint: Some(tint_id(&id)),
        key: Some(id),
        handle: None,
        child: child.into_any_element(),
        inert: false,
        open: false,
        tint_state: None,
    }
}

pub fn framed_handle(handle: FocusHandle, child: impl IntoElement) -> FocusFrame {
    FocusFrame { key: None, handle: Some(handle), child: child.into_any_element(), tint: None, inert: false, open: false, tint_state: None }
}

impl FocusFrame {
    /// 包着的是按钮：叠悬停层、按下层，状态存在 `id` 下（同一视图里不能重复）。
    pub fn tint(mut self, id: impl Into<ElementId>) -> Self {
        self.tint = Some(tint_id(&id.into()));
        self
    }

    /// 按钮不可用时调用：不叠悬停层、按下层（和按钮的 `.disabled(..)` 传同一个值）。
    pub fn inert(mut self, inert: bool) -> Self {
        self.inert = inert;
        self
    }
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
    FramedTrigger { id: id.into(), handle: None, trigger, open: false }
}

/// 同 [`framed_trigger`]，按钮用的是自己给的焦点句柄（`Button::track_focus`，如 [`menu_trigger_focus`]）。
/// `id` 只用来存悬停层、按下层的状态，同一视图里不能重复。
pub fn framed_trigger_with<E: gpui_kit::component::Selectable + IntoElement + 'static>(
    id: impl Into<ElementId>,
    handle: FocusHandle,
    trigger: E,
) -> FramedTrigger<E> {
    FramedTrigger { id: id.into(), handle: Some(handle), trigger, open: false }
}

pub struct FramedTrigger<E> {
    id: ElementId,
    handle: Option<FocusHandle>,
    trigger: E,
    /// 菜单打开着（`Popover` 通过 `Selectable::selected` 告诉触发器）。
    open: bool,
}

impl<E: gpui_kit::component::Selectable + IntoElement + 'static> gpui_kit::component::Selectable for FramedTrigger<E> {
    fn selected(mut self, selected: bool) -> Self {
        self.open |= selected;
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
        self.open |= open;
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
        let mut frame = framed_with(self.id, self.handle, self.trigger);
        frame.open = self.open;
        frame
    }
}

/// 按钮可能给了 `track_focus`（`Some`），也可能没有（按 id 取）。
/// 包的总是按钮，都叠悬停层、按下层（状态存在 `id` 下）。
pub fn framed_with(id: impl Into<ElementId>, handle: Option<FocusHandle>, child: impl IntoElement) -> FocusFrame {
    let id = id.into();
    match handle {
        Some(handle) => framed_handle(handle, child).tint(id),
        None => framed(id, child),
    }
}

impl IntoElement for FocusFrame {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// [`FocusFrame`] 在 prepaint 和 paint 之间传的东西。
pub struct FramePaint {
    /// 高可见性轮廓开启时：控件所在的点击区域（点下去时把焦点给控件，见 `paint`）。
    focus_hitbox: Option<Hitbox>,
    /// 叠悬停层、按下层时：它们的点击区域（判断悬停、按下）。
    tint: Option<Hitbox>,
}

impl Element for FocusFrame {
    type RequestLayoutState = ();
    type PrepaintState = FramePaint;

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
        if let Some(id) = self.tint.clone().filter(|_| !self.inert) {
            self.tint_state = Some(window.use_keyed_state(id, cx, |_, _| motion::Tint::new()));
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
    ) -> FramePaint {
        // 先于控件插入：在控件的点击区域下面，不挡控件自己的悬停、点击。
        let focus_hitbox =
            (high_visibility_outlines() && self.handle.is_some()).then(|| window.insert_hitbox(bounds, HitboxBehavior::Normal));
        self.child.prepaint(window, cx);
        // 悬停层的点击区域插在按钮之后（上面）：组件库按钮挡住下面的点击区域，插在下面就永远不算悬停。
        // 这一块不挡任何东西，按钮照常收到悬停和点击。
        let tint = self.tint_state.is_some().then(|| window.insert_hitbox(bounds, HitboxBehavior::Normal));
        if high_visibility_outlines() && self.handle.as_ref().is_some_and(|handle| handle.is_focused(window)) {
            let radius = cx.theme().radius;
            defer_outline(bounds, Corners::all(radius), window, cx);
        }
        FramePaint { focus_hitbox, tint }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut FramePaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        // 悬停层、按下层叠在按钮上面（半透明，文字仍清楚）：常态、悬停、按下之间渐变。
        let radius = cx.theme().radius;
        let p = palette(cx);
        if self.open {
            if p.high_contrast {
                window.paint_quad(quad(bounds, radius, transparent_black(), px(2.), hc_line(&p), BorderStyle::Solid));
            } else {
                window.paint_quad(fill(bounds, p.pressed).corner_radii(radius));
            }
        }
        if let (Some(hitbox), Some(tint)) = (state.tint.as_ref(), self.tint_state.as_ref()) {
            let tint = tint.read(cx).clone();
            if p.high_contrast {
                // 高对比度：按钮的文字颜色由组件库固定，叠不透明的底色会盖住它，改为加粗描边。
                tint.paint_ring(bounds, radius, hc_line(&p), hitbox, window);
            } else {
                tint.paint(bounds, radius, true, (Some(p.hover), Some(p.pressed)), hitbox, window, cx);
            }
        }
        let hitbox = &mut state.focus_hitbox;
        // 组件库的按钮按下鼠标时 `prevent_default`，点了也不拿焦点（为了不把焦点从输入框抢走）：
        // 所以 Tab 过去有外圈，鼠标点了却没有。高可见性轮廓开启时说好了“键盘、鼠标聚焦都显示”，
        // 这里在按下时直接把焦点给它（`focus` 不看 `prevent_default`）。
        if let (Some(hitbox), Some(handle)) = (hitbox.take(), self.handle.clone()) {
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble
                    && event.button == MouseButton::Left
                    && hitbox.is_hovered(window)
                    && !handle.is_focused(window)
                {
                    handle.focus(window, cx);
                }
            });
        }
    }
}

/// 测试用：高可见性轮廓的开关是整个进程共用的，改它的测试拿着这把锁，不和别的这类测试同时跑。
#[cfg(test)]
pub(crate) static HIGH_VISIBILITY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        .dim_when_disabled()
        .relative()
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
        // 悬停、按下时在轨道上叠一层（画在轨道上面，轨道是实色）。
        .child(motion::highlight("switch-state", transparent_black(), TRACK_H / 2., false).hover_color(p.hover).pressed_color(p.pressed))
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
    Pressable {
        id,
        inner,
        focus: None,
        outline: high_visibility_outlines(),
        focus_on_hover: false,
        disabled: false,
        dim_disabled: false,
        palette: *p,
    }
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
    disabled: bool,
    /// 禁用时整体变淡（40% 不透明度）。自己管不透明度的控件（如发送按钮）不设。
    dim_disabled: bool,
    palette: Palette,
}

impl Pressable {
    pub fn selected(mut self, selected: bool) -> Self {
        self.inner = self.inner.selected(selected);
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.inner = self.inner.disabled(disabled);
        self.disabled = disabled;
        self
    }

    /// 禁用时整体变淡到 40%，看得出不能操作。
    pub fn dim_when_disabled(mut self) -> Self {
        self.dim_disabled = true;
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
        let Pressable { id, mut inner, focus, outline, focus_on_hover, disabled, dim_disabled, palette } = self;
        if disabled && dim_disabled {
            inner = inner.opacity(0.4).cursor_default();
        }
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

/// 列表项、行的悬停、按下、选中底色（画在最底层的 [`motion::Highlight`]），作为行自己的方法调用。
///
/// 平时悬停叠 `hover`、按下叠 `pressed`（正文色 5% / 9%），以 `FAST` 渐变。高对比度时按 Windows 对比度主题的做法：
/// 悬停、按下、选中的行都铺“选定文本”的底色 `selected`，行的文字换成 `on_selected`（系统高对比白里是紫底白字；
/// 内置高对比度配色里是浅蓝底黑字），即时切换。行里带颜色的文字、图标用 [`RowColor::row_color`] / [`row_icon`]，
/// 高对比度下跟着行的文字色走。
pub trait HoverFill: ParentElement + InteractiveElement + Styled + Sized {
    fn hover_fill(self, p: &Palette, corner: f32, selected: bool) -> Self {
        self.hover_fill_with(p.selected, p, corner, selected)
    }

    /// 同 [`HoverFill::hover_fill`]，选中时的底色另给（如侧边栏里正在看的会话用强调色的浅底）。
    fn hover_fill_with(self, selected_fill: Hsla, p: &Palette, corner: f32, selected: bool) -> Self {
        if p.high_contrast {
            let on_selected = p.on_selected;
            let layer = motion::highlight("hover-fill", p.selected, corner, selected)
                .hover_color(p.selected)
                .pressed_color(p.selected)
                .instant()
                .outset(FOCUS_BORDER);
            let row = self.child(layer);
            let row = if selected { row.text_color(on_selected) } else { row };
            return row.hover(move |style| style.text_color(on_selected));
        }
        self.child(
            motion::highlight("hover-fill", selected_fill, corner, selected)
                .hover_color(p.hover)
                .pressed_color(p.pressed)
                .outset(FOCUS_BORDER),
        )
    }
}

/// 高对比度下按钮悬停、展开时描边的颜色：跟随系统对比度主题时用“选定文本”的底色（与 Windows 按钮悬停一致）；
/// 内置配色的选中底色很浅，描不出来，用正文色。
fn hc_line(p: &Palette) -> Hsla {
    if p.on_selected != p.text { p.selected } else { p.text }
}

/// 行里带颜色的文字、图标用它设颜色：高对比度下不设，跟着行的文字色走（行被悬停、选中时换成 `on_selected`）。
///
/// 不能用 `group_hover` 换颜色：文字的颜色在排版时就定了，那时还不知道所在的行是否被悬停。跟随系统的高对比度主题里
/// 次要文字、状态颜色本来都等于正文色，不丢信息。
pub trait RowColor: Styled + Sized {
    fn row_color(self, color: Hsla, p: &Palette) -> Self {
        if p.high_contrast { self } else { self.text_color(color) }
    }
}

impl<T: Styled + Sized> RowColor for T {}

/// 行里带颜色的图标（见 [`RowColor`]）：颜色放在外面一层上，图标不设颜色时取文字色。
pub fn row_icon(icon: impl IntoElement, color: Hsla, p: &Palette) -> Div {
    h_flex().flex_none().row_color(color, p).child(icon)
}

impl<T: ParentElement + InteractiveElement + Styled + Sized> HoverFill for T {}

/// 导航项（设置弹窗左侧）。调用方再加 `on_click`。
///
/// 读屏软件上是一组标签页（外层用 [`Role::TabList`]），当前页读作“已选择”。
/// `Pressable::selected` 只管样式，选中状态要另外用 `aria_selected` 告诉读屏软件。
pub fn nav_item(id: impl Into<ElementId>, icon: impl Into<Icon>, label: impl Into<SharedString>, selected: bool, p: &Palette) -> Pressable {
    let label = label.into();
    pressable(id, p)
        .role(Role::Tab)
        .accessibility_label(label.clone())
        .selected(selected)
        .aria_selected(selected)
        .min_h(px(size::NAV_ITEM))
        .px(px(10.))
        .flex()
        // 无样式按钮默认把内容居中；导航项和侧边栏的列表项一样靠左。
        .justify_start()
        .items_center()
        .gap(px(space::S))
        .rounded(px(radius::MD))
        .cursor_pointer()
        .text_label()
        .relative()
        // 当前页加粗（不论是否高对比度），其他页常规字重。
        .font_weight(if selected { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
        .hover_fill(p, radius::MD, selected)
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
    list_item_with_fill(id, icon, label, selected, p.selected, p)
}

/// 同 [`list_item`]，选中时的底色另给（悬停仍是中性的 `selected`）。用在“当前项”要和悬停分得开的地方，
/// 如侧边栏里正在看的会话。
pub fn list_item_with_fill(
    id: impl Into<ElementId>,
    icon: Option<IconName>,
    label: impl Into<SharedString>,
    selected: bool,
    selected_fill: Hsla,
    p: &Palette,
) -> Pressable {
    let label = label.into();
    pressable(id, p)
        .accessibility_label(label.clone())
        .selected(selected)
        .min_h(px(size::LIST_ITEM))
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(space::S))
        .rounded(px(radius::MD))
        .cursor_pointer()
        .text_body()
        .relative()
        // 选中的会话加粗（不论是否高对比度）。
        .when(selected, |item| item.font_weight(FontWeight::SEMIBOLD))
        .hover_fill_with(selected_fill, p, radius::MD, selected)
        .when_some(icon, |item, icon| item.child(Icon::new(icon).size(px(size::ICON_MD))))
        .child(div().flex_1().min_w(px(0.)).truncate().child(label))
}

/// 弹出菜单里的一项（模型、会话的“…”等）：比列表项紧凑一些，圆角 `SM`（菜单圆角 `LG` 减去内边距 4）。
/// 参数和 [`list_item`] 一样；`selected` 只改样式，读屏软件的勾选状态另用 [`menu_choice`]。
pub fn menu_item(
    id: impl Into<ElementId>,
    icon: Option<IconName>,
    label: impl Into<SharedString>,
    selected: bool,
    p: &Palette,
) -> Pressable {
    let label = label.into();
    pressable(id, p)
        .accessibility_label(label.clone())
        .selected(selected)
        .min_h(px(size::LIST_ITEM))
        .px(px(space::S))
        .flex()
        .items_center()
        .gap(px(space::S))
        .rounded(px(radius::SM))
        .cursor_pointer()
        .text_body()
        .relative()
        .when(selected, |item| item.font_weight(FontWeight::MEDIUM))
        .hover_fill(p, radius::SM, false)
        .when_some(icon, |item, icon| item.child(Icon::new(icon).size(px(size::ICON_MD))))
        .child(div().flex_1().min_w(px(0.)).truncate().child(label))
}

/// 组件库按钮的尺寸，取自控件高度阶梯（见规范 §2.5）。都是最小高度：文字放大时按钮跟着变高。
/// 不设尺寸就是 `CONTROL_MD`（32，组件库的默认尺寸），用于表单与弹窗里的主要操作。
///
/// 变体：工具栏、行内操作用 [`ControlSize::subtle`]，其余用组件库的默认（带描边）/ `.primary()` / `.danger()`。
/// 组件库自己不换悬停、按下色（见 [`control_theme`]），渐变由外面的 [`FocusFrame`] 画。
pub trait ControlSize: gpui_kit::component::Sizable + Styled + Sized {
    /// 24：列表行、卡片标题里的行内操作。
    fn control_xs(self) -> Self {
        self.small().min_h(px(size::CONTROL_XS)).min_w(px(size::CONTROL_XS))
    }
    /// 28：工具栏、输入卡片底栏、卡片内的操作、弹窗顶栏。
    fn control_sm(self) -> Self {
        self.small().min_h(px(size::CONTROL_SM)).min_w(px(size::CONTROL_SM))
    }
    /// 安静的按钮（工具栏、列表行内的操作）：常态透明，悬停、按下由外面的 [`FocusFrame`] 叠层渐变。
    /// 代替组件库的 `.ghost()`：它的悬停读组件库共用的 `accent`（Markdown 行内代码、组件库菜单也用），没法单独设成不变色。
    fn subtle(self, cx: &App) -> Self;
    /// 空间不够时允许收缩，最窄到 `min_width`（组件库按钮默认不收缩）。标签要放进 [`truncated`]，
    /// 才会显示省略号而不是被截断。
    fn shrinkable(self, min_width: f32) -> Self {
        self.flex_shrink(1.).min_w(px(min_width))
    }
}

/// 可以截断的按钮标签：宽度不够时末尾显示省略号。配合 [`ControlSize::shrinkable`]。
pub fn truncated(label: impl Into<SharedString>) -> Div {
    div().flex_shrink(1.).min_w(px(0.)).truncate().child(label.into())
}

impl ControlSize for gpui_kit::component::button::Button {
    fn subtle(self, cx: &App) -> Self {
        use gpui_kit::component::button::{ButtonCustomVariant, ButtonVariants as _};
        let text = palette(cx).text;
        let clear = text.opacity(0.);
        self.custom(ButtonCustomVariant::new(cx).color(clear).foreground(text).hover(clear).active(clear))
    }
}

/// 把调色板套到组件库控件（按钮、输入框、工具提示、Markdown 里的代码块）上：圆角取本规范的阶梯，
/// 悬停、按下、选中的颜色由调色板派生。浅色、深色、配色方案、高对比度都调用它（见 `sync_theme`）。
///
/// 组件库在 `Theme::change` 时从原始主题派生 `button`、`button_hover`、`button_active`、`button_foreground`，
/// 之后再改 `input` 不会联动，所以这里直接写它们。
pub fn control_theme(p: &Palette, theme: &mut gpui_kit::component::Theme) {
    let dark = theme.is_dark();
    theme.radius = px(radius::MD);
    theme.radius_lg = px(radius::LG);
    let shade = |color: Hsla, delta: f32| Hsla { l: (color.l + delta).clamp(0., 1.), ..color };
    let c = &mut theme.colors;
    c.input = p.control_border;
    // ghost 按钮：悬停读 `accent`（组件库在深色下再减半，这里先加倍），按下读 `button_active`，展开读 `secondary_active`。
    c.accent = if dark { p.text.opacity(0.10) } else { p.hover };
    c.accent_foreground = p.text;
    c.secondary_active = p.selected;
    // 按钮的悬停、按下不由组件库换色（它是即时的），而由外面的 `FocusFrame` 叠半透明层渐变：
    // 这里把各变体的悬停、按下色设成与常态相同。
    c.button = p.surface;
    c.button_hover = p.surface;
    c.button_active = p.surface;
    c.button_foreground = p.text;
    // 主要按钮：浅色、深色都用强调色。
    c.primary = p.accent;
    c.primary_hover = shade(p.accent, 0.06);
    c.primary_active = shade(p.accent, -0.06);
    c.primary_foreground = p.on_accent;
    c.button_primary = p.accent;
    c.button_primary_hover = p.accent;
    c.button_primary_active = p.accent;
    c.button_primary_foreground = p.on_accent;
    let danger = c.button_danger;
    c.button_danger_hover = danger;
    c.button_danger_active = danger;
}

/// [`control_theme`] 写的值现在是不是都已经在主题上（`sync_theme` 据此决定要不要更新主题）。
pub fn control_theme_applied(p: &Palette, theme: &gpui_kit::component::Theme) -> bool {
    let mut wanted = theme.clone();
    control_theme(p, &mut wanted);
    let (a, b) = (&wanted.colors, &theme.colors);
    wanted.radius == theme.radius
        && wanted.radius_lg == theme.radius_lg
        && a.input == b.input
        && a.accent == b.accent
        && a.button == b.button
        && a.button_hover == b.button_hover
        && a.button_active == b.button_active
        && a.button_foreground == b.button_foreground
        && a.button_primary == b.button_primary
        && a.button_primary_hover == b.button_primary_hover
        && a.button_danger_hover == b.button_danger_hover
        && a.primary == b.primary
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
    // 每一次按键（包括绑定成动作的 Tab、方向键）在派发之前都经过这里。只有真正挪了焦点的按键才算
    // “键盘焦点”（和浏览器的 `:focus-visible` 一样）：打字、在展开的列表里按方向键这些不挪焦点的按键，
    // 不会让鼠标点过去的控件突然冒出焦点框。Tab 总算（只有一个可聚焦控件时焦点不动，也要看得见它在哪）。
    cx.intercept_keystrokes(|event, window, cx| {
        if moves_focus_always(&event.keystroke) {
            set_focus_from_keyboard(true, window);
            return;
        }
        // 派发完这次按键再比较：焦点变了就是键盘挪过去的。有的焦点要等下一帧画出来才移过去
        // （打开菜单、弹窗时的 `focus_after_paint`），根视图渲染时再看一次（见 [`settle_focus_source`]）。
        PENDING_KEY_FOCUS.with(|pending| *pending.borrow_mut() = Some((window.focused(cx), std::time::Instant::now())));
        window.defer(cx, settle_focus_source);
    })
    .detach();
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

/// 不管焦点动没动都算键盘导航的键：Tab、Shift+Tab（F6 在区域之间跳，也算）。
fn moves_focus_always(keystroke: &Keystroke) -> bool {
    let modifiers = &keystroke.modifiers;
    matches!(keystroke.key.as_str(), "tab" | "f6") && !modifiers.control && !modifiers.alt && !modifiers.platform
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
/// 打开、关闭、焦点、Esc 和点外面关闭都交给组件库的 `Popover`；外观和进场在这里画：
/// 菜单从触发按钮那一侧淡入并移过来一点（`BASE`），阴影随之渐显；选项和菜单一起出现，不错峰。
/// 关上时立即移除（`Popover` 本来就不再画内容，焦点也已经回到触发按钮）。
///
/// 键盘：打开时焦点落在第 `initial` 项（当前选中的那一项），↑ / ↓ / Home / End 在各项之间移动，
/// Enter / 空格选中。`body` 用第一个参数（[`MenuFocus`]）给每一项取焦点句柄，见那里的说明。
#[allow(clippy::too_many_arguments)] // 和 `Popover` 的构造一一对应，拆成结构体反而不好读。
pub fn menu_popover<T: gpui_kit::component::Selectable + IntoElement + 'static>(
    id: impl Into<ElementId>,
    open: bool,
    anchor: Anchor,
    trigger: T,
    on_open_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
    initial: usize,
    body: impl Fn(&MenuFocus, &mut Window, &mut App) -> AnyElement + 'static,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    menu_popover_in(id, open, anchor, trigger, on_open_change, initial, body, None, window, cx)
}

/// 同 [`menu_popover`]，但触发按钮在空间不够时可以收缩，最窄到 `min_width`（如输入卡片下方的选项行）。
/// 触发按钮自己也要允许收缩（见 [`ControlSize::shrinkable`]）。
#[allow(clippy::too_many_arguments)]
pub fn shrinking_menu_popover<T: gpui_kit::component::Selectable + IntoElement + 'static>(
    id: impl Into<ElementId>,
    open: bool,
    anchor: Anchor,
    trigger: T,
    on_open_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
    initial: usize,
    body: impl Fn(&MenuFocus, &mut Window, &mut App) -> AnyElement + 'static,
    min_width: f32,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    menu_popover_in(id, open, anchor, trigger, on_open_change, initial, body, Some(min_width), window, cx)
}

#[allow(clippy::too_many_arguments)]
fn menu_popover_in<T: gpui_kit::component::Selectable + IntoElement + 'static>(
    id: impl Into<ElementId>,
    open: bool,
    anchor: Anchor,
    trigger: T,
    on_open_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
    initial: usize,
    body: impl Fn(&MenuFocus, &mut Window, &mut App) -> AnyElement + 'static,
    shrink_to: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    use gpui_kit::component::popover::Popover;

    let id = id.into();
    let menu = MenuFocus::new(&id, window, cx);
    // 打开的那一刻不把焦点交给菜单项：组件库的 `Popover` 在按下鼠标（或 Enter）的同一个事件里就移焦点，
    // 那时菜单还没画出来，无障碍树里没有这一项，讲述人的焦点落到窗口上，把窗口标题再读一遍。
    // 所以让它“移”到触发按钮自己（焦点不变），菜单画出来之后再移到当前项（见下面 `content`）。
    let hold_focus = menu_trigger_focus(id.clone(), window, cx);
    if !open {
        menu.0.update(cx, |state, _| state.initial_given = false);
    }
    let phase = motion::presence((id.clone(), "presence"), open, motion::BASE, window, cx).unwrap_or(motion::Phase::SETTLED);
    // 向上展开的菜单从下面浮上来，向下展开的从上面落下来。
    let from = match anchor {
        Anchor::BottomLeft | Anchor::BottomRight | Anchor::BottomCenter => motion::Edge::Bottom,
        _ => motion::Edge::Top,
    };
    let popover = Popover::new(id)
        .anchor(anchor)
        .appearance(false)
        .open(open)
        // 打开时焦点落在当前选中的项上（而不是菜单本身），方向键马上就能用，读屏软件读出当前选项。
        .track_focus(&hold_focus)
        .on_open_change(on_open_change)
        .when(shrink_to.is_some(), |popover| popover.trigger_style(StyleRefinement::default().flex().min_w(px(0.))))
        .trigger(trigger)
        .content(move |_, window, cx| {
            let p = palette(cx);
            menu.begin(cx);
            let content = body(&menu, window, cx);
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
            motion::pop(
                div().rounded(px(radius::LG)).border_1().border_color(p.border).bg(p.raised).text_color(p.text).child(content),
                from,
                motion::NUDGE,
                phase,
                shadow::menu(&p),
            )
        });
    match shrink_to {
        Some(min_width) => div().relative().flex().flex_shrink(1.).min_w(px(min_width)).child(popover).into_any_element(),
        None => div().relative().flex_none().child(popover).into_any_element(),
    }
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

/// 单选菜单里的一项（权限、预设、模型）：读屏软件读作“单选菜单项，已勾选 / 未勾选”。
/// 只设语义，样式由调用方决定。
pub fn menu_choice(item: Pressable, checked: bool) -> Pressable {
    item.role(Role::MenuItemRadio).aria_toggled(if checked { Toggled::True } else { Toggled::False })
}

/// 动效：时长、缓动、浮层进场、悬停与按下、加载指示。见 `design/motion.rs`。
pub mod motion;

/// 浮层的阴影。按层级取（见规范 2.5）；动画里由 [`motion::pop`] 按不透明度渐显。
///
/// GPUI 的 `blur_radius` 按 sigma 计，同样的数值比 CSS 模糊一倍，所以这里的数值约为 CSS 写法的一半。
/// 深色表面上 10% 左右的黑色几乎看不见，深色模式用更深的阴影。
pub mod shadow {
    use gpui_kit::{BoxShadow, hsla, px};

    use super::Palette;

    /// 当前是不是深色（页面底色偏暗）。
    fn dark(p: &Palette) -> bool {
        p.surface.l < 0.5
    }

    fn ink(alpha: f32) -> gpui_kit::Hsla {
        hsla(0., 0., 0., alpha)
    }

    /// 层级 1：菜单、下拉列表、浮在内容上的小按钮。
    pub fn menu(p: &Palette) -> Vec<BoxShadow> {
        let alpha = if dark(p) { 0.4 } else { 0.12 };
        vec![BoxShadow::new(px(0.), px(4.), ink(alpha)).blur_radius(px(8.))]
    }

    /// 输入卡片、审批面板：贴着内容的一层很淡的阴影。
    pub fn composer(p: &Palette) -> Vec<BoxShadow> {
        let alpha = if dark(p) { 0.2 } else { 0.04 };
        vec![BoxShadow::new(px(0.), px(1.), ink(alpha)).blur_radius(px(1.)), BoxShadow::new(px(0.), px(2.), ink(alpha)).blur_radius(px(3.))]
    }

    /// 层级 2：弹窗。
    pub fn modal(p: &Palette) -> Vec<BoxShadow> {
        let alpha = if dark(p) { 0.5 } else { 0.2 };
        vec![BoxShadow::new(px(0.), px(12.), ink(alpha)).blur_radius(px(12.))]
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

        let _lock = super::HIGH_VISIBILITY_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

    /// 组件库的按钮按下鼠标时不拿焦点；高可见性轮廓开启时，套了 [`super::framed`] 的按钮点了也要拿到焦点（外圈才画得出来）。
    #[gpui::test]
    fn clicking_a_framed_button_focuses_it_with_high_visibility_outlines(cx: &mut gpui::TestAppContext) {
        use gpui_kit::component::button::Button;
        use gpui_kit::{
            AppContext as _, Context, IntoElement, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement as _, PlatformInput,
            Render, Styled as _, Window, div, point, px,
        };

        struct View;
        impl Render for View {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().p(px(40.)).child(super::framed("plain", Button::new("plain").outline().label("plain")))
            }
        }

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
        let click = |cx: &mut gpui::TestAppContext| {
            let position = point(px(48.), px(48.));
            cx.update_window(window, |_, window, cx| {
                let down = MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                };
                window.dispatch_event(PlatformInput::MouseDown(down), cx);
                let up = MouseUpEvent { button: MouseButton::Left, position, modifiers: Modifiers::default(), click_count: 1 };
                window.dispatch_event(PlatformInput::MouseUp(up), cx);
            })
            .unwrap();
            cx.run_until_parked();
        };
        let focused = |cx: &mut gpui::TestAppContext| cx.update_window(window, |_, window, cx| window.focused(cx).is_some()).unwrap();

        // 默认样式：和组件库一样，点了不拿焦点。
        let _lock = super::HIGH_VISIBILITY_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        super::set_high_visibility_outlines(false);
        draw(cx);
        click(cx);
        assert!(!focused(cx), "没开高可见性轮廓时点按钮不拿焦点");

        super::set_high_visibility_outlines(true);
        draw(cx);
        click(cx);
        assert!(focused(cx), "开了高可见性轮廓，点按钮应该拿到焦点");
        let before = super::OUTLINES_DRAWN.with(|count| count.get());
        draw(cx);
        assert_eq!(super::OUTLINES_DRAWN.with(|count| count.get()), before + 1, "拿到焦点后画外圈");
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
                    "trigger",
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
                    move |menu, _, cx| {
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

    /// 按钮的常态、悬停、按下颜色在浅色、深色、配色方案、高对比度下都分得开（以前配色方案下主要按钮三种状态同色，
    /// 深色下 ghost 按钮按下和悬停几乎一样）。
    #[gpui::test]
    fn button_states_differ_in_every_theme(cx: &mut gpui::TestAppContext) {
        use gpui_kit::component::{Theme, ThemeMode};

        let custom = {
            let set = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            let light = set(&[("accent", "#0F766E"), ("text", "#1F2937"), ("surface", "#FFFFFF")]);
            let dark = set(&[("accent", "#2DD4BF"), ("text", "#F3F4F6"), ("surface", "#111827")]);
            super::PaletteOverride::parse(&light, &dark).unwrap()
        };
        let high_contrast = {
            let theme = crate::backend::builtin_high_contrast_theme();
            let mut over = super::PaletteOverride::parse(&theme.light, &theme.dark).unwrap();
            over.high_contrast = true;
            over
        };
        cx.update(|cx| {
            gpui_kit::init(cx);
            for (name, over) in [("default", super::PaletteOverride::default()), ("scheme", custom), ("high contrast", high_contrast)] {
                cx.set_global(over);
                for mode in [ThemeMode::Light, ThemeMode::Dark] {
                    Theme::change(mode, None, cx);
                    if mode.is_dark() {
                        Theme::update(cx, super::soften_dark);
                    }
                    let p = super::palette(cx);
                    Theme::update(cx, |theme| super::control_theme(&p, theme));
                    let colors = &gpui_kit::component::ActiveTheme::theme(cx).colors;
                    let label = format!("{name} / {mode:?}");
                    // 组件库自己不换色（即时的），悬停、按下由 `FocusFrame` 叠的半透明层渐变。
                    assert_eq!(colors.button_primary, colors.button_primary_hover, "{label}: primary hover");
                    assert_eq!(colors.button_primary, colors.button_primary_active, "{label}: primary pressed");
                    assert_eq!(colors.button, colors.button_hover, "{label}: default hover");
                    assert_eq!(colors.button, colors.button_active, "{label}: default pressed");
                    assert_eq!(colors.button_danger, colors.button_danger_hover, "{label}: danger hover");
                    // 叠层：悬停、按下两两不同，且都看得见，按下比悬停深。
                    assert!(p.hover.a > 0. && p.pressed.a > p.hover.a, "{label}: tint layers visible");
                    assert_ne!(p.hover, p.pressed, "{label}: hover vs pressed");
                    assert_ne!(p.selected, p.hover, "{label}: selected vs hover");
                }
            }
        });
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

    /// 高对比度配色下调色板带着标志（列表项悬停据此改用深一档的底色、按钮改为描边），默认配色不带。
    #[gpui::test]
    fn palette_knows_high_contrast(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(super::PaletteOverride::default());
            assert!(!super::palette(cx).high_contrast);
            let theme = crate::backend::builtin_high_contrast_theme();
            let mut over = super::PaletteOverride::parse(&theme.light, &theme.dark).unwrap();
            over.high_contrast = true;
            cx.set_global(over);
            let p = super::palette(cx);
            assert!(p.high_contrast);
            // 内置配色没设 `on_selected`：等于正文色。
            assert_eq!(p.on_selected, p.text);
        });
    }

    /// 跟随 Windows 的“高对比白”：选中、悬停是紫底白字，链接深蓝，次要文字和状态色都是文本色，底色都是背景色。
    #[gpui::test]
    fn system_high_contrast_colors_become_the_palette(cx: &mut gpui::TestAppContext) {
        let white = crate::system_a11y::SystemColors {
            window: 0xFFFFFF,
            window_text: 0x000000,
            highlight: 0x37006E,
            highlight_text: 0xFFFFFF,
            hot_light: 0x00009F,
            gray_text: 0x600000,
            button_face: 0xFFFFFF,
            button_text: 0x000000,
        };
        let theme = crate::backend::system_high_contrast_theme(white);
        let mut over = super::PaletteOverride::parse(&theme.light, &theme.dark).expect("系统颜色是合法的配色");
        over.high_contrast = true;
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(over);
            let p = super::palette(cx);
            let hex = |color: u32| gpui_kit::Hsla::from(gpui_kit::rgb(color));
            assert_eq!(p.selected, hex(0x37006E));
            assert_eq!(p.on_selected, hex(0xFFFFFF));
            assert_eq!(p.accent, hex(0x00009F));
            for color in [p.text_secondary, p.danger, p.warning, p.success, p.border] {
                assert_eq!(color, p.text);
            }
            for color in [p.raised, p.sidebar, p.sunken] {
                assert_eq!(color, p.surface);
            }
            // 选定文本的文字色在选定底色上看得清。
            assert!(super::contrast(p.on_selected, p.selected) >= 4.5);
            // 按钮悬停的描边用选定色（与 Windows 一致）。
            assert_eq!(super::hc_line(&p), p.selected);
        });
    }
}
