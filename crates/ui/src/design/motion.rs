//! 动效。规则见 `docs/design.md` §5.5；这里是唯一的实现位置，视图不直接写时长、缓动和位移。
//!
//! 基调：**响应即时，动画只说明连续性。** 用户操作的结果第一帧就以最终样子出现、可以操作；
//! 动画不能挡在结果前面，也不能让人等。所以：
//! - 高频操作（切换会话、切页、新消息、流式内容、悬停进入、键盘导航）不做动画。
//! - 不动画布局（宽度、高度、行高）：展开、折叠直接到位，只转动箭头。
//! - 必须有的进场不从透明开始（[`FLOOR`]），位移只有几像素；没有过冲，没有错峰。
//! - 退场只淡出、不位移，而且不挡操作。
//! 开启“减少动态效果”时没有动画：元素直接以最终状态出现、消失；循环动画停在起始帧。
//!
//! 几种做法，按场合选：
//! - [`appear`] / [`error_in`] / [`message_in`]：异步出现、需要引起注意的小块内容（审批、提示、错误）。
//!   动画状态记在元素上，元素一挂上就播放。包裹层的 `id` 要稳定，需要重播时换一个 `id`。
//! - [`presence`] + [`pop`]：浮层（弹窗、菜单、下拉框）的进场和退场。关闭后元素还要再画一小段时间，
//!   调用方在 [`presence`] 返回 `None` 之前继续渲染它。
//! - [`Highlight`]：悬停、选中的底色：亮起立即到位，熄灭时淡出。
//! - [`tween`]：一个数值（不透明度、角度）的过渡。[`turn`] 用它转动展开箭头。
//! - [`spinner`] / [`typing_dots`]：只用在图标和小圆点上。
//!
//! GPUI 没有图层合成：`opacity` 按每个图元分别相乘。带阴影的浮层淡入时，阴影会先于卡片显出来、
//! 像一块暗板透过卡片，所以 [`pop`] 把阴影强度按不透明度的三次方渐显。

use std::cell::Cell;
use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::base::motion::{Easing, Presence, PresencePhase, Transition, TransitionId};
use gpui_kit::component::{Icon, IconName, h_flex};
use gpui_kit::*;

// ---- 时长、位移 ----

/// 悬停熄灭、箭头、所有退场。
pub const FAST: Duration = Duration::from_millis(90);
/// 菜单、下拉框、开关、提示的进场。
pub const BASE: Duration = Duration::from_millis(120);
/// 弹窗、首页输入框滑到底部、首次启动的欢迎页。
pub const SLOW: Duration = Duration::from_millis(180);

/// 提示、错误这类小块内容的位移。
pub const NUDGE: f32 = 2.;
/// 菜单、下拉框、欢迎页的位移。
pub const SHORT: f32 = 4.;
/// 弹窗的位移。
pub const MEDIUM: f32 = 8.;

/// 进场起始的不透明度：第一帧就读得清，动画只是收个尾，不是“从无到有”。
pub const FLOOR: f32 = 0.5;

/// 循环动画的帧率上限：只是状态提示，不值得每帧重绘。
const LOOP_FPS: f32 = 30.;

// ---- 缓动 ----

/// 进场：很快到位，最后一段放缓。
const ENTER: Easing = Easing::CubicBezier { x1: 0.16, y1: 1., x2: 0.3, y2: 1. };
/// 退场：干脆地离开。
const EXIT: Easing = Easing::CubicBezier { x1: 0.4, y1: 0., x2: 1., y2: 1. };
/// 状态变化（底色、旋转、开关）。
const STANDARD: Easing = Easing::CubicBezier { x1: 0.2, y1: 0., x2: 0., y2: 1. };

pub fn enter(t: f32) -> f32 {
    ENTER.sample(t.clamp(0., 1.))
}

pub fn exit(t: f32) -> f32 {
    EXIT.sample(t.clamp(0., 1.))
}

pub fn standard(t: f32) -> f32 {
    STANDARD.sample(t.clamp(0., 1.))
}

