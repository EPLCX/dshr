use std::sync::Arc;

use serde_json::Value;

use crate::event::{ContinueReason, EventBody, SessionEvent};
use crate::message::{ContentBlock, Message, Role};

/// 插件自定义事件（[`EventBody::Ext`]）怎么进模型上下文。一个 projector 管一种事件类型；
/// 没有 projector 的事件（插件已停用或卸载）留在日志里，投影时跳过。
pub trait EventProjector: Send + Sync {
    /// 管的事件类型，如 `todo/item`。
    fn kind(&self) -> &str;

    /// 这条事件在上下文里的文本，作为 user 内容接在对话里；`None` 表示这条事件不进上下文。
    /// 日志只追加、不改写，旧版本写下的数据也会走到这里，要兼容。
    fn project(&self, data: &Value) -> Option<String>;
}

/// 从会话日志投影出的模型上下文（对应原版 `deriveMessages()`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Projection {
    pub system: Option<Arc<str>>,
    pub messages: Vec<Message>,
}

const INTERRUPTED_RESULT: &str = "Tool call was interrupted before it produced a result.";

/// 压缩摘要在上下文里的前缀。
pub const COMPACTION_PREFIX: &str = "[Earlier conversation was compacted. Summary of what happened so far:]";

/// 只投影核心事件；插件事件全部跳过。见 [`derive_messages_with`]。
pub fn derive_messages<'a>(events: impl IntoIterator<Item = &'a SessionEvent>) -> Projection {
    derive_messages_with(events, &[])
}

/// 会话日志是模型上下文的唯一来源。规则：
/// - 相邻的 user 内容（工具结果 + 新输入）合并到同一条 user 消息；
/// - 被中断而没有结果的工具调用补一条错误结果，保证请求合法；
/// - 插件事件交给 `projectors` 里管这个类型的那个，得到的文本接在 user 内容里。
///   同一步里还有工具调用没出结果时先攒着，等结果齐了再接上（工具结果必须紧跟在调用后面）。
pub fn derive_messages_with<'a>(events: impl IntoIterator<Item = &'a SessionEvent>, projectors: &[Arc<dyn EventProjector>]) -> Projection {
    let events: Vec<&SessionEvent> = events.into_iter().collect();
    let mut projection = Projection::default();
    let mut pending: Vec<String> = Vec::new();
    // 等工具结果齐了再接上的插件事件内容（文字、工具交回的图片）。
    let mut deferred: Vec<ContentBlock> = Vec::new();

    // 最近一次压缩之前的对话由摘要代替（系统消息始终保留）。
    // `Arc<str>` 共享：摘要文本只算一份，上下文里的前缀块直接引用它。
    let compaction = events.iter().rev().find_map(|event| match &event.body {
        EventBody::Compaction { summary, upto_seq } => Some((summary.clone(), *upto_seq)),
        _ => None,
    });
    if let Some((summary, _)) = &compaction {
        push_user(&mut projection.messages, ContentBlock::text(format!("{COMPACTION_PREFIX}\n\n{summary}")));
    }

    for event in events {
        if let Some((_, upto)) = &compaction
            && event.seq <= *upto
            && !matches!(event.body, EventBody::SystemMessage { .. })
        {
            continue;
        }
        match &event.body {
            EventBody::SystemMessage { text } => match &mut projection.system {
                // 共享的 `Arc<str>` 不能原地改，要改时先写回成新的一份。
                Some(system) => *system = format!("{system}\n\n{text}").into(),
                None => projection.system = Some(text.clone()),
            },
            EventBody::UserMessage { text } => {
                flush_pending(&mut projection.messages, &mut pending, &mut deferred);
                push_user(&mut projection.messages, ContentBlock::text(text.clone()));
            }
            EventBody::UserAttachment { path, content, media_type, data } => {
                match (media_type, data) {
                    // 图片附件：投影成 Image 块，能看图的模型直接看到；不能看图的模型收到一句说明（agent 换的）。
                    (Some(media_type), Some(data)) => {
                        push_user(
                            &mut projection.messages,
                            ContentBlock::Image { source: crate::message::ImageSource::base64(media_type.clone(), data.clone()) },
                        );
                    }
                    _ => {
                        let content = content.as_deref().unwrap_or_default();
                        push_user(
                            &mut projection.messages,
                            ContentBlock::text(format!("<attachment path=\"{path}\">\n{content}\n</attachment>")),
                        );
                    }
                }
            }
            EventBody::AssistantMessage { content, .. } => {
                flush_pending(&mut projection.messages, &mut pending, &mut deferred);
                if content.is_empty() {
                    continue;
                }
                pending.extend(content.iter().filter_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                }));
                projection.messages.push(Message { role: Role::Assistant, content: content.clone() });
            }
            EventBody::ToolResult { call_id, output, is_error } => {
                pending.retain(|id| id != call_id);
                push_user(
                    &mut projection.messages,
                    ContentBlock::ToolResult { tool_use_id: call_id.clone(), content: output.clone(), is_error: *is_error },
                );
                if pending.is_empty() {
                    push_texts(&mut projection.messages, &mut deferred);
                }
            }
            EventBody::TurnContinue { reason: ContinueReason::MaxTokens } => {
                flush_pending(&mut projection.messages, &mut pending, &mut deferred);
                push_user(&mut projection.messages, ContentBlock::text(MAX_TOKENS_CONTINUE));
            }
            EventBody::Ext { kind, data } if kind == crate::message::MEDIA_IMAGES => {
                let images = data.get("images").and_then(serde_json::Value::as_array).into_iter().flatten();
                deferred.extend(images.filter_map(|image| {
                    let media_type = image.get("media_type").and_then(serde_json::Value::as_str)?;
                    let data = image.get("data").and_then(serde_json::Value::as_str)?;
                    Some(ContentBlock::Image { source: crate::message::ImageSource::base64(media_type, data) })
                }));
                if pending.is_empty() {
                    push_texts(&mut projection.messages, &mut deferred);
                }
            }
            EventBody::Ext { kind, data } => {
                let projector = projectors.iter().find(|projector| projector.kind() == kind);
                if let Some(text) = projector.and_then(|projector| projector.project(data)) {
                    deferred.push(ContentBlock::text(text));
                    if pending.is_empty() {
                        push_texts(&mut projection.messages, &mut deferred);
                    }
                }
            }
            _ => {}
        }
    }
    flush_pending(&mut projection.messages, &mut pending, &mut deferred);
    projection
}

