//! `export const mentions = [{ id, label, icon, search(query, host), resolve(id, host) }]`：输入框 `@` 提及的来源。
//!
//! 用户亲手触发（打 `@`、选中一项），所以和页面动作一样可以读工作区、联网（按能力）；不写、不启动程序、不调模型。
//! `search` 每次输入停顿后问一次，最多等 2 秒；`resolve` 最多等 10 秒，结果最多 256 KB。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use dshr_agent::{MAX_MENTION_BYTES, MentionContent, MentionItem, MentionSource};
use dshr_tools::ToolContext;
use serde_json::{Value, json};

use super::host::Permits;
use super::runtime::{Instance, Job, MentionDecl};
use crate::manifest::Capabilities;

const SEARCH_WAIT: Duration = Duration::from_secs(2);
const RESOLVE_WAIT: Duration = Duration::from_secs(10);

pub(super) struct JsMention {
    id: String,
    decl: MentionDecl,
    instance: Arc<Instance>,
    permits: Permits,
}

impl JsMention {
    pub fn new(plugin: &str, decl: MentionDecl, capabilities: &Capabilities, instance: Arc<Instance>) -> Self {
        Self {
            id: format!("{plugin}:{}", decl.id),
            decl,
            instance,
            permits: Permits { write: false, net: !capabilities.net.is_empty(), exec: false, llm: false, act: false },
        }
    }

    fn ask(&self, payload: Value, workspace: Option<&Path>, wait: Duration) -> Result<Value, String> {
        let job = Job::Dispatch { kind: "mention", name: self.decl.id.clone(), payload: payload.to_string() };
        let ctx = workspace.map(|workspace| ToolContext::new(workspace.to_owned()));
        let text = self.instance.ask_with(job, ctx, wait, self.permits)?.unwrap_or_else(|| "null".into());
        serde_json::from_str(&text).map_err(|error| format!("the mention source returned bad JSON: {error}"))
    }
}

impl MentionSource for JsMention {
    fn id(&self) -> &str {
        &self.id
    }

    fn label(&self) -> &str {
        &self.decl.label
    }

    fn icon(&self) -> Option<&str> {
        self.decl.icon.as_deref()
    }

    fn search(&self, query: &str, workspace: Option<&Path>) -> Result<Vec<MentionItem>, String> {
        let answer = self.ask(json!({ "part": "search", "query": query }), workspace, SEARCH_WAIT)?;
        let items = answer.as_array().map(Vec::as_slice).unwrap_or_default();
        Ok(items
            .iter()
            .take(dshr_agent::MAX_MENTION_ITEMS)
            .filter_map(|item| {
                let id = item["id"].as_str().filter(|id| !id.is_empty())?.to_owned();
                Some(MentionItem {
                    title: item["title"].as_str().unwrap_or(&id).to_owned(),
                    detail: item["detail"].as_str().map(str::to_owned),
                    id,
                })
            })
            .collect())
    }

    fn resolve(&self, item: &MentionItem, workspace: Option<&Path>) -> Result<MentionContent, String> {
        let answer = self.ask(json!({ "part": "resolve", "id": item.id, "title": item.title }), workspace, RESOLVE_WAIT)?;
        let mut text = answer["text"].as_str().unwrap_or_default().to_owned();
        if text.len() > MAX_MENTION_BYTES {
            let mut cut = MAX_MENTION_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        Ok(MentionContent { title: answer["title"].as_str().unwrap_or(&item.title).to_owned(), text })
    }
}
