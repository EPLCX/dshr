use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use dshr_agent::{
    Agent, AgentConfig, AgentPlugins, ApprovalRequest, Approver, AutoApprove, Chunk, Decision, DelegatePlugin, InstructionsPlugin,
    LiveEvent, PermissionMode, Preset, Session, TitlePlugin,
};
use dshr_config::credentials::resolve_api_key;
use dshr_config::{Config, ModelRef, Paths, ReasoningEffort};
use dshr_context::Context as Plugins;
use dshr_core::{EventBody, TurnEndReason};
use dshr_mcp::McpPlugin;
use dshr_plugins::{CreatorPlugin, JsPlugins, Scaffold};
use dshr_skills::SkillsPlugin;
use dshr_tools::ToolContext;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

mod dev;
mod dist;

#[derive(Parser)]
#[command(name = "dshr", display_name = "DSHR", version, about = "DSHR 命令行工具：运行代理与管理会话")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 运行一轮对话，输出最终回答后退出。
    Headless {
        /// 发给 agent 的任务。
        prompt: String,
        /// 工作区目录，默认当前目录。
        #[arg(short, long)]
        workspace: Option<PathBuf>,
        /// 继续已有会话。
        #[arg(long)]
        resume: Option<String>,
        /// 完全访问：修改任意文件、运行命令都不再询问。
        #[arg(short, long, conflicts_with_all = ["manual", "auto_review"])]
        yes: bool,
        /// 手动确认：每一次修改、运行命令都先询问。默认是“工作区内修改”。
        #[arg(long, conflicts_with = "auto_review")]
        manual: bool,
        /// 自动审查：让模型判断风险，低风险的直接放行，有风险的再询问。
        #[arg(long)]
        auto_review: bool,
        /// 使用的模型，`provider/model`，默认取配置里的默认模型。
        #[arg(short, long)]
        model: Option<String>,
        /// 思考等级：off / low / high / max，默认取配置（不写则用模型自己的默认值）。
        #[arg(long, value_parser = parse_effort)]
        effort: Option<ReasoningEffort>,
        /// 预设：standard / plan / review / create（创造模式：agent 自己写一个插件）。
        #[arg(long, value_parser = parse_preset, default_value = "standard")]
        preset: Preset,
    },
    /// 列出本地会话。
    Sessions,
    /// 管理 JS 插件（`<数据目录>/plugins/`）。
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// MCP 服务器（`config.toml` 的 `[plugins.mcp.servers]`）。
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// 实际连接一遍配置里的 MCP 服务器，列出状态、工具、资源和 prompts。
    List,
    /// 设置服务器配置里 `${secret:NAME}` 用到的密钥：值从标准输入读一行，存进系统钥匙串。
    Secret { server: String, name: String },
    /// 登录一个要求 OAuth 的远程（HTTP）服务器：打印授权地址并用浏览器打开，token 存进系统钥匙串。
    Login { server: String },
    /// 退出登录：删掉存着的 token。
    Logout { server: String },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// 生成一个新插件的骨架：`plugin.toml` 和带示例工具的 `index.js`。
    New {
        /// 插件 id，也是文件夹名：小写字母、数字、`-`、`_`。
        id: String,
        /// 显示名，默认等于 id。
        #[arg(long)]
        name: Option<String>,
        /// 一句话说明。
        #[arg(long)]
        description: Option<String>,
        /// 声明能力，可以写多次：fs:workspace:read、fs:workspace:write、net:<主机>、exec:<程序>、llm、
        /// session:read、hooks:gate、hooks:tool-output、background。
        #[arg(long = "cap", value_name = "能力")]
        capabilities: Vec<String>,
        /// 放在哪个目录下，默认是插件目录。
        #[arg(long)]
        dir: Option<PathBuf>,
        /// 同时生成插件包的示例：skills/、commands/，以及清单里注释掉的 [mcp.servers]。
        #[arg(long)]
        bundle: bool,
    },
    /// 把插件目录之外的一个文件夹当作插件加载（开发用，不拷贝）：记在 plugin-links.json 里，桌面版热重载照样盯着它。
    Link {
        folder: PathBuf,
        /// 清单里没写 id 时用的 id（默认是文件夹名）。
        #[arg(long)]
        id: Option<String>,
    },
    /// 取消链接（不动文件夹本身）。
    Unlink { id: String },
    /// 插件调试台：盯着插件的文件，每次改动后重新加载并打印报告；可以直接调用工具、画页面。参数是插件文件夹
    /// （没链接过就先链接）或插件 id，默认当前目录。
    Dev { target: Option<String> },
    /// 列出插件目录里的插件，并实际加载一遍，显示能不能用。
    List,
    /// 允许一个插件使用它清单里声明的全部能力（文件、网络）。没允许过的插件不激活。
    Grant { id: String },
    /// 收回一个插件的全部授权。
    Revoke { id: String },
    /// 安装插件：本地文件夹、.zip、.tar.gz，或 http(s) 地址（GitHub 仓库地址会换成源码包）。装之前列出它要的权限并问你。
    Install {
        source: String,
        /// 压缩包的 SHA-256，对不上就不装。
        #[arg(long)]
        sha256: Option<String>,
        /// 要装的插件 id（清单里没写 id 时也用它当文件夹名）。
        #[arg(long)]
        id: Option<String>,
        /// 不问，直接装（声明的能力仍然要另外允许）。
        #[arg(long)]
        yes: bool,
        /// 发布者的签名和之前装的对不上（或者之前签了名、这次没签）也装。
        #[arg(long)]
        force: bool,
    },
    /// 打包成 `<id>-<版本>.dshrplugin`（zip）：先实际加载一遍，加载不了的不打包；`--sign` 用 `dshr plugin keygen` 的密钥签名。
    Pack {
        /// 插件文件夹，默认当前目录。
        folder: Option<PathBuf>,
        /// 输出文件，默认在插件文件夹旁边。
        #[arg(short, long)]
        out: Option<PathBuf>,
        #[arg(long)]
        sign: bool,
    },
    /// 生成签名密钥（私钥存进系统钥匙串，打印公钥）；已经有了就打印现有的公钥。
    Keygen {
        /// 换一把新的（旧的作废，用旧钥匙签的插件的用户更新时会看到“发布者变了”）。
        #[arg(long)]
        replace: bool,
    },
    /// 在插件索引（config.toml 的 [plugin_index]）里找插件。
    Search { query: String },
    /// 列出索引里有新版本的已装插件。
    Outdated,
    /// 按索引更新插件（同样列出权限并确认）。
    Update {
        id: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        force: bool,
    },
    /// 回滚到升级前归档的旧版本（每个插件留最近 3 个）。
    Rollback {
        id: String,
        /// 要回到的版本，默认最近的一个。
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// 卸载插件：删掉它的文件夹、收回授权。
    Uninstall { id: String },
    /// 启用插件（写进 config.toml）。
    Enable { id: String },
    /// 停用插件（写进 config.toml）。
    Disable { id: String },
    /// 管理插件的密钥（清单里 `secrets` 声明的名字），存在系统钥匙串里。
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
}

