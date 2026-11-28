//! 动效。规则见 `docs/design.md` §5.5；这里是唯一的实现位置，视图不直接写时长、缓动和位移。
//!
//! 基调：动效只服务于控件反馈和浮层出现。导航、换页、布局变化即时完成；悬停、按下以 `FAST` 渐变，
//! 键盘移动的高亮即时；
//! 任何动画都不推迟内容出现（没有延迟、没有错峰），不用过冲；浮层关上时立即移除，不挡输入。
//! 任何信息都不能只靠动画表达。
//! 开启“减少动态效果”时没有动画：元素直接以最终状态出现、消失；循环动画停在起始帧。
//!
//! 几种做法，按场合选：
//! - [`Entrance`]（及 [`fade_in`]）：动画状态记在元素上，元素一挂上就播放。包裹层的 `id` 要稳定，
//!   需要重播时换一个 `id`；不要用会变的 `id` 包裹可能持有键盘焦点的控件，`id` 一变焦点就丢了。
//! - [`presence`] + [`pop`]：浮层（弹窗、菜单、下拉框）的进场。关上时 [`presence`] 直接返回 `None`。
//! - [`Highlight`]：悬停、按下、选中的底色，画在控件的最底层。
//! - [`tween`]：一个数值（不透明度、角度）的过渡。[`turn`] 用它转动展开箭头。
//! - [`spinner`]：只用在图标上。GPUI 只能变换 SVG 和图片，
//!   不能缩放、旋转整块内容。
//!
//! GPUI 没有图层合成：`opacity` 按每个图元分别相乘。带阴影的浮层淡入时，阴影会先于卡片显出来、
//! 像一块暗板透过卡片，所以 [`pop`] 把阴影强度按不透明度的三次方渐显。

use std::cell::Cell;
use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::base::motion::{Easing, Presence, PresencePhase, Transition, TransitionId};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::*;

// ---- 时长、位移 ----

/// 状态变化（展开箭头、开关、发送按钮、标题栏前后台）、搜索对话框与行内提示的淡入。
pub const FAST: Duration = Duration::from_millis(100);
/// 菜单、下拉列表进场。
pub const BASE: Duration = Duration::from_millis(150);
/// 弹窗进场。
pub const SLOW: Duration = Duration::from_millis(200);

/// 菜单、下拉列表、小块内容的位移。
pub const NUDGE: f32 = 4.;
/// 弹窗的位移。
pub const SHORT: f32 = 8.;

/// 循环动画的帧率上限：只是状态提示，不值得每帧重绘。
const LOOP_FPS: f32 = 30.;

// ---- 缓动 ----

/// 进场：很快到位，最后一段放缓。
const ENTER: Easing = Easing::CubicBezier { x1: 0.16, y1: 1., x2: 0.3, y2: 1. };
/// 状态变化（底色、旋转）。
const STANDARD: Easing = Easing::CubicBezier { x1: 0.2, y1: 0., x2: 0., y2: 1. };

pub fn enter(t: f32) -> f32 {
    ENTER.sample(t.clamp(0., 1.))
}

pub fn standard(t: f32) -> f32 {
    STANDARD.sample(t.clamp(0., 1.))
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

/// 一次性的进场：元素挂上时从透明（可选：偏一点）过渡到原位。见模块说明里的第一种做法。
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
        let element = element.opacity(enter(t));
        match self.from {
            Some((edge, distance)) => shift(element, edge, distance, 1. - enter(t)),
            None => element,
        }
    }

    pub fn play<E: IntoElement + Styled + 'static>(self, element: E, id: impl Into<ElementId>) -> AnimationElement<E> {
        element.with_animation(id, Animation::new(self.duration), move |element, t| self.apply(element, t))
    }
}

/// 淡入（`FAST`，不位移）。用于行内的提示、错误、保存结果这类就地出现的小块内容。
pub fn fade_in<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>) -> AnimationElement<E> {
    Entrance::new(FAST).play(element, id)
}

// ---- 浮层进场 ----

/// 正在进场的进度（线性，0 到 1）。浮层关上时立即移除，没有退场。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Phase {
    pub progress: f32,
}

