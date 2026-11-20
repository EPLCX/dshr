//! 主程序（桌面版、命令行版）交给插件宿主的服务：插件替用户发消息、写输入框、发通知、装插件之前问用户、
//! 页面上的动作调用模型、打开浏览器。
//!
//! 这些服务属于“这个进程里的主程序”，不属于某个 `Context`，所以放在一个进程级的表里：主程序启动时
//! [`set_app_services`] 填一次，没填的项在插件里调用时报“这个程序不支持”。

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use dshr_agent::LlmAccess;

/// 插件要发的一条消息发到哪里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTarget {
    /// 当前打开的会话（桌面版是正在看的那个，命令行版是正在跑的那个）。
    Current,
    /// 指定 id 的会话（要已经打开着）。
    Id(String),
    /// 新建一个会话。
    New,
}

#[derive(Debug, Clone)]
pub struct SendRequest {
    /// 发消息的插件（对话区会显示“由 <插件> 发送”）。
    pub plugin: String,
    pub text: String,
    pub target: SessionTarget,
    /// 新会话的工作区；没有时用当前的。
    pub workspace: Option<PathBuf>,
}

/// 一个打开着的会话。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OpenSession {
    pub id: String,
    pub title: String,
    pub workspace: Option<String>,
    pub running: bool,
    pub current: bool,
}

/// 会话控制（`session:control`）。实现要快：只把请求排进主程序的队列，不等这一轮跑完。
pub trait SessionControl: Send + Sync {
    /// 发一条消息，返回它进了哪个会话（新会话还没有 id 时是空字符串）。正在运行时排队。
    fn send(&self, request: SendRequest) -> Result<String, String>;
    /// 停止会话（`None` 是当前会话）正在跑的这一轮。
    fn stop(&self, session: Option<String>) -> Result<(), String>;
    fn list(&self) -> Vec<OpenSession>;
}

/// 输入框（页面动作、命令、斜杠命令里的 `host.composer`）。
pub trait ComposerAccess: Send + Sync {
    /// 替换输入框里的文字。
    fn set_text(&self, text: &str) -> Result<(), String>;
    /// 输入框里现在的文字。
    fn text(&self) -> Option<String>;
}

/// 一条桌面通知。
#[derive(Debug, Clone)]
pub struct Notification {
    pub plugin: String,
    pub title: String,
    pub body: String,
    /// 点了打开这个插件的哪一页。
    pub page: Option<String>,
}

/// 装插件之前给用户看的内容。
#[derive(Debug, Clone, Default)]
pub struct InstallRequest {
    /// 发起安装的插件；用户自己在命令行装时是 `None`。
    pub requested_by: Option<String>,
    pub id: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub source: String,
    pub capabilities: Vec<String>,
    /// 同名插件已经装着（这次是覆盖升级）。
    pub replaces: bool,
    /// 已经装着的那个版本的版本号（`replaces` 时）。
    pub replaces_version: Option<String>,
    /// 从安装包（`.dshrplugin`、zip、tar.gz）装时：包的摘要、文件数、解开后的大小。
    pub package: Option<PackageInfo>,
    /// 签名和发布者。
    pub publisher: Publisher,
    /// 带脚本（会在 QuickJS 里运行 JS）。
    pub has_script: bool,
    /// 自带的 MCP 服务器：名字和启动方式（命令行或地址）。它们在沙箱外以用户的权限运行。
    pub mcp_servers: Vec<(String, String)>,
    /// 带 `skills/`。
    pub skills: bool,
    pub commands: usize,
    pub presets: usize,
    /// 要求的宿主版本（`[engines] dshr`）。
    pub requires_dshr: Option<String>,
    /// 当前宿主满足 `requires_dshr`（不满足时装上也不激活）。
    pub compatible: bool,
}

/// 安装包的信息。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageInfo {
    pub sha256: Option<String>,
    pub files: usize,
    pub bytes: u64,
}

/// 包的签名和“首次信任”的发布者比起来怎么样（见 [`crate::sources`]）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Publisher {
    /// 没签名，之前也没有记下发布者。
    #[default]
    Unsigned,
    /// 签了名，第一次见到这个插件的发布者。
    Signed { key: String },
    /// 签了名，和之前装的同一个发布者。
    Trusted { key: String },
    /// 发布者变了：之前由 `previous` 签名，这次换了钥匙（`now`）或者没签（`None`）。不明确同意就不装。
    Changed { previous: String, now: Option<String> },
}

/// 用户对安装确认的回答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallAnswer {
    Deny,
    Allow,
    /// 发布者变了也装（信任新的发布者）。只有用户自己发起的安装才会这样回答；插件代装时当作拒绝。
    AllowNewPublisher,
}

impl InstallAnswer {
    pub fn allowed(self) -> bool {
        !matches!(self, Self::Deny)
    }
}

pub type LlmSource = Arc<dyn Fn() -> Option<LlmAccess> + Send + Sync>;
pub type Notifier = Arc<dyn Fn(&Notification) + Send + Sync>;
/// 问用户要不要装：阻塞地等用户回答。
pub type InstallPrompt = Arc<dyn Fn(&InstallRequest) -> InstallAnswer + Send + Sync>;
/// 用系统浏览器打开一个地址（OAuth 登录）。
pub type UrlOpener = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

#[derive(Default, Clone)]
pub struct AppServices {
    /// 当前的模型（默认模型）：没有会话的地方（页面动作、命令）调 `host.llm` 时用。
    pub llm: Option<LlmSource>,
    pub session: Option<Arc<dyn SessionControl>>,
    pub composer: Option<Arc<dyn ComposerAccess>>,
    pub notifier: Option<Notifier>,
    pub install_prompt: Option<InstallPrompt>,
    pub open_url: Option<UrlOpener>,
    /// 插件改了 `config.toml`（`host.dshr.config.patch`、`host.dshr.mcp.*`）之后调用：主程序重新读配置，
    /// MCP 服务器的增删即时生效。
    pub config_changed: Option<Arc<dyn Fn() + Send + Sync>>,
}

static SERVICES: RwLock<Option<AppServices>> = RwLock::new(None);

/// 主程序启动时调用：填入它支持的服务。可以多次调用（后一次覆盖前一次填的项）。
pub fn set_app_services(update: impl FnOnce(&mut AppServices)) {
    let mut services = SERVICES.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    update(services.get_or_insert_with(AppServices::default));
}

/// 现在的服务（克隆一份，不持锁）。
pub fn app_services() -> AppServices {
    SERVICES.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone().unwrap_or_default()
}
