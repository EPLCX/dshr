//! 平滑滚动与中键自动滚动，作用于整个窗口里所有可滚动的区域。
//!
//! GPUI 的滚动容器各自在冒泡阶段处理 [`ScrollEvent`]。这里在窗口根部的捕获阶段先拿到事件：
//! - 鼠标滚轮（按行的离散滚动）：吞掉原事件，把距离记下来，在接下来的几帧里指数减速地滚完
//!   （拨下去立刻起步，连拨越拨越快），逐帧以整物理像素的增量重新派发，于是不管是普通 div、
//!   虚拟列表（对话区）还是插件画的区域，都一起变得平滑。
//!
//! 捕获阶段按绘制顺序派发，而对话区的 `ScrollBounce` 也在捕获阶段接管滚轮并停止传播，
//! 所以 [`SmoothScroll::layer`] 必须是根元素的第一个子元素，先于所有滚动容器注册监听器。
//!   触控板等精确滚动本来就是连续的，直接放行；按着 Ctrl/⌘（缩放等用途）或设置为减少动态效果时也放行。
//! - 中键按下：在按下处显示一个锚点，之后指针离锚点越远滚得越快。按住中键拖动后松开即结束；
//!   单击中键则保持自动滚动，直到再次点击、按任意键、滚动滚轮或窗口失去焦点。
//!
//! 重新派发的滚动事件落在原来的位置（滚轮）或锚点处（自动滚动），保证滚的始终是同一个容器。
//! 派发会把窗口记录的指针位置挪过去，所以派发后补一个移到真实指针处的移动事件，悬停状态不会错乱。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::{Icon, IconName, Sizable as _, v_flex};
use gpui_kit::*;

use crate::design::palette;

/// 滚轮缓动的衰减率（1/秒）：每秒剩余距离按 e^(-rate·t) 缩小，一格约 0.1 秒滚完九成五。
/// 第一帧就走掉一大截（60Hz 下约 39%），拨下去立刻有反应，不会像从静止起步的弹簧那样发粘。
const WHEEL_RATE: f32 = 30.;
/// 收尾时的最低速度（逻辑像素/秒）：指数衰减的尾巴越走越慢，低于这个速度就按它匀速滚完，
/// 不拖着几个像素慢慢挪。
const WHEEL_MIN_SPEED: f32 = 240.;
/// 剩余距离小于这个值时一次滚完。
const WHEEL_REST_DISTANCE: f32 = 0.5;
/// 两帧之间按多长时间算：太长（比如窗口卡顿）时截断，避免一下子跳太远。
/// 不能太短：界面卡顿时每帧只按这么长推进，截得太狠滚动会越来越落后于实际时间。
const MAX_FRAME: Duration = Duration::from_millis(100);
/// 自动滚动时锚点周围不滚动的半径。
const AUTO_DEAD_ZONE: f32 = 10.;
/// 按住中键拖出这么远后松开才结束；没拖这么远就当作单击，进入保持模式。
const AUTO_DRAG_THRESHOLD: f32 = 6.;
/// 锚点圆的直径。
const INDICATOR_SIZE: f32 = 30.;

/// 一个窗口的平滑滚动状态。由窗口的根视图持有，在根元素里放一个 [`SmoothScroll::layer`]。
#[derive(Clone, Default)]
pub(crate) struct SmoothScroll(Rc<RefCell<State>>);

#[derive(Default)]
struct State {
    /// 两个方向上滚轮缓动的状态。
    wheel_x: Glide,
    wheel_y: Glide,
    /// 已经算出、但因为凑不满一个物理像素还没派发的距离（逻辑像素）。
    carry: Point<f32>,
    /// 滚轮事件的位置和修饰键，重新派发时沿用。
    wheel_position: Point<Pixels>,
    wheel_modifiers: Modifiers,
    /// 自动滚动；`None` 表示没在自动滚动。
    auto: Option<AutoScroll>,
    /// 当前按着的鼠标键，补发移动事件时带上，不打断拖动。
    pressed: Option<MouseButton>,
    /// 逐帧驱动是否已经排上；上一帧的时间。
    ticking: bool,
    last_frame: Option<Instant>,
    /// 正在派发自己合成的事件，捕获监听器要放行。
    replaying: bool,
}

