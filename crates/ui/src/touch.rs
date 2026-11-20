//! 触摸屏：把 Windows 的触摸输入交给 GPUI 的手势识别。
//!
//! GPUI 自带一套手势识别：轻点 → 点击，拖动 → 带惯性的滚动，长按 → 次要点击（右键菜单）或工具提示。
//! 它吃的是 `PlatformInput::Touch`，可是 GPUI 的 Windows 后端（gpui-pre 0.3）只把触控板交给
//! DirectManipulation，触摸屏的输入没人处理，落到系统默认行为：轻点、长按被模拟成鼠标左右键，
//! 手指拖动被当成按住左键拖动——在对话区里拖是选中文字而不是滚动，侧边栏、菜单也拖不动。
//!
//! 这里给窗口挂一个子类过程，先于 GPUI 收到 `WM_POINTER*`：触摸（`PT_TOUCH`）的按下、移动、抬起
//! 换算成窗口坐标，经通道交给界面线程上的任务，再作为 `TouchEvent` 派发给窗口；这些消息就此处理掉，
//! 系统不再把它们模拟成鼠标。笔和鼠标不动。按下时落在非客户区（标题栏的拖动区、窗口按钮、边框）的触摸
//! 也不动：照常能用手指拖动窗口、点窗口按钮。
//!
//! 哪条消息变成什么事件由 [`TouchTracker`] 决定（与平台无关，有测试）；Windows 上的部分只负责取出指针信息。
//! 其它平台什么都不做：macOS 没有触摸屏，GPUI 的其它后端自己处理触摸。

#[cfg(any(windows, test))]
use std::collections::HashMap;

use gpui_kit::{App, Window};
#[cfg(any(windows, test))]
use gpui_kit::{Pixels, Point, TouchEvent, TouchId, TouchPhase};

/// 给窗口接上触摸屏。在打开窗口的回调里调用一次。
pub fn install(window: &mut Window, cx: &mut App) {
    #[cfg(windows)]
    windows::install(window, cx);
    #[cfg(not(windows))]
    let _ = (window, cx);
}

/// 一条指针消息。
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PointerMessage {
    Down,
    Update,
    Up,
    /// 指针被别的窗口拿走了（`WM_POINTERCAPTURECHANGED`）。
    CaptureLost,
}

/// 从一条指针消息里取出来的信息。
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct PointerSample {
    pub id: u32,
    /// 是手指（不是笔、不是鼠标）。
    pub touch: bool,
    /// 手指贴在屏幕上。
    pub in_contact: bool,
    /// 系统取消了这次触摸（比如被识别成了系统手势）。
    pub canceled: bool,
    /// 按下的位置在客户区里（不是标题栏的拖动区、窗口按钮或边框）。只对 `Down` 有意义。
    pub in_client_area: bool,
    /// 窗口坐标（逻辑像素）。
    pub position: Point<Pixels>,
}

/// 记着哪些触摸交给了 GPUI：只有按下时落在客户区的那些，之后的移动、抬起才接着交；
/// 其余的（笔、鼠标、落在标题栏上的触摸）原样交还系统。
#[cfg(any(windows, test))]
#[derive(Default)]
pub(crate) struct TouchTracker {
    /// 交给了 GPUI、还没结束的触摸，和它最后的位置（取消时用）。
    active: HashMap<u32, Point<Pixels>>,
}

