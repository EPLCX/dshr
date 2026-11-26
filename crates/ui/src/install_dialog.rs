//! 安装插件的确认框：程序内自绘的模态对话框，取代系统原生的提示框。
//!
//! 用户从设置页装插件（安装包、文件夹、索引、回滚）和有 `plugins:install` 能力的插件替用户装插件时，
//! 都经桥（[`crate::bridge`]）走到这里。对话框把要装的东西摊开给用户看：
//!
//! - 头部：名字、版本、id、说明，以及按 [`assess`] 给出的风险总评（颜色之外还有图标和文字）。
//! - 来源与完整性：来源、发起的插件、SHA-256、文件数和大小、签名与发布者（首次、同一个、已变化）。
//! - 权限：每项一行，需要留意的排在前面、标出来；装上不等于授权。
//! - 包内容：会不会运行脚本、自带的 MCP 服务器（逐个列出启动命令）、Skills、命令、预设、版本要求。
//!
//! 风险高时“安装”按钮要等一小会儿才能点，防止误点、连点。默认焦点在“取消”上，Esc 也是取消；回车不会安装。
//! 对话框没回答就被丢掉时，发送端随之丢弃，等待的一方收到的是“不同意”。

use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use dshr_plugins::app::{InstallAnswer, InstallRequest, Publisher};
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
// 完整的 Lucide 目录：默认集合以外用到的图标（ShieldCheck、ShieldAlert）由 `assets.rs` 带上。
use gpui_kit::assets::IconName;
use gpui_kit::component::{Disableable as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::design::{
    Tone, Typography as _, badge, heading, mono_font, motion, palette, radius, readable, readable_with_id, shadow, size, space,
};
use crate::permissions;

/// 风险高时“安装”按钮解锁前要等的时间。
const UNLOCK_DELAY: Duration = Duration::from_millis(1500);
/// 对话框的宽度。
const DIALOG_WIDTH: f32 = 560.;

/// 风险总评。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl Risk {
    fn tone(self) -> Tone {
        match self {
            Risk::Low => Tone::Success,
            Risk::Medium => Tone::Warning,
            Risk::High => Tone::Danger,
        }
    }

    fn icon(self) -> IconName {
        match self {
            Risk::Low => IconName::ShieldCheck,
            Risk::Medium => IconName::CircleAlert,
            Risk::High => IconName::TriangleAlert,
        }
    }

    fn title(self) -> String {
        match self {
            Risk::Low => t!("install.risk_low"),
            Risk::Medium => t!("install.risk_medium"),
            Risk::High => t!("install.risk_high"),
        }
        .to_string()
    }

    fn detail(self) -> String {
        match self {
            Risk::Low => t!("install.risk_low_detail"),
            Risk::Medium => t!("install.risk_medium_detail"),
            Risk::High => t!("install.risk_high_detail"),
        }
        .to_string()
    }
}

/// 一项能力是不是“高风险”：任意联网、运行任意或指定程序、自带 MCP 服务器（在沙箱外以用户的权限运行）。
fn capability_high(name: &str) -> bool {
    name == "net:*" || name.starts_with("exec:") || name.starts_with("mcp:")
}

/// 给安装请求打个风险总评。
///
/// - 高：发布者变了；要任意联网、运行程序；自带 MCP 服务器。
/// - 需要留意：有“需要留意”的权限（[`permissions::capability_sensitive`]）；带脚本却没有签名；
///   当前版本的 dshr 不满足它的版本要求（装上也不会激活）。
/// - 低：其余。
pub fn assess(request: &InstallRequest) -> Risk {
    if matches!(request.publisher, Publisher::Changed { .. })
        || !request.mcp_servers.is_empty()
        || request.capabilities.iter().any(|name| capability_high(name))
    {
        return Risk::High;
    }
    let unsigned_script = request.has_script && request.publisher == Publisher::Unsigned;
    if unsigned_script || !request.compatible || request.capabilities.iter().any(|name| permissions::capability_sensitive(name)) {
        return Risk::Medium;
    }
    Risk::Low
}

/// 公钥的简短指纹：`dshr-ed25519:` 后面的前 16 个字符。
fn fingerprint(key: &str) -> String {
    let body = key.strip_prefix(dshr_plugins::pack::KEY_PREFIX).unwrap_or(key);
    let short: String = body.chars().take(16).collect();
    if body.chars().count() > 16 { format!("{short}…") } else { short }
}

