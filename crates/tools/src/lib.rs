//! 工具抽象与内置工具。

mod builtin;
mod edit;
mod plan;
mod plugins;
mod search;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use dshr_context::{ExtensionPoint, Hook, Options, Snapshot};
use dshr_llm::ToolSpec;
use serde_json::Value;

pub use builtin::{ListDir, ReadFile, RunShell, WriteFile};
pub use edit::{EditFile, unified_diff};
pub use plan::{Plan, PlanStep, StepStatus, UPDATE_PLAN, UpdatePlan};
pub use plugins::{BuiltinTools, FsPlugin, PlanPlugin, SearchPlugin, ShellPlugin};
pub use search::{Glob, Grep, find_files};

/// 工具执行环境。
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    /// 执行过程中的实时输出（如命令的 stdout/stderr），界面据此显示终端面板；只用于展示，不进日志。
    pub output: Option<OutputSink>,
    /// 工具往会话日志里写自定义事件的通道（[`ToolContext::emit_event`]）；不在 agent 循环里时没有。
    pub events: Option<EventSink>,
    /// 本轮与这次调用的选项（权限模式等），工具和钩子按自己认识的类型取。
    pub options: Options,
    /// 这一步用的插件快照；工具从这里取钩子（如 [`WrapCommand`]）。
    pub plugins: Arc<Snapshot>,
}

/// `run_shell` 要启动的进程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommand {
    /// 模型给的命令原文。
    pub script: String,
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl ShellCommand {
    /// 平台默认的 shell：Windows 是 `cmd /C`，其他是 `sh -c`。
    pub fn new(script: &str) -> Self {
        let (program, args) = dshr_sandbox::shell_command(script, Path::new(""), false);
        Self { script: script.to_owned(), program, args }
    }
}

/// 钩子：dshr 自己的文件工具（`write_file`、`edit_file`）写盘之前（对应原版的 `fs/write-intent`）。按优先级依次问，
/// 后一个看到前一个改过的内容；有一个拒绝就不写。在工具执行时同步调用（审批已经过了），要快。
pub struct WriteIntent;

impl Hook for WriteIntent {
    type Handler = dyn Fn(&WriteRequest<'_>) -> WriteDecision + Send + Sync;
}

/// 一次要写的文件。
pub struct WriteRequest<'a> {
    pub tool: &'a str,
    /// 模型给的路径（相对工作区）。
    pub path: &'a str,
    /// 原来的内容；新文件是 `None`。
    pub before: Option<&'a str>,
    /// 要写的内容（前面的钩子可能已经改过）。
    pub after: &'a str,
    pub ctx: &'a ToolContext,
}

pub enum WriteDecision {
    /// 照写。
    Keep,
    /// 改成这些内容；`by` 是谁改的（写进工具结果）。
    Rewrite {
        content: String,
        by: String,
    },
    Deny(String),
}

/// 前后内容超过这么大时不问钩子（钩子是同步的，大文件不值得让它们慢慢看）。
pub const MAX_WRITE_INTENT_BYTES: usize = 256 * 1024;

/// 依次问 [`WriteIntent`] 钩子：返回最终要写的内容和改过它的插件，或者拒绝的原因。
pub fn apply_write_intents(
    ctx: &ToolContext,
    tool: &str,
    path: &str,
    before: Option<&str>,
    after: String,
) -> Result<(String, Vec<String>), String> {
    let hooks = ctx.plugins.hooks::<WriteIntent>();
    if hooks.is_empty() || before.is_some_and(|before| before.len() > MAX_WRITE_INTENT_BYTES) || after.len() > MAX_WRITE_INTENT_BYTES {
        return Ok((after, Vec::new()));
    }
    let mut content = after;
    let mut changed_by = Vec::new();
    for hook in hooks {
        let request = WriteRequest { tool, path, before, after: &content, ctx };
        match hook(&request) {
            WriteDecision::Keep => {}
            WriteDecision::Rewrite { content: rewritten, by } => {
                content = rewritten;
                if !changed_by.contains(&by) {
                    changed_by.push(by);
                }
            }
            WriteDecision::Deny(reason) => return Err(reason),
        }
    }
    Ok((content, changed_by))
}

