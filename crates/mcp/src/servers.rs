//! 一组 MCP 服务器（[`ServerSet`]）：每个服务器一个状态，断开后在后台重连，工具列表跟着服务器变。
//!
//! `config.toml` 里配置的服务器是一组（[`crate::McpPlugin`]），每个插件自带的服务器各是一组。
//!
//! 状态：
//! - **未连接**：刚配置好还没用过。第一次要工具时连接（同时连，各限时 30 秒），这一次要等。
//! - **连接中** → **可用**：握手、取工具列表（服务器有 prompts 时也取），之后一直复用。
//! - **失败**：连不上或断开了（进程退出、读循环结束、HTTP 会话失效）。在后台按 1 秒起翻倍、最多 5 分钟重连，
//!   不让对话等它；连续失败 [`MAX_ATTEMPTS`] 次就 **停下**，等用户点“重连”或改配置。
//! - **已停用**：配置里 `enabled = false`。
//!
//! 执行中的调用遇到断开直接报错，不自动重试：工具可能有副作用。

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use dshr_tools::{Access, Tool, ToolContext, ToolOutput, truncate};
use serde_json::{Map, Value, json};
use tokio::process::{Child, Command};

use crate::client::{Client, McpError, PromptInfo, ResourceInfo, ToolInfo};
use crate::config::{Launch, ServerConfig};

/// 启动服务器、握手并取到工具列表的时限。`npx` 第一次运行要下载包，给得宽一些。
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// 单次工具调用的默认时限。
pub(crate) const DEFAULT_CALL_TIMEOUT_MS: u64 = 120_000;
/// 模型接口对工具名的长度限制。
pub(crate) const MAX_TOOL_NAME: usize = 64;
/// 连续失败这么多次就停下重连。
pub const MAX_ATTEMPTS: u32 = 5;
const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(300);
/// 每个服务器留在内存里的日志行数（服务器的 stderr，以及连接、断开的记录）。
const LOG_LINES: usize = 200;

/// `CREATE_NO_WINDOW`：子进程不创建控制台窗口。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 一个服务器最近的日志。
#[derive(Default)]
pub struct LogBuffer {
    lines: Mutex<VecDeque<String>>,
}

impl LogBuffer {
    pub fn push(&self, line: impl Into<String>) {
        let mut lines = self.lines.lock().expect("日志锁中毒");
        if lines.len() == LOG_LINES {
            lines.pop_front();
        }
        lines.push_back(line.into());
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("日志锁中毒").iter().cloned().collect()
    }
}

/// 建立到服务器的连接（启动进程或准备 HTTP 客户端），还没握手。测试里换成内存假服务器。
#[async_trait::async_trait]
pub trait Connector: Send + Sync {
    async fn connect(&self, launch: &Launch, logs: &Arc<LogBuffer>) -> Result<Connection, McpError>;
}

/// 一个建立好的连接：客户端，以及（stdio 服务器的）进程。进程随它一起结束。
pub struct Connection {
    pub client: Client,
    pub process: Option<Child>,
}

/// 真正的连接方式：stdio 启动子进程，HTTP 直接连。
pub struct ProcessConnector;

#[async_trait::async_trait]
impl Connector for ProcessConnector {
    async fn connect(&self, launch: &Launch, logs: &Arc<LogBuffer>) -> Result<Connection, McpError> {
        match launch {
            Launch::Command { command, args, env, cwd } => {
                let mut process = Command::new(resolve_program(command));
                process.args(args).envs(env);
                if let Some(cwd) = cwd {
                    process.current_dir(cwd);
                }
                process.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
                #[cfg(windows)]
                process.creation_flags(CREATE_NO_WINDOW);
                let mut child = process.spawn().map_err(|source| McpError::Spawn { command: command.clone(), source })?;
                let stdin = child.stdin.take().expect("stdin 已设为 piped");
                let stdout = child.stdout.take().expect("stdout 已设为 piped");
                if let Some(stderr) = child.stderr.take() {
                    // 服务器的日志走 stderr：留最近的几百行给设置页看。一定要读走，否则管道满了服务器会卡住。
                    let logs = logs.clone();
                    tokio::spawn(async move {
                        use tokio::io::AsyncBufReadExt;
                        let mut lines = tokio::io::BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            logs.push(line);
                        }
                    });
                }
                Ok(Connection { client: Client::new(stdout, stdin), process: Some(child) })
            }
            Launch::Http { url, headers } => {
                let (transport, incoming) = crate::http::connect(url, headers)?;
                Ok(Connection { client: Client::with_transport(transport, incoming), process: None })
            }
        }
    }
}

