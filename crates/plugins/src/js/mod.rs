//! JS 插件：`<数据目录>/plugins/<名字>/index.js`，跑在内嵌的 QuickJS 里（详见 `docs/plugins.md`）。
//!
//! 每个 JS 插件对内核来说就是一个普通的 [`Plugin`]：`apply` 时加载模块，把它导出的工具贡献到
//! [`Tools`]；加载失败时插件是“激活失败”，原因显示在设置页里。

/// 流式响应正文的接收端：一块一块的字节，或者读到一半出的错。
pub(super) type BodyReceiver = tokio::sync::mpsc::Receiver<Result<Vec<u8>, String>>;

mod api;
mod app;
mod bundle;
mod creator;
mod dshr;
mod hooks;
mod host;
mod mention;
mod net;
mod oauth;
mod observe;
mod ops;
mod process;
mod reload;
mod runtime;
mod services;
mod storage;
mod stream;
mod surface;
mod watch;

#[cfg(test)]
pub(crate) use bundle::{TEST_CONNECTOR, server_status as bundle_server_status};
pub use creator::CreatorPlugin;
pub use host::LogLine;
pub use reload::{JsPlugins, PluginReport, ReloadReport, Watcher};
#[cfg(test)]
pub(crate) use watch::TEST_WORKSPACES;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dshr_agent::{EventKinds, EventLabel, EventLabels, PromptContext, PromptContributor, PromptParts};
use dshr_context::{Manifest, Plugin, Scope};
use dshr_core::{EventBody, EventProjector};
use dshr_tools::{Access, Tool, ToolContext, ToolOutput, Tools};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::grants::Grants;
use crate::manifest::{Capabilities, Found, PluginManifest};
use host::Permits;
use runtime::{Command, Instance, Job, Reply, ToolDecl};

/// 工具名里不能用的前缀：留给 MCP 服务器的工具。
const MCP_PREFIX: &str = "mcp__";

/// 一个 JS 插件。克隆出来的指向同一个运行时：主代理和子代理的两个 `Context` 各加一份，
/// 脚本只加载一次、只有一条线程。
#[derive(Clone)]
pub struct JsPlugin {
    inner: Arc<Inner>,
    /// 装进这个 `Context` 时已经有的工具名；脚本不能用这些名字（否则会顶替掉 `read_file` 之类）。
    reserved: Arc<HashSet<String>>,
}

struct Inner {
    id: &'static str,
    /// 插件文件夹（重载时算指纹用）。
    dir: std::path::PathBuf,
    manifest: Result<PluginManifest, String>,
    /// 清单里声明了、用户还没允许过的能力（见 `grants`）。不为空时插件不激活，代码也不执行。
    missing: Vec<String>,
    /// 加载成功后留着；失败的不留，下次激活时重新加载。
    instance: Mutex<Option<Arc<Instance>>>,
    /// 正在加载运行时：同一时间只起一个。
    starting: Mutex<()>,
    /// 启动时预加载（[`JsPlugins::preload`]）失败的原因。紧接着的那次激活直接报这个错，不再把同样的
    /// 加载（可能要等到超时）重来一遍；只用一次，之后的激活（热重载、重新启用）照常重新加载。
    preload_error: Mutex<Option<String>>,
    /// 热重载时旧版本 `state.snapshot()` 的结果，加载运行时时交给 `state.restore`（只用一次）。
    restore: Mutex<Option<String>>,
    /// 这个版本在几个 `Context` 里处于激活状态；后台任务只在大于 0 时运行。
    active: Arc<std::sync::atomic::AtomicUsize>,
    /// 常驻服务的状态（设置页显示）。
    services: services::ServiceStatus,
}

impl JsPlugin {
    fn new(found: Found, grants: &Grants) -> Self {
        let missing = match &found.manifest {
            Ok(manifest) => crate::grants::missing(grants, &found.id, &manifest.capabilities.names()),
            Err(_) => Vec::new(),
        };
        let inner = Inner {
            id: intern(&found.id),
            dir: found.dir,
            manifest: found.manifest,
            missing,
            instance: Mutex::new(None),
            starting: Mutex::new(()),
            preload_error: Mutex::new(None),
            restore: Mutex::new(None),
            active: Arc::default(),
            services: Arc::default(),
        };
        Self { inner: Arc::new(inner), reserved: Arc::default() }
    }

