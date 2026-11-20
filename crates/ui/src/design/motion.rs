//! 动效。规则见 `docs/design.md` §5.5；这里是唯一的实现位置，视图不直接写时长、缓动和位移。
//!
//! 基调：有方向、有层次，但不抢内容。进场用先快后慢的曲线，浮层（弹窗、菜单）的位移带一点过冲，
//! 退场比进场短、只往回退一半距离；任何信息都不能只靠动画表达。
//! 开启“减少动态效果”时没有动画：元素直接以最终状态出现、消失；循环动画停在起始帧。
//!
//! 几种做法，按场合选：
//! - [`Entrance`]（及 [`fade_in`] / [`nudge_in`] / [`rise_in`] / [`slide_in`] / [`error_in`]）：
//!   动画状态记在元素上，元素一挂上就播放。适合“出现即挂载”的内容。包裹层的 `id` 要稳定，
//!   需要重播时换一个 `id`；不要用会变的 `id` 包裹可能持有键盘焦点的控件，`id` 一变焦点就丢了。
//! - [`presence`] + [`pop`] / [`reveal`]：进场和退场。关闭后元素还要再画一小段时间，调用方在
//!   [`presence`] 返回 `None` 之前继续渲染它（弹窗、菜单、下拉框、展开的表单）。
//! - [`Timeline`]：按登记的时刻算进度，不依赖元素状态。虚拟列表的行滚出视口再滚回来，元素状态已经没了，
//!   用前两种会重播；用这个只在真正出现、展开、折叠时播放一次。
//! - [`Highlight`]：悬停、选中的底色过渡，画在控件的最底层。
//! - [`tween`]：一个数值（不透明度、角度）的过渡。[`turn`] 用它转动展开箭头。
//! - [`spinner`] / [`typing_dots`]：只用在图标和小圆点上。GPUI 只能变换 SVG 和图片，
//!   不能缩放、旋转整块内容。
//!
//! GPUI 没有图层合成：`opacity` 按每个图元分别相乘。带阴影的浮层淡入时，阴影会先于卡片显出来、
//! 像一块暗板透过卡片，所以 [`pop`] 把阴影强度按不透明度的三次方渐显。

use std::cell::Cell;
use std::collections::HashMap;
use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::hash::Hash;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::base::motion::{Easing, MotionReveal, Presence, PresencePhase, Transition, TransitionId};
use gpui_kit::component::{Icon, IconName, h_flex};
use gpui_kit::*;

// ---- 时长、位移、错峰 ----

/// 悬停、选中、小块提示的淡入，以及所有退场。
pub const FAST: Duration = Duration::from_millis(140);
/// 菜单、下拉框、列表行进场、展开与折叠。
pub const BASE: Duration = Duration::from_millis(220);
/// 弹窗、页面切换、首页标题。
pub const SLOW: Duration = Duration::from_millis(320);

/// 提示、错误这类小块内容的位移。
pub const NUDGE: f32 = 4.;
/// 行内元素、列表行、菜单的位移。
pub const SHORT: f32 = 8.;
/// 页面切换。
pub const PAGE: f32 = 12.;
/// 弹窗。
pub const MEDIUM: f32 = 16.;

/// 菜单里的选项。
pub const STAGGER_ITEM: Duration = Duration::from_millis(15);
/// 侧边栏第一次载入的会话行。
pub const STAGGER_ROW: Duration = Duration::from_millis(20);
/// 首页标题的几个部分。
pub const STAGGER_HERO: Duration = Duration::from_millis(60);
/// 错峰最多排到第几个；再往后的和最后一个同时出现，长列表不会拖很久。
pub const STAGGER_MAX: usize = 8;

/// 循环动画的帧率上限：只是状态提示，不值得每帧重绘。
const LOOP_FPS: f32 = 30.;

// ---- 缓动 ----

/// 进场：很快到位，最后一段放缓。
const ENTER: Easing = Easing::CubicBezier { x1: 0.16, y1: 1., x2: 0.3, y2: 1. };
/// 退场：先慢后快，干脆地离开。
const EXIT: Easing = Easing::CubicBezier { x1: 0.4, y1: 0., x2: 1., y2: 1. };
/// 状态变化（底色、高度、旋转）。
const STANDARD: Easing = Easing::CubicBezier { x1: 0.2, y1: 0., x2: 0., y2: 1. };
/// 过冲的幅度：约 7%，到位前轻轻越过一点再回来。
const BACK: f32 = 1.4;

