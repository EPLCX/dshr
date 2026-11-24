//! “搜索会话”弹窗：按标题找会话。顶上是搜索框，下面是结果（没输入时是最近的会话），底部是按键提示。
//! ↑ ↓ 选择，Enter 打开，Esc 关闭；鼠标悬停的那一项成为高亮项，点一下打开。
//! 搜索框的角色是 `SearchInput`（UIA 里是编辑框）：讲述人聚焦到它时自动切到焦点模式，打的字进输入框。

use std::ops::Range;
use std::time::{Duration, SystemTime};

use dshr_agent::SessionSummary;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::design::{Typography as _, motion, palette, radius, shadow, size, space};

/// 没输入时列出几个最近的会话。
const RECENT: usize = 20;
/// 结果最多显示几个。
const MAX_RESULTS: usize = 100;
/// 弹窗宽度。
pub const DIALOG_WIDTH: f32 = 640.;

pub enum SessionSearchEvent {
    /// 打开这个会话（id）。
    Open(String),
    Close,
}

pub struct SessionSearchView {
    sessions: Vec<SessionSummary>,
    input: Entity<InputState>,
    /// 当前结果：`sessions` 里的下标，按显示顺序。
    matches: Vec<usize>,
    highlighted: usize,
    focus: FocusHandle,
    /// 刚打开、焦点还没进搜索框。等弹窗第一次画出来再给焦点：在那之前无障碍树里还没有搜索框，
    /// 讲述人收不到这次焦点变化，停在浏览模式里，打的字被当成快速导航键（“没有下一个标题级别”），输入不进去。
    focus_pending: bool,
    scroll: ScrollHandle,
    phase: motion::Phase,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SessionSearchEvent> for SessionSearchView {}

impl SessionSearchView {
    pub fn new(sessions: Vec<SessionSummary>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("search.placeholder").to_string()));
        let subscriptions = vec![cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| match event {
            InputEvent::Change => this.refilter(cx),
            InputEvent::PressEnter { .. } => this.open_highlighted(cx),
            _ => {}
        })];
        let matches = filter(&sessions, "", RECENT);
        Self {
            sessions,
            input,
            matches,
            highlighted: 0,
            focus: cx.focus_handle(),
            focus_pending: true,
            scroll: ScrollHandle::new(),
            phase: motion::Phase::SETTLED,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_phase(&mut self, phase: motion::Phase) {
        self.phase = phase;
    }

    /// 焦点放进搜索框。
    fn focus_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_pending = false;
        gpui_kit::Focusable::focus_handle(self.input.read(cx), cx).focus(window, cx);
    }

    fn query(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.query(cx);
        self.matches = filter(&self.sessions, &query, RECENT);
        self.highlighted = 0;
        self.scroll.scroll_to_item(0);
        // 读屏软件听到有几个结果（输入时焦点一直在搜索框里）。
        if !query.trim().is_empty() {
            crate::announce::announce(t!("search.count", count = self.matches.len()).to_string(), cx);
        }
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() as isize - 1;
        self.highlighted = (self.highlighted as isize + delta).clamp(0, last) as usize;
        // 第 0 个子元素是分组标题。
        self.scroll.scroll_to_item(self.highlighted + 1);
        cx.notify();
    }

    fn open_highlighted(&mut self, cx: &mut Context<Self>) {
        if let Some(&index) = self.matches.get(self.highlighted) {
            cx.emit(SessionSearchEvent::Open(self.sessions[index].id.clone()));
        }
    }

    fn render_row(&self, position: usize, index: usize, query: &str, p: &crate::design::Palette, cx: &mut Context<Self>) -> AnyElement {
        let session = &self.sessions[index];
        let highlighted = position == self.highlighted;
        let mut subtitle = session.workspace.as_deref().map(crate::folder_name).unwrap_or_else(|| t!("sidebar.other_sessions").to_string());
        if let Some(ago) = session.modified.and_then(|modified| SystemTime::now().duration_since(modified).ok()).map(time_ago) {
            subtitle.push_str(" · ");
            subtitle.push_str(&ago);
        }
        let highlights: Vec<(Range<usize>, HighlightStyle)> = match_ranges(&session.title, query)
            .into_iter()
            .map(|range| (range, HighlightStyle { color: Some(p.accent), font_weight: Some(FontWeight::SEMIBOLD), ..Default::default() }))
            .collect();
        let title = StyledText::new(session.title.clone()).with_highlights(highlights);
        h_flex()
            .id(("session-search-row", position))
            .role(Role::ListBoxOption)
            .aria_selected(highlighted)
            .aria_label(format!("{}, {subtitle}", session.title))
            .aria_position_in_set(position)
            .aria_size_of_set(self.matches.len())
            .flex_none()
            .items_center()
            .gap(px(space::M))
            .px(px(space::L))
            .py(px(space::S))
            .rounded(px(radius::LG))
            .cursor_pointer()
            .when(highlighted, |row| row.bg(p.selected))
            .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, window, cx| {
                if this.highlighted != position && !window.last_input_was_touch() {
                    this.highlighted = position;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.highlighted = position;
                this.open_highlighted(cx);
            }))
            .child(Icon::new(IconName::ArrowRight).size(px(size::ICON_MD)).flex_none().text_color(if highlighted {
                p.accent
            } else {
                p.text_secondary
            }))
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .child(div().text_body().truncate().child(title))
                    .child(div().text_caption().text_color(p.text_secondary).truncate().child(subtitle)),
            )
            .into_any_element()
    }
}

