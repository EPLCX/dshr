//! UI 与 agent 之间的桥：GPUI 有自己的执行器，网络与工具执行放在独立的 tokio 运行时里，
//! 两边只通过 tokio 的 channel 通信（channel 不依赖运行时，可以在 GPUI 任务里 await）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use dshr_agent::{
    Agent, AgentConfig, ApprovalRequest, Approver, Decision, PermissionMode, Preset, Session, SessionCatalog, SessionSummary,
};
use dshr_config::account;
use dshr_config::credentials::{self, KeySource};
use dshr_config::{Config, Keybindings, ModelRef, Paths, ReasoningEffort, ResolvedProvider, UiConfig};
use dshr_context::{Context as Plugins, PluginInfo, Status as PluginStatus};
use dshr_core::{ContentBlock, Role, StopReason};
use dshr_llm::mock::ScriptedProvider;
use dshr_llm::{BlockKind, ChatRequest, EventStream, HttpClient, LlmError, Provider, StreamEvent};
use dshr_plugins::{GrantRequest, JsPlugins};
use dshr_tools::ToolContext;
use futures::StreamExt;
use rust_i18n::t;
use serde_json::json;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot};

/// 侧边栏最多记住的工作区数量。
const MAX_WORKSPACES: usize = 20;

/// 内置 DeepSeek 提供商 ID：账号登录的令牌只对它生效。
const DEEPSEEK: &str = "deepseek";

/// 内置的高对比度配色，只给“辅助功能 → 高对比度”用（不在 `themes()` 的配色方案列表里）。
/// 按 WCAG 2.2 AAA：所有文字色（正文、次要文字、强调色、成功、警告、错误）在每一种底色（页面、浮层、侧边栏、
/// 下沉区、选中项）上都不低于 7:1；描边、焦点框这类非文字元素不低于 3:1。测试里逐对检查（`high_contrast_meets_aaa`）。
pub(crate) fn builtin_high_contrast_theme() -> dshr_agent::ThemeDef {
    let set = |pairs: &[(&str, &str)]| {
        pairs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect::<std::collections::BTreeMap<_, _>>()
    };
    dshr_agent::ThemeDef {
        plugin: String::new(),
        id: "high-contrast".into(),
        name: String::new(),
        light: set(&[
            ("text", "#000000"),
            ("text_secondary", "#1F1F1F"),
            ("surface", "#FFFFFF"),
            ("raised", "#FFFFFF"),
            ("sidebar", "#F2F2F2"),
            ("sunken", "#E6E6E6"),
            ("selected", "#F2DA00"),
            ("border", "#000000"),
            ("accent", "#002A9E"),
            ("accent_soft", "#E0E8FF"),
            ("on_accent", "#FFFFFF"),
            ("success", "#00450F"),
            ("warning", "#5C3700"),
            ("danger", "#800000"),
            ("focus_ring", "#0037B8"),
        ]),
        dark: set(&[
            ("text", "#FFFFFF"),
            ("text_secondary", "#E8E8E8"),
            ("surface", "#000000"),
            ("raised", "#161616"),
            ("sidebar", "#0E0E0E"),
            ("sunken", "#242424"),
            ("selected", "#14285C"),
            ("border", "#FFFFFF"),
            ("accent", "#A8BFFF"),
            ("accent_soft", "#14285C"),
            ("on_accent", "#000000"),
            ("success", "#3DDC7C"),
            ("warning", "#FFBF47"),
            ("danger", "#FFA8A8"),
            ("focus_ring", "#B3C5FF"),
        ]),
    }
}

/// 跟随 Windows 对比度主题时的配色：直接用系统颜色（WCAG 与 Windows 的做法：开着对比度主题时，应用用系统颜色）。
///
/// 对比度主题只有几种颜色：没有次要文字色、状态色，都用文本色（状态靠文字和图标表达）；没有分层的底色，
/// 页面、浮层、侧边栏都用背景色，层次靠描边。选中项、悬停的行用“选定文本”的配色；链接、强调用超链接色。
/// 明暗由系统主题本身决定，两套相同。
pub(crate) fn system_high_contrast_theme(colors: crate::system_a11y::SystemColors) -> dshr_agent::ThemeDef {
    let hex = |color: u32| format!("#{color:06X}");
    let (text, background) = (hex(colors.window_text), hex(colors.window));
    let roles: Vec<(&str, String)> = vec![
        ("text", text.clone()),
        ("text_secondary", text.clone()),
        ("success", text.clone()),
        ("warning", text.clone()),
        ("danger", text.clone()),
        ("border", text.clone()),
        ("focus_ring", text.clone()),
        ("surface", background.clone()),
        ("raised", background.clone()),
        ("sidebar", background.clone()),
        ("sunken", background.clone()),
        ("accent_soft", background.clone()),
        ("selected", hex(colors.highlight)),
        ("on_selected", hex(colors.highlight_text)),
        ("accent", hex(colors.hot_light)),
        ("on_accent", background),
    ];
    let set: std::collections::BTreeMap<String, String> = roles.into_iter().map(|(key, value)| (key.to_string(), value)).collect();
    dshr_agent::ThemeDef { plugin: String::new(), id: "system-high-contrast".into(), name: String::new(), light: set.clone(), dark: set }
}

/// （插件快照，按它算出的要隐藏的入口）。
type HiddenCache = (Arc<dshr_context::Snapshot>, Arc<std::collections::HashSet<String>>);

pub struct Backend {
    runtime: Runtime,
    paths: Paths,
    /// `config.toml` 里的内容（不含环境变量覆盖），设置页修改后写回。
    config: RwLock<Config>,
    demo: bool,
    /// 所有模型请求共用的连接池：每轮对话不必重新建连接、重新握手。只在 `runtime` 里使用。
    http: HttpClient,
    /// 主程序装配的插件：工具、协议实现等都从这里取。
    plugins: Plugins,
    /// 会话列表缓存：刷新时只重读有变化的日志。
    catalog: SessionCatalog,
    /// 各提供商解析出的 API Key（`None` 表示查过、没有）。读系统钥匙串可能要几十毫秒甚至弹出授权框，
    /// 不该在每轮开始时都在界面线程上做一次；在设置里保存或删除 Key 时失效。
    api_keys: Mutex<HashMap<String, Option<String>>>,
    /// DeepSeek 账号的登录令牌（外层 `None` 表示还没读过钥匙串）。和 `api_keys` 一样：启动时判断有没有可用的
    /// 提供商、每轮开始、设置页每次绘制都要问，不能每次都去读钥匙串。登录、退出时失效（[`Self::forget_deepseek_key`]）。
    account_token: Mutex<Option<Option<String>>>,
    /// 插件目录里的 JS 插件（[Self::with_scripts]）。
    scripts: Option<JsPlugins>,
    /// `config.toml` 里配置的 MCP 服务器（[Self::with_mcp]）：设置页看状态、重连；改了配置就地生效。
    mcp: Option<dshr_mcp::McpPlugin>,
    /// 插件索引里有新版本的插件：id → 新版本号（[`Self::check_plugin_updates`] 填）。
    plugin_updates: Mutex<HashMap<String, String>>,
    /// [`Self::hidden_entries`] 的缓存：快照和由它算出的结果。
    hidden: Mutex<Option<HiddenCache>>,
}