#[derive(Subcommand)]
enum SecretCommand {
    /// 列出插件声明的密钥和它们有没有设置（不显示值）。
    List { id: String },
    /// 设置一个密钥：值从标准输入读一行，不出现在命令行里。
    Set { id: String, name: String },
    /// 删除一个密钥。
    Delete { id: String, name: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // TLS 统一用 aws-lc-rs 后端，先装成进程级默认值（`ureq`、`wss://` 都要它）。
    dshr_config::tls::install_crypto_provider();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_env("DSHR_LOG")).with_writer(std::io::stderr).init();

    let paths = Paths::from_env()?;
    match Cli::parse().command {
        Command::Headless { prompt, workspace, resume, yes, manual, auto_review, model, effort, preset } => {
            let permission = if yes {
                PermissionMode::FullAccess
            } else if manual {
                PermissionMode::Manual
            } else if auto_review {
                PermissionMode::AutoReview
            } else {
                PermissionMode::Workspace
            };
            headless(&paths, prompt, RunOptions { workspace, resume, permission, model, effort, preset }).await
        }
        Command::Sessions => list_sessions(&paths),
        Command::Plugin { command: PluginCommand::New { id, name, description, capabilities, dir, bundle } } => {
            let scaffold = Scaffold { id, name, description, capabilities, bundle };
            let parent = dir.unwrap_or_else(|| paths.plugins_dir());
            let dir = scaffold.create(&parent)?;
            // 能力是这条命令里亲手写的，就当已经允许过。之后清单里再加的能力仍然要允许一次。
            if !scaffold.capabilities.is_empty() && parent == paths.plugins_dir() {
                let names = dshr_plugins::discover(&parent)
                    .into_iter()
                    .find(|found| found.id == scaffold.id)
                    .and_then(|found| found.manifest.ok())
                    .map(|manifest| manifest.capabilities.names())
                    .unwrap_or_default();
                dshr_plugins::grant_capabilities(&dshr_plugins::grants_path(&parent), &scaffold.id, &names).context("写不了授权记录")?;
            }
            println!("已生成 {}", dir.display());
            println!("  {}  清单：id、能力、配置项", dir.join(dshr_plugins::MANIFEST_FILE).display());
            println!("  {}  入口：导出 tools（还可以有 prompt、events、hooks、cards、ui、schedules……）", dir.join("index.js").display());
            println!("改完用 `dshr plugin list` 检查能不能加载；桌面版开着时保存即生效。");
            Ok(())
        }
        Command::Plugin { command: PluginCommand::List } => list_plugins(&paths),
        Command::Plugin { command: PluginCommand::Link { folder, id } } => {
            let id = dshr_plugins::links::link(&paths.plugins_dir(), &folder, id.as_deref()).map_err(anyhow::Error::msg)?;
            println!("已链接 {id} → {}。桌面版开着时马上装上；声明的能力要用 `dshr plugin grant {id}` 允许。", folder.display());
            Ok(())
        }
        Command::Plugin { command: PluginCommand::Unlink { id } } => {
            let folder = dshr_plugins::links::unlink(&paths.plugins_dir(), &id).map_err(anyhow::Error::msg)?;
            println!("已取消链接 {id}（{} 没有动）", folder.display());
            Ok(())
        }
        Command::Plugin { command: PluginCommand::Dev { target } } => dev::run(&paths, target).await,
        Command::Plugin { command: PluginCommand::Grant { id } } => {
            let scripts = JsPlugins::discover(&paths.plugins_dir());
            let request = scripts.grant_requests().into_iter().find(|request| request.id == id);
            scripts.grant(&id).map_err(anyhow::Error::msg)?;
            match request {
                Some(request) => println!("已允许 {id}：{}", request.missing.join("、")),
                None => println!("{id} 声明的能力都已经允许过了"),
            }
            Ok(())
        }
        Command::Plugin { command: PluginCommand::Revoke { id } } => {
            JsPlugins::discover(&paths.plugins_dir()).revoke(&id).map_err(anyhow::Error::msg)?;
            println!("已收回 {id} 的授权；它声明了能力的话，下次加载时不会激活");
            Ok(())
        }
        Command::Plugin { command: PluginCommand::Secret { command } } => plugin_secret(&paths, command),
        Command::Plugin { command: PluginCommand::Install { source, sha256, id, yes, force } } => {
            dist::install(&paths, &source, sha256.as_deref(), id.as_deref(), yes, force).await
        }
        Command::Plugin { command: PluginCommand::Pack { folder, out, sign } } => dist::pack(folder, out, sign),
        Command::Plugin { command: PluginCommand::Keygen { replace } } => {
            let key = dshr_plugins::pack::keygen(replace).map_err(anyhow::Error::msg)?;
            println!("签名公钥：{key}");
            println!("把它写进插件索引的 publisher.key，用户就能确认包是你签的。私钥在系统钥匙串里（条目 plugin-signing）。");
            Ok(())
        }
        Command::Plugin { command: PluginCommand::Search { query } } => dist::search(&paths, &query).await,
        Command::Plugin { command: PluginCommand::Outdated } => dist::outdated(&paths).await,
        Command::Plugin { command: PluginCommand::Update { id, all, yes, force } } => {
            dist::update(&paths, id.as_deref(), all, yes, force).await
        }
        Command::Plugin { command: PluginCommand::Rollback { id, to, yes } } => dist::rollback(&paths, &id, to.as_deref(), yes),
        Command::Plugin { command: PluginCommand::Uninstall { id } } => {
            dshr_plugins::install::uninstall(&paths.plugins_dir(), &id).map_err(anyhow::Error::msg)?;
            println!("已卸载 {id}");
            Ok(())
        }
        Command::Mcp { command: McpCommand::List } => list_mcp(&paths).await,
        Command::Mcp { command: McpCommand::Login { server } } => {
            let mcp = configured_mcp(&paths)?;
            let open = |url: &str| -> Result<(), String> {
                println!("在浏览器里打开这个地址登录（最多等 5 分钟）：\n{url}");
                let _ = open_in_browser(url);
                Ok(())
            };
            mcp.servers().login(&server, &open).await.map_err(anyhow::Error::msg)?;
            println!("已登录 {server}");
            Ok(())
        }
        Command::Mcp { command: McpCommand::Logout { server } } => {
            configured_mcp(&paths)?.servers().logout(&server).map_err(anyhow::Error::msg)?;
            println!("已退出 {server} 的登录");
            Ok(())
        }
        Command::Mcp { command: McpCommand::Secret { server, name } } => {
            eprint!("{server} 的 {name}：");
            let mut value = String::new();
            std::io::stdin().read_line(&mut value).context("读不了标准输入")?;
            let value = value.trim_end_matches(['\r', '\n']);
            if value.is_empty() {
                anyhow::bail!("值是空的，没有保存");
            }
            dshr_mcp::save_secret(&server, &name, value).map_err(anyhow::Error::msg)?;
            println!("已保存 {server} 的 {name}");
            Ok(())
        }
        Command::Plugin { command: PluginCommand::Enable { id } } => set_plugin_enabled(&paths, &id, true),
        Command::Plugin { command: PluginCommand::Disable { id } } => set_plugin_enabled(&paths, &id, false),
    }
}

