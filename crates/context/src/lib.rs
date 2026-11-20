//! 插件内核：服务、扩展点、钩子、事件（详见 `docs/architecture.md` 的“插件内核”）。
//!
//! 所有功能都写成 [`Plugin`]，在 [`Plugin::apply`] 里通过 [`Scope`] 注册。注册先暂存在 Scope 里，
//! `apply` 成功后才一次写入注册表，并记成这个插件的 [`Disposable`]；插件停用时按相反顺序撤销。
//!
//! 读取走 [`Snapshot`]：每次注册表变化后发布一份新的不可变快照。agent 在每个 step 开始时取一份，
//! 插件的增删从下一个 step 起生效，一个 step 内看到的状态始终一致。
//!
//! 这个 crate 不认识工具、Provider 等具体类型：扩展点的标记类型定义在拥有条目类型的 crate 里
//! （如 `dshr_tools::Tools`）。

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
pub use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

type AnyArc = Arc<dyn Any + Send + Sync>;
type Listener<E> = Arc<dyn Fn(&E) + Send + Sync>;

pub trait Plugin: Send + Sync + 'static {
    fn manifest(&self) -> Manifest;

    /// 注册本插件提供的东西。返回错误时这次注册全部作废，插件状态记为 [`Status::Failed`]。
    /// 不要在这里增删、启停插件（注册表此时上着锁）。
    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()>;

    /// 这份插件的“版本标识”：[`Context::replace`] 换上的和装着的是同一个版本时什么都不做
    /// （也不发布快照）。默认 `None`，换上的总被当作新的一份；指向同一份内部状态的插件
    /// （如克隆共享一个运行时的 JS 插件）返回共享部分的地址。
    fn version(&self) -> Option<usize> {
        None
    }
}

/// 一组一起装配的插件（形如 Bevy 的 `PluginGroup`）。
pub trait PluginGroup {
    fn add_to(self, cx: &Context) -> Result<(), ContextError>;
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub id: &'static str,
    /// 缺少任一必需服务时插件处于等待；服务出现后自动激活，服务撤销时连带停用。
    pub requires: Vec<ServiceId>,
    /// 有就用的服务。激活之后它出现或消失，插件会重新激活一次。
    pub optional: Vec<ServiceId>,
    /// 配置（`config.toml` 的 `[plugins.<id>]`）的 JSON Schema；设置页据此生成表单。没有配置项的插件是 `None`。
    pub config: Option<Arc<Value>>,
}

impl Manifest {
    pub fn new(id: &'static str) -> Self {
        Self { id, requires: Vec::new(), optional: Vec::new(), config: None }
    }

    /// 声明配置的类型，和 `apply` 里 [`Scope::config`] 取的是同一个。
    /// 字段的文档注释就是设置页里的说明，`#[schemars(range(..))]` 之类的约束也会带过去。
    pub fn config<C: JsonSchema>(mut self) -> Self {
        self.config = Some(Arc::new(schemars::schema_for!(C).to_value()));
        self
    }

    /// 直接给出配置的 JSON Schema：给配置形状不是 Rust 类型的插件用（如脚本插件，schema 写在它的清单里）。
    /// 这样的插件用 [`Scope::config_value`] 取配置。
    pub fn config_schema(mut self, schema: Value) -> Self {
        self.config = Some(Arc::new(schema));
        self
    }

    pub fn requires<S: ?Sized + 'static>(mut self) -> Self {
        self.requires.push(ServiceId::of::<S>());
        self
    }

    pub fn optional<S: ?Sized + 'static>(mut self) -> Self {
        self.optional.push(ServiceId::of::<S>());
        self
    }
}

/// 逐项比较（配置的 schema 按 JSON 值比），给 [`Context::replace`] 判断“换上的就是这一份”用。
impl PartialEq for Manifest {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.requires == other.requires && self.optional == other.optional && self.config == other.config
    }
}

