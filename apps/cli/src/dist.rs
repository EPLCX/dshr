//! 分发相关的命令：打包、签名密钥、按索引搜索与安装、查更新、更新、回滚。

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use dshr_config::{Config, Paths};
use dshr_plugins::index::IndexEntry;
use dshr_plugins::install::{CommitOptions, Staged};

/// 列出要装的插件、问用户，同意后装上。`options` 决定发布者变了时装不装、记不记索引。
pub fn confirm_and_commit(paths: &Paths, staged: Staged, yes: bool, options: CommitOptions) -> anyhow::Result<()> {
    let dir = paths.plugins_dir();
    let request = staged.request(&dir, None);
    println!(
        "插件：{}{}",
        request.name.as_deref().unwrap_or(&request.id),
        request.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default()
    );
    println!("  id：{}", request.id);
    println!("  来源：{}", request.source);
    if let Some(description) = &request.description {
        println!("  说明：{description}");
    }
    match &staged.signer {
        Some(signer) => println!("  签名：{signer}"),
        None => println!("  签名：没有（无法确认发布者）"),
    }
    if let Some(sha256) = &staged.sha256 {
        println!("  SHA-256：{sha256}");
    }
    if request.replaces {
        println!("  会替换已经装着的同名插件（旧版本归档起来，可以 `dshr plugin rollback {}`）", request.id);
    }
    if request.capabilities.is_empty() {
        println!("  权限：不需要特殊权限");
    } else {
        println!("  权限（装好后还要用 `dshr plugin grant {}` 允许）：", request.id);
        for capability in &request.capabilities {
            println!("    · {capability}");
        }
    }
    // 自带的 MCP 服务器会在沙箱外运行：把实际要启动的命令写出来。
    for (name, server) in &staged.manifest.bundle.mcp_servers {
        println!("  自带 MCP 服务器 {name}：{}（在沙箱外以你的权限运行）", server.describe());
    }
    if let Some(requirement) = &staged.manifest.requires_dshr
        && !dshr_plugins::host_satisfies(Some(requirement))
    {
        println!("  注意：它需要 DSHR {requirement}，现在是 {}，装上也不会激活", dshr_plugins::host_version());
    }
    if options.force {
        println!("  --force：发布者变了也装");
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("标准输入不是终端，无法确认；确认没问题的话加 --yes");
        }
        eprint!("安装？[y/N] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("没有安装");
            return Ok(());
        }
    }
    let installed = staged.commit_with(&dir, options).map_err(anyhow::Error::msg)?;
    println!("已安装到 {}", installed.display());
    Ok(())
}

/// `dshr plugin install <来源>`：文件夹、压缩包、网址，或者索引里的插件 id。
pub async fn install(paths: &Paths, source: &str, sha256: Option<&str>, id: Option<&str>, yes: bool, force: bool) -> anyhow::Result<()> {
    let local = Path::new(source);
    let looks_like_id = !local.exists() && !source.contains("://") && dshr_plugins::valid_id(source);
    if looks_like_id {
        let entry = find_in_index(paths, source).await?;
        let version =
            entry.best_version().with_context(|| format!("索引里的 {source} 没有适合 dshr {} 的版本", dshr_plugins::host_version()))?;
        println!("从索引 {} 装 {source} {}", entry.index, version.version);
        let staged = dshr_plugins::index::stage(&entry, version).await.map_err(anyhow::Error::msg)?;
        return confirm_and_commit(paths, staged, yes, CommitOptions { force, index: Some(entry.index.clone()) });
    }
    let staged = if local.exists() {
        if local.is_file() {
            let bytes = std::fs::read(local)?;
            dshr_plugins::install::stage_bytes(&bytes, source, source, id, sha256)
        } else {
            dshr_plugins::install::stage_local(local, id)
        }
    } else {
        let bytes = dshr_plugins::install::download(source).await.map_err(anyhow::Error::msg)?;
        dshr_plugins::install::stage_bytes(&bytes, &dshr_plugins::install::download_url(source), source, id, sha256)
    }
    .map_err(anyhow::Error::msg)?;
    confirm_and_commit(paths, staged, yes, CommitOptions { force, index: None })
}

async fn load_index(paths: &Paths) -> anyhow::Result<Vec<IndexEntry>> {
    let config = Config::load(paths)?;
    if config.plugin_index.urls.is_empty() {
        bail!("没有配置插件索引：在 config.toml 里写 [plugin_index] urls = [\"https://…/index.json\"]");
    }
    let (entries, problems) = dshr_plugins::index::fetch(&config.plugin_index.urls).await;
    for problem in problems {
        eprintln!("取不到索引：{problem}");
    }
    Ok(entries)
}

async fn find_in_index(paths: &Paths, id: &str) -> anyhow::Result<IndexEntry> {
    load_index(paths).await?.into_iter().find(|entry| entry.id == id).with_context(|| format!("索引里没有 {id}"))
}

/// `dshr plugin search <词>`。
pub async fn search(paths: &Paths, query: &str) -> anyhow::Result<()> {
    let entries = load_index(paths).await?;
    let found = dshr_plugins::index::search(&entries, query);
    if found.is_empty() {
        println!("索引里没有找到 {query}");
    }
    for entry in found {
        let version = entry.best_version().map(|version| version.version.clone()).unwrap_or_else(|| "（没有适合当前 dshr 的版本）".into());
        println!("{} {version}  {}", entry.id, entry.name.as_deref().unwrap_or_default());
        if let Some(description) = &entry.description {
            println!("  {description}");
        }
        if let Some(publisher) = &entry.publisher {
            println!("  发布者：{}{}", publisher.name.as_deref().unwrap_or("?"), if publisher.key.is_some() { "（签名）" } else { "" });
        }
    }
    Ok(())
}

