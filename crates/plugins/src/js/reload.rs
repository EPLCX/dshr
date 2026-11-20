//! 插件目录里的全部 JS 插件，以及热重载：改了脚本不用重启。
//!
//! 重载一个插件分三步：在一边把新版本加载好并检查（旧版本照常工作）→ 成功后在每个 `Context` 里
//! 原地换掉旧的（`Context::replace`，只发布一次快照）→ 旧的运行时等最后一次进行中的调用结束后自己退出。
//! agent 每一步开始时取一份快照，所以新版本从下一步起生效。新版本加载不了时旧版本继续用。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dshr_context::{Context, ContextError, PluginGroup, Status};
use dshr_tools::Tools;
use serde_json::Value;

use super::{JsPlugin, script_config};
use crate::grants::{self, GrantRequest, Grants};
use crate::manifest::{self, Found};

/// 收不到文件系统通知时（目录在不支持通知的文件系统上等）多久看一次插件目录。
/// 文件要连续两次看到的都一样才算改完了，所以保存之后大约一到两个间隔生效。
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// 收到文件变化通知后，等目录安静这么久再看：保存一个文件常常是好几次写入。
const SETTLE: Duration = Duration::from_millis(300);

/// 插件目录里的全部 JS 插件。克隆出来的指向同一份状态。
#[derive(Clone, Default)]
pub struct JsPlugins {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    dir: PathBuf,
    state: Mutex<State>,
    /// 重载一次一次来（监视线程和手动重载可能同时触发）。只在重载里拿，读状态的调用不碰它。
    reloading: Mutex<()>,
}

#[derive(Default)]
struct State {
    /// 现在装着的插件，按 id 排序。
    loaded: Vec<Loaded>,
    /// 装了这些插件的 `Context`（主代理的、子代理的）。
    attached: Vec<Attached>,
    /// 看到变了、但还没确认改完的：id → 上一次看到的指纹。
    pending: HashMap<String, u64>,
    /// 新版本加载不了、仍在用旧版本的：id → 原因。新版本换上或文件夹没了时清掉。
    kept: HashMap<String, String>,
}

struct Loaded {
    plugin: JsPlugin,
    /// 加载它（或者试过加载它）时文件夹的指纹，连同它得到的授权（见 [`stamp`]）。
    stamp: u64,
}

/// 一个插件“现在的版本”：文件夹里文件的指纹，加上用户给它的授权和开关状态。授权或开关变了
/// 也算变了，所以在设置页或命令行里允许、停用之后，热重载会把它装上或停下。
fn stamp(dir: &Path, id: &str, grants: &Grants, enabled: Option<bool>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    manifest::fingerprint(dir).hash(&mut hasher);
    grants.get(id).hash(&mut hasher);
    enabled.hash(&mut hasher);
    hasher.finish()
}

/// 插件在 `cx` 里是不是开着的（`[plugins.<id>]` 里没有 `enabled = false`）。
fn cx_enabled(cx: &Context, id: &str) -> bool {
    cx.config(id).and_then(|config| config.get("enabled").cloned()) != Some(Value::Bool(false))
}

struct Attached {
    cx: Context,
    /// 装进这个 `Context` 时已经有的工具名（内置的、`delegate` 等），脚本不能用。
    reserved: Arc<HashSet<String>>,
    /// 这个 `Context` 里确实是我们装进去的插件。id 和别的插件撞了的不在里面，重载时不能动它。
    owned: HashSet<&'static str>,
}

/// 一次重载做了什么。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadReport {
    /// 换成了新版本的，和新出现的。
    pub reloaded: Vec<String>,
    /// 文件夹没了、已经卸掉的。
    pub removed: Vec<String>,
    /// 新版本加载不了、仍在用旧版本的：(id, 原因)。
    pub kept: Vec<(String, String)>,
}

impl ReloadReport {
    pub fn is_empty(&self) -> bool {
        self.reloaded.is_empty() && self.removed.is_empty() && self.kept.is_empty()
    }
}