#[cfg(any(windows, test))]
impl TouchTracker {
    /// 这条消息要变成的触摸事件；`None` 表示不归这里管，交给系统默认处理。
    pub fn handle(&mut self, message: PointerMessage, sample: PointerSample) -> Option<TouchEvent> {
        let event =
            |phase, position| TouchEvent { id: TouchId(u64::from(sample.id)), phase, position, predicted_position: None, force: None };
        match message {
            PointerMessage::Down => {
                if !sample.touch || !sample.in_client_area {
                    return None;
                }
                self.active.insert(sample.id, sample.position);
                Some(event(TouchPhase::Started, sample.position))
            }
            PointerMessage::Update => {
                let last = self.active.get_mut(&sample.id)?;
                if !sample.in_contact {
                    return None;
                }
                *last = sample.position;
                Some(event(TouchPhase::Moved, sample.position))
            }
            PointerMessage::Up => {
                self.active.remove(&sample.id)?;
                let phase = if sample.canceled { TouchPhase::Cancelled } else { TouchPhase::Ended };
                Some(event(phase, sample.position))
            }
            PointerMessage::CaptureLost => {
                let last = self.active.remove(&sample.id)?;
                Some(event(TouchPhase::Cancelled, last))
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use futures::StreamExt as _;
    use futures::channel::mpsc;
    use gpui_kit::{App, PlatformInput, TouchEvent, Window, point, px};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::Input::Pointer::{GetPointerInfo, POINTER_FLAG_CANCELED, POINTER_FLAG_INCONTACT, POINTER_INFO};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HTCLIENT, PT_TOUCH, SendMessageW, WM_NCDESTROY, WM_NCHITTEST, WM_POINTERCAPTURECHANGED, WM_POINTERDOWN, WM_POINTERUP,
        WM_POINTERUPDATE,
    };

    use super::{PointerMessage, PointerSample, TouchTracker};

    /// 子类过程的编号（同一个窗口上别的子类用别的编号）。
    const SUBCLASS_ID: usize = 0x6473_6872;

    /// 交给子类过程的状态：窗口销毁（`WM_NCDESTROY`）时由它收回。
    struct Bridge {
        tracker: TouchTracker,
        events: mpsc::UnboundedSender<TouchEvent>,
    }

    pub fn install(window: &mut Window, cx: &mut App) {
        let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
        let hwnd = handle.hwnd.get() as HWND;
        let (events, mut received) = mpsc::unbounded();
        let bridge = Box::into_raw(Box::new(Bridge { tracker: TouchTracker::default(), events }));
        // SAFETY: hwnd 是 GPUI 这个窗口的句柄，在界面线程上；bridge 归子类过程所有，挂不上时在这里收回。
        if unsafe { SetWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID, bridge as usize) } == 0 {
            // SAFETY: 没挂上，没有别人拿着这个指针。
            drop(unsafe { Box::from_raw(bridge) });
            return;
        }
        // 子类过程在 GPUI 处理消息之外运行，不能直接碰窗口：经通道交给界面线程上的这个任务派发。
        window
            .spawn(cx, async move |cx| {
                while let Some(event) = received.next().await {
                    let dispatched = cx.update(|window, cx| {
                        window.dispatch_event(PlatformInput::Touch(event), cx);
                    });
                    if dispatched.is_err() {
                        break;
                    }
                }
            })
            .detach();
    }

    unsafe extern "system" fn subclass(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM, _: usize, data: usize) -> LRESULT {
        let kind = match message {
            WM_POINTERDOWN => Some(PointerMessage::Down),
            WM_POINTERUPDATE => Some(PointerMessage::Update),
            WM_POINTERUP => Some(PointerMessage::Up),
            WM_POINTERCAPTURECHANGED => Some(PointerMessage::CaptureLost),
            _ => None,
        };
        if let Some(kind) = kind {
            // SAFETY: data 是 install 交出来的 Bridge；子类过程只在窗口所在的界面线程上被调用，
            // 窗口销毁（WM_NCDESTROY）之前一直有效。
            let bridge = unsafe { &mut *(data as *mut Bridge) };
            // GET_POINTERID_WPARAM：低 16 位是指针编号。
            let id = (wparam & 0xFFFF) as u32;
            if let Some(sample) = unsafe { sample(hwnd, kind, id) }
                && let Some(event) = bridge.tracker.handle(kind, sample)
            {
                let _ = bridge.events.unbounded_send(event);
                // 处理掉：系统不再把这次触摸模拟成鼠标。
                return 0;
            }
        } else if message == WM_NCDESTROY {
            // SAFETY: 窗口在销毁，之后不会再有消息进到这个子类过程；摘掉它再收回 Bridge。
            unsafe {
                RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID);
                drop(Box::from_raw(data as *mut Bridge));
            }
        }
        // SAFETY: 原样交给下一个窗口过程。
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }

