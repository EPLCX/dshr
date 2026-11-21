//! 创造模式（`Preset::Create`）：agent 按用户的描述自己写一个 JS 插件，加载、试用、改到能用。
//! 对应原版的 Creator 预设。
//!
//! 这个插件提供五个工具，只在创造模式下出现在请求里，其余模式下调用也会被拒绝：
//!
//! - `plugin_write`：写插件文件夹里的一个文件。只能写 `<插件目录>/<id>/` 里面。
//! - `plugin_read`：读插件文件夹里的文件，或者列出文件夹。
//! - `plugin_reload`：马上重载，告诉模型加载结果、导出的工具、等授权的能力和最近的日志。
//! - `plugin_page`：像用户一样画一页、执行一页上的动作。
//! - `plugin_grant`：请用户允许清单里声明的能力。
//!
//! ## 授权规则
//!
//! 核心原则：**插件能做什么，只能由用户亲自决定**。模型可以随便写、随便试“什么都碰不到”的代码，
//! 但凡是会让代码获得能力的操作，都要用户本人点头（[`Gate::Require`]：不看权限模式，自动审查和插件的
//! `reviewTool` 不能代为放行，审批卡片上也没有“本会话不再询问”）。
//!
//! | 工具 | 手动确认 | 工作区内修改 / 自动审查 | 完全访问 |
//! |---|---|---|---|
//! | 非创造模式下的任何 `plugin_*` | 拒绝 | 拒绝 | 拒绝 |
//! | `plugin_read` | 放行 | 放行 | 放行 |
//! | `plugin_write`：新插件，或没有任何已允许能力的插件 | 询问 | 放行 | 放行 |
//! | `plugin_write`：**已经允许过能力的插件** | 必须确认 | 必须确认 | 必须确认 |
//! | `plugin_write`：链接到插件目录之外的插件（`dshr plugin link`） | 拒绝 | 拒绝 | 拒绝 |
//! | `plugin_reload`、`plugin_page`（只画页面） | 放行 | 放行 | 放行 |
//! | `plugin_page`（执行动作） | 按权限模式（视同调用外部服务） | 同左 | 同左 |
//! | `plugin_grant` | 必须确认 | 必须确认 | 必须确认 |
//!
//! - 新插件在用户允许能力之前什么都碰不到（声明了能力却没被允许的插件根本不激活），所以写它不必逐次询问。
//! - 已经被允许过能力的插件不一样：改过的代码热重载后**直接带着这些能力运行**，等于把能力交给了新代码，
//!   所以每次写都要用户确认，审批卡片上写明它已经被允许了哪些能力。
//! - 链接的插件在别的文件夹里，在插件目录里写同名文件夹会把它遮住，所以直接拒绝，请用户去原文件夹里改。
//! - `plugin_reload`、`plugin_page` 只会带着用户已经允许的能力运行代码，不会多出任何权限。
//! - `plugin_grant` 输入里的能力必须和等授权的完全一致，授予的也**正好是**这几项（不重新读清单）：
//!   用户在审批卡片上看到的就是实际授予的，确认之后清单里又多出来的能力仍然要再确认一次。
//! - 命令行的 `--yes` 是用户明确要求全部放行，`plugin_grant` 也会被放行。
//!
//! 创造模式下还会把插件的写法（`creator.md`）追加到系统提示词后面。
//!
//! ## 不让它跑偏
//!
//! 模型在创造模式里容易把力气花在“找资料”上：逆向程序、翻安装目录、在工作区里写临时脚本，迟迟不写插件。所以：
//!
//! - `plugin_docs`：插件的完整文档编进程序里，按主题查，不用去别处找；
//! - 创造模式下拒绝读或搜索可执行文件、安装包和程序安装目录，拒绝用 `run_shell` 抽取二进制里的字符串，
//!   拒绝 `write_file` / `edit_file`（插件文件只经 `plugin_write` 写）；
//! - 这一轮还没写出插件文件时，调查（`plugin_*` 以外的工具调用）满 [`SOFT_LIMIT`] 次在请求里提醒它动手，
//!   满 [`HARD_LIMIT`] 次后请求里只留 `plugin_*` 工具。次数从请求里的消息推出来，不另存状态。

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use dshr_agent::{AgentConfig, BeforeRequest, Gate, GateRequest, GateTool, PermissionMode, Preset};
use dshr_context::{Manifest, Plugin, Scope, Status};
use dshr_llm::ChatRequest;
use dshr_tools::{Access, Tool, ToolContext, ToolOutput, Tools};
use serde_json::{Value, json};