/// 一个插件现在的情况：创造模式里 `plugin_reload` 把它告诉模型。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginReport {
    pub id: String,
    /// 在 `Context` 里的状态；还没装进任何 `Context` 时是 `None`。
    pub status: Option<Status>,
    /// 加载成功时导出的工具名。
    pub tools: Vec<String>,
    /// 清单里声明了、用户还没允许过的能力。
    pub missing: Vec<String>,
    /// 热重载时新版本没换上、仍在用旧版本的原因。
    pub kept: Option<String>,
    /// 连续失败被停下的原因。
    pub stopped: Option<String>,
    pub logs: Vec<super::host::LogLine>,
    /// 界面扩展：每个入口、要藏起来的内置入口、每一页实际画一次的结果（几个节点，或者错在哪）。
    pub ui: Vec<String>,
}

impl JsPlugins {
    /// 插件目录（`<数据目录>/plugins`）。
    pub fn dir(&self) -> &Path {
        &self.shared.dir
    }

    /// 插件 `id` 现在装着的界面扩展；没有激活、没有 `ui` 导出时是 `None`。
    pub fn surface(&self, id: &str) -> Option<std::sync::Arc<dyn dshr_agent::UiSurface>> {
        let state = self.shared.lock();
        state.attached.iter().find_map(|attached| {
            attached.cx.snapshot().items::<dshr_agent::UiSurfaces>().iter().find(|surface| surface.plugin() == id).cloned()
        })
    }

    /// 插件 `id` 现在的情况；插件目录里没有它时是 `None`。
    pub fn report(&self, id: &str) -> Option<PluginReport> {
        let missing = self.grant_request(id).map(|request| request.missing);
        let state = self.shared.lock();
        let loaded = state.loaded.iter().find(|loaded| loaded.plugin.id() == id)?;
        let plugin = &loaded.plugin;
        let status = state.attached.iter().find_map(|attached| attached.cx.status(id));
        let active = status == Some(Status::Active);
        let mut report = PluginReport {
            id: id.to_owned(),
            tools: if active { plugin.tool_names() } else { Vec::new() },
            status,
            missing: missing.unwrap_or_default(),
            kept: state.kept.get(id).cloned(),
            stopped: plugin.stopped(),
            logs: Vec::new(),
            ui: Vec::new(),
        };
        // 画页面要执行脚本：先放开锁。
        let plugin = plugin.clone();
        drop(state);
        if active {
            report.ui = plugin.ui_summary();
        }
        report.logs = super::host::LogBuffer::of(plugin.id()).lines();
        Some(report)
    }

    /// 插件现在的情况，写成几段话（创造模式的 `plugin_reload`、`dshr plugin dev` 用）：加载结果、工具、等授权的能力、
    /// 界面入口与每一页的预览、数据目录、最近的日志。会画页面（执行脚本）。
    pub fn describe(&self, id: &str) -> Option<String> {
        self.report(id).map(|report| super::creator::describe(&report, &self.shared.dir))
    }

    /// 设置页里这个插件的卡片要显示的东西：名字、说明、声明的能力和其中还没允许的、密钥、服务、提供的东西。
    /// 不执行脚本（不画页面），可以随时调用。一次要很多张卡片时用 [`Self::cards`]。
    pub fn card(&self, id: &str) -> Option<crate::PluginCard> {
        let requests: Vec<GrantRequest> = self.grant_request(id).into_iter().collect();
        let links = crate::links::load(&self.shared.dir);
        let state = self.shared.lock();
        let loaded = state.loaded.iter().find(|loaded| loaded.plugin.id() == id)?;
        Some(card_of(loaded, &requests, &links, &state))
    }

    /// 全部 JS 插件的卡片，按 id 排序。
    ///
    /// 设置页打开、刷新时用这个，而不是对每个插件调用一次 [`Self::card`]：等授权的清单（要把插件目录里的
    /// 清单都读一遍）、链接记录各只读一次，密钥有没有设置走缓存（[`crate::secrets::is_set`]），
    /// 插件多时不会变成“插件数 × 插件数”次磁盘读写。
    pub fn cards(&self) -> Vec<crate::PluginCard> {
        self.cards_with(&self.grant_requests())
    }

    /// 同 [`Self::cards`]，等授权的清单由调用方给（它已经读过一遍时不用再读）。
    pub fn cards_with(&self, requests: &[GrantRequest]) -> Vec<crate::PluginCard> {
        let links = crate::links::load(&self.shared.dir);
        let state = self.shared.lock();
        state.loaded.iter().map(|loaded| card_of(loaded, requests, &links, &state)).collect()
    }

