//! 插件包：插件文件夹里除了脚本之外，还可以带上这些东西（都不需要写代码）：
//!
//! - `[mcp.servers.<名字>]`（清单里）：插件自带的 MCP 服务器，写法和 `config.toml` 的 `[plugins.mcp.servers]` 一样，
//!   另外能用 `${plugin}`（插件文件夹）、`${data}`（插件数据目录）、`${secret:NAME}`（清单 `secrets` 里声明的密钥）。
//!   每个服务器派生一项能力 `mcp:<名字>@<启动参数的哈希>`：启动参数一变就要再允许一次。
//! - `skills/<名字>/SKILL.md`：插件自带的 Skills，在系统提示词里叫 `<插件>:<名字>`。
//! - `commands/<名字>.md`：斜杠命令 `/<插件>:<名字>`。front matter 写 `description`、`argument-hint`、
//!   `mode: insert | send`（默认 insert：放进输入框），正文是提示词模板，`$ARGUMENTS` 换成命令后面的文字。
//! - `presets/<id>.md`：预设。front matter 写 `label`、`description`、`readOnly`、`tools`（`[read_file, grep]`），
//!   正文是这个预设追加到系统提示词后面的说明。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::Digest as _;

/// 每个插件最多带多少个命令、预设、MCP 服务器；单个文件多大。
const MAX_COMMANDS: usize = 50;
const MAX_PRESETS: usize = 20;
const MAX_SERVERS: usize = 16;
const MAX_FILE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bundle {
    /// `skills/` 文件夹（有的话）。
    pub skills: Option<PathBuf>,
    pub commands: Vec<BundleCommand>,
    pub presets: Vec<BundlePreset>,
    /// 插件自带的 MCP 服务器，按名字。
    pub mcp_servers: BTreeMap<String, dshr_mcp::ServerConfig>,
}

impl Bundle {
    pub fn is_empty(&self) -> bool {
        self.skills.is_none() && self.commands.is_empty() && self.presets.is_empty() && self.mcp_servers.is_empty()
    }

    /// 自带的 MCP 服务器派生的能力：`mcp:<名字>@<启动参数的 8 位哈希>`。
    pub fn capabilities(&self) -> Vec<String> {
        self.mcp_servers.iter().map(|(name, server)| format!("mcp:{name}@{}", launch_hash(server))).collect()
    }
}

/// 一个 `commands/<名字>.md`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    /// `mode: send`：直接发送；否则放进输入框。
    pub send: bool,
    pub template: String,
}

impl BundleCommand {
    /// 套上参数：`$ARGUMENTS` 换成命令后面的文字；模板里没有 `$ARGUMENTS` 而又给了参数时，接在最后。
    pub fn render(&self, args: &str) -> String {
        let args = args.trim();
        if self.template.contains("$ARGUMENTS") {
            self.template.replace("$ARGUMENTS", args)
        } else if args.is_empty() {
            self.template.clone()
        } else {
            format!("{}\n\n{args}", self.template.trim_end())
        }
    }
}

/// 一个 `presets/<id>.md`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlePreset {
    pub id: String,
    pub label: String,
    pub description: String,
    pub instructions: String,
    pub read_only: bool,
    pub tools: Option<Vec<String>>,
}

/// 启动参数（命令、参数、环境变量、工作目录、地址、请求头）的哈希：授权记的是它，参数一变就要再允许。
pub fn launch_hash(server: &dshr_mcp::ServerConfig) -> String {
    let canonical = serde_json::json!({
        "command": server.command, "args": server.args, "env": server.env, "cwd": server.cwd,
        "url": server.url, "headers": server.headers,
    });
    let digest = sha2::Sha256::digest(canonical.to_string().as_bytes());
    digest.iter().take(4).map(|byte| format!("{byte:02x}")).collect()
}