/// 字节数给人看的写法。
fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} B")
    } else if value < KB * KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{:.1} MB", value / KB / KB)
    }
}

pub enum InstallDialogEvent {
    /// 用户回答了（或者对话框被关掉）：工作区把它收起来。
    Closed,
}

pub struct InstallDialog {
    request: InstallRequest,
    risk: Risk,
    /// 回答发出后是 `None`。没回答就被丢掉时，等待的一方收到“断开”，当作不同意。
    answer: Option<SyncSender<InstallAnswer>>,
    pub focus: FocusHandle,
    cancel_focus: FocusHandle,
    opened: Instant,
    phase: motion::Phase,
    /// 正文的滚动：Tab 走到看不见的地方时滚过去（见 `reveal::reveal_in`）。
    body_scroll: ScrollHandle,
}

impl EventEmitter<InstallDialogEvent> for InstallDialog {}

impl InstallDialog {
    pub fn new(request: InstallRequest, answer: SyncSender<InstallAnswer>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let risk = assess(&request);
        let focus = cx.focus_handle();
        let cancel_focus = cx.focus_handle();
        // 默认焦点在“取消”上：误按回车、空格不会装上。
        cancel_focus.focus(window, cx);
        if risk == Risk::High {
            // 解锁倒计时：每 250 毫秒重画一次，直到解锁。
            cx.spawn(async move |this, cx| {
                let started = Instant::now();
                while started.elapsed() < UNLOCK_DELAY + Duration::from_millis(100) {
                    cx.background_executor().timer(Duration::from_millis(250)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        let name = request.name.clone().unwrap_or_else(|| request.id.clone());
        crate::announce::announce(t!("install.announce", name = name, risk = risk.title()).to_string(), cx);
        Self {
            request,
            risk,
            answer: Some(answer),
            focus,
            cancel_focus,
            opened: Instant::now(),
            phase: motion::Phase::SETTLED,
            body_scroll: ScrollHandle::new(),
        }
    }

    pub fn set_phase(&mut self, phase: motion::Phase) {
        self.phase = phase;
    }

    /// 用户自己发起的安装（不是插件代装）。只有这时才能“信任新发布者并安装”。
    fn by_user(&self) -> bool {
        self.request.requested_by.is_none()
    }

    fn publisher_changed(&self) -> bool {
        matches!(self.request.publisher, Publisher::Changed { .. })
    }

    /// 还要等多久才能点“安装”。
    fn locked_for(&self) -> Option<Duration> {
        if self.risk != Risk::High {
            return None;
        }
        UNLOCK_DELAY.checked_sub(self.opened.elapsed()).filter(|left| !left.is_zero())
    }

    fn respond(&mut self, answer: InstallAnswer, cx: &mut Context<Self>) {
        if let Some(sender) = self.answer.take() {
            let _ = sender.send(answer);
        }
        cx.emit(InstallDialogEvent::Closed);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.respond(InstallAnswer::Deny, cx);
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if self.locked_for().is_some() || self.answer.is_none() {
            return;
        }
        let answer = match (self.publisher_changed(), self.by_user()) {
            (false, _) => InstallAnswer::Allow,
            (true, true) => InstallAnswer::AllowNewPublisher,
            // 插件代装时发布者变了：不给装（按钮本来就是禁用的）。
            (true, false) => return,
        };
        self.respond(answer, cx);
    }

    /// 一个区块：小标题 + 内容。
    fn section(id: &'static str, title: String, body: impl IntoElement, p: &crate::design::Palette) -> Stateful<Div> {
        v_flex()
            .id(id)
            .role(Role::Group)
            .aria_label(title.clone())
            .gap(px(space::S))
            .child(div().text_label().text_color(p.text_secondary).child(readable(title)))
            .child(body)
    }

    /// 一行“键：值”。
    fn fact(id: impl Into<ElementId>, label: String, value: impl IntoElement, p: &crate::design::Palette) -> Stateful<Div> {
        h_flex()
            .id(id)
            .gap(px(space::M))
            .items_start()
            .child(div().flex_none().w(px(96.)).text_body().text_color(p.text_secondary).child(readable(label)))
            .child(div().flex_1().min_w(px(0.)).text_body().child(value))
    }

    /// 一行带图标、可着色的说明。
    fn note(id: impl Into<ElementId>, icon: IconName, text: String, tone: Tone, p: &crate::design::Palette) -> Stateful<Div> {
        let color = tone.color(p);
        h_flex()
            .id(id)
            .gap(px(space::S))
            .items_start()
            .child(div().flex_none().pt(px(3.)).child(Icon::new(icon).size(px(size::ICON_SM)).text_color(color)))
            .child(
                div().flex_1().min_w(px(0.)).text_body().when(tone != Tone::Neutral, |text| text.text_color(color)).child(readable(text)),
            )
    }

    fn render_header(&self, p: &crate::design::Palette) -> Div {
        let request = &self.request;
        let name = request.name.clone().unwrap_or_else(|| request.id.clone());
        let tone = self.risk.tone();
        let color = tone.color(p);
        let banner = h_flex()
            .id("install-risk")
            .role(Role::Status)
            .aria_label(format!("{}：{}", self.risk.title(), self.risk.detail()))
            .gap(px(space::M))
            .items_start()
            .p(px(space::M))
            .rounded(px(radius::LG))
            .border_1()
            .border_color(color.opacity(0.5))
            .bg(color.opacity(0.1))
            .child(div().flex_none().pt(px(2.)).child(Icon::new(self.risk.icon()).size(px(size::ICON_LG)).text_color(color)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(2.))
                    .child(div().text_label().text_color(color).child(readable(self.risk.title())))
                    .child(div().text_caption().text_color(p.text).child(readable(self.risk.detail()))),
            );
        v_flex()
            .gap(px(space::M))
            .child(
                h_flex()
                    .gap(px(space::S))
                    .items_center()
                    .flex_wrap()
                    .child(heading(t!("install.title", name = name).to_string(), 1).text_title())
                    .when_some(request.version.clone(), |row, version| row.child(badge(format!("v{version}"), p))),
            )
            .child(div().text_caption().text_color(p.text_secondary).child(readable(format!("id：{}", request.id))))
            .when_some(request.description.clone(), |column, description| {
                column.child(div().text_body().child(readable_with_id("install-description", description)))
            })
            .child(banner)
    }

    fn render_source(&self, mono: &SharedString, p: &crate::design::Palette) -> Stateful<Div> {
        let request = &self.request;
        let mut rows = v_flex().gap(px(space::S));
        if let Some(by) = &request.requested_by {
            rows =
                rows.child(Self::note("install-by", IconName::Bot, t!("install.requested_by", plugin = by).to_string(), Tone::Warning, p));
        }
        rows = rows.child(Self::fact(
            "install-source",
            t!("install.source_label").to_string(),
            div().text_mono(mono.clone()).child(readable_with_id("install-source-value", request.source.clone())),
            p,
        ));
        if let Some(package) = &request.package {
            if let Some(sha256) = &package.sha256 {
                // 太长了：分两行显示，读屏软件读完整的。
                let (head, tail) = sha256.split_at(sha256.len() / 2);
                rows = rows.child(Self::fact(
                    "install-sha256",
                    "SHA-256".to_owned(),
                    v_flex()
                        .id("install-sha256-value")
                        .role(Role::Label)
                        .aria_value(sha256.clone())
                        .text_mono(mono.clone())
                        .text_color(p.text_secondary)
                        .child(head.to_owned())
                        .child(tail.to_owned()),
                    p,
                ));
            }
            rows = rows.child(Self::fact(
                "install-files",
                t!("install.files_label").to_string(),
                readable(t!("install.files", count = package.files, size = human_size(package.bytes)).to_string()),
                p,
            ));
        }
        // 签名与发布者。
        let (icon, text, tone) = match &request.publisher {
            Publisher::Unsigned => {
                (IconName::CircleAlert, t!("install.unsigned").to_string(), if request.has_script { Tone::Warning } else { Tone::Neutral })
            }
            Publisher::Signed { key } => {
                (IconName::ShieldCheck, t!("install.signed_by", key = fingerprint(key)).to_string(), Tone::Neutral)
            }
            Publisher::Trusted { key } => {
                (IconName::ShieldCheck, t!("install.same_publisher", key = fingerprint(key)).to_string(), Tone::Success)
            }
            Publisher::Changed { previous, now: Some(now) } => (
                IconName::ShieldAlert,
                t!("install.publisher_changed", previous = fingerprint(previous), now = fingerprint(now)).to_string(),
                Tone::Danger,
            ),
            Publisher::Changed { previous, now: None } => {
                (IconName::ShieldAlert, t!("install.publisher_removed", previous = fingerprint(previous)).to_string(), Tone::Danger)
            }
        };
        rows = rows.child(Self::note("install-publisher", icon, text, tone, p));
        if request.replaces {
            let text = match &request.replaces_version {
                Some(version) => t!("install.replaces_version", version = version).to_string(),
                None => t!("install.replaces").to_string(),
            };
            rows = rows.child(Self::note("install-replaces", IconName::Info, text, Tone::Neutral, p));
        }
        Self::section("install-section-source", t!("install.section_source").to_string(), rows, p)
    }

    fn render_permissions(&self, mono: &SharedString, p: &crate::design::Palette) -> Stateful<Div> {
        let request = &self.request;
        // 需要留意的排在前面，其余按清单里的顺序。
        let mut names: Vec<&String> = request.capabilities.iter().collect();
        names.sort_by_key(|name| (!capability_high(name), !permissions::capability_sensitive(name)));
        let body = if names.is_empty() {
            v_flex().child(Self::note(
                "install-no-permissions",
                IconName::CircleCheck,
                t!("install.no_permissions").to_string(),
                Tone::Success,
                p,
            ))
        } else {
            let mut list = v_flex().gap(px(space::XS));
            for (index, name) in names.into_iter().enumerate() {
                let text = permissions::capability_text(name);
                let (tone, tag) = if capability_high(name) {
                    (Tone::Danger, Some(t!("install.high_risk").to_string()))
                } else if permissions::capability_sensitive(name) {
                    (Tone::Warning, Some(t!("install.sensitive").to_string()))
                } else {
                    (Tone::Neutral, None)
                };
                let color = tone.color(p);
                let spoken = match &tag {
                    Some(tag) => format!("{text}（{name}），{tag}"),
                    None => format!("{text}（{name}）"),
                };
                list = list.child(
                    h_flex()
                        .id(SharedString::from(format!("install-permission-{index}")))
                        .role(Role::Label)
                        .aria_value(spoken)
                        .gap(px(space::M))
                        .items_start()
                        .py(px(space::S))
                        .px(px(space::M))
                        .rounded(px(radius::MD))
                        .bg(p.sunken)
                        .border_l_2()
                        .border_color(if tag.is_some() { color } else { p.border })
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w(px(0.))
                                .gap(px(2.))
                                .child(div().text_body().when(tag.is_some(), |text| text.text_color(color)).child(text))
                                .child(div().text_caption().font_family(mono.clone()).text_color(p.text_secondary).child(name.clone())),
                        )
                        .when_some(tag, |row, tag| {
                            row.child(
                                div()
                                    .flex_none()
                                    .px(px(space::S))
                                    .py(px(1.))
                                    .rounded_full()
                                    .text_caption()
                                    .bg(color.opacity(0.12))
                                    .text_color(color)
                                    .child(tag),
                            )
                        }),
                );
            }
            list.child(div().pt(px(space::XS)).child(Self::note(
                "install-grant-later",
                IconName::Info,
                t!("install.grant_later").to_string(),
                Tone::Neutral,
                p,
            )))
        };
        Self::section("install-section-permissions", t!("install.section_permissions").to_string(), body, p)
    }

    fn render_contents(&self, mono: &SharedString, p: &crate::design::Palette) -> Option<Stateful<Div>> {
        let request = &self.request;
        let mut rows = v_flex().gap(px(space::S));
        let mut any = false;
        if request.has_script {
            any = true;
            rows = rows.child(Self::note("install-script", IconName::FileText, t!("install.runs_script").to_string(), Tone::Neutral, p));
        }
        for (index, (name, launch)) in request.mcp_servers.iter().enumerate() {
            any = true;
            rows = rows.child(
                v_flex()
                    .id(SharedString::from(format!("install-mcp-{index}")))
                    .gap(px(2.))
                    .child(Self::note(
                        ("install-mcp-note", index),
                        IconName::SquareTerminal,
                        t!("install.mcp_server", name = name).to_string(),
                        Tone::Danger,
                        p,
                    ))
                    .child(
                        div()
                            .ml(px(size::ICON_SM + space::S))
                            .px(px(space::S))
                            .py(px(space::XS))
                            .rounded(px(radius::SM))
                            .bg(p.sunken)
                            .text_caption()
                            .font_family(mono.clone())
                            .child(readable_with_id(("install-mcp-launch", index), launch.clone())),
                    ),
            );
        }
        if request.skills {
            any = true;
            rows = rows.child(Self::note("install-skills", IconName::Bot, t!("install.skills").to_string(), Tone::Neutral, p));
        }
        if request.commands > 0 {
            any = true;
            rows = rows.child(Self::note(
                "install-commands",
                IconName::SquareTerminal,
                t!("install.commands", count = request.commands).to_string(),
                Tone::Neutral,
                p,
            ));
        }
        if request.presets > 0 {
            any = true;
            rows = rows.child(Self::note(
                "install-presets",
                IconName::Cpu,
                t!("install.presets", count = request.presets).to_string(),
                Tone::Neutral,
                p,
            ));
        }
        if !request.compatible {
            any = true;
            let requirement = request.requires_dshr.clone().unwrap_or_default();
            let current = dshr_plugins::host_version().to_string();
            rows = rows.child(Self::note(
                "install-incompatible",
                IconName::CircleAlert,
                t!("install.incompatible", requirement = requirement, current = current).to_string(),
                Tone::Warning,
                p,
            ));
        }
        any.then(|| Self::section("install-section-contents", t!("install.section_contents").to_string(), rows, p))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> Div {
        let p = palette(cx);
        let changed = self.publisher_changed();
        let blocked = changed && !self.by_user();
        let locked = self.locked_for();
        let base = if changed { t!("install.allow_new_publisher").to_string() } else { t!("install.allow").to_string() };
        let label = match locked {
            Some(left) => t!("install.unlock_in", label = base, seconds = left.as_secs() + 1).to_string(),
            None => base.clone(),
        };
        let install = Button::new("install-allow")
            .label(label)
            .accessibility_label(base)
            .disabled(locked.is_some() || blocked)
            .on_click(cx.listener(|this, _, _, cx| this.confirm(cx)));
        let install = if changed || self.risk == Risk::High { install.danger() } else { install.primary() };
        let cancel = Button::new("install-deny")
            .label(t!("install.deny").to_string())
            .track_focus(&self.cancel_focus)
            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)));
        v_flex()
            .gap(px(space::S))
            .when(blocked, |footer| {
                footer.child(Self::note(
                    "install-blocked",
                    IconName::TriangleAlert,
                    t!("install.publisher_blocked").to_string(),
                    Tone::Danger,
                    &p,
                ))
            })
            .child(
                h_flex()
                    .gap(px(space::S))
                    .justify_end()
                    .child(crate::design::framed_handle(self.cancel_focus.clone(), cancel))
                    .child(crate::design::framed("install-allow", install)),
            )
    }
}

impl Render for InstallDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = window;
        let p = palette(cx);
        let mono = mono_font(cx);
        let name = self.request.name.clone().unwrap_or_else(|| self.request.id.clone());
        let header = self.render_header(&p);
        let source = self.render_source(&mono, &p);
        let permissions = self.render_permissions(&mono, &p);
        let contents = self.render_contents(&mono, &p);
        let footer = self.render_footer(cx);
        v_flex()
            .id("install-dialog")
            .role(Role::AlertDialog)
            .aria_label(t!("install.title", name = name).to_string())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.cancel(cx);
                }
            }))
            .occlude()
            .w(px(DIALOG_WIDTH))
            .max_w(relative(0.94))
            .max_h(relative(0.9))
            .rounded(px(radius::XL))
            .bg(p.raised)
            .border_1()
            .border_color(p.border)
            .text_color(p.text)
            .overflow_hidden()
            .map(|dialog| motion::pop(dialog, motion::Edge::Bottom, motion::SHORT, self.phase, shadow::modal(&p)))
            .child(div().px(px(space::XXL)).pt(px(space::XL)).pb(px(space::M)).child(header))
            .child(
                v_flex()
                    .id("install-dialog-body")
                    .track_scroll(&self.body_scroll)
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .px(px(space::XXL))
                    .pb(px(space::L))
                    .gap(px(space::XL))
                    .child(crate::reveal::reveal_in(&self.body_scroll, "reveal-install-source", source))
                    .child(crate::reveal::reveal_in(&self.body_scroll, "reveal-install-permissions", permissions))
                    .children(contents.map(|contents| crate::reveal::reveal_in(&self.body_scroll, "reveal-install-contents", contents))),
            )
            .child(div().h(px(1.)).bg(p.border))
            .child(div().px(px(space::XXL)).py(px(space::L)).child(footer))
            // Tab 只在对话框里循环。
            .focus_trap("install-dialog-trap", &self.focus)
    }
}

