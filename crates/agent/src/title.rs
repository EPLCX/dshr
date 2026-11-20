//! 会话标题：首条消息写入后在后台生成，写入 `session/title`。

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use dshr_context::{ExtensionPoint, Manifest, Plugin, Scope};
use dshr_core::{ContentBlock, Message, Role};
use dshr_llm::{ChatRequest, Provider};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{hooks::complete_text, session::write_title};

const TITLE_PROMPT: &str = "Write a short title for this conversation: at most 8 words, or at most 16 characters \
if the user writes in Chinese, in the language the user wrote in. Reply with the title only, without quotes or \
trailing punctuation.";
const TITLE_CONTEXT_CHARS: usize = 2000;
const TITLE_MAX_CHARS: usize = 40;

pub struct TitlePlugin;

impl Plugin for TitlePlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("title")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<AutoTitle>(());
        Ok(())
    }
}

pub(crate) struct AutoTitle;

impl ExtensionPoint for AutoTitle {
    type Item = ();
}

static TITLE_CHANGES: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);

/// 订阅后台标题落盘的通知；通知独立于对话事件流，可以在回合结束后送达。
pub fn title_changes() -> watch::Receiver<u64> {
    TITLE_CHANGES.subscribe()
}

pub(crate) fn spawn_title(path: PathBuf, user: &str, model: String, provider: Arc<dyn Provider>, cancel: CancellationToken) {
    let request = ChatRequest {
        model,
        system: Some(TITLE_PROMPT.into()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::text(user.chars().take(TITLE_CONTEXT_CHARS).collect::<String>())],
        }],
        tools: Vec::new(),
        // 有的模型即使不要求也会先输出思考，不限制输出长度。
        max_tokens: None,
        effort: None,
    };
    tokio::spawn(async move {
        let Some(text) = complete_text(provider.as_ref(), request, &cancel).await else { return };
        let title = clean_title(&text);
        if title.is_empty() || cancel.is_cancelled() {
            return;
        }
        let result = tokio::task::spawn_blocking(move || write_title(&path, title)).await;
        match result {
            Ok(Ok(true)) => TITLE_CHANGES.send_modify(|generation| *generation += 1),
            Ok(Ok(false)) => (),
            Ok(Err(error)) => tracing::debug!(%error, "写入自动标题失败"),
            Err(error) => tracing::debug!(%error, "自动标题任务失败"),
        }
    });
}

/// 取第一行非空文字，去掉引号、书名号、Markdown 标记和末尾标点。
pub(crate) fn clean_title(raw: &str) -> String {
    let line = raw.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default();
    let line = line.strip_prefix("Title:").or_else(|| line.strip_prefix("标题：")).unwrap_or(line);
    let wrappers: &[char] = &['"', '\'', '“', '”', '‘', '’', '《', '》', '「', '」', '`', '*', '#', ' '];
    let trailing: &[char] = &['。', '.', '!', '！', '?', '？', '，', ',', ';', '；', ':', '：'];
    line.trim_matches(wrappers).trim_end_matches(trailing).trim_matches(wrappers).chars().take(TITLE_MAX_CHARS).collect()
}
