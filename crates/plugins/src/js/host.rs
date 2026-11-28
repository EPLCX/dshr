//! 脚本能调到的宿主函数，以及它们共用的状态。
//!
//! 这些函数都跑在插件自己的线程上、在 QuickJS 的调用栈里，所以**不能 panic**：panic 穿过 C 的调用栈
//! 会让插件线程整个结束（插件随之停用）。出错一律返回 `Err`，变成脚本里的异常。
//!
//! 快的操作（文件、数据目录）同步完成；慢的（`fetch`、`exec`、`llm`）只做检查，然后交给 `ops.rs`
//! 在 IO 运行时上跑，返回一个操作编号，`prelude.js` 把它包成 Promise。

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use dshr_agent::{LlmAccess, SessionInfo};
use dshr_tools::{ShellCommand, ToolContext, WrapCommand};
use rquickjs::{Ctx, Exception, Function, Object};
use serde::Deserialize;
use serde_json::{Value, json};

use super::net::Net;
use super::ops::Ops;
use crate::manifest::Capabilities;

/// 一次能读、能写的工作区文件大小上限。
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// 插件数据目录里单个文件的上限（`readData` / `writeData` / `appendData`）。
pub(super) const MAX_DATA_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// 一次最多列出的目录项。
const MAX_DIR_ENTRIES: usize = 2000;
/// `glob` 最多返回的路径数、`grep` 最多返回的匹配行数。
const MAX_GLOB_RESULTS: usize = 5000;
const MAX_GREP_RESULTS: usize = 1000;
/// `exec` 的 stdout、stderr 各自最多留这么多字节。
const MAX_EXEC_OUTPUT: usize = 1024 * 1024;
/// 一次调用里最多发几次模型请求。
const MAX_LLM_REQUESTS: u32 = 8;
/// `host.session.events` 最多返回多少条、合计多大。
const MAX_SESSION_EVENTS: usize = 500;
const MAX_SESSION_BYTES: usize = 1024 * 1024;

/// 这次执行允许做的“有副作用”的事。插件的能力是上限，这里再按调用的场合收紧：
/// 工具调用按工具的声明，`prompt`、投影、画页面一律只读，用户在页面上点的动作按插件的能力。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Permits {
    /// 写工作区（插件有写能力、工具没有声明 `readOnly`）。
    pub write: bool,
    /// 联网。
    pub net: bool,
    /// 启动程序。
    pub exec: bool,
    /// 调用模型。
    pub llm: bool,
    /// 替用户做事：发消息、起子代理、改宿主数据、装插件（各自还要对应的能力）。
    pub act: bool,
}

/// 正在执行的那次调用。
pub(crate) struct Call {
    pub ctx: ToolContext,
    pub permits: Permits,
}

/// 每个插件留在内存里的日志行数。
const LOG_LINES: usize = 200;

/// 插件日志里的一行：`console.*`、`host.log`，以及宿主记下的工具出错。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// `info`、`warn`、`error`、`debug`。
    pub level: String,
    pub text: String,
}

/// 一个插件最近的日志。按 id 存，热重载换了运行时也接着往下记。
#[derive(Default)]
pub(crate) struct LogBuffer(Mutex<VecDeque<LogLine>>);

impl LogBuffer {
    /// 插件 `id` 的日志。
    pub fn of(id: &'static str) -> Arc<Self> {
        static BUFFERS: Mutex<Vec<(&'static str, Arc<LogBuffer>)>> = Mutex::new(Vec::new());
        let mut buffers = BUFFERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, buffer)) = buffers.iter().find(|(other, _)| *other == id) {
            return buffer.clone();
        }
        let buffer = Arc::new(Self::default());
        buffers.push((id, buffer.clone()));
        buffer
    }

    pub fn push(&self, level: &str, text: &str) {
        let mut lines = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if lines.len() >= LOG_LINES {
            lines.pop_front();
        }
        // 一行最多留 2000 个字符：日志是给人看的，不是存输出的地方。
        let text: String = text.chars().take(2000).collect();
        lines.push_back(LogLine { level: level.to_owned(), text });
    }

    pub fn lines(&self) -> Vec<LogLine> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().cloned().collect()
    }
}

/// 插件线程上的状态：宿主函数和中断检查都从这里取。
pub(crate) struct State {
    pub plugin: &'static str,
    /// 这个插件的日志（[`LogBuffer::of`]）。
    pub logs: Arc<LogBuffer>,
    pub capabilities: Capabilities,
    /// 清单里声明的密钥名。
    secrets: Vec<String>,
    /// 这次运行里交给过脚本的密钥值：日志里出现时换成 `***`。
    revealed: RefCell<Vec<String>>,
    /// 插件自己的数据目录（`<数据目录>/plugin-data/<id>/`）：`host.readData` / `writeData` 读写这里，不需要能力。
    pub(super) data_dir: PathBuf,
    /// 打开着的 SQLite 数据库（`host.db`），按文件名。
    pub(super) databases: RefCell<std::collections::HashMap<String, rusqlite::Connection>>,
    call: RefCell<Option<Call>>,
    /// 当前这段执行的最后期限；到了就打断脚本。
    deadline: Cell<Option<Instant>>,
    limit: Cell<Duration>,
    expired: Cell<bool>,
    /// 调用方放弃了这次调用（用户中断）时变成 `true`。
    cancelled: RefCell<Option<Arc<AtomicBool>>>,
    /// HTTP 客户端，第一次 `fetch` 时才建；建不出来时记下原因。
    net: OnceCell<Result<Net, String>>,
    /// 交给 IO 运行时的慢操作。
    pub ops: Ops,
    /// 这次调用里发过几次模型请求。
    llm_requests: Cell<u32>,
    /// 每一页被要求重画（`host.ui.refresh`）的次数：界面那边读。
    pub refresh: Arc<Mutex<std::collections::HashMap<String, u64>>>,
    /// 这次调用里起过几个子代理。
    agent_runs: Cell<u32>,
    /// 上次替用户发消息、发通知的时间（限流）。
    last_send: Cell<Option<Instant>>,
    last_notify: Cell<Option<Instant>>,
    /// 这次执行是用户亲手触发的（页面动作、命令、斜杠命令）：可以写输入框。
    pub user_initiated: Cell<bool>,
    /// 还没取走的流式响应、WebSocket（操作编号 → 连上后填进来的槽）。
    pub(super) pending_streams: RefCell<Vec<(u64, PendingSlot<super::BodyReceiver>)>>,
    pub(super) pending_sockets: RefCell<Vec<(u64, PendingSlot<Arc<super::stream::Socket>>)>>,
    /// `host.tools.set` 设的动态工具（运行时那边共享的一份）。
    pub(super) dynamic: RefCell<Option<Arc<super::runtime::DynamicTools>>>,
    /// `host.ui.hide` / `show` 改的入口显隐（运行时那边共享的一份）。
    pub(super) visibility: RefCell<Option<Arc<super::runtime::EntryVisibility>>>,
}

pub(super) type PendingSlot<T> = Arc<Mutex<Option<T>>>;

/// 一次调用里最多起几个子代理。
const MAX_AGENT_RUNS: u32 = 4;
/// 替用户发消息、发通知的最短间隔。
const SEND_INTERVAL: Duration = Duration::from_secs(5);
const NOTIFY_INTERVAL: Duration = Duration::from_secs(10);
/// 工具交回、随消息发出的图片：最多几张，支持哪些类型。
const MAX_IMAGES: usize = 4;
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// 一张 base64 图片的类型和大小（解码后不超过 [`MAX_FILE_BYTES`]）。
fn check_image(media_type: &str, data: &str) -> Result<(), String> {
    if !IMAGE_TYPES.contains(&media_type) {
        return Err(format!("unsupported image type {media_type} (png, jpeg, gif or webp)"));
    }
    if data.is_empty() {
        return Err("an image has no data".into());
    }
    if (data.len() / 4 * 3) as u64 > MAX_FILE_BYTES {
        return Err(format!("an image is larger than {} MB", MAX_FILE_BYTES / 1024 / 1024));
    }
    Ok(())
}
/// 装插件时下载的压缩包上限。
const MAX_INSTALL_DOWNLOAD: usize = 50 * 1024 * 1024;

impl State {
    pub fn new(
        plugin: &'static str,
        capabilities: Capabilities,
        secrets: Vec<String>,
        data_dir: PathBuf,
        refresh: Arc<Mutex<std::collections::HashMap<String, u64>>>,
    ) -> Self {
        Self {
            plugin,
            data_dir,
            databases: RefCell::new(std::collections::HashMap::new()),
            logs: LogBuffer::of(plugin),
            capabilities,
            secrets,
            revealed: RefCell::new(Vec::new()),
            net: OnceCell::new(),
            call: RefCell::new(None),
            deadline: Cell::new(None),
            limit: Cell::new(Duration::ZERO),
            expired: Cell::new(false),
            cancelled: RefCell::new(None),
            ops: Ops::default(),
            llm_requests: Cell::new(0),
            refresh,
            agent_runs: Cell::new(0),
            last_send: Cell::new(None),
            last_notify: Cell::new(None),
            user_initiated: Cell::new(false),
            pending_streams: RefCell::new(Vec::new()),
            pending_sockets: RefCell::new(Vec::new()),
            dynamic: RefCell::new(None),
            visibility: RefCell::new(None),
        }
    }

    /// 数据目录（`~/.dshr`）：插件数据目录是 `<数据目录>/plugin-data/<id>/`。
    fn home(&self) -> PathBuf {
        self.data_dir.parent().and_then(Path::parent).map(Path::to_owned).unwrap_or_else(|| self.data_dir.clone())
    }

    fn plugins_dir(&self) -> PathBuf {
        self.home().join("plugins")
    }

    /// HTTP 客户端（第一次用时建）。
    pub(super) fn net(&self) -> Result<&Net, String> {
        self.net.get_or_init(|| Net::new(&self.capabilities)).as_ref().map_err(Clone::clone)
    }