fn set_plugin_enabled(paths: &Paths, id: &str, enabled: bool) -> anyhow::Result<()> {
    let mut config = Config::load(paths)?;
    config.set_plugin_enabled(id, enabled);
    config.save(paths)?;
    println!("{id} 已{}（桌面版里 JS 插件立即生效，内置插件重启后生效）", if enabled { "启用" } else { "停用" });
    Ok(())
}

/// 命令行版的会话控制：插件发来的消息排在这一轮后面依次执行；不能新建会话。
struct HeadlessSessions {
    session: String,
    queue: Arc<std::sync::Mutex<std::collections::VecDeque<(String, String)>>>,
    cancel: CancellationToken,
}

impl dshr_plugins::app::SessionControl for HeadlessSessions {
    fn send(&self, request: dshr_plugins::app::SendRequest) -> Result<String, String> {
        match &request.target {
            dshr_plugins::app::SessionTarget::Current => {}
            dshr_plugins::app::SessionTarget::Id(id) if *id == self.session => {}
            _ => return Err("命令行版只能发到当前会话".into()),
        }
        self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push_back((request.plugin, request.text));
        Ok(self.session.clone())
    }

    fn stop(&self, _session: Option<String>) -> Result<(), String> {
        self.cancel.cancel();
        Ok(())
    }

