//! 安装插件：`dshr plugin install`、设置页、有 `plugins:install` 能力的插件（插件市场）共用。
//!
//! 来源可以是本地文件夹、安装包（`dshr plugin pack` 打出的 `.dshrplugin`，以及 `.zip`、`.tar.gz`），或者 http(s) 地址（下载下来的 zip / tar.gz；GitHub 仓库地址
//! `https://github.com/<owner>/<repo>` 换成它默认分支的源码包）。流程：
//!
//! 1. 放进临时的“暂存”文件夹并解压；压缩包里只有一个顶层文件夹时以它为插件文件夹。
//! 2. 读清单（和插件目录里的插件同一套检查），给出 id、名字、版本、声明的能力。
//! 3. **问用户**（[`crate::app::InstallPrompt`]，命令行里是终端提问）。不同意就删掉暂存，什么都不留下。
//! 4. 同意后改名挪进插件目录（同名插件先挪到一边，成功后删掉，失败时挪回来）。热重载随即装上它，
//!    声明的能力仍然要另外允许——装插件不等于授权。

use std::path::{Path, PathBuf};

use crate::app::{InstallRequest, PackageInfo};
use crate::manifest::{self, MANIFEST_FILE, PluginManifest};

/// 压缩包最大、解压后最大、最多多少个文件。
const MAX_ARCHIVE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 200 * 1024 * 1024;
const MAX_FILES: usize = 20_000;

/// 能当作插件安装包装的文件扩展名（不区分大小写）。`.dshrplugin` 就是 zip。
pub const PACKAGE_EXTENSIONS: &[&str] = &[PACKAGE_EXTENSION, "zip", "tar.gz", "tgz"];
/// `dshr plugin pack` 打出的包的扩展名（和 `pack::EXTENSION` 相同；这里不依赖 `js` feature）。
pub const PACKAGE_EXTENSION: &str = "dshrplugin";

/// 放在暂存区、还没装上的插件。丢弃时删掉暂存文件夹。
pub struct Staged {
    /// 暂存区的根（整个删掉）。
    root: PathBuf,
    /// 插件文件夹（`root` 本身或它下面唯一的顶层文件夹）。
    dir: PathBuf,
    pub manifest: PluginManifest,
    pub source: String,
    /// 包签了名时是签名的公钥（已经验过）。
    pub signer: Option<String>,
    /// 压缩包的 SHA-256（从压缩包装时）。
    pub sha256: Option<String>,
    /// 插件文件夹里的文件数和总大小（暂存时统计）。
    pub files: usize,
    pub bytes: u64,
}