use super::JsPlugins;
use crate::grants;
use crate::manifest::{MANIFEST_FILE, valid_id};

const GUIDE: &str = include_str!("creator.md");

const WRITE: &str = "plugin_write";
const READ: &str = "plugin_read";
const RELOAD: &str = "plugin_reload";
const GRANT: &str = "plugin_grant";
const PAGE: &str = "plugin_page";
const DOCS: &str = "plugin_docs";
const TOOLS: [&str; 6] = [WRITE, READ, RELOAD, GRANT, PAGE, DOCS];

/// 这一轮还没写出插件文件时，调查满这么多次就提醒模型动手。
const SOFT_LIMIT: usize = 6;
/// 满这么多次后请求里只留 `plugin_*` 工具，直到写出插件文件。
const HARD_LIMIT: usize = 14;

/// 单个插件文件最多这么大。
const MAX_FILE_BYTES: usize = 512 * 1024;
/// `plugin_reload` 带回的日志行数。
const REPORT_LOG_LINES: usize = 20;

/// 创造模式。和装进 `Context` 的 [`JsPlugins`] 用同一份：重载作用于主代理和子代理。
pub struct CreatorPlugin {
    scripts: JsPlugins,
}

impl CreatorPlugin {
    pub fn new(scripts: JsPlugins) -> Self {
        Self { scripts }
    }
}

fn creating(config: &dshr_context::Options) -> bool {
    Preset::of(config) == Preset::Create
}

impl Plugin for CreatorPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("creator")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.hook::<BeforeRequest>(
            0,
            Arc::new(|request: &mut ChatRequest, config: &AgentConfig| {
                if !creating(&config.options) {
                    request.tools.retain(|tool| !TOOLS.contains(&tool.name.as_str()));
                    return;
                }
                let mut guide = GUIDE.to_owned();
                let (explored, wrote) = exploration(&request.messages);
                if !wrote && explored >= HARD_LIMIT {
                    request.tools.retain(|tool| tool.name.starts_with("plugin_"));
                    guide.push_str(&format!(
                        "\n\n**You have made {explored} look-around calls this turn without writing a plugin file.** \
                         Only the plugin_* tools are available now: write plugin.toml and index.js with plugin_write, \
                         or tell the user what is blocking you."
                    ));
                } else if !wrote && explored >= SOFT_LIMIT {
                    guide.push_str(&format!(
                        "\n\n**{explored} look-around calls this turn and no plugin file yet.** That is enough exploring: \
                         write plugin.toml and index.js with plugin_write now (use plugin_docs for anything you still need). \
                         After {HARD_LIMIT} calls only the plugin_* tools stay available."
                    ));
                }
                request.system = Some(match request.system.take() {
                    Some(system) => format!("{system}\n\n{guide}"),
                    None => guide,
                });
            }),
        );
        // 排在预设（-100）、JS 插件的 `gate`（-50）和权限（0）前面：这里给了结论，别的钩子改不了。
        // 规则见模块文档里的表。
        let plugins_dir = self.scripts.dir().to_owned();
        cx.hook::<GateTool>(
            -200,
            Arc::new(move |request: &GateRequest<'_>| {
                if !TOOLS.contains(&request.tool) {
                    return creating(&request.ctx.options).then(|| off_task(request)).flatten();
                }
                if !creating(&request.ctx.options) {
                    return Some(Gate::Deny(format!("`{}` is only available in creation mode.", request.tool)));
                }
                match request.tool {
                    WRITE => Some(write_gate(&plugins_dir, request)),
                    // 授权是用户自己的决定：不管权限模式是什么都问，也不交给自动审查。
                    GRANT => Some(Gate::Require(None)),
                    // 读、重载、画页面：交给权限模式（读是放行的；执行页面动作按调用外部服务处理）。
                    _ => None,
                }
            }),
        );
        let dir = self.scripts.dir().to_owned();
        cx.contribute::<Tools>(Arc::new(WriteTool { dir: dir.clone() }));
        cx.contribute::<Tools>(Arc::new(ReadTool { dir }));
        cx.contribute::<Tools>(Arc::new(ReloadTool { scripts: self.scripts.clone() }));
        cx.contribute::<Tools>(Arc::new(GrantTool { scripts: self.scripts.clone() }));
        cx.contribute::<Tools>(Arc::new(PageTool { scripts: self.scripts.clone() }));
        cx.contribute::<Tools>(Arc::new(DocsTool));
        Ok(())
    }
}