/// 进场时的不透明度：从 [`FLOOR`] 开始。
fn enter_opacity(t: f32) -> f32 {
    FLOOR + (1. - FLOOR) * enter(t)
}

// ---- 方向与位移 ----

/// 元素从哪一边来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
}

/// 把元素画在离终点还差 `remaining`（1 是整段距离，0 是到位）的地方。用相对定位，不影响布局。
/// 不要用在已经 `absolute()` 定位的元素上：那会改掉它真正的位置。
pub fn shift<E: Styled>(element: E, from: Edge, distance: f32, remaining: f32) -> E {
    let offset = px(distance * remaining);
    match from {
        Edge::Top => element.top(-offset),
        Edge::Bottom => element.top(offset),
    }
}

// ---- 进场 ----

/// 一次性的进场：元素挂上时从半透明（可选：偏几像素）收到原位。只给异步出现、需要注意的内容用，
/// 以及首次启动的欢迎页；用户自己操作的结果直接显示。
#[derive(Debug, Clone, Copy)]
pub struct Entrance {
    duration: Duration,
    from: Option<(Edge, f32)>,
}

impl Entrance {
    pub fn new(duration: Duration) -> Self {
        Self { duration, from: None }
    }

    /// 从 `edge` 那边偏 `distance` 像素的地方进来。
    pub fn from(mut self, edge: Edge, distance: f32) -> Self {
        self.from = Some((edge, distance));
        self
    }

    /// 按进度（0 到 1，线性）画出中间状态。
    fn apply<E: Styled>(&self, element: E, t: f32) -> E {
        let element = element.opacity(enter_opacity(t));
        match self.from {
            Some((edge, distance)) => shift(element, edge, distance, 1. - enter(t)),
            None => element,
        }
    }

    pub fn play<E: IntoElement + Styled + 'static>(self, element: E, id: impl Into<ElementId>) -> AnimationElement<E> {
        one_shot(element, id, self.duration, move |element, t| self.apply(element, t))
    }
}

/// 一次性动画：先停在起点 1ms，再用 `duration` 从 0 走到 1（线性，缓动由 `apply` 自己套）。
///
/// 所有一次性动画都是“停一下 + 播放”这两段。GPUI 按元素 id 记着播到第几段，同一个 id 段数一变它会去取
/// 不存在的段而崩溃；段数固定就不会出这种事。GPUI 的动画元素本身会遵守“减少动态效果”（直接画最终状态）。
fn one_shot<E: IntoElement + Styled + 'static>(
    element: E,
    id: impl Into<ElementId>,
    duration: Duration,
    apply: impl Fn(E, f32) -> E + 'static,
) -> AnimationElement<E> {
    element.with_animations(id, vec![Animation::new(MIN_HOLD), Animation::new(duration)], move |element, index, t| {
        apply(element, if index == 0 { 0. } else { t })
    })
}

/// “停在起点”那一段的长度：不能是 0（GPUI 用它做除数），短到看不出来。
const MIN_HOLD: Duration = Duration::from_millis(1);

/// 异步出现、需要注意的小块内容（审批区、操作结果提示）：从半透明收到原位，上浮两像素。
pub fn appear<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>) -> AnimationElement<E> {
    Entrance::new(BASE).from(Edge::Bottom, NUDGE).play(element, id)
}

/// 出错：第一帧就完全显示，再左右轻晃一下，表示“这里需要处理”。原因必须同时写在文字里。
pub fn error_in<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>) -> AnimationElement<E> {
    one_shot(element, id, FAST * 2, |element, t| element.left(px(shake(t))))
}

/// 操作结果的一行提示：成功时 [`appear`]，失败时 [`error_in`]。
pub fn message_in<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>, error: bool) -> AnyElement {
    if error { error_in(element, id).into_any_element() } else { appear(element, id).into_any_element() }
}

/// 晃动的横向位移：一个来回，越晃越小，结束时回到原位。
fn shake(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    NUDGE * 1.5 * (TAU * t).sin() * (1. - t)
}

