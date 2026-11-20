//! MCP 服务器的 prompts 出现在输入框的 `/` 菜单里：`/mcp:<服务器>:<prompt> key=value …`。
//! 选中后取到 prompt 的文字放进输入框（不直接发送：先让用户看一眼）。

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use dshr_agent::{CommandResult, SlashEntry, UiEntry, UiSurface};
use serde_json::{Map, Value};

/// 取 prompt 最多等多久。
const PROMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// 服务器所在的 tokio 运行时：界面在自己的后台线程上执行斜杠命令，那里没有运行时。
static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// 记下运行时（第一次连接服务器时）。
pub(crate) fn remember_runtime() {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let _ = RUNTIME.set(handle);
    }
}

/// 在服务器所在的运行时上同步地跑一段异步代码（界面的后台线程上调用），最多等 `limit`。
fn block_on<T>(limit: Duration, work: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    let runtime = RUNTIME.get().ok_or("MCP 服务器还没有启动")?;
    let timed = async move {
        match tokio::time::timeout(limit, work).await {
            Ok(result) => result,
            Err(_) => Err(format!("{} 秒内没有结果", limit.as_secs())),
        }
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| runtime.block_on(timed))
    } else {
        runtime.block_on(timed)
    }
}

/// MCP 服务器的资源出现在输入框的 `@` 菜单里。条目 id 是“服务器名\nURI”。
pub(crate) struct ResourceMentions;

impl dshr_agent::MentionSource for ResourceMentions {
    fn id(&self) -> &str {
        "mcp"
    }

    fn label(&self) -> &str {
        "MCP"
    }

    fn icon(&self) -> Option<&str> {
        Some("network")
    }

    fn search(&self, query: &str, _workspace: Option<&Path>) -> Result<Vec<dshr_agent::MentionItem>, String> {
        let query = query.to_lowercase();
        let resources = block_on(Duration::from_secs(5), async { Ok(crate::servers::list_all_resources().await) })?;
        Ok(resources
            .into_iter()
            .filter(|(_, resource)| {
                query.is_empty() || resource.name.to_lowercase().contains(&query) || resource.uri.to_lowercase().contains(&query)
            })
            .take(dshr_agent::MAX_MENTION_ITEMS)
            .map(|(server, resource)| dshr_agent::MentionItem {
                id: format!("{server}\n{}", resource.uri),
                title: resource.name,
                detail: Some(format!("{server} · {}", resource.uri)),
            })
            .collect())
    }

    fn resolve(&self, item: &dshr_agent::MentionItem, _workspace: Option<&Path>) -> Result<dshr_agent::MentionContent, String> {
        let (server, uri) = item.id.split_once('\n').ok_or("bad resource id")?;
        let (server, uri) = (server.to_owned(), uri.to_owned());
        let text = block_on(PROMPT_TIMEOUT, async move { crate::servers::read_resource_text(&server, &uri).await })?;
        let mut text = text;
        if text.len() > dshr_agent::MAX_MENTION_BYTES {
            let mut cut = dshr_agent::MAX_MENTION_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        Ok(dshr_agent::MentionContent { title: item.title.clone(), text })
    }
}

pub(crate) struct PromptSurface;

impl UiSurface for PromptSurface {
    fn plugin(&self) -> &str {
        "mcp"
    }

    fn entries(&self) -> &[UiEntry] {
        &[]
    }

    fn hidden(&self) -> &[String] {
        &[]
    }

    fn render(&self, _page: &str, _workspace: Option<&Path>) -> Result<Value, String> {
        Err("MCP has no pages".into())
    }

    fn act(&self, _page: &str, _action: &str, _payload: Value, _workspace: Option<&Path>) -> Result<Option<String>, String> {
        Err("MCP has no pages".into())
    }

    fn slash_commands(&self) -> Vec<SlashEntry> {
        crate::servers::all_prompts()
            .into_iter()
            .map(|(server, prompt)| {
                let mut description = prompt.description.clone();
                let arguments: Vec<String> = prompt
                    .arguments
                    .iter()
                    .map(|argument| if argument.required { format!("{}=…", argument.name) } else { format!("[{}=…]", argument.name) })
                    .collect();
                if !arguments.is_empty() {
                    description = format!("{description} ({})", arguments.join(" ")).trim().to_owned();
                }
                SlashEntry { name: format!("mcp:{server}:{}", prompt.name), description }
            })
            .collect()
    }