/// 服务的键：服务类型（通常是 `dyn Trait`）的 `TypeId`。
#[derive(Debug, Clone, Copy)]
pub struct ServiceId {
    type_id: TypeId,
    name: &'static str,
}

impl ServiceId {
    pub fn of<S: ?Sized + 'static>() -> Self {
        Self { type_id: TypeId::of::<S>(), name: type_name::<S>() }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

impl PartialEq for ServiceId {
    fn eq(&self, other: &Self) -> bool {
        self.type_id == other.type_id
    }
}

impl Eq for ServiceId {}

/// 多实例扩展点。实现它的类型只是个标记，如 `struct Tools;`。
pub trait ExtensionPoint: 'static {
    type Item: Clone + Send + Sync + 'static;
}

/// 有序钩子链。`Handler` 通常是 `dyn Fn(..)`；怎么调用、怎么合并结果由定义钩子的 crate 决定。
pub trait Hook: 'static {
    type Handler: ?Sized + Send + Sync + 'static;
}

/// 类型化事件。
pub trait Event: Send + Sync + 'static {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Active,
    /// 等待这些必需服务出现。
    Pending(Vec<&'static str>),
    Disabled,
    /// `apply` 失败；重新启用时再试。
    Failed(String),
}

/// 一个已注册的插件：给插件管理界面看的。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginInfo {
    pub id: &'static str,
    pub status: Status,
    /// 配置的 JSON Schema（[`Manifest::config`]）。
    pub config: Option<Arc<Value>>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ContextError {
    #[error("插件 {0} 已经注册")]
    DuplicatePlugin(&'static str),
    #[error("插件 {0} 装着的就是这一份")]
    IdenticalPlugin(&'static str),
    #[error("没有插件 {0}")]
    UnknownPlugin(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Point,
    Hook,
    Event,
}

type Key = (Kind, TypeId);

/// 一个键下的条目，类型擦除后放进注册表。
trait Bucket: Send + Sync {
    fn remove(&mut self, registration: u64);
    /// 按（优先级，注册顺序）排好的 `Arc<Vec<T>>`。
    fn freeze(&self) -> AnyArc;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

struct Entries<T>(Vec<(i32, u64, T)>);

impl<T: Clone + Send + Sync + 'static> Bucket for Entries<T> {
    fn remove(&mut self, registration: u64) {
        self.0.retain(|(_, id, _)| *id != registration);
    }

    fn freeze(&self) -> AnyArc {
        let mut sorted: Vec<_> = self.0.iter().collect();
        sorted.sort_by_key(|(priority, id, _)| (*priority, *id));
        Arc::new(sorted.into_iter().map(|(_, _, item)| item.clone()).collect::<Vec<T>>())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

type Buckets = HashMap<Key, Box<dyn Bucket>>;

/// 一次注册的撤销凭据（对应 Cordis 的 effect）。
enum Disposable {
    Service(TypeId),
    Entry(Key, u64),
}

/// 暂存的一条注册：提交时写进对应的桶（参数是插件的序号）。
type StagedEntry = Box<dyn FnOnce(&mut Buckets, u64)>;

enum Staged {
    Service(ServiceId, AnyArc),
    Entry(Key, StagedEntry),
}

/// 插件注册时拿到的句柄。
pub struct Scope {
    id: &'static str,
    /// 激活时已有的服务，加上本插件刚提供的。
    services: HashMap<TypeId, AnyArc>,
    staged: Vec<Staged>,
    config: Option<Value>,
    runtime: Option<Handle>,
    cancel: CancellationToken,
}

impl Scope {
    /// 提供单例服务，按 `S` 的 `TypeId` 存储。同一个服务只能有一个提供者。
    pub fn provide<S: ?Sized + Send + Sync + 'static>(&mut self, service: Arc<S>) {
        let id = ServiceId::of::<S>();
        let value: AnyArc = Arc::new(service);
        self.services.insert(id.type_id, value.clone());
        self.staged.push(Staged::Service(id, value));
    }

    /// 取服务。写在 `requires` 里的服务一定取得到。
    pub fn get<S: ?Sized + Send + Sync + 'static>(&self) -> Option<Arc<S>> {
        downcast_service(self.services.get(&TypeId::of::<S>()))
    }

    /// 向扩展点贡献一项。
    pub fn contribute<P: ExtensionPoint>(&mut self, item: P::Item) {
        self.stage((Kind::Point, TypeId::of::<P>()), 0, item);
    }

    /// 挂一个钩子；`priority` 小的先执行，相同时按注册顺序。
    pub fn hook<H: Hook>(&mut self, priority: i32, handler: Arc<H::Handler>) {
        self.stage((Kind::Hook, TypeId::of::<H>()), priority, handler);
    }

    /// 订阅事件。
    pub fn on<E: Event>(&mut self, listener: impl Fn(&E) + Send + Sync + 'static) {
        let listener: Listener<E> = Arc::new(listener);
        self.stage((Kind::Event, TypeId::of::<E>()), 0, listener);
    }

    /// 后台任务，插件停用时取消。需要 tokio 运行时：[`Context::with_runtime`] 给的，或当前线程所在的。
    pub fn spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        let Some(runtime) = self.runtime.clone().or_else(|| Handle::try_current().ok()) else {
            tracing::error!(plugin = self.id, "没有 tokio 运行时，后台任务没有启动");
            return;
        };
        let cancel = self.cancel.clone();
        runtime.spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = task => {}
            }
        });
    }

    /// 本插件的配置；没有配置或格式不对时用默认值。
    /// 配置类型要能给出 JSON Schema，在 [`Manifest::config`] 里声明后设置页就能显示它。
    pub fn config<C: DeserializeOwned + Default + JsonSchema>(&self) -> C {
        let Some(value) = &self.config else {
            return C::default();
        };
        serde::Deserialize::deserialize(value).unwrap_or_else(|error| {
            tracing::warn!(plugin = self.id, %error, "插件配置无效，使用默认值");
            C::default()
        })
    }

    /// 本插件的配置原样（`[plugins.<id>]` 转成的 JSON）；没有配置时是 `None`。
    pub fn config_value(&self) -> Option<&Value> {
        self.config.as_ref()
    }

    fn stage<T: Clone + Send + Sync + 'static>(&mut self, key: Key, priority: i32, item: T) {
        self.staged.push(Staged::Entry(
            key,
            Box::new(move |buckets, registration| {
                buckets
                    .entry(key)
                    .or_insert_with(|| Box::new(Entries::<T>(Vec::new())))
                    .as_any_mut()
                    .downcast_mut::<Entries<T>>()
                    .expect("同一个键下的条目类型相同")
                    .0
                    .push((priority, registration, item));
            }),
        ));
    }
}