/// 一轮对话使用的模型选项（输入框里选的）。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOptions {
    pub model: ModelRef,
    /// 思考等级；`None` 不发送，用模型的默认值。
    pub effort: Option<ReasoningEffort>,
    pub permission: PermissionMode,
    pub preset: Preset,
    /// 选中的插件预设（优先于 `preset`）。
    pub plugin_preset: Option<Arc<dshr_agent::PresetDef>>,
}

/// 模型菜单里的一项。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelChoice {
    pub reference: ModelRef,
    pub label: String,
    pub provider_name: String,
}

/// 提供商的凭据状态，供设置页显示。
#[derive(Debug, Clone, PartialEq)]
pub struct KeyStatus {
    /// 生效中的 Key（掩码）与来源。
    pub active: Option<(String, KeySource)>,
    /// 适配器约定的环境变量是否已设置。
    pub env_present: bool,
}

impl Backend {
    /// 建后台运行时。主程序在装插件、开窗口之前就建好它（后台任务不等界面线程）。
    pub fn make_runtime() -> Result<Runtime, String> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("dshr-agent")
            .enable_all()
            .build()
            .map_err(|error| format!("无法启动后台运行时: {error}"))
    }

    /// 主程序已经读过配置、建好运行时时用它装配后端，不重复读盘。
    pub fn from_parts(plugins: Plugins, paths: Paths, config: Config, runtime: Runtime) -> Self {
        let demo = std::env::var("DSHR_PROVIDER").is_ok_and(|value| value == "demo");
        Self {
            runtime,
            paths,
            config: RwLock::new(config),
            demo,
            http: HttpClient::default(),
            plugins,
            catalog: SessionCatalog::default(),
            api_keys: Mutex::default(),
            account_token: Mutex::default(),
            scripts: None,
            mcp: None,
            plugin_updates: Mutex::default(),
            hidden: Mutex::default(),
        }
    }

    /// 主程序装配的 MCP 插件（和插件容器里的是同一组服务器）。
    pub fn with_mcp(mut self, mcp: dshr_mcp::McpPlugin) -> Self {
        self.mcp = Some(mcp);
        self
    }

    /// 输入框 `@` 提及的来源：内置的工作区文件，加上插件（JS 插件的 `mentions`、MCP 服务器的资源）提供的。
    pub fn mention_sources(&self) -> Vec<Arc<dyn dshr_agent::MentionSource>> {
        let mut sources: Vec<Arc<dyn dshr_agent::MentionSource>> = vec![Arc::new(dshr_agent::FileMentions)];
        sources.extend(self.plugins.snapshot().items::<dshr_agent::MentionSources>().iter().cloned());
        sources
    }

    /// MCP 服务器现在的状态。
    pub fn mcp_status(&self) -> Vec<dshr_mcp::ServerStatus> {
        self.mcp.as_ref().map(dshr_mcp::McpPlugin::status).unwrap_or_default()
    }

    /// 手动重连一个 MCP 服务器（在后台连）。
    pub fn mcp_reconnect(&self, name: &str) -> Result<(), String> {
        let mcp = self.mcp.clone().ok_or("没有 MCP 插件")?;
        let name = name.to_owned();
        let (answer, answered) = std::sync::mpsc::channel();
        self.runtime.spawn(async move {
            let _ = answer.send(mcp.servers().reconnect(&name));
        });
        answered.recv().map_err(|_| "后台运行时没有回应".to_owned())?
    }

    /// 登录一个远程 MCP 服务器：在后台找授权服务器、打开系统浏览器，等用户登录完（最多 5 分钟）。在后台线程上调用。
    pub fn mcp_login(&self, name: &str) -> Result<(), String> {
        let mcp = self.mcp.clone().ok_or("没有 MCP 插件")?;
        let opener = dshr_plugins::app::app_services().open_url.ok_or("这个程序不能打开浏览器")?;
        let name = name.to_owned();
        self.runtime.block_on(async move { mcp.servers().login(&name, &*opener).await })
    }

    /// 退出一个远程 MCP 服务器的登录。
    pub fn mcp_logout(&self, name: &str) -> Result<(), String> {
        let mcp = self.mcp.clone().ok_or("没有 MCP 插件")?;
        let name = name.to_owned();
        let (answer, answered) = std::sync::mpsc::channel();
        self.runtime.spawn(async move {
            let _ = answer.send(mcp.servers().logout(&name));
        });
        answered.recv().map_err(|_| "后台运行时没有回应".to_owned())?
    }

    /// 一个 MCP 服务器最近的日志（它的 stderr，以及连接、断开的记录）。
    pub fn mcp_logs(&self, name: &str) -> Vec<String> {
        self.mcp.as_ref().map(|mcp| mcp.servers().logs(name)).unwrap_or_default()
    }

    /// 把文件里的 `[plugins.mcp]` 交给 MCP 插件：新增的连、删掉的断、改了的重连，不用重启。
    fn apply_mcp_config(&self) {
        let Some(mcp) = self.mcp.clone() else { return };
        let config = self.config.read().expect("配置锁中毒");
        let value = config.plugin_configs().into_iter().find(|(id, _)| id == "mcp").map(|(_, value)| value);
        let servers = value.map(|value| dshr_mcp::Config::from_value(value).servers).unwrap_or_default();
        mcp.servers().configure(servers);
        // 在后台先连上，下一步就有新服务器的工具。
        self.runtime.spawn(async move { mcp.prewarm().await });
    }

    /// 配置文件被别处改了（插件的 `host.dshr.config.patch`、`host.dshr.mcp.*`）：重新读一遍，MCP 服务器随之生效。
    pub fn reload_config(&self) {
        match Config::load(&self.paths) {
            Ok(config) => *self.config.write().expect("配置锁中毒") = config,
            Err(error) => {
                tracing::warn!(%error, "重新读配置失败");
                return;
            }
        }
        self.apply_mcp_config();
    }

    pub fn is_demo(&self) -> bool {
        self.demo
    }

    /// 运行时生效的配置（叠加了环境变量）。
    pub fn config(&self) -> Config {
        self.file_config().with_env()
    }

    pub fn config_path(&self) -> PathBuf {
        self.paths.config_file()
    }

    /// 数据目录（`~/.dshr`）：全局的 `AGENTS.md` 和 `skills/` 在这里。
    pub fn home(&self) -> &Path {
        &self.paths.home
    }

    /// 主程序装配的插件，按注册顺序：状态和配置的 schema。
    pub fn plugins(&self) -> Vec<PluginInfo> {
        self.plugins.describe()
    }

    /// 插件现在的状态；没有这个插件时是 `None`。
    pub fn plugin_status(&self, id: &str) -> Option<PluginStatus> {
        self.plugins.status(id)
    }

    /// 插件目录里的 JS 插件，给设置页授权、看热重载结果用。主程序装配完插件后交进来；没有时这些功能不显示。
    pub fn with_scripts(mut self, scripts: JsPlugins) -> Self {
        self.scripts = Some(scripts);
        self
    }

    /// 等用户允许能力的 JS 插件。
    pub fn grant_requests(&self) -> Vec<GrantRequest> {
        self.scripts.as_ref().map(JsPlugins::grant_requests).unwrap_or_default()
    }

    /// 允许插件 `id` 清单里声明的能力，随即重载它。
    pub fn grant_plugin(&self, id: &str) -> Result<(), String> {
        let scripts = self.scripts.as_ref().ok_or_else(|| "没有 JS 插件".to_owned())?;
        scripts.grant(id).map(|_| ())
    }

    /// 开关插件：写进 `config.toml`；JS 插件同时就地启停，不用重启（内置插件要重启才生效）。
    pub fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        self.update_config(|config| config.set_plugin_enabled(id, enabled))?;
        if self.script_ids().contains(&id) {
            let scripts = self.scripts.as_ref().ok_or_else(|| "没有 JS 插件".to_owned())?;
            scripts.set_enabled(id, enabled)?;
        }
        Ok(())
    }

    /// 插件给界面加的入口和页面（`dshr_agent::UiSurfaces`），按装配顺序。每次取当前快照：热重载之后就是新的。
    pub fn surfaces(&self) -> Vec<Arc<dyn dshr_agent::UiSurface>> {
        self.plugins.snapshot().items::<dshr_agent::UiSurfaces>().to_vec()
    }

    /// 插件 `plugin` 的界面扩展。
    pub fn surface(&self, plugin: &str) -> Option<Arc<dyn dshr_agent::UiSurface>> {
        self.surfaces().into_iter().find(|surface| surface.plugin() == plugin)
    }

    /// 插件要藏起来的内置入口（`settings:models`、`composer:presets` 等）。按快照缓存：界面每次绘制都会问。
    pub fn hidden_entries(&self) -> Arc<std::collections::HashSet<String>> {
        let snapshot = self.plugins.snapshot();
        let mut cache = self.hidden.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached, hidden)) = &*cache
            && Arc::ptr_eq(cached, &snapshot)
        {
            return hidden.clone();
        }
        let hidden: Arc<std::collections::HashSet<String>> = Arc::new(
            snapshot
                .items::<dshr_agent::UiSurfaces>()
                .iter()
                .flat_map(|surface| surface.hidden().iter().cloned())
                .filter(|target| dshr_agent::hideable(target))
                .collect(),
        );
        *cache = Some((snapshot, hidden.clone()));
        hidden
    }

    /// 插件的组成变了（装上、卸下、热重载）时收到通知。
    pub fn plugin_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.plugins.subscribe()
    }

    /// 插件的组成变了没有（装上、卸下、热重载都会换快照）：收到 [`Self::plugin_changes`] 的通知后比一次，变了就刷新入口。
    pub fn plugins_generation(&self) -> usize {
        Arc::as_ptr(&self.plugins.snapshot()) as *const () as usize
    }

    /// 主程序交进来了 JS 插件（[`Self::with_scripts`]）。
    pub fn has_scripts(&self) -> bool {
        self.scripts.is_some()
    }

    /// 插件目录里现在装着的 JS 插件的 id。
    pub fn script_ids(&self) -> Vec<&'static str> {
        self.scripts.as_ref().map(JsPlugins::ids).unwrap_or_default()
    }

    /// JS 插件所在的目录（`<数据目录>/plugins`）。
    pub fn plugins_dir(&self) -> PathBuf {
        self.paths.plugins_dir()
    }

    /// 在插件目录里生成一个新插件的骨架（和 `dshr plugin new <id>` 一样，不带能力）。
    /// 热重载会在一秒左右之后把它装上。
    pub fn create_plugin(&self, id: &str) -> Result<PathBuf, String> {
        dshr_plugins::Scaffold::new(id).create(&self.paths.plugins_dir()).map_err(|error| format!("{error:#}"))
    }

    /// 从本地安装包（`.dshrplugin`、`.zip`、`.tar.gz`）或插件文件夹安装：暂存（包签了名就验签）、问用户
    /// （和插件替用户装插件时同一个确认框）、装进插件目录，随即重载。
    /// 会等用户回答，要在后台线程上调用。返回装好的插件 id。
    pub fn install_plugin(&self, source: &Path) -> Result<String, String> {
        let staged = if source.is_file() {
            dshr_plugins::install::stage_package(source)?
        } else {
            dshr_plugins::install::stage_local(source, None)?
        };
        self.confirm_install(staged, dshr_plugins::install::CommitOptions::default())
    }

    /// 配置的插件索引地址（`[plugin_index] urls`）。
    pub fn plugin_index_urls(&self) -> Vec<String> {
        self.file_config().plugin_index.urls
    }

    /// 查一次插件索引，记下哪些已装插件有新版本。没有配置索引时什么都不做。在 tokio 运行时上调用。
    pub async fn check_plugin_updates(&self) {
        let urls = self.plugin_index_urls();
        if urls.is_empty() {
            return;
        }
        let (entries, problems) = dshr_plugins::index::fetch(&urls).await;
        for problem in problems {
            tracing::warn!(problem, "插件索引取不到");
        }
        let installed: Vec<(String, Option<String>)> = dshr_plugins::discover(&self.paths.plugins_dir())
            .into_iter()
            .map(|found| (found.id, found.manifest.ok().and_then(|manifest| manifest.version)))
            .collect();
        let stale = dshr_plugins::index::outdated(&entries, &installed);
        *self.plugin_updates.lock().expect("更新锁中毒") = stale.into_iter().map(|(id, _, latest)| (id, latest)).collect();
    }

    /// 索引里有、比装着的新的版本号。
    pub fn plugin_update(&self, id: &str) -> Option<String> {
        self.plugin_updates.lock().expect("更新锁中毒").get(id).cloned()
    }

    /// 归档的旧版本（新的在前），回滚用。
    pub fn plugin_versions(&self, id: &str) -> Vec<String> {
        dshr_plugins::sources::versions(&self.paths.plugins_dir(), id).into_iter().map(|archived| archived.version).collect()
    }

    /// 在插件索引里找插件。会联网，要在后台线程上调用。
    pub fn browse_plugins(&self, query: &str) -> Result<Vec<dshr_plugins::index::IndexEntry>, String> {
        let urls = self.plugin_index_urls();
        if urls.is_empty() {
            return Err("没有配置插件索引".into());
        }
        let (entries, problems) = self.runtime.block_on(dshr_plugins::index::fetch(&urls));
        if entries.is_empty() && !problems.is_empty() {
            return Err(problems.join("；"));
        }
        Ok(dshr_plugins::index::search(&entries, query).into_iter().cloned().collect())
    }

    /// 按索引安装或更新一个插件：下载、对 sha256、验发布者，弹出确认，同意后装上。会等用户回答，在后台线程上调用。
    pub fn install_from_index(&self, id: &str) -> Result<String, String> {
        let urls = self.plugin_index_urls();
        let (entries, _) = self.runtime.block_on(dshr_plugins::index::fetch(&urls));
        let entry = entries.into_iter().find(|entry| entry.id == id).ok_or_else(|| format!("索引里没有 {id}"))?;
        let version = entry.best_version().cloned().ok_or_else(|| format!("索引里的 {id} 没有适合当前 dshr 的版本"))?;
        let staged = self.runtime.block_on(dshr_plugins::index::stage(&entry, &version))?;
        let options = dshr_plugins::install::CommitOptions { force: false, index: Some(entry.index.clone()) };
        let id = self.confirm_install(staged, options)?;
        self.plugin_updates.lock().expect("更新锁中毒").remove(&id);
        Ok(id)
    }

    /// 回滚到最近归档的旧版本（同样确认）。在后台线程上调用。
    pub fn rollback_plugin(&self, id: &str) -> Result<String, String> {
        let plugins_dir = self.paths.plugins_dir();
        let archived = dshr_plugins::sources::rollback_source(&plugins_dir, id, None)?;
        let mut staged = dshr_plugins::install::stage_local(&archived.dir, Some(id))?;
        staged.source = format!("rollback:{}", archived.version);
        self.confirm_install(staged, dshr_plugins::install::CommitOptions { force: true, index: None })?;
        Ok(archived.version)
    }

    /// 弹出安装确认（和插件替用户装插件时同一个），同意后装上并重载。
    /// 用户在确认框里明确选了“信任新发布者并安装”时，发布者变了也装。
    fn confirm_install(
        &self,
        staged: dshr_plugins::install::Staged,
        mut options: dshr_plugins::install::CommitOptions,
    ) -> Result<String, String> {
        use dshr_plugins::app::InstallAnswer;
        let plugins_dir = self.paths.plugins_dir();
        let request = staged.request(&plugins_dir, None);
        let prompt = dshr_plugins::app::app_services().install_prompt.ok_or("这个程序不能确认安装")?;
        match prompt(&request) {
            InstallAnswer::Deny => return Err(format!("没有安装 {}", request.id)),
            InstallAnswer::Allow => {}
            InstallAnswer::AllowNewPublisher => options.force = true,
        }
        let id = staged.manifest.id.clone();
        staged.commit_with(&plugins_dir, options)?;
        if let Some(scripts) = &self.scripts {
            scripts.reload();
        }
        Ok(id)
    }

    /// 取消链接一个开发中的插件（文件夹不动），随即重载。
    pub fn unlink_plugin(&self, id: &str) -> Result<(), String> {
        dshr_plugins::links::unlink(&self.paths.plugins_dir(), id)?;
        if let Some(scripts) = &self.scripts {
            scripts.reload();
        }
        Ok(())
    }

    /// 删除（卸载）一个 JS 插件：删掉插件文件夹，连同安装来源和授权记录一起收回，随即重载。
    /// 链接的开发中插件只取消链接，外面的文件夹留着。
    pub fn uninstall_plugin(&self, id: &str) -> Result<(), String> {
        let plugins_dir = self.paths.plugins_dir();
        if plugins_dir.join(id).is_dir() {
            dshr_plugins::install::uninstall(&plugins_dir, id)?;
        } else {
            dshr_plugins::links::unlink(&plugins_dir, id)?;
        }
        if let Some(scripts) = &self.scripts {
            scripts.reload();
        }
        Ok(())
    }

    /// JS 插件 `id` 最近的日志，旧的在前。
    pub fn plugin_logs(&self, id: &str) -> Vec<dshr_plugins::LogLine> {
        self.scripts.as_ref().map(|scripts| scripts.logs(id)).unwrap_or_default()
    }

    /// 连续失败、这次运行里被停下的 JS 插件：(id, 原因)。
    pub fn stopped_plugins(&self) -> Vec<(String, String)> {
        self.scripts.as_ref().map(JsPlugins::stopped).unwrap_or_default()
    }

    /// 热重载时新版本加载不了、仍在用旧版本的 JS 插件：(id, 原因)。
    pub fn kept_plugins(&self) -> Vec<(String, String)> {
        self.scripts.as_ref().map(JsPlugins::kept).unwrap_or_default()
    }

    /// 插件自定义事件在对话区显示的一行字（[`dshr_agent::EventLabels`]）；没有插件负责这个类型时是 `None`。
    /// 每次取当前快照：热重载换上的新版本立刻用上。
    pub fn event_label(&self, kind: &str, data: &serde_json::Value) -> Option<String> {
        let snapshot = self.plugins.snapshot();
        let label = snapshot.items::<dshr_agent::EventLabels>().iter().find(|label| label.kind() == kind)?.clone();
        label.label(data)
    }

    /// 插件事件在对话区画成的节点树（JS 插件 `events` 里的 `render`）；没有插件负责、或画不出来时是 `None`。
    pub fn event_tree(&self, kind: &str, data: &serde_json::Value) -> Option<serde_json::Value> {
        let surface = self.surfaces().into_iter().find(|surface| surface.event_kinds().iter().any(|known| known == kind))?;
        surface.render_event(kind, data).inspect_err(|error| tracing::warn!(kind, error, "插件没能画出这条事件")).ok()
    }

    /// 插件给自己的工具画的卡片（JS 插件的 `cards`）；没有、或画不出来时是 `None`。
    pub fn tool_card_view(
        &self,
        tool: &str,
        input: &serde_json::Value,
        output: Option<&str>,
        is_error: bool,
    ) -> Option<dshr_agent::ToolCardView> {
        let surface = self.surfaces().into_iter().find(|surface| surface.card_tools().iter().any(|known| known == tool))?;
        surface.card(tool, input, output, is_error).inspect_err(|error| tracing::warn!(tool, error, "插件没能画出工具卡片")).ok()
    }

    /// 默认模型（新会话用的那个）：插件在页面动作、命令里调用 `host.llm` 时用。连不上（没有 Key）时是 `None`。
    pub fn default_llm(&self) -> Option<dshr_agent::LlmAccess> {
        let model = self.config().default_model()?;
        self.llm_for(&model)
    }

    /// 连上 `provider/model`（插件的 `host.llm.complete` 指定别的模型时用）。提供商没配置、没有 Key 时是 `None`。
    pub fn llm_for(&self, model: &dshr_config::ModelRef) -> Option<dshr_agent::LlmAccess> {
        let config = self.config();
        let mut resolved = config.provider(&model.provider)?;
        // 账号登录的令牌是推理凭据，放在 x-dsh-auth-token 头里（原版行为）；没有账号时退回 API Key。
        let key = self
            .account_token_for(&resolved)
            .inspect(|token| {
                resolved.extra_headers.push((account::ACCOUNT_TOKEN_HEADER.into(), token.clone()));
            })
            .or_else(|| self.api_key(&resolved));
        let provider = dshr_llm::connect(&self.plugins.snapshot(), &self.http, &resolved, key).ok()?;
        Some(dshr_agent::LlmAccess { provider, model: model.model.clone() })
    }

    /// 能选的配色方案：内置的高对比度、数据目录 `themes/` 里的（`<id>.toml`：`name`、`[light]`、`[dark]`），加上插件带的。
    pub fn themes(&self) -> Vec<dshr_agent::ThemeDef> {
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct File {
            name: String,
            light: std::collections::BTreeMap<String, String>,
            dark: std::collections::BTreeMap<String, String>,
        }
        // 内置的高对比度配色不在这里：它只由“辅助功能 → 高对比度”开关，不出现在配色方案里。
        let mut themes = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.paths.home.join("themes")) {
            for path in entries.flatten().map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|ext| ext == "toml")) {
                let id = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
                match std::fs::read_to_string(&path)
                    .map_err(|error| error.to_string())
                    .and_then(|text| toml::from_str::<File>(&text).map_err(|error| error.to_string()))
                {
                    Ok(file) => themes.push(dshr_agent::ThemeDef {
                        plugin: String::new(),
                        name: if file.name.is_empty() { id.clone() } else { file.name },
                        id,
                        light: file.light,
                        dark: file.dark,
                    }),
                    Err(error) => tracing::warn!(path = %path.display(), error, "配色方案读不出来"),
                }
            }
        }
        for surface in self.surfaces() {
            themes.extend(surface.themes().iter().cloned());
        }
        themes.sort_by(|a, b| {
            let key = |theme: &dshr_agent::ThemeDef| {
                if theme.name.is_empty() { t!("settings.palette_high_contrast").to_string() } else { theme.name.clone() }
            };
            key(a).cmp(&key(b))
        });
        themes
    }

    /// 配色方案在配置里的写法：文件里的是 `<id>`，插件带的是 `<插件>/<id>`。
    pub fn theme_key(theme: &dshr_agent::ThemeDef) -> String {
        if theme.plugin.is_empty() { theme.id.clone() } else { format!("{}/{}", theme.plugin, theme.id) }
    }

    /// 插件提供的服务商（`providers` 导出），和它们有没有已经加进配置。
    pub fn plugin_providers(&self) -> Vec<(dshr_llm::PluginProviderInfo, bool)> {
        let config = self.file_config();
        self.plugins
            .snapshot()
            .items::<dshr_llm::ProviderAuths>()
            .iter()
            .map(|auth| {
                let info = auth.describe();
                let added = config.providers.iter().any(|provider| provider.plugin.as_deref() == Some(info.id.as_str()));
                (info, added)
            })
            .collect()
    }

    /// 把插件服务商加进配置（`adapter = "plugin"`）。模型列表之后在编辑表单里“获取可用模型”。
    pub fn add_plugin_provider(&self, info: &dshr_llm::PluginProviderInfo) -> Result<(), String> {
        let id = info.id.replace('/', "-");
        self.update_config(|config| {
            if config.providers.iter().any(|provider| provider.plugin.as_deref() == Some(info.id.as_str())) {
                return;
            }
            config.providers.push(dshr_config::ProviderConfig {
                id,
                adapter: dshr_config::PLUGIN_ADAPTER.into(),
                name: info.name.clone(),
                protocol: Some(info.protocol),
                base_url: info.base_url.clone(),
                models: Vec::new(),
                plugin: Some(info.id.clone()),
                prompt_cache: None,
            });
        })
    }

    /// 插件提供的预设（输入卡片的预设菜单里内置四项后面）。
    pub fn plugin_presets(&self) -> Vec<Arc<dshr_agent::PresetDef>> {
        self.plugins.snapshot().items::<dshr_agent::PluginPresets>().to_vec()
    }

    /// JS 插件的卡片信息（设置页“扩展”）；不是 JS 插件时是 `None`。
    pub fn plugin_card(&self, id: &str) -> Option<dshr_plugins::PluginCard> {
        self.scripts.as_ref().and_then(|scripts| scripts.card(id))
    }

    /// 全部 JS 插件的卡片，按 id。设置页一次取完：不对每个插件各读一遍插件目录（见 `JsPlugins::cards`）。
    pub fn plugin_cards(&self) -> HashMap<String, dshr_plugins::PluginCard> {
        self.scripts.as_ref().map(|scripts| scripts.cards().into_iter().map(|card| (card.id.clone(), card)).collect()).unwrap_or_default()
    }

    /// JS 插件 `id` 已经被允许的能力（审批面板上说明“修改它的代码会带着这些能力运行”时用）。
    pub fn plugin_granted(&self, id: &str) -> Vec<String> {
        self.scripts.as_ref().map(|scripts| scripts.granted(id)).unwrap_or_default()
    }

    /// 设置页“扩展”要显示的全部数据，一次收集好。要读插件目录、授权记录、归档的旧版本，
    /// 可能访问系统钥匙串：在后台线程上调用，界面线程只拿结果赋值。
    pub fn extensions_snapshot(&self) -> ExtensionsData {
        let grants = self.grant_requests();
        let cards = self
            .scripts
            .as_ref()
            .map(|scripts| scripts.cards_with(&grants).into_iter().map(|card| (card.id.clone(), card)).collect())
            .unwrap_or_default();
        let plugins_dir = self.paths.plugins_dir();
        let rollback = self
            .script_ids()
            .into_iter()
            .filter_map(|id| {
                let version = dshr_plugins::sources::versions(&plugins_dir, id).into_iter().next()?.version;
                Some((id.to_owned(), version))
            })
            .collect();
        ExtensionsData {
            config: self.file_config(),
            plugins: self.plugins(),
            grants,
            kept: self.kept_plugins(),
            stopped: self.stopped_plugins(),
            cards,
            rollback,
            mcp_status: self.mcp_status(),
        }
    }

    /// JS 插件声明的密钥和它们有没有设置：(名字, 已设置)。有没有设置走缓存，不必每次都去问系统钥匙串。
    pub fn plugin_secrets(&self, id: &str) -> Vec<(String, bool)> {
        self.plugin_card(id).map(|card| card.secrets).unwrap_or_default()
    }

    /// 保存（`value` 非空）或删除 JS 插件的一个密钥。
    pub fn set_plugin_secret(&self, id: &str, name: &str, value: &str) -> Result<(), String> {
        if value.is_empty() { dshr_plugins::secrets::delete(id, name) } else { dshr_plugins::secrets::save(id, name, value) }
    }

    /// 只含 `config.toml` 的配置，供设置页编辑。
    pub fn file_config(&self) -> Config {
        self.config.read().expect("配置锁中毒").clone()
    }

    /// 界面设置（主题、语言、动效等）。渲染时用它，不必克隆整份配置。
    pub fn ui_config(&self) -> UiConfig {
        self.config.read().expect("配置锁中毒").ui.clone()
    }

    /// 侧边栏里记住的工作区，最近使用的在前。
    pub fn workspaces(&self) -> Vec<PathBuf> {
        self.config.read().expect("配置锁中毒").ui.workspaces.clone()
    }

    /// 修改配置并写回 `config.toml`；写入失败时内存中的配置保持不变。
    pub fn update_config(&self, edit: impl FnOnce(&mut Config)) -> Result<(), String> {
        let mut config = self.config.write().expect("配置锁中毒");
        let mut next = config.clone();
        edit(&mut next);
        next.save(&self.paths).map_err(|error| error.to_string())?;
        *config = next;
        drop(config);
        // 提供商可能被改了（换了 ID、换了环境变量名），已解析的 Key 重新查。
        self.api_keys.lock().expect("Key 缓存锁中毒").clear();
        // MCP 服务器的增删、开关就地生效（没变的服务器不动）。
        self.apply_mcp_config();
        Ok(())
    }

    /// 提供商的 API Key（钥匙串优先，其次环境变量）。第一次用到时查一次并记住。
    fn api_key(&self, provider: &ResolvedProvider) -> Option<String> {
        let mut keys = self.api_keys.lock().expect("Key 缓存锁中毒");
        keys.entry(provider.id.clone()).or_insert_with(|| credentials::resolve_api_key(provider).map(|(key, _)| key)).clone()
    }

    /// DeepSeek 账号登录的令牌（`x-dsh-auth-token`，无 Bearer 前缀，API 网关认得）。
    /// 只有 `deepseek-oauth` 提供商用它：`deepseek`（API Key 提供商）与账号登录互不影响，
    /// 不会因为登录过账号就顶替用户配置的密钥。自定义 base_url 时不带（可能发给第三方）。
    fn account_token_for(&self, provider: &ResolvedProvider) -> Option<String> {
        if provider.id != dshr_config::providers::DEEPSEEK_OAUTH {
            return None;
        }
        let adapter = dshr_config::providers::adapter(dshr_config::providers::DEEPSEEK_OAUTH)?;
        if adapter.base_url != provider.base_url {
            return None;
        }
        self.account_token()
    }

    /// 钥匙串里的账号令牌，第一次用到时读一次并记住。
    fn account_token(&self) -> Option<String> {
        self.account_token.lock().expect("账号令牌缓存锁中毒").get_or_insert_with(account::account_token).clone()
    }

    fn forget_api_key(&self, provider_id: &str) {
        self.api_keys.lock().expect("Key 缓存锁中毒").remove(provider_id);
    }

    /// 用户改过的快捷键；文件损坏时当作没改过。
    pub fn keybindings(&self) -> Keybindings {
        Keybindings::load(&self.paths).unwrap_or_default()
    }

    pub fn save_keybindings(&self, bindings: &Keybindings) -> Result<(), String> {
        bindings.save(&self.paths).map_err(|error| error.to_string())
    }

    pub fn key_status(&self, provider: &ResolvedProvider) -> KeyStatus {
        KeyStatus {
            active: credentials::resolve_api_key(provider).map(|(key, source)| (credentials::mask(&key), source)),
            env_present: credentials::env_api_key(provider).is_some(),
        }
    }

    /// 同 [`credentials::is_ready`]，但 Key 走缓存（[`Self::api_key`]），不每次都读钥匙串。
    pub fn is_ready(&self, provider: &ResolvedProvider) -> bool {
        provider.id != dshr_config::providers::DEEPSEEK_OAUTH && (!provider.requires_key || self.api_key(provider).is_some())
    }

    /// 账号登录：在后台运行时上跑完整流程，`open_browser` 由界面提供（`cx.open_url`）。
    /// 成功后令牌存进系统钥匙串，并让 deepseek 提供商的凭据缓存失效。
    pub fn sign_in_deepseek(&self, open_browser: impl FnOnce(String) + Send + 'static) -> tokio::task::JoinHandle<Result<String, String>> {
        self.runtime.spawn_blocking(move || {
            let outcome = account::sign_in(move |url| open_browser(url.to_owned())).map_err(|error| error.to_string())?;
            account::save_grant(&outcome.grant)?;
            Ok(outcome.user.get("id").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned())
        })
    }

    /// 账号令牌刚保存过：下次解析凭据时重新读钥匙串。
    pub fn forget_deepseek_key(&self) {
        self.forget_api_key(DEEPSEEK);
        self.forget_api_key(dshr_config::providers::DEEPSEEK_OAUTH);
        *self.account_token.lock().expect("账号令牌缓存锁中毒") = None;
    }

    /// 是否已登录 DeepSeek 账号（钥匙串里有登录令牌）。
    pub fn is_signed_in(&self) -> bool {
        self.account_token().is_some()
    }

    /// 退出 DeepSeek 账号：删除钥匙串里的登录令牌，两个 DeepSeek 提供商的凭据缓存都失效。
    pub fn sign_out_deepseek(&self) {
        // 删除失败（钥匙串不可用）时本地缓存照样清掉，下次请求重新读。
        let _ = account::sign_out();
        self.forget_deepseek_key();
    }

    /// 账号登录成功后：确保配置里有 `deepseek-oauth` 提供商，并把默认模型切到它
    /// （保持模型 ID 不变：两个 DeepSeek 提供商的目录相同）。用户随后可以在设置里改回。
    pub fn prefer_oauth_provider(&self) -> Result<(), String> {
        self.update_config(|config| {
            if !config.providers.iter().any(|provider| provider.adapter == dshr_config::providers::DEEPSEEK_OAUTH) {
                let adapter = dshr_config::providers::adapter(dshr_config::providers::DEEPSEEK_OAUTH).expect("内置 DeepSeek OAuth 适配器");
                config.providers.push(dshr_config::ProviderConfig::from_adapter(adapter));
            }
            if let Some(reference) = config.default_model()
                && reference.provider == DEEPSEEK
                && config.provider(dshr_config::providers::DEEPSEEK_OAUTH).is_some()
            {
                config.model.default = ModelRef::new(dshr_config::providers::DEEPSEEK_OAUTH, &reference.model).to_string();
            }
        })
    }

    pub fn save_api_key(&self, provider_id: &str, key: &str) -> Result<(), String> {
        if key.trim().is_empty() {
            return Err(t!("error.empty_api_key").to_string());
        }
        self.forget_api_key(provider_id);
        credentials::save_api_key(provider_id, key).map_err(|error| error.to_string())
    }

    pub fn delete_api_key(&self, provider_id: &str) -> Result<(), String> {
        self.forget_api_key(provider_id);
        credentials::delete_api_key(provider_id).map_err(|error| error.to_string())
    }

    /// 是否有任何提供商可用（不需要 Key、已配置 Key，或已登录 DeepSeek 账号）。
    pub fn has_ready_provider(&self) -> bool {
        self.demo || self.config().providers().iter().any(|provider| self.is_ready(provider) || self.account_token_for(provider).is_some())
    }

    /// 启动时是否先显示欢迎窗：没有可用的提供商，且用户没有选过“稍后设置”。
    pub fn needs_welcome(&self) -> bool {
        !self.file_config().ui.welcome_skipped && !self.has_ready_provider()
    }

    /// 模型菜单：按提供商顺序列出目录里的模型。
    pub fn model_choices(&self) -> Vec<ModelChoice> {
        self.config()
            .providers()
            .into_iter()
            .flat_map(|provider| {
                provider
                    .models
                    .iter()
                    .map(move |model| ModelChoice {
                        reference: ModelRef::new(&provider.id, &model.id),
                        label: model.display_name().to_owned(),
                        provider_name: provider.name.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// 在后台运行时上请求模型列表。官方 DeepSeek OAuth 目录优先使用账号令牌。
    pub fn fetch_models(
        &self,
        provider: ResolvedProvider,
        api_key: Option<String>,
    ) -> tokio::task::JoinHandle<Result<Vec<dshr_config::ModelEntry>, String>> {
        let (plugins, http) = (self.plugins.snapshot(), self.http.clone());
        let token = if provider.plugin.is_none() && provider.models_url.0 == dshr_config::providers::DEEPSEEK_MODELS_URL {
            self.account_token_for(&provider)
        } else {
            None
        };
        self.runtime.spawn(async move {
            if let Some(token) = token {
                return dshr_llm::connect::list_deepseek_account_models(&token).await.map_err(|error| error.to_string());
            }
            let key = api_key.filter(|key| !key.trim().is_empty()).or_else(|| credentials::resolve_api_key(&provider).map(|(key, _)| key));
            dshr_llm::list_models(&plugins, &http, &provider, key.as_deref()).await.map_err(|error| error.to_string())
        })
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// 会话列表。要读磁盘（第一次会扫描所有日志），在后台线程调用。
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        self.catalog.list(&self.paths.sessions_dir())
    }

    /// 把目录记为最近使用的工作区（排到最前）。
    pub fn remember_workspace(&self, path: &Path) -> Result<(), String> {
        if self.config.read().expect("配置锁中毒").ui.workspaces.first().map(PathBuf::as_path) == Some(path) {
            return Ok(());
        }
        self.update_config(|config| {
            let workspaces = &mut config.ui.workspaces;
            workspaces.retain(|known| known != path);
            workspaces.insert(0, path.to_owned());
            workspaces.truncate(MAX_WORKSPACES);
        })
    }

    /// 从侧边栏移除工作区；它下面的会话不受影响，归到“其他”里。
    pub fn forget_workspace(&self, path: &Path) -> Result<(), String> {
        self.update_config(|config| config.ui.workspaces.retain(|known| known != path))
    }

    pub fn create_session(&self) -> Result<Session, String> {
        let id = uuid::Uuid::new_v4().to_string();
        Session::create(&id, self.paths.session_log(&id)).map_err(|error| error.to_string())
    }

    /// 打开会话要读、解析整份日志：返回一个可以放到后台线程上跑的任务，别在界面线程上直接调用。
    pub fn open_session(&self, id: &str) -> impl FnOnce() -> Result<Session, String> + Send + 'static {
        let (id, log) = (id.to_owned(), self.paths.session_log(id));
        move || Session::open(&id, log).map_err(|error| error.to_string())
    }

    pub fn rename_session(&self, id: &str, title: &str) -> Result<(), String> {
        dshr_agent::rename_session(&self.paths.sessions_dir(), id, title).map_err(|error| error.to_string())
    }

    /// 移到回收目录（`sessions/.trash`），不是彻底删除。
    pub fn delete_session(&self, id: &str) -> Result<(), String> {
        dshr_agent::delete_session(&self.paths.sessions_dir(), id).map_err(|error| error.to_string())
    }

    /// 从第 `turn` 轮结束处分叉，返回新会话 ID。
    /// 同 [`Self::open_session`]，返回放到后台跑的任务：要复制整份日志。
    pub fn fork_session(&self, id: &str, turn: u32) -> impl FnOnce() -> Result<String, String> + Send + 'static {
        let (dir, id) = (self.paths.sessions_dir(), id.to_owned());
        move || {
            let new_id = uuid::Uuid::new_v4().to_string();
            dshr_agent::fork_session(&dir, &id, turn, &new_id).map_err(|error| error.to_string())
        }
    }

    pub fn build_agent(&self, workspace: PathBuf, approver: Arc<dyn Approver>, options: &TurnOptions) -> Result<Agent, String> {
        let config = self.config();
        let provider: Arc<dyn Provider> = if self.demo {
            Arc::new(DemoProvider)
        } else {
            let mut resolved = config
                .provider(&options.model.provider)
                .ok_or_else(|| t!("error.unknown_provider", provider = options.model.provider).to_string())?;
            // 账号登录的令牌是推理凭据，放在 x-dsh-auth-token 头里（原版行为）；没有账号时退回 API Key。
            let key = self
                .account_token_for(&resolved)
                .inspect(|token| {
                    resolved.extra_headers.push((account::ACCOUNT_TOKEN_HEADER.into(), token.clone()));
                })
                .or_else(|| self.api_key(&resolved));
            dshr_llm::connect(&self.plugins.snapshot(), &self.http, &resolved, key).map_err(|error| match error {
                LlmError::MissingApiKey => t!("error.no_api_key", provider = resolved.name).to_string(),
                error => error.to_string(),
            })?
        };
        // 插件能切换到的模型（`model:route`）：当前提供商目录里的。
        let catalog = config.provider(&options.model.provider).map(|provider| provider.models).unwrap_or_default();
        let model_choices: Vec<String> = catalog.iter().map(|entry| entry.id.clone()).collect();
        let model = config.model;
        Ok(Agent::new(
            provider,
            self.plugins.clone(),
            approver,
            ToolContext::new(workspace),
            AgentConfig {
                model: options.model.model.clone(),
                effort: options.effort,
                context_window: model.context_window,
                ..AgentConfig::default()
            }
            // 规划模式只读，不管输入卡片上选的权限：预设插件会拦下写入与命令。
            .with(options.permission)
            .with(options.preset)
            .with(dshr_agent::ModelChoices(model_choices))
            .with_option(options.plugin_preset.clone().map(dshr_agent::PluginPreset)),
        ))
    }
}

/// 对话区向 JS 插件要事件、工具卡片的显示内容（见 [`crate::transcript::PluginViewResolver`]）。
/// 只在后台线程上调用会问插件的那几个方法：每次都要到插件的线程上去等它回答。
impl crate::transcript::PluginViewSource for Backend {
    fn handles_event(&self, kind: &str) -> bool {
        let snapshot = self.plugins.snapshot();
        snapshot.items::<dshr_agent::UiSurfaces>().iter().any(|surface| surface.event_kinds().iter().any(|known| known == kind))
            || snapshot.items::<dshr_agent::EventLabels>().iter().any(|label| label.kind() == kind)
    }

    fn handles_tool(&self, tool: &str) -> bool {
        self.plugins
            .snapshot()
            .items::<dshr_agent::UiSurfaces>()
            .iter()
            .any(|surface| surface.card_tools().iter().any(|known| known == tool))
    }

    fn event_tree(&self, kind: &str, data: &serde_json::Value) -> Option<serde_json::Value> {
        Backend::event_tree(self, kind, data)
    }

    fn event_label(&self, kind: &str, data: &serde_json::Value) -> Option<String> {
        Backend::event_label(self, kind, data)
    }

    fn tool_card(&self, tool: &str, input: &serde_json::Value, output: &str, is_error: bool) -> Option<dshr_agent::ToolCardView> {
        self.tool_card_view(tool, input, Some(output), is_error)
    }
}

/// 设置页“扩展”一次刷新要的全部数据（[`Backend::extensions_snapshot`]），在后台线程上收集。
pub struct ExtensionsData {
    pub config: Config,
    /// 全部插件和它们现在的状态，按注册顺序。
    pub plugins: Vec<PluginInfo>,
    /// 等授权的 JS 插件。
    pub grants: Vec<GrantRequest>,
    /// 新版本加载不了、仍在用旧版本的：(id, 原因)。
    pub kept: Vec<(String, String)>,
    /// 连续失败被停下的：(id, 原因)。
    pub stopped: Vec<(String, String)>,
    /// JS 插件的卡片，按 id。
    pub cards: HashMap<String, dshr_plugins::PluginCard>,
    /// 可以回滚到的最新旧版本，按 id；没有归档的不在里面。
    pub rollback: HashMap<String, String>,
    pub mcp_status: Vec<dshr_mcp::ServerStatus>,
}

/// 一条待用户处理的审批。
pub struct PendingApproval {
    pub call_id: String,
    pub tool: String,
    /// 附在审批处的说明（自动审查认为有风险的理由；必须确认时是宿主给的说明）。
    pub note: Option<String>,
    /// 必须由用户亲自确认（`Gate::Require`）：不能“本会话不再询问”，之前选过的也不算数。
    pub mandatory: bool,
    /// 工具调用的参数：审批面板据此把要授予的插件能力写成人能读懂的说法。
    pub input: std::sync::Arc<serde_json::Value>,
    pub respond: oneshot::Sender<Decision>,
}

/// 把审批请求转发给界面，等待用户点击。界面关闭或会话结束时视为拒绝。
pub struct UiApprover {
    pub tx: mpsc::UnboundedSender<PendingApproval>,
}

#[async_trait::async_trait]
impl Approver for UiApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        let (respond, decision) = oneshot::channel();
        let pending = PendingApproval {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            note: request.note.clone(),
            mandatory: request.mandatory,
            input: request.input.clone(),
            respond,
        };
        if self.tx.send(pending).is_err() {
            return Decision::Deny;
        }
        decision.await.unwrap_or(Decision::Deny)
    }
}

