//! 程序的图标资源：组件库默认带的那一百来个图标，加上我们另外用到的几个。
//!
//! `gpui_kit::assets::Assets` 只内置组件库自己用的图标；完整 Lucide 目录里的其它图标要程序自己带，
//! 否则 `Icon::new` 画出来是空的。这里用 `icon_assets!` 只挑用到的几个（每个几百字节），找不到的路径交给 `Assets`。
//! 新用了默认集合以外的图标（`gpui_kit::assets::IconName::…`），加到下面的列表里。

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(ExtraIcons, [Image, Share2, Database, GitBranch, Hand, ShieldCheck, ShieldAlert, CornerDownLeft]);

/// 通用无障碍标志（圆圈里张开双臂的人形），见 `design::AppIcon::UniversalAccess`。Lucide 没有这个图标，
/// 按它的风格画：24×24、2px 描边、圆角端点，人形取自 `person-standing`、缩进圆圈里。
pub(crate) const UNIVERSAL_ACCESS_PATH: &str = "icons/dshr/universal-access.svg";
const UNIVERSAL_ACCESS_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><circle cx="12" cy="7" r="1"/><path d="m7.5 9.5 4.5 1.5 4.5-1.5"/><path d="M12 11v3"/><path d="m9.5 18 2.5-4 2.5 4"/></svg>"#;

/// 交给 `Application::with_assets` 的资源。
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == UNIVERSAL_ACCESS_PATH {
            return Ok(Some(Cow::Borrowed(UNIVERSAL_ACCESS_SVG)));
        }
        match ExtraIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        if UNIVERSAL_ACCESS_PATH.starts_with(path) {
            paths.push(UNIVERSAL_ACCESS_PATH.into());
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