    /// 扫描 `dir`（`<数据目录>/plugins`）：每个子文件夹一个插件。这里只读清单，脚本到激活时才加载。
    pub fn discover(dir: &Path) -> Self {
        let grants = grants::load(&grants::grants_path(dir));
        // 发现时还没有附着任何 `Context`，开关状态还不知道（None）；附着之后重载会用实际的开关状态。
        let loaded = manifest::discover(dir)
            .into_iter()
            .map(|found| Loaded { stamp: stamp(&found.dir, &found.id, &grants, None), plugin: JsPlugin::new(found, &grants) })
            .collect();
        let state = State { loaded, ..State::default() };
        Self { shared: Arc::new(Shared { dir: dir.to_owned(), state: Mutex::new(state), reloading: Mutex::new(()) }) }
    }

    /// 同时加载各插件的运行时（起 QuickJS、执行模块），等全部加载完（或各自超时）再返回。
    ///
    /// 不预加载时，运行时在装进 `Context`（`apply`）时一个接一个地加载，启动要等所有插件加载时间之和，
    /// 一个慢插件就能让窗口晚出现好几秒。预加载之后 `apply` 直接用加载好的运行时。`configs` 是
    /// `config.toml` 里的 `[plugins.<id>]`：写了 `enabled = false` 的不加载。
    pub fn preload(&self, configs: &[(String, Value)]) {
        let plugins: Vec<JsPlugin> = self.shared.lock().loaded.iter().map(|loaded| loaded.plugin.clone()).collect();
        std::thread::scope(|scope| {
            for plugin in &plugins {
                let config = configs.iter().find(|(id, _)| id == plugin.id()).map(|(_, config)| config);
                scope.spawn(move || plugin.preload_at_startup(config));
            }
        });
    }

    pub fn ids(&self) -> Vec<&'static str> {
        self.shared.lock().loaded.iter().map(|loaded| loaded.plugin.id()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.shared.lock().loaded.is_empty()
    }

    /// 插件目录里现在等授权的插件（按磁盘上的清单算：热重载还没换上的新版本多要的能力也在里面）。
    pub fn grant_requests(&self) -> Vec<GrantRequest> {
        let grants = grants::load(&grants::grants_path(&self.shared.dir));
        manifest::discover(&self.shared.dir)
            .into_iter()
            .filter_map(|found| {
                let declared = found.manifest.ok()?.capabilities.names();
                let missing = grants::missing(&grants, &found.id, &declared);
                (!missing.is_empty()).then_some(GrantRequest { id: found.id, declared, missing })
            })
            .collect()
    }

    /// 只看插件 `id` 是否在等授权（按磁盘上的清单算），不读别的插件的清单。
    pub fn grant_request(&self, id: &str) -> Option<GrantRequest> {
        let grants = grants::load(&grants::grants_path(&self.shared.dir));
        let found = self.find_on_disk(id)?;
        let declared = found.manifest.ok()?.capabilities.names();
        let missing = grants::missing(&grants, id, &declared);
        (!missing.is_empty()).then(|| GrantRequest { id: id.to_owned(), declared, missing })
    }

    /// 插件 `id` 现在在磁盘上的样子：先看上次所在的文件夹，不扫整个插件目录。
    fn find_on_disk(&self, id: &str) -> Option<Found> {
        let known = self.shared.lock().loaded.iter().find(|loaded| loaded.plugin.id() == id).map(|loaded| loaded.plugin.dir().to_owned());
        manifest::discover_one(&self.shared.dir, id, known.as_deref())
    }

    /// 允许 `id` 的清单里现在声明的全部能力，然后只重载这一个插件，让它装上。
    ///
    /// 这是设置页、命令行里“用户看着清单点允许”的入口。替别人（创造模式里的模型）请求授权时用
    /// [`Self::grant_exact`]：授予的必须正好是用户在审批卡片上看到的那几项。
    pub fn grant(&self, id: &str) -> Result<ReloadReport, String> {
        let found = self.find_on_disk(id).ok_or_else(|| format!("插件目录里没有 {id}"))?;
        let manifest = found.manifest?;
        grants::grant(&grants::grants_path(&self.shared.dir), id, &manifest.capabilities.names())
            .map_err(|error| format!("写不了授权记录：{error}"))?;
        Ok(self.shared.reload_only(false, Some(id)))
    }

