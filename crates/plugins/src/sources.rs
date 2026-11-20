//! 装过的插件从哪里来、是谁签的，以及旧版本的归档（回滚用）。
//!
//! - `<数据目录>/plugin-sources.json`：插件 id → [`Source`]（来源、版本、sha256、签名的公钥、从哪个索引装的、安装时间）。
//! - 发布者“首次信任”：第一次装时记下签名的公钥，之后的更新必须是同一把钥匙签的；之前签了名、这次没签也不行。
//!   用户明确说了（`--force`）才放过，提示里写明“发布者变了”。
//! - 升级时旧版本不删，挪到 `<数据目录>/plugin-versions/<id>/<版本>-<时间>/`，每个插件留最近 [`KEEP_VERSIONS`] 个；
//!   回滚就是从那里再装一次（同样确认，能力变了同样要再允许）。数据目录和授权不动。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::app::Publisher;

pub const SOURCES_FILE: &str = "plugin-sources.json";
const VERSIONS_DIR: &str = "plugin-versions";
/// 每个插件留几个旧版本。
pub const KEEP_VERSIONS: usize = 3;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    /// 安装来源：文件、网址、`index:<索引地址>`、`rollback`。
    pub source: String,
    pub version: Option<String>,
    pub sha256: Option<String>,
    /// 签名的公钥（`dshr-ed25519:…`）；没签名时没有。
    pub signer: Option<String>,
    /// 从索引装的：索引的地址（`dshr plugin outdated` 据此查新版本）。
    pub index: Option<String>,
    /// 安装时间（Unix 秒）。
    pub installed_at: u64,
}

fn home(plugins_dir: &Path) -> &Path {
    plugins_dir.parent().unwrap_or(plugins_dir)
}

pub fn sources_path(plugins_dir: &Path) -> PathBuf {
    home(plugins_dir).join(SOURCES_FILE)
}

pub fn load(plugins_dir: &Path) -> BTreeMap<String, Source> {
    std::fs::read_to_string(sources_path(plugins_dir)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn save(plugins_dir: &Path, sources: &BTreeMap<String, Source>) -> Result<(), String> {
    let path = sources_path(plugins_dir);
    let text = serde_json::to_string_pretty(sources).map_err(|error| error.to_string())?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text).and_then(|()| std::fs::rename(&temp, &path)).map_err(|error| format!("写不了 {}：{error}", path.display()))
}

pub(crate) fn record(plugins_dir: &Path, id: &str, mut source: Source) -> Result<(), String> {
    source.installed_at = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0);
    let mut sources = load(plugins_dir);
    // 回滚、本地重装不改“从哪个索引装的”和首次信任的公钥以外的东西：保留索引地址，方便之后照常查更新。
    if source.index.is_none()
        && let Some(previous) = sources.get(id)
    {
        source.index = previous.index.clone();
    }
    sources.insert(id.to_owned(), source);
    save(plugins_dir, &sources)
}

pub(crate) fn forget(plugins_dir: &Path, id: &str) {
    let mut sources = load(plugins_dir);
    if sources.remove(id).is_some() {
        let _ = save(plugins_dir, &sources);
    }
}

/// 首次信任：`signer` 是这次要装的包的签名公钥。
pub(crate) fn check_publisher(plugins_dir: &Path, id: &str, signer: Option<&str>) -> Result<(), String> {
    match publisher_status(plugins_dir, id, signer) {
        Publisher::Changed { previous, now: Some(now) } => {
            Err(format!("发布者变了：{id} 之前由 {previous} 签名，这个包由 {now} 签名。确认可信的话加 --force"))
        }
        Publisher::Changed { previous, now: None } => {
            Err(format!("{id} 之前的版本有签名（{previous}），这个包没有签名。确认可信的话加 --force"))
        }
        _ => Ok(()),
    }
}

/// 这个包的签名和之前记下的发布者比起来怎么样（安装确认里显示，[`check_publisher`] 据此拦下）。
pub fn publisher_status(plugins_dir: &Path, id: &str, signer: Option<&str>) -> Publisher {
    let previous = load(plugins_dir).get(id).and_then(|source| source.signer.clone());
    match (previous, signer) {
        (None, None) => Publisher::Unsigned,
        (None, Some(key)) => Publisher::Signed { key: key.to_owned() },
        (Some(previous), Some(key)) if previous == key => Publisher::Trusted { key: previous },
        (Some(previous), now) => Publisher::Changed { previous, now: now.map(str::to_owned) },
    }
}

fn versions_dir(plugins_dir: &Path, id: &str) -> PathBuf {
    home(plugins_dir).join(VERSIONS_DIR).join(id)
}

/// 把装着的插件文件夹归档起来（升级前），只留最近 [`KEEP_VERSIONS`] 个。
pub(crate) fn archive(plugins_dir: &Path, id: &str, folder: &Path) -> Result<PathBuf, String> {
    let version = crate::manifest::load(folder, id).ok().and_then(|manifest| manifest.version).unwrap_or_else(|| "unversioned".into());
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_millis()).unwrap_or(0);
    let dir = versions_dir(plugins_dir, id);
    std::fs::create_dir_all(&dir).map_err(|error| format!("建不了 {}：{error}", dir.display()))?;
    let target = dir.join(format!("{version}-{stamp}"));
    std::fs::rename(folder, &target).map_err(|error| format!("归档不了旧版本：{error}"))?;
    let mut versions = versions(plugins_dir, id);
    while versions.len() > KEEP_VERSIONS {
        if let Some(oldest) = versions.pop() {
            let _ = std::fs::remove_dir_all(&oldest.dir);
        }
    }
    Ok(target)
}

/// 一个归档的旧版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archived {
    pub version: String,
    pub dir: PathBuf,
}

/// 归档的旧版本，新的在前。
pub fn versions(plugins_dir: &Path, id: &str) -> Vec<Archived> {
    let Ok(entries) = std::fs::read_dir(versions_dir(plugins_dir, id)) else { return Vec::new() };
    let mut found: Vec<(u128, Archived)> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (version, stamp) = name.rsplit_once('-')?;
            Some((stamp.parse().ok()?, Archived { version: version.to_owned(), dir: entry.path() }))
        })
        .collect();
    found.sort_by_key(|(stamp, _)| std::cmp::Reverse(*stamp));
    found.into_iter().map(|(_, archived)| archived).collect()
}

/// 回滚要装的那个旧版本：给了版本号就找那个，否则是最近的一个。
pub fn rollback_source(plugins_dir: &Path, id: &str, to: Option<&str>) -> Result<Archived, String> {
    let versions = versions(plugins_dir, id);
    let listed = || versions.iter().map(|archived| archived.version.clone()).collect::<Vec<_>>().join("、");
    match to {
        Some(wanted) => versions.iter().find(|archived| archived.version == wanted).cloned().ok_or_else(|| {
            if versions.is_empty() {
                format!("{id} 没有归档的旧版本")
            } else {
                format!("{id} 没有归档的 {wanted}（有：{}）", listed())
            }
        }),
        None => versions.first().cloned().ok_or_else(|| format!("{id} 没有归档的旧版本（升级时才会归档）")),
    }
}