// ---- 浮层的进场与退场 ----

/// 正在进场或退场的进度。`progress` 是线性的：进场从 0 到 1，退场从 1 回到 0。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Phase {
    pub progress: f32,
    pub leaving: bool,
}

impl Phase {
    /// 已经完全显示、没有动画。
    pub const SETTLED: Phase = Phase { progress: 1., leaving: false };

    /// 内容的可见程度（不透明度，0 到 1）。
    pub fn visibility(self) -> f32 {
        if self.leaving { 1. - exit(1. - self.progress) } else { enter(self.progress) }
    }
}

/// 进场、退场的状态，按 `id` 记在当前视图里。`present` 变成 `false` 后还会返回一段时间的
/// `Some(Phase { leaving: true, .. })`，调用方这期间继续画它（不响应操作、不挡下面的操作）；
/// 返回 `None` 才真正移除。中途反悔（打开又立刻关上）时从当前位置往回走，不会跳。
///
/// 在视图的 `render` 里调用，并且每帧都要调用（关着的时候也是），状态才不会丢。
pub fn presence(
    id: impl Into<ElementId>,
    present: bool,
    enter: Duration,
    exit: Duration,
    window: &mut Window,
    cx: &mut App,
) -> Option<Phase> {
    // 线性推进，缓动在画的时候按属性分别套（不透明度、位移、阴影各用各的曲线）。
    let transition = Transition::new(if present { enter } else { exit }).easing(Easing::Linear);
    let sample = Presence::new(TransitionId::from(id.into()), present).transition(transition).sample(window, cx);
    match sample.phase {
        PresencePhase::Absent => None,
        PresencePhase::Present => Some(Phase::SETTLED),
        PresencePhase::Entering => Some(Phase { progress: sample.progress, leaving: false }),
        // 已经退到看不见了就不用再画。第一次渲染时就是关着的，组件库也会报一段“从 0 退到 0”的退场，
        // 不能因此画一个透明的遮罩挡住点击。
        PresencePhase::Exiting if sample.progress <= f32::EPSILON => None,
        PresencePhase::Exiting => Some(Phase { progress: sample.progress, leaving: true }),
    }
}

/// 浮层（弹窗卡片、菜单、下拉列表）在 `phase` 时的样子：进场时淡入并从 `from` 那边收几像素到位
/// （没有过冲）；退场时原地淡出，不位移。阴影按不透明度的三次方渐显。
/// `shadow` 是完全显示时的阴影（见 `design::shadow`）。
pub fn pop<E: Styled>(element: E, from: Edge, distance: f32, phase: Phase, shadow: Vec<BoxShadow>) -> E {
    let visibility = phase.visibility();
    let remaining = if phase.leaving { 0. } else { 1. - enter(phase.progress) };
    let ink = visibility * visibility * visibility;
    let shadow = shadow.into_iter().map(|mut shadow| {
        shadow.color = shadow.color.opacity(ink);
        shadow
    });
    shift(element.opacity(visibility), from, distance, remaining).shadow(shadow.collect())
}

/// 退场中的浮层不响应操作：盖一层透明的遮挡。它只在退场的一小段时间里存在。
pub fn inert(element: impl IntoElement) -> Div {
    div().relative().child(element).child(div().absolute().inset_0().occlude())
}

// ---- 悬停、选中的底色 ----

/// 悬停、选中时的底色：亮起立即到位，熄灭时淡出。放在控件的第一个子元素（铺满控件、画在最底层），控件本身不再设
/// `bg` / `hover`。悬停按这一层自己的范围判断，被别的浮层盖住时不算悬停。
pub struct Highlight {
    id: ElementId,
    color: Hsla,
    radius: Pixels,
    active: bool,
    hover: bool,
    outset: Pixels,
}

/// 见 [`Highlight`]。`active` 时（选中、菜单打开）一直显示。
pub fn highlight(id: impl Into<ElementId>, color: Hsla, radius: f32, active: bool) -> Highlight {
    Highlight { id: id.into(), color, radius: px(radius), active, hover: true, outset: px(0.) }
}