    fn list(&self) -> Vec<dshr_plugins::app::OpenSession> {
        vec![dshr_plugins::app::OpenSession {
            id: self.session.clone(),
            title: String::new(),
            workspace: None,
            running: true,
            current: true,
        }]
    }
}

/// 插件替用户发的消息，命令行版一次运行里最多跟着跑几轮（防止插件之间互相触发停不下来）。
const MAX_PLUGIN_FOLLOW_UPS: usize = 10;

/// 插件 `id` 清单里声明的密钥名；插件不存在或清单读不出来时报错。
fn declared_secrets(paths: &Paths, id: &str) -> anyhow::Result<Vec<String>> {
    let found = dshr_plugins::discover(&paths.plugins_dir())
        .into_iter()
        .find(|found| found.id == id)
        .with_context(|| format!("插件目录里没有 {id}"))?;
    let manifest = found.manifest.map_err(anyhow::Error::msg)?;
    Ok(manifest.secrets)
}

fn plugin_secret(paths: &Paths, command: SecretCommand) -> anyhow::Result<()> {
    match command {
        SecretCommand::List { id } => {
            let declared = declared_secrets(paths, &id)?;
            if declared.is_empty() {
                println!("{id} 没有声明密钥");
            }
            for name in declared {
                let state = if dshr_plugins::secrets::load(&id, &name).is_some() { "已设置" } else { "未设置" };
                println!("{name}\t{state}\t（也可以用环境变量 {}）", dshr_plugins::secrets::env_name(&id, &name));
            }
            Ok(())
        }
        SecretCommand::Set { id, name } => {
            if !declared_secrets(paths, &id)?.contains(&name) {
                anyhow::bail!("{id} 的 plugin.toml 没有在 secrets 里声明 {name}");
            }
            eprint!("{name} 的值：");
            let mut value = String::new();
            std::io::stdin().read_line(&mut value).context("读不了标准输入")?;
            let value = value.trim_end_matches(['\r', '\n']);
            if value.is_empty() {
                anyhow::bail!("值是空的，没有保存");
            }
            dshr_plugins::secrets::save(&id, &name, value).map_err(anyhow::Error::msg)?;
            println!("已保存 {id} 的 {name}");
            Ok(())
        }
        SecretCommand::Delete { id, name } => {
            dshr_plugins::secrets::delete(&id, &name).map_err(anyhow::Error::msg)?;
            println!("已删除 {id} 的 {name}");
            Ok(())
        }
    }
}

/// 配置里的 MCP 服务器（还没连接）。
fn configured_mcp(paths: &Paths) -> anyhow::Result<McpPlugin> {
    let config = Config::load(paths)?;
    let servers = config
        .plugin_configs()
        .into_iter()
        .find(|(id, _)| id == "mcp")
        .map(|(_, value)| dshr_mcp::Config::from_value(value).servers)
        .unwrap_or_default();
    let mcp = McpPlugin::new();
    mcp.servers().configure(servers);
    Ok(mcp)
}

/// 用系统浏览器打开地址（打不开也没关系：地址已经打印出来了）。
fn open_in_browser(url: &str) -> std::io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program).arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().map(|_| ())
}