    /// 能激活时的清单：清单读得出来，声明的能力也都允许过。
    fn ready(&self) -> Result<&PluginManifest, String> {
        let manifest = self.inner.manifest.as_ref().map_err(Clone::clone)?;
        if !crate::manifest::host_satisfies(manifest.requires_dshr.as_deref()) {
            return Err(format!(
                "需要 dshr {}，现在是 {}：更新 dshr，或者装这个插件适合当前版本的旧版本",
                manifest.requires_dshr.as_deref().unwrap_or_default(),
                crate::manifest::host_version()
            ));
        }
        if !self.inner.missing.is_empty() {
            return Err(format!(
                "需要授权：{}。在“设置 → 扩展”里允许，或运行 `dshr plugin grant {}`",
                self.inner.missing.join("、"),
                self.inner.id
            ));
        }
        Ok(manifest)
    }

    pub fn id(&self) -> &'static str {
        self.inner.id
    }

    /// 插件文件夹。
    pub(crate) fn dir(&self) -> &std::path::Path {
        &self.inner.dir
    }

    /// 界面扩展的情况，给创造模式的 `plugin_reload` 看：入口、藏起来的内置入口，以及每一页实际画一次的结果。
    fn ui_summary(&self) -> Vec<String> {
        use dshr_agent::UiSurface as _;
        let instance = {
            let slot = self.inner.instance.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match slot.as_ref() {
                Some(instance) => instance.clone(),
                None => return Vec::new(),
            }
        };
        let Ok(manifest) = &self.inner.manifest else { return Vec::new() };
        let ui = &instance.exports.ui;
        let mut lines = Vec::new();
        for (place, entries) in [("sidebar", &ui.sidebar), ("settings", &ui.settings), ("panel", &ui.panels), ("status", &ui.status)] {
            for entry in entries {
                lines.push(format!("{place} entry `{}` (\"{}\") opens page `{}`", entry.id, entry.label, entry.page));
            }
        }
        let exports = &instance.exports;
        if !exports.cards.is_empty() {
            lines.push(format!("draws cards for tools: {}", exports.cards.join(", ")));
        }
        if !exports.renders.is_empty() {
            lines.push(format!("renders events as trees: {}", exports.renders.join(", ")));
        }
        if !exports.hooks.is_empty() {
            lines.push(format!("hooks: {}", exports.hooks.join(", ")));
        }
        for schedule in &exports.schedules {
            lines.push(format!("schedule `{}` runs every {} s", schedule.id, schedule.every_ms / 1000));
        }
        if !ui.hide.is_empty() {
            lines.push(format!("hides built-in entries: {}", ui.hide.join(", ")));
        }
        for theme in &exports.themes {
            let keys = |colors: &std::collections::BTreeMap<String, String>| colors.keys().cloned().collect::<Vec<_>>().join(", ");
            lines.push(format!(
                "color scheme `{}/{}` (\"{}\"): light [{}], dark [{}]",
                self.inner.id,
                theme.id,
                theme.name,
                keys(&theme.light),
                keys(&theme.dark)
            ));
        }
        let surface = surface::JsSurface::new(self.inner.id, manifest, manifest.timeout, instance.clone());
        for page in &ui.pages {
            lines.push(match surface.render(page, None) {
                Ok(tree) => format!("page `{page}` renders ({} nodes): {}", dshr_agent::validate_tree(&tree).unwrap_or(0), preview(&tree)),
                Err(error) => format!("page `{page}` FAILED to render: {error}"),
            });
        }
        lines
    }

    /// 已经加载好的运行时导出的工具名。
    fn tool_names(&self) -> Vec<String> {
        let slot = self.inner.instance.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.as_ref().map(|instance| instance.exports.tools.iter().map(|tool| tool.name.clone()).collect()).unwrap_or_default()
    }

    /// 因为连续失败被停下时的原因（见 [`runtime::STRIKES`]）。
    fn stopped(&self) -> Option<String> {
        let slot = self.inner.instance.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.as_ref().and_then(|instance| instance.health.stopped())
    }

    /// 启动时在自己的线程上先把运行时加载好（见 [`JsPlugins::preload`]）。
    fn preload_at_startup(&self, config: Option<&Value>) {
        // 配置里停用的不加载：激活时也不会用到它。
        if config.and_then(|config| config.get("enabled")) == Some(&Value::Bool(false)) {
            return;
        }
        let Ok(manifest) = self.ready() else { return };
        if !manifest.has_script {
            return;
        }
        if let Err(error) = self.instance(manifest, &script_config(config.cloned())) {
            *self.inner.preload_error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
        }
    }

    fn loaded_instance(&self) -> Option<Arc<Instance>> {
        self.inner.instance.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    fn instance(&self, manifest: &PluginManifest, config: &Value) -> Result<Arc<Instance>, String> {
        if let Some(instance) = self.loaded_instance() {
            return Ok(instance);
        }
        // 加载模块可能要几秒：只拿 `starting` 锁，`instance` 槽位不锁着，`tool_names`、`stopped` 照常立即返回。
        let _starting = self.inner.starting.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(instance) = self.loaded_instance() {
            return Ok(instance);
        }
        let restore = self.inner.restore.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        let instance = Arc::new(Instance::start(self.inner.id, manifest, config, restore)?);
        if !instance.exports.schedules.is_empty() && manifest.capabilities.background {
            hooks::start_schedules(self.inner.id, manifest, &instance, self.inner.active.clone());
        }
        if !instance.exports.watch.is_empty() && manifest.capabilities.background && manifest.capabilities.fs_read {
            watch::start(self.inner.id, manifest, &instance, self.inner.active.clone());
        }
        if !instance.exports.services.is_empty() && manifest.capabilities.background {
            services::start(
                self.inner.id,
                manifest,
                config,
                Arc::downgrade(&instance),
                self.inner.active.clone(),
                self.inner.services.clone(),
            );
        }
        *self.inner.instance.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(instance.clone());
        Ok(instance)
    }

    /// 已经加载的版本提供了哪些东西（设置页卡片上的“提供：…”）。
    pub(crate) fn provides(&self) -> Vec<(&'static str, usize)> {
        let bundle = self.inner.manifest.as_ref().map(|manifest| manifest.bundle.clone()).unwrap_or_default();
        let skills = bundle.skills.as_ref().map_or(0, |dir| {
            dshr_skills::discover_plugin_skills(&[Arc::new(dshr_skills::SkillRoot { plugin: self.inner.id.to_owned(), dir: dir.clone() })])
                .len()
        });
        let bundled =
            [("mcp", bundle.mcp_servers.len()), ("skills", skills), ("slash", bundle.commands.len()), ("presets", bundle.presets.len())];
        let Some(instance) = self.loaded_instance() else {
            return bundled.into_iter().filter(|(_, count)| *count > 0).collect();
        };
        let exports = &instance.exports;
        let ui = &exports.ui;
        let mut provides: Vec<(&'static str, usize)> = [
            ("tools", exports.tools.len()),
            ("sidebar", ui.sidebar.len()),
            ("settings", ui.settings.len()),
            ("panels", ui.panels.len()),
            ("status", ui.status.len()),
            ("hooks", exports.hooks.len()),
            ("cards", exports.cards.len()),
            ("commands", exports.commands.len()),
            ("slash", exports.slash.len()),
            ("presets", exports.presets.len()),
            ("providers", exports.providers.len()),
            ("themes", exports.themes.len()),
            ("schedules", exports.schedules.len()),
            ("services", exports.services.len()),
            ("prompt", usize::from(exports.prompt)),
            ("events", exports.events.len().max(exports.labels.len()).max(exports.renders.len())),
        ]
        .into_iter()
        .collect();
        for (kind, count) in bundled {
            match provides.iter_mut().find(|(existing, _)| *existing == kind) {
                Some((_, existing)) => *existing += count,
                None => provides.push((kind, count)),
            }
        }
        provides.retain(|(_, count)| *count > 0);
        provides
    }

    /// 常驻服务的状态：(服务 id, 一句话)。
    pub(crate) fn service_status(&self) -> Vec<(String, String)> {
        self.inner
            .services
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(id, text)| (id.clone(), text.clone()))
            .collect()
    }

    /// 热重载前：旧版本导出了 `state` 时拿它的快照。
    fn snapshot(&self) -> Option<String> {
        let instance = self.loaded_instance()?;
        if !instance.exports.lifecycle.state {
            return None;
        }
        let job = Job::Dispatch { kind: "lifecycle", name: "snapshot".into(), payload: "null".into() };
        match instance.ask(job, None, LIFECYCLE_WAIT) {
            Ok(snapshot) => snapshot.filter(|snapshot| snapshot != "null"),
            Err(error) => {
                tracing::warn!(plugin = self.inner.id, error, "插件没能给出热重载前的状态");
                None
            }
        }
    }

    /// 新版本加载时交给 `state.restore` 的状态。
    fn set_restore(&self, snapshot: String) {
        *self.inner.restore.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(snapshot);
    }

    /// 这个版本被换下或卸掉：调用它的 `deactivate`。
    fn deactivate(&self) {
        let Some(instance) = self.loaded_instance() else { return };
        if !instance.exports.lifecycle.deactivate {
            return;
        }
        let job = Job::Dispatch { kind: "lifecycle", name: "deactivate".into(), payload: "null".into() };
        if let Err(error) = instance.ask(job, None, LIFECYCLE_WAIT) {
            tracing::warn!(plugin = self.inner.id, error, "插件的 deactivate 失败");
        }
    }

    /// 模块导出的名字合不合规。`apply` 和热重载前的预加载用的是同一套检查。
    fn check(&self, exports: &runtime::Exports) -> anyhow::Result<()> {
        let capabilities = self.inner.manifest.as_ref().map(|manifest| manifest.capabilities.clone()).unwrap_or_default();
        if !(exports.schedules.is_empty() && exports.services.is_empty()) && !capabilities.background {
            anyhow::bail!("导出了 schedules 或 services，但 plugin.toml 没有声明 `background` 能力");
        }
        if !exports.watch.is_empty() && !(capabilities.background && capabilities.fs_read) {
            anyhow::bail!("导出了 watch，但 plugin.toml 没有同时声明 `background` 和 `fs:workspace:read` 能力");
        }
        if !exports.providers.is_empty() && !capabilities.provider {
            anyhow::bail!("导出了 providers，但 plugin.toml 没有声明 `provider` 能力");
        }
        for provider in &exports.providers {
            if !crate::manifest::valid_id(&provider.id) {
                anyhow::bail!("服务商 id `{}` 不合规：只用小写字母、数字、`-`、`_`", provider.id);
            }
            let host = reqwest::Url::parse(&provider.base_url).ok().and_then(|url| url.host_str().map(str::to_owned));
            if !host.as_deref().is_some_and(|host| capabilities.allows_host(host)) {
                anyhow::bail!("服务商 {} 的地址 {} 不在声明的 `net:` 范围里", provider.id, provider.base_url);
            }
        }
        for kind in exports.events.iter().chain(&exports.labels).chain(&exports.renders) {
            // 名字要合规，而且只能是自己命名空间里的：别的插件的事件轮不到它来决定怎么进上下文。
            EventBody::ext(kind.as_str(), Value::Null)?;
            if kind.split_once('/').is_none_or(|(namespace, _)| namespace != self.inner.id) {
                anyhow::bail!("事件类型 `{kind}` 应当以 `{}/` 开头", self.inner.id);
            }
        }
        let ui = &exports.ui;
        for entry in ui.sidebar.iter().chain(&ui.settings).chain(&ui.panels).chain(&ui.status) {
            if !crate::manifest::valid_id(&entry.id) {
                anyhow::bail!("界面入口 id `{}` 不合规：只用小写字母、数字、`-`、`_`", entry.id);
            }
        }
        for target in &ui.hide {
            if !dshr_agent::hideable(target) {
                anyhow::bail!(
                    "ui.hide 里的 `{target}` 不能藏：可以是 settings:general、settings:models、composer:presets、composer:permission、status:<id>、sidebar:<id>"
                );
            }
        }
        for decl in &exports.tools {
            let name = &decl.name;
            if !valid_tool_name(name) {
                anyhow::bail!("工具名 `{name}` 不合规：只用字母、数字、`_`、`-`，最长 64，不能以 `{MCP_PREFIX}` 开头");
            }
            if self.reserved.contains(name) {
                anyhow::bail!("工具名 `{name}` 已经被占用，换一个名字");
            }
        }
        Ok(())
    }

    /// 热重载时先在一边把新版本加载好、检查过：成功后再去替换正在用的那一份，
    /// 这样 `apply` 拿到的是已经加载好的运行时，不会失败。
    fn preload(&self, config: &Value) -> Result<(), String> {
        let manifest = self.ready()?;
        if !manifest.has_script {
            return Ok(());
        }
        let instance = self.instance(manifest, config)?;
        self.check(&instance.exports).map_err(|error| format!("{error:#}"))
    }
}