fn downcast_service<S: ?Sized + Send + Sync + 'static>(value: Option<&AnyArc>) -> Option<Arc<S>> {
    value?.downcast_ref::<Arc<S>>().cloned()
}

/// 按类型存取的一组选项：宿主把一轮对话、一次工具调用的设置放进来，插件按自己认识的类型取。
/// 克隆只增加引用计数。
#[derive(Clone, Default)]
pub struct Options(HashMap<TypeId, AnyArc>);

impl Options {
    pub fn new() -> Self {
        Self::default()
    }

    /// 放入一项；同一类型已有时替换。
    pub fn insert<T: Send + Sync + 'static>(&mut self, option: T) {
        self.0.insert(TypeId::of::<T>(), Arc::new(option));
    }

    pub fn with<T: Send + Sync + 'static>(mut self, option: T) -> Self {
        self.insert(option);
        self
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.0.get(&TypeId::of::<T>())?.downcast_ref()
    }
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Options({})", self.0.len())
    }
}

/// 注册表在某一刻的不可变视图。
#[derive(Default)]
pub struct Snapshot {
    services: HashMap<TypeId, AnyArc>,
    lists: HashMap<Key, AnyArc>,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Snapshot")
    }
}

impl Snapshot {
    pub fn service<S: ?Sized + Send + Sync + 'static>(&self) -> Option<Arc<S>> {
        downcast_service(self.services.get(&TypeId::of::<S>()))
    }

    /// 扩展点下的所有条目，按注册顺序。
    pub fn items<P: ExtensionPoint>(&self) -> &[P::Item] {
        self.list(Kind::Point, TypeId::of::<P>())
    }

    /// 钩子链，按执行顺序。
    pub fn hooks<H: Hook>(&self) -> &[Arc<H::Handler>] {
        self.list(Kind::Hook, TypeId::of::<H>())
    }

    /// 依次调用订阅了 `E` 的监听器。
    pub fn emit<E: Event>(&self, event: &E) {
        for listener in self.list::<Listener<E>>(Kind::Event, TypeId::of::<E>()) {
            listener(event);
        }
    }

    fn list<T: 'static>(&self, kind: Kind, type_id: TypeId) -> &[T] {
        self.lists.get(&(kind, type_id)).and_then(|list| list.downcast_ref::<Vec<T>>()).map_or(&[], Vec::as_slice)
    }
}

