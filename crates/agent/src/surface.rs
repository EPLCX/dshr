//! 扩展点：画不了界面的插件（JS 插件）给界面加入口和页面，或者藏起内置的入口。
//!
//! 页面是**声明式**的：插件给出一棵 JSON 节点树（文字、按钮、输入框、开关、卡片……），界面用自己的组件画出来；
//! 用户点了按钮，界面把动作和输入框里的值交回插件，插件改自己的状态，界面再要一次新的树。
//! 插件的代码因此不在界面线程上跑，长得也和内置页面一样。节点的写法见 `docs/plugins.md` 的“界面扩展”。
//!
//! 这里的调用会执行插件代码，可能要等一会儿：界面应当在后台线程上调用 [`UiSurface::render`] 和 [`UiSurface::act`]。

use std::path::Path;
use std::sync::Arc;

use dshr_context::ExtensionPoint;
use serde_json::Value;

pub struct UiSurfaces;

impl ExtensionPoint for UiSurfaces {
    type Item = Arc<dyn UiSurface>;
}

/// 入口放在哪里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPlace {
    /// 侧边栏底部、“设置”上面；点了以弹窗打开页面。
    Sidebar,
    /// 设置弹窗里的一页。
    Settings,
    /// 输入卡片上方的可折叠面板，内容就是页面。
    Panel,
    /// 侧边栏底部的状态行：页面画出来的第一段文字就是状态，点了以弹窗打开整页。
    Status,
}

/// 插件的命令：进命令面板，可以带快捷键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEntry {
    pub id: String,
    pub title: String,
    /// 默认快捷键（`ctrl-shift-u` 这种写法）；和内置的冲突时不生效。
    pub keys: Option<String>,
}

/// 插件的斜杠命令：输入框里以 `/名字` 开头时出现在补全菜单里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashEntry {
    pub name: String,
    pub description: String,
}

/// 命令、斜杠命令执行完之后界面做什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    /// 什么都不做。
    Nothing,
    /// 显示一句话。
    Message(String),
    /// 把输入框里的文字换成这些。
    Insert(String),
    /// 直接发送这条消息。
    Send(String),
    /// 打开插件的这一页。
    Open(String),
}

/// 配色方案：`Palette` 字段名 → `#rrggbb`，浅色、深色各一套，只覆盖给出的键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeDef {
    pub plugin: String,
    pub id: String,
    pub name: String,
    pub light: std::collections::BTreeMap<String, String>,
    pub dark: std::collections::BTreeMap<String, String>,
}

/// 插件给自己的某个工具画的卡片（对应原版的 `tool.call.toolview`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCardView {
    /// 标题里跟在工具名后面的附加信息。
    pub title: Option<String>,
    /// 展开后的内容：一棵节点树（[`validate_tree`] 检查过）。
    pub body: Option<Value>,
}

/// 插件加的一个入口：点了打开它的某个页面。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiEntry {
    pub place: EntryPlace,
    /// 插件内唯一。
    pub id: String,
    pub label: String,
    /// 图标名（`star`、`globe` 等，见文档）；没有或不认识时用默认图标。
    pub icon: Option<String>,
    /// 打开的页面名。
    pub page: String,
}

/// 可以藏起来的内置入口：`settings:general`、`settings:models`、`composer:presets`、`composer:permission`、
/// `status:<id>`、`sidebar:<id>`。“设置 → 扩展”和侧边栏的“设置”不能藏：藏了就没法再把插件关掉。
pub fn hideable(target: &str) -> bool {
    match target.split_once(':') {
        Some(("settings", page)) => page != "extensions" && !page.is_empty(),
        Some(("composer", control)) => matches!(control, "presets" | "permission"),
        Some(("status" | "sidebar", id)) => !id.is_empty() && id != "settings",
        _ => false,
    }
}

pub trait UiSurface: Send + Sync {
    /// 提供它的插件。
    fn plugin(&self) -> &str;

    fn entries(&self) -> &[UiEntry];

    /// 要藏起来的内置入口（[`hideable`] 认可的那些）。
    fn hidden(&self) -> &[String];

    /// 页面现在的节点树。`workspace` 是当前打开的工作区，没有时插件的文件函数指向它自己的数据目录。
    fn render(&self, page: &str, workspace: Option<&Path>) -> Result<Value, String>;