/// 这一轮（最后一条用户输入之后）调用了几次 `plugin_*` 以外的工具，写过插件文件没有。
/// “用户输入”是带文字、不带工具结果的 user 消息：工具结果后面接的插件提醒、插队消息不算新的一轮。
fn exploration(messages: &[dshr_core::Message]) -> (usize, bool) {
    use dshr_core::{ContentBlock, Role};
    let start = messages
        .iter()
        .rposition(|message| {
            message.role == Role::User
                && message.content.iter().any(|block| matches!(block, ContentBlock::Text { .. }))
                && !message.content.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        })
        .map_or(0, |index| index + 1);
    let (mut explored, mut wrote) = (0, false);
    for message in &messages[start..] {
        if message.role != Role::Assistant {
            continue;
        }
        for block in &message.content {
            if let ContentBlock::ToolUse { name, .. } = block {
                if name == WRITE {
                    wrote = true;
                } else if !name.starts_with("plugin_") {
                    explored += 1;
                }
            }
        }
    }
    (explored, wrote)
}

/// 可执行文件、安装包这类二进制的扩展名。
const BINARY_EXTENSIONS: [&str; 15] =
    [".exe", ".dll", ".asar", ".so", ".dylib", ".node", ".pak", ".msi", ".pdb", ".wasm", ".jar", ".class", ".pyc", ".bin", ".dat"];
/// 程序安装目录（小写，路径分隔符统一成 `/`）。
const INSTALL_DIRS: [&str; 8] =
    ["appdata/local/programs", "program files", "programdata", "/applications/", "/usr/lib/", "/usr/share/", "/opt/", "resources/app"];
/// 用 shell 逆向程序的写法（小写）。
const REVERSE_COMMANDS: [&str; 9] =
    ["readallbytes", "strings ", "strings.exe", "objdump", "otool ", "dumpbin", "ildasm", "asar extract", "asar list"];

fn binary_or_install(text: &str) -> bool {
    let text = text.to_lowercase().replace('\\', "/");
    let binary = BINARY_EXTENSIONS.iter().any(|extension| {
        text.match_indices(extension)
            .any(|(at, _)| text[at + extension.len()..].chars().next().is_none_or(|next| !next.is_ascii_alphanumeric() && next != '_'))
    });
    binary || INSTALL_DIRS.iter().any(|dir| text.contains(dir))
}

/// 创造模式下明显跑偏的调用：逆向程序、翻安装目录、往工作区写文件。
fn off_task(request: &GateRequest<'_>) -> Option<Gate> {
    let text = |key: &str| request.input.get(key).and_then(Value::as_str).unwrap_or_default();
    match request.tool {
        "read_file" | "list_dir" | "grep" | "glob" if binary_or_install(text("path")) || binary_or_install(text("pattern")) => {
            Some(Gate::Deny(format!(
                "Creation mode builds a plugin from the documented API; `{}` is a program binary or install folder. \
                 Don't reverse-engineer dshr or other apps: use plugin_docs for anything you need, then write the plugin with plugin_write.",
                if text("path").is_empty() { text("pattern") } else { text("path") }
            )))
        }
        "run_shell"
            if binary_or_install(text("command"))
                || REVERSE_COMMANDS.iter().any(|pattern| text("command").to_lowercase().contains(pattern)) =>
        {
            Some(Gate::Deny(
                "Scanning program binaries or install folders is off-task in creation mode. Everything you need is \
                 documented (plugin_docs); write the plugin with plugin_write and let plugin_reload report problems."
                    .to_owned(),
            ))
        }
        "write_file" | "edit_file" => Some(Gate::Deny(format!(
            "In creation mode the plugin's files go through plugin_write; don't create scripts or temp files in the \
             user's workspace ({}). If the user wants a workspace file changed, ask them to switch to the standard preset.",
            text("path")
        ))),
        _ => None,
    }
}

/// `plugin_write` 的结论（见模块文档里的表）。
fn write_gate(plugins_dir: &Path, request: &GateRequest<'_>) -> Gate {
    let id = request.input.get("id").and_then(Value::as_str).unwrap_or_default();
    // id 不合规的交给工具自己报错（它什么都不会写）。
    if !valid_id(id) {
        return Gate::Allow;
    }
    if crate::links::load(plugins_dir).contains_key(id) && !plugins_dir.join(id).is_dir() {
        return Gate::Deny(format!(
            "`{id}` is a linked plugin that lives outside the plugins folder; writing here would shadow it. \
             Ask the user to edit it in its own folder, or create the plugin under a new id."
        ));
    }
    let granted: Vec<String> =
        grants::load(&grants::grants_path(plugins_dir)).remove(id).map(|set| set.into_iter().collect()).unwrap_or_default();
    if !granted.is_empty() {
        // 改过的代码会直接带着这些能力运行：等于把能力交给新代码，只能由用户决定。
        return Gate::Require(Some(format!(
            "The plugin `{id}` already has these approved capabilities: {}. The changed code will run with them right away.",
            granted.join(", ")
        )));
    }
    // 没有任何能力的插件什么都碰不到：手动确认模式下照样问，其余模式直接写。
    if PermissionMode::of(&request.ctx.options) == PermissionMode::Manual { Gate::Ask } else { Gate::Allow }
}