/// `dshr mcp list`：和 `headless` 同样连接一遍，列出每个服务器的状态。
async fn list_mcp(paths: &Paths) -> anyhow::Result<()> {
    let config = Config::load(paths)?;
    let servers: std::collections::BTreeMap<String, dshr_mcp::ServerConfig> = config
        .plugin_configs()
        .into_iter()
        .find(|(id, _)| id == "mcp")
        .map(|(_, value)| dshr_mcp::Config::from_value(value).servers)
        .unwrap_or_default();
    if servers.is_empty() {
        println!("没有配置 MCP 服务器（在 config.toml 的 [plugins.mcp.servers.<名字>] 里写）");
        return Ok(());
    }
    let mcp = McpPlugin::new();
    mcp.servers().configure(servers.clone());
    let tools = mcp.servers().tools().await;
    for status in mcp.status() {
        let state = match status.state {
            dshr_mcp::ServerState::Ready => "已连接".to_owned(),
            dshr_mcp::ServerState::Disabled => "已停用".to_owned(),
            dshr_mcp::ServerState::Failed | dshr_mcp::ServerState::Stopped => {
                format!("失败：{}", status.error.as_deref().unwrap_or_default())
            }
            dshr_mcp::ServerState::Idle | dshr_mcp::ServerState::Connecting => "未连接".to_owned(),
            dshr_mcp::ServerState::NeedsLogin => format!("需要登录（`dshr mcp login {}`）", status.name),
        };
        println!("{}  {state}", status.name);
        if let Some(server) = servers.get(&status.name) {
            println!("  {}", server.describe());
        }
        let prefix = dshr_mcp::tool_name(&status.name, "");
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).filter(|name| name.starts_with(&prefix)).collect();
        if !names.is_empty() {
            println!("  工具：{}", names.join("、"));
        }
        if status.resources {
            println!("  提供资源（模型用 mcp_list_resources / mcp_read_resource 读）");
        }
        if status.prompts > 0 {
            println!("  prompts：{} 个（桌面版输入框里 /mcp:{}:<名字>）", status.prompts, status.name);
        }
        for line in mcp.servers().logs(&status.name).iter().rev().take(5).rev() {
            println!("  | {line}");
        }
    }
    Ok(())
}

fn parse_preset(value: &str) -> Result<Preset, String> {
    match value {
        "standard" => Ok(Preset::Standard),
        "plan" => Ok(Preset::Plan),
        "review" => Ok(Preset::Review),
        "create" => Ok(Preset::Create),
        other => Err(format!("应为 standard / plan / review / create：{other}")),
    }
}

fn parse_effort(value: &str) -> Result<ReasoningEffort, String> {
    ReasoningEffort::ALL.into_iter().find(|effort| effort.as_str() == value).ok_or_else(|| format!("应为 off / low / high / max：{value}"))
}

/// 命令行版装配的插件。
fn plugins(config: &Config, paths: &Paths) -> anyhow::Result<Plugins> {
    plugins_with_scripts(config, paths).map(|(plugins, _)| plugins)
}

/// 同 [`plugins`]，另外交回插件目录里的 JS 插件（`dshr plugin dev` 要重载、看报告）。
fn plugins_with_scripts(config: &Config, paths: &Paths) -> anyhow::Result<(Plugins, JsPlugins)> {
    // MCP 服务器只启动一份，主代理和子代理共用。
    let mcp = McpPlugin::new();
    // 插件目录里的 JS 插件：每个只加载一次，主代理和子代理共用同一个运行时。
    let scripts = JsPlugins::discover(&paths.plugins_dir());
    // 主代理和子代理共用的内置部分。
    let builtin = || -> anyhow::Result<Plugins> {
        let plugins = Plugins::new();
        // `[plugins.<id>]`：各插件的配置，以及 `enabled = false`。
        plugins.configure(config.plugin_configs());
        plugins.add_group(dshr_llm::BuiltinProviders)?;
        plugins.add_group(dshr_tools::BuiltinTools)?;
        plugins.add_group(AgentPlugins)?;
        plugins.add(InstructionsPlugin::new(&paths.home))?;
        plugins.add(SkillsPlugin::new(&paths.home))?;
        plugins.add(mcp.clone())?;
        Ok(plugins)
    };
    // 子代理的插件里没有 delegate，所以不能再委派。
    let subagent = builtin()?;
    subagent.add_group(scripts.clone())?;
    let plugins = builtin()?;
    plugins.add(TitlePlugin)?;
    plugins.add(DelegatePlugin::new(subagent))?;
    // 创造模式（`--preset create`）：agent 自己写插件。
    plugins.add(CreatorPlugin::new(scripts.clone()))?;
    // JS 插件放在最后：此前已有的工具名（含 delegate）它们都不能占用。
    plugins.add_group(scripts.clone())?;
    Ok((plugins, scripts))
}