    /// 只允许 `capabilities` 这几项（用户在审批卡片上确认过的），不重新读清单。确认之后清单里又多出来的
    /// 能力不会被顺带允许，仍然显示为等授权，插件也因此不会激活，直到再确认一次。
    pub fn grant_exact(&self, id: &str, capabilities: &[String]) -> Result<ReloadReport, String> {
        grants::grant(&grants::grants_path(&self.shared.dir), id, capabilities).map_err(|error| format!("写不了授权记录：{error}"))?;
        Ok(self.shared.reload_only(false, Some(id)))
    }

    /// 插件 `id` 已经被允许的能力（没有时是空的）。
    pub fn granted(&self, id: &str) -> Vec<String> {
        grants::load(&grants::grants_path(&self.shared.dir)).remove(id).map(|set| set.into_iter().collect()).unwrap_or_default()
    }

    /// 收回 `id` 的全部授权，然后只重载这一个插件：声明了能力的插件随即停下。
    pub fn revoke(&self, id: &str) -> Result<ReloadReport, String> {
        grants::revoke(&grants::grants_path(&self.shared.dir), id).map_err(|error| format!("写不了授权记录：{error}"))?;
        Ok(self.shared.reload_only(false, Some(id)))
    }

    /// 就地开关插件 `id`：改掉所有附着的 `Context` 里的 `enabled`，不用重启。持久化到 `config.toml` 由调用方负责。
    ///
    /// - 停用：在每个 `Context` 里撤销它的全部注册（正在执行的调用用旧版本跑完），只重算这一个插件的指纹，
    ///   不重新加载、不执行脚本。
    /// - 启用：只改配置里的标记，再只重载这一个插件——新实例在一边预加载（执行脚本，不持状态锁），
    ///   然后在每个 `Context` 里一次换上。加载失败落为“激活失败”，原因在状态里。
    ///
    /// 不持着状态锁去启停：启停会激活插件、发布快照，设置页同时在读卡片、日志，持锁会让界面卡住。
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<ReloadReport, String> {
        let contexts: Vec<Context> = {
            let state = self.shared.lock();
            if !state.loaded.iter().any(|loaded| loaded.plugin.id() == id) {
                return Err(format!("插件目录里没有 {id}"));
            }
            state.attached.iter().map(|attached| attached.cx.clone()).collect()
        };
        if enabled {
            // 直接 `enable` 会先把旧的那一份激活一遍（停用的插件没有预加载，要执行脚本），
            // 随后的重载又换一次：只改标记，让重载一次换上预加载好的新实例。
            for cx in &contexts {
                if matches!(cx.status(id), Some(Status::Failed(_))) {
                    // 本来就是开着、只是激活失败的：开关状态没变、指纹不变，重载不会再试，照旧重新激活一次。
                    let _ = cx.enable(id);
                } else {
                    cx.mark_enabled(id, true);
                }
            }
            return Ok(self.shared.reload_only(false, Some(id)));
        }
        for cx in &contexts {
            let _ = cx.disable(id);
        }
        // 停用不需要新版本：只更新指纹里的开关状态，免得下一次重载把它当成“改过了”再换一次。
        let grants = grants::load(&grants::grants_path(&self.shared.dir));
        let dir = {
            let state = self.shared.lock();
            state.loaded.iter().find(|loaded| loaded.plugin.id() == id).map(|loaded| loaded.plugin.dir().to_owned())
        };
        if let Some(dir) = dir {
            let enabled = contexts.first().map(|cx| cx_enabled(cx, id));
            let fresh = stamp(&dir, id, &grants, enabled);
            let mut state = self.shared.lock();
            if let Some(loaded) = state.loaded.iter_mut().find(|loaded| loaded.plugin.id() == id) {
                loaded.stamp = fresh;
            }
        }
        // 和启用一侧一致：报告里列出这次开关动到的插件（虽然停用不换新版本）。
        Ok(ReloadReport { reloaded: vec![id.to_owned()], ..ReloadReport::default() })
    }

    /// 插件 `id` 最近的日志（`console.*`、`host.log`、工具出错），旧的在前。热重载前后接着记。
    pub fn logs(&self, id: &str) -> Vec<super::host::LogLine> {
        let state = self.shared.lock();
        match state.loaded.iter().find(|loaded| loaded.plugin.id() == id) {
            Some(loaded) => super::host::LogBuffer::of(loaded.plugin.id()).lines(),
            None => Vec::new(),
        }
    }