/// `<插件目录>/<id>`；id 不合规时是 `Err`（给模型的说明）。
fn plugin_dir(dir: &Path, input: &Value) -> Result<(String, PathBuf), String> {
    let id = input.get("id").and_then(Value::as_str).unwrap_or_default();
    if !valid_id(id) {
        return Err(format!(
            "`{id}` is not a valid plugin id: use lowercase letters, digits, `-` and `_`, starting with a letter or digit, at most 64 characters."
        ));
    }
    Ok((id.to_owned(), dir.join(id)))
}

/// 插件文件夹里的相对路径；跑出文件夹的拒绝。
fn inside(folder: &Path, path: &str) -> Result<PathBuf, String> {
    let relative = Path::new(path);
    let plain = !path.is_empty() && relative.components().all(|part| matches!(part, Component::Normal(_)));
    if !plain {
        return Err(format!("`{path}` must be a relative path inside the plugin folder (no `..`, no absolute paths)."));
    }
    Ok(folder.join(relative))
}

struct WriteTool {
    dir: PathBuf,
}

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        WRITE
    }

    fn description(&self) -> &str {
        "Create or overwrite one file of a dshr plugin, in the plugin's own folder (`<plugins>/<id>/<path>`). \
         Typical files are `plugin.toml` and `index.js`. Parent folders are created. Call plugin_reload afterwards. \
         Writing to a plugin that already has approved capabilities needs the user's confirmation each time."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Plugin id, also the folder name" },
                "path": { "type": "string", "description": "Path inside the plugin folder, e.g. index.js or lib/util.js" },
                "content": { "type": "string", "description": "The whole file content" }
            },
            "required": ["id", "path", "content"]
        })
    }

    fn access(&self, input: &Value, _ctx: &ToolContext) -> Access {
        let target = plugin_dir(&self.dir, input)
            .and_then(|(_, folder)| inside(&folder, input.get("path").and_then(Value::as_str).unwrap_or_default()));
        Access::Write(target.unwrap_or_else(|_| self.dir.clone()))
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolOutput {
        let (id, folder) = match plugin_dir(&self.dir, &input) {
            Ok(found) => found,
            Err(error) => return ToolOutput::error(error),
        };
        let path = input.get("path").and_then(Value::as_str).unwrap_or_default();
        let target = match inside(&folder, path) {
            Ok(target) => target,
            Err(error) => return ToolOutput::error(error),
        };
        let Some(content) = input.get("content").and_then(Value::as_str) else {
            return ToolOutput::error("`content` is required.");
        };
        if content.len() > MAX_FILE_BYTES {
            return ToolOutput::error(format!("The file is too large ({} bytes, at most {MAX_FILE_BYTES}).", content.len()));
        }
        let written = target.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&target, content));
        match written {
            Ok(()) => ToolOutput::ok(format!(
                "Wrote {id}/{path} ({} bytes). Call plugin_reload when the plugin's files are ready.",
                content.len()
            )),
            Err(error) => ToolOutput::error(format!("Could not write {}: {error}", target.display())),
        }
    }
}

struct ReadTool {
    dir: PathBuf,
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        READ
    }

    fn description(&self) -> &str {
        "Read one file of a dshr plugin, or list the plugin's files when `path` is omitted. \
         Without any id, lists the installed plugins."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Plugin id; omit to list all plugins" },
                "path": { "type": "string", "description": "File inside the plugin folder; omit to list its files" }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolOutput {
        if input.get("id").and_then(Value::as_str).is_none_or(str::is_empty) {
            let found = crate::manifest::discover(&self.dir);
            if found.is_empty() {
                return ToolOutput::ok(format!("No plugins yet ({}).", self.dir.display()));
            }
            let lines: Vec<String> = found
                .iter()
                .map(|plugin| match &plugin.manifest {
                    Ok(manifest) => format!("{} — {}", plugin.id, manifest.description.as_deref().unwrap_or("")),
                    Err(error) => format!("{} — invalid manifest: {error}", plugin.id),
                })
                .collect();
            return ToolOutput::ok(lines.join("\n"));
        }
        let (id, folder) = match plugin_dir(&self.dir, &input) {
            Ok(found) => found,
            Err(error) => return ToolOutput::error(error),
        };
        match input.get("path").and_then(Value::as_str).filter(|path| !path.is_empty()) {
            Some(path) => {
                let target = match inside(&folder, path) {
                    Ok(target) => target,
                    Err(error) => return ToolOutput::error(error),
                };
                match std::fs::read_to_string(&target) {
                    Ok(text) => ToolOutput::ok(dshr_tools::truncate(text)),
                    Err(error) => ToolOutput::error(format!("Could not read {id}/{path}: {error}")),
                }
            }
            None => {
                if !folder.is_dir() {
                    return ToolOutput::ok(format!("There is no plugin `{id}` yet; the id is free."));
                }
                let mut files = Vec::new();
                list(&folder, &folder, &mut files);
                files.sort();
                ToolOutput::ok(format!("Files of {id}:\n{}", files.join("\n")))
            }
        }
    }
}