/// 按键提示里的一个键帽。
fn key_cap(text: &'static str, p: &crate::design::Palette) -> Div {
    div()
        .flex_none()
        .min_w(px(22.))
        .h(px(22.))
        .px(px(space::XS + 2.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::SM))
        .border_1()
        .border_color(p.border)
        .bg(p.surface)
        .text_caption()
        .text_color(p.text)
        .child(text)
}

impl Render for SessionSearchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let query = self.query(cx);
        let searching = !query.trim().is_empty();
        let header = if searching { t!("search.results", count = self.matches.len()).to_string() } else { t!("search.recent").to_string() };
        let rows: Vec<AnyElement> = self
            .matches
            .clone()
            .into_iter()
            .enumerate()
            .map(|(position, index)| self.render_row(position, index, &query, &p, cx))
            .collect();
        let empty = rows.is_empty().then(|| {
            let text = if searching { t!("search.empty") } else { t!("search.no_sessions") };
            div().px(px(space::L)).py(px(space::XL)).text_body().text_color(p.text_secondary).child(text.to_string())
        });
        let hint = |keys: Vec<&'static str>, label: String| {
            h_flex()
                .gap(px(space::XS))
                .items_center()
                .children(keys.into_iter().map(|key| key_cap(key, &p)))
                .child(div().pl(px(space::XS)).text_caption().text_color(p.text_secondary).child(label))
        };
        v_flex()
            .id("session-search")
            .role(Role::Dialog)
            .aria_label(t!("sidebar.search").to_string())
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| match event.keystroke.key.as_str() {
                "up" => {
                    cx.stop_propagation();
                    this.step(-1, cx);
                }
                "down" => {
                    cx.stop_propagation();
                    this.step(1, cx);
                }
                "escape" => {
                    cx.stop_propagation();
                    cx.emit(SessionSearchEvent::Close);
                }
                _ => {}
            }))
            .occlude()
            .w(px(DIALOG_WIDTH))
            .max_w(relative(0.94))
            .max_h(relative(0.8))
            .rounded(px(radius::XXL))
            .bg(p.raised)
            .border_1()
            .border_color(p.border)
            .text_color(p.text)
            .overflow_hidden()
            .map(|dialog| motion::pop(dialog, motion::Edge::Top, motion::MEDIUM, self.phase, shadow::modal()))
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(px(space::M))
                    .px(px(space::XL))
                    .py(px(space::L))
                    .child(Icon::new(IconName::Search).size(px(size::ICON_LG)).text_color(p.text_secondary))
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            Input::new(&self.input)
                                .appearance(false)
                                .large()
                                .role(Role::SearchInput)
                                .aria_label(t!("sidebar.search").to_string()),
                        ),
                    )
                    .child(crate::design::framed(
                        "close-session-search",
                        Button::new("close-session-search")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .accessibility_label(t!("search.hint_close").to_string())
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SessionSearchEvent::Close))),
                    )),
            )
            .child(div().flex_none().h(px(1.)).bg(p.border))
            .child(
                v_flex()
                    .id("session-search-results")
                    .role(Role::ListBox)
                    .aria_label(header.clone())
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .p(px(space::M))
                    .gap(px(2.))
                    .child(
                        div()
                            .flex_none()
                            .px(px(space::L))
                            .pt(px(space::XS))
                            .pb(px(space::S))
                            .text_caption()
                            .text_color(p.text_secondary)
                            .child(header),
                    )
                    .children(rows)
                    .children(empty),
            )
            .child(div().flex_none().h(px(1.)).bg(p.border))
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(px(space::XL))
                    .px(px(space::XL))
                    .py(px(space::M))
                    // 外层的裁切是方的：底色自己带上圆角，不从弹窗的圆角里露出来。
                    .rounded_b(px(radius::XXL))
                    .bg(p.sidebar)
                    .child(hint(vec!["↑", "↓"], t!("search.hint_select").to_string()))
                    .child(hint(vec!["Enter"], t!("search.hint_open").to_string()))
                    .child(hint(vec!["Esc"], t!("search.hint_close").to_string())),
            )
            // Tab 只在弹窗里循环。
            .focus_trap("session-search-trap", &self.focus)
            .when(self.focus_pending, |dialog| {
                let view = cx.entity().downgrade();
                dialog.child(crate::design::focus_after_paint(move |window, cx| {
                    view.update(cx, |this, cx| this.focus_input(window, cx)).ok();
                }))
            })
    }
}

