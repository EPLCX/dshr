//! 外部插件宿主。
//!
//! 现在有一种：**JS 插件**——`<数据目录>/plugins/<名字>/` 下一个 `index.js`（可选 `plugin.toml`），
//! 跑在内嵌的 QuickJS（`rquickjs`）里，每个插件一条线程。脚本本身碰不到文件、网络和进程，
//! 只能通过 `host` 上的函数做清单里声明过的事。写法和能力见 `docs/plugins.md`。
//!
//! QuickJS 是 C 代码，放在 `js` feature 后面（默认开）；关掉时 [`JsPlugins`] 是空的。

#[cfg(feature = "js")]
mod js;
// 没有 JS 运行时的构建里授权记录用不上。
/// 主程序交给插件宿主的服务（会话控制、输入框、通知、安装确认……）。
pub mod app;
/// 插件包：自带的 MCP 服务器、Skills、命令、预设。
pub mod bundle;
#[cfg_attr(not(feature = "js"), allow(dead_code))]
mod grants;
/// 插件索引：搜索、按 id 安装、查更新。
pub mod index;
/// 安装、卸载插件。
pub mod install;
/// 在任意文件夹里开发插件：`dshr plugin link`。
pub mod links;
mod manifest;
/// 打包与签名：`dshr plugin pack`、`dshr plugin keygen`。
#[cfg(feature = "js")]
pub mod pack;
mod scaffold;
/// 插件密钥（清单里的 `secrets`）的存取：设置页、命令行用。
pub mod secrets;
/// 装过的插件的来源、签名者和旧版本归档（回滚）。
pub mod sources;

pub use grants::{GRANTS_FILE, GrantRequest, Grants, grant as grant_capabilities, grants_path};
#[cfg(feature = "js")]
pub use js::{CreatorPlugin, JsPlugin, JsPlugins, LogLine, PluginReport, ReloadReport, Watcher};
pub use manifest::{Capabilities, Found, MANIFEST_FILE, PluginManifest, discover, fingerprint, host_satisfies, host_version, valid_id};
pub use scaffold::Scaffold;

/// 读一个插件文件夹的清单（和扫描插件目录同一套检查；清单里没写 id 时用文件夹名）。
pub fn load_manifest(dir: &std::path::Path) -> Result<PluginManifest, String> {
    let folder = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
    manifest::load(dir, &folder)
}

/// 设置页里一个 JS 插件的卡片（`JsPlugins::card`）。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginCard {
    pub id: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    /// 清单里声明的能力（清单里的写法）。
    pub declared: Vec<String>,
    /// 其中还没允许过的。
    pub missing: Vec<String>,
    /// 声明的密钥和有没有设置。
    pub secrets: Vec<(String, bool)>,
    /// 常驻服务的状态：(id, 一句话)。
    pub services: Vec<(String, String)>,
    /// 提供的东西：(种类, 数量)。种类是 `tools`、`sidebar`、`settings`、`panels`、`status`、`hooks`、`cards`、
    /// `commands`、`slash`、`presets`、`providers`、`themes`、`schedules`、`services`、`prompt`、`events`。
    pub provides: Vec<(&'static str, usize)>,
    pub status: Option<dshr_context::Status>,
    pub kept: Option<String>,
    pub stopped: Option<String>,
    /// 插件自带的 MCP 服务器：(名字, 启动方式, 现在的状态；插件没激活时没有)。
    pub mcp_servers: Vec<(String, String, Option<dshr_mcp::ServerStatus>)>,
    /// 链接到插件目录之外的文件夹（`dshr plugin link`，开发中）时是那个文件夹。
    pub linked: Option<std::path::PathBuf>,
}

/// 没有编进 JS 运行时的构建：插件目录里的东西不加载。
#[cfg(not(feature = "js"))]
#[derive(Clone, Default)]
pub struct JsPlugins;

#[cfg(not(feature = "js"))]
impl JsPlugins {
    pub fn discover(dir: &std::path::Path) -> Self {
        let found = manifest::discover(dir);
        if !found.is_empty() {
            tracing::warn!(count = found.len(), "这个构建没有 JS 运行时，插件目录里的插件没有加载");
        }
        Self
    }

    pub fn ids(&self) -> Vec<&'static str> {
        Vec::new()
    }

    pub fn is_empty(&self) -> bool {
        true
    }

    pub fn watch(&self) -> Watcher {
        Watcher
    }

    pub fn grant_requests(&self) -> Vec<GrantRequest> {
        Vec::new()
    }

    pub fn grant(&self, _id: &str) -> Result<(), String> {
        Err("这个构建没有 JS 运行时".into())
    }

    pub fn revoke(&self, _id: &str) -> Result<(), String> {
        Err("这个构建没有 JS 运行时".into())
    }

    pub fn kept(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    pub fn stopped(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    pub fn logs(&self, _id: &str) -> Vec<LogLine> {
        Vec::new()
    }

    pub fn card(&self, _id: &str) -> Option<PluginCard> {
        None
    }

    pub fn cards(&self) -> Vec<PluginCard> {
        Vec::new()
    }

    pub fn cards_with(&self, _requests: &[GrantRequest]) -> Vec<PluginCard> {
        Vec::new()
    }

    pub fn grant_request(&self, _id: &str) -> Option<GrantRequest> {
        None
    }

    pub fn grant_exact(&self, _id: &str, _capabilities: &[String]) -> Result<(), String> {
        Err("这个构建没有 JS 运行时".into())
    }

    pub fn granted(&self, _id: &str) -> Vec<String> {
        Vec::new()
    }
}

/// 没有 JS 运行时的构建里没有创造模式：插件装上，但什么都不提供。
#[cfg(not(feature = "js"))]
pub struct CreatorPlugin;

#[cfg(not(feature = "js"))]
impl CreatorPlugin {
    pub fn new(_scripts: JsPlugins) -> Self {
        Self
    }
}

#[cfg(not(feature = "js"))]
impl dshr_context::Plugin for CreatorPlugin {
    fn manifest(&self) -> dshr_context::Manifest {
        dshr_context::Manifest::new("creator")
    }

    fn apply(&self, _cx: &mut dshr_context::Scope) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 插件日志里的一行。
#[cfg(not(feature = "js"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: String,
    pub text: String,
}

/// 没有 JS 运行时的构建里什么都不盯。
#[cfg(not(feature = "js"))]
pub struct Watcher;

#[cfg(not(feature = "js"))]
impl Watcher {
    pub fn detach(self) {}
}

#[cfg(not(feature = "js"))]
impl dshr_context::PluginGroup for JsPlugins {
    fn add_to(self, _cx: &dshr_context::Context) -> Result<(), dshr_context::ContextError> {
        Ok(())
    }
}

#[cfg(all(test, feature = "js"))]
mod tests;
