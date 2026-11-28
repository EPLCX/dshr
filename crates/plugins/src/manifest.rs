//! 插件清单（`plugin.toml`）与插件目录的扫描。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

/// 清单文件名。可以没有：那样 id 就是文件夹名，入口是 `index.js`，没有任何能力。
pub const MANIFEST_FILE: &str = "plugin.toml";
pub(crate) const DEFAULT_MAIN: &str = "index.js";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// 插件能用的宿主功能。脚本本身什么都碰不到，文件只能通过 `host` 上的函数读写，而这些函数先查这里。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// `fs:workspace:read`：读工作区里的文件、列目录。
    pub fs_read: bool,
    /// `fs:workspace:write`：写工作区里的文件。
    pub fs_write: bool,
    /// `net:<主机>`：可以用 `host.fetch` 访问的主机，小写。每项是一个主机名（`api.example.com`）、
    /// 一个域名下的所有子域（`*.example.com`）或任何主机（`*`）。
    pub net: Vec<String>,
    /// `exec:<程序>`：可以用 `host.exec` 启动的程序名（`git`、`npm`），或任何程序（`*`）。
    pub exec: Vec<String>,
    /// `llm`：可以用 `host.llm.complete` 调用当前会话的模型（花的是用户的 token）。
    pub llm: bool,
    /// `session:read`：可以读当前会话的事件（`host.session`）。
    pub session_read: bool,
    /// `hooks:gate`：`hooks.beforeTool` 可以直接放行工具调用（不声明时只能拒绝或要求询问）。
    pub hooks_gate: bool,
    /// `hooks:tool-output`：`hooks.afterTool` 可以改写工具交给模型的结果。
    pub hooks_tool_output: bool,
    /// `hooks:write`：`hooks.beforeWrite` 可以改写 `write_file`、`edit_file` 要写的内容（只拒绝不需要它）。
    pub hooks_write: bool,
    /// `background`：可以导出 `schedules`、`services`，在后台运行。
    pub background: bool,
    /// `session:control`：替用户发消息、继续 / 停止对话、新建会话（`host.session.send`）。
    pub session_control: bool,
    /// `agent:run`：启动子代理（`host.agent.run`）。
    pub agent_run: bool,
    /// `hooks:review`：`hooks.reviewTool` 在询问用户之前审查工具调用，可以直接放行。
    pub hooks_review: bool,
    /// `context:compact`：`hooks.beforeStep` 可以压缩对话上下文。
    pub context_compact: bool,
    /// `context:rewrite`：`hooks.transformRequest` 可以改写发给模型的消息。
    pub context_rewrite: bool,
    /// `provider`：导出 `providers`，提供模型服务商。
    pub provider: bool,
    /// `oauth`：`host.oauth` 打开浏览器登录第三方账号。
    pub oauth: bool,
    /// `model:route`：`hooks.beforeRequest` 可以为每次请求选择模型和思考等级。
    pub model_route: bool,
    /// `notify`：`host.notify` 发送桌面通知。
    pub notify: bool,
    /// `dshr:config:read` / `dshr:config:write`：读 / 改程序设置。
    pub config_read: bool,
    pub config_write: bool,
    /// `dshr:skills`：管理 Skills。
    pub skills: bool,
    /// `dshr:sessions:read` / `dshr:sessions:write`：读全部会话 / 导入、归档、删除会话。
    pub sessions_read: bool,
    pub sessions_write: bool,
    /// `dshr:mcp`：管理 MCP 服务器。
    pub mcp: bool,
    /// `plugins:install`：下载并安装插件（每次都要用户确认）。
    pub plugins_install: bool,
    /// `permissions:mode`：导出 `permissions`，提供权限模式（选中时由插件决定每次工具调用放不放行）。
    pub permissions_mode: bool,
    /// `permissions:approve`：`hooks.approve` 代用户回答审批（和界面同时等，先答的生效）。
    pub permissions_approve: bool,
    /// 插件自带的 MCP 服务器派生的能力：`mcp:<名字>@<启动参数的哈希>`（见 `bundle.rs`）。不在清单的
    /// `capabilities` 里写，由 `[mcp.servers]` 推出来。
    pub mcp_servers: Vec<String>,
    /// `plugins:call:<id>`：可以调用插件 `<id>` 导出的 `api`（清单里写 `uses = ["<id>"]`）。
    pub plugins_call: Vec<String>,
}