pub fn enter(t: f32) -> f32 {
    ENTER.sample(t.clamp(0., 1.))
}

pub fn exit(t: f32) -> f32 {
    EXIT.sample(t.clamp(0., 1.))
}

pub fn standard(t: f32) -> f32 {
    STANDARD.sample(t.clamp(0., 1.))
}

/// 带过冲的先快后慢（ease-out-back）。只用于位移；结果会略大于 1，不能拿来当不透明度。
pub fn overshoot(t: f32) -> f32 {
    let t = t.clamp(0., 1.) - 1.;
    1. + (BACK + 1.) * t * t * t + BACK * t * t
}

// ---- 方向与位移 ----

/// 元素从哪一边来（退场时往这一边退）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// 把元素画在离终点还差 `remaining`（1 是整段距离，0 是到位）的地方。用相对定位，不影响布局。
/// 不要用在已经 `absolute()` 定位的元素上：那会改掉它真正的位置。
pub fn shift<E: Styled>(element: E, from: Edge, distance: f32, remaining: f32) -> E {
    let offset = px(distance * remaining);
    match from {
        Edge::Top => element.top(-offset),
        Edge::Bottom => element.top(offset),
        Edge::Left => element.left(-offset),
        Edge::Right => element.left(offset),
    }
}

// ---- 进场 ----

/// 一次性的进场：元素挂上时从透明（可选：偏一点）过渡到原位。见模块说明里的第一种做法。
#[derive(Debug, Clone, Copy)]
pub struct Entrance {
    duration: Duration,
    delay: Duration,
    from: Option<(Edge, f32)>,
}

impl Entrance {
    pub fn new(duration: Duration) -> Self {
        Self { duration, delay: Duration::ZERO, from: None }
    }

    /// 从 `edge` 那边偏 `distance` 像素的地方进来。
    pub fn from(mut self, edge: Edge, distance: f32) -> Self {
        self.from = Some((edge, distance));
        self
    }

    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// 第 `index` 个错峰出现，间隔 `step`；超过 [`STAGGER_MAX`] 的不再往后推。
    pub fn stagger(self, index: usize, step: Duration) -> Self {
        self.delay(step * index.min(STAGGER_MAX) as u32)
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
        one_shot(element, id, self.delay, self.duration, move |element, t| self.apply(element, t))
    }
}

/// 一次性动画：先停在起点 `delay`，再用 `duration` 从 0 走到 1（线性，缓动由 `apply` 自己套）。
///
/// 所有一次性动画都是“停一下 + 播放”这两段，延迟为 0 时也一样（停 1ms）。GPUI 按元素 id 记着播到第几段：
/// 同一个 id 这一帧是两段、下一帧变成一段（比如侧边栏只在刚载入时错峰），它会去取不存在的第二段而崩溃。
/// 段数固定就不会出这种事。GPUI 的动画元素本身会遵守“减少动态效果”（直接画最终状态）。
fn one_shot<E: IntoElement + Styled + 'static>(
    element: E,
    id: impl Into<ElementId>,
    delay: Duration,
    duration: Duration,
    apply: impl Fn(E, f32) -> E + 'static,
) -> AnimationElement<E> {
    element.with_animations(id, vec![Animation::new(delay.max(MIN_HOLD)), Animation::new(duration)], move |element, index, t| {
        apply(element, if index == 0 { 0. } else { t })
    })
}

/// 不需要延迟时“停在起点”的那一段的长度：不能是 0（GPUI 用它做除数），短到看不出来。
const MIN_HOLD: Duration = Duration::from_millis(1);

/// 淡入。用于遮罩、整块区域的切换。
pub fn fade_in<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>) -> AnimationElement<E> {
    Entrance::new(FAST).play(element, id)
}

/// 淡入并上浮一点。用于提示、错误、保存结果这类就地出现的小块内容。
pub fn nudge_in<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>) -> AnimationElement<E> {
    Entrance::new(FAST).from(Edge::Bottom, NUDGE).play(element, id)
}

