//! 桌面版交给插件宿主的服务（`dshr_plugins::app`）：插件替用户发消息、停止对话、写输入框、发通知、
//! 装插件前确认、打开浏览器、页面动作里调用默认模型。
//!
//! 插件在自己的线程上调用这些服务，而界面只能在主线程上动：请求先排进 [`UiBridge`] 的队列，同时唤醒工作区窗口，
//! 由它在主线程上取出来处理（以前是每 250 毫秒看一次，没有插件时也一直在唤醒主线程）。只读的东西（打开着的会话、
//! 输入框里的字）由窗口在每次重画时写一份快照。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use dshr_plugins::app::{
    ComposerAccess, InstallAnswer, InstallRequest, ModelInfo, Notification, OpenSession, OptionChange, SendRequest, SessionControl,
};

use crate::backend::Backend;

/// 装插件的确认最多等这么久，过了算不同意。
const INSTALL_WAIT: Duration = Duration::from_secs(600);

pub enum BridgeRequest {
    Send(SendRequest),
    Stop(Option<String>),
    /// 改会话（`None` 是当前会话）的模型或预设。已经在 [`SessionHandle`] 里检查过。
    SetOptions(Option<String>, ResolvedChange),
    SetComposer(String),
    Install(Box<InstallRequest>, std::sync::mpsc::SyncSender<InstallAnswer>),
    OpenUrl(String),
}

#[derive(Default)]
pub struct UiBridge {
    requests: Mutex<VecDeque<BridgeRequest>>,
    sessions: Mutex<Vec<OpenSession>>,
    composer: Mutex<Option<String>>,
    /// 来了请求时叫醒工作区窗口（见 [`UiBridge::subscribe`]）。
    waker: Mutex<Option<futures::channel::mpsc::UnboundedSender<()>>>,
}

impl UiBridge {
    pub fn global() -> Arc<Self> {
        static BRIDGE: OnceLock<Arc<UiBridge>> = OnceLock::new();
        BRIDGE.get_or_init(Arc::default).clone()
    }

    fn push(&self, request: BridgeRequest) {
        self.requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push_back(request);
        if let Some(waker) = &*self.waker.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            let _ = waker.unbounded_send(());
        }
    }

    /// 工作区窗口订阅：每来一个请求收到一次通知。后订阅的顶替先订阅的（只有一个工作区窗口）。
    pub fn subscribe(&self) -> futures::channel::mpsc::UnboundedReceiver<()> {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        *self.waker.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sender);
        receiver
    }

    /// 取出排着的请求（工作区窗口在主线程上处理）。
    pub fn take(&self) -> Vec<BridgeRequest> {
        self.requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).drain(..).collect()
    }

    /// 窗口写下现在打开着的会话和输入框里的字，插件读的是这份快照。
    pub fn set_snapshot(&self, sessions: Vec<OpenSession>, composer: Option<String>) {
        let mut current = self.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *current != sessions {
            *current = sessions;
        }
        drop(current);
        let mut current = self.composer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *current != composer {
            *current = composer;
        }
    }
}

/// 检查过的选项改动：模型已经解析成 `provider/model`，预设已经找到。
pub enum ResolvedChange {
    Model { model: Option<dshr_config::ModelRef>, effort: Option<Option<dshr_config::ReasoningEffort>> },
    Preset(dshr_agent::Preset),
    PluginPreset(Arc<dshr_agent::PresetDef>),
}

struct Handle(Arc<UiBridge>);

/// 会话控制：除了转给界面，还要按配置检查插件要换的模型、预设（检查是同步的，插件立刻知道对不对）。
struct SessionHandle {
    bridge: Arc<UiBridge>,
    backend: Arc<Backend>,
}

impl SessionHandle {
    /// 要改的会话在快照里的样子。当前会话还没有 id（新会话还没发过消息）时是 `Ok(None)`。
    fn target(&self, session: &Option<String>) -> Result<Option<OpenSession>, String> {
        let sessions = self.bridge.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        match session {
            Some(id) => sessions.into_iter().find(|open| open.id == *id).map(Some).ok_or_else(|| format!("the session {id} is not open")),
            None => Ok(sessions.into_iter().find(|open| open.current)),
        }
    }