impl Phase {
    /// 已经完全显示、没有动画。
    pub const SETTLED: Phase = Phase { progress: 1. };

    /// 内容的可见程度（不透明度，0 到 1）。
    pub fn visibility(self) -> f32 {
        enter(self.progress)
    }
}

/// 关上时记进组件库的那一小段：不能是 0（组件库用它做除数），短到看不出来。
const CLOSE: Duration = Duration::from_millis(1);

/// 浮层（弹窗、菜单、下拉列表）的进场状态，按 `id` 记在当前视图里。`present` 时返回进场进度，
/// 关上时直接返回 `None`（立即移除，不画退场，不挡输入）。
///
/// 在视图的 `render` 里调用，并且每帧都要调用（关着的时候也是）：关上的状态也要记进组件库的 `Presence`，
/// 下次打开才会重新播放进场。
pub fn presence(id: impl Into<ElementId>, present: bool, enter: Duration, window: &mut Window, cx: &mut App) -> Option<Phase> {
    // 线性推进，缓动在画的时候按属性分别套（不透明度、位移、阴影各用各的曲线）。
    let transition = Transition::new(if present { enter } else { CLOSE }).easing(Easing::Linear);
    let sample = Presence::new(TransitionId::from(id.into()), present).transition(transition).sample(window, cx);
    if !present {
        return None;
    }
    match sample.phase {
        PresencePhase::Present => Some(Phase::SETTLED),
        PresencePhase::Entering => Some(Phase { progress: sample.progress }),
        // 关上后马上又打开（还在那 1ms 的退场里）：从头进场。
        PresencePhase::Exiting | PresencePhase::Absent => Some(Phase { progress: 0. }),
    }
}

/// 浮层（弹窗卡片、菜单、下拉列表）在 `phase` 时的样子：不透明度、从 `from` 那边移过来的位移、渐显的阴影。
/// `shadow` 是完全显示时的阴影（见 `design::shadow`）。
pub fn pop<E: Styled>(element: E, from: Edge, distance: f32, phase: Phase, shadow: Vec<BoxShadow>) -> E {
    let visibility = phase.visibility();
    let ink = visibility * visibility * visibility;
    let shadow = shadow.into_iter().map(|mut shadow| {
        shadow.color = shadow.color.opacity(ink);
        shadow
    });
    shift(element.opacity(visibility), from, distance, 1. - visibility).shadow(shadow.collect())
}

// ---- 悬停、按下、选中的底色 ----

/// 一个 0–1 的值朝目标过渡（标准曲线）。没有时长（或减少动态效果）时直接到目标。
#[derive(Debug, Clone, Copy)]
struct Fade {
    from: f32,
    to: f32,
    started: Instant,
}

impl Fade {
    fn at(value: f32, now: Instant) -> Self {
        Self { from: value, to: value, started: now }
    }

    fn value(&self, now: Instant, duration: Option<Duration>) -> f32 {
        let Some(duration) = duration.filter(|duration| !duration.is_zero()) else { return self.to };
        let t = now.saturating_duration_since(self.started).as_secs_f32() / duration.as_secs_f32();
        self.from + (self.to - self.from) * standard(t)
    }

    /// 换目标（从当前值接着走），返回现在的值。
    fn toward(&mut self, target: f32, now: Instant, duration: Option<Duration>) -> f32 {
        if self.to != target {
            self.from = self.value(now, duration);
            self.to = target;
            self.started = now;
        }
        self.value(now, duration)
    }
}

/// 悬停层、按下层（叠在控件上的半透明底色）跨帧的状态：指针是否在上面、是否按着，以及两层各自的过渡。
/// [`Highlight`] 和组件库按钮外面的 `FocusFrame` 共用。克隆共享同一份状态。
#[derive(Debug, Clone, Default)]
pub struct Tint(Rc<TintInner>);

#[derive(Debug)]
struct TintInner {
    hovered: Cell<bool>,
    pressed: Cell<bool>,
    hover: Cell<Fade>,
    press: Cell<Fade>,
}