impl Highlight {
    /// 不跟随悬停（只表示选中）。
    pub fn without_hover(mut self) -> Self {
        self.hover = false;
        self
    }

    /// 向外多铺这么宽：绝对定位从边框内侧算起，控件有边框（如焦点框）时用它铺满整个控件。
    pub fn outset(mut self, outset: f32) -> Self {
        self.outset = px(outset);
        self
    }
}

/// [`Highlight`] 跨帧的状态。悬停与否要记下来：`prepaint` 时 GPUI 的命中测试还是上一帧的，
/// 只有 `paint` 和事件里问得准（组件库的 `hover` 样式也是这样记的）。
#[derive(Debug, Clone)]
struct HighlightState {
    from: f32,
    to: f32,
    started: Instant,
    hovered: Rc<Cell<bool>>,
}

impl HighlightState {
    /// 亮起立即到位（悬停的反馈要跟手）；熄灭时用 `FAST` 淡出。
    fn value(&self, now: Instant) -> f32 {
        if self.to >= self.from {
            return self.to;
        }
        let t = now.saturating_duration_since(self.started).as_secs_f32() / FAST.as_secs_f32();
        self.from + (self.to - self.from) * standard(t)
    }
}

impl IntoElement for Highlight {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Highlight {
    type RequestLayoutState = ();
    type PrepaintState = (Hitbox, f32, Rc<Cell<bool>>);

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
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
        let style = Style { position: Position::Absolute, inset: Edges::all((-self.outset).into()), ..Default::default() };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> (Hitbox, f32, Rc<Cell<bool>>) {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let (active, hover) = (self.active, self.hover);
        let reduce = cx.reduce_motion();
        let (value, hovered) =
            window.with_element_state(global_id.expect("Highlight 总有 id"), |state: Option<HighlightState>, window| {
                let now = Instant::now();
                let mut state = state.unwrap_or_else(|| {
                    let target = if active { 1. } else { 0. };
                    HighlightState { from: target, to: target, started: now, hovered: Rc::default() }
                });
                let target = if active || (hover && state.hovered.get()) { 1. } else { 0. };
                if state.to != target {
                    state.from = state.value(now);
                    state.to = target;
                    state.started = now;
                }
                let value = if reduce { target } else { state.value(now) };
                if value != target {
                    window.request_animation_frame();
                }
                ((value, state.hovered.clone()), state)
            });
        (hitbox, value, hovered)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        (hitbox, value, hovered): &mut (Hitbox, f32, Rc<Cell<bool>>),
        window: &mut Window,
        _: &mut App,
    ) {
        if *value > 0. {
            window.paint_quad(fill(bounds, self.color.opacity(*value)).corner_radii(self.radius));
        }
        if self.hover {
            // 这一帧刚出现在指针下面（或指针离开时它刚好不在了）：记下来，下一帧开始过渡。
            // 触屏上没有悬停：点过之后 GPUI 记的指针位置还留在手指离开的地方，不能让底色一直亮着。
            let now = hitbox.is_hovered(window) && !window.last_input_was_touch();
            if now != hovered.get() {
                hovered.set(now);
                window.request_animation_frame();
            }
            // 指针进出这一块时记下来并重画所在的视图（缓存的视图不会自己重画）。
            let hitbox = hitbox.clone();
            let hovered = hovered.clone();
            let view = window.current_view();
            window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture {
                    let now = hitbox.is_hovered(window) && !window.last_input_was_touch();
                    if now != hovered.get() {
                        hovered.set(now);
                        cx.notify(view);
                    }
                }
            });
        }
    }
}

// ---- 开关 ----

/// 开关的轨道和圆钮（[`crate::design::switch`] 里用）：切换时圆钮滑到另一头（`BASE`、标准曲线），
/// 轨道颜色随之过渡。和 [`Highlight`] 一样按元素状态记进度，减少动态效果时直接到位。
pub struct SwitchTrack {
    pub id: ElementId,
    pub checked: bool,
    /// 关、开时轨道的颜色。
    pub track: (Hsla, Hsla),
    /// 关、开时圆钮的颜色。
    pub thumb: (Hsla, Hsla),
    pub width: Pixels,
    pub height: Pixels,
    pub thumb_size: Pixels,
    pub inset: Pixels,
}