/// 工具结果里注明内容被哪些插件调整过。
pub fn adjusted_note(changed_by: &[String]) -> String {
    if changed_by.is_empty() {
        String::new()
    } else {
        format!("\n(The content was adjusted before writing by: {}.)", changed_by.join(", "))
    }
}

/// 钩子：命令启动前改写它（如包进沙箱）。
pub struct WrapCommand;

impl Hook for WrapCommand {
    type Handler = dyn Fn(&mut ShellCommand, &ToolContext) + Send + Sync;
}

/// 接收工具实时输出的回调。
#[derive(Clone)]
pub struct OutputSink(pub Arc<dyn Fn(&str) + Send + Sync>);

impl std::fmt::Debug for OutputSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OutputSink")
    }
}

/// 接收工具写的自定义事件（类型名，数据）的回调。
#[derive(Clone)]
pub struct EventSink(pub Arc<dyn Fn(String, Value) + Send + Sync>);

impl std::fmt::Debug for EventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EventSink")
    }
}

impl ToolContext {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace, output: None, events: None, options: Options::new(), plugins: Arc::default() }
    }

    /// 往会话日志里写一条插件自定义事件（`dshr_core::EventBody::Ext`）。`kind` 写成 `命名空间/名字`，
    /// 如 `todo/item`。事件在这次调用的 `tool/result` 之后按顺序写入；名字不合规的会被丢弃并记一条警告。
    /// 不在 agent 循环里（没有日志可写）时什么也不做。
    pub fn emit_event(&self, kind: &str, data: Value) {
        if let Some(sink) = &self.events {
            (sink.0)(kind.to_owned(), data);
        }
    }

    /// 推送一段实时输出；没有订阅方时什么也不做。
    pub fn emit_output(&self, text: &str) {
        if let Some(sink) = &self.output {
            (sink.0)(text);
        }
    }

    /// 相对路径按工作区解析，并在字面上消去 `.` 与 `..`（目标文件可以还不存在）。
    pub fn resolve(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        let joined = if path.is_absolute() { path.to_owned() } else { self.workspace.join(path) };
        normalize(&joined)
    }

    /// 路径在工作区里：字面上（消去 `.`、`..` 后）在工作区下，并且经过符号链接后的真实位置也在工作区下。
    ///
    /// 只看字面的话，工作区里一个指向外面的链接（`link -> ~/.bashrc`）就能让“工作区内可写”的写入落到外面。
    /// 对路径中已经存在的最深一层取真实路径再比一次；这一层是悬空的符号链接时（写入会顺着它在别处建文件）
    /// 按在工作区外处理。工作区本身取不到真实路径（还不存在）时只能按字面判断。
    pub fn is_inside_workspace(&self, path: &Path) -> bool {
        is_inside(&self.workspace, path)
    }

    /// 读 `path`（模型给的，相对工作区）的访问类型：在工作区里，或者在插件登记的可读目录（[`ReadableRoots`]，
    /// 如 Skills 文件夹）里，是 [`Access::Read`]；其余的是 [`Access::ReadOutside`]，除完全访问模式外要问用户。
    pub fn read_access(&self, path: &str) -> Access {
        let resolved = self.resolve(path);
        let readable =
            self.is_inside_workspace(&resolved) || self.plugins.items::<ReadableRoots>().iter().any(|root| is_inside(root, &resolved));
        if readable { Access::Read } else { Access::ReadOutside(resolved) }
    }
}

/// `path` 在 `root` 下（字面上，以及经过符号链接后的真实位置都在），见 [`ToolContext::is_inside_workspace`]。
fn is_inside(root: &Path, path: &Path) -> bool {
    let path = normalize(path);
    if !path.starts_with(normalize(root)) {
        return false;
    }
    // 根目录取不到真实路径（还不存在）时只能按字面判断。
    let Ok(root) = root.canonicalize() else { return true };
    let mut probe = Some(path.as_path());
    while let Some(existing) = probe {
        match existing.canonicalize() {
            Ok(real) => return real.starts_with(&root),
            // 条目存在却解析不了：悬空的符号链接（或链接成环）。
            Err(_) if existing.symlink_metadata().is_ok() => return false,
            Err(_) => probe = existing.parent(),
        }
    }
    true
}

