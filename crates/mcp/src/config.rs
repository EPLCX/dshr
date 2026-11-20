//! 服务器配置：`[plugins.mcp.servers.<名字>]`，以及插件清单里的 `[mcp.servers.<名字>]`（同一种写法）。
//!
//! ```toml
//! [plugins.mcp.servers.files]          # stdio：启动一个进程
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "D:/docs"]
//! env = { TOKEN = "${secret:TOKEN}" }
//!
//! [plugins.mcp.servers.docs]           # streamable HTTP：连一个地址
//! url = "https://mcp.example.com/mcp"
//! headers = { Authorization = "Bearer ${env:DOCS_TOKEN}" }
//! ```
//!
//! 值里可以写占位符：`${env:NAME}` 是环境变量，`${secret:NAME}` 是存在系统钥匙串里的密钥（见 [`crate::McpPlugin`]）；
//! 插件自带的服务器另有 `${plugin}`（插件文件夹）和 `${data}`（插件数据目录）。`$${` 写出字面的 `${`。

use std::collections::BTreeMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;

/// `[plugins.mcp]`。
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Config {
    /// MCP 服务器，按名字。
    pub servers: BTreeMap<String, ServerConfig>,
}

impl Config {
    /// `[plugins.mcp]` 的内容（`Config::plugin_configs` 里 `mcp` 那一项）；写错了当作没有服务器。
    pub fn from_value(value: serde_json::Value) -> Self {
        serde_json::from_value(value).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct ServerConfig {
    /// stdio 服务器的启动命令。和 `url` 二选一。
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// stdio 服务器的工作目录；不写时继承 dshr 的。
    #[serde(default)]
    pub cwd: Option<String>,
    /// streamable HTTP 服务器的地址。和 `command` 二选一。
    #[serde(default)]
    pub url: Option<String>,
    /// HTTP 服务器的请求头。
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// 写成 `false` 可以暂时停用这个服务器而不删配置。
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// 单次工具调用的时限，默认两分钟。
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// HTTP 服务器的 OAuth 登录：授权服务器不支持动态注册客户端时在这里写 `client_id`（和 `client_secret`）、
    /// 要申请的 `scopes`。不写时按服务器的元数据来。
    #[serde(default)]
    pub oauth: Option<OauthConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(default)]
pub struct OauthConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
}

fn enabled_by_default() -> bool {
    true
}

impl ServerConfig {
    /// stdio 服务器。
    pub fn command(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args,
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            timeout_ms: None,
            oauth: None,
        }
    }

    /// HTTP 服务器。
    pub fn http(url: impl Into<String>) -> Self {
        Self { url: Some(url.into()), ..Self::command("", Vec::new()) }
    }

    /// 给人看的启动方式：命令行，或者地址。
    pub fn describe(&self) -> String {
        match &self.url {
            Some(url) => url.clone(),
            None => std::iter::once(self.command.as_str()).chain(self.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" "),
        }
    }

    /// 检查写法（不换占位符）：`command` 和 `url` 恰好有一个。
    pub fn validate(&self) -> Result<(), String> {
        match (self.command.trim().is_empty(), &self.url) {
            (true, None) => Err("要写 `command`（stdio 服务器）或 `url`（HTTP 服务器）".into()),
            (false, Some(_)) => Err("`command` 和 `url` 只能写一个".into()),
            _ => Ok(()),
        }
    }

    /// 换好占位符的启动方式。`lookup(种类, 名字)` 给出占位符的值。
    pub fn launch(&self, lookup: &dyn Fn(&str, &str) -> Result<String, String>) -> Result<Launch, String> {
        self.validate()?;
        let expand = |text: &str| expand(text, lookup);
        match &self.url {
            Some(url) => Ok(Launch::Http {
                url: expand(url)?,
                headers: self.headers.iter().map(|(name, value)| Ok((name.clone(), expand(value)?))).collect::<Result<_, String>>()?,
            }),
            None => Ok(Launch::Command {
                command: expand(&self.command)?,
                args: self.args.iter().map(|arg| expand(arg)).collect::<Result<_, _>>()?,
                env: self.env.iter().map(|(name, value)| Ok((name.clone(), expand(value)?))).collect::<Result<_, String>>()?,
                cwd: self.cwd.as_deref().map(expand).transpose()?.map(PathBuf::from),
            }),
        }
    }

    /// 配置里用到的占位符：(种类, 名字)。
    pub fn placeholders(&self) -> Vec<(String, String)> {
        let found = std::cell::RefCell::new(Vec::new());
        let texts = std::iter::once(&self.command)
            .chain(&self.args)
            .chain(self.env.values())
            .chain(self.cwd.iter())
            .chain(self.url.iter())
            .chain(self.headers.values());
        for text in texts {
            let _ = expand(text, &|kind, name| {
                found.borrow_mut().push((kind.to_owned(), name.to_owned()));
                Ok(String::new())
            });
        }
        found.into_inner()
    }
}

/// 换好占位符的启动方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    Command { command: String, args: Vec<String>, env: BTreeMap<String, String>, cwd: Option<PathBuf> },
    Http { url: String, headers: Vec<(String, String)> },
}

/// 把 `text` 里的 `${种类:名字}`、`${种类}` 换成 `lookup` 给的值；`$${` 是字面的 `${`。
pub fn expand(text: &str, lookup: &dyn Fn(&str, &str) -> Result<String, String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        if rest[..start].ends_with('$') {
            out.push_str(&rest[..start - 1]);
            out.push_str("${");
            rest = &rest[start + 2..];
            continue;
        }
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| format!("`{text}` 里的 `${{` 没有配对的 `}}`"))?;
        let inner = &after[..end];
        let (kind, name) = inner.split_once(':').unwrap_or((inner, ""));
        out.push_str(&lookup(kind.trim(), name.trim())?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}