/// 文件夹里的文件（相对路径和大小），最多四层、两百个。
fn list(root: &Path, dir: &Path, files: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if files.len() >= 200 {
            return;
        }
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path).display().to_string().replace('\\', "/");
        if path.is_dir() {
            if relative.split('/').count() < 4 && !relative.starts_with('.') {
                list(root, &path, files);
            }
        } else {
            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            files.push(format!("{relative} ({size} bytes)"));
        }
    }
}

struct ReloadTool {
    scripts: JsPlugins,
}

#[async_trait::async_trait]
impl Tool for ReloadTool {
    fn name(&self) -> &str {
        RELOAD
    }

    fn description(&self) -> &str {
        "Load the current files of a dshr plugin now and report: whether it loaded (or the error), the tools it \
         exports (callable from your next step), capabilities still waiting for the user's approval, and its recent log."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Plugin id" } },
            "required": ["id"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolOutput {
        let (id, folder) = match plugin_dir(self.scripts.dir(), &input) {
            Ok(found) => found,
            Err(error) => return ToolOutput::error(error),
        };
        if !folder.join(MANIFEST_FILE).is_file() && !folder.join("index.js").is_file() {
            return ToolOutput::error(format!("{id} has neither {MANIFEST_FILE} nor index.js yet; write them with plugin_write."));
        }
        // 加载要执行脚本的顶层代码（最多 2 秒），不占着异步线程。
        let scripts = self.scripts.clone();
        let reloaded = tokio::task::spawn_blocking(move || {
            scripts.reload();
            scripts.report(&id)
        })
        .await;
        match reloaded {
            Ok(Some(report)) => {
                let failed = matches!(report.status, Some(Status::Failed(_))) && report.missing.is_empty();
                let text = describe(&report, self.scripts.dir());
                if failed { ToolOutput::error(text) } else { ToolOutput::ok(text) }
            }
            Ok(None) => ToolOutput::error("The plugin was not found after reloading; check that its manifest has a valid id."),
            Err(error) => ToolOutput::error(format!("Reloading failed: {error}")),
        }
    }
}

/// 给模型看的插件情况。
pub(super) fn describe(report: &crate::PluginReport, plugins_dir: &Path) -> String {
    let mut lines = Vec::new();
    let data = plugins_dir.parent().unwrap_or(plugins_dir).join("plugin-data").join(&report.id);
    lines.push(match &report.status {
        Some(Status::Active) => format!("Plugin {}: loaded.", report.id),
        Some(Status::Disabled) => format!("Plugin {}: disabled by the user in settings; it will not run.", report.id),
        Some(Status::Pending(services)) => format!("Plugin {}: waiting for {}.", report.id, services.join(", ")),
        Some(Status::Failed(_)) if !report.missing.is_empty() => {
            format!("Plugin {}: loads only after the user approves its capabilities.", report.id)
        }
        Some(Status::Failed(error)) => format!("Plugin {}: failed to load.\n{error}", report.id),
        None => format!("Plugin {}: not installed in this session.", report.id),
    });
    if let Some(reason) = &report.kept {
        lines.push(format!("The edited version did not load, so the previous version is still in use:\n{reason}"));
    }
    if !report.tools.is_empty() {
        lines.push(format!("Tools (callable from your next step): {}", report.tools.join(", ")));
    }
    if !report.missing.is_empty() {
        lines.push(format!(
            "Capabilities waiting for approval: {}. Explain them to the user, then call plugin_grant with exactly this list.",
            report.missing.join(", ")
        ));
    }
    if !report.ui.is_empty() {
        lines.push(format!("UI (previewed without a workspace; try actions with plugin_page):\n{}", report.ui.join("\n")));
    }
    lines.push(format!("Private data folder (host.readData / writeData): {}", data.display()));
    if let Some(reason) = &report.stopped {
        lines.push(format!("Stopped after repeated runaway calls: {reason}. Fix the code and reload."));
    }
    if !report.logs.is_empty() {
        let start = report.logs.len().saturating_sub(REPORT_LOG_LINES);
        let log: Vec<String> = report.logs[start..].iter().map(|line| format!("[{}] {}", line.level, line.text)).collect();
        lines.push(format!("Recent log:\n{}", log.join("\n")));
    }
    lines.join("\n\n")
}

struct GrantTool {
    scripts: JsPlugins,
}

#[async_trait::async_trait]
impl Tool for GrantTool {
    fn name(&self) -> &str {
        GRANT
    }

    fn description(&self) -> &str {
        "Ask the user to approve the capabilities a plugin's manifest declares (file access, network hosts). \
         The user always confirms this personally in the app, in every permission mode; it is never auto-approved. \
         `capabilities` must list exactly the ones plugin_reload reports as waiting, and exactly those are granted. \
         Explain to the user what they allow before calling this."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Plugin id" },
                "capabilities": { "type": "array", "items": { "type": "string" }, "description": "e.g. [\"net:api.example.com\"]" }
            },
            "required": ["id", "capabilities"]
        })
    }

    fn access(&self, _input: &Value, _ctx: &ToolContext) -> Access {
        Access::External
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolOutput {
        let (id, _) = match plugin_dir(self.scripts.dir(), &input) {
            Ok(found) => found,
            Err(error) => return ToolOutput::error(error),
        };
        let asked: BTreeSet<String> = input
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        let waiting: BTreeSet<String> =
            self.scripts.grant_request(&id).map(|request| request.missing.into_iter().collect()).unwrap_or_default();
        if waiting.is_empty() {
            return ToolOutput::ok(format!("{id} has no capabilities waiting for approval."));
        }
        // 用户在审批卡片上看到的就是 `capabilities`：和清单里的对不上就不授予，免得批准的和实际给的不一样。
        if asked != waiting {
            let waiting: Vec<_> = waiting.into_iter().collect();
            return ToolOutput::error(format!(
                "Nothing granted: `capabilities` must be exactly the ones waiting for approval: {}.",
                waiting.join(", ")
            ));
        }
        // 只授予用户在审批卡片上看到的这几项，不重新读清单：确认之后清单又改了的，多出来的仍然等授权。
        let scripts = self.scripts.clone();
        let approved: Vec<String> = asked.into_iter().collect();
        let granted = tokio::task::spawn_blocking(move || scripts.grant_exact(&id, &approved).map(|_| scripts.report(&id))).await;
        match granted {
            Ok(Ok(Some(report))) => ToolOutput::ok(format!("The user approved.\n\n{}", describe(&report, self.scripts.dir()))),
            Ok(Ok(None)) => ToolOutput::ok("The user approved."),
            Ok(Err(error)) => ToolOutput::error(error),
            Err(error) => ToolOutput::error(format!("Granting failed: {error}")),
        }
    }
}