impl Default for TintInner {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            hovered: Cell::new(false),
            pressed: Cell::new(false),
            hover: Cell::new(Fade::at(0., now)),
            press: Cell::new(Fade::at(0., now)),
        }
    }
}

impl Tint {
    pub fn new() -> Self {
        Self::default()
    }

    /// paint 里调用（指针是否在上面要到 paint 时才准）：按指针现在的状态推进两层的过渡，画悬停层、按下层
    /// （按下层逐渐盖过悬停层），并登记指针进出、按下松开时重画所在视图。
    /// `smooth` 时两层以 [`FAST`] 渐变（减少动态效果时即时），过渡中每帧重画，结束即停。
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &self,
        bounds: Bounds<Pixels>,
        radius: Pixels,
        smooth: bool,
        (hover_color, pressed_color): (Option<Hsla>, Option<Hsla>),
        hitbox: &Hitbox,
        window: &mut Window,
        cx: &App,
    ) {
        let state = &self.0;
        // 触屏上没有悬停：点过之后 GPUI 记的指针位置还留在手指离开的地方，不能让底色一直亮着。
        let over = hitbox.is_hovered(window) && !window.last_input_was_touch();
        state.hovered.set(over);
        let down = over && state.pressed.get() && pressed_color.is_some();
        let duration = (smooth && !cx.reduce_motion()).then_some(FAST);
        let now = Instant::now();
        let step = |cell: &Cell<Fade>, target: f32| {
            let mut fade = cell.get();
            let value = fade.toward(target, now, duration);
            cell.set(fade);
            (value, fade.to)
        };
        let (hover, hover_to) = step(&state.hover, if over { 1. } else { 0. });
        let (press, press_to) = step(&state.press, if down { 1. } else { 0. });
        if hover != hover_to || press != press_to {
            window.request_animation_frame();
        }
        let hover_strength = if pressed_color.is_some() { hover * (1. - press) } else { hover };
        if let Some(color) = hover_color.filter(|_| hover_strength > 0.) {
            window.paint_quad(fill(bounds, color.opacity(hover_strength)).corner_radii(radius));
        }
        if let Some(color) = pressed_color.filter(|_| press > 0.) {
            window.paint_quad(fill(bounds, color.opacity(press)).corner_radii(radius));
        }
        self.listen(hitbox, pressed_color.is_some(), window);
    }

    /// 高对比度下的组件库按钮：不叠底色（会盖住按钮自己固定颜色的文字），悬停时在内侧画 2px 描边，
    /// 按住时加粗到 3px。即时生效。
    pub fn paint_ring(&self, bounds: Bounds<Pixels>, radius: Pixels, color: Hsla, hitbox: &Hitbox, window: &mut Window) {
        let state = &self.0;
        let over = hitbox.is_hovered(window) && !window.last_input_was_touch();
        state.hovered.set(over);
        let width = match (over, over && state.pressed.get()) {
            (_, true) => Some(3.),
            (true, false) => Some(2.),
            _ => None,
        };
        if let Some(width) = width {
            window.paint_quad(quad(bounds, radius, transparent_black(), px(width), color, BorderStyle::Solid));
        }
        self.listen(hitbox, true, window);
    }

    /// 指针进出、按下松开时重画所在的视图（缓存的视图不会自己重画）。
    fn listen(&self, hitbox: &Hitbox, track_press: bool, window: &mut Window) {
        let view = window.current_view();
        {
            let (hitbox, tint) = (hitbox.clone(), self.clone());
            window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture {
                    let now = hitbox.is_hovered(window) && !window.last_input_was_touch();
                    if now != tint.0.hovered.get() {
                        tint.0.hovered.set(now);
                        cx.notify(view);
                    }
                }
            });
        }
        if track_press {
            let (hitbox, tint) = (hitbox.clone(), self.clone());
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture && event.button == MouseButton::Left && hitbox.is_hovered(window) {
                    tint.0.pressed.set(true);
                    cx.notify(view);
                }
            });
            let tint = self.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture && event.button == MouseButton::Left && tint.0.pressed.get() {
                    tint.0.pressed.set(false);
                    cx.notify(view);
                }
            });
        }
    }
}

