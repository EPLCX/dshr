//! 能力授权：清单里声明的能力要用户允许过才生效。
//!
//! 记在数据目录的 `plugin-grants.json` 里：插件 id → 允许过的能力（清单里的写法）。没有声明能力的插件
//! 不用授权；清单里多出一项没允许过的能力，整个插件就不激活，直到再允许一次——装插件的人看一眼
//! 它要什么，热重载时悄悄多出来的 `net:*` 也不会直接生效。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// 授权记录的文件名，放在插件目录的上一级（数据目录）里。
pub const GRANTS_FILE: &str = "plugin-grants.json";

/// 插件 id → 允许过的能力。
pub type Grants = BTreeMap<String, BTreeSet<String>>;

/// 插件目录 `<数据目录>/plugins` 对应的授权文件。
pub fn grants_path(plugins_dir: &Path) -> PathBuf {
    plugins_dir.parent().unwrap_or(plugins_dir).join(GRANTS_FILE)
}

/// 读授权记录。没有文件是空的；文件坏了也当空的（记一条警告），这样只会多问一次，不会多放行。
pub fn load(path: &Path) -> Grants {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "授权记录读不出来，当作什么都没允许过");
            Grants::new()
        }),
        Err(_) => Grants::new(),
    }
}

/// 给 `id` 加上这些能力（已经允许过的保留）。先写临时文件再改名，不会留下写了一半的文件。
pub fn grant(path: &Path, id: &str, capabilities: &[String]) -> std::io::Result<()> {
    let mut grants = load(path);
    grants.entry(id.to_owned()).or_default().extend(capabilities.iter().cloned());
    write(path, &grants)
}

/// 收回 `id` 的全部授权。
pub fn revoke(path: &Path, id: &str) -> std::io::Result<()> {
    let mut grants = load(path);
    if grants.remove(id).is_none() {
        return Ok(());
    }
    write(path, &grants)
}

fn write(path: &Path, grants: &Grants) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(grants).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

/// `declared` 里还没允许过的。
pub fn missing(grants: &Grants, id: &str, declared: &[String]) -> Vec<String> {
    let granted = grants.get(id);
    declared.iter().filter(|name| !granted.is_some_and(|granted| granted.contains(*name))).cloned().collect()
}

/// 一个等用户授权的插件：清单里声明了、还没允许过的能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRequest {
    pub id: String,
    /// 清单里声明的全部能力。
    pub declared: Vec<String>,
    /// 其中还没允许过的。
    pub missing: Vec<String>,
}
