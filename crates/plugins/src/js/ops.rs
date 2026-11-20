//! 插件线程之外完成的“慢操作”：`fetch`、`exec`、`llm.complete`……
//!
//! 脚本调用这类宿主函数时，宿主只做同步的检查（能力、路径），把真正的工作交给共用的 IO 运行时，
//! 马上返回一个操作编号；`prelude.js` 用这个编号造一个 Promise。操作完成后结果经 channel 回到插件线程，
//! 由 `runtime.rs` 的事件循环交给脚本（`settle`）。这样一次调用里可以同时等好几个请求（`Promise.all`），
//! 定时器也能在等待期间触发。
//!
//! 一次调用结束（正常返回、出错、超时、被中断）时，它发起的、还没完成的操作全部作废：
//! 运行时那边立即中止，迟到的结果被丢弃，不会落到下一次调用里。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
/// 一次调用里同时进行的操作最多这么多。
const MAX_IN_FLIGHT: usize = 32;

/// 一个操作的结果：成功时是交给脚本的文本（多半是 JSON），失败时是错误信息。
pub(crate) type OpResult = Result<String, String>;

/// 完成队列里的一项：操作编号和结果。
type Done = (u64, OpResult);

/// 所有插件共用的 IO 运行时：网络请求、子进程、模型请求都在这里跑。
pub(crate) fn io() -> Result<&'static tokio::runtime::Runtime, String> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("dshr-js-io")
                .enable_all()
                .build()
                .map_err(|error| format!("cannot start the plugin IO runtime: {error}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

pub(crate) struct Ops {
    next: Cell<u64>,
    /// 这次调用的完成队列；每次调用开始时换一对新的，上一次调用的迟到结果发到已经丢弃的接收端。
    channel: RefCell<(mpsc::Sender<Done>, mpsc::Receiver<Done>)>,
    in_flight: Cell<usize>,
    /// 这次调用结束时取消：还在跑的操作都中止。调用方放弃时插件线程会结束这次调用，也走这里。
    /// 用令牌而不是定时去看标志：操作在等网络时不必每隔几十毫秒醒一次。
    call_over: RefCell<CancellationToken>,
    /// 单个操作的取消（`AbortController`）。
    aborts: RefCell<HashMap<u64, CancellationToken>>,
    /// 这次调用打开的流式响应和 WebSocket：调用结束时全部关掉。
    resources: RefCell<HashMap<u64, Resource>>,
}

/// 一次调用里打开着的东西，按编号存。
#[derive(Clone)]
pub(crate) enum Resource {
    /// 流式响应的正文：一块一块读。
    Stream(Arc<tokio::sync::Mutex<super::BodyReceiver>>),
    /// WebSocket 连接。
    Socket(Arc<super::stream::Socket>),
    /// `host.spawn` 启动的程序：资源被丢掉时进程被结束。
    Process(Arc<super::process::Process>),
}

impl Default for Ops {
    fn default() -> Self {
        Self {
            next: Cell::new(1),
            channel: RefCell::new(mpsc::channel()),
            in_flight: Cell::new(0),
            call_over: RefCell::new(CancellationToken::new()),
            aborts: RefCell::new(HashMap::new()),
            resources: RefCell::new(HashMap::new()),
        }
    }
}

impl Ops {
    /// 一次调用开始。
    pub fn begin(&self) {
        *self.channel.borrow_mut() = mpsc::channel();
        *self.call_over.borrow_mut() = CancellationToken::new();
        self.in_flight.set(0);
        self.aborts.borrow_mut().clear();
    }

    /// 一次调用结束：没完成的操作作废，打开着的流和连接关掉。
    pub fn end(&self) {
        self.call_over.borrow().cancel();
        self.in_flight.set(0);
        self.aborts.borrow_mut().clear();
        self.resources.borrow_mut().clear();
    }

    /// 登记一个打开着的东西，返回它的编号。
    pub fn add_resource(&self, resource: Resource) -> Result<u64, String> {
        if self.resources.borrow().len() >= MAX_IN_FLIGHT {
            return Err(format!("too many open streams and sockets (the limit is {MAX_IN_FLIGHT})"));
        }
        let id = self.next.get();
        self.next.set(id + 1);
        self.resources.borrow_mut().insert(id, resource);
        Ok(id)
    }

    pub fn resource(&self, id: u64) -> Option<Resource> {
        self.resources.borrow().get(&id).cloned()
    }

    pub fn remove_resource(&self, id: u64) {
        self.resources.borrow_mut().remove(&id);
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.get()
    }

    /// 在 IO 运行时上开始一个操作，返回编号。`remaining` 是这次调用剩下的时限。
    /// 调用结束（包括调用方放弃、插件线程随即结束调用）或被单独中止时，操作立刻停下。
    pub fn start<F>(&self, work: F, remaining: Duration) -> Result<u64, String>
    where
        F: Future<Output = OpResult> + Send + 'static,
    {
        if self.in_flight.get() >= MAX_IN_FLIGHT {
            return Err(format!("too many host operations at once (the limit is {MAX_IN_FLIGHT})"));
        }
        let runtime = io()?;
        let id = self.next.get();
        self.next.set(id + 1);
        let abort = CancellationToken::new();
        self.aborts.borrow_mut().insert(id, abort.clone());
        let call_over = self.call_over.borrow().clone();
        let done = self.channel.borrow().0.clone();
        runtime.spawn(async move {
            let result = tokio::select! {
                result = work => result,
                () = tokio::time::sleep(remaining) => Err("the operation did not finish within the call's time limit".into()),
                () = call_over.cancelled() => Err("the operation was aborted".into()),
                () = abort.cancelled() => Err("the operation was aborted".into()),
            };
            // 调用已经结束时接收端不在了，结果直接丢弃。
            let _ = done.send((id, result));
        });
        self.in_flight.set(self.in_flight.get() + 1);
        Ok(id)
    }

    /// 中止一个操作（`AbortController.abort()`）。它的结果仍会以错误的形式回来。
    pub fn abort(&self, id: u64) {
        if let Some(abort) = self.aborts.borrow().get(&id) {
            abort.cancel();
        }
    }

    /// 等一个操作完成，最多等 `wait`。
    pub fn wait(&self, wait: Duration) -> Option<Done> {
        let received = self.channel.borrow().1.recv_timeout(wait).ok()?;
        self.in_flight.set(self.in_flight.get().saturating_sub(1));
        self.aborts.borrow_mut().remove(&received.0);
        Some(received)
    }
}