    /// 取出指针 `id` 的信息；取不到（指针已经没了）时，捕获丢失照样要处理，其余的交还系统。
    unsafe fn sample(hwnd: HWND, kind: PointerMessage, id: u32) -> Option<PointerSample> {
        // SAFETY: POINTER_INFO 是纯数据，全零是合法值；GetPointerInfo 只写这个结构。
        let mut info: POINTER_INFO = unsafe { std::mem::zeroed() };
        if unsafe { GetPointerInfo(id, &mut info) } == 0 {
            return (kind == PointerMessage::CaptureLost).then(|| PointerSample {
                id,
                touch: true,
                in_contact: false,
                canceled: true,
                in_client_area: false,
                position: point(px(0.), px(0.)),
            });
        }
        let screen = info.ptPixelLocation;
        let in_client_area = kind == PointerMessage::Down && {
            // 和 GPUI 自己的命中测试同一个答案：标题栏的拖动区、窗口按钮回答的不是 HTCLIENT。
            // MAKELPARAM(x, y)：屏幕坐标，各占 16 位（可以是负数）。
            let packed = ((screen.y as u16 as u32) << 16 | screen.x as u16 as u32) as i32 as LPARAM;
            let hit = unsafe { SendMessageW(hwnd, WM_NCHITTEST, 0, packed) };
            hit == HTCLIENT as LRESULT
        };
        let mut client = POINT { x: screen.x, y: screen.y };
        unsafe { ScreenToClient(hwnd, &mut client) };
        // 物理像素换成逻辑像素：和 GPUI 用的缩放比一样（DPI / 96）。
        let scale = match unsafe { GetDpiForWindow(hwnd) } {
            0 => 1.,
            dpi => dpi as f32 / 96.,
        };
        Some(PointerSample {
            id,
            touch: info.pointerType == PT_TOUCH,
            in_contact: info.pointerFlags & POINTER_FLAG_INCONTACT != 0,
            canceled: info.pointerFlags & POINTER_FLAG_CANCELED != 0,
            in_client_area,
            position: point(px(client.x as f32 / scale), px(client.y as f32 / scale)),
        })
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{TouchPhase, point, px};

    use super::{PointerMessage, PointerSample, TouchTracker};

    fn sample(id: u32, x: f32) -> PointerSample {
        PointerSample { id, touch: true, in_contact: true, canceled: false, in_client_area: true, position: point(px(x), px(10.)) }
    }

    #[test]
    fn a_touch_in_the_client_area_becomes_touch_events() {
        let mut tracker = TouchTracker::default();
        let started = tracker.handle(PointerMessage::Down, sample(1, 5.)).unwrap();
        assert_eq!(started.phase, TouchPhase::Started);
        assert_eq!(started.id.0, 1);
        assert_eq!(tracker.handle(PointerMessage::Update, sample(1, 8.)).unwrap().phase, TouchPhase::Moved);
        // 悬停（不贴着屏幕）的更新不算移动，交还系统。
        assert!(tracker.handle(PointerMessage::Update, PointerSample { in_contact: false, ..sample(1, 9.) }).is_none());
        let ended = tracker.handle(PointerMessage::Up, sample(1, 8.)).unwrap();
        assert_eq!(ended.phase, TouchPhase::Ended);
        // 结束之后的消息不再归这里。
        assert!(tracker.handle(PointerMessage::Update, sample(1, 9.)).is_none());
    }

    #[test]
    fn pens_mice_and_non_client_touches_stay_with_the_system() {
        let mut tracker = TouchTracker::default();
        assert!(tracker.handle(PointerMessage::Down, PointerSample { touch: false, ..sample(1, 0.) }).is_none());
        assert!(tracker.handle(PointerMessage::Update, sample(1, 1.)).is_none());
        // 按在标题栏上：拖动窗口归系统，之后的移动、抬起也都归系统。
        assert!(tracker.handle(PointerMessage::Down, PointerSample { in_client_area: false, ..sample(2, 0.) }).is_none());
        assert!(tracker.handle(PointerMessage::Update, sample(2, 1.)).is_none());
        assert!(tracker.handle(PointerMessage::Up, sample(2, 1.)).is_none());
    }

    #[test]
    fn cancelled_and_lost_touches_are_cancelled() {
        let mut tracker = TouchTracker::default();
        tracker.handle(PointerMessage::Down, sample(1, 0.));
        let up = tracker.handle(PointerMessage::Up, PointerSample { canceled: true, ..sample(1, 0.) }).unwrap();
        assert_eq!(up.phase, TouchPhase::Cancelled);
        // 被别的窗口拿走：在最后的位置取消。
        tracker.handle(PointerMessage::Down, sample(2, 3.));
        tracker.handle(PointerMessage::Update, sample(2, 7.));
        let lost = tracker.handle(PointerMessage::CaptureLost, PointerSample { in_contact: false, ..sample(2, 0.) }).unwrap();
        assert_eq!(lost.phase, TouchPhase::Cancelled);
        assert_eq!(lost.position, point(px(7.), px(10.)));
        // 两根手指互不影响。
        tracker.handle(PointerMessage::Down, sample(3, 0.));
        tracker.handle(PointerMessage::Down, sample(4, 0.));
        assert_eq!(tracker.handle(PointerMessage::Up, sample(3, 0.)).unwrap().phase, TouchPhase::Ended);
        assert_eq!(tracker.handle(PointerMessage::Update, sample(4, 2.)).unwrap().phase, TouchPhase::Moved);
    }
}