struct Service {
    owner: &'static str,
    value: AnyArc,
}

/// 改配置里的 `enabled` 开关：启用就是把 `enabled = false` 去掉（其余键留着）。
fn set_enabled_flag(config: &mut HashMap<String, Value>, id: &str, enabled: bool) {
    let entry = config.entry(id.to_owned()).or_insert_with(|| Value::Object(Default::default()));
    if let Value::Object(object) = entry {
        if enabled {
            object.remove("enabled");
        } else {
            object.insert("enabled".to_owned(), Value::Bool(false));
        }
    }
}

struct Slot {
    plugin: Arc<dyn Plugin>,
    manifest: Manifest,
    status: Status,
    effects: Vec<Disposable>,
    cancel: CancellationToken,
    /// 激活时各个可选服务在不在；之后有变化就重新激活。
    optional_seen: Vec<bool>,
}

#[derive(Default)]
struct Registry {
    next_registration: u64,
    services: HashMap<TypeId, Service>,
    buckets: Buckets,
    slots: Vec<Slot>,
    config: HashMap<String, Value>,
}

impl Registry {
    fn index(&self, id: &str) -> Result<usize, ContextError> {
        self.slots.iter().position(|slot| slot.manifest.id == id).ok_or_else(|| ContextError::UnknownPlugin(id.to_owned()))
    }