    /// 连续失败、这次运行里被停下的插件：(id, 原因)，按 id 排序。改了文件（热重载）或重启后恢复。
    pub fn stopped(&self) -> Vec<(String, String)> {
        let state = self.shared.lock();
        state.loaded.iter().filter_map(|loaded| Some((loaded.plugin.id().to_owned(), loaded.plugin.stopped()?))).collect()
    }

    /// 热重载时新版本加载不了、仍在用旧版本的插件：(id, 原因)，按 id 排序。
    pub fn kept(&self) -> Vec<(String, String)> {
        let mut kept: Vec<_> = self.shared.lock().kept.iter().map(|(id, reason)| (id.clone(), reason.clone())).collect();
        kept.sort();
        kept
    }

    /// 现在就把插件目录重新看一遍：改过的换成新版本，新出现的装上，文件夹没了的卸掉。
    /// 作用于此前 `add_group` 过的每一个 `Context`。
    pub fn reload(&self) -> ReloadReport {
        self.shared.reload(false)
    }

    /// 起一条线程盯着插件目录，有变化就重载。返回的 [`Watcher`] 丢弃时线程停下；
    /// 要它一直跑到程序退出，用 [`Watcher::detach`]。
    ///
    /// 用系统的文件变化通知（macOS FSEvents、Windows ReadDirectoryChangesW、Linux inotify）：
    /// 目录没有变化时线程一直睡着，不会每隔几百毫秒把所有插件的清单和文件扫一遍。拿不到通知时退回到定时查看。
    pub fn watch(&self) -> Watcher {
        match self.watch_events() {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!(%error, "收不到插件目录的文件变化通知，改为每 {} 秒查看一次", POLL_INTERVAL.as_secs());
                self.watch_every(POLL_INTERVAL)
            }
        }
    }

    fn watch_events(&self) -> Result<Watcher, String> {
        use notify::Watcher as _;

        std::fs::create_dir_all(&self.shared.dir).map_err(|error| error.to_string())?;
        let (sender, receiver) = std::sync::mpsc::channel::<Signal>();
        let changes = sender.clone();
        let mut events = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok_and(|event| !event.kind.is_access()) {
                let _ = changes.send(Signal::Changed);
            }
        })
        .map_err(|error| error.to_string())?;
        events.watch(&self.shared.dir, notify::RecursiveMode::Recursive).map_err(|error| error.to_string())?;
        // 链接到别处的插件文件夹（`dshr plugin link`）也盯着。
        let mut linked = crate::links::load(&self.shared.dir);
        for folder in linked.values() {
            let _ = events.watch(folder, notify::RecursiveMode::Recursive);
        }
        let shared = self.shared.clone();
        let thread = std::thread::Builder::new()
            .name("dshr-js-watch".into())
            .spawn(move || {
                // 通知来源跟着线程走：`detach` 之后也一直有效。
                let mut events = events;
                loop {
                    match receiver.recv_timeout(POLL_INTERVAL) {
                        Ok(Signal::Changed) => {}
                        Ok(Signal::Stop) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        // 没有文件变化：看一眼链接记录变了没有（链接、取消链接不在插件目录里，收不到通知）。
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            let now = crate::links::load(&shared.dir);
                            if now == linked {
                                continue;
                            }
                            for (id, folder) in &now {
                                if linked.get(id) != Some(folder) {
                                    let _ = events.watch(folder, notify::RecursiveMode::Recursive);
                                }
                            }
                            for (id, folder) in &linked {
                                if now.get(id) != Some(folder) {
                                    let _ = events.unwatch(folder);
                                }
                            }
                            linked = now;
                        }
                    }
                    // 等目录安静下来再看；改动没看稳（要连续两次看到的一样）时隔一会儿再看一次。
                    loop {
                        match receiver.recv_timeout(SETTLE) {
                            Ok(Signal::Changed) => continue,
                            Ok(Signal::Stop) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        log_report(&shared.reload(true));
                        if !shared.has_pending() {
                            break;
                        }
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Watcher { stop: Arc::new(AtomicBool::new(false)), signal: Some(sender), thread: Some(thread) })
    }

    /// 定时查看插件目录（测试、或拿不到文件变化通知时用）。
    pub fn watch_every(&self, interval: Duration) -> Watcher {
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopped) = (self.shared.clone(), stop.clone());
        let thread = std::thread::Builder::new().name("dshr-js-watch".into()).spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                log_report(&shared.reload(true));
            }
        });
        if let Err(error) = &thread {
            tracing::error!(%error, "起不了监视插件目录的线程，改了插件要重启才生效");
        }
        Watcher { stop, signal: None, thread: thread.ok() }
    }
}