/// [`Staged::commit_with`] 的选项。
#[derive(Debug, Clone, Default)]
pub struct CommitOptions {
    /// 发布者变了（或者之前签了名、这次没签）也装：用户明确说了的时候。
    pub force: bool,
    /// 从哪个索引装的（之后据此查更新）。
    pub index: Option<String>,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Staged {
    /// 给用户看的安装请求。
    pub fn request(&self, plugins_dir: &Path, requested_by: Option<&str>) -> InstallRequest {
        let installed = plugins_dir.join(&self.manifest.id);
        let replaces = installed.exists();
        let replaces_version = replaces.then(|| manifest::load(&installed, &self.manifest.id).ok().and_then(|old| old.version)).flatten();
        let bundle = &self.manifest.bundle;
        InstallRequest {
            requested_by: requested_by.map(str::to_owned),
            id: self.manifest.id.clone(),
            name: self.manifest.name.clone(),
            version: self.manifest.version.clone(),
            description: self.manifest.description.clone(),
            source: self.source.clone(),
            capabilities: self.manifest.capabilities.names(),
            replaces,
            replaces_version,
            package: self.sha256.as_ref().map(|sha256| PackageInfo { sha256: Some(sha256.clone()), files: self.files, bytes: self.bytes }),
            publisher: crate::sources::publisher_status(plugins_dir, &self.manifest.id, self.signer.as_deref()),
            has_script: self.manifest.has_script,
            mcp_servers: bundle.mcp_servers.iter().map(|(name, server)| (name.clone(), server.describe())).collect(),
            skills: bundle.skills.is_some(),
            commands: bundle.commands.len(),
            presets: bundle.presets.len(),
            requires_dshr: self.manifest.requires_dshr.clone(),
            compatible: crate::manifest::host_satisfies(self.manifest.requires_dshr.as_deref()),
        }
    }

    /// 挪进插件目录，返回装好的文件夹。发布者的公钥和之前装的对不上时不装（见 [`crate::sources`]）。
    pub fn commit(self, plugins_dir: &Path) -> Result<PathBuf, String> {
        self.commit_with(plugins_dir, CommitOptions::default())
    }

    /// 同 [`Self::commit`]：旧版本归档起来（回滚用），记下来源。
    pub fn commit_with(self, plugins_dir: &Path, options: CommitOptions) -> Result<PathBuf, String> {
        let id = self.manifest.id.clone();
        if !options.force {
            crate::sources::check_publisher(plugins_dir, &id, self.signer.as_deref())?;
        }
        std::fs::create_dir_all(plugins_dir).map_err(|error| format!("建不了插件目录：{error}"))?;
        let target = plugins_dir.join(&id);
        let backup = plugins_dir.join(format!(".{id}.old"));
        let _ = std::fs::remove_dir_all(&backup);
        let replacing = target.exists();
        if replacing {
            std::fs::rename(&target, &backup).map_err(|error| format!("挪不开旧版本：{error}"))?;
        }
        let moved = std::fs::rename(&self.dir, &target).or_else(|_| copy_dir(&self.dir, &target));
        if let Err(error) = moved {
            let _ = std::fs::remove_dir_all(&target);
            if replacing {
                let _ = std::fs::rename(&backup, &target);
            }
            return Err(format!("装不进插件目录：{error}"));
        }
        // 旧版本归档（回滚用）；归档不了就算了，不影响这次安装。
        if replacing && let Err(error) = crate::sources::archive(plugins_dir, &id, &backup) {
            tracing::warn!(plugin = id, error, "旧版本没能归档");
        }
        let _ = std::fs::remove_dir_all(&backup);
        let source = crate::sources::Source {
            source: self.source.clone(),
            version: self.manifest.version.clone(),
            sha256: self.sha256.clone(),
            signer: self.signer.clone(),
            index: options.index,
            installed_at: 0,
        };
        if let Err(error) = crate::sources::record(plugins_dir, &id, source) {
            tracing::warn!(plugin = id, error, "安装来源没能记下");
        }
        Ok(target)
    }
}

/// 卸载：删掉插件文件夹，收回它的授权。
pub fn uninstall(plugins_dir: &Path, id: &str) -> Result<(), String> {
    if !manifest::valid_id(id) {
        return Err(format!("插件 id `{id}` 不合规"));
    }
    let dir = plugins_dir.join(id);
    if !dir.is_dir() {
        return Err(format!("插件目录里没有 {id}"));
    }
    std::fs::remove_dir_all(&dir).map_err(|error| format!("删不掉 {}：{error}", dir.display()))?;
    crate::sources::forget(plugins_dir, id);
    crate::grants::revoke(&crate::grants::grants_path(plugins_dir), id).map_err(|error| error.to_string())
}

/// 从本地文件夹或压缩包暂存。
pub fn stage_local(source: &Path, expected_id: Option<&str>) -> Result<Staged, String> {
    let root = staging_dir()?;
    let result = if source.is_dir() {
        copy_dir(source, &root.join("plugin")).map_err(|error| format!("复制不了 {}：{error}", source.display()))
    } else {
        let bytes = std::fs::read(source).map_err(|error| format!("读不了 {}：{error}", source.display()))?;
        unpack(&bytes, &source.to_string_lossy(), &root.join("plugin"))
    };
    finish(root, result, source.display().to_string(), expected_id)
}

/// 文件名看起来是插件安装包（见 [`PACKAGE_EXTENSIONS`]）。
pub fn is_package_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    PACKAGE_EXTENSIONS.iter().any(|extension| name.ends_with(&format!(".{extension}")))
}

