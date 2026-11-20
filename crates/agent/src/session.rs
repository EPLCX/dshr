use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};

use dshr_core::{CoreError, EventBody, LogEntry, SessionEvent, SessionLog, read_entries};
use tokio::sync::mpsc::UnboundedSender;

use crate::LiveEvent;

// The registry and cloned writer handles don't keep a file open without a Session.
// An inactive title write opens and closes its own log while holding this same lock.
type SharedWriter = Arc<Mutex<WriterState>>;
static WRITERS: LazyLock<Mutex<HashMap<PathBuf, Weak<Mutex<WriterState>>>>> = LazyLock::new(Default::default);

#[derive(Default)]
struct WriterState {
    active: Option<ActiveLog>,
}

/// 一份日志在本进程里的写入端，同一份日志的所有 [`Session`] 共用。
///
/// 别人写进日志、某个 `Session` 自己没写的事件（后台写的标题，同一会话的另一个 `Session` 写的事件）
/// 放在 `shared` 里，各 `Session` 凭自己的游标（绝对序号）补进内存，保证每个 `Session` 的事件列表、
/// 轮次编号都和日志一致。只有一个 `Session` 时它自己写的事件不进 `shared`；所有 `Session` 都补齐后清空。
struct ActiveLog {
    log: SessionLog,
    sessions: usize,
    has_title: bool,
    shared: Vec<SessionEvent>,
    /// `shared[0]` 的绝对序号。
    base: usize,
    /// 有一个 `Session` 正在跑一轮（[`Session::claim_turn`]）：同一份日志上不能同时跑两轮，事件会交错。
    turn_running: bool,
}

impl ActiveLog {
    fn new(log: SessionLog, has_title: bool) -> Self {
        Self { log, sessions: 1, has_title, shared: Vec::new(), base: 0, turn_running: false }
    }

    /// 已经共享过的事件之后的位置：新打开的 `Session` 从日志文件读到了此前的一切，从这里开始跟。
    fn end(&self) -> usize {
        self.base + self.shared.len()
    }

    /// 不经过 `Session` 写的事件（后台标题）：所有 `Session` 都要补上。
    fn append_detached(&mut self, body: EventBody) -> Result<SessionEvent, CoreError> {
        let event = self.log.append(body)?;
        if is_title(&event) {
            self.has_title = true;
        }
        self.shared.push(event.clone());
        Ok(event)
    }

    /// 从 `cursor` 起还没补的事件。
    fn unseen(&self, cursor: usize) -> &[SessionEvent] {
        &self.shared[cursor.saturating_sub(self.base).min(self.shared.len())..]
    }

    /// 只剩一个 `Session` 且它已经补齐时，丢掉共享的事件（不再有人需要）。
    fn trim(&mut self, cursor: usize) {
        if self.sessions <= 1 && cursor >= self.end() {
            self.base = self.end();
            self.shared.clear();
        }
    }
}

/// [`Session::claim_turn`] 的凭据：丢弃时放开。
pub(crate) struct TurnClaim(SharedWriter);

impl Drop for TurnClaim {
    fn drop(&mut self) {
        if let Some(active) = lock(&self.0).active.as_mut() {
            active.turn_running = false;
        }
    }
}

fn is_title(event: &SessionEvent) -> bool {
    matches!(event.body, EventBody::SessionTitle { .. })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn writer_for_path(path: &Path) -> io::Result<SharedWriter> {
    // Resolve aliases for existing files, and use the canonical parent when reserving a new log.
    let key = match path.canonicalize() {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let name = path.file_name().ok_or(error)?;
            log_parent(path).canonicalize()?.join(name)
        }
        Err(error) => return Err(error),
    };
    let mut registry = lock(&WRITERS);
    if let Some(writer) = registry.get(&key).and_then(Weak::upgrade) {
        return Ok(writer);
    }
    registry.retain(|_, writer| writer.strong_count() != 0);
    let writer = SharedWriter::default();
    registry.insert(key, Arc::downgrade(&writer));
    Ok(writer)
}

