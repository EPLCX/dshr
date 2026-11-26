//! OpenAI Chat Completions 兼容接口（OpenAI、Kimi、OpenRouter、Ollama 等）。
//!
//! 线上协议的流式增量没有“内容块”概念，这里用 [`OpenAiStream`] 把它们折算成
//! 与 Messages API 相同的 [`StreamEvent`] 序列，上层无需区分协议。

use std::borrow::Cow;
use std::collections::BTreeMap;

use dshr_core::{ContentBlock, ImageSource, Role, StopReason};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Serialize;
use serde_json::Value;

use crate::types::{BlockKind, ChatRequest, EventStream, LlmError, Provider, StreamEvent, UsageUpdate};

pub struct OpenAiProvider {
    client: reqwest::Client,
    id: String,
    base_url: String,
    api_key: Option<String>,
    /// 每个请求都带上的额外请求头（插件服务商的鉴权）。
    extra_headers: Vec<(String, String)>,
}

impl OpenAiProvider {
    pub fn new(id: impl Into<String>, base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self::with_client(crate::HttpClient::default(), id, base_url, api_key)
    }

    /// 复用已有的 HTTP 客户端（连接池）。
    pub fn with_client(client: crate::HttpClient, id: impl Into<String>, base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: client.0,
            id: id.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.filter(|key| !key.trim().is_empty()),
            extra_headers: Vec::new(),
        }
    }

    /// 每个请求都带上这些请求头。
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        let mut builder = self.client.post(format!("{}/chat/completions", self.base_url));
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        for (name, value) in &self.extra_headers {
            builder = builder.header(name, value);
        }
        let response = builder.json(&request_body(&request)).send().await?;
        if !response.status().is_success() {
            return Err(LlmError::from_response(response).await);
        }

        Ok(sse_events(response.bytes_stream()))
    }
}

/// 收到 `finish_reason` 之后，最多再等这么久的用量块和 `[DONE]`；有的兼容服务两样都不发、连接也不断开。
#[cfg(not(test))]
const FINISH_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const FINISH_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// 逐条解析 SSE 字节流。收到 `[DONE]` 立即结束——不少网关发完 `[DONE]` 并不断开连接，
/// 等连接关闭就会一直卡在“生成中”；连接提前关闭时同样补齐收尾事件。
pub(crate) fn sse_events<S, B, E>(bytes: S) -> EventStream
where
    S: futures::Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let events = bytes.eventsource();
    let stream = futures::stream::unfold((Box::pin(events), OpenAiStream::default()), |(mut events, mut state)| async move {
        loop {
            if state.finished {
                return None;
            }
            let next = if state.stop_reason.is_some() {
                // 已经有结束原因：只再等一小会儿，超时当作流已结束。
                tokio::time::timeout(FINISH_GRACE, events.next()).await.unwrap_or(None)
            } else {
                events.next().await
            };
            let batch = match next {
                Some(Ok(event)) if event.data.trim() == "[DONE]" => state.finish().into_iter().map(Ok).collect(),
                Some(Ok(event)) if event.data.trim().is_empty() => Vec::new(),
                Some(Ok(event)) => match serde_json::from_str::<Value>(&event.data) {
                    Ok(chunk) => match state.push(&chunk) {
                        Ok(events) => events.into_iter().map(Ok).collect(),
                        Err(error) => {
                            // 出错后不再读后面的数据。
                            state.finished = true;
                            vec![Err(error)]
                        }
                    },
                    Err(error) => {
                        state.finished = true;
                        vec![Err(LlmError::Protocol(format!("{error}: {}", event.data)))]
                    }
                },
                Some(Err(error)) => {
                    state.finished = true;
                    vec![Err(crate::messages::stream_error(error))]
                }
                None => state.finish().into_iter().map(Ok).collect(),
            };
            if !batch.is_empty() {
                return Some((futures::stream::iter(batch), (events, state)));
            }
        }
    })
    .flatten();
    Box::pin(stream)
}