/// 无界面运行一轮时，命令行上给的选项。
struct RunOptions {
    workspace: Option<PathBuf>,
    resume: Option<String>,
    permission: PermissionMode,
    model: Option<String>,
    effort: Option<ReasoningEffort>,
    preset: Preset,
}

async fn headless(paths: &Paths, prompt: String, options: RunOptions) -> anyhow::Result<()> {
    let RunOptions { workspace, resume, permission, model, effort, preset } = options;
    let config = Config::load(paths)?.with_env();
    let reference = match model {
        Some(model) => ModelRef::parse(&model).with_context(|| format!("模型格式应为 provider/model：{model}"))?,
        None => config.default_model().context("没有可用的模型，请先在配置里添加提供商")?,
    };
    let provider = config.provider(&reference.provider).with_context(|| format!("未配置提供商 {}", reference.provider))?;
    let api_key = resolve_api_key(&provider).map(|(key, _)| key);
    let plugins = plugins(&config, paths)?;
    let http = dshr_llm::HttpClient::default();
    let plugins_snapshot = plugins.snapshot();
    let llm = dshr_llm::connect(&plugins_snapshot, &http, &provider, api_key).with_context(|| match provider.env_key {
        Some(env) => format!("{} 未配置 API Key：在桌面版设置里保存，或设置环境变量 {env}", provider.name),
        None => format!("无法连接 {}", provider.name),
    })?;

    let workspace = match workspace {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    // 不用 canonicalize：Windows 上会得到 `\\?\` 前缀路径，cmd 不接受它作为工作目录。
    let workspace = std::path::absolute(&workspace)?;
    if !workspace.is_dir() {
        bail!("工作区不存在: {}", workspace.display());
    }

    let mut session = match resume {
        Some(id) => Session::open(&id, paths.session_log(&id)).with_context(|| format!("无法打开会话 {id}"))?,
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            Session::create(&id, paths.session_log(&id))?
        }
    };

    // 完全访问不会用到审批；其余模式都可能询问。
    let approver: Arc<dyn Approver> = if permission == PermissionMode::FullAccess {
        Arc::new(AutoApprove)
    } else if std::io::stdin().is_terminal() {
        Arc::new(TerminalApprover::default())
    } else {
        bail!("标准输入不是终端，无法询问审批；请加 --yes（完全访问）");
    };
    let agent = Agent::new(
        llm,
        plugins,
        approver,
        ToolContext::new(workspace),
        AgentConfig {
            model: reference.model.clone(),
            effort: effort.or(config.model.effort),
            context_window: config.model.context_window,
            ..AgentConfig::default()
        }
        .with(permission)
        .with(preset),
    );

    let cancel = CancellationToken::new();
    let ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n[正在中断…]");
            ctrl_c.cancel();
        }
    });

    // 插件能用的命令行版服务：替用户发消息（排在这一轮后面）、通知写到 stderr、登录地址打印出来。
    let follow_ups = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    let sessions = Arc::new(HeadlessSessions { session: session.id().to_owned(), queue: follow_ups.clone(), cancel: cancel.clone() });
    dshr_plugins::app::set_app_services(|services| {
        services.session = Some(sessions.clone());
        services.notifier = Some(Arc::new(|notification: &dshr_plugins::app::Notification| {
            eprintln!("\n[通知 · {}] {} {}", notification.plugin, notification.title, notification.body);
        }));
        services.open_url = Some(Arc::new(|url: &str| {
            eprintln!("\n[请在浏览器里打开] {url}");
            Ok(())
        }));
        // 插件的 `host.llm.complete` 指定别的模型时：按配置连上那个提供商。
        let (config, snapshot, http) = (config.clone(), plugins_snapshot.clone(), http.clone());
        services.models = Some(Arc::new(move |spec: &str| {
            let reference = ModelRef::parse(spec)?;
            let provider = config.provider(&reference.provider)?;
            let api_key = resolve_api_key(&provider).map(|(key, _)| key);
            let llm = dshr_llm::connect(&snapshot, &http, &provider, api_key).ok()?;
            Some(dshr_agent::LlmAccess { provider: llm, model: reference.model })
        }));
    });

    let (tx, rx) = mpsc::unbounded_channel();
    let printer = tokio::spawn(print_events(rx));
    let had_title = session.has_title();
    let mut outcome = agent.run_turn(&mut session, prompt, &tx, &cancel).await;
    for _ in 0..MAX_PLUGIN_FOLLOW_UPS {
        let next = follow_ups.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).pop_front();
        let Some((plugin, text)) = next else { break };
        if cancel.is_cancelled() || outcome.as_ref().is_ok_and(|outcome| outcome.reason == TurnEndReason::Error) {
            break;
        }
        eprintln!("\n[由插件 {plugin} 发送] {text}");
        outcome = agent.run_turn(&mut session, text, &tx, &cancel).await;
    }
    drop(tx);
    printer.await?;
    let outcome = outcome?;

    // 只显示退出前已写入的标题，不等后台请求。
    session.refresh_title();
    match session.title().filter(|_| !had_title) {
        Some(title) => eprintln!("\n[会话 {}「{title}」，{:?}]", session.id(), outcome.reason),
        None => eprintln!("\n[会话 {}，{:?}]", session.id(), outcome.reason),
    }
    match outcome.reason {
        TurnEndReason::Completed => Ok(()),
        TurnEndReason::Error => bail!(outcome.error.unwrap_or_default()),
        reason => bail!("本轮未完成: {reason:?}"),
    }
}

