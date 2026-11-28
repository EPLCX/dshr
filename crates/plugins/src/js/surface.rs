//! JS 插件的界面扩展（见 `dshr_agent::UiSurface`）：`ui` 导出的入口和页面、`cards` 导出的工具卡片、
//! `events` 导出里的 `render`（对话区里的节点树）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dshr_agent::{CommandEntry, CommandResult, EntryPlace, SlashEntry, ThemeDef, ToolCardView, UiEntry, UiSurface, validate_tree};
use dshr_tools::ToolContext;
use serde_json::{Value, json};

use super::host::Permits;
use super::runtime::{EntryDecl, Instance, Job};
use crate::manifest::{Capabilities, PluginManifest};

/// 画一页最多等这么久。界面在后台线程上等，不会卡住；等不到时页面显示原因，可以重试。
const RENDER_WAIT: Duration = Duration::from_secs(3);
/// 卡片、事件的节点树最多等这么久：对话区在后台线程上同步地问（和 `label` 一样，见 `dshr_ui` 的
/// `PluginViewResolver`），等不到就用通用的显示。打开会话时每条事件都要问一次，所以等得短。
const CARD_WAIT: Duration = Duration::from_millis(300);
const EVENT_WAIT: Duration = Duration::from_millis(200);
/// 交给卡片函数的工具结果最多这么多字符。
const MAX_CARD_OUTPUT: usize = 64 * 1024;

pub(super) struct JsSurface {
    plugin: &'static str,
    entries: Vec<UiEntry>,
    hidden: Vec<String>,
    cards: Vec<String>,
    renders: Vec<String>,
    commands: Vec<CommandEntry>,
    slash: Vec<SlashEntry>,
    themes: Vec<ThemeDef>,
    capabilities: Capabilities,
    /// 动作的时限：和工具调用一样。
    timeout: Duration,
    /// 没有打开的工作区时，文件函数指向这里：`<数据目录>/plugin-data/<id>/`。
    data_dir: PathBuf,
    instance: Arc<Instance>,
}

impl JsSurface {
    pub fn new(plugin: &'static str, manifest: &PluginManifest, timeout: Duration, instance: Arc<Instance>) -> Self {
        let ui = &instance.exports.ui;
        let entry = |place: EntryPlace, decl: &EntryDecl| UiEntry {
            place,
            id: decl.id.clone(),
            label: decl.label.clone(),
            icon: decl.icon.clone(),
            page: decl.page.clone(),
        };
        let entries = ui
            .sidebar
            .iter()
            .map(|decl| entry(EntryPlace::Sidebar, decl))
            .chain(ui.settings.iter().map(|decl| entry(EntryPlace::Settings, decl)))
            .chain(ui.panels.iter().map(|decl| entry(EntryPlace::Panel, decl)))
            .chain(ui.status.iter().map(|decl| entry(EntryPlace::Status, decl)))
            .collect();
        Self {
            plugin,
            entries,
            hidden: ui.hide.iter().filter(|target| dshr_agent::hideable(target)).cloned().collect(),
            cards: instance.exports.cards.clone(),
            renders: instance.exports.renders.clone(),
            commands: instance
                .exports
                .commands
                .iter()
                .map(|command| CommandEntry { id: command.id.clone(), title: command.title.clone(), keys: command.keys.clone() })
                .collect(),
            slash: instance
                .exports
                .slash
                .iter()
                .map(|command| SlashEntry { name: command.name.clone(), description: command.description.clone() })
                .collect(),
            themes: instance
                .exports
                .themes
                .iter()
                .map(|theme| ThemeDef {
                    plugin: plugin.to_owned(),
                    id: theme.id.clone(),
                    name: theme.name.clone(),
                    light: theme.light.clone(),
                    dark: theme.dark.clone(),
                })
                .collect(),
            capabilities: manifest.capabilities.clone(),
            timeout,
            data_dir: data_dir(manifest),
            instance,
        }
    }

