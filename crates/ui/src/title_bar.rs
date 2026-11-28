//! Windows 上的自绘标题栏。
//!
//! 创建窗口时设 `TitlebarOptions { appears_transparent: true }`，GPUI 在 Windows 上把客户区延伸到原来的标题栏
//! （处理 `WM_NCCALCSIZE`），标题栏和最小化 / 最大化 / 关闭按钮由这里画，颜色取自当前配色，跟着浅色、深色和
//! 配色方案走。哪里能拖动、哪里是哪个按钮用 `window_control_area` 标出，GPUI 在 `WM_NCHITTEST` 里据此回答
//! `HTCAPTION` / `HTMINBUTTON` / `HTMAXBUTTON` / `HTCLOSE`：拖动、双击最大化、右键系统菜单、Win11 悬停最大化
//! 按钮出现的贴靠布局、按钮的点击都由系统处理，这里不写点击逻辑。关闭按钮发出的是普通的 `WM_CLOSE`，
//! 照样经过 `on_window_should_close`（关闭到托盘）。
//!
//! macOS 上同样隐藏系统标题栏、自己画底色和品牌，但保留系统的红黄绿按钮（位置由 `traffic_light_position`
//! 定到标题栏中间）：全屏、按住 Option 缩放、悬停出现的窗口排列菜单都是它们带的。macOS 没有命中测试，
//! 拖动由这里处理：按下后一移动就交给 `start_window_move`，双击按系统设置缩放或最小化
//! （`titlebar_double_click`）；窗口选项里的 `app_owns_titlebar_drag` 让 AppKit 不再抢着处理标题栏。
//!
//! Linux 仍用系统标题栏：[`custom`] 为假时什么都不画。
//!
//! 外观（见 `docs/design.md` 第 4 节“窗口框架”）：标题栏没有自己的底色，和侧边栏一起铺在窗口的“框架”色
//! （`surface.sidebar`）上，下面的对话区是一块圆角面板，两者靠面板的描边和圆角分开。标题栏左边是当前位置
//! （工作区 › 会话标题，运行中时带状态），右边是窗口按钮；窗口不在前台时整条变淡。