    /// 这次调用能不能联网。
    pub(super) fn require_net(&self) -> Result<(), String> {
        if self.capabilities.net.is_empty() {
            return Err("this plugin does not declare any `net:<host>` capability in plugin.toml".into());
        }
        self.with_call(|call| {
            if call.permits.net {
                Ok(())
            } else {
                Err("network access is not available here (tools that are not readOnly, async hooks, actions, schedules and services can connect)".into())
            }
        })
    }

    /// 这次调用的许可满足 `check` 才行。
    pub(super) fn with_permits(&self, check: impl FnOnce(&Permits) -> bool, refused: &str) -> Result<(), String> {
        self.with_call(|call| if check(&call.permits) { Ok(()) } else { Err(refused.to_owned()) })
    }

    /// 往这次调用的实时输出写字的函数（可以带到别的线程上）。
    pub(super) fn output_sink(&self) -> impl Fn(&str) + Send + 'static {
        let ctx = self.call.borrow().as_ref().map(|call| call.ctx.clone());
        move |text: &str| {
            if let Some(ctx) = &ctx {
                ctx.emit_output(text);
            }
        }
    }

    fn may_act(&self) -> bool {
        self.call.borrow().as_ref().is_some_and(|call| call.permits.act)
    }

    // ---- 替用户做事：会话控制、子代理 ----

    /// `{ text, session: "current" | "new" | <id>, workspace?, images? }`，返回会话 id。
    fn session_send(&self, request: &str) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Image {
            data: String,
            #[serde(alias = "mediaType", alias = "media_type")]
            media_type: String,
            #[serde(default)]
            name: Option<String>,
        }
        #[derive(Deserialize)]
        struct Request {
            #[serde(default)]
            text: String,
            #[serde(default)]
            session: Option<String>,
            #[serde(default)]
            workspace: Option<String>,
            #[serde(default)]
            images: Vec<Image>,
        }
        if !self.capabilities.session_control {
            return Err("this plugin does not declare the `session:control` capability in plugin.toml".into());
        }
        if !self.may_act() {
            return Err(
                "sending messages is only available to tools that are not declared readOnly, page actions, async hooks and services".into(),
            );
        }
        let request: Request = serde_json::from_str(request).map_err(|error| format!("invalid session.send request: {error}"))?;
        if request.text.trim().is_empty() && request.images.is_empty() {
            return Err("the message is empty".into());
        }
        if request.images.len() > MAX_IMAGES {
            return Err(format!("a message can carry at most {MAX_IMAGES} images"));
        }
        let mut images = Vec::with_capacity(request.images.len());
        for (index, image) in request.images.into_iter().enumerate() {
            check_image(&image.media_type, &image.data)?;
            let path = image.name.filter(|name| !name.trim().is_empty()).unwrap_or_else(|| format!("plugin-image-{}", index + 1));
            images.push(dshr_agent::UserAttachment::Image { path, media_type: image.media_type, data: image.data });
        }
        if self.last_send.get().is_some_and(|last| last.elapsed() < SEND_INTERVAL) {
            return Err(format!("a plugin can send at most one message every {} seconds", SEND_INTERVAL.as_secs()));
        }
        let control = crate::app::app_services().session.ok_or("this app does not let plugins send messages")?;
        let target = match request.session.as_deref() {
            None | Some("current") => crate::app::SessionTarget::Current,
            Some("new") => crate::app::SessionTarget::New,
            Some(id) => crate::app::SessionTarget::Id(id.to_owned()),
        };
        let id = control.send(crate::app::SendRequest {
            plugin: self.plugin.to_owned(),
            text: request.text,
            target,
            workspace: request.workspace.map(PathBuf::from),
            images,
        })?;
        self.last_send.set(Some(Instant::now()));
        tracing::info!(plugin = self.plugin, session = id, "插件替用户发了一条消息");
        Ok(id)
    }

    fn session_stop(&self, session: Option<String>) -> Result<(), String> {
        if !self.capabilities.session_control {
            return Err("this plugin does not declare the `session:control` capability in plugin.toml".into());
        }
        if !self.may_act() {
            return Err("stopping a session is only available where the plugin may act for the user".into());
        }
        crate::app::app_services().session.ok_or("this app does not let plugins control sessions")?.stop(session)
    }

    /// `{ session?, model?, effort?, preset? }`：改会话的模型、思考等级或预设（`session:control`）。
    /// `effort` 为 `null` 时改回模型的默认值，不写时不改。
    fn session_set(&self, request: &str) -> Result<(), String> {
        if !self.capabilities.session_control {
            return Err("this plugin does not declare the `session:control` capability in plugin.toml".into());
        }
        if !self.may_act() {
            return Err("changing the session's model or preset is only available where the plugin may act for the user".into());
        }
        let request: Value = serde_json::from_str(request).map_err(|error| format!("invalid session options: {error}"))?;
        let text = |key: &str| -> Result<Option<String>, String> {
            match request.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.trim().to_owned())),
                Some(_) => Err(format!("`{key}` must be a non-empty string")),
            }
        };
        let session = text("session")?.filter(|session| session != "current");
        let change = if let Some(preset) = text("preset")? {
            crate::app::OptionChange::Preset(preset)
        } else {
            let effort = match request.get("effort") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::String(effort)) if ["off", "low", "high", "max"].contains(&effort.as_str()) => Some(Some(effort.clone())),
                Some(other) => return Err(format!("unknown effort {other}: use off, low, high, max or null")),
            };
            let model = text("model")?;
            if model.is_none() && effort.is_none() {
                return Err("nothing to change: give a model, an effort or a preset".into());
            }
            crate::app::OptionChange::Model { model, effort }
        };
        let control = crate::app::app_services().session.ok_or("this app does not let plugins control sessions")?;
        control.set_options(session, change)?;
        tracing::info!(plugin = self.plugin, "插件改了会话的选项");
        Ok(())
    }

    /// 可以选的模型（`session:read` 或 `session:control`）。
    fn session_models(&self) -> Result<String, String> {
        if !(self.capabilities.session_control || self.capabilities.session_read) {
            return Err("this plugin needs `session:read` or `session:control` to list models".into());
        }
        let models = crate::app::app_services().session.map(|control| control.models()).unwrap_or_default();
        serde_json::to_string(&models).map_err(|error| error.to_string())
    }

    fn session_list(&self) -> Result<String, String> {
        if !(self.capabilities.session_control || self.capabilities.session_read) {
            return Err("this plugin needs `session:read` or `session:control` to list sessions".into());
        }
        let sessions = crate::app::app_services().session.map(|control| control.list()).unwrap_or_default();
        serde_json::to_string(&sessions).map_err(|error| error.to_string())
    }

    /// 起一个子代理：`{ task, preset? }`，结果是子代理的报告。
    fn agent_run(&self, request: &str) -> Result<u64, String> {
        #[derive(Deserialize)]
        struct Request {
            task: String,
            #[serde(default)]
            preset: Option<String>,
        }
        if !self.capabilities.agent_run {
            return Err("this plugin does not declare the `agent:run` capability in plugin.toml".into());
        }
        let request: Request = serde_json::from_str(request).map_err(|error| format!("invalid agent.run request: {error}"))?;
        let preset = match request.preset.as_deref() {
            None => None,
            Some("standard") => Some(dshr_agent::Preset::Standard),
            Some("plan") => Some(dshr_agent::Preset::Plan),
            Some("review") => Some(dshr_agent::Preset::Review),
            Some(other) => return Err(format!("unknown preset {other} (standard, plan, review)")),
        };
        let (env, runner, ctx) = self.with_call(|call| {
            if !call.permits.act {
                return Err("sub-agents are only available to tools that are not declared readOnly and async hooks".into());
            }
            let env =
                call.ctx.options.get::<dshr_agent::CallEnv>().cloned().ok_or("sub-agents are only available inside the agent loop")?;
            let runner = call.ctx.plugins.service::<dshr_agent::SubagentRunner>().ok_or("sub-agents are not available here")?;
            Ok((env, runner, call.ctx.clone()))
        })?;
        let runs = self.agent_runs.get() + 1;
        if runs > MAX_AGENT_RUNS {
            return Err(format!("at most {MAX_AGENT_RUNS} sub-agents per call"));
        }
        self.agent_runs.set(runs);
        let log_name = format!("{}-{}-{runs}", env.call_id, self.plugin);
        let workspace = ctx.workspace.clone();
        let work = async move {
            let progress = ctx.clone();
            runner
                .run(&env, workspace, &log_name, &request.task, preset, move |line| progress.emit_output(line))
                .await
                .map(|report| json!({ "report": report }).to_string())
        };
        self.ops.start(work, self.remaining())
    }

    // ---- 通知、输入框 ----

    fn notify(&self, request: &str) -> Result<bool, String> {
        #[derive(Deserialize)]
        struct Request {
            title: String,
            #[serde(default)]
            body: String,
            #[serde(default)]
            open: Option<String>,
        }
        if !self.capabilities.notify {
            return Err("this plugin does not declare the `notify` capability in plugin.toml".into());
        }
        let request: Request = serde_json::from_str(request).map_err(|error| format!("invalid notification: {error}"))?;
        if self.last_notify.get().is_some_and(|last| last.elapsed() < NOTIFY_INTERVAL) {
            self.log("warn", &format!("notification dropped (at most one every {} s): {}", NOTIFY_INTERVAL.as_secs(), request.title));
            return Ok(false);
        }
        self.last_notify.set(Some(Instant::now()));
        let notification = crate::app::Notification {
            plugin: self.plugin.to_owned(),
            title: request.title.chars().take(120).collect(),
            body: request.body.chars().take(500).collect(),
            page: request.open,
        };
        match crate::app::app_services().notifier {
            Some(notify) => notify(&notification),
            None => self.log("info", &format!("[notification] {}: {}", notification.title, notification.body)),
        }
        Ok(true)
    }

    fn composer_set(&self, text: &str) -> Result<(), String> {
        if !self.user_initiated.get() {
            return Err("the input box can only be changed from page actions, commands and slash commands".into());
        }
        crate::app::app_services().composer.ok_or("this app has no input box")?.set_text(text)
    }

    fn composer_get(&self) -> Result<Option<String>, String> {
        if !self.user_initiated.get() {
            return Err("the input box can only be read from page actions, commands and slash commands".into());
        }
        Ok(crate::app::app_services().composer.and_then(|composer| composer.text()))
    }

    fn dshr(&self, method: &str, args: &str) -> Result<String, String> {
        let home = self.home();
        super::dshr::Dshr { capabilities: &self.capabilities, home: &home, may_act: self.may_act(), plugin: self.plugin }.call(method, args)
    }

    // ---- 插件管理 ----

    fn plugins_call(&self, method: &str, args: &str) -> Result<String, String> {
        if !self.capabilities.plugins_install {
            return Err("this plugin does not declare the `plugins:install` capability in plugin.toml".into());
        }
        let args: Vec<Value> = serde_json::from_str(args).map_err(|error| format!("invalid arguments: {error}"))?;
        let dir = self.plugins_dir();
        match method {
            "list" => {
                let grants = crate::grants::load(&crate::grants::grants_path(&dir));
                let config = dshr_config::Config::load(&dshr_config::Paths { home: self.home() }).unwrap_or_default();
                let list: Vec<Value> = crate::manifest::discover(&dir)
                    .into_iter()
                    .map(|found| {
                        let manifest = found.manifest.as_ref().ok();
                        let declared = manifest.map(|manifest| manifest.capabilities.names()).unwrap_or_default();
                        let missing = crate::grants::missing(&grants, &found.id, &declared);
                        json!({
                            "id": found.id,
                            "name": manifest.and_then(|manifest| manifest.name.clone()),
                            "version": manifest.and_then(|manifest| manifest.version.clone()),
                            "description": manifest.and_then(|manifest| manifest.description.clone()),
                            "capabilities": declared,
                            "missing": missing,
                            "enabled": config.plugin_enabled(&found.id),
                            "error": found.manifest.as_ref().err(),
                        })
                    })
                    .collect();
                Ok(Value::Array(list).to_string())
            }
            "uninstall" | "setEnabled" => {
                if !self.may_act() {
                    return Err("managing plugins is only available where the plugin may act for the user".into());
                }
                let id = args.first().and_then(Value::as_str).ok_or("the plugin id is required")?;
                if id == self.plugin {
                    return Err("a plugin cannot uninstall or switch off itself".into());
                }
                if method == "uninstall" {
                    crate::install::uninstall(&dir, id)?;
                } else {
                    let paths = dshr_config::Paths { home: self.home() };
                    let mut config = dshr_config::Config::load(&paths).map_err(|error| error.to_string())?;
                    config.set_plugin_enabled(id, args.get(1).and_then(Value::as_bool).unwrap_or(true));
                    config.save(&paths).map_err(|error| error.to_string())?;
                }
                tracing::info!(plugin = self.plugin, method, target = id, "插件管理了另一个插件");
                Ok("null".into())
            }
            other => Err(format!("unknown host.plugins method {other}")),
        }
    }

    /// 下载并安装一个插件：`{ url, sha256?, id? }`。下载、解压、问用户都在 IO 运行时上，结果是装好的 id。
    fn plugins_install(&self, request: &str) -> Result<u64, String> {
        #[derive(Deserialize)]
        struct Request {
            url: String,
            #[serde(default)]
            sha256: Option<String>,
            #[serde(default)]
            id: Option<String>,
        }
        if !self.capabilities.plugins_install {
            return Err("this plugin does not declare the `plugins:install` capability in plugin.toml".into());
        }
        if !self.may_act() {
            return Err("installing plugins is only available where the plugin may act for the user".into());
        }
        let request: Request = serde_json::from_str(request).map_err(|error| format!("invalid install request: {error}"))?;
        let prompt =
            crate::app::app_services().install_prompt.ok_or("this app cannot ask you to confirm an install; use `dshr plugin install`")?;
        let url = crate::install::download_url(&request.url);
        let net = self.net.get_or_init(|| Net::new(&self.capabilities)).as_ref().map_err(Clone::clone)?;
        let download = net.download(&self.capabilities, &url, MAX_INSTALL_DOWNLOAD)?;
        let (dir, plugin) = (self.plugins_dir(), self.plugin);
        let work = async move {
            let bytes = download.await?;
            tokio::task::spawn_blocking(move || {
                let staged = crate::install::stage_bytes(&bytes, &url, &request.url, request.id.as_deref(), request.sha256.as_deref())?;
                let ask = staged.request(&dir, Some(plugin));
                // 插件代装时不能放过发布者的变化：只有明确的“安装”才算同意。
                if prompt(&ask) != crate::app::InstallAnswer::Allow {
                    return Err("you declined the install".to_owned());
                }
                let id = staged.manifest.id.clone();
                staged.commit(&dir)?;
                tracing::info!(plugin, installed = id, "插件装了另一个插件");
                Ok(json!({ "id": id }).to_string())
            })
            .await
            .map_err(|error| error.to_string())?
        };
        // 用户确认要时间：这件事不受调用剩下的时限约束，最多等 10 分钟。
        self.ops.start(work, Duration::from_secs(600))
    }

    /// 开始计时：`limit` 之后、或 `cancelled` 变成 `true` 之后，脚本会被打断。
    pub fn arm(&self, limit: Duration, cancelled: Option<Arc<AtomicBool>>) {
        self.deadline.set(Some(Instant::now() + limit));
        self.limit.set(limit);
        self.expired.set(false);
        *self.cancelled.borrow_mut() = cancelled;
    }

    pub fn disarm(&self) {
        self.deadline.set(None);
        *self.cancelled.borrow_mut() = None;
    }

    /// 这段执行的最后期限和取消标记：SQLite 这类在宿主里跑的长操作据此自己停下。
    pub(super) fn interrupt_handle(&self) -> (Option<Instant>, Option<Arc<AtomicBool>>) {
        (self.deadline.get(), self.cancelled.borrow().clone())
    }

    /// 这段执行还剩多久。
    pub fn remaining(&self) -> Duration {
        self.deadline.get().map_or(Duration::ZERO, |deadline| deadline.saturating_duration_since(Instant::now()))
    }

    pub fn begin_call(&self, call: Option<Call>) {
        *self.call.borrow_mut() = call;
        self.llm_requests.set(0);
        self.agent_runs.set(0);
        self.ops.begin();
    }

    pub fn end_call(&self) {
        // 这次调用开了事务却没提交（忘了、抛异常、超时）：回滚，不留给下一次调用。
        self.rollback_open_transactions();
        *self.call.borrow_mut() = None;
        self.pending_streams.borrow_mut().clear();
        self.pending_sockets.borrow_mut().clear();
        self.user_initiated.set(false);
        self.ops.end();
    }

    /// QuickJS 执行脚本时隔一阵问一次：要不要打断。
    pub fn should_interrupt(&self) -> bool {
        if self.is_cancelled() {
            return true;
        }
        if self.deadline.get().is_some_and(|deadline| Instant::now() >= deadline) {
            self.expired.set(true);
            return true;
        }
        false
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.borrow().as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// 这段执行是不是因为超时被打断的；是的话返回当时的时限。
    pub fn expired(&self) -> Option<Duration> {
        self.expired.get().then(|| self.limit.get())
    }

    pub(super) fn with_call_pub<T>(&self, run: impl FnOnce(&Call) -> Result<T, String>) -> Result<T, String> {
        self.with_call(run)
    }

    fn with_call<T>(&self, run: impl FnOnce(&Call) -> Result<T, String>) -> Result<T, String> {
        match &*self.call.borrow() {
            Some(call) => run(call),
            None => Err("host functions can only be used while a tool is executing".into()),
        }
    }

    fn require_read(&self) -> Result<(), String> {
        if self.capabilities.fs_read {
            Ok(())
        } else {
            Err("this plugin does not declare the `fs:workspace:read` capability in plugin.toml".into())
        }
    }

    /// 写工作区的前提：有写能力，而且这次调用允许写。
    fn with_write<T>(&self, run: impl FnOnce(&Call) -> Result<T, String>) -> Result<T, String> {
        if !self.capabilities.fs_write {
            return Err("this plugin does not declare the `fs:workspace:write` capability in plugin.toml".into());
        }
        self.with_call(|call| {
            if !call.permits.write {
                return Err(
                    "writing files is not allowed here (the tool is declared readOnly, or this is a prompt, render, hook or projection)"
                        .into(),
                );
            }
            run(call)
        })
    }

    // ---- 工作区里的文件 ----

    fn read_bytes(&self, path: &str) -> Result<Vec<u8>, String> {
        self.require_read()?;
        self.with_call(|call| {
            let resolved = confine(&call.ctx, path)?;
            let size = std::fs::metadata(&resolved).map_err(|error| format!("cannot read {path}: {error}"))?.len();
            if size > MAX_FILE_BYTES {
                return Err(format!("{path} is too large to read ({size} bytes, the limit is {MAX_FILE_BYTES})"));
            }
            std::fs::read(&resolved).map_err(|error| format!("cannot read {path}: {error}"))
        })
    }

    fn read_file(&self, path: &str) -> Result<String, String> {
        String::from_utf8(self.read_bytes(path)?).map_err(|_| format!("{path} is not UTF-8 text; use host.readFileBytes for binary files"))
    }

    fn read_file_bytes(&self, path: &str) -> Result<String, String> {
        Ok(base64::engine::general_purpose::STANDARD.encode(self.read_bytes(path)?))
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        self.require_read()?;
        self.with_call(|call| {
            let resolved = confine(&call.ctx, path)?;
            let entries = std::fs::read_dir(&resolved).map_err(|error| format!("cannot list {path}: {error}"))?;
            let mut names: Vec<String> = entries
                .flatten()
                .take(MAX_DIR_ENTRIES)
                .map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // 文件夹名后面带 `/`。
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) { format!("{name}/") } else { name }
                })
                .collect();
            names.sort();
            Ok(names)
        })
    }

    /// `{ type: "file" | "dir" | "other", size, modified }`；不存在时是 `null`。
    fn stat(&self, path: &str) -> Result<String, String> {
        self.require_read()?;
        self.with_call(|call| {
            let resolved = confine(&call.ctx, path)?;
            let metadata = match std::fs::metadata(&resolved) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("null".into()),
                Err(error) => return Err(format!("cannot stat {path}: {error}")),
                Ok(metadata) => metadata,
            };
            let kind = if metadata.is_file() {
                "file"
            } else if metadata.is_dir() {
                "dir"
            } else {
                "other"
            };
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_millis() as u64);
            Ok(json!({ "type": kind, "size": metadata.len(), "modified": modified }).to_string())
        })
    }

    /// 工作区里匹配 glob 的文件（相对路径，`/` 分隔）。遵守 `.gitignore`，不看隐藏文件。
    /// 走遍整个工作区，交给 IO 运行时的阻塞线程池，不占插件线程（结果经 `settle` 回来）。
    fn glob(&self, pattern: &str) -> Result<u64, String> {
        self.require_read()?;
        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| format!("invalid glob {pattern}: {error}"))?
            .compile_matcher();
        let root = self.with_call(|call| Ok(call.ctx.workspace.clone()))?;
        let work = async move {
            tokio::task::spawn_blocking(move || {
                let mut found = Vec::new();
                for entry in ignore::WalkBuilder::new(&root).build().flatten() {
                    if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                        continue;
                    }
                    let Ok(relative) = entry.path().strip_prefix(&root) else { continue };
                    let relative = relative.to_string_lossy().replace('\\', "/");
                    if matcher.is_match(&relative) {
                        found.push(relative);
                        if found.len() >= MAX_GLOB_RESULTS {
                            break;
                        }
                    }
                }
                found.sort();
                serde_json::to_string(&found).map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())?
        };
        self.ops.start(work, self.remaining())
    }

    /// 在工作区里按正则搜索：`[{ path, line, text }]`。和 `glob` 一样在 IO 运行时上跑。
    fn grep(&self, pattern: &str, options: &str) -> Result<u64, String> {
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Options {
            glob: Option<String>,
            ignore_case: bool,
            max_results: Option<usize>,
        }
        self.require_read()?;
        let options: Options = serde_json::from_str(options).map_err(|error| format!("invalid grep options: {error}"))?;
        let matcher = dshr_tools::LineSearch::matcher(pattern, options.ignore_case)
            .map_err(|error| format!("invalid regular expression: {error}"))?;
        let filter = match &options.glob {
            Some(glob) => Some(
                globset::GlobBuilder::new(glob)
                    .literal_separator(true)
                    .build()
                    .map_err(|error| format!("invalid glob {glob}: {error}"))?
                    .compile_matcher(),
            ),
            None => None,
        };
        let limit = options.max_results.unwrap_or(200).min(MAX_GREP_RESULTS);
        let root = self.with_call(|call| Ok(call.ctx.workspace.clone()))?;
        let work = async move {
            tokio::task::spawn_blocking(move || {
                // 和 tools 里的 grep 同一套：并行遍历、ripgrep 内核整块搜索、跳过二进制和超过 2 MB 的文件。
                // 不看隐藏文件（和以前一样）。找完按路径（逐级目录）和行号排序，结果顺序与遍历先后无关。
                let search = dshr_tools::LineSearch { matcher: &matcher, hidden: false, per_file: limit, stop: None };
                let keep = |relative: &str, _: &Path| filter.as_ref().is_none_or(|filter| filter.is_match(relative));
                let clip = |line: &str| line.trim_end_matches(['\n', '\r']).chars().take(500).collect();
                let mut found = search.run(&root, &root, &keep, clip);
                found.sort_by(|a, b| Path::new(&a.0).cmp(Path::new(&b.0)).then(a.1.cmp(&b.1)));
                found.truncate(limit);
                let matches: Vec<Value> =
                    found.into_iter().map(|(path, line, text)| json!({ "path": path, "line": line, "text": text })).collect();
                Ok(Value::Array(matches).to_string())
            })
            .await
            .map_err(|error| error.to_string())?
        };
        self.ops.start(work, self.remaining())
    }

    fn write_bytes(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        self.with_write(|call| {
            if bytes.len() as u64 > MAX_FILE_BYTES {
                return Err(format!("the content is too large to write ({} bytes, the limit is {MAX_FILE_BYTES})", bytes.len()));
            }
            let resolved = confine(&call.ctx, path)?;
            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent).map_err(|error| format!("cannot write {path}: {error}"))?;
            }
            std::fs::write(&resolved, bytes).map_err(|error| format!("cannot write {path}: {error}"))
        })
    }

    /// 接在工作区文件末尾（没有时新建）。
    fn append_file(&self, path: &str, text: &str) -> Result<(), String> {
        self.with_write(|call| {
            if text.len() as u64 > MAX_FILE_BYTES {
                return Err(format!("the content is too large to append ({} bytes, the limit is {MAX_FILE_BYTES})", text.len()));
            }
            let resolved = confine(&call.ctx, path)?;
            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent).map_err(|error| format!("cannot write {path}: {error}"))?;
            }
            append_to(&resolved, text.as_bytes()).map_err(|error| format!("cannot write {path}: {error}"))
        })
    }

    fn write_file_bytes(&self, path: &str, encoded: &str) -> Result<(), String> {
        let bytes = decode_base64(encoded)?;
        self.write_bytes(path, &bytes)
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.with_write(|call| {
            let resolved = confine(&call.ctx, path)?;
            std::fs::create_dir_all(&resolved).map_err(|error| format!("cannot create {path}: {error}"))
        })
    }

    /// 删除文件；`recursive` 时也可以删文件夹。工作区本身不能删。
    fn remove(&self, path: &str, recursive: bool) -> Result<(), String> {
        self.with_write(|call| {
            let resolved = confine(&call.ctx, path)?;
            if same_path(&resolved, &call.ctx.workspace) {
                return Err("the workspace itself cannot be removed".into());
            }
            let metadata = std::fs::symlink_metadata(&resolved).map_err(|error| format!("cannot remove {path}: {error}"))?;
            let result = if metadata.is_dir() {
                if recursive { std::fs::remove_dir_all(&resolved) } else { std::fs::remove_dir(&resolved) }
            } else {
                std::fs::remove_file(&resolved)
            };
            result.map_err(|error| format!("cannot remove {path}: {error}"))
        })
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.with_write(|call| {
            let (source, target) = (confine(&call.ctx, from)?, confine(&call.ctx, to)?);
            if same_path(&source, &call.ctx.workspace) {
                return Err("the workspace itself cannot be renamed".into());
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|error| format!("cannot rename to {to}: {error}"))?;
            }
            std::fs::rename(&source, &target).map_err(|error| format!("cannot rename {from} to {to}: {error}"))
        })
    }

    // ---- 插件自己的数据目录 ----

    /// 插件自己的数据：没有这个文件时是 `None`。
    fn read_data(&self, path: &str) -> Result<Option<String>, String> {
        let resolved = confine_data(&self.data_dir, path)?;
        match std::fs::metadata(&resolved) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot read data {path}: {error}")),
            Ok(metadata) if metadata.len() > MAX_DATA_FILE_BYTES => {
                return Err(format!("data {path} is too large to read ({} bytes, the limit is {MAX_DATA_FILE_BYTES})", metadata.len()));
            }
            Ok(_) => {}
        }
        std::fs::read_to_string(&resolved).map(Some).map_err(|error| format!("cannot read data {path}: {error}"))
    }

    fn write_data(&self, path: &str, text: &str) -> Result<(), String> {
        if text.len() as u64 > MAX_DATA_FILE_BYTES {
            return Err(format!("the data is too large to write ({} bytes, the limit is {MAX_DATA_FILE_BYTES})", text.len()));
        }
        let resolved = confine_data(&self.data_dir, path)?;
        let existing = std::fs::metadata(&resolved).map(|metadata| metadata.len()).unwrap_or(0);
        let total = data_usage(&self.data_dir);
        if total.saturating_sub(existing) + text.len() as u64 > MAX_DATA_BYTES {
            return Err(format!("the plugin's data folder would exceed {} MB", MAX_DATA_BYTES / 1024 / 1024));
        }
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("cannot write data {path}: {error}"))?;
        }
        std::fs::write(&resolved, text).map_err(|error| format!("cannot write data {path}: {error}"))?;
        adjust_data_usage(&self.data_dir, existing, text.len() as u64);
        Ok(())
    }

    /// 接在数据文件末尾（没有时新建）：日志、JSONL 记录不用先读出整个文件。
    fn append_data(&self, path: &str, text: &str) -> Result<(), String> {
        let resolved = confine_data(&self.data_dir, path)?;
        let existing = std::fs::metadata(&resolved).map(|metadata| metadata.len()).unwrap_or(0);
        if existing + text.len() as u64 > MAX_DATA_FILE_BYTES {
            return Err(format!(
                "data {path} would grow past {MAX_DATA_FILE_BYTES} bytes; start a new file, trim it with writeData, or use host.db"
            ));
        }
        if data_usage(&self.data_dir) + text.len() as u64 > MAX_DATA_BYTES {
            return Err(format!("the plugin's data folder would exceed {} MB", MAX_DATA_BYTES / 1024 / 1024));
        }
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("cannot write data {path}: {error}"))?;
        }
        append_to(&resolved, text.as_bytes()).map_err(|error| format!("cannot write data {path}: {error}"))?;
        adjust_data_usage(&self.data_dir, 0, text.len() as u64);
        Ok(())
    }

    /// 删除数据目录里的一个文件；不存在时什么也不做。
    fn remove_data(&self, path: &str) -> Result<(), String> {
        let resolved = confine_data(&self.data_dir, path)?;
        let size = std::fs::metadata(&resolved).map(|metadata| metadata.len()).unwrap_or(0);
        match std::fs::remove_file(&resolved) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(format!("cannot remove data {path}: {error}")),
            Err(_) => Ok(()),
            Ok(()) => {
                adjust_data_usage(&self.data_dir, size, 0);
                Ok(())
            }
        }
    }

    /// 数据目录里的文件（相对路径，`/` 分隔）。
    fn list_data(&self) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, found: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, found);
                } else if let Ok(relative) = path.strip_prefix(root) {
                    found.push(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        let mut found = Vec::new();
        walk(&self.data_dir, &self.data_dir, &mut found);
        found.sort();
        found
    }

    // ---- 慢操作：交给 IO 运行时 ----

    fn fetch(&self, url: &str, options: &str) -> Result<u64, String> {
        if self.capabilities.net.is_empty() {
            return Err("this plugin does not declare any `net:<host>` capability in plugin.toml".into());
        }
        self.with_call(|call| {
            if call.permits.net {
                Ok(())
            } else {
                Err("network access is only available to tools that are not declared readOnly, async hooks and page actions".into())
            }
        })?;
        let net = self.net.get_or_init(|| Net::new(&self.capabilities)).as_ref().map_err(Clone::clone)?;
        let request = net.prepare(&self.capabilities, url, options, |name| {
            self.secret(name)?.ok_or_else(|| format!("the secret {name} has not been set"))
        })?;
        self.ops.start(request, self.remaining())
    }

    /// 启动一个程序（不经 shell），等它结束：`{ code, stdout, stderr }`。
    fn exec(&self, program: &str, args: &str, options: &str) -> Result<u64, String> {
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Options {
            cwd: Option<String>,
            stdin: Option<String>,
            env: std::collections::BTreeMap<String, String>,
            timeout_ms: Option<u64>,
        }
        if self.capabilities.exec.is_empty() {
            return Err("this plugin does not declare any `exec:<program>` capability in plugin.toml".into());
        }
        if !self.capabilities.allows_program(program) {
            return Err(format!("this plugin may not run {program}: add `exec:{program}` to capabilities in plugin.toml"));
        }
        let args: Vec<String> = serde_json::from_str(args).map_err(|_| "exec arguments must be an array of strings".to_owned())?;
        let options: Options = serde_json::from_str(options).map_err(|error| format!("invalid exec options: {error}"))?;
        let (command, workspace) = self.with_call(|call| {
            if !call.permits.exec {
                return Err("running programs is only available to tools that are not declared readOnly and to page actions".into());
            }
            let cwd = confine(&call.ctx, options.cwd.as_deref().unwrap_or("."))?;
            let mut shell = ShellCommand {
                script: std::iter::once(program).chain(args.iter().map(String::as_str)).map(shell_quote).collect::<Vec<_>>().join(" "),
                program: program.into(),
                args: args.iter().map(Into::into).collect(),
            };
            // 和 `run_shell` 一样经过命令钩子（沙箱）。
            for wrap in call.ctx.plugins.hooks::<WrapCommand>() {
                wrap(&mut shell, &call.ctx);
            }
            Ok((shell, cwd))
        })?;
        let remaining = match options.timeout_ms {
            Some(limit) => self.remaining().min(Duration::from_millis(limit)),
            None => self.remaining(),
        };
        let work = async move {
            use tokio::io::AsyncWriteExt as _;
            let mut process = tokio::process::Command::new(&command.program);
            process
                .args(&command.args)
                .current_dir(&workspace)
                .envs(&options.env)
                .stdin(if options.stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            hide_window(&mut process);
            let mut child = process.spawn().map_err(|error| format!("cannot start {}: {error}", command.script))?;
            if let (Some(input), Some(mut stdin)) = (options.stdin, child.stdin.take()) {
                stdin.write_all(input.as_bytes()).await.map_err(|error| format!("cannot write to stdin: {error}"))?;
            }
            let output = child.wait_with_output().await.map_err(|error| format!("{} failed: {error}", command.script))?;
            let text = |bytes: &[u8]| {
                let cut = bytes.len().min(MAX_EXEC_OUTPUT);
                let mut text = String::from_utf8_lossy(&bytes[..cut]).into_owned();
                if bytes.len() > cut {
                    text.push_str("\n… [truncated]");
                }
                text
            };
            Ok(json!({ "code": output.status.code(), "stdout": text(&output.stdout), "stderr": text(&output.stderr) }).to_string())
        };
        self.ops.start(work, remaining)
    }

    /// `host.llm.complete` / `stream` 共用：检查能力和次数，解析出要连的模型和请求。默认用当前会话的模型；
    /// `model` 换模型（`provider/model` 换提供商，只写模型名时沿用当前提供商），`effort` 指定思考等级。
    fn prepare_llm(&self, request: &str) -> Result<(LlmAccess, dshr_llm::ChatRequest), String> {
        #[derive(Deserialize)]
        struct Turn {
            role: String,
            content: String,
        }
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Request {
            system: Option<String>,
            prompt: Option<String>,
            messages: Vec<Turn>,
            max_tokens: Option<u32>,
            model: Option<String>,
            effort: Option<String>,
        }
        if !self.capabilities.llm {
            return Err("this plugin does not declare the `llm` capability in plugin.toml".into());
        }
        let request: Request = serde_json::from_str(request).map_err(|error| format!("invalid llm request: {error}"))?;
        let access = self.with_call(|call| {
            if !call.permits.llm {
                return Err("the model is not available here (only in tools, async hooks and page actions)".into());
            }
            call.ctx.options.get::<LlmAccess>().cloned().ok_or_else(|| "the model is not available here (there is no session)".to_owned())
        })?;
        if self.llm_requests.get() >= MAX_LLM_REQUESTS {
            return Err(format!("at most {MAX_LLM_REQUESTS} model requests per call"));
        }
        let access = match request.model.as_deref().map(str::trim).filter(|model| !model.is_empty()) {
            None => access,
            // 只写模型名：同一个提供商。
            Some(model) if !model.contains('/') => LlmAccess { provider: access.provider, model: model.to_owned() },
            Some(spec) => {
                let resolve = crate::app::app_services()
                    .models
                    .ok_or_else(|| format!("this program cannot switch to another provider's model ({spec})"))?;
                resolve(spec)
                    .ok_or_else(|| format!("the model {spec} is not available: the provider is not configured, or it has no API key"))?
            }
        };
        let effort = match request.effort.as_deref() {
            None => None,
            Some(effort) => Some(
                dshr_llm::ReasoningEffort::ALL
                    .into_iter()
                    .find(|known| known.as_str() == effort)
                    .ok_or_else(|| format!("unknown effort {effort} (use off, low, high or max)"))?,
            ),
        };
        let mut messages = Vec::new();
        for turn in request.messages {
            let role = match turn.role.as_str() {
                "user" => dshr_core::Role::User,
                "assistant" => dshr_core::Role::Assistant,
                other => return Err(format!("message role must be user or assistant, not {other}")),
            };
            messages.push(dshr_core::Message { role, content: vec![dshr_core::ContentBlock::text(turn.content)] });
        }
        if let Some(prompt) = request.prompt {
            messages.push(dshr_core::Message { role: dshr_core::Role::User, content: vec![dshr_core::ContentBlock::text(prompt)] });
        }
        if messages.is_empty() {
            return Err("give a prompt or at least one message".into());
        }
        self.llm_requests.set(self.llm_requests.get() + 1);
        let chat = dshr_llm::ChatRequest {
            model: access.model.clone(),
            system: request.system,
            messages,
            tools: Vec::new(),
            max_tokens: Some(request.max_tokens.unwrap_or(4096).min(32_000)),
            effort,
        };
        Ok((access, chat))
    }

    /// 发一次不带工具的请求，返回文本。
    fn llm(&self, request: &str) -> Result<u64, String> {
        let (access, chat) = self.prepare_llm(request)?;
        let work = async move {
            use futures::StreamExt as _;
            let mut stream = access.provider.stream(chat).await.map_err(|error| error.to_string())?;
            let mut accumulator = dshr_llm::MessageAccumulator::default();
            while let Some(event) = stream.next().await {
                let event = event.map_err(|error| error.to_string())?;
                accumulator.push(&event).map_err(|error| error.to_string())?;
                // 消息已经结束：不再等连接关闭。
                if event == dshr_llm::StreamEvent::MessageStop {
                    break;
                }
            }
            let message = accumulator.finish();
            let text: String = message
                .content
                .iter()
                .filter_map(|block| match block {
                    dshr_core::ContentBlock::Text { text } => Some(&**text),
                    _ => None,
                })
                .collect();
            Ok(json!({ "text": text, "usage": message.usage }).to_string())
        };
        self.ops.start(work, self.remaining())
    }

    /// 流式请求：操作完成（连上模型）后，`prelude.js` 用 `claimStream` 取到一个流，每块是一段 JSON：
    /// `{ type: "text" | "thinking", text }`，最后一块是 `{ type: "done", text, usage }`（`text` 是全文）。
    /// 流属于这次调用：调用结束时关掉，模型那边的请求随之中止。
    fn llm_stream(&self, request: &str) -> Result<u64, String> {
        let (access, chat) = self.prepare_llm(request)?;
        let slot: Arc<std::sync::Mutex<Option<super::BodyReceiver>>> = Arc::default();
        let filled = slot.clone();
        let work = async move {
            use futures::StreamExt as _;
            let mut stream = access.provider.stream(chat).await.map_err(|error| error.to_string())?;
            let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Vec<u8>, String>>(64);
            *filled.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(receiver);
            // 连上之后在后台一直读：脚本不读了（流被关掉）时发送失败，请求随之丢弃。
            tokio::spawn(async move {
                let mut accumulator = dshr_llm::MessageAccumulator::default();
                let chunk = |value: Value| Ok(value.to_string().into_bytes());
                while let Some(event) = stream.next().await {
                    let event = match event {
                        Ok(event) => event,
                        Err(error) => {
                            let _ = sender.send(Err(error.to_string())).await;
                            return;
                        }
                    };
                    let piece = match &event {
                        dshr_llm::StreamEvent::TextDelta { text, .. } => Some(json!({ "type": "text", "text": text })),
                        dshr_llm::StreamEvent::ThinkingDelta { thinking, .. } => Some(json!({ "type": "thinking", "text": thinking })),
                        _ => None,
                    };
                    if let Err(error) = accumulator.push(&event) {
                        let _ = sender.send(Err(error.to_string())).await;
                        return;
                    }
                    if let Some(piece) = piece
                        && sender.send(chunk(piece)).await.is_err()
                    {
                        return;
                    }
                    if event == dshr_llm::StreamEvent::MessageStop {
                        break;
                    }
                }
                let message = accumulator.finish();
                let text: String = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        dshr_core::ContentBlock::Text { text } => Some(&**text),
                        _ => None,
                    })
                    .collect();
                let _ = sender.send(chunk(json!({ "type": "done", "text": text, "usage": message.usage }))).await;
            });
            Ok("{}".to_owned())
        };
        let id = self.ops.start(work, self.remaining())?;
        self.pending_streams.borrow_mut().push((id, slot));
        Ok(id)
    }

    // ---- 会话、密钥 ----

    /// 当前会话的事件（最近的在后）：`{ kinds?: [...], last?: n }`。
    fn session_events(&self, options: &str) -> Result<String, String> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Options {
            kinds: Option<Vec<String>>,
            last: Option<usize>,
        }
        if !self.capabilities.session_read {
            return Err("this plugin does not declare the `session:read` capability in plugin.toml".into());
        }
        let options: Options = serde_json::from_str(options).map_err(|error| format!("invalid options: {error}"))?;
        let info =
            self.with_call(|call| call.ctx.options.get::<SessionInfo>().cloned().ok_or_else(|| "there is no session here".to_owned()))?;
        let wanted = |kind: &str| options.kinds.as_ref().is_none_or(|kinds| kinds.iter().any(|wanted| wanted == kind));
        let last = options.last.unwrap_or(50).min(MAX_SESSION_EVENTS);
        let picked = recent_event_lines(&info.log_path, last, MAX_SESSION_BYTES, &wanted)
            .map_err(|error| format!("cannot read the session: {error}"))?;
        let mut reply = format!(r#"{{"id":"{}","events":["#, info.id);
        for (index, text) in picked.iter().enumerate() {
            if index > 0 {
                reply.push(',');
            }
            reply.push_str(text);
        }
        reply.push_str("]}");
        Ok(reply)
    }

    pub(super) fn secret_value(&self, name: &str) -> Result<Option<String>, String> {
        self.secret(name)
    }

    /// 清单里声明过的密钥；没设置时是 `None`。
    fn secret(&self, name: &str) -> Result<Option<String>, String> {
        if !self.secrets.iter().any(|declared| declared == name) {
            return Err(format!("the secret {name} is not declared: add it to `secrets` in plugin.toml"));
        }
        let value = crate::secrets::load(self.plugin, name);
        if let Some(value) = &value {
            let mut revealed = self.revealed.borrow_mut();
            if !revealed.contains(value) {
                revealed.push(value.clone());
            }
        }
        Ok(value)
    }

    /// 插件只能写自己命名空间里的事件（`<插件 id>/<名字>`）：别的插件的事件怎么进上下文、怎么显示，
    /// 是那个插件定的，不能冒充。
    fn emit_event(&self, kind: &str, json: &str) -> Result<(), String> {
        if kind.split_once('/').is_none_or(|(namespace, _)| namespace != self.plugin) {
            return Err(format!("event kinds of this plugin must start with `{}/`, got `{kind}`", self.plugin));
        }
        let data = serde_json::from_str(json).map_err(|error| format!("event data is not valid JSON: {error}"))?;
        self.with_call(|call| {
            call.ctx.emit_event(kind, data);
            Ok(())
        })
    }

    fn output(&self, text: &str) {
        if let Some(call) = &*self.call.borrow() {
            call.ctx.emit_output(text);
        }
    }

    fn workspace(&self) -> String {
        self.call.borrow().as_ref().map(|call| call.ctx.workspace.display().to_string()).unwrap_or_default()
    }

    fn log(&self, level: &str, message: &str) {
        // 交给过脚本的密钥不进日志。
        let mut message = message.to_owned();
        for secret in self.revealed.borrow().iter().filter(|secret| secret.len() >= 4) {
            message = message.replace(secret.as_str(), "***");
        }
        self.logs.push(level, &message);
        match level {
            "error" => tracing::error!(plugin = self.plugin, "{message}"),
            "warn" => tracing::warn!(plugin = self.plugin, "{message}"),
            "debug" => tracing::debug!(plugin = self.plugin, "{message}"),
            _ => tracing::info!(plugin = self.plugin, "{message}"),
        }
    }

    /// 工具交回的图片：写一条 `media/images` 事件，排在这次调用的结果后面。
    fn tool_images(&self, images: &str) -> Result<(), String> {
        let images: Vec<Value> = serde_json::from_str(images).map_err(|error| format!("invalid images: {error}"))?;
        if images.len() > MAX_IMAGES {
            return Err(format!("a tool can return at most {MAX_IMAGES} images"));
        }
        for image in &images {
            let media_type = image.get("media_type").and_then(Value::as_str).unwrap_or_default();
            check_image(media_type, image.get("data").and_then(Value::as_str).unwrap_or_default())?;
        }
        self.with_call(|call| {
            let call_id = call.ctx.options.get::<dshr_agent::CallEnv>().map(|env| env.call_id.clone());
            call.ctx.emit_event(dshr_core::MEDIA_IMAGES, json!({ "call_id": call_id, "images": images }));
            Ok(())
        })
    }

    /// `host.tools.set([...])`：检查名字后整组换掉动态工具。
    fn tools_set(&self, specs: &str) -> Result<(), String> {
        const MAX_DYNAMIC_TOOLS: usize = 128;
        let decls: Vec<super::runtime::ToolDecl> = serde_json::from_str(specs).map_err(|error| format!("invalid tools: {error}"))?;
        if decls.len() > MAX_DYNAMIC_TOOLS {
            return Err(format!("at most {MAX_DYNAMIC_TOOLS} dynamic tools"));
        }
        let dynamic = self.dynamic.borrow().clone().ok_or("dynamic tools are not available here")?;
        let reserved = dynamic.reserved.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        let mut seen = std::collections::HashSet::new();
        for decl in &decls {
            if !super::valid_tool_name(&decl.name) {
                return Err(format!(
                    "tool name `{}` is invalid: letters, digits, `_` and `-` only, at most 64, not starting with mcp__",
                    decl.name
                ));
            }
            if reserved.contains(&decl.name) {
                return Err(format!("the tool name `{}` is already taken", decl.name));
            }
            if !seen.insert(decl.name.clone()) {
                return Err(format!("the tool `{}` is defined twice", decl.name));
            }
        }
        *dynamic.decls.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = decls;
        Ok(())
    }

    /// `host.ui.hide(target)` / `show(target)`：变了就通知界面重新算入口。
    fn ui_visibility(&self, target: &str, hidden: bool) -> Result<(), String> {
        let visibility = self.visibility.borrow().clone().ok_or("entries cannot be changed here")?;
        if visibility.set(target.trim(), hidden)? {
            dshr_agent::notify_entries_changed();
        }
        Ok(())
    }

    fn ui_hidden(&self) -> String {
        let hidden = self.visibility.borrow().as_ref().map(|visibility| visibility.hidden()).unwrap_or_default();
        serde_json::to_string(&hidden).unwrap_or_else(|_| "[]".to_owned())
    }

    fn refresh_page(&self, page: &str) {
        let mut pages = self.refresh.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *pages.entry(page.to_owned()).or_default() += 1;
        drop(pages);
        dshr_agent::notify_page_refresh();
    }
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|error| format!("invalid base64: {error}"))
}