#[derive(Clone)]
struct SwitchState {
    from: f32,
    to: f32,
    started: Instant,
}

impl SwitchState {
    fn value(&self, now: Instant) -> f32 {
        let t = now.saturating_duration_since(self.started).as_secs_f32() / BASE.as_secs_f32();
        self.from + (self.to - self.from) * standard(t.min(1.))
    }
}

impl IntoElement for SwitchTrack {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SwitchTrack {
    type RequestLayoutState = ();
    type PrepaintState = f32;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
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
        let style = Style { size: size(self.width.into(), self.height.into()), flex_shrink: 0., ..Style::default() };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> f32 {
        let target = if self.checked { 1. } else { 0. };
        let reduce = cx.reduce_motion();
        window.with_element_state(global_id.expect("SwitchTrack 总有 id"), |state: Option<SwitchState>, window| {
            let now = Instant::now();
            let mut state = state.unwrap_or(SwitchState { from: target, to: target, started: now });
            if state.to != target {
                state.from = state.value(now);
                state.to = target;
                state.started = now;
            }
            let value = if reduce { target } else { state.value(now) };
            if value != target {
                window.request_animation_frame();
            }
            (value, state)
        })
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        value: &mut f32,
        window: &mut Window,
        _: &mut App,
    ) {
        let t = *value;
        let mix = |(off, on): (Hsla, Hsla)| -> Hsla {
            let (off, on) = (off.to_rgb(), on.to_rgb());
            Rgba {
                r: off.r + (on.r - off.r) * t,
                g: off.g + (on.g - off.g) * t,
                b: off.b + (on.b - off.b) * t,
                a: off.a + (on.a - off.a) * t,
            }
            .into()
        };
        window.paint_quad(fill(bounds, mix(self.track)).corner_radii(bounds.size.height / 2.));
        let travel = self.width - self.thumb_size - self.inset * 2.;
        let origin = point(bounds.left() + self.inset + travel * t, bounds.top() + self.inset);
        let thumb = Bounds { origin, size: size(self.thumb_size, self.thumb_size) };
        window.paint_quad(fill(thumb, mix(self.thumb)).corner_radii(self.thumb_size / 2.));
    }
}

// ---- 图标与循环 ----

/// 一个数值过渡到 `target`（FAST、标准曲线），按 `id` 记在当前视图里；第一次直接取目标值。
/// 用于不透明度、旋转这类状态变化。在视图的 `render` 里调用。
pub fn tween(id: impl Into<ElementId>, target: f32, window: &mut Window, cx: &mut App) -> f32 {
    gpui_kit::base::motion::transition(TransitionId::from(id.into()), target, Transition::new(FAST).easing(STANDARD), window, cx)
}

/// 四分之一圈（收起朝右的箭头展开后朝下）。
pub const QUARTER_TURN: f32 = FRAC_PI_2;
/// 半圈（收起朝上的箭头展开后朝下）。
pub const HALF_TURN: f32 = PI;

/// 展开箭头：`turned` 时转过 `angle`（弧度），中间有过渡。
pub fn turn(icon: Icon, id: impl Into<ElementId>, turned: bool, angle: f32, window: &mut Window, cx: &mut App) -> Icon {
    let t = tween(id, if turned { 1. } else { 0. }, window, cx);
    if t == 0. { icon } else { icon.rotate(radians(t * angle)) }
}

/// 运行中的加载圈。减少动态效果时停在起始角度，旁边的文字照常说明状态。
pub fn spinner(id: impl Into<ElementId>, icon_size: f32, color: Hsla) -> AnimationElement<Icon> {
    Icon::new(IconName::LoaderCircle).size(px(icon_size)).text_color(color).with_animation(
        id,
        Animation::new(Duration::from_millis(900)).repeat().with_max_fps(LOOP_FPS),
        |icon, t| icon.rotate(radians(t * TAU)),
    )
}

/// 正在回复、还没有文字时的三个点：依次亮起。减少动态效果时三个点一样淡，读作“…”。
pub fn typing_dots(color: Hsla) -> Div {
    h_flex().h(px(24.)).gap(px(NUDGE)).items_center().children((0..3).map(move |index| {
        div().size(px(6.)).rounded_full().bg(color).with_animation(
            ("typing-dot", index),
            Animation::new(Duration::from_millis(1200)).repeat_synced().with_max_fps(LOOP_FPS),
            move |dot, t| dot.opacity(pulse(t, index)),
        )
    }))
}

/// 第 `index` 个点在 `t` 时的不透明度。`t = 0`（也就是减少动态效果时）三个点一样。
fn pulse(t: f32, index: usize) -> f32 {
    let x = (t - index as f32 * 0.2).rem_euclid(1.);
    let bump = if x < 0.6 { (PI * x / 0.6).sin() } else { 0. };
    0.35 + 0.65 * bump
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    // 不用 `super::*`：那样会把 GPUI 的 `test` 宏一起引进来，盖过标准的 `#[test]`。
    use gpui::TestAppContext;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Window, div};