/// 请求体。字段都借用 [`ChatRequest`] 里的数据，直接序列化：不先复制成一棵 `Value` 树（每一步都要发整段上下文，
/// 里面可能有整份文件内容和图片），图片的 data URI、工具参数的 JSON 字符串也是序列化时直接写出。
pub(crate) fn request_body(request: &ChatRequest) -> WireRequest<'_> {
    WireRequest {
        model: &request.model,
        messages: to_openai_messages(request),
        stream: true,
        stream_options: StreamOptions { include_usage: true },
        max_tokens: request.max_tokens,
        reasoning_effort: request.effort.map(|effort| effort.as_str()),
        tools: request
            .tools
            .iter()
            .map(|tool| WireTool {
                kind: "function",
                function: WireFunction { name: &tool.name, description: &tool.description, parameters: &tool.input_schema },
            })
            .collect(),
    }
}

#[derive(Serialize)]
pub(crate) struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    stream: bool,
    stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum WireMessage<'a> {
    System {
        content: &'a str,
    },
    User {
        content: UserContent<'a>,
    },
    Assistant {
        /// 没有文本时发 `null`（不能省略）。
        content: Option<Cow<'a, str>>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<WireCall<'a>>,
    },
    Tool {
        tool_call_id: &'a str,
        content: &'a str,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
enum UserContent<'a> {
    Text(Cow<'a, str>),
    Parts(Vec<UserPart<'a>>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UserPart<'a> {
    Text { text: Cow<'a, str> },
    ImageUrl { image_url: ImageUrl<'a> },
}

#[derive(Serialize)]
struct ImageUrl<'a> {
    url: DataUri<'a>,
}

/// `data:<media type>;base64,<data>`，序列化时直接写出，不先拼成字符串。
struct DataUri<'a>(&'a ImageSource);

impl Serialize for DataUri<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("data:{};base64,{}", self.0.media_type, self.0.data))
    }
}

#[derive(Serialize)]
struct WireCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireCallFunction<'a>,
}

#[derive(Serialize)]
struct WireCallFunction<'a> {
    name: &'a str,
    /// 参数是 JSON 文本（字符串），序列化时直接写出。
    arguments: JsonText<'a>,
}

struct JsonText<'a>(&'a Value);

impl Serialize for JsonText<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self.0)
    }
}

/// 消息里的文本块，用空行连起来；只有一块时直接借用。
fn joined_text(content: &[ContentBlock]) -> Cow<'_, str> {
    let mut texts = content.iter().filter_map(|block| match block {
        ContentBlock::Text { text } => Some(&**text),
        _ => None,
    });
    let Some(first) = texts.next() else { return Cow::Borrowed("") };
    match texts.next() {
        None => Cow::Borrowed(first),
        Some(second) => {
            let mut joined = format!("{first}\n\n{second}");
            for text in texts {
                joined.push_str("\n\n");
                joined.push_str(text);
            }
            Cow::Owned(joined)
        }
    }
}

/// 内容块 → OpenAI 消息：工具结果拆成独立的 `tool` 消息，且排在同一条用户消息的文本之前；
/// 思考内容不回传。
fn to_openai_messages(request: &ChatRequest) -> Vec<WireMessage<'_>> {
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(WireMessage::System { content: system });
    }
    for message in &request.messages {
        let text = joined_text(&message.content);
        match message.role {
            Role::User => {
                for block in &message.content {
                    if let ContentBlock::ToolResult { tool_use_id, content, .. } = block {
                        messages.push(WireMessage::Tool { tool_call_id: tool_use_id, content });
                    }
                }
                // 图片（工具交回的）跟在工具消息后面，作为一条带 image_url 的 user 消息。
                let images: Vec<UserPart<'_>> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Image { source } => Some(UserPart::ImageUrl { image_url: ImageUrl { url: DataUri(source) } }),
                        _ => None,
                    })
                    .collect();
                if images.is_empty() {
                    if !text.is_empty() {
                        messages.push(WireMessage::User { content: UserContent::Text(text) });
                    }
                } else {
                    let mut parts = Vec::with_capacity(images.len() + 1);
                    if !text.is_empty() {
                        parts.push(UserPart::Text { text });
                    }
                    parts.extend(images);
                    messages.push(WireMessage::User { content: UserContent::Parts(parts) });
                }
            }
            Role::Assistant => {
                let tool_calls = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, name, input } => {
                            Some(WireCall { id, kind: "function", function: WireCallFunction { name, arguments: JsonText(input) } })
                        }
                        _ => None,
                    })
                    .collect();
                messages.push(WireMessage::Assistant { content: (!text.is_empty()).then_some(text), tool_calls });
            }
        }
    }
    messages
}

