//! 设置弹窗的“关于”页：软件信息、版本、许可证与第三方声明。
//! 声明编进程序里（由 `scripts/third-party-notices.ps1` 生成），打开时导出成 Markdown 文件再用系统默认程序查看。

use std::path::PathBuf;
use std::sync::OnceLock;

use gpui_kit::component::button::Button;
use gpui_kit::component::{IconName, h_flex, v_flex};
use gpui_kit::*;
use rust_i18n::t;

use crate::design::ControlSize as _;

use crate::design::{
    Tone, Typography as _, brand_mark, card, description, field, hint, mono_font, palette, readable_with_id, section_header, size, space,
};
use crate::settings::SettingsView;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// 源码仓库（来自工作区 `Cargo.toml` 的 `repository`）。
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const LICENSE: &str = include_str!("../../../LICENSE");
const NOTICES: &str = include_str!("../../../THIRD_PARTY_NOTICES.md");

/// 第三方声明里“依赖清单”表格的行数，即随程序分发的 Rust crate 个数。
fn crate_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        NOTICES
            .lines()
            .skip_while(|line| !line.starts_with("## 依赖清单"))
            .skip(1)
            .take_while(|line| !line.starts_with("## "))
            .filter(|line| line.starts_with("| ["))
            .count()
    })
}

fn platform() -> String {
    let os = match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    };
    format!("{os} · {}", std::env::consts::ARCH)
}

/// 把编进程序的第三方声明导出为 Markdown 文件，用系统默认程序打开。
/// Windows 上写到当前目录，便于用户留存；其他平台写到临时目录。
fn open_notices(cx: &mut App) -> Result<(), String> {
    let path = notices_export_path();
    std::fs::write(&path, NOTICES).map_err(|error| t!("about.notices_failed", error = error.to_string()).to_string())?;
    cx.open_with_system(&path);
    Ok(())
}

#[cfg(windows)]
fn notices_export_path() -> PathBuf {
    std::env::current_dir().unwrap_or_default().join("THIRD_PARTY_NOTICES.md")
}

#[cfg(not(windows))]
fn notices_export_path() -> PathBuf {
    std::env::temp_dir().join("dshr-THIRD_PARTY_NOTICES.md")
}

impl SettingsView {
    pub(crate) fn render_about(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let mono = mono_font(cx);

        let identity = card(&p).p(px(space::XL)).child(
            h_flex().gap(px(space::L)).items_center().child(brand_mark(size::ICON_HERO, &p)).child(
                v_flex()
                    .gap(px(space::XS))
                    .child(div().text_title().child(readable_with_id("about-name", crate::APP_NAME)))
                    .child(description(t!("about.tagline").to_string(), &p)),
            ),
        );

        let info_row = |id: &'static str, label: String, value: String| {
            h_flex()
                .gap(px(space::L))
                .child(
                    div()
                        .w(px(120.))
                        .flex_none()
                        .text_body()
                        .text_color(p.text_secondary)
                        .child(readable_with_id(SharedString::from(format!("{id}-label")), label)),
                )
                .child(div().text_body().child(readable_with_id(SharedString::from(format!("{id}-value")), value)))
        };
        let info = v_flex()
            .gap(px(space::S))
            .child(info_row("about-version", t!("about.version").to_string(), VERSION.to_owned()))
            .child(info_row("about-platform", t!("about.platform").to_string(), platform()))
            .child(info_row("about-license", t!("about.license").to_string(), "MIT".to_owned()))
            .child(info_row("about-repository", t!("about.repository").to_string(), REPOSITORY.to_owned()))
            // 地址本身在上面一行读得到；按钮的名字说清楚会做什么（用系统浏览器打开）。
            .child(
                h_flex().pl(px(120. + space::L)).child(crate::design::framed(
                    "about-open-repository",
                    Button::new("about-open-repository")
                        .subtle(cx)
                        .control_sm()
                        .icon(IconName::ExternalLink)
                        .label(t!("about.open_repository").to_string())
                        .tooltip(REPOSITORY)
                        .on_click(cx.listener(|_, _, _, cx| cx.open_url(REPOSITORY))),
                )),
            );

        let license = field(
            t!("about.license").to_string(),
            v_flex().gap(px(space::S)).child(description(t!("about.license_desc").to_string(), &p)).child(
                card(&p)
                    .p(px(space::L))
                    .text_mono(mono.clone())
                    .text_color(p.text_secondary)
                    .child(readable_with_id("about-license-text", LICENSE.trim_end())),
            ),
        );

        let notice_error = self.about_error.clone().map(|error| hint(error, Tone::Danger, &p));
        let third_party = field(
            t!("about.third_party").to_string(),
            v_flex()
                .gap(px(space::S))
                .child(description(t!("about.third_party_desc", count = crate_count()).to_string(), &p))
                .child(h_flex().child(crate::design::framed(
                    "about-open-notices",
                    Button::new("about-open-notices").control_sm().label(t!("about.open_notices").to_string()).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.about_error = open_notices(cx).err();
                            cx.notify();
                        },
                    )),
                )))
                .children(notice_error),
        );

        v_flex()
            .gap(px(space::XL))
            .child(section_header(t!("settings.about").to_string(), None, &p))
            .child(identity)
            .child(info)
            .child(license)
            .child(third_party)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn notices_list_crates() {
        assert!(super::crate_count() > 100);
    }
}