/// 读插件文件夹里的包内容。`servers` 是清单里的 `[mcp.servers]`，`secrets` 是清单声明的密钥名。
pub(crate) fn load(dir: &Path, servers: BTreeMap<String, dshr_mcp::ServerConfig>, secrets: &[String]) -> Result<Bundle, String> {
    if servers.len() > MAX_SERVERS {
        return Err(format!("[mcp.servers] 最多 {MAX_SERVERS} 个"));
    }
    for (name, server) in &servers {
        if !crate::manifest::valid_id(name) {
            return Err(format!("MCP 服务器名 `{name}` 不合规：只用小写字母、数字、`-`、`_`"));
        }
        server.validate().map_err(|error| format!("MCP 服务器 {name}：{error}"))?;
        for (kind, key) in server.placeholders() {
            match kind.as_str() {
                "plugin" | "data" if key.is_empty() => {}
                "env" if !key.is_empty() => {}
                "secret" if secrets.contains(&key) => {}
                "secret" => {
                    return Err(format!("MCP 服务器 {name} 用了密钥 {key}，但清单的 secrets 里没有声明它"));
                }
                _ => {
                    return Err(format!(
                        "MCP 服务器 {name} 里的占位符 `${{{kind}{}}}` 不认识（能用 ${{plugin}}、${{data}}、${{env:NAME}}、${{secret:NAME}}）",
                        if key.is_empty() { String::new() } else { format!(":{key}") }
                    ));
                }
            }
        }
    }
    let skills = Some(dir.join("skills")).filter(|path| path.is_dir());
    let mut commands = Vec::new();
    for (name, text) in markdown_files(&dir.join("commands"), MAX_COMMANDS)? {
        let (front, body) = front_matter(&text);
        let mode = front.get("mode").map(String::as_str).unwrap_or("insert");
        if !matches!(mode, "insert" | "send") {
            return Err(format!("commands/{name}.md 的 mode 只能是 insert 或 send"));
        }
        commands.push(BundleCommand {
            description: front.get("description").cloned().unwrap_or_default(),
            argument_hint: front.get("argument-hint").cloned(),
            send: mode == "send",
            template: body.trim().to_owned(),
            name,
        });
    }
    let mut presets = Vec::new();
    for (id, text) in markdown_files(&dir.join("presets"), MAX_PRESETS)? {
        let (front, body) = front_matter(&text);
        let read_only = match front.get("readOnly").or_else(|| front.get("read_only")).map(String::as_str) {
            None | Some("false") => false,
            Some("true") => true,
            Some(other) => return Err(format!("presets/{id}.md 的 readOnly 应当是 true 或 false，不是 {other}")),
        };
        let tools = front.get("tools").map(|tools| {
            tools
                .trim_matches(['[', ']'])
                .split(',')
                .map(|tool| tool.trim().trim_matches(['"', '\'']).to_owned())
                .filter(|tool| !tool.is_empty())
                .collect()
        });
        presets.push(BundlePreset {
            label: front.get("label").cloned().unwrap_or_else(|| id.clone()),
            description: front.get("description").cloned().unwrap_or_default(),
            instructions: body.trim().to_owned(),
            read_only,
            tools,
            id,
        });
    }
    Ok(Bundle { skills, commands, presets, mcp_servers: servers })
}

/// 文件夹里的 `<名字>.md`（只看这一层），按名字排序。名字不合规、文件太大时报错。
fn markdown_files(dir: &Path, max: usize) -> Result<Vec<(String, String)>, String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(Vec::new()) };
    let folder = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    files.sort();
    if files.len() > max {
        return Err(format!("{folder}/ 里最多 {max} 个 .md 文件"));
    }
    let mut found = Vec::new();
    for path in files {
        let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        if !crate::manifest::valid_id(&name) {
            return Err(format!("{folder}/{name}.md 的名字不合规：只用小写字母、数字、`-`、`_`"));
        }
        let size = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        if size > MAX_FILE_BYTES {
            return Err(format!("{folder}/{name}.md 超过 {} KB", MAX_FILE_BYTES / 1024));
        }
        let text = std::fs::read_to_string(&path).map_err(|error| format!("读不了 {folder}/{name}.md：{error}"))?;
        found.push((name, text));
    }
    Ok(found)
}

/// 开头 `---` 之间的 `键: 值`（值的引号去掉），和后面的正文。没有 front matter 时整篇是正文。
fn front_matter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut fields = BTreeMap::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix("---").filter(|rest| rest.starts_with('\n') || rest.starts_with("\r\n")) else {
        return (fields, text);
    };
    let Some(end) = rest.find("\n---") else { return (fields, text) };
    for line in rest[..end].lines() {
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            let value = value.strip_prefix('"').and_then(|value| value.strip_suffix('"')).unwrap_or(value);
            fields.insert(key.trim().to_owned(), value.to_owned());
        }
    }
    let body = &rest[end + 4..];
    (fields, body.strip_prefix("\r\n").or_else(|| body.strip_prefix('\n')).unwrap_or(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_and_templates() {
        let (fields, body) = front_matter("---\ndescription: \"Review: strict\"\nmode: send\n---\nCheck $ARGUMENTS\n");
        assert_eq!(fields["description"], "Review: strict");
        assert_eq!(fields["mode"], "send");
        assert_eq!(body, "Check $ARGUMENTS\n");
        assert_eq!(front_matter("no header").1, "no header");

        let command = BundleCommand {
            name: "r".into(),
            description: String::new(),
            argument_hint: None,
            send: false,
            template: "Check $ARGUMENTS now".into(),
        };
        assert_eq!(command.render(" a.rs "), "Check a.rs now");
        let plain = BundleCommand { template: "Summarize.".into(), ..command };
        assert_eq!(plain.render(""), "Summarize.");
        assert_eq!(plain.render("briefly"), "Summarize.\n\nbriefly");
    }

    #[test]
    fn launch_hash_changes_with_arguments() {
        let a = dshr_mcp::ServerConfig::command("node", vec!["a.js".into()]);
        let mut b = a.clone();
        assert_eq!(launch_hash(&a), launch_hash(&b));
        b.args.push("--x".into());
        assert_ne!(launch_hash(&a), launch_hash(&b));
        // 开关不影响授权。
        let mut c = a.clone();
        c.enabled = false;
        assert_eq!(launch_hash(&a), launch_hash(&c));
        assert_eq!(launch_hash(&a).len(), 8);
    }
}
