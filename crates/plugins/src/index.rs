//! 插件索引：一个静态 JSON 文件，列出插件和它们的版本（托管在哪里由发布者决定；默认不配置任何索引）。
//!
//! ```toml
//! # config.toml
//! [plugin_index]
//! urls = ["https://example.com/dshr-plugins/index.json"]
//! ```
//!
//! ```json
//! { "plugins": [ { "id": "word-count", "name": "字数统计", "description": "…",
//!     "publisher": { "name": "Alice", "key": "dshr-ed25519:…" },
//!     "versions": [ { "version": "1.2.0", "url": "https://…/word-count-1.2.0.dshrplugin", "sha256": "…", "dshr": ">=0.1" } ] } ] }
//! ```
//!
//! - 按索引装时下载的包必须对得上 `sha256`；索引给了发布者公钥时，包必须由这把钥匙签名。
//! - 挑版本：宿主版本满足 `dshr` 要求的版本里最新的一个（semver 比较）。

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Index {
    #[serde(default)]
    pub plugins: Vec<IndexEntry>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct IndexEntry {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub publisher: Option<Publisher>,
    #[serde(default)]
    pub versions: Vec<IndexVersion>,
    /// 从哪个索引来的（读进来之后填）。
    #[serde(skip)]
    pub index: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Publisher {
    #[serde(default)]
    pub name: Option<String>,
    /// `dshr-ed25519:<base64>`。
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct IndexVersion {
    pub version: String,
    pub url: String,
    pub sha256: String,
    /// 要求的宿主版本（semver 范围）。
    #[serde(default)]
    pub dshr: Option<String>,
}

impl IndexEntry {
    /// 当前宿主能用的最新版本。
    pub fn best_version(&self) -> Option<&IndexVersion> {
        self.versions
            .iter()
            .filter(|version| crate::manifest::host_satisfies(version.dshr.as_deref()))
            .filter_map(|version| semver::Version::parse(&version.version).ok().map(|parsed| (parsed, version)))
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, version)| version)
    }
}

/// 解析一个索引文件的内容。
pub fn parse(text: &str, url: &str) -> Result<Vec<IndexEntry>, String> {
    let index: Index = serde_json::from_str(text).map_err(|error| format!("索引 {url} 格式不对：{error}"))?;
    Ok(index
        .plugins
        .into_iter()
        .filter(|entry| crate::manifest::valid_id(&entry.id))
        .map(|entry| IndexEntry { index: url.to_owned(), ..entry })
        .collect())
}

/// 取所有索引（一个取不到不影响别的，原因放在第二个返回值里）。
#[cfg(feature = "js")]
pub async fn fetch(urls: &[String]) -> (Vec<IndexEntry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut problems = Vec::new();
    for url in urls {
        match crate::install::download(url).await {
            Ok(bytes) => match parse(&String::from_utf8_lossy(&bytes), url) {
                Ok(found) => entries.extend(found),
                Err(error) => problems.push(error),
            },
            Err(error) => problems.push(format!("{url}：{error}")),
        }
    }
    (entries, problems)
}

/// 按 id、名字、说明找（不区分大小写）；空查询列出全部。
pub fn search<'a>(entries: &'a [IndexEntry], query: &str) -> Vec<&'a IndexEntry> {
    let query = query.to_lowercase();
    entries
        .iter()
        .filter(|entry| {
            query.is_empty()
                || entry.id.to_lowercase().contains(&query)
                || entry.name.as_deref().is_some_and(|name| name.to_lowercase().contains(&query))
                || entry.description.as_deref().is_some_and(|text| text.to_lowercase().contains(&query))
        })
        .collect()
}

/// 装着的版本比索引里能用的最新版本旧：(id, 装着的, 最新的)。没有版本号、索引里没有的不算。
pub fn outdated(entries: &[IndexEntry], installed: &[(String, Option<String>)]) -> Vec<(String, String, String)> {
    installed
        .iter()
        .filter_map(|(id, version)| {
            let current = semver::Version::parse(version.as_deref()?).ok()?;
            let latest = entries.iter().find(|entry| entry.id == *id)?.best_version()?;
            let parsed = semver::Version::parse(&latest.version).ok()?;
            (parsed > current).then(|| (id.clone(), current.to_string(), latest.version.clone()))
        })
        .collect()
}

/// 按索引装一个插件：下载、对 sha256、验发布者公钥，然后暂存（确认和提交由调用方做）。
#[cfg(feature = "js")]
pub async fn stage(entry: &IndexEntry, version: &IndexVersion) -> Result<crate::install::Staged, String> {
    let bytes = crate::install::download(&version.url).await?;
    let staged =
        crate::install::stage_bytes(&bytes, &version.url, &format!("index:{}", entry.index), Some(&entry.id), Some(&version.sha256))?;
    if let Some(key) = entry.publisher.as_ref().and_then(|publisher| publisher.key.as_deref()) {
        match staged.signer.as_deref() {
            Some(signer) if signer == key => {}
            Some(signer) => return Err(format!("{} 的包由 {signer} 签名，不是索引里写的发布者 {key}", entry.id)),
            None => return Err(format!("索引要求 {} 的包由 {key} 签名，但这个包没有签名", entry.id)),
        }
    }
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = r#"{ "plugins": [
        { "id": "word-count", "name": "字数统计", "description": "counts words",
          "versions": [
            { "version": "1.0.0", "url": "https://x/1.0.0.zip", "sha256": "a" },
            { "version": "1.3.0", "url": "https://x/1.3.0.zip", "sha256": "b" },
            { "version": "9.0.0", "url": "https://x/9.0.0.zip", "sha256": "c", "dshr": ">=99" }
          ] },
        { "id": "Bad Id", "versions": [] }
    ] }"#;

    #[test]
    fn parses_searches_and_picks_versions() {
        let entries = parse(INDEX, "https://example.com/index.json").unwrap();
        assert_eq!(entries.len(), 1, "id 不合规的条目跳过");
        assert_eq!(entries[0].index, "https://example.com/index.json");
        // 9.0.0 要更新的 dshr：挑 1.3.0。
        assert_eq!(entries[0].best_version().unwrap().version, "1.3.0");
        assert_eq!(search(&entries, "字数").len(), 1);
        assert_eq!(search(&entries, "WORDS").len(), 1);
        assert!(search(&entries, "nothing").is_empty());
        let installed = vec![("word-count".to_owned(), Some("1.0.0".to_owned())), ("other".to_owned(), Some("1.0.0".to_owned()))];
        assert_eq!(outdated(&entries, &installed), [("word-count".to_owned(), "1.0.0".to_owned(), "1.3.0".to_owned())]);
        assert!(outdated(&entries, &[("word-count".to_owned(), Some("1.3.0".to_owned()))]).is_empty());
    }
}