/// 像用户一样用一页：画出来看看，或者执行它的一个动作（填好输入框、点按钮、拨开关）再画一次。
struct PageTool {
    scripts: JsPlugins,
}

#[async_trait::async_trait]
impl Tool for PageTool {
    fn name(&self) -> &str {
        PAGE
    }

    fn description(&self) -> &str {
        "Use one page of a dshr plugin the way the user would. Without `action`, render the page and return its node tree. \
         With `action`, run it as if the user clicked it: `inputs` holds the text typed into the page's input boxes, \
         `value` is the clicked node's payload, `checked` is a switch's new state. Then render the page again. \
         Runs against the open workspace."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Plugin id" },
                "page": { "type": "string", "description": "Page name from ui.pages" },
                "action": { "type": "string", "description": "Action to run; omit to only render" },
                "inputs": { "type": "object", "description": "Input box id → text", "additionalProperties": { "type": "string" } },
                "value": { "description": "The clicked node's payload" },
                "checked": { "type": "boolean", "description": "A switch's new state" }
            },
            "required": ["id", "page"]
        })
    }

    fn access(&self, input: &Value, _ctx: &ToolContext) -> Access {
        // 只画页面是只读的；动作会按插件的能力写文件、联网，和交给外部一样处理。
        if input.get("action").and_then(Value::as_str).is_some() { Access::External } else { Access::Read }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let (id, _) = match plugin_dir(self.scripts.dir(), &input) {
            Ok(found) => found,
            Err(error) => return ToolOutput::error(error),
        };
        let page = input.get("page").and_then(Value::as_str).unwrap_or_default().to_owned();
        let action = input.get("action").and_then(Value::as_str).map(str::to_owned);
        let mut payload = json!({
            "value": input.get("value").cloned().unwrap_or(Value::Null),
            "inputs": input.get("inputs").cloned().unwrap_or_else(|| json!({})),
        });
        if let Some(checked) = input.get("checked").and_then(Value::as_bool) {
            payload["checked"] = Value::Bool(checked);
        }
        let Some(surface) = self.scripts.surface(&id) else {
            return ToolOutput::error(format!("{id} has no active UI; check plugin_reload first."));
        };
        let workspace = ctx.workspace.clone();
        let result = tokio::task::spawn_blocking(move || {
            let message = match &action {
                Some(action) => Some(surface.act(&page, action, payload, Some(&workspace))?),
                None => None,
            };
            let tree = surface.render(&page, Some(&workspace))?;
            Ok::<_, String>((message, tree))
        })
        .await;
        match result {
            Ok(Ok((message, tree))) => {
                let mut text = String::new();
                if let Some(message) = message {
                    text.push_str(&format!("Action finished. Message shown to the user: {}\n\n", message.as_deref().unwrap_or("(none)")));
                }
                text.push_str(&format!("Page now renders as:\n{}", serde_json::to_string_pretty(&tree).unwrap_or_default()));
                ToolOutput::ok(dshr_tools::truncate(text))
            }
            Ok(Err(error)) => ToolOutput::error(error),
            Err(error) => ToolOutput::error(format!("The page call failed: {error}")),
        }
    }
}