/// 交给脚本的配置：`[plugins.<id>]` 去掉 `enabled`（那是给内核看的）；没有配置时是空对象。
fn script_config(raw: Option<Value>) -> Value {
    let mut config = raw.unwrap_or_else(|| Value::Object(Default::default()));
    if let Some(object) = config.as_object_mut() {
        object.remove("enabled");
    }
    config
}

impl Plugin for JsPlugin {
    /// 克隆共享同一个运行时（`inner`）：热重载用这判断换上的和现在装着的是不是同一个版本。
    fn version(&self) -> Option<usize> {
        Some(Arc::as_ptr(&self.inner) as usize)
    }

    fn manifest(&self) -> Manifest {
        let manifest = Manifest::new(self.inner.id);
        match self.inner.manifest.as_ref().ok().and_then(|manifest| manifest.config_schema.clone()) {
            Some(schema) => manifest.config_schema(schema),
            None => manifest,
        }
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        let manifest = self.ready().map_err(anyhow::Error::msg)?;
        let config = script_config(cx.config_value().cloned());
        let timeout = config.get("timeout_ms").and_then(Value::as_u64).map_or(manifest.timeout, Duration::from_millis);
        if !manifest.has_script {
            // “纯包”插件：只有 Skills、命令、预设、MCP 服务器，不启动 QuickJS。
            bundle::apply(cx, self.inner.id, manifest, None);
            return Ok(());
        }
        if let Some(error) = self.inner.preload_error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
            return Err(anyhow::anyhow!("{error}"));
        }
        let instance = self.instance(manifest, &config).map_err(|error| anyhow::anyhow!("{error}"))?;
        self.check(&instance.exports)?;
        if instance.exports.prompt {
            cx.contribute::<PromptParts>(Arc::new(JsPrompt { plugin: self.inner.id, instance: instance.clone() }));
        }
        for kind in &instance.exports.events {
            cx.contribute::<EventKinds>(Arc::new(JsProjector::new(self.inner.id, kind, &instance, false)));
        }
        for kind in &instance.exports.labels {
            cx.contribute::<EventLabels>(Arc::new(JsProjector::new(self.inner.id, kind, &instance, true)));
        }
        let exports = &instance.exports;
        let ui = &exports.ui;
        let surface = (!ui.is_empty()
            || !exports.cards.is_empty()
            || !exports.renders.is_empty()
            || !exports.commands.is_empty()
            || !exports.slash.is_empty()
            || !exports.themes.is_empty())
        .then(|| Arc::new(surface::JsSurface::new(self.inner.id, manifest, timeout, instance.clone())) as Arc<dyn dshr_agent::UiSurface>);
        // 包里的 Skills、命令（和脚本的界面扩展合成一个 surface）、预设、MCP 服务器。
        bundle::apply(cx, self.inner.id, manifest, surface);
        hooks::register(cx, self.inner.id, manifest, timeout, &instance);
        // 动态工具（`host.tools.set`）：不能占用装进来时已有的工具名和自己静态导出的工具名。
        {
            let mut reserved = instance.dynamic.reserved.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            reserved.extend(self.reserved.iter().cloned());
            reserved.extend(instance.exports.tools.iter().map(|tool| tool.name.clone()));
        }
        cx.contribute::<dshr_tools::ToolSources>(Arc::new(DynamicToolSource {
            capabilities: manifest.capabilities.clone(),
            timeout,
            instance: instance.clone(),
        }));
        for decl in &instance.exports.mentions {
            cx.contribute::<dshr_agent::MentionSources>(Arc::new(mention::JsMention::new(
                self.inner.id,
                decl.clone(),
                &manifest.capabilities,
                instance.clone(),
            )));
        }
        if !instance.exports.api.is_empty() {
            api::register(cx, self.inner.id, api::ApiEntry { instance: instance.clone(), capabilities: manifest.capabilities.clone() });
        }
        if !instance.exports.on.is_empty() {
            cx.contribute::<dshr_agent::EventObservers>(Arc::new(observe::JsObserver::new(
                self.inner.id,
                manifest,
                timeout,
                instance.clone(),
            )));
        }
        for provider in &instance.exports.providers {
            cx.contribute::<dshr_llm::ProviderAuths>(Arc::new(JsProviderAuth {
                id: format!("{}/{}", self.inner.id, provider.id),
                decl: provider.clone(),
                manifest: manifest.clone(),
                instance: instance.clone(),
            }));
        }
        for preset in &instance.exports.presets {
            cx.contribute::<dshr_agent::PluginPresets>(Arc::new(dshr_agent::PresetDef {
                plugin: self.inner.id.to_owned(),
                id: preset.id.clone(),
                label: preset.label.clone(),
                description: preset.description.clone(),
                instructions: preset.instructions.clone(),
                read_only: preset.read_only,
                tools: preset.tools.clone(),
            }));
        }
        cx.contribute::<hooks::Activity>(Arc::new(hooks::ActiveGuard::new(self.inner.active.clone())));
        for decl in &instance.exports.tools {
            cx.contribute::<Tools>(Arc::new(JsTool {
                decl: decl.clone(),
                capabilities: manifest.capabilities.clone(),
                timeout,
                instance: instance.clone(),
            }));
        }
        Ok(())
    }
}