/// 从下方浮上来并淡入。用于新出现的行、展开的内容。
pub fn rise_in<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>) -> AnimationElement<E> {
    Entrance::new(BASE).from(Edge::Bottom, SHORT).play(element, id)
}

/// 从 `from` 那边滑进来并淡入（页面切换、用户消息、侧边栏的新行）。
pub fn slide_in<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>, from: Edge, distance: f32) -> AnimationElement<E> {
    Entrance::new(BASE).from(from, distance).play(element, id)
}

/// 出错：淡入，再左右轻轻晃两下，表示“这里需要处理”。原因必须同时写在文字里。
pub fn error_in<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>) -> AnimationElement<E> {
    one_shot(element, id, Duration::ZERO, FAST * 3, |element, t| element.opacity(enter(t * 3.)).left(px(shake(t))))
}

/// 操作结果的一行提示：成功时 [`nudge_in`]，失败时 [`error_in`]。
pub fn message_in<E: IntoElement + Styled + 'static>(element: E, id: impl Into<ElementId>, error: bool) -> AnyElement {
    if error { error_in(element, id).into_any_element() } else { nudge_in(element, id).into_any_element() }
}

/// 晃动的横向位移：两个来回，越晃越小，结束时回到原位。
fn shake(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    NUDGE * (TAU * 2. * t).sin() * (1. - t)
}

/// 按进度（0 到 1，已经过缓动）画出从 `from` 那边滑进来的中间状态。给 [`Timeline`] 用。
pub fn slide<E: Styled>(element: E, t: f32, from: Edge, distance: f32) -> E {
    shift(element.opacity(t.clamp(0., 1.)), from, distance, 1. - t)
}

// ---- 进场与退场 ----

/// 正在进场或退场的进度。`progress` 是线性的：进场从 0 到 1，退场从 1 回到 0。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Phase {
    pub progress: f32,
    pub leaving: bool,
}

impl Phase {
    /// 已经完全显示、没有动画。
    pub const SETTLED: Phase = Phase { progress: 1., leaving: false };

    pub fn is_settled(self) -> bool {
        !self.leaving && self.progress >= 1.
    }

    /// 内容的可见程度（不透明度，0 到 1）。
    pub fn visibility(self) -> f32 {
        if self.leaving { 1. - exit(1. - self.progress) } else { enter(self.progress) }
    }

    /// 高度展开的程度（0 到 1）。
    pub fn extent(self) -> f32 {
        if self.leaving { 1. - exit(1. - self.progress) } else { standard(self.progress) }
    }
}

/// 进场、退场的状态，按 `id` 记在当前视图里。`present` 变成 `false` 后还会返回一段时间的
/// `Some(Phase { leaving: true, .. })`，调用方这期间继续画它（不响应操作）；返回 `None` 才真正移除。
/// 中途反悔（打开又立刻关上）时从当前位置往回走，不会跳。
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