    fn missing(&self, index: usize) -> Vec<&'static str> {
        let requires = &self.slots[index].manifest.requires;
        requires.iter().filter(|id| !self.services.contains_key(&id.type_id)).map(ServiceId::name).collect()
    }

    fn optional_present(&self, index: usize) -> Vec<bool> {
        let optional = &self.slots[index].manifest.optional;
        optional.iter().map(|id| self.services.contains_key(&id.type_id)).collect()
    }

    /// 把 `apply` 暂存的注册写进注册表。服务冲突时什么都不写。
    fn commit(&mut self, owner: &'static str, staged: Vec<Staged>) -> anyhow::Result<Vec<Disposable>> {
        let mut provided = Vec::new();
        for entry in &staged {
            let Staged::Service(id, _) = entry else { continue };
            if let Some(existing) = self.services.get(&id.type_id) {
                anyhow::bail!("服务 {} 已由插件 {} 提供", id.name, existing.owner);
            }
            if provided.contains(id) {
                anyhow::bail!("服务 {} 提供了两次", id.name);
            }
            provided.push(*id);
        }
        let mut effects = Vec::with_capacity(staged.len());
        for entry in staged {
            match entry {
                Staged::Service(id, value) => {
                    self.services.insert(id.type_id, Service { owner, value });
                    effects.push(Disposable::Service(id.type_id));
                }
                Staged::Entry(key, insert) => {
                    self.next_registration += 1;
                    insert(&mut self.buckets, self.next_registration);
                    effects.push(Disposable::Entry(key, self.next_registration));
                }
            }
        }
        Ok(effects)
    }

    fn activate(&mut self, index: usize, runtime: Option<Handle>) {
        let id = self.slots[index].manifest.id;
        let plugin = self.slots[index].plugin.clone();
        let cancel = CancellationToken::new();
        let mut scope = Scope {
            id,
            services: self.services.iter().map(|(key, service)| (*key, service.value.clone())).collect(),
            staged: Vec::new(),
            config: self.config.get(id).cloned(),
            runtime,
            cancel: cancel.clone(),
        };
        match plugin.apply(&mut scope).and_then(|()| self.commit(id, scope.staged)) {
            Ok(effects) => {
                let optional_seen = self.optional_present(index);
                let slot = &mut self.slots[index];
                slot.effects = effects;
                slot.cancel = cancel;
                slot.optional_seen = optional_seen;
                slot.status = Status::Active;
            }
            Err(error) => {
                cancel.cancel();
                tracing::error!(plugin = id, error = format!("{error:#}"), "插件激活失败");
                self.slots[index].status = Status::Failed(format!("{error:#}"));
            }
        }
    }

    /// 撤销插件的全部注册（与注册顺序相反），取消它的后台任务。
    fn deactivate(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.cancel.cancel();
        for effect in std::mem::take(&mut slot.effects).into_iter().rev() {
            match effect {
                Disposable::Service(type_id) => {
                    self.services.remove(&type_id);
                }
                Disposable::Entry(key, registration) => {
                    if let Some(bucket) = self.buckets.get_mut(&key) {
                        bucket.remove(registration);
                    }
                }
            }
        }
    }

    /// 扫一遍所有插件：依赖没了的停用，依赖齐了的激活。返回有没有变化。
    fn step(&mut self, runtime: &Option<Handle>) -> bool {
        let mut changed = false;
        for index in 0..self.slots.len() {
            if self.slots[index].status != Status::Active {
                continue;
            }
            let missing = self.missing(index);
            if !missing.is_empty() || self.optional_present(index) != self.slots[index].optional_seen {
                self.deactivate(index);
                self.slots[index].status = Status::Pending(missing);
                changed = true;
            }
        }
        for index in 0..self.slots.len() {
            if !matches!(self.slots[index].status, Status::Pending(_)) {
                continue;
            }
            let missing = self.missing(index);
            if missing.is_empty() {
                self.activate(index, runtime.clone());
                changed = true;
            } else {
                self.slots[index].status = Status::Pending(missing);
            }
        }
        changed
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            services: self.services.iter().map(|(key, service)| (*key, service.value.clone())).collect(),
            lists: self.buckets.iter().map(|(key, bucket)| (*key, bucket.freeze())).collect(),
        }
    }
}

struct Inner {
    registry: Mutex<Registry>,
    snapshot: ArcSwap<Snapshot>,
    runtime: Option<Handle>,
    /// 每发布一次新快照加一（[`Context::subscribe`]）。
    changes: tokio::sync::watch::Sender<u64>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let registry = self.registry.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner());
        for slot in &registry.slots {
            slot.cancel.cancel();
        }
    }
}

/// 插件容器。克隆只增加引用计数，指向同一个注册表。
#[derive(Clone)]
pub struct Context {
    inner: Arc<Inner>,
}

impl Default for Context {
    fn default() -> Self {
        Self::build(None)
    }
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// 插件的后台任务（[`Scope::spawn`]）跑在这个运行时上。
    pub fn with_runtime(runtime: Handle) -> Self {
        Self::build(Some(runtime))
    }

    fn build(runtime: Option<Handle>) -> Self {
        Self {
            inner: Arc::new(Inner {
                registry: Mutex::default(),
                snapshot: ArcSwap::default(),
                runtime,
                changes: tokio::sync::watch::channel(0).0,
            }),
        }
    }