    /// 用户在页面上触发了一个动作（点按钮、拨开关）。`payload` 里有节点上写的数据和各输入框现在的值。
    /// 返回给用户看的一句话（可以没有）。
    fn act(&self, page: &str, action: &str, payload: Value, workspace: Option<&Path>) -> Result<Option<String>, String>;

    /// 给哪些工具画了卡片（只能是插件自己的工具）。
    fn card_tools(&self) -> &[String] {
        &[]
    }

    /// 画一次工具调用的卡片。`output` 在结果到达之前是 `None`。界面应当在后台线程上调用，结果按输入缓存。
    fn card(&self, _tool: &str, _input: &Value, _output: Option<&str>, _is_error: bool) -> Result<ToolCardView, String> {
        Err("this plugin draws no tool cards".into())
    }

    /// 给哪些事件类型画了节点树（`events` 导出里的 `render`）。
    fn event_kinds(&self) -> &[String] {
        &[]
    }

    /// 把一条插件事件画成节点树，在对话区里显示。
    fn render_event(&self, _kind: &str, _data: &Value) -> Result<Value, String> {
        Err("this plugin renders no events".into())
    }

    fn commands(&self) -> &[CommandEntry] {
        &[]
    }

    /// 斜杠命令。可以随时变（比如 MCP 服务器的 prompts），所以每次问都给一份新的。
    fn slash_commands(&self) -> Vec<SlashEntry> {
        Vec::new()
    }

    /// 执行一个命令（`slash` 为真时是斜杠命令，`args` 是名字后面的文字）。用户亲手触发：插件可以写输入框。
    /// 界面应当在后台线程上调用。
    fn run_command(&self, _id: &str, _slash: bool, _args: &str, _workspace: Option<&Path>) -> Result<CommandResult, String> {
        Err("this plugin has no commands".into())
    }

    /// 插件带的配色方案。
    fn themes(&self) -> &[ThemeDef] {
        &[]
    }

    /// 页面 `page` 被脚本要求重画（`host.ui.refresh(page)`）的次数。界面在 [`page_refreshes`] 通知时问一次，
    /// 变了就重画已经打开的这一页。实现方在计数加一之后调用 [`notify_page_refresh`]。
    fn refresh_generation(&self, _page: &str) -> u64 {
        0
    }
}

/// 扩展点：输入框里 `@` 提及的来源（工单、文档、MCP 服务器的资源……）。选中的条目解析成一段文字，作为附件
/// 随消息发送（写进 `user/attachment`）。两个方法都在界面的后台线程上同步调用，要在几秒内返回。
pub struct MentionSources;

impl ExtensionPoint for MentionSources {
    type Item = Arc<dyn MentionSource>;
}

