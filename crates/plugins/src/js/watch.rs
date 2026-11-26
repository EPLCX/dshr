//! `export const watch = [{ id, glob, run: async ({ workspace, changes, overflow }, host) => {} }]`：工作区里的文件变了就告诉插件。
//!
//! - 要 `background` 和 `fs:workspace:read`。监听的是打开着的会话的工作区（桌面版；命令行版是这次运行的工作区），
//!   每 3 秒看一次有没有新开、关掉的。
//! - 遵守工作区根目录的 `.gitignore`，跳过 `.git`、`node_modules`。变化攒 500 毫秒一批，每批每个工作区最多
//!   [`MAX_CHANGES`] 条（多了给 `overflow: true`）。
//! - 处理函数能读工作区、联网（有 `net:` 时）、读写自己的数据目录；不能写工作区、启动程序、调模型——
//!   不然“文件变了 → 插件改文件 → 又变了”会停不下来，也不该在后台悄悄花 token。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use dshr_tools::ToolContext;
use notify::Watcher as _;
use serde_json::json;

use super::host::Permits;
use super::runtime::{Instance, Job};
use crate::manifest::PluginManifest;

const REFRESH: Duration = Duration::from_secs(3);
const BATCH: Duration = Duration::from_millis(500);
const MAX_CHANGES: usize = 1000;

/// 测试里直接给要监听的工作区（不经主程序的会话列表）。
#[cfg(test)]
pub(crate) static TEST_WORKSPACES: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// 现在打开着的会话的工作区。
fn workspaces() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = crate::app::app_services()
        .session
        .map(|session| session.list().into_iter().filter_map(|open| open.workspace.map(PathBuf::from)).collect())
        .unwrap_or_default();
    #[cfg(test)]
    found.extend(TEST_WORKSPACES.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().cloned());
    found.sort();
    found.dedup();
    found.retain(|path| path.is_dir());
    found
}

/// 一个工作区的监听：文件系统通知，加上根目录的 `.gitignore`。
struct Watched {
    _watcher: notify::RecommendedWatcher,
    ignore: ignore::gitignore::Gitignore,
    /// 工作区的真实路径（开始监听时取一次）：有的平台报告的是解析过符号链接的路径。
    canonical_root: PathBuf,
}

fn skipped(relative: &Path) -> bool {
    relative.components().any(|part| matches!(part.as_os_str().to_str(), Some(".git" | "node_modules")))
}