/// 扩展点：工作区之外、模型可以不经询问就读的目录（Skills 文件夹：系统提示词让模型自己去读里面的 `SKILL.md`）。
pub struct ReadableRoots;

impl ExtensionPoint for ReadableRoots {
    type Item = PathBuf;
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// 一次工具调用要做的事，权限模式据此决定放行、询问或拒绝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// 读工作区里的东西（或者插件登记的可读目录，见 [`ReadableRoots`]）。
    Read,
    /// 读工作区外的这个路径：除完全访问模式外要问用户（被提示词注入时，模型可能去读 `~/.ssh` 之类）。
    ReadOutside(PathBuf),
    /// 写入（或创建）这个路径。
    Write(PathBuf),
    /// 执行任意命令，事先无法判断影响范围。
    Execute,
    /// 交给外部程序处理（如 MCP 服务器）：影响范围同样无法判断，而且不受命令沙箱约束。
    External,
}

impl Access {
    /// 只读，不管在不在工作区里。
    pub fn is_read(&self) -> bool {
        matches!(self, Self::Read | Self::ReadOutside(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;

    /// 这次调用的访问类型；默认只读。
    fn access(&self, _input: &Value, _ctx: &ToolContext) -> Access {
        Access::Read
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput;
}

/// 扩展点：模型可以调用的工具。
pub struct Tools;

impl ExtensionPoint for Tools {
    type Item = Arc<dyn Tool>;
}

/// 扩展点：运行时才知道有哪些工具的来源（如 MCP 服务器）。每个 step 开始时问一次。
pub struct ToolSources;

impl ExtensionPoint for ToolSources {
    type Item = Arc<dyn ToolSource>;
}

#[async_trait::async_trait]
pub trait ToolSource: Send + Sync {
    /// 当前可用的工具。第一次调用可以花时间做连接；失败的来源返回空列表，不要让对话卡住。
    async fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

/// 某一刻可用的工具，按名字查找。
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// 快照里所有插件贡献的工具；重名时后注册的生效。
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        let tools = snapshot.items::<Tools>().iter().map(|tool| (tool.name().to_owned(), tool.clone())).collect();
        Self { tools }
    }

    /// 快照里的工具，加上各个 [`ToolSources`] 当前提供的。
    pub async fn load(snapshot: &Snapshot) -> Self {
        let mut registry = Self::from_snapshot(snapshot);
        for source in snapshot.items::<ToolSources>() {
            registry.tools.extend(source.tools().await.into_iter().map(|tool| (tool.name().to_owned(), tool)));
        }
        registry
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| spec(tool.as_ref())).collect()
    }

    /// 只读的工具（访问类型是 [`Access::Read`]）：只读的一轮里只把这些发给模型，其余的反正会被拒绝。
    ///
    /// 用空输入问一次访问类型。现有工具的访问类型都不看输入的内容（写文件的工具缺 `path` 时按命令算），
    /// 所以这样问出来的就是它每次调用的访问类型。
    pub fn read_only_specs(&self, ctx: &ToolContext) -> Vec<ToolSpec> {
        let empty = Value::Object(serde_json::Map::new());
        self.tools.values().filter(|tool| tool.access(&empty, ctx) == Access::Read).map(|tool| spec(tool.as_ref())).collect()
    }
}

fn spec(tool: &dyn Tool) -> ToolSpec {
    ToolSpec { name: tool.name().to_owned(), description: tool.description().to_owned(), input_schema: tool.input_schema() }
}

/// 工具输出上限，超出部分截断，避免撑爆上下文。
pub const MAX_OUTPUT_CHARS: usize = 30_000;

pub fn truncate(mut text: String) -> String {
    if let Some((cut, _)) = text.char_indices().nth(MAX_OUTPUT_CHARS) {
        let omitted = text[cut..].chars().count();
        text.truncate(cut);
        text.push_str(&format!("\n… [已截断 {omitted} 个字符]"));
    }
    text
}

pub(crate) fn parse<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(input).map_err(|error| ToolOutput::error(format!("参数无效: {error}")))
}

/// 文本是纯 CRLF 换行（有换行，并且每个 `\n` 前面都是 `\r`）。混用两种换行的文件不算。
pub(crate) fn is_crlf(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut any = false;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            if index == 0 || bytes[index - 1] != b'\r' {
                return false;
            }
            any = true;
        }
    }
    any
}