/// 按标题筛选：不分大小写，用空白分开的每个词都要出现在标题里。没输入时是最近的 `recent` 个（列表本来就是最近的在前）。
pub(crate) fn filter(sessions: &[SessionSummary], query: &str, recent: usize) -> Vec<usize> {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if terms.is_empty() {
        return (0..sessions.len().min(recent)).collect();
    }
    sessions
        .iter()
        .enumerate()
        .filter(|(_, session)| {
            let title = session.title.to_lowercase();
            terms.iter().all(|term| title.contains(term.as_str()))
        })
        .map(|(index, _)| index)
        .take(MAX_RESULTS)
        .collect()
}

/// 标题里命中搜索词的地方（字节范围，按位置排好、重叠的合并）。转小写后长度变了的标题不标。
pub(crate) fn match_ranges(title: &str, query: &str) -> Vec<Range<usize>> {
    let lower = title.to_lowercase();
    if lower.len() != title.len() {
        return Vec::new();
    }
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for term in query.split_whitespace().map(str::to_lowercase) {
        let mut from = 0;
        while let Some(found) = lower[from..].find(term.as_str()) {
            let start = from + found;
            let end = start + term.len();
            // 只在字符边界上标（小写和原文长度一样时边界也一样）。
            if title.is_char_boundary(start) && title.is_char_boundary(end) {
                ranges.push(start..end);
            }
            from = end;
        }
    }
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// “3 分钟前”这样的相对时间。
fn time_ago(elapsed: Duration) -> String {
    let minutes = elapsed.as_secs() / 60;
    if minutes < 1 {
        t!("search.just_now").to_string()
    } else if minutes < 60 {
        t!("search.minutes_ago", count = minutes).to_string()
    } else if minutes < 60 * 24 {
        t!("search.hours_ago", count = minutes / 60).to_string()
    } else {
        t!("search.days_ago", count = minutes / (60 * 24)).to_string()
    }
}

#[cfg(test)]
mod tests {
    // 不用 `super::*`：那样会把 GPUI 的 `test` 宏一起引进来，盖过标准的 `#[test]`。
    use super::{Range, SessionSummary, filter, match_ranges};

    fn sessions(titles: &[&str]) -> Vec<SessionSummary> {
        titles
            .iter()
            .enumerate()
            .map(|(index, title)| SessionSummary {
                id: format!("s{index}"),
                title: (*title).to_owned(),
                modified: None,
                log: Default::default(),
                workspace: None,
            })
            .collect()
    }

    #[test]
    fn empty_query_lists_the_most_recent() {
        let all = sessions(&["a", "b", "c"]);
        assert_eq!(filter(&all, "  ", 2), [0, 1]);
        assert_eq!(filter(&all, "", 10), [0, 1, 2]);
    }

    #[test]
    fn every_word_must_match_ignoring_case() {
        let all = sessions(&["Fix login bug", "修复登录问题", "Login page design", "bug triage"]);
        assert_eq!(filter(&all, "LOGIN", 20), [0, 2]);
        assert_eq!(filter(&all, "bug login", 20), [0]);
        assert_eq!(filter(&all, "登录", 20), [1]);
        assert!(filter(&all, "nothing", 20).is_empty());
    }

    #[test]
    fn highlights_merge_and_respect_char_boundaries() {
        assert_eq!(match_ranges("Login page login", "LOG"), [0..3, 11..14]);
        assert_eq!(match_ranges("abcdef", "abc cd"), vec![Range { start: 0, end: 4 }]);
        assert_eq!(match_ranges("修复登录问题", "登录"), vec![Range { start: 6, end: 12 }]);
        assert!(match_ranges("x", "").is_empty());
    }
}