/// 流式文本写到 stdout，工具调用等过程信息写到 stderr。
///
/// 请求中途失败会整条重试，已经流式打出的半段回复收不回来：
/// - stdout 是终端时照常流式显示，重试时在 stderr 注明上面那段作废；
/// - stdout 被重定向（管道、文件，给脚本用）时不流式输出，每条回复写完整后从日志事件里取文字输出一次，
///   结果里不会混进作废的半段。
async fn print_events(rx: mpsc::UnboundedReceiver<LiveEvent>) {
    let stream = std::io::stdout().is_terminal();
    print_events_to(rx, std::io::stdout(), stream).await;
}

async fn print_events_to(mut rx: mpsc::UnboundedReceiver<LiveEvent>, mut stdout: impl Write, stream: bool) {
    // 这一步已经流式打出过文字（重试时要说明作废）。
    let mut streamed = false;
    while let Some(event) = rx.recv().await {
        match event {
            LiveEvent::Chunk(Chunk::Text(text)) if stream => {
                streamed |= !text.is_empty();
                let _ = write!(stdout, "{text}");
                let _ = stdout.flush();
            }
            LiveEvent::Chunk(Chunk::Text(_) | Chunk::Thinking(_)) => {}
            // 命令的实时输出直接转到 stderr，和工具调用提示放在一起。
            LiveEvent::Chunk(Chunk::ToolOutput { text, .. }) => eprint!("{text}"),
            LiveEvent::Durable(event) => match &event.body {
                EventBody::AssistantMessage { content, .. } => {
                    streamed = false;
                    if !stream {
                        let text = dshr_agent::assistant_text(content);
                        if !text.is_empty() {
                            let _ = write!(stdout, "{text}");
                            let _ = stdout.flush();
                        }
                    }
                }
                EventBody::ToolCall { name, input, .. } => eprintln!("\n[调用 {name} {input}]"),
                EventBody::ToolResult { is_error: true, output, .. } => {
                    eprintln!("[工具出错] {}", output.lines().next().unwrap_or_default())
                }
                EventBody::AssistantAttempt { attempt, error } => {
                    if std::mem::take(&mut streamed) {
                        eprintln!("\n[第 {attempt} 次请求失败，上面这段没写完的回复作废，重新生成] {error}");
                    } else {
                        eprintln!("[第 {attempt} 次请求失败] {error}");
                    }
                }
                _ => {}
            },
        }
    }
    let _ = writeln!(stdout);
}

/// 在终端逐条询问。互斥锁保证并发调用时提示不交错。
#[derive(Default)]
struct TerminalApprover {
    prompt: Mutex<()>,
}

#[async_trait::async_trait]
impl Approver for TerminalApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        let _guard = self.prompt.lock().await;
        // 必须由用户亲自确认的（授予插件能力、修改已授权的插件）说明来自宿主，不是自动审查的理由。
        let note = match (&request.note, request.mandatory) {
            (Some(note), true) => format!("\n[必须确认] {note}"),
            (None, true) => "\n[必须确认] 这个操作只能由你亲自决定。".to_owned(),
            (Some(note), false) => format!("\n[自动审查] {note}"),
            (None, false) => String::new(),
        };
        let question = format!("{note}\n允许执行 {} {}？[y/N] ", request.tool, request.input);
        let answer = tokio::task::spawn_blocking(move || {
            eprint!("{question}");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).map(|_| line)
        })
        .await;
        match answer {
            Ok(Ok(line)) if matches!(line.trim(), "y" | "Y" | "yes") => Decision::Allow,
            _ => Decision::Deny,
        }
    }
}