/// POSIX shell 的单引号写法：沙箱钩子把命令包进 `sh -c` 时用。
fn shell_quote(word: &str) -> String {
    if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@".contains(c)) {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// 日志里最近的、类型对得上的事件行（原文，按时间先后）：最多 `last` 条，总长不超过 `max_bytes`
/// （只挑得出一条时那一条照样给）。从最新的往前挑，碰到放不下的就停。
///
/// 不把整份日志读进内存：未压缩的日志从文件末尾往回一行行读，挑够就停，长会话里只读到用得着的那一截；
/// 压缩的日志只能从头解压，顺着读一遍，手里只留当前挑中的那几条。
fn recent_event_lines(path: &Path, last: usize, max_bytes: usize, wanted: &dyn Fn(&str) -> bool) -> std::io::Result<Vec<String>> {
    if last == 0 {
        return Ok(Vec::new());
    }
    let matches = |line: &str| event_type(line).is_some_and(|kind| wanted(&kind));
    let compressed = path.extension().is_some_and(|ext| ext == "zstd" || ext == "zst");
    if compressed {
        use std::io::BufRead as _;
        let mut picked: VecDeque<String> = VecDeque::new();
        let mut bytes = 0;
        for line in dshr_core::log_reader(path)?.lines() {
            let line = line?;
            if line.trim().is_empty() || !matches(&line) {
                continue;
            }
            bytes += line.len();
            picked.push_back(line);
            // 留下的是“以这一行结尾、放得下的最长一段”，读完时正好就是从后往前挑出来的那一段。
            while picked.len() > last || (picked.len() > 1 && bytes > max_bytes) {
                bytes -= picked.pop_front().map_or(0, |line| line.len());
            }
        }
        return Ok(picked.into());
    }
    let mut lines = ReverseLines::open(path)?;
    let mut picked = Vec::new();
    let mut bytes = 0;
    while picked.len() < last {
        let Some(line) = lines.next_line()? else { break };
        if !matches(&line) {
            continue;
        }
        if !picked.is_empty() && bytes + line.len() > max_bytes {
            break;
        }
        bytes += line.len();
        picked.push(line);
    }
    picked.reverse();
    Ok(picked)
}

/// 一行事件的 `type`。只认这一个字段，其余的跳过不建值（工具输出动辄几十 KB，不必为认类型解析出来）。
/// 不是 JSON 对象的行返回 `None`。
fn event_type(line: &str) -> Option<std::borrow::Cow<'_, str>> {
    #[derive(Deserialize)]
    struct Kind<'a> {
        #[serde(rename = "type", borrow, default)]
        kind: Option<std::borrow::Cow<'a, str>>,
    }
    serde_json::from_str::<Kind<'_>>(line).ok()?.kind
}