/// 一个方向上的滚轮缓动：指数减速（ease-out）。速度与剩余距离成正比，新的一格加长剩余距离、
/// 速度随之立刻变大，所以连续拨动时越拨越快、跟手，停下后平稳减速。
#[derive(Clone, Copy, Default)]
struct Glide {
    /// 还没滚完的距离，与 [`ScrollDelta`] 同号（正值向上滚），逻辑像素。
    remaining: f32,
}

impl Glide {
    fn is_idle(&self) -> bool {
        self.remaining == 0.
    }

    /// 加上一格滚轮；换了方向就丢掉旧方向没滚完的部分，立刻往新方向滚。
    fn push(&mut self, delta: f32) {
        if delta * self.remaining < 0. {
            self.remaining = 0.;
        }
        self.remaining += delta;
    }

    /// 前进 `dt` 秒，返回这一段走过的距离。按解析式算，帧间隔不均匀也一致。
    fn advance(&mut self, dt: f32) -> f32 {
        if self.is_idle() {
            return 0.;
        }
        let left = self.remaining.abs();
        let step = (left * (1. - (-WHEEL_RATE * dt).exp())).max(WHEEL_MIN_SPEED * dt);
        if left - step < WHEEL_REST_DISTANCE {
            let moved = self.remaining;
            self.remaining = 0.;
            return moved;
        }
        let moved = step.copysign(self.remaining);
        self.remaining -= moved;
        moved
    }
}

impl State {
    fn stop_wheel(&mut self) {
        self.wheel_x = Glide::default();
        self.wheel_y = Glide::default();
        self.carry = Point::default();
    }
}

#[derive(Clone, Copy)]
struct AutoScroll {
    anchor: Point<Pixels>,
    /// 按住中键拖动过；这时松开中键就结束。
    dragged: bool,
}

impl SmoothScroll {
    /// 放在窗口根元素的最前（第一个子元素）：注册监听器，自动滚动时画锚点、换光标。
    /// 锚点用 `deferred` 画在最上层，所以放在前面也不会被内容盖住。
    pub(crate) fn layer(&self, cx: &App) -> impl IntoElement {
        let anchor = self.0.borrow().auto.map(|auto| auto.anchor);
        let state = self.0.clone();
        div()
            .absolute()
            .inset_0()
            .child(canvas(|_, _, _| {}, move |_, _, window, _| register(state, window)).size_full())
            .children(anchor.map(|anchor| indicator(anchor, cx)))
    }
}

/// 自动滚动的锚点：一个小圆，上下两个箭头。
fn indicator(anchor: Point<Pixels>, cx: &App) -> impl IntoElement {
    let p = palette(cx);
    let half = px(INDICATOR_SIZE / 2.);
    deferred(
        anchored().position(point(anchor.x - half, anchor.y - half)).child(
            v_flex()
                .size(px(INDICATOR_SIZE))
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(p.raised.opacity(0.92))
                .border_1()
                .border_color(p.border)
                .shadow_md()
                .text_color(p.text_secondary)
                .child(Icon::new(IconName::ChevronUp).xsmall())
                .child(Icon::new(IconName::ChevronDown).xsmall()),
        ),
    )
    .with_priority(u32::MAX as usize)
}