/// 把配置里的占位符换成实际的值：`${env:NAME}`、`${secret:NAME}`，以及插件自带的服务器的 `${plugin}`、`${data}`。
/// 参数是服务器名、占位符的种类和名字（`${env:HOME}` 是 `("env", "HOME")`，`${plugin}` 是 `("plugin", "")`）。
pub type Resolver = Arc<dyn Fn(&str, &str, &str) -> Result<String, String> + Send + Sync>;

/// 服务器现在的状态（设置页、`dshr mcp list` 显示）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    Disabled,
    Idle,
    Connecting,
    Ready,
    Failed,
    Stopped,
    /// 远程服务器要求登录（HTTP 401）：设置页“登录”、`dshr mcp login`。
    NeedsLogin,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServerStatus {
    /// 配置里的名字。
    pub name: String,
    /// 自带这个服务器的插件；`config.toml` 里配置的是 `None`。
    pub owner: Option<String>,
    pub state: ServerState,
    /// 失败、停下的原因。
    pub error: Option<String>,
    pub tools: usize,
    pub resources: bool,
    pub prompts: usize,
    /// 失败时离下次重连还有多久（秒）。
    pub retry_in: Option<u64>,
    /// 连续失败了几次。
    pub attempts: u32,
    /// 存着这个服务器的登录 token。
    pub signed_in: bool,
}

impl ServerStatus {
    /// 跨组唯一的名字：`config.toml` 里的是名字本身，插件自带的是 `<插件>/<名字>`。
    pub fn qualified(&self) -> String {
        qualified(self.owner.as_deref(), &self.name)
    }
}

fn qualified(owner: Option<&str>, name: &str) -> String {
    match owner {
        Some(owner) => format!("{owner}/{name}"),
        None => name.to_owned(),
    }
}

/// 一个可用的连接，以及从它取到的东西。
struct Live {
    client: Client,
    _process: Option<Child>,
    tools: Mutex<Vec<Arc<dyn Tool>>>,
    prompts: Mutex<Vec<PromptInfo>>,
    resources: bool,
    /// 这次连上之前已经连续失败了几次、什么时候连上的：连上没多久又断了算接着失败，免得一直崩溃的服务器
    /// 每秒重启一次。
    attempts: u32,
    since: Instant,
}

/// 连上这么久之后才断开，就当它正常工作过，失败次数从头算。
const STABLE: Duration = Duration::from_secs(30);

enum State {
    Disabled,
    Idle,
    Connecting,
    Ready(Arc<Live>),
    Failed { error: String, attempts: u32, retry_at: Instant },
    Stopped { error: String, attempts: u32 },
    NeedsLogin { error: String },
}

/// 连接失败的原因以它开头时，是服务器要求登录。
const NEEDS_LOGIN: &str = "需要登录";

struct Slot {
    name: String,
    owner: Option<String>,
    config: ServerConfig,
    state: Mutex<State>,
    /// 同一时间只有一个连接在建。
    connecting: tokio::sync::Mutex<()>,
    /// 每次换状态（新连接、重新配置、手动重连）加一；后台任务看到它变了就不再动这个服务器。
    epoch: AtomicU64,
    logs: Arc<LogBuffer>,
}

impl Slot {
    fn label(&self) -> String {
        qualified(self.owner.as_deref(), &self.name)
    }

    fn live(&self) -> Option<Arc<Live>> {
        match &*self.state.lock().expect("状态锁中毒") {
            State::Ready(live) => Some(live.clone()),
            _ => None,
        }
    }