/// 从文件末尾往前一行行读（跳过空行、不是 UTF-8 的行）。只读到调用方停下的位置为止。
///
/// 先往前找换行符定出一行的起止，再一次读出整行：再长的行也只读、只分配一次。
pub(super) struct ReverseLines {
    file: std::fs::File,
    /// 下一行（往前数）的结束位置（不含换行符）；`None` 表示已经读到文件开头。
    end: Option<u64>,
    chunk: Vec<u8>,
}

impl ReverseLines {
    const CHUNK: u64 = 64 * 1024;

    pub(super) fn open(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let end = file.metadata()?.len();
        Ok(Self { file, end: Some(end), chunk: Vec::new() })
    }

    pub(super) fn next_line(&mut self) -> std::io::Result<Option<String>> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        while let Some(end) = self.end {
            let start = self.line_start(end)?;
            // 再往前一行的结尾是这一行开头前面的换行符。
            self.end = start.checked_sub(1);
            let mut line = vec![0; (end - start) as usize];
            self.file.seek(SeekFrom::Start(start))?;
            self.file.read_exact(&mut line)?;
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if let Ok(line) = String::from_utf8(line) {
                return Ok(Some(line));
            }
        }
        Ok(None)
    }

    /// `end` 所在这一行的开头：往前找到的第一个换行符之后，找不到就是文件开头。
    fn line_start(&mut self, end: u64) -> std::io::Result<u64> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut cursor = end;
        while cursor > 0 {
            let from = cursor.saturating_sub(Self::CHUNK);
            self.chunk.resize((cursor - from) as usize, 0);
            self.file.seek(SeekFrom::Start(from))?;
            self.file.read_exact(&mut self.chunk)?;
            if let Some(newline) = self.chunk.iter().rposition(|byte| *byte == b'\n') {
                return Ok(from + newline as u64 + 1);
            }
            cursor = from;
        }
        Ok(0)
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// 一个插件的数据目录最多这么大（含 `host.db` 的数据库文件）。
pub(super) const MAX_DATA_BYTES: u64 = 512 * 1024 * 1024;