fn log_parent(path: &Path) -> &Path {
    path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."))
}

/// Persists a generated title only if no title (including a manual rename) exists.
/// Call this once the title is ready, passing just the path across the async wait.
pub(crate) fn write_title(path: &Path, title: String) -> Result<bool, CoreError> {
    persist_title(path, title, true)
}

pub(crate) fn rename_title(path: &Path, title: String) -> Result<(), CoreError> {
    persist_title(path, title, false).map(|_| ())
}

fn persist_title(path: &Path, title: String, only_if_untitled: bool) -> Result<bool, CoreError> {
    let writer = writer_for_path(path)?;
    let mut state = lock(&writer);
    let body = EventBody::SessionTitle { title };
    if let Some(active) = &mut state.active {
        if only_if_untitled && active.has_title {
            return Ok(false);
        }
        active.append_detached(body)?;
    } else if only_if_untitled {
        let (mut log, entries) = SessionLog::open(path)?;
        if entries.iter().filter_map(|entry| entry.known()).any(is_title) {
            return Ok(false);
        }
        log.append(body)?;
    } else {
        SessionLog::open_append(path)?.append(body)?;
    }
    Ok(true)
}

/// Runs a catalog mutation only while no Session owns the log and no title write is in flight.
pub(crate) fn with_closed_log<T>(path: &Path, op: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let writer = writer_for_path(path)?;
    let state = lock(&writer);
    if state.active.is_some() {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "会话日志正在使用中"));
    }
    op()
}

/// 会话里存的交互选项（`session/options` 事件）：值是字符串，界面打开会话时解析，认不出的值忽略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOptions {
    pub preset: String,
    pub plugin_preset: Option<String>,
    pub permission: String,
    pub model_provider: String,
    pub model: String,
}

/// 一个会话：日志文件加上内存中的事件列表。
pub struct Session {
    id: String,
    path: PathBuf,
    writer: SharedWriter,
    /// 共享事件（[`ActiveLog`]）已经补到哪里。
    cursor: usize,
    events: Vec<SessionEvent>,
    last_turn: u32,
    /// 订阅了会话事件的插件（[`crate::EventObservers`]）：agent 每一步按当时的插件快照换一次。
    observers: Vec<Arc<dyn crate::EventObserver>>,
    /// 交给观察者的工作区。
    observed_workspace: Option<PathBuf>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut state = lock(&self.writer);
        let active = state.active.as_mut().expect("Session owns an active log");
        active.sessions -= 1;
        if active.sessions == 0 {
            state.active = None;
        }
    }
}

