//! 给读屏软件的播报（实时区域）。
//!
//! 有些变化不伴随焦点移动：发送消息后开始回复、回复结束、需要审批、保存成功或出错。
//! 读屏软件只会读焦点所在的控件，这些变化就听不到。这里在窗口里放一个不可见的“实时区域”节点：
//! Windows 上读屏软件收到 UIA 的 LiveRegionChanged（读节点名称），macOS 上 VoiceOver 收到
//! 播报通知（读节点的值），所以名称和值都设成要播报的文字。
//!
//! 每次播报换一个节点 id：同一句话连着出现两次（比如两次“已保存”）也会被当作新内容读出来。

use accesskit::Live;
use gpui_kit::*;

#[derive(Default)]
struct Announcement {
    seq: u64,
    text: SharedString,
}

impl Global for Announcement {}

/// 播报一句话。不打断读屏软件正在读的内容（礼貌级别）。
pub fn announce(text: impl Into<SharedString>, cx: &mut App) {
    let text = text.into();
    if text.is_empty() {
        return;
    }
    let seq = cx.try_global::<Announcement>().map_or(0, |current| current.seq) + 1;
    cx.set_global(Announcement { seq, text });
    cx.refresh_windows();
}

/// 过 `delay` 再播报。用在同时有焦点变化的时候：讲述人读焦点会打断正在读的实时区域。
pub fn announce_after(text: impl Into<SharedString>, delay: std::time::Duration, cx: &mut App) {
    let text = text.into();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(delay).await;
        cx.update(|cx| announce(text, cx));
    })
    .detach();
}

/// 实时区域节点。放在窗口根元素里（每个窗口一个）；看不见，不占布局。
pub fn live_region(cx: &App) -> impl IntoElement {
    let (seq, text) = cx.try_global::<Announcement>().map_or((0, SharedString::default()), |current| (current.seq, current.text.clone()));
    div().id(("live-region", seq as usize)).absolute().top_0().left_0().size(px(1.)).overflow_hidden().opacity(0.).when(
        !text.is_empty(),
        |region| {
            region
                .role(Role::Label)
                .aria_label(text.clone())
                .aria_value(text)
                .a11y_synthetic_children(|builder| builder.parent_node().set_live(Live::Polite))
        },
    )
}

use gpui_kit::prelude::FluentBuilder as _;