fn append_to(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::OpenOptions::new().create(true).append(true).open(path)?.write_all(bytes)
}

/// 子进程不创建控制台窗口（Windows 的 `CREATE_NO_WINDOW`）。桌面版没有控制台，不加这个标志
/// 插件每启动一个命令行程序（git、ssh……）都会弹出一个黑色的控制台窗口。
pub(super) fn hide_window(command: &mut tokio::process::Command) {
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    #[cfg(not(windows))]
    let _ = command;
}

/// 数据目录里的相对路径；`..`、绝对路径一律拒绝。数据目录是宿主建的，里面没有指到外面的链接。
pub(super) fn confine_data(dir: &Path, path: &str) -> Result<PathBuf, String> {
    let relative = Path::new(path);
    let plain = !path.is_empty() && relative.components().all(|part| matches!(part, std::path::Component::Normal(_)));
    if !plain {
        return Err(format!("{path}: data paths must be relative paths inside the plugin's data folder (no `..`)"));
    }
    Ok(dir.join(relative))
}

/// 数据目录的占用多久重新统计一次。期间 `writeData` / `appendData` / `removeData` 自己增减；
/// 别的途径（`host.db` 的数据库文件变大、用户手动删文件）在下次统计时算进来。配额本来就是防失控的粗略上限。
const DATA_USAGE_TTL: Duration = Duration::from_secs(10);

