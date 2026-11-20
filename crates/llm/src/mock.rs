//! 脚本化 Provider：按顺序回放预先写好的回复，并记录收到的请求。用于 agent 测试与行为对照。

use std::collections::VecDeque;
use std::sync::Mutex;

use dshr_core::StopReason;
use futures::stream;
use serde_json::Value;

use crate::types::{BlockKind, ChatRequest, EventStream, LlmError, Provider, StreamEvent};

#[derive(Default)]
pub struct ScriptedProvider {
    replies: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ScriptedProvider {
    pub fn new(replies: impl IntoIterator<Item = Vec<StreamEvent>>) -> Self {
        Self { replies: Mutex::new(replies.into_iter().collect()), requests: Mutex::default() }
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// 一条只含文本的回复。
    pub fn text_reply(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart { model: Some("mock".into()), input_tokens: Some(1) },
            StreamEvent::BlockStart { index: 0, kind: BlockKind::Text },
            StreamEvent::TextDelta { index: 0, text: text.into() },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::MessageDelta { stop_reason: Some(StopReason::EndTurn), output_tokens: Some(1) },
            StreamEvent::MessageStop,
        ]
    }

    /// 一条同时发起多个工具调用的回复：(调用 ID, 工具名, 参数)。
    pub fn tool_calls_reply(calls: &[(&str, &str, Value)]) -> Vec<StreamEvent> {
        let mut events = vec![StreamEvent::MessageStart { model: Some("mock".into()), input_tokens: Some(1) }];
        for (index, (id, name, input)) in calls.iter().enumerate() {
            events.push(StreamEvent::BlockStart { index, kind: BlockKind::ToolUse { id: (*id).into(), name: (*name).into() } });
            events.push(StreamEvent::ToolInputDelta { index, partial_json: input.to_string() });
            events.push(StreamEvent::BlockStop { index });
        }
        events.push(StreamEvent::MessageDelta { stop_reason: Some(StopReason::ToolUse), output_tokens: Some(1) });
        events.push(StreamEvent::MessageStop);
        events
    }

    /// 一条只含单个工具调用的回复。
    pub fn tool_reply(id: &str, name: &str, input: Value) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart { model: Some("mock".into()), input_tokens: Some(1) },
            StreamEvent::BlockStart { index: 0, kind: BlockKind::ToolUse { id: id.into(), name: name.into() } },
            StreamEvent::ToolInputDelta { index: 0, partial_json: input.to_string() },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::MessageDelta { stop_reason: Some(StopReason::ToolUse), output_tokens: Some(1) },
            StreamEvent::MessageStop,
        ]
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "mock"
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        // 报告的输入用量按请求大小算（约 4 字节一个 token），而不是脚本里写死的值：
        // 按用量判断上下文大小的逻辑（上下文压缩）在测试里才有意义。
        let input_tokens = request_bytes(&request) as u64 / 4;
        self.requests.lock().unwrap().push(request);
        let reply = self.replies.lock().unwrap().pop_front().ok_or_else(|| LlmError::Api("脚本回复已用完".into()))?;
        let reply = reply.into_iter().map(move |event| match event {
            StreamEvent::MessageStart { model, input_tokens: Some(_) } => {
                StreamEvent::MessageStart { model, input_tokens: Some(input_tokens) }
            }
            other => other,
        });
        Ok(Box::pin(stream::iter(reply.map(Ok))))
    }
}

fn request_bytes(request: &ChatRequest) -> usize {
    fn json_len(value: &impl serde::Serialize) -> usize {
        serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
    }
    request.system.as_ref().map_or(0, String::len) + json_len(&request.messages) + json_len(&request.tools)
}