/// 回复被输出上限截断后发给模型的提示。
const MAX_TOKENS_CONTINUE: &str = "[Your previous reply was cut off because it reached the output token limit, \
and any unfinished tool call was discarded. Continue from where you stopped. If you were writing a large file, \
write it in smaller parts: create it with a first part, then append the rest with further edits.]";

fn flush_pending(messages: &mut Vec<Message>, pending: &mut Vec<String>, deferred: &mut Vec<ContentBlock>) {
    for id in pending.drain(..) {
        push_user(messages, ContentBlock::ToolResult { tool_use_id: id, content: INTERRUPTED_RESULT.into(), is_error: true });
    }
    push_texts(messages, deferred);
}

fn push_texts(messages: &mut Vec<Message>, blocks: &mut Vec<ContentBlock>) {
    for block in blocks.drain(..) {
        push_user(messages, block);
    }
}

fn push_user(messages: &mut Vec<Message>, block: ContentBlock) {
    match messages.last_mut() {
        Some(last) if last.role == Role::User => last.content.push(block),
        _ => messages.push(Message { role: Role::User, content: vec![block] }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(body: EventBody) -> SessionEvent {
        SessionEvent { seq: 0, time: 0, body }
    }

    fn assistant(content: Vec<ContentBlock>) -> EventBody {
        EventBody::AssistantMessage { model: "m".into(), content, stop_reason: None, usage: None }
    }

    #[test]
    fn tool_round_trip_and_merge() {
        let events = vec![
            ev(EventBody::SystemMessage { text: "sys".into() }),
            ev(EventBody::TurnStart { turn: 1 }),
            ev(EventBody::UserMessage { text: "list files".into() }),
            ev(assistant(vec![ContentBlock::ToolUse { id: "c1".into(), name: "list_dir".into(), input: json!({"path": "."}).into() }])),
            ev(EventBody::ToolCall { call_id: "c1".into(), name: "list_dir".into(), input: json!({}) }),
            ev(EventBody::ToolResult { call_id: "c1".into(), output: "a.txt".into(), is_error: false }),
            ev(assistant(vec![ContentBlock::text("done")])),
        ];
        let p = derive_messages(&events);
        assert_eq!(p.system.as_deref(), Some("sys"));
        let roles: Vec<_> = p.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::User, Role::Assistant, Role::User, Role::Assistant]);
    }

    #[test]
    fn compaction_replaces_earlier_events() {
        let mut seq = 0;
        let mut ev = |body| {
            seq += 1;
            SessionEvent { seq, time: 0, body }
        };
        let events = vec![
            ev(EventBody::SystemMessage { text: "sys".into() }),
            ev(EventBody::UserMessage { text: "old question".into() }),
            ev(assistant(vec![ContentBlock::text("old answer")])),
            ev(EventBody::TurnEnd { turn: 1, reason: crate::TurnEndReason::Completed, error: None }),
            ev(EventBody::Compaction { summary: "用户问过旧问题".into(), upto_seq: 4 }),
            ev(EventBody::UserMessage { text: "new question".into() }),
        ];
        let p = derive_messages(&events);
        assert_eq!(p.system.as_deref(), Some("sys"));
        assert_eq!(p.messages.len(), 1);
        let ContentBlock::Text { text } = &p.messages[0].content[0] else { panic!() };
        assert!(text.starts_with(COMPACTION_PREFIX) && text.contains("用户问过旧问题"));
        assert_eq!(p.messages[0].content[1], ContentBlock::text("new question"));
    }

    struct Notes;

    impl EventProjector for Notes {
        fn kind(&self) -> &str {
            "notes/add"
        }

        fn project(&self, data: &Value) -> Option<String> {
            Some(format!("[note] {}", data.get("text")?.as_str()?))
        }
    }

    fn note(text: &str) -> EventBody {
        EventBody::ext("notes/add", json!({"text": text})).unwrap()
    }

    #[test]
    fn ext_events_are_projected_by_their_plugin() {
        let events = vec![
            ev(EventBody::UserMessage { text: "记一下".into() }),
            ev(note("买牛奶")),
            // 数据不对、projector 不要的事件，以及别的插件的事件，都不进上下文。
            ev(EventBody::ext("notes/add", json!({"oops": 1})).unwrap()),
            ev(EventBody::ext("other/thing", json!({})).unwrap()),
        ];
        let projectors: Vec<Arc<dyn EventProjector>> = vec![Arc::new(Notes)];
        let p = derive_messages_with(&events, &projectors);
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].content, [ContentBlock::text("记一下"), ContentBlock::text("[note] 买牛奶")]);

        // 插件不在时事件被跳过，其余内容不受影响。
        let p = derive_messages(&events);
        assert_eq!(p.messages[0].content, [ContentBlock::text("记一下")]);
    }

    #[test]
    fn ext_text_waits_until_tool_results_are_complete() {
        let tool_use = |id: &str| ContentBlock::ToolUse { id: id.into(), name: "x".into(), input: json!({}).into() };
        let result = |id: &str| EventBody::ToolResult { call_id: id.into(), output: "ok".into(), is_error: false };
        let events = vec![
            ev(EventBody::UserMessage { text: "go".into() }),
            ev(assistant(vec![tool_use("a"), tool_use("b")])),
            ev(result("a")),
            // 第一个工具写的事件：第二个工具的结果还没到，不能插在两个结果中间。
            ev(note("来自 a")),
            ev(result("b")),
            ev(assistant(vec![tool_use("c")])),
            // 调用被中断、没有结果：补上的错误结果仍然排在事件文本前面。
            ev(note("中断前")),
        ];
        let projectors: Vec<Arc<dyn EventProjector>> = vec![Arc::new(Notes)];
        let p = derive_messages_with(&events, &projectors);
        let shape = |message: &Message| -> Vec<String> {
            message
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::ToolResult { tool_use_id, .. } => format!("result:{tool_use_id}"),
                    ContentBlock::Text { text } => text.to_string(),
                    _ => "other".into(),
                })
                .collect()
        };
        assert_eq!(shape(&p.messages[2]), ["result:a", "result:b", "[note] 来自 a"]);
        assert_eq!(shape(&p.messages[4]), ["result:c", "[note] 中断前"]);
    }

    #[test]
    fn interrupted_tool_call_gets_synthetic_result() {
        let events = vec![
            ev(EventBody::UserMessage { text: "go".into() }),
            ev(assistant(vec![ContentBlock::ToolUse { id: "c9".into(), name: "run_shell".into(), input: json!({}).into() }])),
            ev(EventBody::UserMessage { text: "never mind".into() }),
        ];
        let p = derive_messages(&events);
        assert_eq!(p.messages.len(), 3);
        let last = &p.messages[2];
        assert_eq!(last.role, Role::User);
        assert!(matches!(
            &last.content[0],
            ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "c9"
        ));
        assert_eq!(last.content[1], ContentBlock::text("never mind"));
    }
}