use gpui_kit::component::{Icon, IconName, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::design::{motion, palette, size, space};

/// 标题栏高度：和侧边栏顶上的品牌行一样高，两边对齐。
pub const HEIGHT: f32 = size::TITLE_BAR;
/// 自绘标题栏时，对话面板左上角的圆角（外沿，含 1px 描边）。
pub const PANEL_RADIUS: f32 = crate::design::radius::LG;
/// 面板里面贴着左上角画的东西用这个圆角（描边内侧），否则方角会盖住面板的圆角。没有面板时为 0。
pub fn panel_inner_radius() -> f32 {
    if custom() { PANEL_RADIUS - 1. } else { 0. }
}

/// 每个窗口按钮的宽度，和 Windows 自带的一样。
const BUTTON_WIDTH: f32 = 46.;
/// 关闭按钮悬停时的红色，和系统的一样。
const CLOSE_HOVER: u32 = 0xC42B1C;

/// macOS 的红黄绿按钮占的宽度（从窗口左边算起，含右侧留白），侧边栏的品牌放在它右边。
const TRAFFIC_LIGHTS_WIDTH: f32 = 80.;

/// 这个平台上标题栏是不是自己画的。用 `cfg!` 而不是 `#[cfg]`：别的平台编译时这些代码也做类型检查。
pub fn custom() -> bool {
    cfg!(windows) || cfg!(target_os = "macos")
}

fn macos() -> bool {
    cfg!(target_os = "macos")
}

/// 创建窗口用的标题栏选项。
pub fn options() -> TitlebarOptions {
    TitlebarOptions {
        title: Some(crate::APP_NAME.into()),
        appears_transparent: custom(),
        // 红黄绿按钮高 14 px 左右：放在标题栏（和侧边栏的品牌行）正中。
        traffic_light_position: macos().then(|| point(px(14.), px((HEIGHT - 14.) / 2.))),
    }
}

/// macOS：标题栏由程序自己拖动（见模块说明）。其他平台不起作用。
pub fn app_owns_drag() -> bool {
    macos()
}

/// macOS 上拖动用的状态：按下了鼠标、还没开始拖。
struct Pressed(bool);

/// 弹窗遮罩铺的范围：自绘标题栏时从标题栏下面开始，遮罩打开时窗口按钮照样能用、窗口照样能拖。
pub fn below<E: Styled>(element: E) -> E {
    if custom() { element.top(px(HEIGHT)).left_0().right_0().bottom_0() } else { element.inset_0() }
}

/// 侧边栏顶上品牌行（标志和应用名称，大小和原来一样）额外的左边距：macOS 上给红黄绿按钮让位，
/// 全屏时按钮隐藏，不用让。`base` 是品牌行本来离窗口左边的距离。
pub fn brand_offset(base: f32, window: &Window) -> f32 {
    if custom() && macos() && !window.is_fullscreen() { (TRAFFIC_LIGHTS_WIDTH - base).max(0.) } else { 0. }
}

/// 能拖动窗口的区域：Windows 上标给系统的命中测试；macOS 上按下后一移动就交给 `start_window_move`，
/// 双击按系统设置缩放或最小化。不自绘标题栏的平台上就是普通的 div。
pub fn drag_region(id: &'static str, window: &mut Window, cx: &mut App) -> Stateful<Div> {
    let area = div().id(id);
    if !custom() {
        return area;
    }
    if !macos() {
        return area.window_control_area(WindowControlArea::Drag);
    }
    let pressed = window.use_keyed_state(id, cx, |_, _| Pressed(false));
    area.on_mouse_down(
        MouseButton::Left,
        window.listener_for(&pressed, |pressed, event: &MouseDownEvent, window, _| {
            if event.click_count == 2 {
                pressed.0 = false;
                window.titlebar_double_click();
            } else {
                pressed.0 = true;
            }
        }),
    )
    .on_mouse_up(MouseButton::Left, window.listener_for(&pressed, |pressed, _: &MouseUpEvent, _, _| pressed.0 = false))
    .on_mouse_move(window.listener_for(&pressed, |pressed, _: &MouseMoveEvent, window, _| {
        if pressed.0 {
            pressed.0 = false;
            window.start_window_move();
        }
    }))
}

/// 标题栏：一条可以拖动的横条，左边是 `title`（当前位置），Windows 上右侧是最小化 / 最大化 / 关闭按钮
/// （macOS 用系统的红黄绿按钮）。没有自己的底色，铺在父元素的底色上。工作区窗口里它只盖在对话区上方，
/// 侧边栏一直通到窗口顶上；不自绘标题栏的平台返回 `None`。
/// `resizable`：窗口能不能调整大小，不能时不显示最大化按钮。
pub fn title_bar(resizable: bool, title: Option<AnyElement>, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
    if !custom() {
        return None;
    }
    let p = palette(cx);
    // 窗口不在前台时整条变淡（有过渡），和系统标题栏一样。
    let active = motion::tween("title-bar-active", if window.is_window_active() { 1. } else { 0. }, window, cx);
    let dim = 0.55 + 0.45 * active;
    let maximized = window.is_maximized();
    let bar = h_flex()
        .id("title-bar")
        .flex_none()
        .w_full()
        .h(px(HEIGHT))
        .text_color(p.text)
        // 标题也在拖动区里：拖它、双击它和拖空白处一样。
        .child(
            drag_region("title-bar-drag", window, cx)
                .h_full()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .pl(px(space::L))
                .pr(px(space::S))
                .opacity(dim)
                .children(title),
        )
        .when(!macos(), |bar| {
            bar.child(control(
                "title-bar-minimize",
                IconName::WindowMinimize,
                WindowControlArea::Min,
                t!("title_bar.minimize"),
                false,
                dim,
                &p,
            ))
            .when(resizable, |bar| {
                let (icon, label) = if maximized {
                    (IconName::WindowRestore, t!("title_bar.restore"))
                } else {
                    (IconName::WindowMaximize, t!("title_bar.maximize"))
                };
                bar.child(control("title-bar-maximize", icon, WindowControlArea::Max, label, false, dim, &p))
            })
            .child(control(
                "title-bar-close",
                IconName::WindowClose,
                WindowControlArea::Close,
                t!("title_bar.close"),
                true,
                dim,
                &p,
            ))
        });
    Some(bar.into_any_element())
}

/// 一个窗口按钮。点击由系统处理（见模块说明），所以不是 Tab 停靠点；键盘用 Alt+Space、Alt+F4。
/// 悬停的底色有过渡（关闭按钮是系统的红色）；`dim` 是窗口不在前台时图标的淡化程度。
fn control(
    id: &'static str,
    icon: IconName,
    area: WindowControlArea,
    label: impl Into<SharedString>,
    close: bool,
    dim: f32,
    p: &crate::design::Palette,
) -> impl IntoElement {
    let (hover_bg, hover_fg): (Hsla, Hsla) = if close { (rgb(CLOSE_HOVER).into(), white()) } else { (p.selected, p.on_selected) };
    div()
        .id(id)
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(BUTTON_WIDTH))
        .h_full()
        .window_control_area(area)
        .role(Role::Button)
        .aria_label(label)
        .hover(move |style| style.text_color(hover_fg))
        // 按下时略暗一点（底色由下面的悬停层画，按下的颜色叠不上去）。
        .active(move |style| style.text_color(hover_fg).opacity(0.85))
        .child(motion::highlight("control-fill", hover_bg, 0., false))
        .child(Icon::new(icon).size(px(size::ICON_SM)).opacity(dim))
}