/// `host.tools.set` 设的工具：每一步取一次最新的声明。
struct DynamicToolSource {
    capabilities: Capabilities,
    timeout: Duration,
    instance: Arc<Instance>,
}

#[async_trait::async_trait]
impl dshr_tools::ToolSource for DynamicToolSource {
    async fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let decls = self.instance.dynamic.decls.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        decls
            .into_iter()
            .map(|decl| {
                Arc::new(JsTool { decl, capabilities: self.capabilities.clone(), timeout: self.timeout, instance: self.instance.clone() })
                    as Arc<dyn Tool>
            })
            .collect()
    }
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(MCP_PREFIX)
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// 内核里插件 id 是 `&'static str`，而脚本插件的 id 运行时才知道：每个不同的 id 泄漏一份，
/// 同一个 id 只泄漏一次（数量以装过的插件数为界）。
fn intern(id: &str) -> &'static str {
    static IDS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut ids = IDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = ids.iter().find(|existing| **existing == id) {
        return existing;
    }
    let leaked: &'static str = Box::leak(id.to_owned().into_boxed_str());
    ids.push(leaked);
    leaked
}

struct JsTool {
    decl: ToolDecl,
    capabilities: Capabilities,
    timeout: Duration,
    instance: Arc<Instance>,
}