/// 装着的插件和它们的版本。
fn installed(paths: &Paths) -> Vec<(String, Option<String>)> {
    dshr_plugins::discover(&paths.plugins_dir())
        .into_iter()
        .map(|found| (found.id, found.manifest.ok().and_then(|manifest| manifest.version)))
        .collect()
}

/// `dshr plugin outdated`。
pub async fn outdated(paths: &Paths) -> anyhow::Result<()> {
    let entries = load_index(paths).await?;
    let stale = dshr_plugins::index::outdated(&entries, &installed(paths));
    if stale.is_empty() {
        println!("索引里的插件都是最新的");
    }
    for (id, current, latest) in stale {
        println!("{id}  {current} → {latest}（`dshr plugin update {id}`）");
    }
    Ok(())
}

/// `dshr plugin update [<id> | --all]`。
pub async fn update(paths: &Paths, id: Option<&str>, all: bool, yes: bool, force: bool) -> anyhow::Result<()> {
    if id.is_none() && !all {
        bail!("写要更新的插件 id，或者 --all");
    }
    let entries = load_index(paths).await?;
    let stale = dshr_plugins::index::outdated(&entries, &installed(paths));
    let targets: Vec<_> = stale.into_iter().filter(|(stale, _, _)| id.is_none_or(|id| id == stale)).collect();
    if targets.is_empty() {
        println!(
            "{}",
            if all {
                "没有要更新的插件".to_owned()
            } else {
                format!("{} 已经是最新的（或者不在索引里）", id.unwrap_or_default())
            }
        );
        return Ok(());
    }
    for (id, current, latest) in targets {
        println!("\n更新 {id}：{current} → {latest}");
        let entry = entries.iter().find(|entry| entry.id == id).expect("outdated 里的插件在索引里");
        let version = entry.best_version().expect("outdated 里的插件有可用版本");
        let staged = dshr_plugins::index::stage(entry, version).await.map_err(anyhow::Error::msg)?;
        confirm_and_commit(paths, staged, yes, CommitOptions { force, index: Some(entry.index.clone()) })?;
    }
    Ok(())
}

/// `dshr plugin rollback <id> [--to <版本>]`。
pub fn rollback(paths: &Paths, id: &str, to: Option<&str>, yes: bool) -> anyhow::Result<()> {
    let dir = paths.plugins_dir();
    let archived = dshr_plugins::sources::rollback_source(&dir, id, to).map_err(anyhow::Error::msg)?;
    println!("回滚 {id} 到 {}（{}）", archived.version, archived.dir.display());
    let mut staged = dshr_plugins::install::stage_local(&archived.dir, Some(id)).map_err(anyhow::Error::msg)?;
    staged.source = format!("rollback:{}", archived.version);
    // 回滚是装回自己装过的版本：发布者检查不拦（旧版本当初装的时候已经检查过）。
    confirm_and_commit(paths, staged, yes, CommitOptions { force: true, index: None })
}

/// `dshr plugin pack [<文件夹>] [-o <文件>] [--sign]`。
pub fn pack(folder: Option<PathBuf>, out: Option<PathBuf>, sign: bool) -> anyhow::Result<()> {
    let folder = std::path::absolute(folder.unwrap_or_else(|| PathBuf::from(".")))?;
    let manifest = dshr_plugins::load_manifest(&folder).map_err(anyhow::Error::msg)?;
    // 先把脚本实际加载一遍：加载不了的不打包。
    if manifest.has_script {
        check_loads(&folder, &manifest.id)?;
    }
    let key = if sign {
        Some(dshr_plugins::pack::signing_key().map_err(anyhow::Error::msg)?.context("还没有签名密钥：先运行 `dshr plugin keygen`")?)
    } else {
        None
    };
    let packed = dshr_plugins::pack::pack(&folder, out.as_deref(), key.as_ref()).map_err(anyhow::Error::msg)?;
    println!("已打包 {}（{} 个文件）", packed.path.display(), packed.files);
    println!("  id：{}  版本：{}", packed.id, packed.version.as_deref().unwrap_or("（清单里没写 version）"));
    println!("  SHA-256：{}", packed.sha256);
    match &packed.signer {
        Some(signer) => println!("  签名：{signer}"),
        None => println!("  没有签名（加 --sign 签名）"),
    }
    Ok(())
}

/// 在一个临时数据目录里把插件实际加载一遍（执行顶层代码、检查导出），不碰真正的插件目录。
fn check_loads(folder: &Path, id: &str) -> anyhow::Result<()> {
    let home = tempfile_dir()?;
    let plugins = home.join("plugins");
    std::fs::create_dir_all(&plugins)?;
    dshr_plugins::links::link(&plugins, folder, None).map_err(anyhow::Error::msg)?;
    // 能力当作已经允许过：这里只检查代码能不能加载。
    if let Some(found) = dshr_plugins::discover(&plugins).into_iter().find(|found| found.id == id)
        && let Ok(manifest) = &found.manifest
    {
        dshr_plugins::grant_capabilities(&dshr_plugins::grants_path(&plugins), id, &manifest.capabilities.names())?;
    }
    let scripts = dshr_plugins::JsPlugins::discover(&plugins);
    let cx = dshr_context::Context::new();
    cx.add_group(dshr_tools::BuiltinTools)?;
    cx.add_group(scripts)?;
    let status = cx.status(id);
    let _ = std::fs::remove_dir_all(&home);
    match status {
        Some(dshr_context::Status::Failed(reason)) => bail!("{id} 加载不了，没有打包：{reason}"),
        _ => Ok(()),
    }
}

fn tempfile_dir() -> anyhow::Result<PathBuf> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_nanos()).unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("dshr-pack-check-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
