//! 插件包的激活：把插件文件夹里的 Skills、命令、预设、MCP 服务器接到内核上（见 `crate::bundle`）。
//! 有没有脚本都一样；没有脚本的“纯包”插件不启动 QuickJS。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use dshr_agent::{CommandEntry, CommandResult, SlashEntry, ThemeDef, ToolCardView, UiEntry, UiSurface, UiSurfaces};
use dshr_context::Scope;
use dshr_tools::{Tool, ToolSource, ToolSources};
use serde_json::Value;

use crate::bundle::BundleCommand;
use crate::manifest::PluginManifest;

/// 测试里换成内存假服务器。
#[cfg(test)]
pub(crate) static TEST_CONNECTOR: std::sync::OnceLock<Arc<dyn dshr_mcp::Connector>> = std::sync::OnceLock::new();

/// 把包里的东西贡献出去。`surface` 是脚本的界面扩展（有的话），命令要和它合成一个。
pub(super) fn apply(cx: &mut Scope, plugin: &'static str, manifest: &PluginManifest, surface: Option<Arc<dyn UiSurface>>) {
    let bundle = &manifest.bundle;
    if let Some(dir) = &bundle.skills {
        cx.contribute::<dshr_skills::SkillRoots>(Arc::new(dshr_skills::SkillRoot { plugin: plugin.to_owned(), dir: dir.clone() }));
        // 模型按提示词自己去读这些 Skill：不在工作区里也不用每次询问。
        cx.contribute::<dshr_tools::ReadableRoots>(dir.clone());
    }
    for preset in &bundle.presets {
        cx.contribute::<dshr_agent::PluginPresets>(Arc::new(dshr_agent::PresetDef {
            plugin: plugin.to_owned(),
            id: preset.id.clone(),
            label: preset.label.clone(),
            description: preset.description.clone(),
            instructions: preset.instructions.clone(),
            read_only: preset.read_only,
            tools: preset.tools.clone(),
        }));
    }
    let surface = match (surface, bundle.commands.is_empty()) {
        (surface, true) => surface,
        (inner, false) => Some(Arc::new(BundleSurface::new(plugin, &bundle.commands, inner)) as Arc<dyn UiSurface>),
    };
    if let Some(surface) = surface {
        cx.contribute::<UiSurfaces>(surface);
    }
    if !bundle.mcp_servers.is_empty() {
        let servers = servers(plugin, manifest);
        servers.set.configure(bundle.mcp_servers.clone());
        cx.contribute::<ToolSources>(Arc::new(BundleTools(servers)));
    }
}

/// 一个插件自带的那组 MCP 服务器。主代理和子代理（以及热重载前后的两个版本）共用一组：只要还有人用就留着，
/// 没人用了（插件停用、卸掉）就全部断开，进程随之结束。
struct SharedServers {
    set: dshr_mcp::ServerSet,
}

impl Drop for SharedServers {
    fn drop(&mut self) {
        self.set.configure(BTreeMap::new());
    }
}

static SERVERS: LazyLock<Mutex<HashMap<String, Weak<SharedServers>>>> = LazyLock::new(Mutex::default);

/// 插件 `plugin` 的那组服务器：还有人用就是同一组（热重载时没变的服务器不用重启），否则新建一组。
fn servers(plugin: &'static str, manifest: &PluginManifest) -> Arc<SharedServers> {
    let mut all = SERVERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    all.retain(|_, servers| servers.strong_count() > 0);
    let shared = match all.get(plugin).and_then(Weak::upgrade) {
        Some(shared) => shared,
        None => {
            #[cfg(test)]
            let set = match TEST_CONNECTOR.get() {
                Some(connector) => dshr_mcp::ServerSet::with_connector(Some(plugin.to_owned()), connector.clone()),
                None => dshr_mcp::ServerSet::new(Some(plugin.to_owned())),
            };
            #[cfg(not(test))]
            let set = dshr_mcp::ServerSet::new(Some(plugin.to_owned()));
            let shared = Arc::new(SharedServers { set });
            all.insert(plugin.to_owned(), Arc::downgrade(&shared));
            shared
        }
    };
    // 每次激活都按现在的清单换一次占位符的取法（文件夹、密钥名单可能变了）。
    let (dir, data, secrets) = (manifest.dir.clone(), super::surface::data_dir(manifest), manifest.secrets.clone());
    shared.set.set_resolver(Arc::new(move |server, kind, name| match kind {
        "plugin" => Ok(dir.display().to_string()),
        "data" => {
            std::fs::create_dir_all(&data).map_err(|error| format!("建不了插件数据目录：{error}"))?;
            Ok(data.display().to_string())
        }
        "env" => std::env::var(name).map_err(|_| format!("环境变量 {name} 没有设置")),
        "secret" if secrets.iter().any(|declared| declared == name) => crate::secrets::load(plugin, name).ok_or_else(|| {
            format!("MCP 服务器 {server} 要用的密钥 {name} 还没有填：在“设置 → 扩展”里这个插件下面填，或运行 `dshr plugin secret set {plugin} {name}`")
        }),
        other => Err(format!("不认识的占位符 `${{{other}}}`")),
    }));
    shared
}