impl JsTool {
    /// 这个工具能不能写文件：插件有写能力，而且工具没有声明 `readOnly`。
    fn writes(&self) -> bool {
        self.capabilities.fs_write && !self.decl.read_only
    }

    /// 这个工具能不能联网：插件声明了 `net:` 能力，而且工具没有声明 `readOnly`。
    fn connects(&self) -> bool {
        !self.capabilities.net.is_empty() && !self.decl.read_only
    }

    /// 这个工具能不能替用户做事（发消息、起子代理、改宿主数据、装插件）：插件有这类能力，而且工具没有声明 `readOnly`。
    fn acts(&self) -> bool {
        self.capabilities.acts_for_user() && !self.decl.read_only
    }

    /// 这个工具能不能启动程序：插件声明了 `exec:` 能力，而且工具没有声明 `readOnly`。
    fn executes(&self) -> bool {
        !self.capabilities.exec.is_empty() && !self.decl.read_only
    }
}

/// 调用方不再等结果（这一轮被中断，`execute` 的 future 被丢弃）时通知插件线程。
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl Tool for JsTool {
    fn name(&self) -> &str {
        &self.decl.name
    }

    fn description(&self) -> &str {
        &self.decl.description
    }

    fn input_schema(&self) -> Value {
        self.decl.input_schema.clone()
    }

    /// 访问类型按插件的能力来，不由脚本说了算：
    /// - 能启动程序的工具算“执行命令”（和 `run_shell` 一样，按权限模式询问或放行）；
    /// - 能联网的工具算“交给外部”（和 MCP 的工具一样）：它可以把读到的内容发出去，影响范围事先判断不了，
    ///   所以只读模式下拒绝，“工作区内修改”下每次询问；
    /// - 只能写工作区的算“写工作区”：只读模式下拒绝，“工作区内修改”下放行；
    /// - 其余算只读。
    fn access(&self, _input: &Value, ctx: &ToolContext) -> Access {
        if self.executes() {
            Access::Execute
        } else if self.connects() || self.acts() {
            Access::External
        } else if self.writes() {
            Access::Write(ctx.workspace.clone())
        } else {
            Access::Read
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel = CancelOnDrop(cancelled.clone());
        let (reply, result) = oneshot::channel();
        let command = Command {
            job: Job::Tool { name: self.decl.name.clone(), input },
            ctx: Some(ctx.clone()),
            permits: Permits {
                write: self.writes(),
                net: self.connects(),
                exec: self.executes(),
                llm: self.capabilities.llm,
                act: self.acts(),
            },
            timeout: self.timeout,
            cancelled,
            reply: Reply::Tool(reply),
        };
        if let Some(reason) = self.instance.health.stopped() {
            return ToolOutput::error(format!(
                "This plugin has been stopped for the rest of this session: {reason}. It comes back when its files change (hot reload) or after a restart."
            ));
        }
        if self.instance.send(command).is_err() {
            return ToolOutput::error("The plugin's runtime has stopped.");
        }
        result.await.unwrap_or_else(|_| ToolOutput::error("The plugin's runtime stopped before the tool finished."))
    }
}

/// `prompt` 导出：每轮开始时问一次。这里是同步代码，等的时间要短；超时或出错时这一轮就没有这段内容。
const PROMPT_WAIT: Duration = Duration::from_secs(1);
/// 追加到系统提示词后面的内容最多这么多字节。
const MAX_PROMPT_BYTES: usize = 32 * 1024;
/// 插件服务商（`providers` 导出）：每个请求前问脚本的 `auth(host)` 要地址和鉴权。
struct JsProviderAuth {
    /// `<插件 id>/<服务商 id>`。
    id: String,
    decl: runtime::ProviderDecl,
    manifest: PluginManifest,
    instance: Arc<Instance>,
}

/// 问插件服务商的鉴权、模型列表最多等这么久。
const PROVIDER_WAIT: Duration = Duration::from_secs(10);

impl JsProviderAuth {
    async fn ask(&self, part: &str) -> Result<Value, dshr_llm::LlmError> {
        let data_dir = surface::data_dir(&self.manifest);
        let _ = std::fs::create_dir_all(&data_dir);
        let job = Job::Dispatch { kind: "provider", name: self.decl.id.clone(), payload: serde_json::json!({ "part": part }).to_string() };
        // 可以联网（刷新 token）、读写自己的数据，别的都不行。
        let permits = Permits { net: !self.manifest.capabilities.net.is_empty(), ..Permits::default() };
        let answer = self
            .instance
            .call(job, Some(ToolContext::new(data_dir)), PROVIDER_WAIT, permits)
            .await
            .map_err(|error| dshr_llm::LlmError::Api(format!("插件服务商 {} 出错：{error}", self.id)))?;
        Ok(answer.and_then(|text| serde_json::from_str(&text).ok()).unwrap_or(Value::Null))
    }
}

#[async_trait::async_trait]
impl dshr_llm::ProviderAuth for JsProviderAuth {
    fn id(&self) -> &str {
        &self.id
    }

    fn describe(&self) -> dshr_llm::PluginProviderInfo {
        dshr_llm::PluginProviderInfo {
            id: self.id.clone(),
            name: self.decl.name.clone(),
            protocol: if self.decl.protocol == "messages" { dshr_config::Protocol::Messages } else { dshr_config::Protocol::OpenAi },
            base_url: self.decl.base_url.clone(),
        }
    }

    async fn credentials(&self) -> Result<dshr_llm::Credentials, dshr_llm::LlmError> {
        let answer = self.ask("auth").await?;
        let text = |key: &str| answer.get(key).and_then(Value::as_str).map(str::to_owned);
        let headers = answer
            .get("headers")
            .and_then(Value::as_object)
            .map(|headers| headers.iter().filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_owned()))).collect())
            .unwrap_or_default();
        let base_url = text("baseUrl").or_else(|| Some(self.decl.base_url.clone()));
        // 插件只能把请求发到它声明过的主机。
        if let Some(url) = &base_url {
            let host = reqwest::Url::parse(url).ok().and_then(|url| url.host_str().map(str::to_owned)).unwrap_or_default();
            if !self.manifest.capabilities.allows_host(&host) {
                return Err(dshr_llm::LlmError::Api(format!("插件服务商的地址 {url} 不在它声明的 `net:` 范围里")));
            }
        }
        Ok(dshr_llm::Credentials { base_url, api_key: text("apiKey"), headers })
    }

    async fn models(&self) -> Result<Vec<String>, dshr_llm::LlmError> {
        if let Some(models) = &self.decl.models {
            return Ok(models.clone());
        }
        let answer = self.ask("models").await?;
        Ok(answer.as_array().map(|models| models.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default())
    }
}

