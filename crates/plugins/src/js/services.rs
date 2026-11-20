//! 常驻服务：JS 插件的 `services` 导出（要 `background`）。
//!
//! 服务要一直跑（守着一个 WebSocket、长轮询一个 IM 接口），而插件的运行时一次只做一件事：服务占着它，
//! 工具就调不动了。所以有服务的插件另起一个**服务运行时**——同一份代码再加载一次，模块里的变量和工具那边
//! 不共享（要共享状态就放数据目录）。每个服务是这个运行时里一次没有时限的调用：
//!
//! - 插件被停用（没有处于激活状态的 `Context`）或者这个版本被换下、卸掉时，调用被中断，脚本停在原地；
//! - 服务出错或返回后按退避重启（1 秒起，每次翻倍，最多 5 分钟；跑满 1 分钟后算一次新的开始）；
//! - 连续 5 次在 1 分钟之内就失败，就停下，原因显示在设置页这个插件的卡片上。
//!
//! 服务里能联网（`fetch`、`websocket`，有 `net:` 时）、替用户发消息（有 `session:control` 时）、发通知，
//! 不能写工作区、不能启动程序；没有工作区，文件函数指向插件自己的数据目录。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use dshr_tools::ToolContext;
use serde_json::Value;

use super::host::Permits;
use super::runtime::{Instance, Job};
use crate::manifest::PluginManifest;

/// 服务这次调用的“时限”：实际上不限。
const SERVICE_LIMIT: Duration = Duration::from_secs(365 * 24 * 3600);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// 跑了这么久之后再失败，算一次新的开始（退避和计数清零）。
const HEALTHY_RUN: Duration = Duration::from_secs(60);
const MAX_QUICK_FAILURES: u32 = 5;
/// 多久看一次插件还在不在用。
const WATCH: Duration = Duration::from_millis(500);

/// 服务的状态：服务 id → 一句话（“运行中”“已停下：…”）。设置页显示。
pub(crate) type ServiceStatus = Arc<Mutex<BTreeMap<String, String>>>;

/// 起服务运行时、跑起每个服务。`owner` 是工具那边的运行时：它没了（换版本、卸掉）服务就收工。
pub(super) fn start(
    plugin: &'static str,
    manifest: &PluginManifest,
    config: &Value,
    owner: Weak<Instance>,
    active: Arc<AtomicUsize>,
    status: ServiceStatus,
) {
    let Ok(runtime) = super::ops::io() else {
        tracing::error!(plugin, "没有 IO 运行时，常驻服务没有启动");
        return;
    };
    let (manifest, config) = (manifest.clone(), config.clone());
    let set = {
        let status = status.clone();
        move |id: &str, text: String| {
            status.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(id.to_owned(), text);
        }
    };
    runtime.spawn(async move {
        let ids: Vec<String> = match owner.upgrade() {
            Some(owner) => owner.exports.services.iter().map(|service| service.id.clone()).collect(),
            None => return,
        };
        let loaded = {
            let (manifest, config) = (manifest.clone(), config.clone());
            tokio::task::spawn_blocking(move || Instance::start(plugin, &manifest, &config, None)).await
        };
        let instance = match loaded.map_err(|error| error.to_string()).and_then(|loaded| loaded) {
            Ok(instance) => Arc::new(instance),
            Err(error) => {
                for id in &ids {
                    set(id, format!("服务运行时加载失败：{error}"));
                }
                return;
            }
        };
        let data_dir = super::surface::data_dir(&manifest);
        let capabilities = &manifest.capabilities;
        let permits =
            Permits { write: false, exec: false, net: !capabilities.net.is_empty(), llm: false, act: capabilities.session_control };
        let mut runs = Vec::new();
        for id in ids {
            let (instance, owner, active, data_dir, set) = (instance.clone(), owner.clone(), active.clone(), data_dir.clone(), set.clone());
            runs.push(tokio::spawn(async move {
                let alive = || owner.strong_count() > 0;
                let (mut backoff, mut quick_failures) = (Duration::from_secs(1), 0u32);
                loop {
                    if !alive() {
                        return;
                    }
                    if active.load(Ordering::Relaxed) == 0 {
                        set(&id, "已停用".into());
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    let _ = std::fs::create_dir_all(&data_dir);
                    set(&id, "运行中".into());
                    let started = Instant::now();
                    let job = Job::Dispatch { kind: "service", name: id.clone(), payload: "null".into() };
                    let call = instance.call(job, Some(ToolContext::new(data_dir.clone())), SERVICE_LIMIT, permits);
                    // 插件停用或换下时丢掉这次调用：脚本随即被中断。
                    let watch = async {
                        loop {
                            tokio::time::sleep(WATCH).await;
                            if !alive() || active.load(Ordering::Relaxed) == 0 {
                                return;
                            }
                        }
                    };
                    let outcome = tokio::select! {
                        outcome = call => outcome,
                        () = watch => continue,
                    };
                    let ran = started.elapsed();
                    if ran >= HEALTHY_RUN {
                        backoff = Duration::from_secs(1);
                        quick_failures = 0;
                    }
                    match outcome {
                        Ok(_) => set(&id, format!("已退出，{} 秒后重启", backoff.as_secs())),
                        Err(error) => {
                            quick_failures += 1;
                            let first = error.lines().next().unwrap_or_default().to_owned();
                            tracing::warn!(plugin, service = id, error = first, "插件的常驻服务出错");
                            if quick_failures >= MAX_QUICK_FAILURES {
                                set(&id, format!("连续 {MAX_QUICK_FAILURES} 次很快失败，已停下（最近一次：{first}）"));
                                return;
                            }
                            set(&id, format!("出错，{} 秒后重启：{first}", backoff.as_secs()));
                        }
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }));
        }
        for run in runs {
            let _ = run.await;
        }
        // 所有服务都收工了：服务运行时随 `instance` 释放而退出。
        drop(instance);
    });
}