/// 编进程序里的插件文档：插件的三篇全文，加上配置、MCP、权限等文档（`plugin_docs` 按小节检索）。
const DOC_FILES: [(&str, &str); 11] = [
    ("plugins.md", include_str!("../../../../docs/plugins.md")),
    ("plugin-api.md", include_str!("../../../../docs/plugin-api.md")),
    ("plugin-publishing.md", include_str!("../../../../docs/plugin-publishing.md")),
    ("configuration.md", include_str!("../../../../docs/configuration.md")),
    ("mcp.md", include_str!("../../../../docs/mcp.md")),
    ("permissions.md", include_str!("../../../../docs/permissions.md")),
    ("instructions-and-skills.md", include_str!("../../../../docs/instructions-and-skills.md")),
    ("agent.md", include_str!("../../../../docs/agent.md")),
    ("sessions.md", include_str!("../../../../docs/sessions.md")),
    ("cli.md", include_str!("../../../../docs/cli.md")),
    ("troubleshooting.md", include_str!("../../../../docs/troubleshooting.md")),
];
/// 一次最多交回这么多字节。
const DOCS_LIMIT: usize = 24 * 1024;

/// 文档的一个小节：所在文件、标题路径（`插件 API 参考 › hooks › beforeTool`）、正文。
struct DocSection {
    file: &'static str,
    path: String,
    text: String,
}

/// 按 `#`… 标题切成小节（代码块里的 `#` 不算）。
fn doc_sections() -> Vec<DocSection> {
    let mut sections = Vec::new();
    for (file, source) in DOC_FILES {
        let mut stack: Vec<(usize, String)> = Vec::new();
        let mut fence = false;
        let mut current: Option<DocSection> = None;
        for line in source.lines() {
            if line.starts_with("```") {
                fence = !fence;
            }
            let level = line.chars().take_while(|c| *c == '#').count();
            let heading = !fence && (1..=4).contains(&level) && line[level..].starts_with(' ');
            if heading {
                if let Some(done) = current.take() {
                    sections.push(done);
                }
                let title = line[level..].trim().replace("&lt;", "<").replace("&gt;", ">");
                stack.retain(|(depth, _)| *depth < level);
                stack.push((level, title));
                let path = stack.iter().map(|(_, title)| title.as_str()).collect::<Vec<_>>().join(" › ");
                current = Some(DocSection { file, path, text: String::new() });
            } else if let Some(section) = current.as_mut() {
                section.text.push_str(line);
                section.text.push('\n');
            }
        }
        sections.extend(current);
    }
    sections.retain(|section| !section.text.trim().is_empty());
    sections
}

