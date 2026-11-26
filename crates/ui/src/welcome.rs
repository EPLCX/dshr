//! 欢迎窗（600×700）：首次启动、还没有可用凭据时显示，完成后才打开工作区窗口。

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Disableable as _, h_flex, v_flex};
use gpui_kit::*;
use rust_i18n::t;

use crate::backend::Backend;
use crate::design::{Tone, Typography as _, brand_mark, description, divider, heading, hint, motion, palette, size, space};
use crate::open_workspace_window;
use crate::smooth_scroll::SmoothScroll;

pub const WELCOME_SIZE: (f32, f32) = (600., 700.);

/// 欢迎窗里填写的 Key 属于默认提供商 DeepSeek。
const DEEPSEEK: &str = "deepseek";

pub struct WelcomeView {
    backend: Arc<Backend>,
    workspace: Option<PathBuf>,
    api_key: Entity<InputState>,
    error: Option<String>,
    /// 账号登录进行中的任务句柄；`Some` 时按钮置灰并显示等待提示。
    sign_in: Option<tokio::task::JoinHandle<Result<String, String>>>,
    scroll: SmoothScroll,
    /// 上半部分的滚动：Tab 走到看不见的地方时滚过去（见 `reveal::reveal_in`）。
    body_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl WelcomeView {
    pub fn new(backend: Arc<Backend>, workspace: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let api_key = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("sk-…"));
        let subscription = cx.subscribe_in(&api_key, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.save_and_continue(window, cx);
            }
        });
        // 打开即可输入，不用先点输入框。
        api_key.read(cx).focus_handle(cx).focus(window, cx);
        Self {
            backend,
            workspace,
            api_key,
            error: None,
            sign_in: None,
            scroll: SmoothScroll::default(),
            body_scroll: ScrollHandle::new(),
            _subscriptions: vec![subscription],
        }
    }

    /// 点「使用 DeepSeek 账号登录」：打开浏览器走 PKCE 授权，成功后令牌进钥匙串。
    fn sign_in_with_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sign_in.is_some() {
            return;
        }
        self.error = None;
        // 登录线程拿到授权地址后让界面打开系统浏览器。
        let (url_tx, url_rx) = futures::channel::oneshot::channel::<String>();
        let handle = self.backend.sign_in_deepseek(move |url| {
            let _ = url_tx.send(url);
        });
        self.sign_in = Some(handle);
        cx.notify();

        cx.spawn_in(window, async move |_, cx| {
            if let Ok(url) = url_rx.await {
                cx.update(|_window, cx| cx.open_url(&url)).ok();
            }
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            let Some(handle) = this.update(cx, |this, _| this.sign_in.take()).ok().flatten() else {
                return;
            };
            let result = handle.await.map_err(|error| error.to_string()).and_then(|inner| inner);
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_account) => {
                        this.backend.forget_deepseek_key();
                        // 默认模型改用 OAuth 提供商（配置里没有时补上）。
                        if let Err(error) = this.backend.prefer_oauth_provider() {
                            this.error = Some(error);
                            cx.notify();
                            return;
                        }
                        this.finish(window, cx);
                    }
                    Err(error) => {
                        this.error = Some(error);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn save_and_continue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.api_key.read(cx).value().to_string();
        match self.backend.save_api_key(DEEPSEEK, &key) {
            Ok(()) => self.finish(window, cx),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn set_up_later(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = self.backend.update_config(|config| config.ui.welcome_skipped = true) {
            self.error = Some(error);
            cx.notify();
            return;
        }
        self.finish(window, cx);
    }

    /// 先打开工作区窗口再关闭欢迎窗，避免中间没有窗口导致应用退出。
    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        open_workspace_window(self.backend.clone(), self.workspace.take(), cx);
        window.remove_window();
    }
}

impl Render for WelcomeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::design::update_touch_mode(window, cx);
        crate::design::settle_focus_source(window, cx);
        let p = palette(cx);
        // 上半部分可滚动，底部按钮固定：窗口内容区比设计尺寸矮时（如 macOS 标题栏占掉的高度、
        // 较大的系统字号）只会滚动上半部分，按钮不会被截断。
        let body = v_flex()
            .id("welcome-body")
            .track_scroll(&self.body_scroll)
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .px(px(space::XXXL + space::L))
            .pt(px(space::XXXL))
            .pb(px(space::L))
            .gap(px(space::XXXL))
            .child(
                v_flex()
                    .items_center()
                    .gap(px(space::M))
                    .pt(px(space::XL))
                    .child(brand_mark(size::ICON_HERO, &p))
                    .child(heading(t!("welcome.title").to_string(), 1).text_display())
                    .child(description(t!("welcome.subtitle").to_string(), &p)),
            )
            .child(crate::reveal::reveal_in(
                &self.body_scroll,
                "reveal-welcome-sign-in",
                v_flex()
                    .gap(px(space::S))
                    .child(crate::design::framed(
                        "welcome-sign-in",
                        Button::new("welcome-sign-in")
                            .w_full()
                            .outline()
                            .label(t!("welcome.sign_in").to_string())
                            .disabled(self.sign_in.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.sign_in_with_account(window, cx))),
                    ))
                    .children(
                        self.sign_in
                            .is_some()
                            .then(|| div().flex().justify_center().child(hint(t!("welcome.signing_in").to_string(), Tone::Neutral, &p))),
                    ),
            ))
            .child(
                h_flex()
                    .gap(px(space::M))
                    .child(div().flex_1().child(divider(&p)))
                    .child(hint(t!("welcome.or_api_key").to_string(), Tone::Neutral, &p))
                    .child(div().flex_1().child(divider(&p))),
            )
            .child(crate::reveal::reveal_in(
                &self.body_scroll,
                "reveal-welcome-api-key",
                v_flex()
                    .gap(px(space::S))
                    .child(crate::design::framed_handle(
                        gpui_kit::Focusable::focus_handle(self.api_key.read(cx), cx),
                        Input::new(&self.api_key).aria_label(t!("settings.api_key").to_string()),
                    ))
                    .child(hint(t!("welcome.api_key_hint").to_string(), Tone::Neutral, &p))
                    .children(self.error.clone().map(|error| motion::fade_in(hint(error, Tone::Danger, &p), "welcome-error-enter"))),
            ));
        crate::design::track_focus_source(v_flex())
            .size_full()
            .bg(p.surface)
            .text_color(p.text)
            // 欢迎窗不能调整大小：标题栏只有最小化和关闭（Windows 上自绘，其他平台是系统的）。
            .children(crate::title_bar::title_bar(false, None, window, cx))
            // 平滑滚动层放在最前，先于各滚动容器拿到滚轮事件（见 `smooth_scroll`）。
            .child(self.scroll.layer(cx))
            .child(body)
            .child(
                v_flex()
                    .flex_none()
                    .px(px(space::XXXL + space::L))
                    .pt(px(space::L))
                    .pb(px(space::XXXL))
                    .gap(px(space::S))
                    .child(crate::design::framed(
                        "welcome-save",
                        Button::new("welcome-save")
                            .primary()
                            .w_full()
                            .label(t!("welcome.save_continue").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.save_and_continue(window, cx))),
                    ))
                    .child(crate::design::framed(
                        "welcome-later",
                        Button::new("welcome-later")
                            .ghost()
                            .w_full()
                            .label(t!("welcome.later").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.set_up_later(window, cx))),
                    )),
            )
    }
}