pub(super) fn start(plugin: &'static str, manifest: &PluginManifest, instance: &Arc<Instance>, active: Arc<AtomicUsize>) {
    if instance.exports.watch.is_empty() {
        return;
    }
    let Ok(runtime) = super::ops::io() else {
        tracing::error!(plugin, "没有 IO 运行时，文件监听没有启动");
        return;
    };
    let mut globs = Vec::new();
    for decl in &instance.exports.watch {
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &decl.globs {
            match globset::Glob::new(pattern) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(error) => tracing::warn!(plugin, watch = decl.id, %error, "监听的 glob 写错了"),
            }
        }
        match builder.build() {
            Ok(set) => globs.push((decl.id.clone(), set)),
            Err(error) => tracing::warn!(plugin, watch = decl.id, %error, "监听的 glob 写错了"),
        }
    }
    let permits = Permits { write: false, exec: false, net: !manifest.capabilities.net.is_empty(), llm: false, act: false };
    let timeout = manifest.timeout;
    let weak: Weak<Instance> = Arc::downgrade(instance);
    runtime.spawn(async move {
        let (sender, mut changes) = tokio::sync::mpsc::unbounded_channel::<(PathBuf, PathBuf, &'static str)>();
        let mut watched: HashMap<PathBuf, Watched> = HashMap::new();
        let mut pending: BTreeMap<PathBuf, BTreeMap<String, &'static str>> = BTreeMap::new();
        let mut overflow: Vec<PathBuf> = Vec::new();
        let mut refresh = tokio::time::interval(REFRESH);
        let mut flush_at: Option<tokio::time::Instant> = None;
        loop {
            let flush = async {
                match flush_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = refresh.tick() => {
                    if weak.strong_count() == 0 {
                        return;
                    }
                    let wanted = workspaces();
                    watched.retain(|root, _| wanted.contains(root));
                    for root in wanted {
                        if watched.contains_key(&root) {
                            continue;
                        }
                        let (sender, events_root) = (sender.clone(), root.clone());
                        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                            let Ok(event) = event else { return };
                            let kind = match event.kind {
                                notify::EventKind::Create(_) => "created",
                                notify::EventKind::Modify(_) => "modified",
                                notify::EventKind::Remove(_) => "removed",
                                _ => return,
                            };
                            for path in event.paths {
                                let _ = sender.send((events_root.clone(), path, kind));
                            }
                        });
                        let Ok(mut watcher) = watcher else { continue };
                        if watcher.watch(&root, notify::RecursiveMode::Recursive).is_err() {
                            continue;
                        }
                        let mut builder = ignore::gitignore::GitignoreBuilder::new(&root);
                        builder.add(root.join(".gitignore"));
                        let ignore = builder.build().unwrap_or_else(|_| ignore::gitignore::Gitignore::empty());
                        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
                        watched.insert(root, Watched { _watcher: watcher, ignore, canonical_root });
                    }
                }
                Some((root, path, kind)) = changes.recv() => {
                    let Some(watch) = watched.get(&root) else { continue };
                    // 编辑器保存时常是“写临时文件再改名”：只认还在工作区里的路径。
                    let relative = path.strip_prefix(&root).or_else(|_| path.strip_prefix(&watch.canonical_root)).map(Path::to_path_buf);
                    let Ok(relative) = relative else { continue };
                    if relative.as_os_str().is_empty() || skipped(&relative) {
                        continue;
                    }
                    // 先按插件声明的 glob 过滤（只是字符串匹配）：大多数变化（构建输出、缓存）没有插件关心，
                    // 用不着再查忽略规则、stat 一下。
                    let key = relative.to_string_lossy().replace('\\', "/");
                    if !globs.iter().any(|(_, set)| set.is_match(&key)) {
                        continue;
                    }
                    let is_dir = path.is_dir();
                    if is_dir || watch.ignore.matched_path_or_any_parents(&relative, is_dir).is_ignore() {
                        continue;
                    }
                    let entry = pending.entry(root.clone()).or_default();
                    if entry.len() >= MAX_CHANGES {
                        if !overflow.contains(&root) {
                            overflow.push(root.clone());
                        }
                    } else {
                        entry.insert(key, kind);
                    }
                    flush_at.get_or_insert_with(|| tokio::time::Instant::now() + BATCH);
                }
                _ = flush => {
                    flush_at = None;
                    let batches = std::mem::take(&mut pending);
                    let overflowed = std::mem::take(&mut overflow);
                    let Some(instance) = weak.upgrade() else { return };
                    if active.load(Ordering::Relaxed) == 0 {
                        continue;
                    }
                    for (root, files) in batches {
                        for (id, set) in &globs {
                            let matched: Vec<_> = files
                                .iter()
                                .filter(|(path, _)| set.is_match(path.as_str()))
                                .map(|(path, kind)| json!({ "path": path, "kind": kind }))
                                .collect();
                            if matched.is_empty() {
                                continue;
                            }
                            let payload = json!({
                                "workspace": root.display().to_string(),
                                "changes": matched,
                                "overflow": overflowed.contains(&root),
                            });
                            let job = Job::Dispatch { kind: "watch", name: id.clone(), payload: payload.to_string() };
                            let ctx = ToolContext::new(root.clone());
                            if let Err(error) = instance.call(job, Some(ctx), timeout, permits).await {
                                tracing::warn!(plugin, watch = id, error, "插件处理文件变化失败");
                            }
                        }
                    }
                }
            }
        }
    });
}