    fn set(&self, state: State) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        *self.state.lock().expect("状态锁中毒") = state;
    }

    fn status(&self) -> ServerStatus {
        let state = self.state.lock().expect("状态锁中毒");
        let mut status = ServerStatus {
            name: self.name.clone(),
            owner: self.owner.clone(),
            state: ServerState::Idle,
            error: None,
            tools: 0,
            resources: false,
            prompts: 0,
            retry_in: None,
            attempts: 0,
            signed_in: crate::oauth::load_token(&self.label()).is_some(),
        };
        match &*state {
            State::Disabled => status.state = ServerState::Disabled,
            State::Idle => {}
            State::Connecting => status.state = ServerState::Connecting,
            State::Ready(live) => {
                status.state = ServerState::Ready;
                status.tools = live.tools.lock().expect("工具锁中毒").len();
                status.prompts = live.prompts.lock().expect("prompts 锁中毒").len();
                status.resources = live.resources;
            }
            State::Failed { error, attempts, retry_at } => {
                status.state = ServerState::Failed;
                status.error = Some(error.clone());
                status.attempts = *attempts;
                status.retry_in = Some(retry_at.saturating_duration_since(Instant::now()).as_secs());
            }
            State::Stopped { error, attempts } => {
                status.state = ServerState::Stopped;
                status.error = Some(error.clone());
                status.attempts = *attempts;
            }
            State::NeedsLogin { error } => {
                status.state = ServerState::NeedsLogin;
                status.error = Some(error.clone());
            }
        }
        status
    }
}

struct SetInner {
    owner: Option<String>,
    connector: Arc<dyn Connector>,
    resolver: Mutex<Option<Resolver>>,
    slots: Mutex<BTreeMap<String, Arc<Slot>>>,
}

/// 所有还在用的服务器组：资源工具、`@` 提及、prompts 斜杠命令要跨组找服务器。
static SETS: LazyLock<Mutex<Vec<Weak<SetInner>>>> = LazyLock::new(Mutex::default);

/// 一组 MCP 服务器。克隆出来的指向同一组。
#[derive(Clone)]
pub struct ServerSet {
    inner: Arc<SetInner>,
}

impl ServerSet {
    /// `owner`：自带这些服务器的插件 id；`config.toml` 里配置的是 `None`。
    pub fn new(owner: Option<String>) -> Self {
        Self::with_connector(owner, Arc::new(ProcessConnector))
    }

    pub fn with_connector(owner: Option<String>, connector: Arc<dyn Connector>) -> Self {
        let inner = Arc::new(SetInner { owner, connector, resolver: Mutex::new(None), slots: Mutex::default() });
        let mut sets = SETS.lock().expect("服务器组锁中毒");
        sets.retain(|set| set.strong_count() > 0);
        sets.push(Arc::downgrade(&inner));
        Self { inner }
    }

    /// 占位符怎么换（见 [`Resolver`]）。没有设置时只认 `${env:NAME}`。
    pub fn set_resolver(&self, resolver: Resolver) {
        *self.inner.resolver.lock().expect("resolver 锁中毒") = Some(resolver);
    }

    pub fn owner(&self) -> Option<&str> {
        self.inner.owner.as_deref()
    }

    /// 换成这组配置：新增的等第一次用到时再连，删掉的断开，改了的断开重来，没变的不动。
    /// 不需要在 tokio 运行时里调用（桌面版在界面线程上改配置）。
    pub fn configure(&self, servers: BTreeMap<String, ServerConfig>) {
        let mut slots = self.inner.slots.lock().expect("服务器锁中毒");
        slots.retain(|name, slot| {
            let keep = servers.get(name).is_some_and(|config| *config == slot.config);
            if !keep {
                // 断开：丢掉连接（stdio 服务器的进程随之结束），还在跑的后台任务看到 epoch 变了就退出。
                slot.set(State::Disabled);
            }
            keep
        });
        for (name, config) in servers {
            if slots.contains_key(&name) {
                continue;
            }
            let state = match config.validate() {
                _ if !config.enabled => State::Disabled,
                Ok(()) => State::Idle,
                Err(error) => State::Stopped { error, attempts: 0 },
            };
            let slot = Slot {
                name: name.clone(),
                owner: self.inner.owner.clone(),
                config,
                state: Mutex::new(state),
                connecting: tokio::sync::Mutex::new(()),
                epoch: AtomicU64::new(0),
                logs: Arc::default(),
            };
            slots.insert(name, Arc::new(slot));
        }
    }

    fn slots(&self) -> Vec<Arc<Slot>> {
        self.inner.slots.lock().expect("服务器锁中毒").values().cloned().collect()
    }