    fn context(&self, workspace: Option<&Path>) -> Result<ToolContext, String> {
        match workspace {
            Some(workspace) => Ok(ToolContext::new(workspace.to_owned())),
            None => {
                std::fs::create_dir_all(&self.data_dir).map_err(|error| format!("cannot create {}: {error}", self.data_dir.display()))?;
                Ok(ToolContext::new(self.data_dir.clone()))
            }
        }
    }

    fn tree(text: Option<String>, what: &str) -> Result<Value, String> {
        let tree: Value = serde_json::from_str(&text.unwrap_or_default())
            .map_err(|error| format!("{what} returned something that is not JSON: {error}"))?;
        validate_tree(&tree)?;
        Ok(tree)
    }

    /// 把 `image`、`avatar` 节点里的文件路径换成 `data:` URI：`data/…` 是插件自己的数据目录，其余是工作区里的路径
    /// （要 `fs:workspace:read`）。读不了的图换成一段说明文字（头像去掉图片，只显示首字）。界面只认 `data:` URI。
    /// 子节点在 `children`、`actions`、`left`、`right` 和标签页各页的 `children` 里。
    fn inline_images(&self, node: &mut Value, workspace: Option<&Path>) {
        use base64::Engine as _;
        for key in ["children", "actions", "left", "right"] {
            if let Some(children) = node.get_mut(key).and_then(Value::as_array_mut) {
                for child in children {
                    self.inline_images(child, workspace);
                }
            }
        }
        if let Some(tabs) = node.get_mut("tabs").and_then(Value::as_array_mut) {
            for child in tabs.iter_mut().filter_map(|tab| tab.get_mut("children")).filter_map(Value::as_array_mut).flatten() {
                self.inline_images(child, workspace);
            }
        }
        let kind = node.get("type").and_then(Value::as_str);
        if kind != Some("image") && !(kind == Some("avatar") && node.get("src").is_some()) {
            return;
        }
        let avatar = kind == Some("avatar");
        let src = node.get("src").and_then(Value::as_str).unwrap_or_default().to_owned();
        if src.starts_with("data:") {
            return;
        }
        let path = match src.strip_prefix("data/") {
            Some(relative) => Some(self.data_dir.join(relative)),
            None if self.capabilities.fs_read => workspace.map(|workspace| workspace.join(&src)),
            None => None,
        };
        let media_type = match src.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("svg") => "image/svg+xml",
            _ => "application/octet-stream",
        };
        let bytes = path
            .filter(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.len() <= 4 * 1024 * 1024))
            .and_then(|path| std::fs::read(path).ok());
        *node = match bytes {
            Some(bytes) => {
                let mut image = node.clone();
                image["src"] =
                    Value::String(format!("data:{media_type};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)));
                image
            }
            None if avatar => {
                let mut avatar = node.clone();
                if let Some(object) = avatar.as_object_mut() {
                    object.remove("src");
                }
                avatar
            }
            None => serde_json::json!({ "type": "text", "text": format!("[image {src} cannot be shown]"), "style": "caption" }),
        };
    }
}

/// `<数据目录>/plugin-data/<id>/`：插件目录是 `<数据目录>/plugins/<id>/`。
pub(super) fn data_dir(manifest: &PluginManifest) -> PathBuf {
    let fallback = manifest.dir.parent().and_then(Path::parent).unwrap_or(&manifest.dir);
    manifest.home.as_deref().unwrap_or(fallback).join("plugin-data").join(&manifest.id)
}

impl UiSurface for JsSurface {
    fn plugin(&self) -> &str {
        self.plugin
    }

    fn entries(&self) -> &[UiEntry] {
        &self.entries
    }

    fn hidden(&self) -> &[String] {
        &self.hidden
    }

    fn render(&self, page: &str, workspace: Option<&Path>) -> Result<Value, String> {
        let ctx = self.context(workspace)?;
        // 画页面只读：和 `prompt` 一样，有读能力就能读文件，不能写、不能联网。
        let text = self.instance.ask(Job::Render { page: page.to_owned() }, Some(ctx), RENDER_WAIT)?;
        let mut tree = Self::tree(text, "render()")?;
        self.inline_images(&mut tree, workspace);
        Ok(tree)
    }