/// 插件自带的服务器的工具。
struct BundleTools(Arc<SharedServers>);

#[async_trait::async_trait]
impl ToolSource for BundleTools {
    async fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.0.set.tools().await
    }
}

/// 插件自带的服务器现在的状态（设置页、`dshr plugin list`）。
pub(crate) fn server_status(plugin: &str) -> Vec<dshr_mcp::ServerStatus> {
    let all = SERVERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    all.get(plugin).and_then(Weak::upgrade).map(|shared| shared.set.status()).unwrap_or_default()
}

/// 包里的斜杠命令（`/<插件>:<名字>`），加上脚本的界面扩展（有的话）：两者在界面上是同一个插件的同一个 surface。
struct BundleSurface {
    plugin: &'static str,
    commands: Vec<BundleCommand>,
    slash: Vec<SlashEntry>,
    inner: Option<Arc<dyn UiSurface>>,
}

impl BundleSurface {
    fn new(plugin: &'static str, commands: &[BundleCommand], inner: Option<Arc<dyn UiSurface>>) -> Self {
        let slash = commands
            .iter()
            .map(|command| {
                let description = match &command.argument_hint {
                    Some(hint) => format!("{} {hint}", command.description).trim().to_owned(),
                    None => command.description.clone(),
                };
                SlashEntry { name: format!("{plugin}:{}", command.name), description }
            })
            .collect();
        Self { plugin, commands: commands.to_vec(), slash, inner }
    }
}

impl UiSurface for BundleSurface {
    fn plugin(&self) -> &str {
        self.plugin
    }

    fn entries(&self) -> &[UiEntry] {
        self.inner.as_ref().map_or(&[], |inner| inner.entries())
    }

    fn hidden(&self) -> &[String] {
        self.inner.as_ref().map_or(&[], |inner| inner.hidden())
    }

    fn render(&self, page: &str, workspace: Option<&Path>) -> Result<Value, String> {
        self.inner.as_ref().ok_or("this plugin has no pages")?.render(page, workspace)
    }

    fn act(&self, page: &str, action: &str, payload: Value, workspace: Option<&Path>) -> Result<Option<String>, String> {
        self.inner.as_ref().ok_or("this plugin has no pages")?.act(page, action, payload, workspace)
    }

    fn card_tools(&self) -> &[String] {
        self.inner.as_ref().map_or(&[], |inner| inner.card_tools())
    }

    fn card(&self, tool: &str, input: &Value, output: Option<&str>, is_error: bool) -> Result<ToolCardView, String> {
        self.inner.as_ref().ok_or("this plugin draws no tool cards")?.card(tool, input, output, is_error)
    }

    fn event_kinds(&self) -> &[String] {
        self.inner.as_ref().map_or(&[], |inner| inner.event_kinds())
    }

    fn render_event(&self, kind: &str, data: &Value) -> Result<Value, String> {
        self.inner.as_ref().ok_or("this plugin renders no events")?.render_event(kind, data)
    }

    fn commands(&self) -> &[CommandEntry] {
        self.inner.as_ref().map_or(&[], |inner| inner.commands())
    }

    fn slash_commands(&self) -> Vec<SlashEntry> {
        let mut slash = self.slash.clone();
        if let Some(inner) = &self.inner {
            slash.extend(inner.slash_commands());
        }
        slash
    }

    fn run_command(&self, id: &str, slash: bool, args: &str, workspace: Option<&Path>) -> Result<CommandResult, String> {
        let own = id.strip_prefix(self.plugin).and_then(|rest| rest.strip_prefix(':'));
        if slash && let Some(command) = own.and_then(|name| self.commands.iter().find(|command| command.name == name)) {
            let text = command.render(args);
            return Ok(if command.send { CommandResult::Send(text) } else { CommandResult::Insert(text) });
        }
        self.inner.as_ref().ok_or_else(|| format!("没有命令 {id}"))?.run_command(id, slash, args, workspace)
    }

    fn themes(&self) -> &[ThemeDef] {
        self.inner.as_ref().map_or(&[], |inner| inner.themes())
    }

    fn refresh_generation(&self, page: &str) -> u64 {
        self.inner.as_ref().map_or(0, |inner| inner.refresh_generation(page))
    }
}
