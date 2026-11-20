//! MCP：把外部 MCP 服务器提供的工具、资源和 prompts 接进来。
//!
//! 服务器写在 `config.toml` 里（写法见 [`config`]），可以是一条启动命令（stdio）或一个地址（streamable HTTP）。
//! 每个服务器各有状态（见 [`servers`]）：第一次要工具时连接，之后复用；断开了在后台重连；服务器说工具列表变了
//! 就重新取。改了配置不用重启：新增的连、删掉的断、改了的重连、没变的不动。
//!
//! 工具在模型那边叫 `mcp__<服务器>__<工具>`。它们在 dshr 之外运行、不受命令沙箱约束，所以“工作区内修改”模式下
//! 每次调用都会询问。服务器提供资源时，模型多两个工具 `mcp_list_resources`、`mcp_read_resource`；
//! 服务器的 prompts 出现在输入框的 `/` 菜单里（`/mcp:<服务器>:<prompt>`）。

mod client;
pub mod config;
mod http;
pub mod oauth;
pub mod servers;
mod surface;
pub mod transport;

use std::sync::Arc;

use dshr_agent::UiSurfaces;
use dshr_context::{Manifest, Plugin, Scope};
use dshr_tools::ToolSources;

pub use client::{
    CallResult, Client, ImageContent, McpError, Notification, PromptArgument, PromptInfo, ResourceContent, ResourceInfo, ResourceTemplate,
    ToolInfo,
};
pub use config::{Config, Launch, ServerConfig};
pub use servers::{
    Connection, Connector, LogBuffer, MAX_ATTEMPTS, ProcessConnector, Resolver, ServerSet, ServerState, ServerStatus, get_prompt_text,
    list_all_resources, read_resource_text, tool_name,
};

/// `config.toml` 里配置的服务器。主代理和子代理的插件容器共用同一个实例（克隆它），服务器只启动一份。
#[derive(Clone)]
pub struct McpPlugin {
    servers: ServerSet,
}

impl Default for McpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl McpPlugin {
    pub fn new() -> Self {
        Self::with_connector(Arc::new(ProcessConnector))
    }

    /// 换一种建立连接的方式（测试里接内存假服务器）。
    pub fn with_connector(connector: Arc<dyn Connector>) -> Self {
        let servers = ServerSet::with_connector(None, connector);
        servers.set_resolver(Arc::new(|server, kind, name| match kind {
            "env" => std::env::var(name).map_err(|_| format!("环境变量 {name} 没有设置")),
            "secret" => load_secret(server, name).ok_or_else(|| {
                format!(
                    "密钥 {name} 没有设置：运行 `dshr mcp secret {server} {name}` 存进系统钥匙串，或设置环境变量 {}",
                    secret_env(server, name)
                )
            }),
            other => Err(format!("不认识的占位符 `${{{other}}}`（这里能用 env、secret）")),
        }));
        Self { servers }
    }

    /// 在后台先把服务器连上、取到工具列表：第一轮对话的第一步就不用等它们（`npx` 类的服务器常要几秒）。
    /// 没有启用的服务器时什么也不做。要在之后跑对话的那个 tokio 运行时上调用：子进程和读写它的任务都挂在
    /// 这个运行时上。
    pub async fn prewarm(&self) {
        self.servers.prewarm().await;
    }

    /// 这组服务器（状态、日志、手动重连）。
    pub fn servers(&self) -> &ServerSet {
        &self.servers
    }

    pub fn status(&self) -> Vec<ServerStatus> {
        self.servers.status()
    }
}

impl Plugin for McpPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("mcp").config::<Config>()
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        let Config { servers } = cx.config();
        self.servers.configure(servers);
        cx.contribute::<ToolSources>(Arc::new(self.servers.clone()));
        cx.contribute::<UiSurfaces>(Arc::new(surface::PromptSurface));
        cx.contribute::<dshr_agent::MentionSources>(Arc::new(surface::ResourceMentions));
        Ok(())
    }
}

/// `config.toml` 里的服务器的密钥 `name` 在钥匙串里的条目名。
fn secret_account(server: &str, name: &str) -> String {
    format!("mcp:{server}:{name}")
}

/// 没有钥匙串条目时读的环境变量：`DSHR_MCP_SECRET_<服务器>_<名字>`（服务器名大写，别的字符换成 `_`）。
pub fn secret_env(server: &str, name: &str) -> String {
    let server: String = server.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    format!("DSHR_MCP_SECRET_{server}_{name}")
}

/// 服务器的密钥：钥匙串 → 环境变量。
pub fn load_secret(server: &str, name: &str) -> Option<String> {
    let non_empty = |value: String| Some(value).filter(|value| !value.is_empty());
    dshr_config::credentials::load_secret(&secret_account(server, name))
        .ok()
        .flatten()
        .and_then(non_empty)
        .or_else(|| std::env::var(secret_env(server, name)).ok().and_then(non_empty))
}

/// 保存服务器的密钥（设置页）。
pub fn save_secret(server: &str, name: &str, value: &str) -> Result<(), String> {
    dshr_config::credentials::save_secret(&secret_account(server, name), value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests;