/// `state.snapshot`、`deactivate` 最多等这么久。
const LIFECYCLE_WAIT: Duration = Duration::from_secs(1);
/// `events` 导出里的投影函数：每条事件问一次，结果按事件数据缓存。
const PROJECT_WAIT: Duration = Duration::from_millis(500);
const PROJECT_CACHE_ENTRIES: usize = 4096;
/// `label` 函数同步地问（对话区在后台线程上问），等得更短：问不到这一条就不显示。
const LABEL_WAIT: Duration = Duration::from_millis(200);

struct JsPrompt {
    plugin: &'static str,
    instance: Arc<Instance>,
}

impl PromptContributor for JsPrompt {
    fn contribute(&self, cx: &PromptContext<'_>) -> Option<String> {
        // 给它一个只读的环境：有读能力的插件可以在这里读工作区里的文件。
        let ctx = ToolContext::new(cx.workspace.to_owned());
        match self.instance.ask(Job::Prompt, Some(ctx), PROMPT_WAIT) {
            Ok(text) => {
                let mut text = text?;
                if let Some((cut, _)) = text.char_indices().find(|(index, _)| *index >= MAX_PROMPT_BYTES) {
                    text.truncate(cut);
                    text.push_str("\n… [truncated]");
                }
                let text = text.trim();
                (!text.is_empty()).then(|| text.to_owned())
            }
            Err(error) => {
                tracing::warn!(plugin = self.plugin, error, "插件的 prompt 没有给出内容");
                None
            }
        }
    }
}