/// 把 Chat Completions 的增量折算成内容块事件。
#[derive(Debug, Default)]
pub struct OpenAiStream {
    started: bool,
    finished: bool,
    next_index: usize,
    /// 按开始顺序记录的块，用于收尾时逐个结束。
    open_blocks: Vec<usize>,
    text: Option<usize>,
    thinking: Option<usize>,
    /// OpenAI 的 tool_calls[].index → 本地块序号。
    tools: BTreeMap<u64, usize>,
    stop_reason: Option<StopReason>,
    output_tokens: Option<u64>,
    /// 最后一块里的用量（`stream_options.include_usage`）；收尾时报告。
    usage: UsageUpdate,
}

impl OpenAiStream {
    pub fn push(&mut self, chunk: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
            let message = error["message"].as_str().map_or_else(|| error.to_string(), str::to_owned);
            return Err(LlmError::Api(message));
        }
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(StreamEvent::MessageStart { model: chunk["model"].as_str().map(str::to_owned), input_tokens: None });
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            let field = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64);
            if let Some(tokens) = field("/completion_tokens") {
                self.output_tokens = Some(tokens);
            }
            // `prompt_tokens` 含命中缓存的部分（OpenAI 记在 `prompt_tokens_details.cached_tokens`，
            // DeepSeek 记在 `prompt_cache_hit_tokens`）。按 Messages API 的口径拆开：输入只算没命中的。
            if let Some(prompt) = field("/prompt_tokens") {
                let cached = field("/prompt_tokens_details/cached_tokens").or_else(|| field("/prompt_cache_hit_tokens")).unwrap_or(0);
                self.usage.input_tokens = Some(prompt.saturating_sub(cached));
                self.usage.cache_read_input_tokens = Some(cached.min(prompt));
            }
        }
        let Some(choice) = chunk["choices"].get(0) else {
            return Ok(events);
        };
        let delta = &choice["delta"];

        // DeepSeek、Kimi 等用 `reasoning_content` 输出思考过程。
        if let Some(thinking) = delta["reasoning_content"].as_str().filter(|text| !text.is_empty()) {
            let index = self.block(&mut events, |state| &mut state.thinking, BlockKind::Thinking);
            events.push(StreamEvent::ThinkingDelta { index, thinking: thinking.to_owned() });
        }
        if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
            let index = self.block(&mut events, |state| &mut state.text, BlockKind::Text);
            events.push(StreamEvent::TextDelta { index, text: text.to_owned() });
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let key = call["index"].as_u64().unwrap_or(0);
            let index = match self.tools.get(&key) {
                Some(index) => *index,
                None => {
                    let index = self.allocate();
                    self.tools.insert(key, index);
                    let id = call["id"].as_str().map_or_else(|| format!("call_{index}"), str::to_owned);
                    let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default().to_owned();
                    events.push(StreamEvent::BlockStart { index, kind: BlockKind::ToolUse { id, name } });
                    index
                }
            };
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str).filter(|a| !a.is_empty()) {
                events.push(StreamEvent::ToolInputDelta { index, partial_json: arguments.to_owned() });
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.stop_reason = Some(match reason {
                "stop" => StopReason::EndTurn,
                "tool_calls" | "function_call" => StopReason::ToolUse,
                "length" => StopReason::MaxTokens,
                _ => StopReason::Other,
            });
        }
        Ok(events)
    }

    /// 结束所有块并发出收尾事件；重复调用无效果。
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(StreamEvent::MessageStart { model: None, input_tokens: None });
        }
        events.extend(self.open_blocks.drain(..).map(|index| StreamEvent::BlockStop { index }));
        if !self.usage.is_empty() {
            events.push(StreamEvent::Usage(self.usage));
        }
        events.push(StreamEvent::MessageDelta {
            stop_reason: Some(self.stop_reason.clone().unwrap_or(StopReason::EndTurn)),
            output_tokens: self.output_tokens,
        });
        events.push(StreamEvent::MessageStop);
        events
    }

    fn allocate(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        self.open_blocks.push(index);
        index
    }

    fn block(&mut self, events: &mut Vec<StreamEvent>, slot: impl Fn(&mut Self) -> &mut Option<usize>, kind: BlockKind) -> usize {
        if let Some(index) = *slot(self) {
            return index;
        }
        let index = self.allocate();
        *slot(self) = Some(index);
        events.push(StreamEvent::BlockStart { index, kind });
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accumulate::MessageAccumulator;
    use crate::types::{ReasoningEffort, ToolSpec};
    use dshr_core::Message;
    use serde_json::json;

    #[test]
    fn images_and_multiple_text_blocks() {
        let image = ImageSource { kind: "base64".into(), media_type: "image/png".into(), data: "QUJD".into() };
        let request = ChatRequest {
            model: "gpt".into(),
            system: None,
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult { tool_use_id: "c1".into(), content: "截图".into(), is_error: false },
                        ContentBlock::Image { source: image },
                        ContentBlock::text("看看"),
                        ContentBlock::text("这个"),
                    ],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::text("好"),
                        ContentBlock::ToolUse { id: "c2".into(), name: "w".into(), input: json!({"text": "a\"b"}).into() },
                    ],
                },
            ],
            tools: Vec::new(),
            max_tokens: Some(10),
            effort: None,
        };
        let body = serde_json::to_value(request_body(&request)).unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {"role": "tool", "tool_call_id": "c1", "content": "截图"},
                {"role": "user", "content": [
                    {"type": "text", "text": "看看\n\n这个"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
                ]},
                {"role": "assistant", "content": "好", "tool_calls": [
                    {"id": "c2", "type": "function", "function": {"name": "w", "arguments": r#"{"text":"a\"b"}"#}}
                ]},
            ])
        );
        assert_eq!(body["max_tokens"], 10);
        assert!(body.get("tools").is_none() && body.get("reasoning_effort").is_none());
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
    }

    #[test]
    fn folds_deltas_into_blocks() {
        let chunks = [
            json!({"model": "kimi", "choices": [{"delta": {"role": "assistant", "reasoning_content": "想"}}]}),
            json!({"choices": [{"delta": {"reasoning_content": "一想"}}]}),
            json!({"choices": [{"delta": {"content": "我来"}}]}),
            json!({"choices": [{"delta": {"content": "看看。"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_a", "type": "function", "function": {"name": "list_dir", "arguments": ""}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"path\":"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\".\"}"}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 7}}),
        ];
        let mut state = OpenAiStream::default();
        let mut acc = MessageAccumulator::default();
        for chunk in &chunks {
            for event in state.push(chunk).unwrap() {
                acc.push(&event).unwrap();
            }
        }
        for event in state.finish() {
            acc.push(&event).unwrap();
        }
        assert!(state.finish().is_empty());

        let message = acc.finish();
        assert_eq!(message.model.as_deref(), Some("kimi"));
        assert_eq!(message.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(message.usage.output_tokens, 7);
        assert_eq!(message.usage.input_tokens, 10);
        assert_eq!(
            message.content,
            vec![
                ContentBlock::Thinking { thinking: "想一想".into(), signature: None },
                ContentBlock::text("我来看看。"),
                ContentBlock::ToolUse { id: "call_a".into(), name: "list_dir".into(), input: json!({"path": "."}).into() },
            ]
        );
    }

    /// 把 SSE 文本切成字节块，之后连接一直不关闭（模拟发完 `[DONE]` 不断开的网关）。
    fn hanging_body(text: &str) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send + 'static {
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> = text.as_bytes().chunks(7).map(|chunk| Ok(chunk.to_vec())).collect();
        futures::stream::iter(chunks).chain(futures::stream::pending())
    }

    async fn collect(stream: EventStream) -> Vec<StreamEvent> {
        let all = stream.map(Result::unwrap).collect::<Vec<_>>();
        tokio::time::timeout(std::time::Duration::from_secs(10), all).await.expect("流应当自己结束")
    }

    #[tokio::test]
    async fn stream_ends_at_done_even_if_connection_stays_open() {
        let body = "data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                    data: [DONE]\n\n";
        let events = collect(sse_events(hanging_body(body))).await;
        assert_eq!(events.last(), Some(&StreamEvent::MessageStop));
        assert!(events.contains(&StreamEvent::TextDelta { index: 0, text: "hi".into() }));
    }

    #[tokio::test]
    async fn stream_ends_after_finish_reason_without_done() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n";
        let events = collect(sse_events(hanging_body(body))).await;
        assert_eq!(events.last(), Some(&StreamEvent::MessageStop));
    }

    #[test]
    fn error_chunk_is_api_error() {
        let mut state = OpenAiStream::default();
        let error = state.push(&json!({"error": {"message": "quota exceeded"}})).unwrap_err();
        assert!(matches!(error, LlmError::Api(message) if message == "quota exceeded"));
    }

    #[test]
    fn converts_history_to_openai_messages() {
        let request = ChatRequest {
            model: "gpt".into(),
            system: Some("sys".into()),
            messages: vec![
                Message { role: Role::User, content: vec![ContentBlock::text("列目录")] },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Thinking { thinking: "嗯".into(), signature: None },
                        ContentBlock::ToolUse { id: "c1".into(), name: "list_dir".into(), input: json!({}).into() },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult { tool_use_id: "c1".into(), content: "a.txt".into(), is_error: false },
                        ContentBlock::text("继续"),
                    ],
                },
            ],
            tools: vec![ToolSpec { name: "list_dir".into(), description: "d".into(), input_schema: json!({"type": "object"}) }],
            max_tokens: None,
            effort: Some(ReasoningEffort::High),
        };
        let body = serde_json::to_value(request_body(&request)).unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "列目录"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "list_dir", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "a.txt"},
                {"role": "user", "content": "继续"},
            ])
        );
        assert_eq!(body["tools"][0]["function"]["parameters"], json!({"type": "object"}));
        assert!(body.get("thinking").is_none());
        assert!(body.get("max_tokens").is_none(), "不限制输出时不发送 max_tokens");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn usage_splits_cached_prompt_tokens() {
        let usage = |chunk: Value| {
            let mut state = OpenAiStream::default();
            let mut acc = MessageAccumulator::default();
            for event in state
                .push(&json!({"choices": [{"delta": {"content": "hi"}}]}))
                .unwrap()
                .into_iter()
                .chain(state.push(&chunk).unwrap())
                .chain(state.finish())
            {
                acc.push(&event).unwrap();
            }
            acc.finish().usage
        };
        let openai = usage(
            json!({"choices": [], "usage": {"prompt_tokens": 1000, "completion_tokens": 5, "prompt_tokens_details": {"cached_tokens": 800}}}),
        );
        assert_eq!((openai.input_tokens, openai.cache_read_input_tokens, openai.output_tokens), (200, 800, 5));
        let deepseek =
            usage(json!({"choices": [], "usage": {"prompt_tokens": 1000, "completion_tokens": 5, "prompt_cache_hit_tokens": 900}}));
        assert_eq!((deepseek.input_tokens, deepseek.cache_read_input_tokens), (100, 900));
        assert_eq!(deepseek.context_tokens(), 1005);
    }
}