/// 按主题检索；没有主题时列出全部小节。
fn lookup_docs(topic: &str) -> String {
    let sections = doc_sections();
    let topic = topic.trim().to_lowercase();
    if topic.is_empty() {
        let mut out = String::from("dshr plugin documentation. Call plugin_docs again with a topic (words from a heading or the text):\n");
        let mut last = "";
        for section in &sections {
            if section.file != last {
                out.push_str(&format!("\n{}\n", section.file));
                last = section.file;
            }
            out.push_str(&format!("  - {}\n", section.path));
        }
        return dshr_tools::truncate(out);
    }
    let terms: Vec<&str> = topic.split(|c: char| c.is_whitespace() || ",，、".contains(c)).filter(|term| !term.is_empty()).collect();
    let mut ranked: Vec<(usize, &DocSection)> = sections
        .iter()
        .map(|section| {
            let title = format!("{} {}", section.file, section.path).to_lowercase();
            let text = section.text.to_lowercase();
            let score = terms.iter().map(|term| if title.contains(term) { 10 } else { 0 } + text.matches(term).count().min(20)).sum();
            (score, section)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    if ranked.is_empty() {
        return format!("No section matches \"{topic}\". Call plugin_docs without a topic to list every section.");
    }
    ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    let mut out = String::new();
    for (_, section) in &ranked {
        let block = format!("### {} › {}\n\n{}\n", section.file, section.path, section.text.trim());
        if !out.is_empty() && out.len() + block.len() > DOCS_LIMIT {
            break;
        }
        out.push_str(&block);
        out.push('\n');
    }
    if ranked.len() > 1 {
        out.push_str(&format!("({} sections matched; narrow the topic to see others.)", ranked.len()));
    }
    dshr_tools::truncate(out)
}

struct DocsTool;

#[async_trait::async_trait]
impl Tool for DocsTool {
    fn name(&self) -> &str {
        DOCS
    }

    fn description(&self) -> &str {
        "Search dshr's own plugin documentation: manifest, capabilities, tools, hooks, events, UI nodes, color schemes, \
         presets, commands, skills, MCP servers in plugins, providers, background jobs, packaging, permissions, \
         troubleshooting. Use it instead of searching programs or source code. Without a topic, lists every section."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "topic": { "type": "string", "description": "Words to look for, e.g. \"themes\", \"beforeTool\", \"ui nodes\", \"secrets\"" }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(lookup_docs(input.get("topic").and_then(Value::as_str).unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dshr_core::{ContentBlock, Message, Role};

    fn call(name: &str) -> ContentBlock {
        ContentBlock::ToolUse { id: name.into(), name: name.into(), input: Arc::new(json!({})) }
    }

    fn result() -> ContentBlock {
        ContentBlock::ToolResult { tool_use_id: "x".into(), content: "ok".into(), is_error: false }
    }

    #[test]
    fn exploration_counts_this_turn_only() {
        let user = |blocks: Vec<ContentBlock>| Message { role: Role::User, content: blocks };
        let assistant = |blocks: Vec<ContentBlock>| Message { role: Role::Assistant, content: blocks };
        let mut messages = vec![
            user(vec![ContentBlock::text("earlier")]),
            assistant(vec![call("grep"), call("grep")]),
            user(vec![result(), result()]),
            user(vec![ContentBlock::text("make a pink theme")]),
            assistant(vec![call("plugin_read"), call("read_file")]),
            // 工具结果后面接的提醒不是新的一轮。
            user(vec![result(), ContentBlock::text("<reminder>"), result()]),
            assistant(vec![call("run_shell"), call("plugin_docs")]),
        ];
        assert_eq!(exploration(&messages), (2, false));
        messages.push(user(vec![result()]));
        messages.push(assistant(vec![call("plugin_write"), call("grep")]));
        assert_eq!(exploration(&messages), (3, true));
    }

    #[test]
    fn spots_binaries_and_install_folders() {
        assert!(binary_or_install("dshr\\dshr.exe"));
        assert!(binary_or_install("C:\\Users\\A\\AppData\\Local\\Programs\\app\\resources\\app.asar"));
        assert!(binary_or_install("powershell -Command \"[IO.File]::ReadAllBytes('x.exe')\""));
        assert!(!binary_or_install("src/executor.rs"));
        assert!(!binary_or_install("docs/data.json"));
        assert!(!binary_or_install("index.js"));
    }

    #[test]
    fn docs_cover_the_plugin_api() {
        let themes = lookup_docs("themes");
        assert!(themes.contains("accent_soft") && themes.contains("focus_ring"), "{themes}");
        assert!(lookup_docs("beforeTool").contains("hooks:gate"));
        assert!(lookup_docs("").contains("plugin-api.md"));
        assert!(lookup_docs("zzzz-nothing").starts_with("No section matches"));
        assert!(lookup_docs("a").len() <= DOCS_LIMIT + 1024);
    }
}