/// `events` 导出里的一个函数：`project`（进模型上下文的文本）或 `label`（对话区里的一行字）。
struct JsProjector {
    plugin: &'static str,
    kind: String,
    /// 是 `label` 函数。
    label: bool,
    instance: Arc<Instance>,
    /// 事件数据（JSON 文本）→ 投影结果。每一步都要把整个日志重新投影一遍，同一条事件不必每次都问脚本；
    /// 同样的数据也因此总是得到同样的文本，请求的前缀保持稳定。
    cache: Mutex<ProjectCache>,
}

/// 两代缓存：当前一代满了就降为上一代、开新的一代。命中上一代的条目搬回当前一代，
/// 所以长会话里还在用的事件不会因为整表清空而全部重新问一遍。
#[derive(Default)]
struct ProjectCache {
    current: HashMap<String, Option<String>>,
    previous: HashMap<String, Option<String>>,
}

impl ProjectCache {
    fn get(&mut self, key: &str) -> Option<Option<String>> {
        if let Some(text) = self.current.get(key) {
            return Some(text.clone());
        }
        let (key, text) = self.previous.remove_entry(key)?;
        self.insert(key, text.clone());
        Some(text)
    }

    fn insert(&mut self, key: String, text: Option<String>) {
        if self.current.len() >= PROJECT_CACHE_ENTRIES {
            self.previous = std::mem::take(&mut self.current);
        }
        self.current.insert(key, text);
    }
}