/// 从本地的安装包（`.dshrplugin`、`.zip`、`.tar.gz`）暂存：只接受文件；扩展名不对、内容也不是 zip / gzip 的不装。
pub fn stage_package(path: &Path) -> Result<Staged, String> {
    if !path.is_file() {
        return Err(format!("{} 不是文件", path.display()));
    }
    let size = std::fs::metadata(path).map_err(|error| format!("读不了 {}：{error}", path.display()))?.len();
    if size > MAX_ARCHIVE_BYTES {
        return Err(format!("安装包太大（上限 {} MB）", MAX_ARCHIVE_BYTES / 1024 / 1024));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("读不了 {}：{error}", path.display()))?;
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let zip = bytes.starts_with(b"PK\x03\x04");
    let gzip = bytes.starts_with(&[0x1f, 0x8b]);
    if !(zip || gzip) {
        return Err(if is_package_name(&name) {
            format!("{name} 已损坏：不是有效的 zip 或 tar.gz")
        } else {
            format!("{name} 不是插件安装包（支持 .{}、.zip、.tar.gz）", PACKAGE_EXTENSION)
        });
    }
    // 按内容决定怎么解：扩展名是 .dshrplugin 的 gzip 包也照样能装。
    let hint = if zip { format!("{name}.zip") } else { format!("{name}.tar.gz") };
    stage_bytes(&bytes, &hint, &path.display().to_string(), None, None)
}

/// 从下载下来的压缩包暂存（下载由调用方做：插件里走 `net:` 检查过的客户端）。
pub fn stage_bytes(bytes: &[u8], name_hint: &str, source: &str, expected_id: Option<&str>, sha256: Option<&str>) -> Result<Staged, String> {
    use sha2::Digest as _;
    let actual: String = sha2::Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    if let Some(expected) = sha256
        && !actual.eq_ignore_ascii_case(expected.trim())
    {
        return Err(format!("SHA-256 对不上：应为 {expected}，实际是 {actual}"));
    }
    let root = staging_dir()?;
    let result = unpack(bytes, name_hint, &root.join("plugin"));
    let mut staged = finish(root, result, source.to_owned(), expected_id)?;
    staged.sha256 = Some(actual);
    Ok(staged)
}

/// 命令行里由用户自己发起的下载（不经插件的 `net:` 检查，用户亲手给的地址）。最多 50 MB。
#[cfg(feature = "js")]
pub async fn download(url: &str) -> Result<Vec<u8>, String> {
    let response = reqwest::Client::builder()
        .user_agent("dshr")
        .build()
        .map_err(|error| error.to_string())?
        .get(download_url(url))
        .send()
        .await
        .map_err(|error| format!("下载失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("下载失败：HTTP {}", response.status().as_u16()));
    }
    let bytes = response.bytes().await.map_err(|error| format!("下载失败：{error}"))?;
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(format!("压缩包太大（上限 {} MB）", MAX_ARCHIVE_BYTES / 1024 / 1024));
    }
    Ok(bytes.to_vec())
}

#[cfg(not(feature = "js"))]
pub async fn download(_url: &str) -> Result<Vec<u8>, String> {
    Err("这个构建不能下载插件；先下载到本地再安装".into())
}

/// 安装来源换成实际下载的地址：GitHub 仓库地址换成默认分支的源码包。
pub fn download_url(source: &str) -> String {
    let trimmed = source.trim_end_matches('/').trim_end_matches(".git");
    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 2 {
            return format!("https://codeload.github.com/{}/{}/tar.gz/HEAD", parts[0], parts[1]);
        }
    }
    source.to_owned()
}

fn finish(root: PathBuf, unpacked: Result<(), String>, source: String, expected_id: Option<&str>) -> Result<Staged, String> {
    let cleanup = |error: String| {
        let _ = std::fs::remove_dir_all(&root);
        error
    };
    unpacked.map_err(cleanup)?;
    let dir = plugin_root(&root.join("plugin")).map_err(cleanup)?;
    let folder = dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    // 文件夹名多半是 `repo-main` 这种：没有清单 id 时用 expected_id。
    let fallback = expected_id.map(str::to_owned).unwrap_or(folder);
    let manifest = manifest::load(&dir, &fallback).map_err(cleanup)?;
    if let Some(expected) = expected_id
        && manifest.id != expected
    {
        return Err(cleanup(format!("插件 id 是 {}，不是要装的 {expected}", manifest.id)));
    }
    // 没有脚本也行（只带 Skills、命令、MCP 服务器的“纯包”插件），但总得有点东西。
    if manifest.has_script && !dir.join(&manifest.main).is_file() {
        return Err(cleanup(format!("没有入口文件 {}", manifest.main)));
    }
    if !manifest.has_script && manifest.bundle.is_empty() {
        return Err(cleanup(format!("没有入口文件 {}，也没有 skills/、commands/、presets/ 或 [mcp.servers]", manifest.main)));
    }
    // 包签了名就验：签名对不上的不装。
    #[cfg(feature = "js")]
    let signer = crate::pack::verify(&dir).map_err(cleanup)?;
    #[cfg(not(feature = "js"))]
    let signer = None;
    let (files, bytes) = measure(&dir);
    Ok(Staged { root, dir, manifest, source, signer, sha256: None, files, bytes })
}

