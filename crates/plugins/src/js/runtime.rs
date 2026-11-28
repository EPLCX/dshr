//! 一个插件一条线程，线程里一个 QuickJS 运行时。
//!
//! QuickJS 的运行时不能跨线程用，而工具要 `Send + Sync`，所以这里是个 actor：工具把调用发进 channel，
//! 线程逐个执行、把结果发回去。同一个插件的调用因此是排队的，不同插件之间互不影响。
//! 所有发送端都释放后线程自己退出。
//!
//! 每次执行是一个小事件循环（[`drive`]）：跑微任务 → 触发到期的定时器 → 等慢操作（`ops.rs`）完成并把结果
//! 交给脚本 → 直到入口函数返回的 Promise 落定，或者超时、被中断。

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dshr_tools::{ToolContext, ToolOutput, truncate};
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{CatchResultExt, CaughtError, Context, Ctx, Error, Function, Module, Object, Promise, Runtime, Value};
use serde::Deserialize;
use tokio::sync::oneshot;

use super::host::{self, Call, Permits, State};
use crate::manifest::PluginManifest;

const PRELUDE: &str = include_str!("prelude.js");
/// 语言本身之外的常用全局（`TextEncoder`、`URL`、`crypto`……），在前导脚本之前执行。
const GLOBALS: &str = include_str!("globals.js");

/// 脚本能用的内存上限。超过后分配失败，脚本里抛异常。
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// 脚本的调用栈上限；线程本身的栈留得比它大。
const STACK_LIMIT: usize = 512 * 1024;
const THREAD_STACK: usize = 4 * 1024 * 1024;
/// 加载模块（执行顶层代码、取出工具列表、`activate`）的时限。
const LOAD_TIMEOUT: Duration = Duration::from_secs(2);
/// 一次调用结束后，清理它留下的微任务最多花这么久。
const DRAIN_TIMEOUT: Duration = Duration::from_millis(200);
/// 事件循环里等慢操作时，最多隔这么久看一次有没有被中断、有没有定时器到期。
const POLL: Duration = Duration::from_millis(20);

/// 连续这么多次工具调用都跑到时限被打断（或内存耗尽），就认定插件出了问题：这次运行里不再执行它，
/// 免得每一步都白等一次超时。正常返回或脚本自己抛的异常都会清零；热重载换上新版本、重启后恢复。
pub(crate) const STRIKES: u32 = 3;

/// 插件线程记下的“健康状况”，工具那一边读。
#[derive(Default)]
pub(crate) struct Health {
    strikes: AtomicU32,
    stopped: Mutex<Option<String>>,
}

impl Health {
    /// 已经被停下时的原因。
    pub fn stopped(&self) -> Option<String> {
        self.stopped.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// 一次工具调用的结果：`runaway` 是“跑到时限 / 内存耗尽”时的错误信息，其余是 `None`。
    fn record(&self, runaway: Option<&str>) {
        let Some(error) = runaway else {
            self.strikes.store(0, Ordering::Relaxed);
            return;
        };
        if self.strikes.fetch_add(1, Ordering::Relaxed) + 1 >= STRIKES {
            let mut stopped = self.stopped.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            stopped.get_or_insert_with(|| {
                let first = error.lines().next().unwrap_or_default();
                format!("连续 {STRIKES} 次工具调用都没能正常结束（最近一次：{first}）")
            });
        }
    }
}

/// 模块导出的一个工具。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolDecl {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(default)]
    pub read_only: bool,
}