    use super::{
        BASE, Edge, Entrance, FAST, FLOOR, HighlightState, Phase, SLOW, appear, enter, enter_opacity, exit, message_in, presence, pulse,
        shake, standard,
    };

    /// 每次渲染记下 [`presence`] 的结果。
    struct Host {
        open: bool,
        phase: Option<Phase>,
    }

    impl Render for Host {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.phase = presence("host-presence", self.open, SLOW, FAST, window, cx);
            div()
        }
    }

    fn host(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Host>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            let (window, host) =
                gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| Host { open: false, phase: None })).unwrap();
            (window, host)
        })
    }

    /// 改开关、过一段时间、重画一次，返回这次的进度。
    fn step(cx: &mut TestAppContext, window: AnyWindowHandle, host: &Entity<Host>, open: bool, wait: Duration) -> Option<Phase> {
        cx.update(|cx| host.update(cx, |host, _| host.open = open));
        cx.executor().advance_clock(wait);
        cx.update_window(window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.update(|cx| host.read(cx).phase)
    }

    #[gpui::test]
    fn closing_keeps_drawing_until_the_exit_finishes(cx: &mut TestAppContext) {
        let (window, host) = host(cx);
        assert_eq!(step(cx, window, &host, false, Duration::ZERO), None);

        let opening = step(cx, window, &host, true, Duration::ZERO).expect("打开后要画");
        assert!(!opening.leaving && opening.progress < 0.1, "{opening:?}");
        assert_eq!(step(cx, window, &host, true, SLOW), Some(Phase::SETTLED));

        // 关上：先是退场（还要画），播完才移除。
        let leaving = step(cx, window, &host, false, Duration::ZERO).expect("退场期间还要画");
        assert!(leaving.leaving && leaving.progress > 0.9, "{leaving:?}");
        let halfway = step(cx, window, &host, false, FAST / 2).unwrap();
        assert!(halfway.leaving && (halfway.progress - 0.5).abs() < 0.15, "{halfway:?}");
        assert_eq!(step(cx, window, &host, false, FAST), None);
    }

    #[gpui::test]
    fn reopening_halfway_turns_back_from_where_it_is(cx: &mut TestAppContext) {
        let (window, host) = host(cx);
        step(cx, window, &host, false, Duration::ZERO);
        step(cx, window, &host, true, Duration::ZERO);
        let halfway = step(cx, window, &host, true, SLOW / 2).unwrap();
        // 进场到一半时关上：从当前位置往回退，不跳到完全显示再退。
        let turned = step(cx, window, &host, false, Duration::ZERO).unwrap();
        assert!(turned.leaving);
        assert!((turned.progress - halfway.progress).abs() < 0.1, "{halfway:?} → {turned:?}");
    }

    /// 同一个 id 前后用不同的进场（成功与出错、有无位移）。
    struct Shapes {
        error: bool,
    }

    impl Render for Shapes {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let entrance = if self.error { Entrance::new(BASE) } else { Entrance::new(BASE).from(Edge::Top, 4.) };
            div()
                .child(entrance.play(div(), "row-enter"))
                .child(message_in(div(), "message-enter", self.error))
                .child(appear(div(), "appear"))
        }
    }

    #[gpui::test]
    fn switching_the_entrance_under_the_same_id_does_not_crash(cx: &mut TestAppContext) {
        let (window, view) = cx.update(|cx| {
            gpui_kit::init(cx);
            let (window, view) = gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| Shapes { error: false })).unwrap();
            (window, view)
        });
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap();
        };
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(20));
        draw(cx);
        cx.update(|cx| view.update(cx, |view, _| view.error = true));
        draw(cx);
        cx.executor().advance_clock(SLOW);
        draw(cx);
        cx.update(|cx| view.update(cx, |view, _| view.error = false));
        draw(cx);
    }

    #[gpui::test]
    fn reduced_motion_shows_and_removes_at_once(cx: &mut TestAppContext) {
        let (window, host) = host(cx);
        cx.update(|cx| cx.set_reduce_motion(true));
        step(cx, window, &host, false, Duration::ZERO);
        assert_eq!(step(cx, window, &host, true, Duration::ZERO), Some(Phase::SETTLED));
        assert_eq!(step(cx, window, &host, false, Duration::ZERO), None);
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn curves_start_at_zero_and_end_at_one() {
        for curve in [enter, exit, standard] {
            assert!(close(curve(0.), 0.));
            assert!(close(curve(1.), 1.));
        }
        // 进场先快：一半时间已经走完大半；退场先慢。
        assert!(enter(0.5) > 0.8);
        assert!(exit(0.5) < 0.5);
    }

    /// 时长都很短：动画只是收尾，不让人等。
    #[test]
    fn durations_stay_short() {
        assert!(FAST < BASE && BASE < SLOW);
        assert!(SLOW <= Duration::from_millis(200));
    }

    /// 进场第一帧就读得清，不是从透明开始。
    #[test]
    fn entrances_start_readable() {
        assert!(close(enter_opacity(0.), FLOOR));
        assert!(close(enter_opacity(1.), 1.));
    }

    /// 悬停底色亮起立即到位，熄灭时才有过渡。
    #[test]
    fn highlight_lights_up_at_once_and_fades_out() {
        let now = Instant::now();
        let on = HighlightState { from: 0., to: 1., started: now, hovered: Default::default() };
        assert_eq!(on.value(now), 1.);
        let off = HighlightState { from: 1., to: 0., started: now, hovered: Default::default() };
        assert!(close(off.value(now), 1.));
        assert!(close(off.value(now + FAST), 0.));
    }

    #[test]
    fn visibility_stays_within_zero_and_one() {
        for step in 0..=20 {
            let progress = step as f32 / 20.;
            for leaving in [false, true] {
                let visibility = Phase { progress, leaving }.visibility();
                assert!((0. ..=1.).contains(&visibility));
            }
        }
        assert_eq!(Phase::SETTLED.visibility(), 1.);
        assert!(close(Phase { progress: 0., leaving: true }.visibility(), 0.));
    }

    #[test]
    fn shake_returns_to_rest() {
        assert!(close(shake(0.), 0.));
        assert!(close(shake(1.), 0.));
        assert!((0..=100).any(|step| shake(step as f32 / 100.).abs() > 1.));
    }

    #[test]
    fn dots_look_the_same_when_still() {
        let still: Vec<f32> = (0..3).map(|index| pulse(0., index)).collect();
        assert!(still.iter().all(|value| close(*value, still[0])));
        assert!((0..3).all(|index| (0..=10).map(|step| pulse(step as f32 / 10., index)).fold(0., f32::max) > 0.9));
    }
}