fn register(state: Rc<RefCell<State>>, window: &mut Window) {
    if state.borrow().auto.is_some() {
        window.set_window_cursor_style(CursorStyle::ResizeUpDown);
    }
    let line_height = window.line_height();

    let s = state.clone();
    window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
        if phase != DispatchPhase::Capture || s.borrow().replaying {
            return;
        }
        // 自动滚动中拨动滚轮：结束自动滚动，这次滚动照常处理。
        if s.borrow_mut().auto.take().is_some() {
            window.refresh();
        }
        // 触屏滑动的增量本来就由手指决定、跟随 GPUI 的手势相位（含松手后的惯性），
        // 走平滑反而慢一拍：放行，让它交给各滚动容器（含 ScrollBounce 的回弹）处理。
        let passthrough = event.source == ScrollSource::Touch
            || event.delta.precise()
            || event.modifiers.control
            || event.modifiers.platform
            || cx.reduce_motion();
        let mut st = s.borrow_mut();
        if passthrough {
            // 精确滚动接管：丢掉还没滚完的部分，两者不要互相拉扯。
            st.stop_wheel();
            return;
        }
        let delta = event.delta.pixel_delta(line_height);
        st.wheel_x.push(delta.x.as_f32());
        st.wheel_y.push(delta.y.as_f32());
        st.wheel_position = event.position;
        st.wheel_modifiers = event.modifiers;
        cx.stop_propagation();
        drop(st);
        start_ticking(&s, window);
    });

    let s = state.clone();
    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
        if phase != DispatchPhase::Capture || s.borrow().replaying {
            return;
        }
        let mut st = s.borrow_mut();
        st.pressed = Some(event.button);
        if st.auto.take().is_some() {
            // 自动滚动中的任何点击只用来结束它，不落到下面的元素上。
            cx.stop_propagation();
            drop(st);
            window.refresh();
            return;
        }
        if event.button == MouseButton::Middle {
            st.auto = Some(AutoScroll { anchor: event.position, dragged: false });
            st.stop_wheel();
            cx.stop_propagation();
            drop(st);
            window.refresh();
            start_ticking(&s, window);
        }
    });

    let s = state.clone();
    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
        if phase != DispatchPhase::Capture || s.borrow().replaying {
            return;
        }
        let mut st = s.borrow_mut();
        st.pressed = None;
        if event.button != MouseButton::Middle {
            return;
        }
        match st.auto {
            Some(auto) => {
                cx.stop_propagation();
                if auto.dragged {
                    st.auto = None;
                    drop(st);
                    window.refresh();
                }
            }
            // 结束自动滚动的那次中键点击，松开时也不往下传。
            None => cx.stop_propagation(),
        }
    });

    let s = state.clone();
    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, _| {
        if phase != DispatchPhase::Capture || s.borrow().replaying {
            return;
        }
        let mut st = s.borrow_mut();
        if let Some(auto) = st.auto.as_mut()
            && event.pressed_button == Some(MouseButton::Middle)
            && distance(event.position, auto.anchor) > AUTO_DRAG_THRESHOLD
        {
            auto.dragged = true;
        }
    });

    let s = state;
    window.on_key_event(move |event: &KeyDownEvent, phase, window, cx| {
        if phase != DispatchPhase::Capture || s.borrow_mut().auto.take().is_none() {
            return;
        }
        if event.keystroke.key == "escape" {
            cx.stop_propagation();
        }
        window.refresh();
    });
}

fn distance(a: Point<Pixels>, b: Point<Pixels>) -> f32 {
    let dx = (a.x - b.x).as_f32();
    let dy = (a.y - b.y).as_f32();
    (dx * dx + dy * dy).sqrt()
}

fn start_ticking(state: &Rc<RefCell<State>>, window: &mut Window) {
    let mut st = state.borrow_mut();
    if st.ticking {
        return;
    }
    st.ticking = true;
    st.last_frame = None;
    let state = state.clone();
    window.on_next_frame(move |window, cx| tick(state, window, cx));
}