/// 悬停、按下、选中时的底色，画在控件的最底层。放在控件的第一个子元素（铺满控件），控件本身不再设
/// `bg` / `hover`。悬停按这一层自己的范围判断，被别的浮层盖住时不算悬停。
///
/// 悬停、按下以 [`FAST`] 渐变（减少动态效果时即时）。键盘移动的高亮走选中色（`active`），默认即时，
/// 上下移动时不拖影；选中状态的切换可以用 [`Highlight::animate`] 要求过渡（如发送与停止之间的底色交替）。
pub struct Highlight {
    id: ElementId,
    /// 选中（`active`）时的底色。
    color: Hsla,
    /// 悬停时叠上去的底色；`None` 表示不跟随悬停。
    hover: Option<Hsla>,
    /// 按住时叠上去的底色（代替悬停色）；`None` 表示没有按下态。
    pressed: Option<Hsla>,
    radius: Pixels,
    active: bool,
    outset: Pixels,
    transition: Option<Duration>,
    smooth: bool,
}

/// 见 [`Highlight`]。`active` 时（选中、菜单打开）一直显示 `color`；悬停时也铺 `color`。
pub fn highlight(id: impl Into<ElementId>, color: Hsla, radius: f32, active: bool) -> Highlight {
    Highlight {
        id: id.into(),
        color,
        hover: Some(color),
        pressed: None,
        radius: px(radius),
        active,
        outset: px(0.),
        transition: None,
        smooth: true,
    }
}

impl Highlight {
    /// 不跟随悬停（只表示选中）。
    pub fn without_hover(mut self) -> Self {
        self.hover = None;
        self
    }

    /// 悬停时的底色（叠在选中色上面）。
    pub fn hover_color(mut self, color: Hsla) -> Self {
        self.hover = Some(color);
        self
    }

    /// 按住鼠标时的底色（代替悬停色）。
    pub fn pressed_color(mut self, color: Hsla) -> Self {
        self.pressed = Some(color);
        self
    }

    /// 选中状态变化时用 `duration` 过渡（标准曲线）；默认即时。
    pub fn animate(mut self, duration: Duration) -> Self {
        self.transition = Some(duration);
        self
    }

    /// 悬停、按下的底色即时切换（默认以 [`FAST`] 渐变）。高对比度下行的文字要和底色同时换，
    /// 渐变的中途会有一段文字与底色对比度不够。
    pub fn instant(mut self) -> Self {
        self.smooth = false;
        self
    }

    /// 向外多铺这么宽：绝对定位从边框内侧算起，控件有边框（如焦点框）时用它铺满整个控件。
    pub fn outset(mut self, outset: f32) -> Self {
        self.outset = px(outset);
        self
    }
}

/// [`Highlight`] 跨帧的状态：选中色的过渡，悬停层、按下层。
#[derive(Debug, Clone)]
struct HighlightState {
    active: Fade,
    tint: Tint,
}

/// [`Highlight`] 在 prepaint 和 paint 之间传的东西。
pub struct HighlightPaint {
    hitbox: Hitbox,
    value: f32,
    tint: Tint,
}

impl IntoElement for Highlight {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Highlight {
    type RequestLayoutState = ();
    type PrepaintState = HighlightPaint;

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
    ) -> HighlightPaint {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let target = if self.active { 1. } else { 0. };
        let transition = if cx.reduce_motion() { None } else { self.transition };
        let (value, tint) = window.with_element_state(global_id.expect("Highlight 总有 id"), |state: Option<HighlightState>, window| {
            let now = Instant::now();
            let mut state = state.unwrap_or_else(|| HighlightState { active: Fade::at(target, now), tint: Tint::new() });
            let value = state.active.toward(target, now, transition);
            if value != target {
                window.request_animation_frame();
            }
            ((value, state.tint.clone()), state)
        });
        HighlightPaint { hitbox, value, tint }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut HighlightPaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let HighlightPaint { hitbox, value, tint } = state;
        if *value > 0. {
            window.paint_quad(fill(bounds, self.color.opacity(*value)).corner_radii(self.radius));
        }
        if self.hover.is_some() || self.pressed.is_some() {
            tint.paint(bounds, self.radius, self.smooth, (self.hover, self.pressed), hitbox, window, cx);
        }
    }
}