/// 不带参数的能力：清单里的写法 → 对应的字段。顺序就是 [`Capabilities::names`] 的顺序。
type FlagField = fn(&mut Capabilities) -> &mut bool;

const FLAGS: &[(&str, FlagField)] = &[
    ("llm", |c| &mut c.llm),
    ("session:read", |c| &mut c.session_read),
    ("session:control", |c| &mut c.session_control),
    ("agent:run", |c| &mut c.agent_run),
    ("hooks:gate", |c| &mut c.hooks_gate),
    ("hooks:review", |c| &mut c.hooks_review),
    ("hooks:tool-output", |c| &mut c.hooks_tool_output),
    ("hooks:write", |c| &mut c.hooks_write),
    ("context:compact", |c| &mut c.context_compact),
    ("context:rewrite", |c| &mut c.context_rewrite),
    ("provider", |c| &mut c.provider),
    ("oauth", |c| &mut c.oauth),
    ("model:route", |c| &mut c.model_route),
    ("notify", |c| &mut c.notify),
    ("background", |c| &mut c.background),
    ("dshr:config:read", |c| &mut c.config_read),
    ("dshr:config:write", |c| &mut c.config_write),
    ("dshr:skills", |c| &mut c.skills),
    ("dshr:sessions:read", |c| &mut c.sessions_read),
    ("dshr:sessions:write", |c| &mut c.sessions_write),
    ("dshr:mcp", |c| &mut c.mcp),
    ("plugins:install", |c| &mut c.plugins_install),
    ("permissions:mode", |c| &mut c.permissions_mode),
    ("permissions:approve", |c| &mut c.permissions_approve),
];

/// 现在认识的能力，报错时列出来。
fn known() -> String {
    let mut names = vec!["fs:workspace:read", "fs:workspace:write", "net:<主机>", "exec:<程序>"];
    names.extend(FLAGS.iter().map(|(name, _)| *name));
    names.join("、")
}

impl Capabilities {
    pub(crate) fn parse(names: &[String]) -> Result<Self, String> {
        let mut capabilities = Self::default();
        for name in names {
            match name.as_str() {
                "fs:workspace:read" => capabilities.fs_read = true,
                "fs:workspace:write" => capabilities.fs_write = true,
                flag if FLAGS.iter().any(|(name, _)| *name == flag) => {
                    let (_, field) = FLAGS.iter().find(|(name, _)| *name == flag).expect("上面刚找到");
                    *field(&mut capabilities) = true;
                }
                other if other.starts_with("plugins:call:") => {
                    let id = &other["plugins:call:".len()..];
                    if !valid_id(id) {
                        return Err(format!("`{other}` 里的插件 id 不合规"));
                    }
                    if !capabilities.plugins_call.iter().any(|known| known == id) {
                        capabilities.plugins_call.push(id.to_owned());
                    }
                }
                other if other.starts_with("exec:") => {
                    let program = &other["exec:".len()..];
                    if !valid_program(program) {
                        return Err(format!("`exec:{program}` 里的程序不对：写程序名（git、npm），或 `*` 表示任何程序；不带路径和参数"));
                    }
                    if !capabilities.exec.iter().any(|known| known == program) {
                        capabilities.exec.push(program.to_owned());
                    }
                }
                other => match other.strip_prefix("net:").map(str::to_ascii_lowercase) {
                    Some(host) if valid_host_pattern(&host) => capabilities.net.push(host),
                    Some(host) => {
                        return Err(format!(
                            "`net:{host}` 里的主机不对：写主机名（api.example.com）、`*.example.com` 或 `*`，不带协议、端口和路径"
                        ));
                    }
                    None => {
                        return Err(format!("不认识的能力 `{other}`（现在有 {}）", known()));
                    }
                },
            }
        }
        Ok(capabilities)
    }