#[cfg(test)]
mod tests {
    // 不用 `super::*`：那样会把 GPUI 的 `test` 宏一起引进来，盖过标准的 `#[test]`。
    use super::{InstallRequest, Publisher, Risk, assess, fingerprint, human_size};

    fn request(capabilities: &[&str]) -> InstallRequest {
        InstallRequest {
            id: "demo".into(),
            compatible: true,
            capabilities: capabilities.iter().map(|name| (*name).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn risk_levels() {
        assert_eq!(assess(&request(&[])), Risk::Low);
        assert_eq!(assess(&request(&["fs:workspace:read", "net:api.example.com"])), Risk::Low);
        assert_eq!(assess(&request(&["fs:workspace:write"])), Risk::Medium);
        assert_eq!(assess(&request(&["net:*"])), Risk::High);
        assert_eq!(assess(&request(&["exec:git"])), Risk::High);

        let mut script = request(&[]);
        script.has_script = true;
        assert_eq!(assess(&script), Risk::Medium);
        script.publisher = Publisher::Signed { key: "dshr-ed25519:abc".into() };
        assert_eq!(assess(&script), Risk::Low);
        script.publisher = Publisher::Changed { previous: "dshr-ed25519:abc".into(), now: None };
        assert_eq!(assess(&script), Risk::High);

        let mut old = request(&[]);
        old.compatible = false;
        assert_eq!(assess(&old), Risk::Medium);

        let mut mcp = request(&[]);
        mcp.mcp_servers = vec![("github".into(), "npx github-mcp".into())];
        assert_eq!(assess(&mcp), Risk::High);
    }

    #[test]
    fn formatting() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(fingerprint("dshr-ed25519:ABCDEFGHIJKLMNOPQRSTUV"), "ABCDEFGHIJKLMNOP…");
        assert_eq!(fingerprint("short"), "short");
    }
}

#[cfg(test)]
mod view_tests {
    // 不用 `super::*`：那样会把 GPUI 的 `test` 宏一起引进来，盖过标准的 `#[test]`。
    use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
    use std::time::{Duration, Instant};

    use dshr_plugins::app::{InstallAnswer, InstallRequest, PackageInfo, Publisher};
    use gpui::TestAppContext;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Bounds, Entity, WindowBounds, WindowOptions, point, px, size};

    use super::{InstallDialog, Risk};

    struct Fixture {
        window: AnyWindowHandle,
        view: Entity<InstallDialog>,
        answers: Receiver<InstallAnswer>,
    }

    fn open(cx: &mut TestAppContext, request: InstallRequest) -> Fixture {
        let (sender, answers) = sync_channel(1);
        let (window, view) = cx.update(|cx| {
            gpui_kit::init(cx);
            crate::design::init(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds { origin: point(px(0.), px(0.)), size: size(px(900.), px(800.)) })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| InstallDialog::new(request, sender, window, cx))).unwrap()
        });
        draw(cx, window);
        Fixture { window, view, answers }
    }

    fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
        cx.update_window(window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    /// 按下再松开（GPUI 松开 Enter 时才算点了有焦点的按钮）。
    fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &'static str) {
        cx.simulate_keystrokes(window, key);
        let keystroke = gpui_kit::Keystroke::parse(key).unwrap();
        cx.update_window(window, |_, window, cx| {
            window.dispatch_event(gpui_kit::PlatformInput::KeyUp(gpui_kit::KeyUpEvent { keystroke }), cx);
        })
        .unwrap();
        draw(cx, window);
    }

    fn package(publisher: Publisher, capabilities: &[&str]) -> InstallRequest {
        InstallRequest {
            id: "word-count".into(),
            name: Some("Word Count".into()),
            version: Some("1.2.0".into()),
            description: Some("Counts words".into()),
            source: "/tmp/word-count-1.2.0.dshrplugin".into(),
            capabilities: capabilities.iter().map(|name| (*name).to_owned()).collect(),
            package: Some(PackageInfo { sha256: Some("ab".repeat(32)), files: 3, bytes: 4096 }),
            publisher,
            has_script: true,
            compatible: true,
            ..Default::default()
        }
    }

    fn confirm(cx: &mut TestAppContext, fixture: &Fixture) {
        let view = fixture.view.clone();
        cx.update(|cx| view.update(cx, |view, cx| view.confirm(cx)));
        draw(cx, fixture.window);
    }

    #[gpui::test]
    fn cancel_has_focus_and_keys_never_install(cx: &mut TestAppContext) {
        let fixture = open(cx, package(Publisher::Signed { key: "dshr-ed25519:KEY".into() }, &["fs:workspace:read"]));
        let focused = cx.update_window(fixture.window, |_, window, cx| fixture.view.read(cx).cancel_focus.is_focused(window)).unwrap();
        assert!(focused, "打开时焦点应当在“取消”上");
        // 回车落在“取消”上：不装。
        press(cx, fixture.window, "enter");
        assert_eq!(fixture.answers.try_recv(), Ok(InstallAnswer::Deny));
    }

    #[gpui::test]
    fn escape_declines(cx: &mut TestAppContext) {
        let fixture = open(cx, package(Publisher::Unsigned, &[]));
        press(cx, fixture.window, "escape");
        assert_eq!(fixture.answers.try_recv(), Ok(InstallAnswer::Deny));
    }

    #[gpui::test]
    fn low_risk_installs_at_once(cx: &mut TestAppContext) {
        let fixture = open(cx, package(Publisher::Signed { key: "dshr-ed25519:KEY".into() }, &["fs:workspace:read"]));
        assert_eq!(cx.update(|cx| fixture.view.read(cx).risk), Risk::Low);
        confirm(cx, &fixture);
        assert_eq!(fixture.answers.try_recv(), Ok(InstallAnswer::Allow));
    }

    #[gpui::test]
    fn high_risk_waits_before_installing(cx: &mut TestAppContext) {
        let fixture = open(cx, package(Publisher::Signed { key: "dshr-ed25519:KEY".into() }, &["exec:git", "net:*"]));
        assert_eq!(cx.update(|cx| fixture.view.read(cx).risk), Risk::High);
        confirm(cx, &fixture);
        assert_eq!(fixture.answers.try_recv(), Err(TryRecvError::Empty), "解锁之前不能装");
        let view = fixture.view.clone();
        cx.update(|cx| view.update(cx, |view, _| view.opened = Instant::now() - Duration::from_secs(5)));
        confirm(cx, &fixture);
        assert_eq!(fixture.answers.try_recv(), Ok(InstallAnswer::Allow));
    }

    #[gpui::test]
    fn changed_publisher_needs_the_user(cx: &mut TestAppContext) {
        let changed = Publisher::Changed { previous: "dshr-ed25519:OLD".into(), now: Some("dshr-ed25519:NEW".into()) };
        // 用户自己装：明确“信任新发布者并安装”。
        let fixture = open(cx, package(changed.clone(), &[]));
        let view = fixture.view.clone();
        cx.update(|cx| view.update(cx, |view, _| view.opened = Instant::now() - Duration::from_secs(5)));
        confirm(cx, &fixture);
        assert_eq!(fixture.answers.try_recv(), Ok(InstallAnswer::AllowNewPublisher));

        // 插件代装：不给装。
        let mut request = package(changed, &[]);
        request.requested_by = Some("market".into());
        let fixture = open(cx, request);
        let view = fixture.view.clone();
        cx.update(|cx| view.update(cx, |view, _| view.opened = Instant::now() - Duration::from_secs(5)));
        confirm(cx, &fixture);
        assert_eq!(fixture.answers.try_recv(), Err(TryRecvError::Empty));
    }
}