/// 各个插件数据目录的占用（同一个插件的几个实例共用一份）。
static DATA_USAGE: LazyLock<Mutex<HashMap<PathBuf, (Instant, u64)>>> = LazyLock::new(Default::default);

/// 数据目录现在的占用：缓存没过期时直接用，不用每写一次都把目录里每个文件 stat 一遍。
pub(super) fn data_usage(dir: &Path) -> u64 {
    let now = Instant::now();
    if let Some((counted, bytes)) = DATA_USAGE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(dir)
        && now.duration_since(*counted) < DATA_USAGE_TTL
    {
        return *bytes;
    }
    let bytes = folder_size(dir);
    DATA_USAGE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(dir.to_owned(), (now, bytes));
    bytes
}

/// 一个文件从 `before` 字节变成 `after` 字节之后，更新缓存的占用（没有缓存时不用管，下次会统计）。
fn adjust_data_usage(dir: &Path, before: u64, after: u64) {
    if let Some((_, bytes)) = DATA_USAGE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get_mut(dir) {
        *bytes = (*bytes + after).saturating_sub(before);
    }
}

fn folder_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => folder_size(&entry.path()),
            Ok(metadata) => metadata.len(),
            Err(_) => 0,
        })
        .sum()
}

/// 把脚本给的路径解析成工作区里的路径。字面上跑出工作区的（`../x`、别处的绝对路径）拒绝；
/// 经过符号链接指到工作区外的也拒绝：对已经存在的最深一层取真实路径再比一次。
pub(super) fn confine_pub(ctx: &ToolContext, path: &str) -> Result<PathBuf, String> {
    confine(ctx, path)
}