/// 发给监视线程的信号。
enum Signal {
    Changed,
    Stop,
}

fn log_report(report: &ReloadReport) {
    for id in &report.reloaded {
        tracing::info!(plugin = id, "插件已重新加载");
    }
    for id in &report.removed {
        tracing::info!(plugin = id, "插件文件夹没了，已卸载");
    }
}

/// 一张设置页卡片。`requests` 是等授权的插件（可以只含这一个），`links` 是链接记录；持着状态锁调用，不执行脚本。
fn card_of(
    loaded: &Loaded,
    requests: &[GrantRequest],
    links: &std::collections::BTreeMap<String, PathBuf>,
    state: &State,
) -> crate::PluginCard {
    let plugin = &loaded.plugin;
    let id = plugin.id();
    let missing = requests.iter().find(|request| request.id == id).map(|request| request.missing.clone());
    let manifest = plugin.inner.manifest.as_ref();
    let declared = manifest.map(|manifest| manifest.capabilities.names()).unwrap_or_default();
    let secrets = manifest
        .map(|manifest| manifest.secrets.iter().map(|name| (name.clone(), crate::secrets::is_set(id, name))).collect())
        .unwrap_or_default();
    crate::PluginCard {
        id: id.to_owned(),
        name: manifest.ok().and_then(|manifest| manifest.name.clone()),
        version: manifest.ok().and_then(|manifest| manifest.version.clone()),
        description: manifest.ok().and_then(|manifest| manifest.description.clone()),
        declared,
        missing: missing.unwrap_or_default(),
        secrets,
        services: plugin.service_status(),
        provides: plugin.provides(),
        status: state.attached.iter().find_map(|attached| attached.cx.status(id)),
        kept: state.kept.get(id).cloned(),
        stopped: plugin.stopped(),
        linked: links.get(id).cloned(),
        mcp_servers: {
            let status = super::bundle::server_status(id);
            manifest
                .map(|manifest| {
                    manifest
                        .bundle
                        .mcp_servers
                        .iter()
                        .map(|(name, server)| (name.clone(), server.describe(), status.iter().find(|status| status.name == *name).cloned()))
                        .collect()
                })
                .unwrap_or_default()
        },
    }
}

impl PluginGroup for JsPlugins {
    /// 要在内置插件之后调用：此刻 `cx` 里已有的工具名，脚本都不能用。
    /// id 和已有插件重复的跳过并记一条警告，不让启动失败。
    fn add_to(self, cx: &Context) -> Result<(), ContextError> {
        // 宿主自己写的插件事件（审查结论）怎么显示：只装一次。
        match cx.add(super::hooks::HostEvents) {
            Ok(()) | Err(ContextError::DuplicatePlugin(_)) => {}
            Err(error) => return Err(error),
        }
        let snapshot = cx.snapshot();
        let reserved: Arc<HashSet<String>> = Arc::new(snapshot.items::<Tools>().iter().map(|tool| tool.name().to_owned()).collect());
        let mut state = self.shared.lock();
        // 指纹里的开关状态现在才是真的：之前按“还不知道”（None）算的换过来，不然下一次重载会空换一次。
        let grants = grants::load(&grants::grants_path(&self.shared.dir));
        for loaded in &mut state.loaded {
            let enabled = Some(cx_enabled(cx, loaded.plugin.id()));
            loaded.stamp = stamp(loaded.plugin.dir(), loaded.plugin.id(), &grants, enabled);
        }
        let mut owned = HashSet::new();
        for loaded in &state.loaded {
            let plugin = JsPlugin { inner: loaded.plugin.inner.clone(), reserved: reserved.clone() };
            let id = plugin.id();
            match cx.add(plugin) {
                Ok(()) => {
                    owned.insert(id);
                }
                Err(ContextError::DuplicatePlugin(id)) => {
                    tracing::warn!(plugin = id, "脚本插件的 id 和已有插件重复，已跳过");
                }
                Err(error) => return Err(error),
            }
        }
        state.attached.push(Attached { cx: cx.clone(), reserved, owned });
        Ok(())
    }
}