impl Session {
    pub fn create(id: impl Into<String>, path: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let id = id.into();
        let path = path.into();
        std::fs::create_dir_all(log_parent(&path))?;
        let writer = writer_for_path(&path)?;
        {
            let mut state = lock(&writer);
            if state.active.is_some() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "会话日志已存在").into());
            }
            state.active = Some(ActiveLog::new(SessionLog::create(&path)?, false));
        }
        Ok(Self::from_events(id, path, writer, 0, Vec::new()))
    }

    /// 恢复已有会话；不认识的事件不参与投影。
    pub fn open(id: impl Into<String>, path: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let id = id.into();
        let path = path.into();
        let writer = writer_for_path(&path)?;
        let (entries, cursor) = {
            let mut state = lock(&writer);
            if let Some(active) = &mut state.active {
                // 持锁读：读完之前没人能追加，文件内容和游标对得上。
                let entries = read_entries(&path)?;
                active.sessions += 1;
                (entries, active.end())
            } else {
                let (log, entries) = SessionLog::open(&path)?;
                let has_title = entries.iter().filter_map(|entry| entry.known()).any(is_title);
                state.active = Some(ActiveLog::new(log, has_title));
                (entries, 0)
            }
        };
        let events = entries
            .into_iter()
            .filter_map(|entry| match entry {
                LogEntry::Known(event) => Some(event),
                LogEntry::Unknown(_) => None,
            })
            .collect();
        Ok(Self::from_events(id, path, writer, cursor, events))
    }

    fn from_events(id: String, path: PathBuf, writer: SharedWriter, cursor: usize, events: Vec<SessionEvent>) -> Self {
        let last_turn = events
            .iter()
            .filter_map(|event| match event.body {
                EventBody::TurnStart { turn } => Some(turn),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        Self { id, path, writer, cursor, events, last_turn, observers: Vec::new(), observed_workspace: None }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// 会话日志文件。
    pub fn log_path(&self) -> &Path {
        &self.path
    }

    /// 子代理日志所在目录：与会话日志同目录下的 `agents/`。
    pub fn agents_dir(&self) -> PathBuf {
        self.path.parent().map_or_else(|| PathBuf::from("agents"), |dir| dir.join("agents"))
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// Checks the shared writer, even if this Session has not refreshed its projection.
    pub fn has_title(&self) -> bool {
        lock(&self.writer).active.as_ref().expect("Session owns an active log").has_title
    }

    /// 把别人写进日志的事件（后台写的标题、同一会话的另一个 `Session` 写的事件）补进内存，
    /// 不再写日志、也不再推送。补了至少一条时返回 `true`。
    pub fn refresh_title(&mut self) -> bool {
        let writer = self.writer.clone();
        let mut state = lock(&writer);
        let active = state.active.as_mut().expect("Session owns an active log");
        self.catch_up(active)
    }

    /// 同 [`Self::refresh_title`]：补的不只是标题。
    pub fn refresh(&mut self) -> bool {
        self.refresh_title()
    }

    fn catch_up(&mut self, active: &mut ActiveLog) -> bool {
        let unseen = active.unseen(self.cursor);
        let added = !unseen.is_empty();
        for event in unseen {
            if let EventBody::TurnStart { turn } = event.body {
                self.last_turn = self.last_turn.max(turn);
            }
            self.events.push(event.clone());
        }
        self.cursor = active.end();
        active.trim(self.cursor);
        added
    }

    /// 开始一轮：同一份日志上已经有一轮在跑（同一会话开在了两处）时返回 `None`。返回的凭据丢弃时放开。
    pub(crate) fn claim_turn(&mut self) -> Option<TurnClaim> {
        let writer = self.writer.clone();
        let mut state = lock(&writer);
        let active = state.active.as_mut().expect("Session owns an active log");
        if active.turn_running {
            return None;
        }
        active.turn_running = true;
        self.catch_up(active);
        drop(state);
        Some(TurnClaim(writer))
    }

    /// 最近一次投影到内存的标题；读取后台写入前先调用 [`Self::refresh_title`]。
    pub fn title(&self) -> Option<&str> {
        self.events.iter().rev().find_map(|event| match &event.body {
            EventBody::SessionTitle { title } => Some(title.as_str()),
            _ => None,
        })
    }

    /// 最近一次写入的交互选项（输入卡片上选的模式、权限、模型）；以最后一条为准。
    /// 旧会话没有这类事件时是 `None`。
    pub fn stored_options(&self) -> Option<SessionOptions> {
        self.events.iter().rev().find_map(|event| match &event.body {
            EventBody::SessionOptions { preset, plugin_preset, permission, model_provider, model } => Some(SessionOptions {
                preset: preset.clone(),
                plugin_preset: plugin_preset.clone(),
                permission: permission.clone(),
                model_provider: model_provider.clone(),
                model: model.clone(),
            }),
            _ => None,
        })
    }

    pub(crate) fn has_workspace(&self) -> bool {
        self.events.iter().any(|event| matches!(event.body, EventBody::SessionWorkspace { .. }))
    }

    pub(crate) fn has_system_message(&self) -> bool {
        self.events.iter().any(|event| matches!(event.body, EventBody::SystemMessage { .. }))
    }

    /// 下一轮的编号（不占用它）。
    pub(crate) fn peek_next_turn(&self) -> u32 {
        self.last_turn + 1
    }

    /// 换成这些观察者（agent 每一步按插件快照调用）。
    pub(crate) fn set_observers(&mut self, observers: Vec<Arc<dyn crate::EventObserver>>, workspace: &Path) {
        self.observers = observers;
        if self.observed_workspace.as_deref() != Some(workspace) {
            self.observed_workspace = Some(workspace.to_owned());
        }
    }

    pub(crate) fn next_turn(&mut self) -> u32 {
        self.last_turn += 1;
        self.last_turn
    }

    /// 追加一条事件（界面层记录 `session/options` 这类界面状态）。
    pub fn append_event(&mut self, body: EventBody, sink: &UnboundedSender<LiveEvent>) -> Result<(), CoreError> {
        self.append(body, sink)
    }

    pub(crate) fn append(&mut self, body: EventBody, sink: &UnboundedSender<LiveEvent>) -> Result<(), CoreError> {
        let event = {
            let writer = self.writer.clone();
            let mut state = lock(&writer);
            let active = state.active.as_mut().expect("Session owns an active log");
            // 先补上别人写的，自己的这条才排在它们后面（和日志里的顺序一致）。
            self.catch_up(active);
            let event = active.log.append(body)?;
            if is_title(&event) {
                active.has_title = true;
            }
            if active.sessions > 1 {
                active.shared.push(event.clone());
            }
            self.cursor = active.end();
            event
        };
        // 订阅方收到的是 `Arc` 共享的同一事件，不再为推送深拷贝整条大消息；
        // 内存里的这份克隆只加引用计数（事件里的大段文本都是 `Arc<str>`）。
        let _ = sink.send(LiveEvent::Durable(Arc::new(event.clone())));
        if !self.observers.is_empty() {
            let observed =
                crate::ObservedSession { id: &self.id, log_path: self.log_path(), workspace: self.observed_workspace.as_deref() };
            for observer in &self.observers {
                observer.observe(&observed, &event);
            }
        }
        self.events.push(event);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seqs(session: &Session) -> Vec<u64> {
        session.events().iter().map(|event| event.seq).collect()
    }

    #[test]
    fn sessions_on_one_log_see_each_others_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s").join("session.v1.jsonl");
        let (sink, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut a = Session::create("s", &path).unwrap();
        a.append(EventBody::TurnStart { turn: 1 }, &sink).unwrap();
        a.append(EventBody::UserMessage { text: "one".into() }, &sink).unwrap();

        // 同一会话开在第二处：从文件读到已有的，之后两边各写的都能互相补上，顺序和日志一致。
        let mut b = Session::open("s", &path).unwrap();
        assert_eq!((seqs(&b), b.peek_next_turn()), (vec![1, 2], 2));
        a.append(EventBody::TurnStart { turn: 2 }, &sink).unwrap();
        b.append(EventBody::UserMessage { text: "from b".into() }, &sink).unwrap();
        assert_eq!((seqs(&b), b.peek_next_turn()), (vec![1, 2, 3, 4], 3));
        assert!(a.refresh());
        assert!(!a.refresh());
        assert_eq!(seqs(&a), [1, 2, 3, 4]);

        // 后台写的标题两边都补上。
        assert!(write_title(&path, "t".into()).unwrap());
        assert!(a.refresh_title() && b.refresh_title());
        assert_eq!((a.title(), b.title()), (Some("t"), Some("t")));

        // 同一份日志同时只跑一轮。
        let claim = a.claim_turn().expect("没人在跑");
        assert!(b.claim_turn().is_none());
        drop(claim);
        assert!(b.claim_turn().is_some());

        // 只剩一个且补齐后，共享的事件不再留着。
        drop(b);
        a.refresh();
        assert!(lock(&a.writer).active.as_ref().unwrap().shared.is_empty());
        a.append(EventBody::TurnEnd { turn: 2, reason: dshr_core::TurnEndReason::Completed, error: None }, &sink).unwrap();
        assert!(lock(&a.writer).active.as_ref().unwrap().shared.is_empty());
        assert_eq!(read_entries(&path).unwrap().len(), a.events().len());
    }
}