    /// 写回清单里的写法，按 读、写、各个 `net:`、各个 `exec:`、其余 的顺序。
    pub fn names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.fs_read {
            names.push("fs:workspace:read".to_owned());
        }
        if self.fs_write {
            names.push("fs:workspace:write".to_owned());
        }
        names.extend(self.net.iter().map(|host| format!("net:{host}")));
        names.extend(self.exec.iter().map(|program| format!("exec:{program}")));
        let mut copy = self.clone();
        for (name, field) in FLAGS {
            if *field(&mut copy) {
                names.push((*name).to_owned());
            }
        }
        names.extend(self.mcp_servers.iter().cloned());
        names.extend(self.plugins_call.iter().map(|id| format!("plugins:call:{id}")));
        names
    }

    /// 这个插件能不能“替用户做事”（发消息、起子代理、改宿主数据、装插件）：它的工具按“交给外部”把关。
    pub fn acts_for_user(&self) -> bool {
        self.session_control
            || self.agent_run
            || self.config_write
            || self.sessions_write
            || self.mcp
            || self.skills
            || self.plugins_install
    }

    /// `program`（`host.exec` 的第一个参数）在不在声明过的范围里。只比程序名：带路径的写法（`/usr/bin/git`、
    /// `./x`）只有声明了 `exec:*` 才能用。
    pub fn allows_program(&self, program: &str) -> bool {
        self.exec.iter().any(|allowed| allowed == "*" || allowed == program)
    }

    /// `host`（URL 里的主机，不带端口）在不在声明过的范围里。
    ///
    /// 通配的两种写法（`*`、`*.example.com`）不包括 `localhost` 和 IP 地址：本机和内网的服务要逐个写明，
    /// 免得一句 `net:*` 顺带放开了它们。按名字比较，不解析 DNS——一个解析到内网地址的公网域名拦不住。
    pub fn allows_host(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        let host = host.trim_end_matches('.');
        let local =
            host == "localhost" || host.ends_with(".localhost") || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok();
        self.net.iter().any(|pattern| match pattern.strip_prefix('*') {
            None => pattern == host,
            Some("") => !local,
            Some(suffix) => !local && host.len() > suffix.len() && host.ends_with(suffix),
        })
    }
}