// ---- 开关 ----

/// 开关的轨道和圆钮（[`crate::design::switch`] 里用）：切换时圆钮滑到另一头（`FAST`、标准曲线），
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
        let t = now.saturating_duration_since(self.started).as_secs_f32() / FAST.as_secs_f32();
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

/// 同 [`tween`]，时长另给（如侧边栏收起、展开用 [`BASE`]）。
pub fn tween_for(id: impl Into<ElementId>, target: f32, duration: Duration, window: &mut Window, cx: &mut App) -> f32 {
    gpui_kit::base::motion::transition(TransitionId::from(id.into()), target, Transition::new(duration).easing(STANDARD), window, cx)
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    // 不用 `super::*`：那样会把 GPUI 的 `test` 宏一起引进来，盖过标准的 `#[test]`。
    use gpui::TestAppContext;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Context, Entity, IntoElement, Render, Window, div};

    use super::{BASE, FAST, Phase, SLOW, enter, presence, standard};

    /// 每次渲染记下 [`presence`] 的结果。
    struct Host {
        open: bool,
        phase: Option<Phase>,
    }

    impl Render for Host {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.phase = presence("host-presence", self.open, SLOW, window, cx);
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
    fn closing_removes_at_once_and_reopening_plays_again(cx: &mut TestAppContext) {
        let (window, host) = host(cx);
        assert_eq!(step(cx, window, &host, false, Duration::ZERO), None);

        let opening = step(cx, window, &host, true, Duration::ZERO).expect("打开后要画");
        assert!(opening.progress < 0.1, "{opening:?}");
        assert_eq!(step(cx, window, &host, true, SLOW), Some(Phase::SETTLED));

        // 关上：立即移除，没有退场。
        assert_eq!(step(cx, window, &host, false, Duration::ZERO), None);
        assert_eq!(step(cx, window, &host, false, FAST), None);

        // 再打开：重新进场。
        let again = step(cx, window, &host, true, Duration::ZERO).expect("再打开要画");
        assert!(again.progress < 0.1, "{again:?}");
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

    /// 按钮的悬停层：有时长时从当前值渐变，`FAST` 后到达目标；中途换目标从当前值接着走；没有时长时直接到目标。
    #[test]
    fn tint_fades_toward_its_target() {
        let start = std::time::Instant::now();
        let mut fade = super::Fade::at(0., start);
        assert_eq!(fade.toward(1., start, Some(FAST)), 0.);
        let halfway = fade.toward(1., start + FAST / 2, Some(FAST));
        assert!(halfway > 0. && halfway < 1., "{halfway}");
        // 鼠标中途移出：从现在的值往回走，不跳。
        let back = fade.toward(0., start + FAST / 2, Some(FAST));
        assert!((back - halfway).abs() < 1e-6, "{back} {halfway}");
        assert_eq!(fade.toward(0., start + FAST / 2 + FAST, Some(FAST)), 0.);
        // 列表行、减少动态效果：即时。
        let mut instant = super::Fade::at(0., start);
        assert_eq!(instant.toward(1., start, None), 1.);
    }

    #[test]
    fn durations_stay_short() {
        // 规则：单个动画不超过 200ms。
        assert!(FAST < BASE && BASE < SLOW);
        assert!(SLOW <= Duration::from_millis(200));
    }

    #[test]
    fn curves_start_at_zero_and_end_at_one() {
        for curve in [enter, standard] {
            assert!(close(curve(0.), 0.));
            assert!(close(curve(1.), 1.));
        }
        // 进场先快：一半时间已经走完大半。
        assert!(enter(0.5) > 0.8);
    }

    #[test]
    fn visibility_stays_within_zero_and_one() {
        for step in 0..=20 {
            let visibility = Phase { progress: step as f32 / 20. }.visibility();
            assert!((0. ..=1.).contains(&visibility));
        }
        assert_eq!(Phase::SETTLED.visibility(), 1.);
    }
}