impl Shared {
    /// 还有看到了变化、但还没看稳（等下一次再看）的插件。
    fn has_pending(&self) -> bool {
        !self.lock().pending.is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `settled`：只处理连续两次看到的指纹都一样的变化——编辑器保存到一半的文件不去加载。
    /// 加载新版本要执行脚本（可能好几秒），这段时间不持有状态锁：界面上读 `kept`、`stopped`、`logs`、
    /// `surface` 的调用不会被卡住。重载之间用 `reloading` 串起来，同一时间只有一次。
    fn reload(&self, settled: bool) -> ReloadReport {
        self.reload_only(settled, None)
    }

    /// 同 [`Self::reload`]；`only` 给了时只看这一个插件（不卸载别的、也不给别的算指纹）。
    fn reload_only(&self, settled: bool, only: Option<&str>) -> ReloadReport {
        let _serial = self.reloading.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut report = ReloadReport::default();
        let grants = grants::load(&grants::grants_path(&self.dir));
        // 开关以第一个附着的 `Context` 为准（`set_enabled` 会把它们改得一样）；
        // 还没附着任何 `Context` 时按发现时的状态（None）算，不然第一次重载会空换一次。只拿一次锁。
        let first = self.lock().attached.first().map(|attached| attached.cx.clone());
        // 只重载一个插件时不读别的插件的清单（插件多时开关、授权会很慢）。
        let discovered: Vec<Found> = match only {
            Some(id) => {
                let known = self.lock().loaded.iter().find(|loaded| loaded.plugin.id() == id).map(|loaded| loaded.plugin.dir().to_owned());
                manifest::discover_one(&self.dir, id, known.as_deref()).into_iter().collect()
            }
            None => manifest::discover(&self.dir),
        };
        // 指纹要把文件夹里的文件都看一遍，也在锁外算。
        let found: Vec<(Found, u64)> = discovered
            .into_iter()
            .map(|found| {
                let enabled = first.as_ref().map(|cx| cx_enabled(cx, &found.id));
                let stamp = stamp(&found.dir, &found.id, &grants, enabled);
                (found, stamp)
            })
            .collect();

        // 第一步（持锁，只做便宜的事）：卸掉文件夹没了的，挑出要换版本的，记下要换进哪些 `Context`。
        let mut work = Vec::new();
        // 被换下或卸掉的旧版本：锁外调用它们的 `deactivate`。
        let mut retired: Vec<JsPlugin> = Vec::new();
        {
            let mut state = self.lock();
            let state = &mut *state;
            let gone: Vec<&'static str> = state
                .loaded
                .iter()
                .map(|loaded| loaded.plugin.id())
                .filter(|id| only.is_none() && !found.iter().any(|(found, _)| found.id == *id))
                .collect();
            for id in gone {
                if settled && state.pending.insert(id.to_owned(), 0) != Some(0) {
                    continue;
                }
                state.pending.remove(id);
                if let Some(loaded) = state.loaded.iter().find(|loaded| loaded.plugin.id() == id) {
                    retired.push(loaded.plugin.clone());
                }
                for attached in &mut state.attached {
                    if attached.owned.remove(id) {
                        let _ = attached.cx.remove(id);
                    }
                }
                state.loaded.retain(|loaded| loaded.plugin.id() != id);
                state.kept.remove(id);
                report.removed.push(id.to_owned());
            }

            for (found, stamp) in found {
                let current = state.loaded.iter().position(|loaded| loaded.plugin.id() == found.id);
                if current.is_some_and(|index| state.loaded[index].stamp == stamp) {
                    state.pending.remove(&found.id);
                    continue;
                }
                if settled && state.pending.insert(found.id.clone(), stamp) != Some(stamp) {
                    continue;
                }
                state.pending.remove(&found.id);
                let targets = state.targets(&found.id);
                let contexts: Vec<(Context, Arc<HashSet<String>>)> =
                    targets.iter().map(|&index| (state.attached[index].cx.clone(), state.attached[index].reserved.clone())).collect();
                work.push((found, stamp, targets, contexts));
            }
        }

        // 第二步（不持锁）：在一边加载、检查新版本；第三步（持锁）：换上或留着旧的。
        for (found, stamp, targets, contexts) in work {
            let fresh = JsPlugin::new(found, &grants);
            // 旧版本导出了 `state` 时，先拿它的快照交给新版本（新版本加载时 `state.restore`）。
            let old = self.lock().loaded.iter().find(|loaded| loaded.plugin.id() == fresh.id()).map(|loaded| loaded.plugin.clone());
            if let Some(snapshot) = old.as_ref().and_then(JsPlugin::snapshot) {
                fresh.set_restore(snapshot);
            }
            let problem = preload(&fresh, &contexts);
            let id = fresh.id().to_owned();
            let mut state = self.lock();
            match state.swap(fresh, &grants, stamp, &targets, problem) {
                Ok(()) => {
                    state.kept.remove(&id);
                    report.reloaded.push(id);
                    retired.extend(old);
                }
                Err(reason) => {
                    tracing::warn!(plugin = id, reason, "插件的新版本加载不了，继续用旧版本");
                    state.kept.insert(id.clone(), reason.clone());
                    report.kept.push((id, reason));
                }
            }
        }
        self.lock().loaded.sort_by_key(|loaded| loaded.plugin.id());
        for plugin in retired {
            plugin.deactivate();
        }
        report
    }
}

/// 在每个要换进去的 `Context` 的环境下预加载 `fresh`；返回第一个问题。被停用的插件不执行它的代码。
fn preload(fresh: &JsPlugin, contexts: &[(Context, Arc<HashSet<String>>)]) -> Option<String> {
    let id = fresh.id();
    let enabled = |cx: &Context| cx_enabled(cx, id);
    for (cx, reserved) in contexts {
        if !enabled(cx) {
            continue;
        }
        let candidate = JsPlugin { inner: fresh.inner.clone(), reserved: reserved.clone() };
        if let Err(reason) = candidate.preload(&script_config(cx.config(id))) {
            return Some(reason);
        }
    }
    None
}

impl State {
    /// 哪些 `Context` 里有（或者该有）插件 `id`：已经是我们装的，或者那里还没有这个 id。
    fn targets(&self, id: &str) -> Vec<usize> {
        (0..self.attached.len())
            .filter(|index| {
                let attached = &self.attached[*index];
                attached.owned.contains(id) || attached.cx.status(id).is_none()
            })
            .collect()
    }