/// 浮层（弹窗卡片、菜单、下拉列表）在 `phase` 时的样子：不透明度、带过冲的位移、渐显的阴影。
/// `shadow` 是完全显示时的阴影（见 `design::shadow`）。
pub fn pop<E: Styled>(element: E, from: Edge, distance: f32, phase: Phase, shadow: Vec<BoxShadow>) -> E {
    let visibility = phase.visibility();
    let remaining = if phase.leaving { exit(1. - phase.progress) * 0.5 } else { 1. - overshoot(phase.progress) };
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

/// 按 `phase` 展开、收起高度（量出内容的高度，按进度裁剪），内容同时淡入淡出。
/// 完全展开时不再测量，但保持同样的元素结构，里面控件的状态（焦点、滚动位置）不会丢。
pub fn reveal(id: impl Into<ElementId>, phase: Phase, child: impl IntoElement) -> AnyElement {
    let id = id.into();
    if phase.is_settled() {
        return div().id(id).w_full().child(div().child(child)).into_any_element();
    }
    let content = div().opacity(phase.visibility()).child(child).into_any_element();
    MotionReveal::new(id, phase.extent(), content).into_any_element()
}

/// 展开、收起用的 [`presence`]（展开 BASE，收起 FAST）。返回 `None` 时不用画；否则把内容交给 [`reveal`]。
pub fn expansion(id: impl Into<ElementId>, open: bool, window: &mut Window, cx: &mut App) -> Option<Phase> {
    presence(id, open, BASE, FAST, window, cx)
}

/// [`expansion`] + [`reveal`]：`open` 时展开，关上时收起后移除。`child` 只在需要画时才构造。
pub fn expand(
    id: impl Into<ElementId>,
    open: bool,
    window: &mut Window,
    cx: &mut App,
    child: impl FnOnce() -> AnyElement,
) -> Option<AnyElement> {
    let id = id.into();
    let phase = expansion(id.clone(), open, window, cx)?;
    Some(reveal(id, phase, child()))
}

// ---- 不依赖元素状态的时刻表 ----

#[derive(Debug, Clone, Copy)]
struct Clip {
    started: Instant,
    leaving: bool,
}

/// 进场、退场时刻表：登记出现、消失的时刻，渲染时按经过的时间算进度。用于虚拟列表。
pub struct Timeline<K> {
    clips: HashMap<K, Clip>,
    enter: Duration,
    exit: Duration,
}

impl<K: Eq + Hash + Clone> Timeline<K> {
    pub fn new(enter: Duration, exit: Duration) -> Self {
        Self { clips: HashMap::new(), enter, exit }
    }

    /// 开始进场。正在退场时从当前位置折回。
    pub fn enter(&mut self, key: K) {
        self.enter_at(key, Instant::now());
    }

    /// 开始退场。正在进场时从当前位置折回。
    pub fn leave(&mut self, key: K) {
        self.leave_at(key, Instant::now());
    }

    fn enter_at(&mut self, key: K, now: Instant) {
        let from = self.progress_at(&key, now).unwrap_or(0.);
        let started = now.checked_sub(self.enter.mul_f32(from)).unwrap_or(now);
        self.clips.insert(key, Clip { started, leaving: false });
    }

    fn leave_at(&mut self, key: K, now: Instant) {
        let from = self.progress_at(&key, now).unwrap_or(1.);
        let started = now.checked_sub(self.exit.mul_f32(1. - from)).unwrap_or(now);
        self.clips.insert(key, Clip { started, leaving: true });
    }

    pub fn clear(&mut self) {
        self.clips.clear();
    }

    /// 丢掉已经播完的和 `keep` 不要的。
    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        let now = Instant::now();
        let (enter, exit) = (self.enter, self.exit);
        self.clips.retain(|key, clip| {
            let length = if clip.leaving { exit } else { enter };
            now.duration_since(clip.started) < length && keep(key)
        });
    }

    /// 登记着的（包括刚播完、还没被 [`Self::retain`] 丢掉的）。
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.clips.keys()
    }

    /// 线性进度：进场从 0 到 1，退场从 1 到 0；没登记或已经播完时为 `None`。
    fn progress_at(&self, key: &K, now: Instant) -> Option<f32> {
        let clip = self.clips.get(key)?;
        let length = if clip.leaving { self.exit } else { self.enter };
        let elapsed = now.saturating_duration_since(clip.started);
        if elapsed >= length || length.is_zero() {
            return None;
        }
        let t = elapsed.as_secs_f32() / length.as_secs_f32();
        Some(if clip.leaving { 1. - t } else { t })
    }

    /// 当前的进度；没登记、已经播完或减少动态效果时为 `None`（调用方按自己的状态画最终样子）。
    /// 还在播放时请求下一帧，调用方不用自己安排重绘。
    pub fn phase(&self, key: &K, window: &mut Window, cx: &App) -> Option<Phase> {
        if cx.reduce_motion() {
            return None;
        }
        let leaving = self.clips.get(key)?.leaving;
        let progress = self.progress_at(key, Instant::now())?;
        window.request_animation_frame();
        Some(Phase { progress, leaving })
    }
}

// ---- 悬停、选中的底色 ----

