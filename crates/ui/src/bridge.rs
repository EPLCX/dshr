//! 桌面版交给插件宿主的服务（`dshr_plugins::app`）：插件替用户发消息、停止对话、写输入框、发通知、
//! 装插件前确认、打开浏览器、页面动作里调用默认模型。
//!
//! 插件在自己的线程上调用这些服务，而界面只能在主线程上动：请求先排进 [`UiBridge`] 的队列，同时唤醒工作区窗口，
//! 由它在主线程上取出来处理（以前是每 250 毫秒看一次，没有插件时也一直在唤醒主线程）。只读的东西（打开着的会话、
//! 输入框里的字）由窗口在每次重画时写一份快照。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use dshr_plugins::app::{ComposerAccess, InstallAnswer, InstallRequest, Notification, OpenSession, SendRequest, SessionControl};

use crate::backend::Backend;

/// 装插件的确认最多等这么久，过了算不同意。
const INSTALL_WAIT: Duration = Duration::from_secs(600);

pub enum BridgeRequest {
    Send(SendRequest),
    Stop(Option<String>),
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

struct Handle(Arc<UiBridge>);

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
        services.session = Some(Arc::new(Handle(bridge.clone())));
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