    /// 换上已经预加载过的 `fresh`（`problem` 是预加载的结果）。`Err` 表示新版本不行、而旧版本还能用，于是什么都没换。
    fn swap(&mut self, fresh: JsPlugin, grants: &Grants, stamp: u64, targets: &[usize], problem: Option<String>) -> Result<(), String> {
        let id = fresh.id();
        let current = self.loaded.iter().position(|loaded| loaded.plugin.id() == id);
        if let Some(reason) = problem {
            // 旧版本还在工作、它要的能力也仍然都允许着，就留着它；记下这次的指纹，文件再变之前不重复尝试。
            // 授权被收回的不留：不然收回授权后旧版本照样跑。
            let working = targets.iter().any(|&index| self.attached[index].cx.status(id) == Some(Status::Active));
            let still_granted = current.is_some_and(|index| match &self.loaded[index].plugin.inner.manifest {
                Ok(old) => grants::missing(grants, id, &old.capabilities.names()).is_empty(),
                Err(_) => false,
            });
            if working && still_granted {
                if let Some(index) = current {
                    self.loaded[index].stamp = stamp;
                }
                return Err(reason);
            }
            // 没有能用的旧版本：换上新的，让“激活失败”的原因更新成现在这个。
        }

        for &index in targets {
            let attached = &mut self.attached[index];
            let plugin = JsPlugin { inner: fresh.inner.clone(), reserved: attached.reserved.clone() };
            match attached.cx.replace(plugin) {
                Ok(()) => {
                    attached.owned.insert(id);
                }
                // 版本没变（比如开关拨了又拨回来）：不用换，但从现在起它算我们装的。
                Err(ContextError::IdenticalPlugin(_)) => {
                    attached.owned.insert(id);
                }
                Err(_) => {}
            }
        }
        match current {
            Some(index) => self.loaded[index] = Loaded { plugin: fresh, stamp },
            None => self.loaded.push(Loaded { plugin: fresh, stamp }),
        }
        Ok(())
    }
}

/// 盯着插件目录的线程。丢弃时线程停下。
pub struct Watcher {
    stop: Arc<AtomicBool>,
    /// 按文件变化通知工作时，用它叫停线程。
    signal: Option<std::sync::mpsc::Sender<Signal>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    /// 让它一直跑到程序退出。
    pub fn detach(mut self) {
        self.thread = None;
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(signal) = &self.signal {
                let _ = signal.send(Signal::Stop);
            }
            let _ = thread.join();
        }
    }
}