/// 悬停、选中时的底色，带过渡。放在控件的第一个子元素（铺满控件、画在最底层），控件本身不再设
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
    fn value(&self, now: Instant) -> f32 {
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
        BASE, Edge, Entrance, FAST, Phase, SHORT, SLOW, STAGGER_ROW, Timeline, enter, exit, message_in, overshoot, presence, pulse, shake,
        standard,
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

    /// 同一个 id 前后用不同的进场（错峰与否、成功与出错）。
    struct Shapes {
        staggered: bool,
        error: bool,
    }

    impl Render for Shapes {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let entrance = Entrance::new(BASE).from(Edge::Left, SHORT);
            let entrance = if self.staggered { entrance.stagger(3, STAGGER_ROW) } else { entrance };
            div().child(entrance.play(div(), "row-enter")).child(message_in(div(), "message-enter", self.error))
        }
    }

    /// 侧边栏的行只在列表刚载入时错峰：错峰的那段播完后同一个 id 换成不错峰的进场，不能崩。
    #[gpui::test]
    fn switching_the_entrance_under_the_same_id_does_not_crash(cx: &mut TestAppContext) {
        let (window, view) = cx.update(|cx| {
            gpui_kit::init(cx);
            let (window, view) =
                gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| Shapes { staggered: true, error: false })).unwrap();
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
        // 过了错峰的延迟：第一段播完，进到第二段。
        cx.executor().advance_clock(STAGGER_ROW * 3 + Duration::from_millis(20));
        draw(cx);
        cx.update(|cx| {
            view.update(cx, |view, _| {
                view.staggered = false;
                view.error = true;
            })
        });
        draw(cx);
        cx.executor().advance_clock(SLOW);
        draw(cx);
        cx.update(|cx| view.update(cx, |view, _| view.staggered = true));
        draw(cx);
    }

    #[gpui::test]
    fn reduced_motion_shows_and_removes_at_once(cx: &mut TestAppContext) {
        let (window, host) = host(cx);
        cx.update(|cx| cx.set_reduce_motion(true));
        step(cx, window, &host, false, Duration::ZERO);
        assert_eq!(step(cx, window, &host, true, Duration::ZERO), Some(Phase::SETTLED));
        assert_eq!(step(cx, window, &host, false, Duration::ZERO), None);
        // 时长本身没变（`BASE` 只是确认常量可用）。
        assert!(BASE > FAST);
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn curves_start_at_zero_and_end_at_one() {
        for curve in [enter, exit, standard, overshoot] {
            assert!(close(curve(0.), 0.));
            assert!(close(curve(1.), 1.));
        }
        // 进场先快：一半时间已经走完大半；退场先慢。
        assert!(enter(0.5) > 0.8);
        assert!(exit(0.5) < 0.5);
    }

    #[test]
    fn overshoot_goes_a_little_past_the_target() {
        let peak = (0..=100).map(|step| overshoot(step as f32 / 100.)).fold(0., f32::max);
        assert!(peak > 1.03 && peak < 1.1, "过冲 {peak}");
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

    #[test]
    fn timeline_plays_once_and_reverses_from_where_it_is() {
        let mut timeline = Timeline::new(Duration::from_millis(200), Duration::from_millis(100));
        let start = Instant::now();
        assert_eq!(timeline.progress_at(&1, start), None);

        timeline.enter_at(1, start);
        assert!(close(timeline.progress_at(&1, start + Duration::from_millis(50)).unwrap(), 0.25));
        assert_eq!(timeline.progress_at(&1, start + Duration::from_millis(200)), None);

        // 进场到一半时收起：从 0.5 往回退，退场一共 100ms，所以再过 50ms 收完。
        let half = start + Duration::from_millis(100);
        timeline.leave_at(1, half);
        assert!(close(timeline.progress_at(&1, half).unwrap(), 0.5));
        assert!(close(timeline.progress_at(&1, half + Duration::from_millis(25)).unwrap(), 0.25));
        assert_eq!(timeline.progress_at(&1, half + Duration::from_millis(50)), None);

        // 退场途中又展开：从当前位置往前走。
        let mut timeline = Timeline::new(Duration::from_millis(200), Duration::from_millis(100));
        timeline.leave_at(2, start);
        let quarter = start + Duration::from_millis(75);
        timeline.enter_at(2, quarter);
        assert!(close(timeline.progress_at(&2, quarter).unwrap(), 0.25));
    }
}