impl JsProjector {
    fn new(plugin: &'static str, kind: &str, instance: &Arc<Instance>, label: bool) -> Self {
        Self { plugin, kind: kind.to_owned(), label, instance: instance.clone(), cache: Mutex::default() }
    }

    fn text(&self, data: &Value) -> Option<String> {
        let data = data.to_string();
        let cached = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&data);
        if let Some(text) = cached {
            return text;
        }
        // 问脚本时不持有缓存锁：界面线程上的 label 不必排在代理那边的投影后面。
        let job = Job::Project { kind: self.kind.clone(), data: data.clone(), label: self.label };
        match self.instance.ask(job, None, if self.label { LABEL_WAIT } else { PROJECT_WAIT }) {
            Ok(text) => {
                self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(data, text.clone());
                text
            }
            // 没问到的不记进缓存，下次投影时再问。
            Err(error) => {
                tracing::warn!(plugin = self.plugin, kind = self.kind, label = self.label, error, "插件没有给出事件的文本");
                None
            }
        }
    }
}

impl EventProjector for JsProjector {
    fn kind(&self) -> &str {
        &self.kind
    }

    fn project(&self, data: &Value) -> Option<String> {
        self.text(data)
    }
}

impl EventLabel for JsProjector {
    fn kind(&self) -> &str {
        &self.kind
    }

    fn label(&self, data: &Value) -> Option<String> {
        self.text(data)
    }
}

/// 页面节点树的开头一段，给模型核对内容。
fn preview(tree: &Value) -> String {
    let text = tree.to_string();
    match text.char_indices().nth(600) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}