/// 插件文件夹：有清单或 `index.js` 的那一层；顶层只有一个文件夹时进去找。
fn plugin_root(dir: &Path) -> Result<PathBuf, String> {
    let is_plugin = |dir: &Path| dir.join(MANIFEST_FILE).is_file() || dir.join(manifest::DEFAULT_MAIN).is_file();
    if is_plugin(dir) {
        return Ok(dir.to_owned());
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(dir).map_err(|error| error.to_string())?.flatten().map(|entry| entry.path()).collect();
    match entries.as_slice() {
        [only] if only.is_dir() && is_plugin(only) => Ok(only.clone()),
        _ => Err(format!("这里面没有插件（找不到 {MANIFEST_FILE} 或 index.js）")),
    }
}

/// 文件夹里的文件数和总大小（不跟符号链接）。
fn measure(dir: &Path) -> (usize, u64) {
    let (mut files, mut bytes) = (0usize, 0u64);
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(entry.path()),
                Ok(kind) if kind.is_file() => {
                    files += 1;
                    bytes += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                }
                _ => {}
            }
        }
    }
    (files, bytes)
}

fn staging_dir() -> Result<PathBuf, String> {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    let name: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let dir = std::env::temp_dir().join(format!("dshr-plugin-install-{name}"));
    std::fs::create_dir_all(&dir).map_err(|error| format!("建不了暂存文件夹：{error}"))?;
    Ok(dir)
}

/// 解压 zip 或 tar.gz；条目路径不能跑出 `target`（`..`、绝对路径），不解出符号链接。
fn unpack(bytes: &[u8], name_hint: &str, target: &Path) -> Result<(), String> {
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(format!("压缩包太大（上限 {} MB）", MAX_ARCHIVE_BYTES / 1024 / 1024));
    }
    std::fs::create_dir_all(target).map_err(|error| error.to_string())?;
    let zip = bytes.starts_with(b"PK\x03\x04") || name_hint.ends_with(".zip");
    let (mut files, mut total) = (0usize, 0u64);
    let mut budget = |size: u64| -> Result<(), String> {
        files += 1;
        total += size;
        if files > MAX_FILES || total > MAX_UNPACKED_BYTES { Err("压缩包解压后太大".into()) } else { Ok(()) }
    };
    if zip {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|error| format!("不是有效的 zip：{error}"))?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
            let Some(relative) = entry.enclosed_name() else { continue };
            let path = target.join(relative);
            if entry.is_dir() {
                std::fs::create_dir_all(&path).map_err(|error| error.to_string())?;
                continue;
            }
            if entry.is_symlink() {
                continue;
            }
            budget(entry.size())?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            let mut file = std::fs::File::create(&path).map_err(|error| error.to_string())?;
            std::io::copy(&mut entry, &mut file).map_err(|error| error.to_string())?;
        }
    } else {
        let decoder = flate2::read::GzDecoder::new(bytes);
        let mut archive = tar::Archive::new(decoder);
        for entry in archive.entries().map_err(|error| format!("不是有效的 tar.gz：{error}"))? {
            let mut entry = entry.map_err(|error| format!("不是有效的 tar.gz：{error}"))?;
            let kind = entry.header().entry_type();
            if !(kind.is_file() || kind.is_dir()) {
                continue;
            }
            let relative = entry.path().map_err(|error| error.to_string())?.into_owned();
            if relative.components().any(|part| !matches!(part, std::path::Component::Normal(_) | std::path::Component::CurDir)) {
                continue;
            }
            let path = target.join(&relative);
            if kind.is_dir() {
                std::fs::create_dir_all(&path).map_err(|error| error.to_string())?;
                continue;
            }
            budget(entry.size())?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            let mut file = std::fs::File::create(&path).map_err(|error| error.to_string())?;
            std::io::copy(&mut entry, &mut file).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let (path, file_type) = (entry.path(), entry.file_type()?);
        let name = entry.file_name();
        // 不复制版本控制和依赖目录、不跟符号链接。
        if name == ".git" || name == "node_modules" || file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            copy_dir(&path, &to.join(&name))?;
        } else {
            std::fs::copy(&path, to.join(&name))?;
        }
    }
    Ok(())
}