    fn resolve(&self, session: &Option<String>, change: OptionChange) -> Result<ResolvedChange, String> {
        match change {
            OptionChange::Model { model, effort } => {
                let model = match model {
                    None => None,
                    Some(spec) => {
                        let config = self.backend.config();
                        let available = |reference: &dshr_config::ModelRef| {
                            config
                                .provider(&reference.provider)
                                .is_some_and(|provider| provider.models.iter().any(|model| model.id == reference.model))
                        };
                        // 先按 `provider/model` 找；找不到时当作模型名，沿用会话现在的提供商（模型名里也可能有 `/`）。
                        let full = dshr_config::ModelRef::parse(&spec).filter(|reference| available(reference));
                        let reference = match full {
                            Some(reference) => reference,
                            None => {
                                let current = match self.target(session)? {
                                    Some(open) => open.model.split_once('/').map(|(provider, _)| provider.to_owned()),
                                    None => config.default_model().map(|model| model.provider),
                                };
                                let provider = current.unwrap_or_default();
                                Some(dshr_config::ModelRef::new(&provider, &spec))
                                    .filter(|reference| available(reference))
                                    .ok_or_else(|| format!("the model {spec} is not configured (see host.session.models())"))?
                            }
                        };
                        Some(reference)
                    }
                };
                let effort = effort.map(|effort| {
                    effort.and_then(|effort| dshr_config::ReasoningEffort::ALL.into_iter().find(|level| level.as_str() == effort))
                });
                Ok(ResolvedChange::Model { model, effort })
            }
            OptionChange::Preset(id) => {
                if let Some((plugin, preset)) = id.split_once('/') {
                    return self
                        .backend
                        .plugin_presets()
                        .into_iter()
                        .find(|def| def.plugin == plugin && def.id == preset)
                        .map(ResolvedChange::PluginPreset)
                        .ok_or_else(|| format!("no plugin provides the preset {id}"));
                }
                match crate::conversation::preset_from_id(&id) {
                    // 创造模式会让模型直接写插件：只能由用户自己切换。
                    Some(dshr_agent::Preset::Create) => Err("plugins cannot switch a session to creation mode".into()),
                    Some(preset) => Ok(ResolvedChange::Preset(preset)),
                    None => Err(format!("unknown preset {id}: use standard, plan, review or <plugin>/<preset>")),
                }
            }
        }
    }
}

impl SessionControl for SessionHandle {
    fn send(&self, request: SendRequest) -> Result<String, String> {
        Handle(self.bridge.clone()).send(request)
    }

    fn stop(&self, session: Option<String>) -> Result<(), String> {
        Handle(self.bridge.clone()).stop(session)
    }

    fn list(&self) -> Vec<OpenSession> {
        Handle(self.bridge.clone()).list()
    }

    fn set_options(&self, session: Option<String>, change: OptionChange) -> Result<(), String> {
        self.target(&session)?;
        let resolved = self.resolve(&session, change)?;
        self.bridge.push(BridgeRequest::SetOptions(session, resolved));
        Ok(())
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.backend
            .model_choices()
            .into_iter()
            .map(|choice| ModelInfo {
                model: format!("{}/{}", choice.reference.provider, choice.reference.model),
                provider: choice.reference.provider,
                name: choice.label,
                provider_name: choice.provider_name,
            })
            .collect()
    }
}

impl SessionControl for Handle {
    fn send(&self, request: SendRequest) -> Result<String, String> {
        let sessions = self.0.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        let id = match &request.target {
            dshr_plugins::app::SessionTarget::Current => {
                sessions.iter().find(|session| session.current).map(|session| session.id.clone()).unwrap_or_default()
            }
            dshr_plugins::app::SessionTarget::Id(id) => {
                if !sessions.iter().any(|session| session.id == *id) {
                    return Err(format!("the session {id} is not open"));
                }
                id.clone()
            }
            dshr_plugins::app::SessionTarget::New => String::new(),
        };
        self.0.push(BridgeRequest::Send(request));
        Ok(id)
    }

    fn stop(&self, session: Option<String>) -> Result<(), String> {
        self.0.push(BridgeRequest::Stop(session));
        Ok(())
    }

    fn list(&self) -> Vec<OpenSession> {
        self.0.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

impl ComposerAccess for Handle {
    fn set_text(&self, text: &str) -> Result<(), String> {
        self.0.push(BridgeRequest::SetComposer(text.to_owned()));
        Ok(())
    }

    fn text(&self) -> Option<String> {
        self.0.composer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

/// 系统通知：在另一条线程上发（有的平台会等通知中心回应）。
fn notify(notification: &Notification) {
    let (title, body, plugin) = (notification.title.clone(), notification.body.clone(), notification.plugin.clone());
    std::thread::spawn(move || {
        let shown = notify_rust::Notification::new().appname(crate::APP_NAME).summary(&title).body(&body).show();
        if let Err(error) = shown {
            tracing::warn!(plugin, %error, "系统通知发不出去");
        }
    });
}

/// 启动时把桌面版的服务交给插件宿主。
pub fn install(backend: Arc<Backend>) {
    let bridge = UiBridge::global();
    dshr_plugins::app::set_app_services(|services| {
        services.session = Some(Arc::new(SessionHandle { bridge: bridge.clone(), backend: backend.clone() }));
        services.composer = Some(Arc::new(Handle(bridge.clone())));
        services.notifier = Some(Arc::new(notify));
        let prompt_bridge = bridge.clone();
        services.install_prompt = Some(Arc::new(move |request: &InstallRequest| {
            let (answer, answered) = std::sync::mpsc::sync_channel(1);
            prompt_bridge.push(BridgeRequest::Install(Box::new(request.clone()), answer));
            answered.recv_timeout(INSTALL_WAIT).unwrap_or(InstallAnswer::Deny)
        }));
        let url_bridge = bridge.clone();
        services.open_url = Some(Arc::new(move |url: &str| {
            url_bridge.push(BridgeRequest::OpenUrl(url.to_owned()));
            Ok(())
        }));
        let reload = backend.clone();
        services.config_changed = Some(Arc::new(move || reload.reload_config()));
        let resolver = backend.clone();
        services.models = Some(Arc::new(move |spec: &str| resolver.llm_for(&dshr_config::ModelRef::parse(spec)?)));
        let backend = backend.clone();
        services.llm = Some(Arc::new(move || backend.default_llm()));
    });
}