pub trait MentionSource: Send + Sync {
    /// 来源的 id（`<插件>:<id>`、`mcp`），也是附件路径的前缀：`@<id>:<条目 id>`。
    fn id(&self) -> &str;
    /// 菜单里的分组名。
    fn label(&self) -> &str;
    /// 图标名（同插件入口的图标），没有时用默认的。
    fn icon(&self) -> Option<&str> {
        None
    }
    /// 按输入的字找条目（最多 20 条）。`workspace` 是当前会话的工作区。
    fn search(&self, query: &str, workspace: Option<&Path>) -> Result<Vec<MentionItem>, String>;
    /// 选中之后：取到要进上下文的内容。
    fn resolve(&self, item: &MentionItem, workspace: Option<&Path>) -> Result<MentionContent, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionItem {
    pub id: String,
    pub title: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionContent {
    pub title: String,
    pub text: String,
}

/// 一个来源最多给多少条。
pub const MAX_MENTION_ITEMS: usize = 20;
/// 提及解析出的文字最多多大（和附件一样）。
pub const MAX_MENTION_BYTES: usize = 256 * 1024;

/// 内置的来源：工作区里的文件（`@file:<相对路径>`）。界面直接用它，不经插件。
pub struct FileMentions;

impl MentionSource for FileMentions {
    fn id(&self) -> &str {
        "file"
    }

    fn label(&self) -> &str {
        "Files"
    }

    fn icon(&self) -> Option<&str> {
        Some("file")
    }

    fn search(&self, query: &str, workspace: Option<&Path>) -> Result<Vec<MentionItem>, String> {
        let Some(workspace) = workspace else { return Ok(Vec::new()) };
        Ok(dshr_tools::find_files(workspace, query, MAX_MENTION_ITEMS)
            .into_iter()
            .map(|path| {
                let title = path.rsplit('/').next().unwrap_or(&path).to_owned();
                MentionItem { detail: Some(path.clone()), id: path, title }
            })
            .collect())
    }

    fn resolve(&self, item: &MentionItem, workspace: Option<&Path>) -> Result<MentionContent, String> {
        let workspace = workspace.ok_or("no workspace is open")?;
        let path = workspace.join(&item.id);
        let metadata = std::fs::metadata(&path).map_err(|error| format!("{}: {error}", item.id))?;
        if metadata.len() as usize > MAX_MENTION_BYTES {
            return Err(format!("{} is larger than {} KB", item.id, MAX_MENTION_BYTES / 1024));
        }
        let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", item.id))?;
        let text = String::from_utf8(bytes).map_err(|_| format!("{} is not a text file", item.id))?;
        Ok(MentionContent { title: item.id.clone(), text })
    }
}

static PAGE_REFRESHES: std::sync::LazyLock<tokio::sync::watch::Sender<u64>> = std::sync::LazyLock::new(|| tokio::sync::watch::channel(0).0);

/// 有插件页面要求重画了（任何一个插件的任何一页）：打开着的页面各自比一下自己的 [`UiSurface::refresh_generation`]。
pub fn notify_page_refresh() {
    PAGE_REFRESHES.send_modify(|generation| *generation += 1);
}

/// 订阅 [`notify_page_refresh`]。不依赖 tokio 运行时，界面线程上可以直接等；没有插件要求重画时不会被叫醒。
pub fn page_refreshes() -> tokio::sync::watch::Receiver<u64> {
    PAGE_REFRESHES.subscribe()
}

/// 页面节点树最多这么多个节点、这么多层；超了是插件写错了（多半是无限递归的数据）。
pub const MAX_NODES: usize = 1000;
/// 图表最多几个序列、每个多少点；热力图最多多少行、列；图片的 `src` 最长多少（base64 后约 4 MB）。
pub const MAX_SERIES: usize = 20;
pub const MAX_POINTS: usize = 2000;
pub const MAX_GRID: usize = 100;
pub const MAX_IMAGE_SRC: usize = 6 * 1024 * 1024;
pub const MAX_DEPTH: usize = 16;

/// 检查一棵页面节点树，返回节点数；不合规时说明是哪个节点、哪里不对。界面画之前、创造模式里重载时都用它。
///
/// 节点：`column` / `row`（`children`、可选 `gap`: small|medium|large）、`card`（`children`、可选 `title`）、
/// `text`（`text`、可选 `style`: title|heading|body|caption|mono、`tone`: neutral|accent|success|danger）、
/// `button`（`label`、`action`、可选 `payload`、`variant`: primary|outline|ghost|danger）、
/// `input`（`id`、可选 `label`、`placeholder`、`value`）、`switch`（`id`、`label`、`checked`、`action`）、
/// `list`（`items`: [{ `title`, 可选 `detail`、`action`、`payload` }]）、`badge`（`text`）、`divider`、
/// `markdown`（`text`）、`code`（`text`、可选 `language`）、`diff`（统一 diff 的 `text`，或 `before` + `after`，可选 `path`）、
/// `table`（`columns`、`rows`）、`select`（`id`、`options`、可选 `label`、`value`、`action`）、
/// `textarea`（`id`、可选 `label`、`placeholder`、`value`、`rows`）、`progress`（`value` 0–1、可选 `label`）、
/// `link`（`label`、`url`：http/https，点了经确认后用系统浏览器打开）。
/// 直接写一个字符串等于 `{ type: "text", text: … }`。
pub fn validate_tree(tree: &Value) -> Result<usize, String> {
    fn check(node: &Value, path: &str, depth: usize, count: &mut usize) -> Result<(), String> {
        *count += 1;
        if *count > MAX_NODES {
            return Err(format!("the page has more than {MAX_NODES} nodes"));
        }
        if depth > MAX_DEPTH {
            return Err(format!("{path}: nested deeper than {MAX_DEPTH} levels"));
        }
        if node.is_string() {
            return Ok(());
        }
        let Some(object) = node.as_object() else {
            return Err(format!("{path}: a node must be an object or a string, got {node}"));
        };
        let kind = object.get("type").and_then(Value::as_str).unwrap_or_default();
        let text = |key: &str| object.get(key).is_some_and(Value::is_string);
        let one_of =
            |key: &str, allowed: &[&str]| object.get(key).is_none_or(|value| value.as_str().is_some_and(|value| allowed.contains(&value)));
        let require = |ok: bool, what: &str| if ok { Ok(()) } else { Err(format!("{path} ({kind}): {what}")) };
        match kind {
            "column" | "row" | "card" => {
                require(one_of("gap", &["small", "medium", "large"]), "`gap` must be small, medium or large")?;
                require(object.get("title").is_none_or(Value::is_string), "`title` must be a string")?;
                let children = object.get("children").map(|children| children.as_array());
                let children = match children {
                    None => return Ok(()),
                    Some(Some(children)) => children,
                    Some(None) => return Err(format!("{path} ({kind}): `children` must be an array")),
                };
                for (index, child) in children.iter().enumerate() {
                    check(child, &format!("{path}.children[{index}]"), depth + 1, count)?;
                }
                Ok(())
            }
            "text" => {
                require(text("text"), "needs a string `text`")?;
                require(one_of("style", &["title", "heading", "body", "caption", "mono"]), "unknown `style`")?;
                require(one_of("tone", &["neutral", "accent", "success", "danger"]), "unknown `tone`")
            }
            "badge" => require(text("text"), "needs a string `text`"),
            "button" => {
                require(text("label") && text("action"), "needs a string `label` and `action`")?;
                require(one_of("variant", &["primary", "outline", "ghost", "danger"]), "unknown `variant`")
            }
            "input" => {
                require(text("id"), "needs a string `id`")?;
                require(
                    ["label", "placeholder", "value"].iter().all(|key| object.get(*key).is_none_or(Value::is_string)),
                    "`label`, `placeholder` and `value` must be strings",
                )
            }
            "switch" => require(text("id") && text("label") && text("action"), "needs a string `id`, `label` and `action`"),
            "list" => {
                let items = object.get("items").and_then(Value::as_array);
                let ok = items.is_some_and(|items| {
                    items
                        .iter()
                        .all(|item| item.get("title").is_some_and(Value::is_string) && item.get("action").is_none_or(Value::is_string))
                });
                *count += items.map_or(0, Vec::len);
                require(ok, "`items` must be an array of { title, detail?, action?, payload? }")
            }
            "divider" => Ok(()),
            "markdown" => require(text("text"), "needs a string `text`"),
            "code" => {
                require(text("text"), "needs a string `text`")?;
                require(object.get("language").is_none_or(Value::is_string), "`language` must be a string")
            }
            "diff" => {
                let pair = text("before") && text("after");
                require(text("text") || pair, "needs a unified diff in `text`, or `before` and `after`")?;
                require(object.get("path").is_none_or(Value::is_string), "`path` must be a string")
            }
            "table" => {
                let columns = object.get("columns").and_then(Value::as_array);
                let rows = object.get("rows").and_then(Value::as_array);
                let width = columns.map_or(0, Vec::len);
                let ok = columns.is_some_and(|columns| !columns.is_empty() && columns.iter().all(Value::is_string))
                    && rows.is_some_and(|rows| {
                        rows.iter().all(|row| {
                            row.as_array().is_some_and(|cells| {
                                cells.len() == width
                                    && cells.iter().all(|cell| cell.is_string() || cell.is_number() || cell.is_boolean() || cell.is_null())
                            })
                        })
                    });
                *count += rows.map_or(0, Vec::len);
                require(ok, "needs `columns` (strings) and `rows` (arrays of strings or numbers, one per column)")
            }
            "select" => {
                require(text("id"), "needs a string `id`")?;
                let options = object.get("options").and_then(Value::as_array);
                let ok = options.is_some_and(|options| {
                    !options.is_empty()
                        && options.iter().all(|option| {
                            option.is_string()
                                || (option.get("value").is_some_and(Value::is_string) && option.get("label").is_none_or(Value::is_string))
                        })
                });
                require(ok, "`options` must be a non-empty array of strings or { value, label? }")?;
                require(
                    ["label", "value", "action"].iter().all(|key| object.get(*key).is_none_or(Value::is_string)),
                    "`label`, `value` and `action` must be strings",
                )
            }
            "textarea" => {
                require(text("id"), "needs a string `id`")?;
                require(
                    ["label", "placeholder", "value"].iter().all(|key| object.get(*key).is_none_or(Value::is_string)),
                    "`label`, `placeholder` and `value` must be strings",
                )?;
                require(
                    object.get("rows").is_none_or(|rows| rows.as_u64().is_some_and(|rows| (1..=40).contains(&rows))),
                    "`rows` must be 1 to 40",
                )
            }
            "progress" => {
                let value = object.get("value").and_then(Value::as_f64);
                require(value.is_some_and(|value| (0.0..=1.0).contains(&value)), "needs a numeric `value` from 0 to 1")?;
                require(object.get("label").is_none_or(Value::is_string), "`label` must be a string")
            }
            "link" => {
                require(text("label") && text("url"), "needs a string `label` and `url`")?;
                let url = object.get("url").and_then(Value::as_str).unwrap_or_default();
                require(url.starts_with("https://") || url.starts_with("http://"), "`url` must be an http or https URL")
            }
            "chart" => {
                let kind = object.get("kind").and_then(Value::as_str).unwrap_or_default();
                require(
                    matches!(kind, "line" | "bar" | "area" | "pie" | "candlestick"),
                    "`kind` must be line, bar, area, pie or candlestick",
                )?;
                let width = if kind == "candlestick" { 5 } else { 2 };
                let series = object.get("series").and_then(Value::as_array);
                let ok = series.is_some_and(|series| {
                    !series.is_empty()
                        && series.len() <= MAX_SERIES
                        && series.iter().all(|one| {
                            one.get("name").is_none_or(Value::is_string)
                                && one.get("data").and_then(Value::as_array).is_some_and(|data| {
                                    data.len() <= MAX_POINTS
                                        && data.iter().all(|point| {
                                            point.as_array().is_some_and(|point| {
                                                point.len() == width
                                                    && (point[0].is_string() || point[0].is_number())
                                                    && point[1..].iter().all(Value::is_number)
                                            })
                                        })
                                })
                        })
                });
                *count += series.map_or(0, Vec::len);
                require(
                    ok,
                    "`series` must be 1–20 of { name?, data: [[x, y], …] } ([x, open, high, low, close] for candlestick, at most 2000 points)",
                )?;
                require(
                    object.get("height").is_none_or(|height| height.as_u64().is_some_and(|height| (80..=600).contains(&height))),
                    "`height` must be 80 to 600",
                )
            }
            "heatmap" => {
                let labels = |key: &str| {
                    object.get(key).and_then(Value::as_array).filter(|labels| labels.iter().all(Value::is_string)).map(Vec::len)
                };
                let (rows, columns) = (labels("rows"), labels("columns"));
                let values = object.get("values").and_then(Value::as_array);
                let ok = matches!((rows, columns), (Some(rows), Some(columns)) if rows <= MAX_GRID && columns <= MAX_GRID)
                    && values.is_some_and(|values| {
                        values.len() == rows.unwrap_or(0)
                            && values.iter().all(|row| {
                                row.as_array().is_some_and(|row| {
                                    row.len() == columns.unwrap_or(0) && row.iter().all(|cell| cell.is_number() || cell.is_null())
                                })
                            })
                    });
                *count += rows.unwrap_or(0);
                require(ok, "needs `rows` and `columns` (labels, at most 100 each) and `values` (one row of numbers per row label)")
            }
            "calendar" => {
                let month = object.get("month").and_then(Value::as_str).unwrap_or_default();
                let valid_month = month.len() == 7
                    && month.as_bytes()[4] == b'-'
                    && month[..4].parse::<u32>().is_ok()
                    && month[5..].parse::<u32>().is_ok_and(|value| (1..=12).contains(&value));
                require(valid_month, "`month` must look like 2026-11")?;
                require(
                    object.get("marks").is_none_or(|marks| marks.as_object().is_some_and(|marks| marks.values().all(Value::is_string))),
                    "`marks` must map dates (2026-11-16) to strings",
                )?;
                require(object.get("action").is_none_or(Value::is_string), "`action` must be a string")
            }
            "kv" => {
                let items = object.get("items").and_then(Value::as_array);
                let ok = items.is_some_and(|items| {
                    items.iter().all(|item| {
                        item.get("key").is_some_and(Value::is_string)
                            && item.get("value").is_some_and(|value| !value.is_object() && !value.is_array())
                    })
                });
                *count += items.map_or(0, Vec::len);
                require(ok, "`items` must be an array of { key, value }")
            }
            "image" => {
                let src = object.get("src").and_then(Value::as_str).unwrap_or_default();
                let ok = src.starts_with("data:image/")
                    || (!src.is_empty() && !src.contains("://") && !src.starts_with('/') && !src.split('/').any(|part| part == ".."));
                require(ok, "`src` must be a data:image/… URI, or a relative path (data/… for the plugin's data folder)")?;
                require(src.len() <= MAX_IMAGE_SRC, "the image is larger than 4 MB")?;
                require(object.get("alt").is_none_or(Value::is_string), "`alt` must be a string")?;
                require(
                    object.get("width").is_none_or(|width| width.as_u64().is_some_and(|width| (16..=1200).contains(&width))),
                    "`width` must be 16 to 1200",
                )
            }
            "" => Err(format!("{path}: a node needs a `type`")),
            other => Err(format!("{path}: unknown node type `{other}`")),
        }
    }
    let mut count = 0;
    check(tree, "page", 0, &mut count)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn trees_and_hide_targets_are_checked() {
        let page = json!({ "type": "column", "children": [
            "plain text",
            { "type": "text", "text": "Title", "style": "title" },
            { "type": "row", "gap": "small", "children": [
                { "type": "input", "id": "name", "placeholder": "Name" },
                { "type": "button", "label": "Add", "action": "add", "variant": "primary" }
            ]},
            { "type": "list", "items": [{ "title": "a", "action": "open", "payload": 1 }] },
            { "type": "switch", "id": "on", "label": "On", "checked": true, "action": "toggle" },
            { "type": "divider" }
        ]});
        assert_eq!(validate_tree(&page), Ok(10));
        let bad = json!({ "type": "column", "children": [{ "type": "button", "label": "x" }] });
        assert!(validate_tree(&bad).unwrap_err().contains("page.children[0] (button)"));
        assert!(validate_tree(&json!({ "type": "gauge" })).unwrap_err().contains("unknown node type `gauge`"));
        assert!(validate_tree(&json!({ "type": "chart" })).unwrap_err().contains("`kind`"));
        let chart = json!({ "type": "chart", "kind": "line", "series": [{ "name": "tokens", "data": [["mon", 1], ["tue", 2.5]] }] });
        assert!(validate_tree(&chart).is_ok());
        let candles = json!({ "type": "chart", "kind": "candlestick", "series": [{ "data": [["d1", 1, 3, 0.5, 2]] }] });
        assert!(validate_tree(&candles).is_ok());
        assert!(validate_tree(&json!({ "type": "chart", "kind": "bar", "series": [{ "data": [[1]] }] })).is_err());
        let heat = json!({ "type": "heatmap", "rows": ["a"], "columns": ["x", "y"], "values": [[1, null]] });
        assert!(validate_tree(&heat).is_ok());
        assert!(validate_tree(&json!({ "type": "heatmap", "rows": ["a"], "columns": ["x"], "values": [[1, 2]] })).is_err());
        assert!(validate_tree(&json!({ "type": "calendar", "month": "2026-11", "marks": { "2026-11-16": "3" } })).is_ok());
        assert!(validate_tree(&json!({ "type": "calendar", "month": "2026-13" })).is_err());
        assert!(validate_tree(&json!({ "type": "kv", "items": [{ "key": "a", "value": 1 }] })).is_ok());
        assert!(validate_tree(&json!({ "type": "image", "src": "data/chart.png", "alt": "chart" })).is_ok());
        assert!(validate_tree(&json!({ "type": "image", "src": "https://x/y.png" })).is_err());
        assert!(validate_tree(&json!({ "type": "image", "src": "../secret.png" })).is_err());
        assert!(validate_tree(&json!(42)).is_err());

        assert!(hideable("settings:models") && hideable("composer:presets") && hideable("status:account"));
        assert!(!hideable("settings:extensions") && !hideable("sidebar:settings") && !hideable("composer:other") && !hideable("x"));
    }
}
