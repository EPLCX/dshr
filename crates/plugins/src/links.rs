//! 在任意文件夹里开发插件：`dshr plugin link <文件夹>` 在数据目录的 `plugin-links.json` 里记一条 id → 文件夹，
//! 不拷贝。插件目录的扫描（[`crate::discover`]）把链接的文件夹也算进来，热重载同样盯着它们。
//!
//! 链接是用户亲手做的（命令行、设置页），和“工作区里的插件目录不加载”不矛盾：克隆一个仓库不会顺带执行它带的代码。
//! 链接的插件和别的插件一样要授权；插件目录里有同 id 的文件夹时以插件目录里的为准。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 链接记录的文件名，放在插件目录的上一级（数据目录）里。
pub const LINKS_FILE: &str = "plugin-links.json";

/// 插件目录 `<数据目录>/plugins` 对应的链接记录。
pub fn links_path(plugins_dir: &Path) -> PathBuf {
    plugins_dir.parent().unwrap_or(plugins_dir).join(LINKS_FILE)
}

/// 链接：插件 id → 文件夹。文件不存在或读不出来时是空的。
pub fn load(plugins_dir: &Path) -> BTreeMap<String, PathBuf> {
    let path = links_path(plugins_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "插件链接记录读不出来，当作没有");
            BTreeMap::new()
        }),
        Err(_) => BTreeMap::new(),
    }
}

fn save(plugins_dir: &Path, links: &BTreeMap<String, PathBuf>) -> Result<(), String> {
    let path = links_path(plugins_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let text = serde_json::to_string_pretty(links).map_err(|error| error.to_string())?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text).and_then(|()| std::fs::rename(&temp, &path)).map_err(|error| format!("写不了 {}：{error}", path.display()))
}

/// 链接一个文件夹，返回插件 id（清单里的 id，没有时是文件夹名，或者 `id` 参数）。
pub fn link(plugins_dir: &Path, folder: &Path, id: Option<&str>) -> Result<String, String> {
    let folder = std::path::absolute(folder).map_err(|error| format!("{}：{error}", folder.display()))?;
    if !folder.is_dir() {
        return Err(format!("{} 不是文件夹", folder.display()));
    }
    let folder_name = folder.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let fallback = id.map(str::to_owned).unwrap_or(folder_name);
    let manifest = crate::manifest::load(&folder, &fallback)?;
    if let Some(id) = id
        && manifest.id != id
    {
        return Err(format!("{} 的清单里 id 是 {}，不是 {id}", folder.display(), manifest.id));
    }
    if plugins_dir.join(&manifest.id).exists() {
        return Err(format!("插件目录里已经有 {}：先卸载它，或者换一个 id", manifest.id));
    }
    let mut links = load(plugins_dir);
    links.insert(manifest.id.clone(), folder);
    save(plugins_dir, &links)?;
    Ok(manifest.id)
}

/// 取消链接（不动文件夹本身）。
pub fn unlink(plugins_dir: &Path, id: &str) -> Result<PathBuf, String> {
    let mut links = load(plugins_dir);
    let folder = links.remove(id).ok_or_else(|| format!("没有链接 {id}"))?;
    save(plugins_dir, &links)?;
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_and_unlink() {
        let home = tempfile::tempdir().unwrap();
        let plugins = home.path().join("plugins");
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("plugin.toml"), "id = \"my-dev\"\n").unwrap();
        std::fs::write(source.path().join("index.js"), "").unwrap();
        assert_eq!(link(&plugins, source.path(), None).unwrap(), "my-dev");
        assert_eq!(load(&plugins)["my-dev"], source.path());
        assert!(link(&plugins, source.path(), Some("other")).unwrap_err().contains("不是 other"));
        // 扫描插件目录时链接的文件夹也算进来。
        let found = crate::discover(&plugins);
        assert_eq!(found.iter().map(|found| found.id.as_str()).collect::<Vec<_>>(), ["my-dev"]);
        assert_eq!(found[0].dir, source.path());
        assert_eq!(unlink(&plugins, "my-dev").unwrap(), source.path());
        assert!(crate::discover(&plugins).is_empty());
        assert!(unlink(&plugins, "my-dev").is_err());
    }
}