fn valid_program(program: &str) -> bool {
    program == "*"
        || (!program.is_empty()
            && program.len() <= 64
            && !program.starts_with('.')
            && program.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+')))
}

/// 会话 id：字母、数字、`-`、`_`（uuid 或时间戳形式），不能带路径。只有 JS 插件（`host.dshr.sessions`）用。
#[cfg(feature = "js")]
pub(crate) fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// 密钥名：大写字母、数字、`_`，以字母开头。
pub(crate) fn valid_secret_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn valid_host_pattern(pattern: &str) -> bool {
    let name = match pattern.strip_prefix('*') {
        Some("") => return true,
        // `*.example.com`：星号后面必须紧跟一个点。
        Some(rest) => match rest.strip_prefix('.') {
            Some(name) => name,
            None => return false,
        },
        None => pattern,
    };
    // IPv6 地址按 URL 里的写法带方括号：`[::1]`。
    let ipv6 = name.starts_with('[') && name.ends_with(']') && name[1..name.len() - 1].parse::<std::net::Ipv6Addr>().is_ok();
    ipv6 || (!name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.')))
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginManifest {
    pub id: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    /// 插件所在文件夹。
    pub dir: PathBuf,
    /// 入口模块，相对 `dir`，用 `/` 分隔。
    pub main: String,
    pub capabilities: Capabilities,
    /// 单次工具调用的时限；`[plugins.<id>]` 里的 `timeout_ms` 可以改。
    pub timeout: Duration,
    /// 配置的 JSON Schema（清单里的 `[config]`），设置页据此生成表单。
    pub config_schema: Option<Value>,
    /// 插件要用的密钥名（`secrets = ["API_TOKEN"]`）。值由用户填，存在系统钥匙串里（见 `secrets.rs`）。
    pub secrets: Vec<String>,
    /// import map（`[imports]`）：裸模块名 → 插件文件夹里的文件，如 `zod = "vendor/zod.js"`。
    pub imports: std::collections::BTreeMap<String, String>,
    /// 有没有脚本：清单写了 `main`，或者文件夹里有 `index.js`。没有脚本的“纯包”插件不启动 QuickJS。
    pub has_script: bool,
    /// 包内容：自带的 MCP 服务器、Skills、命令、预设（见 `bundle.rs`）。
    pub bundle: crate::bundle::Bundle,
    /// 数据目录（`~/.dshr`）：插件数据目录是 `<数据目录>/plugin-data/<id>/`。扫描插件目录时填上；链接到别处的插件
    /// 也靠它找到数据目录。没有时按插件文件夹的上两级算。
    pub home: Option<PathBuf>,
    /// 要求的宿主版本（`[engines] dshr`，semver 范围写法）。
    pub requires_dshr: Option<String>,
    /// 插件自带的文案（`locales/<语言>.json`），见 `locale.rs`。
    pub messages: crate::locale::Messages,
    /// 找不到界面语言的文案时用哪种语言（清单里的 `default_locale`，默认 `en`）。
    pub default_locale: String,
}

impl PluginManifest {
    /// 插件语言文件里 `key` 在 `locale` 下的文案（按语言回退）。
    pub fn message(&self, locale: &str, key: &str) -> Option<&str> {
        crate::locale::lookup(&self.messages, locale, &self.default_locale, key)
    }

    /// 显示名：语言文件里的 `name` 优先，其次是清单的 `name`。
    pub fn display_name(&self, locale: &str) -> Option<String> {
        self.message(locale, "name").map(str::to_owned).or_else(|| self.name.clone())
    }

    /// 一句话说明：语言文件里的 `description` 优先，其次是清单的 `description`。
    pub fn display_description(&self, locale: &str) -> Option<String> {
        self.message(locale, "description").map(str::to_owned).or_else(|| self.description.clone())
    }
}

/// 这个程序的版本（插件的 `[engines] dshr` 拿它比）。
pub fn host_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| semver::Version::new(0, 0, 0))
}

/// 宿主版本满足 `requirement`（没有要求时总是满足；写错了不满足）。
pub fn host_satisfies(requirement: Option<&str>) -> bool {
    match requirement {
        None => true,
        Some(requirement) => semver::VersionReq::parse(requirement).is_ok_and(|requirement| requirement.matches(&host_version())),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawManifest {
    id: Option<String>,
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    main: Option<String>,
    capabilities: Vec<String>,
    timeout_ms: Option<u64>,
    default_locale: Option<String>,
    config: Option<toml::Table>,
    secrets: Vec<String>,
    imports: std::collections::BTreeMap<String, String>,
    mcp: RawMcp,
    /// 要调用的别的插件（它们导出的 `api`）。
    uses: Vec<String>,
    /// 宿主版本要求：`[engines] dshr = ">=0.2"`。
    engines: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawMcp {
    servers: std::collections::BTreeMap<String, dshr_mcp::ServerConfig>,
}

/// 扫描到的一个插件文件夹：清单读得出来就是 `Ok`，读不出来的也留着（连同原因），
/// 这样它会以“激活失败”出现在设置页里，而不是无声无息地消失。
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub id: String,
    /// 插件所在文件夹。
    pub dir: PathBuf,
    pub manifest: Result<PluginManifest, String>,
}

/// 插件 id：小写字母、数字、`-`、`_`，以字母或数字开头，最长 64。它同时是 `[plugins.<id>]` 的键。
pub fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

/// `dir` 下每个子文件夹是一个插件，按文件夹名排序。既没有清单也没有 `index.js` 的文件夹跳过；
/// 文件夹名不能当 id 用、清单里又没写 id 的也跳过（记一条警告）。
pub fn discover(dir: &Path) -> Vec<Found> {
    let mut folders: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()).collect())
        .unwrap_or_default();
    folders.sort();
    // 链接的文件夹（`dshr plugin link`）排在插件目录里的后面：同 id 时以插件目录里的为准。
    folders.extend(crate::links::load(dir).into_values().filter(|path| path.is_dir()));
    let home = dir.parent().map(Path::to_path_buf);
    let mut found: Vec<Found> = Vec::new();
    for folder in folders {
        let Some(plugin) = found_at(&folder, home.as_deref()) else { continue };
        if found.iter().any(|other| other.id == plugin.id) {
            tracing::warn!(folder = %folder.display(), id = plugin.id, "插件 id 重复，已跳过");
            continue;
        }
        found.push(plugin);
    }
    found
}

/// 读一个文件夹里的插件（和 [`discover`] 里同一套检查）；不是插件文件夹、或者清单无效又没法用文件夹名当 id 时是 `None`。
fn found_at(folder: &Path, home: Option<&Path>) -> Option<Found> {
    if !folder.join(MANIFEST_FILE).is_file() && !folder.join(DEFAULT_MAIN).is_file() {
        return None;
    }
    let folder_name = folder.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let manifest = load(folder, &folder_name).map(|mut manifest| {
        manifest.home = home.map(Path::to_path_buf);
        manifest
    });
    let id = match &manifest {
        Ok(manifest) => manifest.id.clone(),
        Err(_) if valid_id(&folder_name) => folder_name,
        Err(error) => {
            tracing::warn!(folder = %folder.display(), error, "插件清单无效，已跳过");
            return None;
        }
    };
    Some(Found { id, dir: folder.to_owned(), manifest })
}

/// 只找插件 `id`，不把插件目录里所有插件的清单都读一遍（插件多时开关、授权一个插件就不用等全部解析完）。
///
/// `known` 是这个插件上次所在的文件夹：它还在、清单里的 id 也没变，并且插件目录里没有同 id 的文件夹
/// 抢在它前面（链接的文件夹排在插件目录之后），就直接用它。否则看 `<插件目录>/<id>`，
/// 最后退回完整的 [`discover`]（文件夹名和 id 不一致、刚改过链接等少见情况）。
pub fn discover_one(dir: &Path, id: &str, known: Option<&Path>) -> Option<Found> {
    let home = dir.parent().map(Path::to_path_buf);
    let conventional = dir.join(id);
    let candidates = known.into_iter().map(Path::to_path_buf).chain(std::iter::once(conventional.clone()));
    for folder in candidates {
        // 链接的文件夹：插件目录里有同名文件夹时以插件目录里的为准，交给完整扫描去判断。
        let inside = folder.parent() == Some(dir);
        if !inside && conventional.is_dir() {
            continue;
        }
        if let Some(found) = found_at(&folder, home.as_deref()).filter(|found| found.id == id) {
            return Some(found);
        }
    }
    discover(dir).into_iter().find(|found| found.id == id)
}

/// 一个插件文件夹里所有文件的“指纹”（相对路径、大小、修改时间）：变了就说明插件被改过。
/// 不读文件内容；隐藏文件夹和 `node_modules` 不看，文件夹最多往下四层、最多两千个文件。
pub fn fingerprint(dir: &Path) -> u64 {
    use std::hash::{Hash, Hasher};

    const MAX_DEPTH: usize = 4;
    const MAX_FILES: usize = 2000;

    fn walk(dir: &Path, depth: usize, files: &mut Vec<(PathBuf, u64, Option<std::time::SystemTime>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            if files.len() >= MAX_FILES {
                return;
            }
            let (path, name) = (entry.path(), entry.file_name());
            let Ok(metadata) = entry.metadata() else { continue };
            if metadata.is_dir() {
                let name = name.to_string_lossy();
                if depth < MAX_DEPTH && !name.starts_with('.') && name != "node_modules" {
                    walk(&path, depth + 1, files);
                }
            } else {
                files.push((path, metadata.len(), metadata.modified().ok()));
            }
        }
    }

    let mut files = Vec::new();
    walk(dir, 0, &mut files);
    files.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (path, len, modified) in files {
        path.strip_prefix(dir).unwrap_or(&path).hash(&mut hasher);
        len.hash(&mut hasher);
        modified.hash(&mut hasher);
    }
    hasher.finish()
}

pub(crate) fn load(dir: &Path, folder_name: &str) -> Result<PluginManifest, String> {
    let path = dir.join(MANIFEST_FILE);
    let raw: RawManifest = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|error| format!("{MANIFEST_FILE} 格式不对：{error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => RawManifest::default(),
        Err(error) => return Err(format!("读不了 {MANIFEST_FILE}：{error}")),
    };
    let id = raw.id.unwrap_or_else(|| folder_name.to_owned());
    let has_script = raw.main.is_some() || dir.join(DEFAULT_MAIN).is_file();
    if !valid_id(&id) {
        return Err(format!("插件 id `{id}` 不合规：只用小写字母、数字、`-`、`_`，以字母或数字开头"));
    }
    let main = raw.main.unwrap_or_else(|| DEFAULT_MAIN.to_owned()).replace('\\', "/");
    if main.is_empty() || main.starts_with('/') || main.contains(':') || main.split('/').any(|part| part == "..") {
        return Err(format!("入口 `{main}` 应当是插件文件夹里的相对路径"));
    }
    let config_schema = match raw.config {
        Some(table) => {
            let mut schema = serde_json::to_value(table).map_err(|error| format!("[config] 转不成 JSON：{error}"))?;
            if let Some(object) = schema.as_object_mut() {
                object.entry("type").or_insert_with(|| Value::String("object".into()));
            }
            Some(schema)
        }
        None => None,
    };
    for name in &raw.secrets {
        if !valid_secret_name(name) {
            return Err(format!("密钥名 `{name}` 不合规：只用大写字母、数字、`_`，以字母开头"));
        }
    }
    let mut imports = std::collections::BTreeMap::new();
    for (specifier, target) in raw.imports {
        let target = target.replace('\\', "/");
        let target = target.strip_prefix("./").unwrap_or(&target).to_owned();
        if specifier.is_empty() || specifier.starts_with('.') || specifier.starts_with('/') {
            return Err(format!("[imports] 里的 `{specifier}` 应当是裸模块名（如 zod），相对路径直接 import 就行"));
        }
        if target.is_empty() || target.starts_with('/') || target.contains(':') || target.split('/').any(|part| part == "..") {
            return Err(format!("[imports] 里 `{specifier}` 指向的 `{target}` 应当是插件文件夹里的相对路径"));
        }
        imports.insert(specifier, target);
    }
    let bundle = crate::bundle::load(dir, raw.mcp.servers, &raw.secrets)?;
    let messages = crate::locale::load(dir)?;
    let default_locale = raw.default_locale.unwrap_or_else(|| crate::locale::DEFAULT_LOCALE.to_owned());
    if !crate::locale::valid_code(&default_locale) {
        return Err(format!("default_locale = \"{default_locale}\" 不是语言代码（如 en、zh-CN）"));
    }
    for key in raw.engines.keys() {
        if key != "dshr" {
            return Err(format!("[engines] 里只认 dshr，不认识 {key}"));
        }
    }
    let requires_dshr = raw.engines.get("dshr").cloned();
    if let Some(requirement) = &requires_dshr {
        semver::VersionReq::parse(requirement).map_err(|error| format!("[engines] dshr = \"{requirement}\" 写错了：{error}"))?;
    }
    let mut capabilities = Capabilities::parse(&raw.capabilities)?;
    capabilities.mcp_servers = bundle.capabilities();
    for used in &raw.uses {
        if !valid_id(used) {
            return Err(format!("uses 里的插件 id `{used}` 不合规"));
        }
        if *used == id {
            return Err("uses 里不能写插件自己".into());
        }
        if !capabilities.plugins_call.contains(used) {
            capabilities.plugins_call.push(used.clone());
        }
    }
    Ok(PluginManifest {
        id,
        name: raw.name,
        version: raw.version,
        description: raw.description,
        dir: dir.to_owned(),
        main,
        capabilities,
        timeout: raw.timeout_ms.map_or(DEFAULT_TIMEOUT, Duration::from_millis),
        config_schema,
        secrets: raw.secrets,
        imports,
        has_script,
        bundle,
        home: None,
        requires_dshr,
        messages,
        default_locale,
    })
}