    /// 注册并尝试激活插件。`apply` 失败不算这里的错误，记在 [`Context::status`] 里。
    pub fn add(&self, plugin: impl Plugin) -> Result<(), ContextError> {
        let manifest = plugin.manifest();
        self.change(|registry| {
            if registry.slots.iter().any(|slot| slot.manifest.id == manifest.id) {
                return Err(ContextError::DuplicatePlugin(manifest.id));
            }
            // 配置里写了 `enabled = false` 的插件注册后保持停用。
            let enabled = registry.config.get(manifest.id).and_then(|config| config.get("enabled")) != Some(&Value::Bool(false));
            registry.slots.push(Slot {
                plugin: Arc::new(plugin),
                manifest,
                status: if enabled { Status::Pending(Vec::new()) } else { Status::Disabled },
                effects: Vec::new(),
                cancel: CancellationToken::new(),
                optional_seen: Vec::new(),
            });
            Ok(())
        })
    }

    /// 把同一个 id 的插件原地换成新的一份（热重载）：撤销旧的注册、激活新的，只发布一次快照，
    /// 读的一方看不到“插件不在”的中间状态。位置不变；是否停用按配置里当前的 `enabled` 决定，
    /// 所以停用状态下换文件不换状态、改回启用时重新激活。没有这个 id 时等同于 [`Context::add`]。
    /// 旧的那份在最后一个持有它的快照、最后一次进行中的调用结束后才释放。
    /// 换上的就是现在装着的那一份时返回 [`ContextError::IdenticalPlugin`]，什么都不做（不发布快照）；
    /// 但配置里的 `enabled` 变了照样按它启停，因为开关不走代码版本。
    pub fn replace(&self, plugin: impl Plugin) -> Result<(), ContextError> {
        let manifest = plugin.manifest();
        let plugin: Arc<dyn Plugin> = Arc::new(plugin);
        self.change(|registry| {
            match registry.index(manifest.id) {
                Ok(index) => {
                    // 配置里 `enabled = false` 的保持停用（热重载时被停用的插件换了文件不换状态）；
                    // 由停用换成启用配置的就重新激活——开关改动因此即时生效。
                    let enabled = registry.config.get(manifest.id).and_then(|config| config.get("enabled")) != Some(&Value::Bool(false));
                    let identical = {
                        let slot = &registry.slots[index];
                        slot.manifest == manifest
                            && plugin.version().is_some()
                            && plugin.version() == slot.plugin.version()
                            && (slot.status != Status::Disabled) == enabled
                    };
                    if identical {
                        return Err(ContextError::IdenticalPlugin(manifest.id));
                    }
                    registry.deactivate(index);
                    let slot = &mut registry.slots[index];
                    slot.plugin = plugin.clone();
                    slot.manifest = manifest;
                    slot.status = if enabled { Status::Pending(Vec::new()) } else { Status::Disabled };
                }
                Err(_) => {
                    let enabled = registry.config.get(manifest.id).and_then(|config| config.get("enabled")) != Some(&Value::Bool(false));
                    registry.slots.push(Slot {
                        plugin,
                        manifest,
                        status: if enabled { Status::Pending(Vec::new()) } else { Status::Disabled },
                        effects: Vec::new(),
                        cancel: CancellationToken::new(),
                        optional_seen: Vec::new(),
                    });
                }
            }
            Ok(())
        })
    }

    /// 插件的配置（[`Context::set_config`] / [`Context::configure`] 给的），原样。
    pub fn config(&self, id: &str) -> Option<Value> {
        self.lock().config.get(id).cloned()
    }

    pub fn add_group(&self, group: impl PluginGroup) -> Result<(), ContextError> {
        group.add_to(self)
    }

    /// 停用插件：撤销它的全部注册，依赖它提供的服务的插件连带回到等待；配置里记下 `enabled = false`。
    pub fn disable(&self, id: &str) -> Result<(), ContextError> {
        self.change(|registry| {
            let index = registry.index(id)?;
            registry.deactivate(index);
            registry.slots[index].status = Status::Disabled;
            set_enabled_flag(&mut registry.config, id, false);
            Ok(())
        })
    }