/// 换成 CRLF 换行（已经是 `\r\n` 的不会变成 `\r\r\n`）。
pub(crate) fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// 原子地写文件：先写同目录下的临时文件、落盘，再改名换掉原文件。写到一半崩溃、磁盘满时原文件不受影响，
/// 不会留下截断的源文件。
///
/// - 原文件的权限照搬；只读的文件照旧拒绝写（改名本身不看文件权限，不能借此绕过）。
/// - 路径是符号链接时写到它指向的文件，链接本身保留。
/// - 临时文件建不了（目录不可写）或改名失败（Windows 上文件正被别的程序打开）时退回直接写入。
pub(crate) async fn write_atomic(path: PathBuf, content: String) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || write_atomic_blocking(&path, content.as_bytes())).await.map_err(std::io::Error::other)?
}

fn write_atomic_blocking(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let target = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::canonicalize(path) {
            Ok(real) => real,
            // 悬空的链接：照旧直接写（顺着链接建文件）。
            Err(_) => return std::fs::write(path, bytes),
        },
        _ => path.to_owned(),
    };
    let existing = std::fs::metadata(&target).ok();
    if existing.as_ref().is_some_and(|meta| meta.permissions().readonly()) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "文件是只读的"));
    }
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else { return std::fs::write(&target, bytes) };
    let mut tmp_name = OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.{}.dshr-tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let tmp = dir.join(tmp_name);
    let Ok(mut file) = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) else {
        return std::fs::write(&target, bytes);
    };
    let written = (|| {
        file.write_all(bytes)?;
        if let Some(meta) = &existing {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_data()
    })();
    drop(file);
    if let Err(error) = written {
        // 多半是磁盘满：原文件还完好，不再退回直接写（那样才会把它截断）。
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return std::fs::write(&target, bytes);
    }
    Ok(())
}