/// 演示用 Provider（`DSHR_PROVIDER=demo`）：不联网，依次列目录、运行命令（触发审批）、
/// 再流式输出一段 Markdown，用来调界面。
struct DemoProvider;

#[async_trait::async_trait]
impl Provider for DemoProvider {
    fn id(&self) -> &str {
        "demo"
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        // 不带工具的请求是标题生成。
        if request.tools.is_empty() {
            return Ok(Box::pin(futures::stream::iter(streamed_text(None, "演示会话").into_iter().map(Ok))));
        }
        // 本轮已经走了几步：最后一条用户文本之后的 assistant 消息数。
        let start = request
            .messages
            .iter()
            .rposition(|message| {
                message.role == Role::User && message.content.iter().any(|block| matches!(block, ContentBlock::Text { .. }))
            })
            .unwrap_or(0);
        let steps = request.messages[start..].iter().filter(|message| message.role == Role::Assistant).count();
        let call_id = format!("demo_{}_{steps}", request.messages.len());
        let events = match steps {
            0 => ScriptedProvider::tool_reply(
                &call_id,
                "update_plan",
                json!({"steps": [
                    {"title": "查看工作区", "status": "completed"},
                    {"title": "写演示文件", "status": "in_progress"},
                    {"title": "运行一条命令", "status": "pending"}
                ]}),
            ),
            1 => ScriptedProvider::tool_reply(&call_id, "list_dir", json!({"path": "."})),
            2 => ScriptedProvider::tool_reply(
                &call_id,
                "write_file",
                json!({"path": "dshr-demo.md", "content": "# dshr 演示\n\n这个文件由演示模型写入。\n"}),
            ),
            3 => ScriptedProvider::tool_reply(&call_id, "run_shell", json!({"command": "echo hello from dshr"})),
            _ => streamed_text(
                (request.effort.is_some_and(|effort| effort != ReasoningEffort::Off)).then_some("工具已全部执行完毕，整理结果后回复。"),
                &format!(
                    "演示回复（模型：`{}`）：我先**列出了工作区**，写了一个演示文件，然后在你批准后**运行了一条命令**。\n\n\
                     - 工具卡片可以点击展开输出，文件改动显示为 diff\n- 运行命令前会请求审批\n- 回复支持 `Markdown` 与代码块\n\n\
                     ```rust\nfn main() {{\n    println!(\"hello from dshr\");\n}}\n```",
                    request.model
                ),
            ),
        };
        let paced = futures::stream::iter(events).then(|event| async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok(event)
        });
        Ok(Box::pin(paced))
    }
}

/// 把（可选的）思考与正文切成多个增量，模拟逐字输出。
fn streamed_text(thinking: Option<&str>, text: &str) -> Vec<StreamEvent> {
    fn pieces(text: &str) -> Vec<String> {
        text.chars().collect::<Vec<_>>().chunks(6).map(|chunk| chunk.iter().collect()).collect()
    }

    let mut events = vec![StreamEvent::MessageStart { model: Some("demo".into()), input_tokens: Some(1) }];
    let mut index = 0;
    if let Some(thinking) = thinking {
        events.push(StreamEvent::BlockStart { index, kind: BlockKind::Thinking });
        events.extend(pieces(thinking).into_iter().map(|thinking| StreamEvent::ThinkingDelta { index, thinking }));
        events.push(StreamEvent::BlockStop { index });
        index += 1;
    }
    events.push(StreamEvent::BlockStart { index, kind: BlockKind::Text });
    events.extend(pieces(text).into_iter().map(|text| StreamEvent::TextDelta { index, text }));
    events.push(StreamEvent::BlockStop { index });
    events.push(StreamEvent::MessageDelta { stop_reason: Some(StopReason::EndTurn), output_tokens: Some(1) });
    events.push(StreamEvent::MessageStop);
    events
}