/// 每帧一次：滚掉一部分滚轮距离，加上自动滚动这一帧的距离，合成一个精确滚动事件派发出去。
fn tick(state: Rc<RefCell<State>>, window: &mut Window, cx: &mut App) {
    let now = Instant::now();
    let mut st = state.borrow_mut();
    let dt = st.last_frame.map_or(Duration::from_millis(16), |last| now - last).min(MAX_FRAME).as_secs_f32();
    st.last_frame = Some(now);

    if st.auto.is_some() && !window.is_window_active() {
        st.auto = None;
        window.refresh();
    }

    let mut position = st.wheel_position;
    let mut modifiers = st.wheel_modifiers;
    let mut raw = point(st.wheel_x.advance(dt), st.wheel_y.advance(dt));
    if let Some(auto) = st.auto {
        let offset = (window.mouse_position().y - auto.anchor.y).as_f32();
        let distance = (offset.abs() - AUTO_DEAD_ZONE).max(0.);
        // 速度（像素/秒）随距离先线性再加速：离 100 像素约 600，离 300 像素约 3000。
        let speed = distance * 4. + distance * distance * 0.02;
        // 指针在锚点下方时往下滚，对应负的滚动增量。
        raw.y -= speed * dt * offset.signum();
        position = auto.anchor;
        modifiers = Modifiers::default();
    }

    // 只按整物理像素滚：GPUI 画字时纵向对齐到整像素，而背景、边框按小数位置画，
    // 偏移带小数时文字一跳一跳、和底下的卡片错开抖动。凑不满一个像素的部分留到下一帧。
    let scale = window.scale_factor();
    let snap = |value: f32| (value * scale).round() / scale;
    let total = point(raw.x + st.carry.x, raw.y + st.carry.y);
    let delta = point(px(snap(total.x)), px(snap(total.y)));
    st.carry = point(total.x - delta.x.as_f32(), total.y - delta.y.as_f32());

    let keep_going = !st.wheel_x.is_idle() || !st.wheel_y.is_idle() || st.auto.is_some();
    if !keep_going {
        st.carry = Point::default();
    }
    st.ticking = keep_going;
    let pressed = st.pressed;
    let replay = delta != Point::default();
    st.replaying = replay;
    drop(st);

    if replay {
        let pointer = window.mouse_position();
        window.dispatch_event(
            PlatformInput::Scroll(ScrollEvent {
                position,
                delta: ScrollDelta::Pixels(delta),
                modifiers,
                touch_phase: TouchPhase::Moved,
                source: ScrollSource::Pointer,
            }),
            cx,
        );
        if pointer != position {
            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position: pointer, pressed_button: pressed, modifiers }), cx);
        }
        state.borrow_mut().replaying = false;
    }

    if keep_going {
        let next = state.clone();
        window.on_next_frame(move |window, cx| tick(next, window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::Glide;

    fn run(glide: &mut Glide, hz: f32) -> (f32, usize) {
        let mut moved = 0.;
        let mut frames = 0;
        while !glide.is_idle() {
            let step = glide.advance(1. / hz);
            assert!(step >= 0., "moved backwards at frame {frames}");
            moved += step;
            frames += 1;
            assert!(frames < hz as usize, "did not settle within a second");
        }
        (moved, frames)
    }

    #[test]
    fn glide_covers_the_distance_without_overshoot() {
        for hz in [60., 144.] {
            let mut glide = Glide::default();
            glide.push(60.);
            let (moved, frames) = run(&mut glide, hz);
            assert!((moved - 60.).abs() < 1e-3, "moved {moved}");
            // 0.25 秒之内滚完，不拖尾巴。
            assert!(frames as f32 / hz < 0.25, "took {frames} frames at {hz}Hz");
        }
    }

    #[test]
    fn glide_starts_moving_on_the_first_frame() {
        let mut glide = Glide::default();
        glide.push(60.);
        // 起步不发粘：第一帧就走掉一大截。
        assert!(glide.advance(1. / 60.) > 10.);
        let mut glide = Glide::default();
        glide.push(60.);
        assert!(glide.advance(1. / 144.) > 3.);
    }

    #[test]
    fn glide_speeds_up_when_pushed_again_and_resets_on_reversal() {
        let mut glide = Glide::default();
        glide.push(60.);
        let first = glide.advance(1. / 60.);
        glide.push(60.);
        assert!(glide.advance(1. / 60.) > first);
        glide.push(-60.);
        assert_eq!(glide.remaining, -60.);
        let (moved, _) = run(&mut Glide { remaining: 60. }, 60.);
        assert!((moved - 60.).abs() < 1e-3);
    }
}