    fn act(&self, page: &str, action: &str, payload: Value, workspace: Option<&Path>) -> Result<Option<String>, String> {
        let ctx = self.context(workspace)?;
        let job = Job::Act { page: page.to_owned(), action: action.to_owned(), payload: payload.to_string() };
        // 用户亲手点的：按插件的能力写文件、联网、启动程序、调模型、替用户做事，不再询问。
        let permits = Permits {
            write: self.capabilities.fs_write,
            net: !self.capabilities.net.is_empty(),
            exec: !self.capabilities.exec.is_empty(),
            llm: self.capabilities.llm,
            act: self.capabilities.acts_for_user(),
        };
        self.instance.ask_with(job, Some(super::app::user_context(ctx)), self.timeout, permits)
    }

    fn card_tools(&self) -> &[String] {
        &self.cards
    }

    fn card(&self, tool: &str, input: &Value, output: Option<&str>, is_error: bool) -> Result<ToolCardView, String> {
        if !self.cards.iter().any(|known| known == tool) {
            return Err(format!("no card for tool {tool}"));
        }
        let output = output.map(|text| match text.char_indices().nth(MAX_CARD_OUTPUT) {
            Some((cut, _)) => &text[..cut],
            None => text,
        });
        let ask = |part: &str| {
            let payload = json!({ "part": part, "input": input, "output": output, "isError": is_error });
            let job = Job::Dispatch { kind: "card", name: tool.to_owned(), payload: payload.to_string() };
            // 卡片是纯展示：没有工作区，不能读写文件、不能联网。
            self.instance.ask(job, None, CARD_WAIT)
        };
        let title = match ask("title")? {
            Some(text) => serde_json::from_str::<Value>(&text).ok().and_then(|value| value.as_str().map(str::to_owned)),
            None => None,
        };
        let body = match ask("render")? {
            Some(text) if text != "null" => {
                let mut tree = Self::tree(Some(text), "the card's render()")?;
                self.inline_images(&mut tree, None);
                Some(tree)
            }
            _ => None,
        };
        Ok(ToolCardView { title, body })
    }

    fn event_kinds(&self) -> &[String] {
        &self.renders
    }

    fn render_event(&self, kind: &str, data: &Value) -> Result<Value, String> {
        let job = Job::Dispatch { kind: "event", name: kind.to_owned(), payload: data.to_string() };
        let mut tree = Self::tree(self.instance.ask(job, None, EVENT_WAIT)?, "the event's render()")?;
        self.inline_images(&mut tree, None);
        Ok(tree)
    }

    fn commands(&self) -> &[CommandEntry] {
        &self.commands
    }

    fn slash_commands(&self) -> Vec<SlashEntry> {
        self.slash.clone()
    }

    fn themes(&self) -> &[ThemeDef] {
        &self.themes
    }

    fn run_command(&self, id: &str, slash: bool, args: &str, workspace: Option<&Path>) -> Result<CommandResult, String> {
        let ctx = super::app::user_context(self.context(workspace)?);
        let job = Job::Dispatch {
            kind: if slash { "slash" } else { "command" },
            name: id.to_owned(),
            payload: serde_json::json!({ "args": args }).to_string(),
        };
        let permits = Permits {
            write: self.capabilities.fs_write,
            net: !self.capabilities.net.is_empty(),
            exec: !self.capabilities.exec.is_empty(),
            llm: self.capabilities.llm,
            act: self.capabilities.acts_for_user(),
        };
        let answer: Value = match self.instance.ask_with(job, Some(ctx), self.timeout, permits)? {
            Some(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
            None => Value::Null,
        };
        Ok(match answer {
            Value::String(text) => CommandResult::Message(text),
            Value::Object(object) => {
                let field = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
                if let Some(text) = field("insert") {
                    CommandResult::Insert(text)
                } else if let Some(text) = field("send") {
                    CommandResult::Send(text)
                } else if let Some(page) = field("open") {
                    CommandResult::Open(page)
                } else if let Some(text) = field("message") {
                    CommandResult::Message(text)
                } else {
                    CommandResult::Nothing
                }
            }
            _ => CommandResult::Nothing,
        })
    }

    fn refresh_generation(&self, page: &str) -> u64 {
        self.instance.refresh_generation(page)
    }
}
