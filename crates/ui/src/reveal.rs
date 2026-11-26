//! 键盘焦点移到看不见的地方时，把它滚进视口。
//!
//! GPUI 不会自动这样做：设置页很长时按 Tab，焦点走到了视口下面，页面却不动，看起来像焦点丢了。
//! 它也不提供“当前焦点元素的位置”。这里的做法：把设置页的每一行、每一块包一层 [`reveal`]。
//! 这一层在派发树上挂一个只用来判断“焦点在不在我里面”的焦点句柄（不是 Tab 停靠点，也不响应鼠标），
//! 焦点进到它里面、而它在视口外时，调整滚动容器的偏移。
//!
//! 嵌套时最里层先处理（它先完成 prepaint），外层看到这次焦点已经处理过就不再动。

use std::cell::RefCell;

use gpui_kit::*;

/// [`reveal`] 默认跟随的滚动容器（设置页的正文）。
#[derive(Default)]
struct RevealScope {
    scroll: Option<ScrollHandle>,
}

impl Global for RevealScope {}

thread_local! {
    /// 最近一次已经处理过的焦点（所有滚动容器共用：焦点同一时刻只在一处，处理过一次就不再动）。
    static REVEALED: RefCell<Option<FocusHandle>> = const { RefCell::new(None) };
}

/// 设置 [`reveal`] 跟随哪个滚动容器。由持有容器的视图在渲染时调用。
pub fn follow_focus_in(scroll: &ScrollHandle, cx: &mut App) {
    cx.set_global(RevealScope { scroll: Some(scroll.clone()) });
}

/// 焦点进到 `child` 里时，保证它在 [`follow_focus_in`] 设的滚动容器里看得见。`id` 在同一层里要唯一。
pub fn reveal(id: impl Into<ElementId>, child: impl IntoElement) -> Reveal {
    Reveal { id: id.into(), scroll: None, child: child.into_any_element() }
}

/// 同 [`reveal`]，但直接指定滚动容器（容器要 `track_scroll` 同一个句柄）。
/// 同时有几个滚动容器都要跟随焦点时用这个（侧边栏的会话列表、插件页面、弹窗的正文……）。
pub fn reveal_in(scroll: &ScrollHandle, id: impl Into<ElementId>, child: impl IntoElement) -> Reveal {
    Reveal { id: id.into(), scroll: Some(scroll.clone()), child: child.into_any_element() }
}

pub struct Reveal {
    id: ElementId,
    scroll: Option<ScrollHandle>,
    child: AnyElement,
}

impl IntoElement for Reveal {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Reveal {
    type RequestLayoutState = ();
    type PrepaintState = ();

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
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let handle = window.with_element_state(global_id.expect("Reveal 总有 id"), |handle: Option<FocusHandle>, _| {
            let handle = handle.unwrap_or_else(|| cx.focus_handle());
            (handle.clone(), handle)
        });
        // 只把句柄挂在派发树上，用来判断焦点是否在这一块里；不加鼠标、Tab 的行为。
        window.set_focus_handle(&handle, cx);
        self.child.prepaint(window, cx);
        let scroll = self.scroll.clone().or_else(|| cx.try_global::<RevealScope>().and_then(|scope| scope.scroll.clone()));
        if let Some(scroll) = scroll {
            reveal_if_focused(&handle, &scroll, bounds, window, cx);
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

/// 离视口边缘留的距离，焦点框不贴边。
const MARGIN: f32 = 24.;

fn reveal_if_focused(handle: &FocusHandle, scroll: &ScrollHandle, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    if !handle.contains_focused(window, cx) {
        return;
    }
    let Some(focused) = window.focused(cx) else {
        return;
    };
    let already = REVEALED.with(|last| {
        let mut last = last.borrow_mut();
        if last.as_ref() == Some(&focused) {
            return true;
        }
        *last = Some(focused);
        false
    });
    if already {
        return;
    }
    let view = scroll.bounds();
    if let Some(y) = target_offset(scroll.offset().y, scroll.max_offset().y, view, bounds) {
        scroll.set_offset(point(scroll.offset().x, y));
        // 这里还在画这一帧：滚动容器已经按旧的偏移排好了子元素，`refresh` 在绘制期间也不起作用。
        // 画完这一帧再要一帧，新的偏移才显示出来；以前要等下一次按键才重画，看起来总是慢一拍。
        window.defer(cx, |window, _| window.refresh());
    }
}

/// 让 `item` 进入视口 `view` 需要的新纵向偏移（GPUI 的偏移向下滚为负）；已经看得见时为 `None`。
fn target_offset(offset: Pixels, max_offset: Pixels, view: Bounds<Pixels>, item: Bounds<Pixels>) -> Option<Pixels> {
    if view.size.height <= px(0.) {
        return None;
    }
    let margin = px(MARGIN);
    let top = view.top() + margin;
    let bottom = view.bottom() - margin;
    let delta = if item.size.height > bottom - top {
        // 比视口还高：顶端看不见时把顶端对齐（里面更小的块会再各自处理）。
        if item.top() < top || item.top() > bottom { top - item.top() } else { px(0.) }
    } else if item.top() < top {
        top - item.top()
    } else if item.bottom() > bottom {
        bottom - item.bottom()
    } else {
        px(0.)
    };
    if delta == px(0.) {
        return None;
    }
    let y = (offset + delta).clamp(-max_offset, px(0.));
    (y != offset).then_some(y)
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Context, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _, Render,
        ScrollHandle, StatefulInteractiveElement as _, Styled as _, Window, div, point, px, size,
    };

    use super::{follow_focus_in, reveal, reveal_in, target_offset};

    /// 300px 高的滚动区里 20 个 60px 高、可 Tab 到的行。
    struct Page {
        scroll: ScrollHandle,
        items: Vec<FocusHandle>,
    }

    impl Render for Page {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            follow_focus_in(&self.scroll, cx);
            div().size_full().child(
                div().id("body").track_scroll(&self.scroll).overflow_y_scroll().h(px(300.)).w(px(200.)).flex().flex_col().children(
                    self.items.iter().enumerate().map(|(index, handle)| {
                        reveal(("row", index), div().id(("item", index)).track_focus(handle).flex_none().h(px(60.)).w_full())
                    }),
                ),
            )
        }
    }

    fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
        cx.update_window(window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    #[gpui::test]
    fn focus_below_the_viewport_scrolls_it_into_view(cx: &mut TestAppContext) {
        let (window, page) = cx.update(|cx| {
            gpui_kit::init(cx);
            let (window, page) = gpui_kit::open_window(Default::default(), cx, |_, cx| {
                cx.new(|cx| Page { scroll: ScrollHandle::new(), items: (0..20).map(|_| cx.focus_handle().tab_stop(true)).collect() })
            })
            .unwrap();
            (window, page)
        });
        draw(cx, window);
        let scroll = cx.update(|cx| page.read(cx).scroll.clone());
        assert_eq!(scroll.offset().y, px(0.));

        // 第 10 行（540..600）在视口（0..300）下面。
        let target = cx.update(|cx| page.read(cx).items[9].clone());
        cx.update_window(window, |_, window, cx| target.focus(window, cx)).unwrap();
        draw(cx, window);
        draw(cx, window);
        let offset = scroll.offset().y;
        assert!(offset < px(0.), "应该向下滚了：{offset:?}");
        // 行底 600 + 偏移 应该落在视口里（留 24px 边距）。
        assert!(px(600.) + offset <= px(300.) - px(24.) + px(0.5), "行在视口里：{offset:?}");

        // 再回到第 1 行，滚回顶部。
        let first = cx.update(|cx| page.read(cx).items[0].clone());
        cx.update_window(window, |_, window, cx| first.focus(window, cx)).unwrap();
        draw(cx, window);
        draw(cx, window);
        assert_eq!(scroll.offset().y, px(0.));
    }

    /// 两个并排的滚动区（像侧边栏和弹窗的正文），各自用 [`reveal_in`] 跟随焦点。
    struct TwoLists {
        left: ScrollHandle,
        right: ScrollHandle,
        items: Vec<FocusHandle>,
    }

    impl Render for TwoLists {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let list = |name: &'static str, scroll: &ScrollHandle, items: &[FocusHandle]| {
                div().id(name).track_scroll(scroll).overflow_y_scroll().h(px(300.)).w(px(200.)).flex().flex_col().children(
                    items.iter().enumerate().map(|(index, handle)| {
                        reveal_in(scroll, (name, index), div().id(("item", index)).track_focus(handle).flex_none().h(px(60.)).w_full())
                    }),
                )
            };
            div().size_full().flex().child(list("left", &self.left, &self.items[..20])).child(list("right", &self.right, &self.items[20..]))
        }
    }

    #[gpui::test]
    fn each_list_follows_focus_with_its_own_scroll_handle(cx: &mut TestAppContext) {
        let (window, page) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(Default::default(), cx, |_, cx| {
                cx.new(|cx| TwoLists {
                    left: ScrollHandle::new(),
                    right: ScrollHandle::new(),
                    items: (0..40).map(|_| cx.focus_handle().tab_stop(true)).collect(),
                })
            })
            .unwrap()
        });
        draw(cx, window);
        let (left, right) = cx.update(|cx| (page.read(cx).left.clone(), page.read(cx).right.clone()));

        // 右边一列的第 15 行：只有右边滚，左边不动。
        let target = cx.update(|cx| page.read(cx).items[34].clone());
        cx.update_window(window, |_, window, cx| target.focus(window, cx)).unwrap();
        draw(cx, window);
        draw(cx, window);
        assert!(right.offset().y < px(0.), "右边应该滚了");
        assert_eq!(left.offset().y, px(0.), "左边不动");

        // 再到左边一列的最后一行。
        let target = cx.update(|cx| page.read(cx).items[19].clone());
        cx.update_window(window, |_, window, cx| target.focus(window, cx)).unwrap();
        draw(cx, window);
        draw(cx, window);
        assert!(left.offset().y < px(0.), "左边应该滚了");
    }

    fn rect(y: f32, height: f32) -> Bounds<gpui_kit::Pixels> {
        Bounds::new(point(px(0.), px(y)), size(px(100.), px(height)))
    }

    #[test]
    fn scrolls_down_to_a_row_below_the_viewport() {
        // 视口 0..500，已经滚了 100；行在 600..640。
        let y = target_offset(px(-100.), px(2000.), rect(0., 500.), rect(600., 40.)).unwrap();
        assert_eq!(y, px(-100.) - px(164.));
    }

    #[test]
    fn scrolls_up_to_a_row_above_the_viewport() {
        let y = target_offset(px(-400.), px(2000.), rect(0., 500.), rect(-50., 40.)).unwrap();
        assert_eq!(y, px(-400.) + px(74.));
    }

    #[test]
    fn leaves_visible_rows_alone_and_clamps() {
        assert_eq!(target_offset(px(-100.), px(2000.), rect(0., 500.), rect(200., 40.)), None);
        // 不会滚过顶。
        assert_eq!(target_offset(px(-10.), px(2000.), rect(0., 500.), rect(-30., 40.)), Some(px(0.)));
    }
}