fn confine(ctx: &ToolContext, path: &str) -> Result<PathBuf, String> {
    let outside = || format!("{path} is outside the workspace; plugins can only access files inside it");
    let resolved = ctx.resolve(path);
    if !ctx.is_inside_workspace(&resolved) {
        return Err(outside());
    }
    let root = ctx.workspace.canonicalize().map_err(|error| format!("cannot resolve the workspace: {error}"))?;
    let mut probe = Some(resolved.as_path());
    while let Some(existing) = probe {
        if let Ok(real) = existing.canonicalize() {
            if !real.starts_with(&root) {
                return Err(outside());
            }
            break;
        }
        probe = existing.parent();
    }
    Ok(resolved)
}

/// `crypto.getRandomValues` / `randomUUID` 用的随机字节（base64）。
fn random_bytes(count: usize) -> Result<String, String> {
    if count > 65536 {
        return Err("getRandomValues can fill at most 65536 bytes at once".into());
    }
    let mut bytes = vec![0; count];
    getrandom::fill(&mut bytes).map_err(|error| format!("no randomness available: {error}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// `crypto.subtle.digest`：SHA-256 / SHA-384 / SHA-512。
fn digest(algorithm: &str, encoded: &str) -> Result<String, String> {
    use sha2::Digest as _;
    let data = decode_base64(encoded)?;
    let hash = match algorithm.to_ascii_uppercase().as_str() {
        "SHA-256" => sha2::Sha256::digest(&data).to_vec(),
        "SHA-384" => sha2::Sha384::digest(&data).to_vec(),
        "SHA-512" => sha2::Sha512::digest(&data).to_vec(),
        other => return Err(format!("unsupported digest algorithm {other} (SHA-256, SHA-384 and SHA-512 are available)")),
    };
    Ok(base64::engine::general_purpose::STANDARD.encode(hash))
}

/// `URL` 的解析：WHATWG 规则（`url` crate），返回各部分。
fn parse_url(input: &str, base: Option<&str>) -> Result<String, String> {
    let url = match base {
        Some(base) => {
            reqwest::Url::parse(base).and_then(|base| base.join(input)).map_err(|error| format!("Invalid URL {input}: {error}"))?
        }
        None => reqwest::Url::parse(input).map_err(|error| format!("Invalid URL {input}: {error}"))?,
    };
    let origin = url.origin().ascii_serialization();
    Ok(json!({
        "href": url.as_str(),
        "protocol": format!("{}:", url.scheme()),
        "username": url.username(),
        "password": url.password().unwrap_or_default(),
        "hostname": url.host_str().unwrap_or_default(),
        "port": url.port().map(|port| port.to_string()).unwrap_or_default(),
        "pathname": url.path(),
        "search": url.query().map(|query| format!("?{query}")).unwrap_or_default(),
        "hash": url.fragment().map(|fragment| format!("#{fragment}")).unwrap_or_default(),
        "origin": if origin == "null" { "null".to_owned() } else { origin },
    })
    .to_string())
}

/// 建出交给 `prelude.js` 的原生函数对象。
pub(crate) fn install<'js>(ctx: &Ctx<'js>, state: &Rc<State>) -> rquickjs::Result<Object<'js>> {
    let native = Object::new(ctx.clone())?;

    macro_rules! native {
        // 不会出错的。
        ($name:literal, |$host:ident $(, $arg:ident: $ty:ty)*| plain $body:expr) => {{
            let $host = state.clone();
            native.set($name, Function::new(ctx.clone(), move |$($arg: $ty),*| $body)?)?;
        }};
        // 出错时变成脚本里的异常。
        ($name:literal, |$host:ident $(, $arg:ident: $ty:ty)*| $body:expr) => {{
            let $host = state.clone();
            native.set(
                $name,
                Function::new(ctx.clone(), move |ctx: Ctx<'js>, $($arg: $ty),*| {
                    $body.map_err(|message: String| Exception::throw_message(&ctx, &message))
                })?,
            )?;
        }};
    }

    native!("log", |host, level: String, message: String| plain host.log(&level, &message));
    native!("output", |host, text: String| plain host.output(&text));
    native!("workspace", |host| plain host.workspace());
    // 多语言（`host.i18n`）：界面当前语言；给定可用的语言，按回退顺序排好。
    native!("locale", |_host| plain crate::locale::current_locale());
    native!("localeCandidates", |_host, locale: String, available: String, fallback: String| plain {
        let available: Vec<String> = serde_json::from_str(&available).unwrap_or_default();
        serde_json::to_string(&crate::locale::candidates(&locale, available.iter().map(String::as_str), &fallback))
            .unwrap_or_else(|_| "[]".to_owned())
    });
    native!("emitEvent", |host, kind: String, json: String| host.emit_event(&kind, &json));
    native!("readFile", |host, path: String| host.read_file(&path));
    native!("readFileBytes", |host, path: String| host.read_file_bytes(&path));
    native!("listDir", |host, path: String| host.list_dir(&path));
    native!("stat", |host, path: String| host.stat(&path));
    native!("glob", |host, pattern: String| host.glob(&pattern).map(|id| id as f64));
    native!("grep", |host, pattern: String, options: String| host.grep(&pattern, &options).map(|id| id as f64));
    native!("writeFile", |host, path: String, text: String| host.write_bytes(&path, text.as_bytes()));
    native!("writeFileBytes", |host, path: String, encoded: String| host.write_file_bytes(&path, &encoded));
    native!("mkdir", |host, path: String| host.mkdir(&path));
    native!("remove", |host, path: String, recursive: bool| host.remove(&path, recursive));
    native!("rename", |host, from: String, to: String| host.rename(&from, &to));
    native!("readData", |host, path: String| host.read_data(&path));
    native!("writeData", |host, path: String, text: String| host.write_data(&path, &text));
    native!("appendData", |host, path: String, text: String| host.append_data(&path, &text));
    native!("appendFile", |host, path: String, text: String| host.append_file(&path, &text));
    native!("removeData", |host, path: String| host.remove_data(&path));
    native!("dbExec", |host, name: String, sql: String, params: String| host.db_exec(&name, &sql, &params));
    native!("dbQuery", |host, name: String, sql: String, params: String| host.db_query(&name, &sql, &params));
    native!("dbClose", |host, name: String| host.db_close(&name));
    native!("zip", |_host, entries: String| super::storage::zip(&entries));
    native!("unzip", |_host, encoded: String| super::storage::unzip(&encoded));
    native!("endTurn", |host| host.end_turn());
    native!("identity", |host| plain host.identity());
    native!("listData", |host| plain host.list_data());
    // 慢操作：返回操作编号（JS 的 number 能精确表示）。
    native!("fetch", |host, url: String, options: String| host.fetch(&url, &options).map(|id| id as f64));
    native!("exec", |host, program: String, args: String, options: String| host.exec(&program, &args, &options).map(|id| id as f64));
    native!("llm", |host, request: String| host.llm(&request).map(|id| id as f64));
    native!("llmStream", |host, request: String| host.llm_stream(&request).map(|id| id as f64));
    native!("spawn", |host, program: String, args: String, options: String| host.spawn(&program, &args, &options).map(|id| id as f64));
    native!("procRead", |host, id: f64| host.proc_read(id as u64).map(|op| op as f64));
    native!("procWrite", |host, id: f64, text: Option<String>| host.proc_write(id as u64, text).map(|op| op as f64));
    native!("procKill", |host, id: f64| host.proc_kill(id as u64));
    native!("procWait", |host, id: f64| host.proc_wait(id as u64).map(|op| op as f64));
    native!("abort", |host, id: f64| plain host.ops.abort(id as u64));
    native!("sessionEvents", |host, options: String| host.session_events(&options));
    native!("secret", |host, name: String| host.secret(&name));
    native!("toolImages", |host, images: String| host.tool_images(&images));
    native!("uiRefresh", |host, page: String| plain host.refresh_page(&page));
    native!("uiVisibility", |host, target: String, hidden: bool| host.ui_visibility(&target, hidden));
    native!("uiHidden", |host| plain host.ui_hidden());
    native!("fetchStream", |host, url: String, options: String| host.fetch_stream(&url, &options).map(|id| id as f64));
    native!("claimStream", |host, op: f64| host.claim_stream(op as u64).map(|id| id as f64));
    native!("readChunk", |host, stream: f64| host.read_chunk(stream as u64).map(|id| id as f64));
    native!("websocket", |host, url: String, options: String| host.websocket(&url, &options).map(|id| id as f64));
    native!("claimSocket", |host, op: f64| host.claim_socket(op as u64).map(|id| id as f64));
    native!("wsSend", |host, socket: f64, data: String, binary: bool| host.ws_send(socket as u64, &data, binary).map(|id| id as f64));
    native!("wsNext", |host, socket: f64| host.ws_next(socket as u64).map(|id| id as f64));
    native!("wsClose", |host, socket: f64| host.ws_close(socket as u64).map(|id| id as f64));
    native!("oauthAuthorize", |host, request: String| host.oauth_authorize(&request).map(|id| id as f64));
    native!("oauthToken", |host, name: String| host.oauth_token(&name).map(|id| id as f64));
    native!("sessionSend", |host, request: String| host.session_send(&request));
    native!("sessionStop", |host, session: Option<String>| host.session_stop(session));
    native!("sessionList", |host| host.session_list());
    native!("sessionSet", |host, request: String| host.session_set(&request));
    native!("sessionModels", |host| host.session_models());
    native!("agentRun", |host, request: String| host.agent_run(&request).map(|id| id as f64));
    native!("notify", |host, request: String| host.notify(&request));
    native!("composerSet", |host, text: String| host.composer_set(&text));
    native!("composerGet", |host| host.composer_get());
    native!("dshr", |host, method: String, args: String| host.dshr(&method, &args));
    native!("plugins", |host, method: String, args: String| host.plugins_call(&method, &args));
    native!("toolsSet", |host, specs: String| host.tools_set(&specs));
    native!("pluginsApi", |host, id: String, method: String, args: String| host.plugins_api(&id, &method, &args).map(|op| op as f64));
    native!("pluginsInstall", |host, request: String| host.plugins_install(&request).map(|id| id as f64));
    native!("randomBytes", |_host, count: usize| random_bytes(count));
    native!("digest", |_host, algorithm: String, encoded: String| digest(&algorithm, &encoded));
    native!("parseUrl", |_host, input: String, base: Option<String>| parse_url(&input, base.as_deref()));
    Ok(native)
}