/// 从参数里取 `path` 并解析成写入目标。
pub(crate) fn write_access(input: &Value, ctx: &ToolContext) -> Access {
    match input.get("path").and_then(Value::as_str) {
        Some(path) => Access::Write(ctx.resolve(path)),
        // 参数无效时按最严格处理，执行时会报参数错误。
        None => Access::Execute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_normalizes_and_detects_escape() {
        let ctx = ToolContext::new(PathBuf::from("/work/ws"));
        assert_eq!(ctx.resolve("a/./b/../c.txt"), PathBuf::from("/work/ws/a/c.txt"));
        assert!(ctx.is_inside_workspace(&ctx.resolve("src/main.rs")));
        assert!(!ctx.is_inside_workspace(&ctx.resolve("../other/secret")));
        assert!(!ctx.is_inside_workspace(Path::new("/work/ws2/x")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_workspace_are_outside() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(outside.path().join("rc"), "x").unwrap();
        symlink(outside.path(), dir.path().join("out")).unwrap();
        symlink(outside.path().join("rc"), dir.path().join("rc")).unwrap();
        symlink(outside.path().join("missing"), dir.path().join("dangling")).unwrap();
        symlink(dir.path().join("src"), dir.path().join("inner")).unwrap();
        let ctx = ToolContext::new(dir.path().to_owned());

        // 普通文件、还不存在的文件、指回工作区里面的链接：在里面。
        assert!(ctx.is_inside_workspace(&ctx.resolve("src/a.rs")));
        assert!(ctx.is_inside_workspace(&ctx.resolve("new/dir/b.rs")));
        assert!(ctx.is_inside_workspace(&ctx.resolve("inner/c.rs")));
        // 经过指到外面的目录链接、文件链接本身、悬空链接：在外面。
        assert!(!ctx.is_inside_workspace(&ctx.resolve("out/new.txt")));
        assert!(!ctx.is_inside_workspace(&ctx.resolve("rc")));
        assert!(!ctx.is_inside_workspace(&ctx.resolve("dangling")));
    }

    #[test]
    fn reads_outside_the_workspace_and_readable_roots() {
        let skills = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let plugins = dshr_context::Context::new();
        plugins.add_group(BuiltinTools).unwrap();
        let mut ctx = ToolContext::new(dir.path().to_owned());
        ctx.plugins = plugins.snapshot();
        let registry = ToolRegistry::from_snapshot(&ctx.plugins);
        let access = |ctx: &ToolContext, name: &str, input: Value| registry.get(name).unwrap().access(&input, ctx);
        let secret = skills.path().join("SKILL.md").display().to_string();

        assert_eq!(access(&ctx, "read_file", serde_json::json!({"path": "src/a.rs"})), Access::Read);
        assert_eq!(access(&ctx, "list_dir", serde_json::json!({})), Access::Read);
        assert_eq!(access(&ctx, "read_file", serde_json::json!({"path": secret})), Access::ReadOutside(PathBuf::from(&secret)));
        assert!(matches!(access(&ctx, "grep", serde_json::json!({"pattern": "x", "path": ".."})), Access::ReadOutside(_)));
        assert!(matches!(access(&ctx, "glob", serde_json::json!({"pattern": "*", "path": "/"})), Access::ReadOutside(_)));

        // 插件登记的可读目录（Skills）：不用询问。
        struct Skills(PathBuf);
        impl dshr_context::Plugin for Skills {
            fn manifest(&self) -> dshr_context::Manifest {
                dshr_context::Manifest::new("test-skills")
            }
            fn apply(&self, cx: &mut dshr_context::Scope) -> anyhow::Result<()> {
                cx.contribute::<ReadableRoots>(self.0.clone());
                Ok(())
            }
        }
        plugins.add(Skills(skills.path().to_owned())).unwrap();
        ctx.plugins = plugins.snapshot();
        assert_eq!(access(&ctx, "read_file", serde_json::json!({"path": secret})), Access::Read);
    }

    #[test]
    fn line_endings() {
        assert!(is_crlf("a\r\nb\r\n") && is_crlf("a\r\nb"));
        assert!(!is_crlf("a\nb") && !is_crlf("a\r\nb\n") && !is_crlf("no newline"));
        assert_eq!(to_crlf("a\nb\r\nc\n"), "a\r\nb\r\nc\r\n");
    }

    #[tokio::test]
    async fn atomic_write_keeps_permissions_links_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sh");
        std::fs::write(&path, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
            write_atomic(path.clone(), "new".into()).await.unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o751);

            // 写链接：内容进了它指向的文件，链接还是链接。
            std::os::unix::fs::symlink(&path, dir.path().join("link")).unwrap();
            write_atomic(dir.path().join("link"), "via link".into()).await.unwrap();
            assert!(std::fs::symlink_metadata(dir.path().join("link")).unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "via link");
        }
        write_atomic(dir.path().join("fresh.txt"), "x".into()).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("fresh.txt")).unwrap(), "x");

        // 只读文件照旧拒绝，内容不变。
        let locked = dir.path().join("locked.txt");
        std::fs::write(&locked, "keep").unwrap();
        let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&locked, permissions).unwrap();
        assert!(write_atomic(locked.clone(), "nope".into()).await.is_err());
        assert_eq!(std::fs::read_to_string(&locked).unwrap(), "keep");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".dshr-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn access_kinds() {
        let ctx = ToolContext::new(PathBuf::from("/w"));
        let plugins = dshr_context::Context::new();
        plugins.add_group(BuiltinTools).unwrap();
        let registry = ToolRegistry::from_snapshot(&plugins.snapshot());
        let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(names, ["edit_file", "glob", "grep", "list_dir", "read_file", "run_shell", "update_plan", "write_file"]);
        let access = |name: &str, input: Value| registry.get(name).unwrap().access(&input, &ctx);
        assert_eq!(access("read_file", serde_json::json!({"path": "a"})), Access::Read);
        assert_eq!(access("grep", serde_json::json!({"pattern": "x"})), Access::Read);
        assert_eq!(access("edit_file", serde_json::json!({"path": "a"})), Access::Write(PathBuf::from("/w/a")));
        assert_eq!(access("run_shell", serde_json::json!({"command": "ls"})), Access::Execute);

        let read_only: Vec<_> = registry.read_only_specs(&ctx).into_iter().map(|spec| spec.name).collect();
        assert_eq!(read_only, ["glob", "grep", "list_dir", "read_file", "update_plan"]);
    }
}