/// 模块导出的东西。
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Exports {
    pub tools: Vec<ToolDecl>,
    /// `events` 导出里登记了投影函数的事件类型。
    pub events: Vec<String>,
    /// `events` 导出里登记了 `label` 函数（对话区里显示的一行字）的事件类型。
    #[serde(default)]
    pub labels: Vec<String>,
    /// `events` 导出里登记了 `render` 函数（对话区里的节点树）的事件类型。
    #[serde(default)]
    pub renders: Vec<String>,
    /// 有没有导出 `prompt` 函数。
    pub prompt: bool,
    /// `hooks` 导出里的钩子名（`beforeTool`、`afterTool`、`beforeStep`、`beforeRequest`、`turnEnded`）。
    #[serde(default)]
    pub hooks: Vec<String>,
    /// `cards` 导出：给哪些工具画了卡片。
    #[serde(default)]
    pub cards: Vec<String>,
    /// `activate`、`deactivate`、`state` 导出。
    #[serde(default)]
    pub lifecycle: LifecycleExports,
    /// `schedules` 导出：后台定时任务。
    #[serde(default)]
    pub schedules: Vec<ScheduleDecl>,
    /// `ui` 导出：入口、要藏起来的内置入口、页面名。
    #[serde(default)]
    pub ui: UiExports,
    #[serde(default)]
    pub presets: Vec<PresetDecl>,
    /// `permissions` 导出：插件提供的权限模式。
    #[serde(default)]
    pub permissions: Vec<PermissionDecl>,
    #[serde(default)]
    pub commands: Vec<CommandDecl>,
    #[serde(default)]
    pub slash: Vec<SlashDecl>,
    /// `services` 导出：常驻服务的 id。
    #[serde(default)]
    pub services: Vec<ServiceDecl>,
    #[serde(default)]
    pub providers: Vec<ProviderDecl>,
    #[serde(default)]
    pub themes: Vec<ThemeDecl>,
    /// `on` 导出：订阅的会话事件类型（`tool/result`、`todo/*`、`*`）。
    #[serde(default)]
    pub on: Vec<String>,
    /// `api` 导出：给别的插件调用的方法名。
    #[serde(default)]
    pub api: Vec<String>,
    /// `mentions` 导出：输入框 `@` 提及的来源。
    #[serde(default)]
    pub mentions: Vec<MentionDecl>,
    /// `watch` 导出：监听工作区里的文件。
    #[serde(default)]
    pub watch: Vec<WatchDecl>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WatchDecl {
    pub id: String,
    pub globs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MentionDecl {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PresetDecl {
    pub id: String,
    pub label: String,
    pub description: String,
    pub instructions: String,
    pub read_only: bool,
    pub tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PermissionDecl {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CommandDecl {
    pub id: String,
    pub title: String,
    pub keys: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SlashDecl {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ServiceDecl {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderDecl {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    /// 固定的模型列表；`None` 时 `models` 是函数，要问脚本。
    pub models: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ThemeDecl {
    pub id: String,
    pub name: String,
    pub light: std::collections::BTreeMap<String, String>,
    pub dark: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct LifecycleExports {
    #[serde(default)]
    pub activate: bool,
    #[serde(default)]
    pub deactivate: bool,
    /// 导出了 `state = { snapshot, restore }`：热重载时把状态带到新版本。
    #[serde(default)]
    pub state: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScheduleDecl {
    pub id: String,
    /// 间隔（毫秒）。
    pub every_ms: u64,
}

/// `ui` 导出里登记的东西。
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct UiExports {
    #[serde(default)]
    pub sidebar: Vec<EntryDecl>,
    #[serde(default)]
    pub settings: Vec<EntryDecl>,
    /// 输入卡片上方的面板。
    #[serde(default)]
    pub panels: Vec<EntryDecl>,
    /// 侧边栏底部的状态行：`page` 是一页，画出来的第一段文字就是状态。
    #[serde(default)]
    pub status: Vec<EntryDecl>,
    #[serde(default)]
    pub hide: Vec<String>,
    #[serde(default)]
    pub pages: Vec<String>,
}

impl UiExports {
    pub fn is_empty(&self) -> bool {
        self.sidebar.is_empty() && self.settings.is_empty() && self.panels.is_empty() && self.status.is_empty() && self.hide.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EntryDecl {
    pub id: String,
    pub label: String,
    pub icon: Option<String>,
    pub page: String,
}

/// 要插件线程做的一件事。
pub(crate) enum Job {
    /// 执行一个工具。
    Tool { name: String, input: serde_json::Value },
    /// 问这一轮要追加到系统提示词后面的内容。
    Prompt,
    /// 把一条自定义事件投影成上下文里的文本（`label` 为真时是对话区里显示的那行字）；`data` 是 JSON。
    Project { kind: String, data: String, label: bool },
    /// 页面现在的节点树（JSON 文本）。
    Render { page: String },
    /// 页面上的一个动作；`payload` 是 JSON。
    Act { page: String, action: String, payload: String },
    /// 其余的入口，交给 `prelude.js` 的 `dispatch`：`kind` 是 `hook`、`card`、`event`、`lifecycle`、`schedule`，
    /// `name` 是其中的哪一个，`payload` 是 JSON。返回 JSON 文本（或 `null`）。
    Dispatch { kind: &'static str, name: String, payload: String },
}

/// 结果送到哪里：工具调用的一方在异步代码里等，`Value` 在同步代码里等，`Async` 在异步代码里等任意结果。
pub(crate) enum Reply {
    Tool(oneshot::Sender<ToolOutput>),
    Value(mpsc::SyncSender<Result<Option<String>, String>>),
    Async(oneshot::Sender<Result<Option<String>, String>>),
    /// 不等结果（会话事件的通知）：做完或者被丢弃时计数减一。
    Done(#[allow(dead_code)] Pending),
}

/// 排队中的一件“不等结果”的事：做完（或者没做就被丢弃）时把计数减一。
pub(crate) struct Pending(pub Arc<std::sync::atomic::AtomicUsize>);

impl Drop for Pending {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) struct Command {
    pub job: Job,
    /// 这件事能用的工作区；没有时 `host` 上的文件函数不可用。
    pub ctx: Option<ToolContext>,
    pub permits: Permits,
    pub timeout: Duration,
    /// 调用方不再等结果时变成 `true`：还没开始的不执行，执行中的被打断。
    pub cancelled: Arc<AtomicBool>,
    pub reply: Reply,
}

/// 一个已经加载好的插件：它的线程和它导出的东西。
pub(crate) struct Instance {
    commands: mpsc::Sender<Command>,
    pub exports: Exports,
    /// 同步的询问（[`Instance::ask`]）超时之后，到这个时刻之前的询问直接放弃：
    /// 线程正忙着的时候，不让一长串事件每条都等到超时。
    busy_until: Mutex<Option<Instant>>,
    pub health: Arc<Health>,
    /// 每一页被脚本要求重画（`host.ui.refresh`）的次数。
    pub refresh: Arc<Mutex<std::collections::HashMap<String, u64>>>,
    /// `host.tools.set` 设的动态工具。
    pub dynamic: Arc<DynamicTools>,
    /// `host.ui.hide` / `show` 改的入口显隐。
    pub visibility: Arc<EntryVisibility>,
    /// 脚本看到的配置（`host.config`）：插件自己写入、用户在设置页修改后就地换掉。
    pub config: Arc<super::config::LiveConfig>,
}

/// `host.tools.set([...])` 设的工具：整组替换，下一步起生效。只属于这个运行时（热重载后的新版本从空的开始）。
#[derive(Default)]
pub(crate) struct DynamicTools {
    pub decls: Mutex<Vec<ToolDecl>>,
    /// 不能用的名字：装进 `Context` 时已有的工具、插件自己静态导出的工具。
    pub reserved: Mutex<std::collections::HashSet<String>>,
}

/// 入口的显隐（`host.ui.hide` / `show`）：插件运行时改，界面每次画的时候读。只属于这个运行时（热重载后回到 `ui.hide`）。
#[derive(Default)]
pub(crate) struct EntryVisibility {
    /// 插件自己的入口：`sidebar:<id>`、`settings:<id>`、`panel:<id>`、`status:<id>`。加载完导出后填。
    pub own: Mutex<std::collections::BTreeSet<String>>,
    /// 藏着的自己的入口（键同 `own`）。
    pub hidden_own: Mutex<std::collections::BTreeSet<String>>,
    /// 藏着的内置入口（`dshr_agent::hideable` 认可的）。初值是 `ui.hide`。
    pub hidden_builtin: Mutex<std::collections::BTreeSet<String>>,
}

impl EntryVisibility {
    fn load(&self, ui: &UiExports) {
        let keys = |place: &str, decls: &[EntryDecl]| decls.iter().map(|decl| format!("{place}:{}", decl.id)).collect::<Vec<_>>();
        let mut own = self.own.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        own.clear();
        own.extend(keys("sidebar", &ui.sidebar));
        own.extend(keys("settings", &ui.settings));
        own.extend(keys("panel", &ui.panels));
        own.extend(keys("status", &ui.status));
        drop(own);
        let mut builtin = self.hidden_builtin.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        builtin.clear();
        builtin.extend(ui.hide.iter().filter(|target| dshr_agent::hideable(target)).cloned());
    }

    /// 藏起或显示 `target`：先找插件自己的入口，找不到再当作内置入口。返回有没有变化。
    pub fn set(&self, target: &str, hidden: bool) -> Result<bool, String> {
        let lock = |set: &Mutex<std::collections::BTreeSet<String>>| -> bool {
            let mut set = set.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if hidden { set.insert(target.to_owned()) } else { set.remove(target) }
        };
        if self.own.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(target) {
            return Ok(lock(&self.hidden_own));
        }
        if dshr_agent::hideable(target) {
            return Ok(lock(&self.hidden_builtin));
        }
        Err(format!(
            "`{target}` is neither one of this plugin's entries (sidebar:<id>, settings:<id>, panel:<id>, status:<id>) nor a built-in entry \
             that can be hidden (settings:<page>, sidebar:<id>, status:<id>, composer:presets, composer:permission)"
        ))
    }

    /// 现在藏着的全部：自己的入口和内置入口。
    pub fn hidden(&self) -> Vec<String> {
        let own = self.hidden_own.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        let builtin = self.hidden_builtin.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        own.into_iter().chain(builtin.into_iter().filter(|target| !self.is_own(target))).collect()
    }

    fn is_own(&self, target: &str) -> bool {
        self.own.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(target)
    }
}

/// 一次询问超时后，多久之内不再问。
const BUSY_BACKOFF: Duration = Duration::from_secs(2);

impl Instance {
    /// 在同步代码里问插件一件事，最多等 `wait`（含排队）。超时、出错都是 `Err`。
    /// 调用方所在的线程会被占住这么久，所以 `wait` 要短。
    pub fn ask(&self, job: Job, ctx: Option<ToolContext>, wait: Duration) -> Result<Option<String>, String> {
        self.ask_with(job, ctx, wait, Permits::default())
    }

    /// 同 [`Self::ask`]，但脚本可以做 `permits` 允许的事（如用户在页面上亲手触发的动作）。
    pub fn ask_with(&self, job: Job, ctx: Option<ToolContext>, wait: Duration, permits: Permits) -> Result<Option<String>, String> {
        if let Some(reason) = self.health.stopped() {
            return Err(reason);
        }
        // 只在检查和写入退避时短暂加锁；等回答期间不持锁，否则别的调用（比如界面线程上的 label）
        // 会排在一个长时间运行的 act 后面，连自己的超时都开始不了。
        let busy = |until: &Option<Instant>| until.is_some_and(|until| Instant::now() < until);
        if busy(&self.busy_until.lock().unwrap_or_else(|poisoned| poisoned.into_inner())) {
            return Err("the plugin is busy".into());
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let (reply, result) = mpsc::sync_channel(1);
        let command = Command { job, ctx, permits, timeout: wait, cancelled: cancelled.clone(), reply: Reply::Value(reply) };
        if self.send(command).is_err() {
            return Err("the plugin's runtime has stopped".into());
        }
        let started = Instant::now();
        let answer = result.recv_timeout(wait).unwrap_or_else(|_| {
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            Err(format!("the plugin did not answer within {} ms", wait.as_millis()))
        });
        // 等了很久还没问到（线程在忙别的，或者脚本自己跑到了时限）：接下来一阵子先不问了。
        if answer.is_err() && started.elapsed() >= wait / 2 {
            *self.busy_until.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now() + BUSY_BACKOFF);
        }
        answer
    }

    /// 在异步代码里让插件做一件事，最多等 `timeout`（含排队）。返回的 future 被丢弃时这件事随之取消。
    pub async fn call(&self, job: Job, ctx: Option<ToolContext>, timeout: Duration, permits: Permits) -> Result<Option<String>, String> {
        struct CancelOnDrop(Arc<AtomicBool>);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        if let Some(reason) = self.health.stopped() {
            return Err(reason);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel = CancelOnDrop(cancelled.clone());
        let (reply, result) = oneshot::channel();
        let command = Command { job, ctx, permits, timeout, cancelled, reply: Reply::Async(reply) };
        if self.send(command).is_err() {
            return Err("the plugin's runtime has stopped".into());
        }
        match tokio::time::timeout(timeout + Duration::from_millis(500), result).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err("the plugin's runtime stopped before it answered".into()),
            Err(_) => Err(format!("the plugin did not answer within {} ms", timeout.as_millis())),
        }
    }

    /// 起线程、加载模块。模块有语法错误、顶层代码抛异常或超时，都在这里返回原因。
    /// `restore` 是上一个版本 `state.snapshot()` 的结果（热重载），在 `activate` 之前交给 `state.restore`。
    pub fn start(
        plugin: &'static str,
        manifest: &PluginManifest,
        config: &serde_json::Value,
        restore: Option<String>,
    ) -> Result<Self, String> {
        let (commands, inbox) = mpsc::channel();
        let (ready, loaded) = mpsc::sync_channel(1);
        let (manifest, config) = (manifest.clone(), config.to_string());
        let health = Arc::new(Health::default());
        let recorder = health.clone();
        let refresh = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let pages = refresh.clone();
        let dynamic = Arc::new(DynamicTools::default());
        let tools = dynamic.clone();
        let visibility = Arc::new(EntryVisibility::default());
        let entries = visibility.clone();
        let live = Arc::new(super::config::LiveConfig::new(config.clone()));
        let live_config = live.clone();
        std::thread::Builder::new()
            .name(format!("dshr-js-{plugin}"))
            .stack_size(THREAD_STACK)
            .spawn(move || run(plugin, &manifest, &config, restore, pages, tools, entries, live_config, &recorder, &ready, &inbox))
            .map_err(|error| format!("起不了插件线程：{error}"))?;
        // 加载本身有时限（`activate` 另有一份）；这里多等一会儿只是兜底。
        match loaded.recv_timeout(LOAD_TIMEOUT * 2 + Duration::from_secs(5)) {
            Ok(Ok(exports)) => {
                Ok(Self { commands, exports, busy_until: Mutex::new(None), health, refresh, dynamic, visibility, config: live })
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err("插件加载没有响应".into()),
        }
    }

    /// 把一次调用排进队列；线程已经不在时把调用原样还回来。
    pub fn send(&self, command: Command) -> Result<(), Box<Command>> {
        self.commands.send(command).map_err(|error| Box::new(error.0))
    }

    /// 页面 `page` 被要求重画的次数。
    pub fn refresh_generation(&self, page: &str) -> u64 {
        let pages = self.refresh.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        pages.get(page).copied().unwrap_or(0)
    }
}

type Loaded = Result<Exports, String>;

/// `prelude.js` 给的入口函数。
struct Entries<'js> {
    call: Function<'js>,
    prompt: Function<'js>,
    project: Function<'js>,
    render: Function<'js>,
    act: Function<'js>,
    dispatch: Function<'js>,
    /// 触发到期的定时器，返回离下一个定时器还有多少毫秒（没有时是 -1）。
    tick: Function<'js>,
    /// 一个慢操作完成了：`(编号, 成功与否, 文本)`。
    settle: Function<'js>,
    /// 一次调用结束：清掉没完成的操作和定时器。
    reset: Function<'js>,
}

#[allow(clippy::too_many_arguments)]
fn run(
    plugin: &'static str,
    manifest: &PluginManifest,
    config: &str,
    restore: Option<String>,
    refresh: Arc<Mutex<std::collections::HashMap<String, u64>>>,
    dynamic: Arc<DynamicTools>,
    visibility: Arc<EntryVisibility>,
    live_config: Arc<super::config::LiveConfig>,
    health: &Health,
    ready: &mpsc::SyncSender<Loaded>,
    inbox: &mpsc::Receiver<Command>,
) {
    let state =
        Rc::new(State::new(plugin, manifest.capabilities.clone(), manifest.secrets.clone(), super::surface::data_dir(manifest), refresh));
    *state.dynamic.borrow_mut() = Some(dynamic);
    *state.visibility.borrow_mut() = Some(visibility.clone());
    *state.live_config.borrow_mut() = Some(live_config);
    *state.config_schema.borrow_mut() = manifest.config_schema.clone();
    let setup = || -> rquickjs::Result<(Runtime, Context)> {
        let runtime = Runtime::new()?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        runtime.set_max_stack_size(STACK_LIMIT);
        let interrupt = state.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || interrupt.should_interrupt())));
        runtime.set_loader(DirResolver { imports: manifest.imports.clone() }, DirLoader { dir: manifest.dir.clone() });
        let context = Context::full(&runtime)?;
        Ok((runtime, context))
    };
    let (_runtime, context) = match setup() {
        Ok(pair) => pair,
        Err(error) => {
            let _ = ready.send(Err(format!("建不了 JS 运行时：{error}")));
            return;
        }
    };
    // 整条线程的生命都在这一次 `with` 里：模块和入口函数的句柄不用跨出去。
    context.with(|ctx| {
        state.arm(LOAD_TIMEOUT, None);
        let loaded = load(&ctx, &state, manifest, config);
        state.disarm();
        let (entries, exports) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        // 入口先按导出登记好：`activate` 里就可以 `host.ui.hide`。
        visibility.load(&exports.ui);
        // 热重载时先把旧版本的状态交给新版本，再 `activate`。都在加载的时限里，失败就是加载失败。
        let lifecycle = |name: &str, payload: String| -> Result<(), String> {
            let job = Job::Dispatch { kind: "lifecycle", name: name.to_owned(), payload };
            match run_job(&ctx, &state, &entries, &job, None, Permits::default(), LOAD_TIMEOUT, Arc::default()) {
                Ok(_) => Ok(()),
                Err(error) => Err(format!("{name} 失败：{error}")),
            }
        };
        if let (Some(snapshot), true) = (restore, exports.lifecycle.state)
            && let Err(error) = lifecycle("restore", snapshot)
        {
            // 状态迁移失败不阻止新版本加载：从空白状态开始，并记一条日志。
            state.logs.push("warn", &error);
            tracing::warn!(plugin, error, "插件没能恢复热重载前的状态");
        }
        if exports.lifecycle.activate
            && let Err(error) = lifecycle("activate", "null".into())
        {
            let _ = ready.send(Err(error));
            return;
        }
        let _ = ready.send(Ok(exports));
        while let Ok(command) = inbox.recv() {
            execute(&ctx, &state, &entries, health, command);
        }
    });
}

/// 执行前导脚本和插件的入口模块，返回入口函数和模块导出的东西。
fn load<'js>(ctx: &Ctx<'js>, state: &Rc<State>, manifest: &PluginManifest, config: &str) -> Result<(Entries<'js>, Exports), String> {
    let explain = |error: Error| describe(ctx, state, error);
    let native = host::install(ctx, state).map_err(explain)?;
    let globals: Function = ctx.eval(GLOBALS).map_err(explain)?;
    globals.call::<_, ()>((native.clone(),)).map_err(explain)?;
    let prelude: Function = ctx.eval(PRELUDE).map_err(explain)?;
    let api: Object = prelude.call((native,)).map_err(explain)?;

    let source =
        std::fs::read_to_string(manifest.dir.join(&manifest.main)).map_err(|error| format!("读不了 {}：{error}", manifest.main))?;
    let module = Module::declare(ctx.clone(), manifest.main.as_str(), source).map_err(explain)?;
    let (module, evaluated) = module.eval().map_err(explain)?;
    evaluated.finish::<Value>().map_err(explain)?;
    let namespace = module.namespace().map_err(explain)?;

    let register: Function = api.get("register").map_err(explain)?;
    let i18n = serde_json::json!({ "messages": manifest.messages, "defaultLocale": manifest.default_locale }).to_string();
    let exports: String = register.call((namespace, config, i18n)).map_err(explain)?;
    let exports = serde_json::from_str(&exports).map_err(|error| format!("模块导出的格式不对：{error}"))?;
    let entries = Entries {
        call: api.get("call").map_err(explain)?,
        prompt: api.get("prompt").map_err(explain)?,
        project: api.get("project").map_err(explain)?,
        render: api.get("render").map_err(explain)?,
        act: api.get("act").map_err(explain)?,
        dispatch: api.get("dispatch").map_err(explain)?,
        tick: api.get("tick").map_err(explain)?,
        settle: api.get("settle").map_err(explain)?,
        reset: api.get("reset").map_err(explain)?,
    };
    Ok((entries, exports))
}

fn execute<'js>(ctx: &Ctx<'js>, state: &Rc<State>, entries: &Entries<'js>, health: &Health, command: Command) {
    let Command { job, ctx: tool_ctx, permits, timeout, cancelled, reply } = command;
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let result = run_job(ctx, state, entries, &job, tool_ctx, permits, timeout, cancelled);
    let counts = matches!(job, Job::Tool { .. } | Job::Dispatch { kind: "hook" | "schedule", .. });
    if counts && !state.is_cancelled() {
        let runaway = match &result {
            Err(error) if error.contains("time limit") || error.contains("out of memory") => Some(error.as_str()),
            _ => None,
        };
        health.record(runaway);
    }
    // 工具、钩子出错也记进插件日志：设置页里能看到它最近为什么失败。
    match (&job, &result) {
        (Job::Tool { name, .. }, Err(error)) => state.logs.push("error", &format!("{name}: {error}")),
        (Job::Dispatch { kind, name, .. }, Err(error)) if *kind != "card" => {
            state.logs.push("error", &format!("{kind} {name}: {error}"));
        }
        _ => {}
    }
    match reply {
        Reply::Tool(reply) => {
            let output = match result {
                Ok(text) => ToolOutput::ok(truncate(text.unwrap_or_default())),
                Err(error) => ToolOutput::error(truncate(error)),
            };
            let _ = reply.send(output);
        }
        Reply::Value(reply) => {
            let _ = reply.send(result);
        }
        Reply::Async(reply) => {
            let _ = reply.send(result);
        }
        Reply::Done(_) => {}
    }
}

/// 执行一件事：开始计时、调用入口函数、驱动事件循环到它落定，再收拾残局。
#[allow(clippy::too_many_arguments)]
fn run_job<'js>(
    ctx: &Ctx<'js>,
    state: &Rc<State>,
    entries: &Entries<'js>,
    job: &Job,
    tool_ctx: Option<ToolContext>,
    permits: Permits,
    timeout: Duration,
    cancelled: Arc<AtomicBool>,
) -> Result<Option<String>, String> {
    let workspace = tool_ctx.as_ref().map(|ctx| ctx.workspace.display().to_string());
    state.begin_call(tool_ctx.map(|ctx| Call { ctx, permits }));
    // 用户亲手触发的：可以读写输入框。
    state.user_initiated.set(matches!(job, Job::Act { .. } | Job::Dispatch { kind: "command" | "slash", .. }));
    state.arm(timeout, Some(cancelled));
    // 入口都是 async 函数：拿到 Promise 后驱动事件循环到它有结果为止。
    let started: rquickjs::Result<Promise> = match job {
        Job::Tool { name, input } => entries.call.call((name.as_str(), input.to_string())),
        Job::Prompt => {
            let context = serde_json::json!({ "workspace": workspace }).to_string();
            entries.prompt.call((context,))
        }
        Job::Project { kind, data, label } => entries.project.call((kind.as_str(), data.as_str(), *label)),
        Job::Render { page } => entries.render.call((page.as_str(),)),
        Job::Act { page, action, payload } => entries.act.call((page.as_str(), action.as_str(), payload.as_str())),
        Job::Dispatch { kind, name, payload } => entries.dispatch.call((*kind, name.as_str(), payload.as_str())),
    };
    let result = started.and_then(|promise| drive(ctx, state, entries, &promise));
    let result = result.map_err(|error| describe(ctx, state, error));
    // 这次调用可能还留着没跑完的微任务（比如没 await 的 Promise）：限时跑完，不留给下一次调用。
    // 没完成的慢操作和定时器作废。
    state.arm(DRAIN_TIMEOUT, None);
    while ctx.execute_pending_job() {}
    let _ = entries.reset.call::<_, ()>(());
    state.disarm();
    state.end_call();
    result
}

/// 事件循环：直到 `promise` 落定。
fn drive<'js>(ctx: &Ctx<'js>, state: &State, entries: &Entries<'js>, promise: &Promise<'js>) -> rquickjs::Result<Option<String>> {
    let run_jobs = || while ctx.execute_pending_job() {};
    loop {
        run_jobs();
        if let Some(result) = promise.result() {
            return result;
        }
        if state.should_interrupt() {
            return Err(Error::Exception);
        }
        let next: f64 = entries.tick.call(())?;
        run_jobs();
        if let Some(result) = promise.result() {
            return result;
        }
        let timer = (next >= 0.0).then(|| Duration::from_millis(next as u64));
        if state.ops.in_flight() == 0 && timer.is_none() {
            // 没有可跑的了：Promise 永远不会落定。
            return Err(Error::WouldBlock);
        }
        let wait = timer.unwrap_or(POLL).min(POLL).min(state.remaining());
        if let Some((id, result)) = state.ops.wait(wait) {
            let (ok, text) = match result {
                Ok(text) => (true, text),
                Err(error) => (false, error),
            };
            entries.settle.call::<_, ()>((id as f64, ok, text))?;
        }
    }
}

/// 把一次失败说成人话。脚本被打断时 QuickJS 给的是一句笼统的 "interrupted"，这里换成真正的原因。
fn describe<'js>(ctx: &Ctx<'js>, state: &State, error: Error) -> String {
    // 先把挂着的异常取走，不然它会留到下一次执行。
    let caught = Err::<(), _>(error).catch(ctx).err();
    if state.is_cancelled() {
        return "Interrupted before the plugin tool finished.".into();
    }
    if let Some(limit) = state.expired() {
        return format!("The plugin tool ran longer than its time limit ({} ms) and was stopped.", limit.as_millis());
    }
    match caught {
        Some(CaughtError::Exception(exception)) => {
            let message = exception.message().unwrap_or_else(|| "unknown error".into());
            match exception.stack().filter(|stack| !stack.trim().is_empty()) {
                Some(stack) => format!("{message}\n{}", stack.trim_end()),
                None => message,
            }
        }
        Some(CaughtError::Value(value)) => {
            let json = ctx.json_stringify(value).ok().flatten().and_then(|json| json.to_string().ok());
            format!("The script threw {}", json.as_deref().unwrap_or("a value that is not an Error"))
        }
        Some(CaughtError::Error(Error::WouldBlock)) => {
            "The tool returned a Promise that never settles (nothing left to run could resolve it).".into()
        }
        Some(CaughtError::Error(error)) => error.to_string(),
        None => "unknown error".into(),
    }
}

/// 内置的小模块：`import path from "dshr:path"`。
fn builtin_module(name: &str) -> Option<&'static str> {
    match name {
        "dshr:path" => Some(include_str!("builtins/path.js")),
        "dshr:events" => Some(include_str!("builtins/events.js")),
        _ => None,
    }
}

/// `import` 认三种写法：插件文件夹里的相对路径；清单 `[imports]` 里登记过的裸模块名；内置模块（`dshr:*`）。
/// 模块名是相对插件文件夹、用 `/` 分隔的路径。
struct DirResolver {
    imports: std::collections::BTreeMap<String, String>,
}

impl Resolver for DirResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        let refuse = |reason: &str| Error::new_resolving_message(base, name, reason);
        if builtin_module(name).is_some() {
            return Ok(name.to_owned());
        }
        if let Some(target) = self.imports.get(name) {
            return Ok(target.clone());
        }
        if !(name.starts_with("./") || name.starts_with("../")) || name.contains(['\\', ':']) {
            return Err(refuse(
                "plugins can only import relative paths inside their own folder, built-in dshr:* modules, or names mapped in [imports] of plugin.toml",
            ));
        }
        let mut parts: Vec<&str> = base.split('/').collect();
        parts.pop();
        for part in name.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_none() {
                        return Err(refuse("the import leaves the plugin folder"));
                    }
                }
                part => parts.push(part),
            }
        }
        Ok(parts.join("/"))
    }
}

struct DirLoader {
    dir: PathBuf,
}

impl Loader for DirLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        if let Some(source) = builtin_module(name) {
            return Module::declare(ctx.clone(), name, source);
        }
        let source = std::fs::read_to_string(self.dir.join(name)).map_err(|error| Error::new_loading_message(name, error.to_string()))?;
        // JSON 文件当作只有一个默认导出的模块：`import data from "./data.json"`。
        if name.ends_with(".json") {
            let value: serde_json::Value =
                serde_json::from_str(&source).map_err(|error| Error::new_loading_message(name, format!("not valid JSON: {error}")))?;
            return Module::declare(ctx.clone(), name, format!("export default {value};"));
        }
        Module::declare(ctx.clone(), name, source)
    }
}