    /// 启用已停用或激活失败的插件；配置里的 `enabled = false` 一并去掉。
    pub fn enable(&self, id: &str) -> Result<(), ContextError> {
        self.change(|registry| {
            let index = registry.index(id)?;
            let slot = &mut registry.slots[index];
            if matches!(slot.status, Status::Disabled | Status::Failed(_)) {
                slot.status = Status::Pending(Vec::new());
            }
            set_enabled_flag(&mut registry.config, id, true);
            Ok(())
        })
    }

    /// 只改配置里的 `enabled` 标记，不激活、不停用、不发布快照。
    ///
    /// 用于“先在一边把新版本预加载好、再用 [`Context::replace`] 一次换上”的启用流程：
    /// 直接 [`Context::enable`] 会立刻激活旧的那一份（可能要执行脚本），随后的 `replace` 再激活一次。
    pub fn mark_enabled(&self, id: &str, enabled: bool) {
        set_enabled_flag(&mut self.lock().config, id, enabled);
    }

    /// 停用并移除插件。
    pub fn remove(&self, id: &str) -> Result<(), ContextError> {
        self.change(|registry| {
            let index = registry.index(id)?;
            registry.deactivate(index);
            registry.slots.remove(index);
            Ok(())
        })
    }

    /// 设置插件的配置（`config.toml` 的 `[plugins.<id>]`），下次激活时生效。
    /// 其中的 `enabled = false` 让之后注册的这个插件保持停用。
    pub fn set_config(&self, id: &str, config: Value) {
        self.lock().config.insert(id.to_owned(), config);
    }

    /// 一次设置多个插件的配置，在注册插件之前调用。
    pub fn configure(&self, configs: impl IntoIterator<Item = (String, Value)>) {
        self.lock().config.extend(configs);
    }

    pub fn status(&self, id: &str) -> Option<Status> {
        self.lock().slots.iter().find(|slot| slot.manifest.id == id).map(|slot| slot.status.clone())
    }

    /// 所有插件及其状态，按注册顺序。
    pub fn plugins(&self) -> Vec<(&'static str, Status)> {
        self.lock().slots.iter().map(|slot| (slot.manifest.id, slot.status.clone())).collect()
    }

    /// 所有插件的状态和配置 schema，按注册顺序。
    pub fn describe(&self) -> Vec<PluginInfo> {
        self.lock()
            .slots
            .iter()
            .map(|slot| PluginInfo { id: slot.manifest.id, status: slot.status.clone(), config: slot.manifest.config.clone() })
            .collect()
    }

    /// 每次发布新快照（插件装上、卸下、启停、热重载）时都会收到通知。不依赖 tokio 运行时，
    /// 界面线程上也能 `changed().await`：比隔一会儿比一次快照省事，没有变化时不会被叫醒。
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }

    /// 当前快照。拿到之后注册表再变化也不影响它。
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner.snapshot.load_full()
    }

    pub fn emit<E: Event>(&self, event: &E) {
        self.snapshot().emit(event);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.inner.registry.lock().expect("插件注册表锁中毒")
    }

    /// 改动注册表，然后把依赖关系重新理顺并发布新快照。
    fn change(&self, edit: impl FnOnce(&mut Registry) -> Result<(), ContextError>) -> Result<(), ContextError> {
        let mut registry = self.lock();
        edit(&mut registry)?;
        // 每次激活或停用都可能让别的插件的依赖成立或失效，反复扫到稳定为止。
        let limit = registry.slots.len() * 4 + 8;
        let settled = (0..limit).any(|_| !registry.step(&self.inner.runtime));
        if !settled {
            tracing::error!("插件依赖无法稳定：可选依赖成环了");
        }
        self.inner.snapshot.store(Arc::new(registry.snapshot()));
        drop(registry);
        self.inner.changes.send_modify(|generation| *generation += 1);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