    fn slot(&self, name: &str) -> Option<Arc<Slot>> {
        self.inner.slots.lock().expect("服务器锁中毒").get(name).cloned()
    }

    /// 可用的服务器的全部工具。还没连过的先连上（同时连，各限时 [`CONNECT_TIMEOUT`]）；失败的在后台重连，
    /// 这里不等它们。
    pub async fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let slots = self.slots();
        let connecting = slots.iter().map(|slot| self.ensure(slot.clone()));
        futures::future::join_all(connecting).await;
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut any_resources = false;
        for slot in &slots {
            if let Some(live) = slot.live() {
                tools.extend(live.tools.lock().expect("工具锁中毒").iter().cloned());
                any_resources |= live.resources;
            }
        }
        if any_resources {
            tools.push(Arc::new(ListResources));
            tools.push(Arc::new(ReadResource));
        }
        tools
    }

    /// 把还没连过的服务器都连上（桌面版启动后在后台调用，第一条消息不用等）。
    pub async fn prewarm(&self) {
        self.tools().await;
    }

    pub fn status(&self) -> Vec<ServerStatus> {
        self.slots().iter().map(|slot| slot.status()).collect()
    }

    pub fn logs(&self, name: &str) -> Vec<String> {
        self.slot(name).map(|slot| slot.logs.lines()).unwrap_or_default()
    }

    /// 手动重连（设置页的“重连”）：清掉失败次数，断开现有连接，下次要工具时重新连接。
    pub fn reconnect(&self, name: &str) -> Result<(), String> {
        let slot = self.slot(name).ok_or_else(|| format!("没有叫 {name} 的 MCP 服务器"))?;
        if !slot.config.enabled {
            return Err(format!("MCP 服务器 {name} 已停用"));
        }
        slot.logs.push("手动重连");
        slot.set(State::Idle);
        // 在运行时里时马上开始连，不等下一次要工具。
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let set = self.clone();
            runtime.spawn(async move { set.connect(slot).await });
        }
        Ok(())
    }

    /// 登录一个远程（HTTP）服务器：找授权服务器、需要时注册客户端、打开浏览器（`open`）授权，token 存进钥匙串，
    /// 然后重连。要在 tokio 运行时上调用；会等用户在浏览器里登录完（最多 5 分钟）。
    pub async fn login(&self, name: &str, open: &(dyn Fn(&str) -> Result<(), String> + Send + Sync)) -> Result<(), String> {
        let slot = self.slot(name).ok_or_else(|| format!("没有叫 {name} 的 MCP 服务器"))?;
        let Launch::Http { url, headers } = self.launch(&slot)? else {
            return Err(format!("{name} 是本机启动的服务器，不需要登录"));
        };
        crate::oauth::login(&slot.label(), &slot.config, &url, &headers, open).await?;
        slot.logs.push("已登录");
        self.reconnect(name)
    }

    /// 退出登录：删掉存着的 token，重连（服务器要登录的话会变成“需要登录”）。
    pub fn logout(&self, name: &str) -> Result<(), String> {
        let slot = self.slot(name).ok_or_else(|| format!("没有叫 {name} 的 MCP 服务器"))?;
        crate::oauth::forget_token(&slot.label());
        slot.logs.push("已退出登录");
        self.reconnect(name)
    }

    /// 可用的服务器的 prompts：(服务器名, prompt)。
    pub fn prompts(&self) -> Vec<(String, PromptInfo)> {
        let mut prompts = Vec::new();
        for slot in self.slots() {
            if let Some(live) = slot.live() {
                prompts.extend(live.prompts.lock().expect("prompts 锁中毒").iter().map(|prompt| (slot.label(), prompt.clone())));
            }
        }
        prompts
    }

    /// 没连上就连（第一次），失败的到点了在后台重连。
    async fn ensure(&self, slot: Arc<Slot>) {
        let idle = matches!(*slot.state.lock().expect("状态锁中毒"), State::Idle);
        if idle {
            self.connect(slot).await;
        }
    }

    /// 连接一个服务器：握手、取工具（有 prompts 时也取），成功后盯着它的通知和断开。
    async fn connect(&self, slot: Arc<Slot>) {
        crate::surface::remember_runtime();
        let _connecting = slot.connecting.lock().await;
        let attempts = {
            let mut state = slot.state.lock().expect("状态锁中毒");
            let attempts = match &*state {
                State::Idle => 0,
                State::Failed { attempts, .. } => *attempts,
                // 别的任务已经连上了，或者已经停用、停下。
                _ => return,
            };
            *state = State::Connecting;
            attempts
        };
        let epoch = slot.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let label = slot.label();
        let result = match tokio::time::timeout(CONNECT_TIMEOUT, self.establish(&slot)).await {
            Ok(result) => result,
            Err(_) => Err(format!("{} 秒内没有连上", CONNECT_TIMEOUT.as_secs())),
        };
        match result {
            Ok(mut live) => {
                if let Some(live) = Arc::get_mut(&mut live) {
                    live.attempts = attempts;
                }
                let mut state = slot.state.lock().expect("状态锁中毒");
                if slot.epoch.load(Ordering::SeqCst) != epoch {
                    // 连接期间被重新配置或停用了：这个连接不要了。
                    return;
                }
                slot.logs.push(format!("已连接：{} 个工具", live.tools.lock().expect("工具锁中毒").len()));
                tracing::info!(server = label, "MCP 服务器已连接");
                self.watch(&slot, &live, epoch);
                *state = State::Ready(live);
            }
            Err(_) if slot.epoch.load(Ordering::SeqCst) != epoch => {}
            Err(error) if error.starts_with(NEEDS_LOGIN) => {
                // 要登录：重连多少次都一样，停下等用户登录。
                slot.logs.push(error.clone());
                slot.set(State::NeedsLogin { error });
            }
            Err(error) => {
                tracing::warn!(server = label, error, "MCP 服务器连接失败");
                slot.logs.push(format!("连接失败：{error}"));
                self.fail(&slot, error, attempts + 1);
            }
        }
    }

    /// 记一次失败：没到上限就排一次后台重连，到了就停下。
    fn fail(&self, slot: &Arc<Slot>, error: String, attempts: u32) {
        if attempts >= MAX_ATTEMPTS {
            slot.logs.push(format!("连续失败 {attempts} 次，不再重连"));
            slot.set(State::Stopped { error, attempts });
            return;
        }
        let delay = (FIRST_RETRY * 2u32.saturating_pow(attempts - 1)).min(MAX_RETRY);
        slot.set(State::Failed { error, attempts, retry_at: Instant::now() + delay });
        let epoch = slot.epoch.load(Ordering::SeqCst);
        let (set, slot) = (self.clone(), slot.clone());
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            if slot.epoch.load(Ordering::SeqCst) == epoch {
                set.connect(slot).await;
            }
        });
    }

    async fn establish(&self, slot: &Arc<Slot>) -> Result<Arc<Live>, String> {
        let mut launch = self.launch(slot)?;
        // 登录过的远程服务器：带上 token（快过期时先刷新）。配置里自己写了 Authorization 的不动。
        if let Launch::Http { headers, .. } = &mut launch
            && !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            && let Some(token) = crate::oauth::bearer(&slot.label()).await
        {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        let login_hint = |error: McpError| match error {
            McpError::Unauthorized { .. } => {
                format!("{NEEDS_LOGIN}：在“设置 → 扩展”里这个服务器下面点“登录”，或运行 `dshr mcp login {}`", slot.label())
            }
            other => other.to_string(),
        };
        let connection = self.inner.connector.connect(&launch, &slot.logs).await.map_err(login_hint)?;
        let client = connection.client;
        client.initialize().await.map_err(login_hint)?;
        let infos = client.list_tools().await.map_err(|error| error.to_string())?;
        let prompts = if client.has_capability("prompts") { client.list_prompts().await.unwrap_or_default() } else { Vec::new() };
        let resources = client.has_capability("resources");
        let live = Arc::new(Live {
            client,
            _process: connection.process,
            tools: Mutex::new(Vec::new()),
            prompts: Mutex::new(prompts),
            resources,
            attempts: 0,
            since: Instant::now(),
        });
        *live.tools.lock().expect("工具锁中毒") = self.wrap_tools(slot, infos);
        Ok(live)
    }

    /// 配置换掉占位符后的启动方式。
    fn launch(&self, slot: &Slot) -> Result<Launch, String> {
        let resolver = self.inner.resolver.lock().expect("resolver 锁中毒").clone();
        let lookup = |kind: &str, name: &str| -> Result<String, String> {
            match (&resolver, kind) {
                (Some(resolver), _) => resolver(&slot.name, kind, name),
                (None, "env") => std::env::var(name).map_err(|_| format!("环境变量 {name} 没有设置")),
                (None, _) => Err(format!("不认识的占位符 `${{{kind}}}`")),
            }
        };
        slot.config.launch(&lookup)
    }

    fn wrap_tools(&self, slot: &Arc<Slot>, infos: Vec<ToolInfo>) -> Vec<Arc<dyn Tool>> {
        let server_label = match &slot.owner {
            Some(owner) => format!("{owner}_{}", slot.name),
            None => slot.name.clone(),
        };
        let timeout = Duration::from_millis(slot.config.timeout_ms.unwrap_or(DEFAULT_CALL_TIMEOUT_MS));
        infos
            .into_iter()
            .map(|info| {
                let description = match info.description.trim() {
                    "" => format!("Tool `{}` from the MCP server `{}`.", info.name, slot.label()),
                    description => description.to_owned(),
                };
                // 模型接口要求参数是对象 schema。
                let input_schema = match info.input_schema {
                    schema @ Value::Object(_) => schema,
                    _ => json!({"type": "object", "properties": {}}),
                };
                let tool = McpTool {
                    name: tool_name(&server_label, &info.name),
                    description,
                    input_schema,
                    remote: info.name,
                    slot: Arc::downgrade(slot),
                    label: slot.label(),
                    timeout,
                };
                Arc::new(tool) as Arc<dyn Tool>
            })
            .collect()
    }

    /// 盯着一个连接：工具 / prompts 列表变了就重新取；断开了就排重连。
    fn watch(&self, slot: &Arc<Slot>, live: &Arc<Live>, epoch: u64) {
        let mut notifications = live.client.subscribe();
        let mut closed = live.client.closed_signal();
        let attempts = if live.since.elapsed() < STABLE { live.attempts } else { 0 };
        let since = live.since;
        let (set, slot, live) = (self.clone(), Arc::downgrade(slot), Arc::downgrade(live));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = closed.changed() => {
                        if changed.is_err() || *closed.borrow() {
                            break;
                        }
                    }
                    notification = notifications.recv() => {
                        let Ok(notification) = notification else { continue };
                        let (Some(slot), Some(live)) = (slot.upgrade(), live.upgrade()) else { return };
                        match notification.method.as_str() {
                            "notifications/tools/list_changed" => {
                                if let Ok(infos) = live.client.list_tools().await {
                                    slot.logs.push(format!("工具列表变了：{} 个", infos.len()));
                                    *live.tools.lock().expect("工具锁中毒") = set.wrap_tools(&slot, infos);
                                }
                            }
                            "notifications/prompts/list_changed" => {
                                if let Ok(prompts) = live.client.list_prompts().await {
                                    *live.prompts.lock().expect("prompts 锁中毒") = prompts;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            // 断开了：还是这个连接的话，排一次重连。
            let Some(slot) = slot.upgrade() else { return };
            if slot.epoch.load(Ordering::SeqCst) != epoch {
                return;
            }
            slot.logs.push("连接断开");
            tracing::warn!(server = slot.label(), "MCP 服务器断开，稍后重连");
            let attempts = if since.elapsed() < STABLE { attempts + 1 } else { 1 };
            set.fail(&slot, "连接断开".into(), attempts);
        });
    }
}

/// `mcp__<服务器>__<工具>`，只留模型接口允许的字符；过长时截断并加一段短哈希，免得截断后撞名。
pub fn tool_name(server: &str, tool: &str) -> String {
    let clean =
        |text: &str| -> String { text.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect() };
    let name = format!("mcp__{}__{}", clean(server), clean(tool));
    if name.len() <= MAX_TOOL_NAME {
        return name;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    let suffix = format!("_{:08x}", hasher.finish() as u32);
    format!("{}{suffix}", &name[..MAX_TOOL_NAME - suffix.len()])
}

struct McpTool {
    name: String,
    description: String,
    input_schema: Value,
    /// 服务器那边的工具名。
    remote: String,
    slot: Weak<Slot>,
    label: String,
    timeout: Duration,
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn access(&self, _input: &Value, _ctx: &ToolContext) -> Access {
        Access::External
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(live) = self.slot.upgrade().and_then(|slot| slot.live()) else {
            return ToolOutput::error(format!("MCP 服务器 {} 现在没有连接（断开了或正在重连）", self.label));
        };
        // 服务器报告的进度显示在工具卡片的终端面板里。
        let output = ctx.clone();
        let progress: crate::client::ProgressFn = Arc::new(move |params: &Value| {
            let mut line = match (params["progress"].as_f64(), params["total"].as_f64()) {
                (Some(done), Some(total)) if total > 0.0 => format!("[{:.0}%]", done / total * 100.0),
                (Some(done), _) => format!("[{done}]"),
                _ => String::new(),
            };
            if let Some(message) = params["message"].as_str() {
                line.push(' ');
                line.push_str(message);
            }
            line.push('\n');
            output.emit_output(&line);
        });
        let call = live.client.call_tool_with(&self.remote, input, Some(progress));
        match tokio::time::timeout(self.timeout, call).await {
            Ok(Ok(result)) => {
                if !result.images.is_empty() {
                    let call_id = ctx.options.get::<dshr_agent::CallEnv>().map(|env| env.call_id.clone());
                    let images: Vec<Value> =
                        result.images.iter().map(|image| json!({"media_type": image.media_type, "data": image.data})).collect();
                    ctx.emit_event(dshr_core::MEDIA_IMAGES, json!({"call_id": call_id, "images": images}));
                }
                ToolOutput { content: truncate(result.text), is_error: result.is_error }
            }
            Ok(Err(error)) if error.is_disconnect() => {
                ToolOutput::error(format!("MCP 服务器 {} 断开了，正在重连；这次调用没有完成", self.label))
            }
            Ok(Err(error)) => ToolOutput::error(format!("MCP 服务器 {} 调用失败: {error}", self.label)),
            Err(_) => ToolOutput::error(format!("MCP 工具超时（{} ms）", self.timeout.as_millis())),
        }
    }
}

/// 所有服务器组里叫 `server` 的、可用的服务器（`config.toml` 里的用名字，插件自带的用 `<插件>/<名字>`）。
fn find_live(server: &str) -> Option<Arc<Live>> {
    all_slots().into_iter().find(|slot| slot.label() == server).and_then(|slot| slot.live())
}

fn all_slots() -> Vec<Arc<Slot>> {
    let sets: Vec<Arc<SetInner>> = SETS.lock().expect("服务器组锁中毒").iter().filter_map(Weak::upgrade).collect();
    sets.iter().flat_map(|set| set.slots.lock().expect("服务器锁中毒").values().cloned().collect::<Vec<_>>()).collect()
}

/// 所有可用的、提供资源的服务器：(服务器名, 客户端)。
async fn resources_of(server: Option<&str>) -> Vec<(String, Result<Vec<ResourceInfo>, McpError>)> {
    let mut found = Vec::new();
    for slot in all_slots() {
        if server.is_some_and(|server| server != slot.label()) {
            continue;
        }
        let Some(live) = slot.live() else { continue };
        if !live.resources {
            continue;
        }
        found.push((slot.label(), live.client.list_resources().await));
    }
    found
}

/// 公开给 `@` 提及用：列出所有服务器的资源。
pub async fn list_all_resources() -> Vec<(String, ResourceInfo)> {
    resources_of(None)
        .await
        .into_iter()
        .filter_map(|(server, resources)| Some((server, resources.ok()?)))
        .flat_map(|(server, resources)| resources.into_iter().map(move |resource| (server.clone(), resource)))
        .collect()
}

/// 读一个资源，把内容写成文字（二进制只留说明）。
pub async fn read_resource_text(server: &str, uri: &str) -> Result<String, String> {
    let live = find_live(server).ok_or_else(|| format!("没有叫 {server} 的已连接 MCP 服务器"))?;
    let contents = live.client.read_resource(uri).await.map_err(|error| error.to_string())?;
    let parts: Vec<String> = contents
        .into_iter()
        .map(|content| match content.text {
            Some(text) => text,
            None => format!("[{} ({}): binary content omitted]", content.uri, content.mime_type.as_deref().unwrap_or("binary")),
        })
        .collect();
    Ok(parts.join("\n\n"))
}

/// 取一个 prompt 的文字。
pub async fn get_prompt_text(server: &str, prompt: &str, arguments: &Map<String, Value>) -> Result<String, String> {
    let live = find_live(server).ok_or_else(|| format!("没有叫 {server} 的已连接 MCP 服务器"))?;
    live.client.get_prompt(prompt, arguments).await.map_err(|error| error.to_string())
}

/// 所有组里可用服务器的 prompts：(服务器名, prompt)。
pub fn all_prompts() -> Vec<(String, PromptInfo)> {
    let mut prompts = Vec::new();
    for slot in all_slots() {
        if let Some(live) = slot.live() {
            prompts.extend(live.prompts.lock().expect("prompts 锁中毒").iter().map(|prompt| (slot.label(), prompt.clone())));
        }
    }
    prompts
}

/// 给模型的工具：列出 MCP 服务器提供的资源。
struct ListResources;

#[async_trait::async_trait]
impl Tool for ListResources {
    fn name(&self) -> &str {
        "mcp_list_resources"
    }

    fn description(&self) -> &str {
        "List the resources (files, records, documents…) that connected MCP servers provide. \
Read one with mcp_read_resource."
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {
            "server": {"type": "string", "description": "Only list this server's resources."},
        }})
    }

    fn access(&self, _input: &Value, _ctx: &ToolContext) -> Access {
        Access::External
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolOutput {
        let found = resources_of(input["server"].as_str()).await;
        if found.is_empty() {
            return ToolOutput::ok("No connected MCP server provides resources.");
        }
        let mut lines = Vec::new();
        for (server, resources) in found {
            match resources {
                Ok(resources) => {
                    lines.push(format!("server {server}: {} resources", resources.len()));
                    lines.extend(resources.iter().map(|resource| {
                        let mut line = format!("- {} ({})", resource.uri, resource.name);
                        if let Some(description) = &resource.description {
                            line.push_str(&format!(": {description}"));
                        }
                        line
                    }));
                }
                Err(error) => lines.push(format!("server {server}: failed to list resources: {error}")),
            }
        }
        ToolOutput::ok(truncate(lines.join("\n")))
    }
}

/// 给模型的工具：读 MCP 服务器上的一个资源。
struct ReadResource;

#[async_trait::async_trait]
impl Tool for ReadResource {
    fn name(&self) -> &str {
        "mcp_read_resource"
    }

    fn description(&self) -> &str {
        "Read a resource from an MCP server (see mcp_list_resources for what is available)."
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {
            "server": {"type": "string", "description": "The server's name, as mcp_list_resources shows it."},
            "uri": {"type": "string", "description": "The resource URI."},
        }, "required": ["server", "uri"]})
    }

    fn access(&self, _input: &Value, _ctx: &ToolContext) -> Access {
        Access::External
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolOutput {
        let (Some(server), Some(uri)) = (input["server"].as_str(), input["uri"].as_str()) else {
            return ToolOutput::error("`server` and `uri` are required");
        };
        match read_resource_text(server, uri).await {
            Ok(text) => ToolOutput::ok(truncate(text)),
            Err(error) => ToolOutput::error(error),
        }
    }
}

#[async_trait::async_trait]
impl dshr_tools::ToolSource for ServerSet {
    async fn tools(&self) -> Vec<Arc<dyn Tool>> {
        ServerSet::tools(self).await
    }
}

/// Windows 上 `npx`、`uvx` 之类其实是 `.cmd` 脚本，直接按名字启动会找不到：
/// 按 `PATH` 和 `PATHEXT` 找到带扩展名的完整路径。其他平台原样返回。
fn resolve_program(command: &str) -> PathBuf {
    let program = PathBuf::from(command);
    if !cfg!(windows) || program.extension().is_some() || program.components().count() > 1 {
        return program;
    }
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let Some(path) = std::env::var_os("PATH") else {
        return program;
    };
    std::env::split_paths(&path)
        .flat_map(|dir| {
            extensions
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| dir.join(format!("{command}{extension}")))
                .collect::<Vec<_>>()
        })
        .find(|candidate| candidate.is_file())
        .unwrap_or(program)
}