    fn run_command(&self, id: &str, _slash: bool, args: &str, _workspace: Option<&Path>) -> Result<CommandResult, String> {
        let (server, name) = id.strip_prefix("mcp:").and_then(|rest| rest.split_once(':')).ok_or_else(|| format!("不认识的命令 {id}"))?;
        let prompt = crate::servers::all_prompts()
            .into_iter()
            .find(|(owner, prompt)| owner == server && prompt.name == name)
            .map(|(_, prompt)| prompt)
            .ok_or_else(|| format!("MCP 服务器 {server} 现在没有 prompt {name}"))?;
        let arguments = match parse_arguments(&prompt, args) {
            Ok(arguments) => arguments,
            Err(message) => return Ok(CommandResult::Message(message)),
        };
        let (server, name) = (server.to_owned(), name.to_owned());
        let text = block_on(PROMPT_TIMEOUT, async move { crate::servers::get_prompt_text(&server, &name, &arguments).await })?;
        Ok(CommandResult::Insert(text))
    }
}

/// 斜杠命令后面的参数：`key=value`（值可以用双引号括起来）；prompt 只有一个参数时，整段文字就是它的值。
/// 缺必填参数时返回一句用法说明。
fn parse_arguments(prompt: &crate::PromptInfo, args: &str) -> Result<Map<String, Value>, String> {
    let mut arguments = Map::new();
    let args = args.trim();
    let named = split_words(args);
    if prompt.arguments.len() == 1 && !args.is_empty() && !named.iter().any(|word| word.contains('=')) {
        arguments.insert(prompt.arguments[0].name.clone(), Value::String(args.to_owned()));
    } else {
        for word in named {
            let (key, value) = word.split_once('=').ok_or_else(|| usage(prompt))?;
            if !prompt.arguments.iter().any(|argument| argument.name == key) {
                return Err(format!("没有参数 {key}。{}", usage(prompt)));
            }
            arguments.insert(key.to_owned(), Value::String(value.to_owned()));
        }
    }
    let missing: Vec<&str> = prompt
        .arguments
        .iter()
        .filter(|argument| argument.required && !arguments.contains_key(&argument.name))
        .map(|argument| argument.name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(format!("缺少参数 {}。{}", missing.join("、"), usage(prompt)));
    }
    Ok(arguments)
}

fn usage(prompt: &crate::PromptInfo) -> String {
    let arguments: Vec<String> = prompt.arguments.iter().map(|argument| format!("{}=…", argument.name)).collect();
    format!("用法：{} {}", prompt.name, arguments.join(" "))
}

/// 按空白分词，双引号里的空白不算。
fn split_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PromptArgument, PromptInfo};

    fn prompt(arguments: &[(&str, bool)]) -> PromptInfo {
        PromptInfo {
            name: "review".into(),
            description: String::new(),
            arguments: arguments
                .iter()
                .map(|(name, required)| PromptArgument { name: (*name).into(), description: String::new(), required: *required })
                .collect(),
        }
    }

    #[test]
    fn arguments_are_parsed_from_key_values_or_whole_text() {
        let two = prompt(&[("file", true), ("style", false)]);
        let parsed = parse_arguments(&two, r#"file=src/main.rs style="very strict""#).unwrap();
        assert_eq!(parsed["file"], "src/main.rs");
        assert_eq!(parsed["style"], "very strict");
        assert!(parse_arguments(&two, "style=x").unwrap_err().contains("缺少参数 file"));
        assert!(parse_arguments(&two, "color=red file=x").unwrap_err().contains("没有参数 color"));

        let one = prompt(&[("topic", true)]);
        assert_eq!(parse_arguments(&one, "rust error handling").unwrap()["topic"], "rust error handling");
        assert!(parse_arguments(&prompt(&[]), "").unwrap().is_empty());
    }
}