#[cfg(test)]
mod tests {
    use super::{ReverseLines, adjust_data_usage, data_usage, recent_event_lines};

    #[test]
    fn data_usage_is_cached_and_adjusted_by_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a"), [0; 100]).unwrap();
        assert_eq!(data_usage(dir.path()), 100);
        // 别的途径写的文件要等缓存过期才算进来；自己写的立刻增减。
        std::fs::write(dir.path().join("b"), [0; 50]).unwrap();
        assert_eq!(data_usage(dir.path()), 100);
        adjust_data_usage(dir.path(), 0, 30);
        adjust_data_usage(dir.path(), 100, 20);
        assert_eq!(data_usage(dir.path()), 50);
        adjust_data_usage(dir.path(), 500, 0);
        assert_eq!(data_usage(dir.path()), 0);
    }

    fn event(seq: usize, kind: &str, text: &str) -> String {
        serde_json::json!({ "seq": seq, "time": 1, "type": kind, "data": { "text": text } }).to_string()
    }

    #[test]
    fn reverse_lines_reads_back_to_front_across_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        // 比一块（64 KB）还长的行、空行、Windows 换行、结尾没有换行符。
        let long = "z".repeat(150_000);
        std::fs::write(&path, format!("first\n\n{long}\r\n   \nlast")).unwrap();
        let mut lines = ReverseLines::open(&path).unwrap();
        assert_eq!(lines.next_line().unwrap().as_deref(), Some("last"));
        assert_eq!(lines.next_line().unwrap().as_deref(), Some(long.as_str()));
        assert_eq!(lines.next_line().unwrap().as_deref(), Some("first"));
        assert_eq!(lines.next_line().unwrap(), None);
        assert_eq!(lines.next_line().unwrap(), None);
    }

    #[test]
    fn recent_lines_respect_kinds_count_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("session.v1.jsonl");
        let mut lines = Vec::new();
        for seq in 1..=20 {
            let kind = if seq % 2 == 0 { "user/message" } else { "tool/result" };
            lines.push(event(seq, kind, &"t".repeat(seq * 10)));
        }
        lines.push("not json".to_owned());
        let text = lines.join("\n") + "\n";
        std::fs::write(&plain, &text).unwrap();
        let compressed = dir.path().join("session.v1.jsonl.zstd");
        std::fs::write(&compressed, zstd::encode_all(text.as_bytes(), 0).unwrap()).unwrap();

        let users = |kind: &str| kind == "user/message";
        let any = |_: &str| true;
        for path in [&plain, &compressed] {
            // 最近的三条用户消息，按时间先后。
            let picked = recent_event_lines(path, 3, usize::MAX, &users).unwrap();
            assert_eq!(picked, vec![lines[15].clone(), lines[17].clone(), lines[19].clone()]);
            // 字节数放不下时少给，但至少给一条。
            let budget = lines[19].len() + lines[18].len();
            assert_eq!(recent_event_lines(path, 10, budget, &any).unwrap(), vec![lines[18].clone(), lines[19].clone()]);
            assert_eq!(recent_event_lines(path, 10, 1, &any).unwrap(), vec![lines[19].clone()]);
            assert!(recent_event_lines(path, 0, usize::MAX, &any).unwrap().is_empty());
            assert_eq!(recent_event_lines(path, 100, usize::MAX, &any).unwrap().len(), 20);
        }
    }
}