/// 插件目录里每个插件一段：id、版本、状态、能力、文件夹。状态来自和 `headless` 一样的装配，
/// 所以和真正运行时看到的一致（包括工具名被占用、`[plugins.<id>]` 里停用了）。
fn list_plugins(paths: &Paths) -> anyhow::Result<()> {
    use dshr_context::Status;

    let dir = paths.plugins_dir();
    let found = dshr_plugins::discover(&dir);
    if found.is_empty() {
        println!("插件目录里还没有插件（{}）；用 `dshr plugin new <id>` 生成一个。", dir.display());
        return Ok(());
    }
    let config = Config::load(paths)?;
    let statuses = plugins(&config, paths)?.describe();
    let requests = JsPlugins::discover(&dir).grant_requests();
    for plugin in found {
        let status = match statuses.iter().find(|info| info.id == plugin.id).map(|info| &info.status) {
            Some(Status::Active) => "可用".to_owned(),
            Some(Status::Disabled) => "已停用（config.toml 的 [plugins.<id>] 里 enabled = false）".to_owned(),
            Some(Status::Pending(needs)) => format!("等待 {}", needs.join("、")),
            Some(Status::Failed(error)) => format!("激活失败：{error}"),
            None => "没有装上：id 和内置插件重名".to_owned(),
        };
        let version = plugin.manifest.as_ref().ok().and_then(|manifest| manifest.version.as_deref());
        match version {
            Some(version) => println!("{} {version}  {status}", plugin.id),
            None => println!("{}  {status}", plugin.id),
        }
        if let Ok(manifest) = &plugin.manifest {
            let names = manifest.capabilities.names();
            if !names.is_empty() {
                println!("  能力：{}", names.join("、"));
            }
            if let Some(request) = requests.iter().find(|request| request.id == plugin.id) {
                println!("  还没允许：{}（`dshr plugin grant {}`）", request.missing.join("、"), plugin.id);
            }
            let bundle = &manifest.bundle;
            for (name, server) in &bundle.mcp_servers {
                println!("  MCP 服务器 {name}：{}（在沙箱外以你的权限运行）", server.describe());
            }
            if !bundle.commands.is_empty() {
                let names: Vec<String> = bundle.commands.iter().map(|command| format!("/{}:{}", plugin.id, command.name)).collect();
                println!("  命令：{}", names.join("、"));
            }
            if !bundle.presets.is_empty() {
                let names: Vec<&str> = bundle.presets.iter().map(|preset| preset.label.as_str()).collect();
                println!("  预设：{}", names.join("、"));
            }
            if let Some(dir) = &bundle.skills {
                println!("  Skills：{}", dir.display());
            }
            if !manifest.has_script && bundle.is_empty() {
                println!("  （没有 index.js，也没有 skills/、commands/、presets/ 或 [mcp.servers]）");
            }
            let unset: Vec<&str> = manifest
                .secrets
                .iter()
                .filter(|name| dshr_plugins::secrets::load(&plugin.id, name).is_none())
                .map(String::as_str)
                .collect();
            if !unset.is_empty() {
                println!("  密钥未设置：{}（`dshr plugin secret set {} <名字>`）", unset.join("、"), plugin.id);
            }
        }
        println!("  {}", plugin.dir.display());
    }
    Ok(())
}

fn list_sessions(paths: &Paths) -> anyhow::Result<()> {
    let sessions = dshr_agent::list_sessions(&paths.sessions_dir());
    if sessions.is_empty() {
        println!("还没有会话（{}）", paths.sessions_dir().display());
    }
    for session in sessions {
        println!("{}  {}", session.id, session.title);
    }
    Ok(())
}

#[cfg(test)]
mod print_tests {
    use std::sync::Arc;

    use dshr_core::{ContentBlock, SessionEvent};

    use super::*;

    fn durable(body: EventBody) -> LiveEvent {
        LiveEvent::Durable(Arc::new(SessionEvent { seq: 0, time: 0, body }))
    }

    /// 一次请求流到一半失败、重试成功：重定向时 stdout 里只有最终那份回复。
    #[tokio::test]
    async fn redirected_output_has_no_discarded_partial_reply() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(LiveEvent::Chunk(Chunk::Text("半截".into()))).unwrap();
        tx.send(durable(EventBody::AssistantAttempt { attempt: 1, error: "连接中断".into() })).unwrap();
        tx.send(LiveEvent::Chunk(Chunk::Text("完整的回复".into()))).unwrap();
        tx.send(durable(EventBody::AssistantMessage {
            model: "m".into(),
            content: vec![ContentBlock::text("完整的回复")],
            stop_reason: None,
            usage: None,
        }))
        .unwrap();
        drop(tx);
        let mut out = Vec::new();
        print_events_to(rx, &mut out, false).await;
        assert_eq!(String::from_utf8(out).unwrap(), "完整的回复\n");
    }
}
