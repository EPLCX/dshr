use dshr_config::PromptCache;
use dshr_core::{ContentBlock, Role, StopReason};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Serialize;
use serde_json::Value;

use crate::types::{BlockKind, ChatRequest, EventStream, LlmError, Provider, ReasoningEffort, StreamEvent, ToolSpec, UsageUpdate};

pub(crate) const API_VERSION: &str = "2023-06-01";

/// Anthropic Messages 兼容接口（`{base}/v1/messages`）。DeepSeek、Anthropic 都走这条；
/// 原版的 DeepSeek 插件同样使用 Messages API。
pub struct MessagesProvider {
    client: reqwest::Client,
    id: String,
    base_url: String,
    api_key: Option<String>,
    /// 每个请求都带上的额外请求头（插件服务商的鉴权）。
    extra_headers: Vec<(String, String)>,
    /// 提示词缓存：给请求打上 `cache_control` 标记（见 [`WireRequest::new`]）。
    prompt_cache: PromptCache,
}

impl MessagesProvider {
    /// `id` 写入 `request/header` 事件，用来区分是哪个提供商。
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
            prompt_cache: PromptCache::Off,
        }
    }

    /// 提示词缓存的写入周期；`Off` 不标记缓存。
    pub fn with_prompt_cache(mut self, prompt_cache: PromptCache) -> Self {
        self.prompt_cache = prompt_cache;
        self
    }

    /// 每个请求都带上这些请求头。
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<WireSystem<'a>>,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: Vec<Marked<'a, ToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
}

/// `cache_control`：缓存到这里为止的前缀。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
struct CacheControl {
    r#type: &'static str,
    /// 5 分钟是默认值，不写（有的兼容网关不认 `ttl`）；1 小时写 `"1h"`。
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<&'static str>,
}

impl CacheControl {
    fn of(prompt_cache: PromptCache) -> Option<Self> {
        match prompt_cache {
            PromptCache::Off => None,
            PromptCache::FiveMinutes => Some(Self { r#type: "ephemeral", ttl: None }),
            PromptCache::OneHour => Some(Self { r#type: "ephemeral", ttl: Some("1h") }),
        }
    }
}

/// 原样序列化的一项，需要时在后面加上 `cache_control`。
#[derive(Serialize)]
struct Marked<'a, T> {
    #[serde(flatten)]
    item: &'a T,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

impl<'a, T> Marked<'a, T> {
    fn plain(item: &'a T) -> Self {
        Self { item, cache_control: None }
    }
}

/// 系统提示词：不缓存时是字符串，要缓存时写成带 `cache_control` 的文本块。
#[derive(Serialize)]
#[serde(untagged)]
enum WireSystem<'a> {
    Text(&'a str),
    Blocks([SystemBlock<'a>; 1]),
}

#[derive(Serialize)]
struct SystemBlock<'a> {
    r#type: &'static str,
    text: &'a str,
    cache_control: CacheControl,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: Role,
    content: Vec<Marked<'a, ContentBlock>>,
}

impl<'a> WireRequest<'a> {
    /// 按请求组装线上格式。打开提示词缓存时放两个缓存断点（Anthropic 最多 4 个）：
    /// - 系统提示词末尾（没有系统提示词时是最后一个工具）：工具定义和系统提示词这段固定前缀；
    /// - 最后一条消息的最后一个可标记的块：整段对话。agent 每一步只在末尾追加，下一次请求从上一次的断点处命中。
    ///
    /// 两个断点用同一个写入周期（Anthropic 要求长周期的断点排在短周期之前，同一周期没有这个问题）。
    fn new(request: &'a ChatRequest, prompt_cache: PromptCache) -> Self {
        let (thinking, output_config) = thinking_for(request.effort);
        let cache = CacheControl::of(prompt_cache);
        let system = request.system.as_deref().filter(|system| !system.is_empty()).map(|text| match cache {
            Some(cache_control) => WireSystem::Blocks([SystemBlock { r#type: "text", text, cache_control }]),
            None => WireSystem::Text(text),
        });
        let mut tools: Vec<_> = request.tools.iter().map(Marked::plain).collect();
        if system.is_none()
            && let Some(last) = tools.last_mut()
        {
            last.cache_control = cache;
        }
        let mut messages: Vec<WireMessage<'a>> = request
            .messages
            .iter()
            .map(|message| WireMessage { role: message.role, content: message.content.iter().map(Marked::plain).collect() })
            .collect();
        if cache.is_some()
            && let Some(last) = messages.last_mut()
            && let Some(block) = last.content.iter_mut().rev().find(|block| cacheable(block.item))
        {
            block.cache_control = cache;
        }
        Self {
            model: &request.model,
            max_tokens: request.max_tokens.unwrap_or(UNLIMITED_MAX_TOKENS),
            stream: true,
            system,
            messages,
            tools,
            thinking,
            output_config,
        }
    }
}

/// 能放 `cache_control` 的块：思考块不行，空文本块不行。
fn cacheable(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Thinking { .. } => false,
        ContentBlock::Text { text } => !text.is_empty(),
        ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } | ContentBlock::Image { .. } => true,
    }
}

/// Messages API 必须带 `max_tokens`；调用方不限制时用这个值。
const UNLIMITED_MAX_TOKENS: u32 = 64_000;

#[derive(Serialize)]
struct Thinking {
    r#type: &'static str,
}

#[derive(Serialize)]
struct OutputConfig {
    effort: &'static str,
}

/// 思考等级：`off` 关闭思考，其余开启自适应思考并用 `output_config.effort` 控制投入。
fn thinking_for(effort: Option<ReasoningEffort>) -> (Option<Thinking>, Option<OutputConfig>) {
    match effort {
        None => (None, None),
        Some(ReasoningEffort::Off) => (Some(Thinking { r#type: "disabled" }), None),
        Some(effort) => (Some(Thinking { r#type: "adaptive" }), Some(OutputConfig { effort: effort.as_str() })),
    }
}

#[async_trait::async_trait]
impl Provider for MessagesProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        let body = WireRequest::new(&request, self.prompt_cache);
        let mut builder = self.client.post(format!("{}/v1/messages", self.base_url)).header("anthropic-version", API_VERSION);
        if let Some(key) = &self.api_key {
            builder = builder.header("x-api-key", key);
        }
        for (name, value) in &self.extra_headers {
            builder = builder.header(name, value);
        }
        let response = builder.json(&body).send().await?;
        if !response.status().is_success() {
            return Err(LlmError::from_response(response).await);
        }
        Ok(sse_events(response.bytes_stream()))
    }
}

/// 逐条解析 SSE 字节流。收到 `message_stop` 或出错就结束：不少网关发完最后一个事件并不断开连接，
/// 等连接关闭就会一直卡在“生成中”。
pub(crate) fn sse_events<S, B, E>(bytes: S) -> EventStream
where
    S: futures::Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let events = bytes
        .eventsource()
        .map(|event| {
            let batch: Vec<Result<StreamEvent, LlmError>> = match event {
                Ok(event) if event.data.trim().is_empty() => Vec::new(),
                Ok(event) => match parse_sse_data(&event.data) {
                    Ok(events) => events.into_iter().map(Ok).collect(),
                    Err(error) => vec![Err(error)],
                },
                Err(error) => vec![Err(stream_error(error))],
            };
            futures::stream::iter(batch)
        })
        .flatten();
    let stream = futures::stream::unfold((Box::pin(events), false), |(mut events, done)| async move {
        if done {
            return None;
        }
        let event = events.next().await?;
        let done = matches!(event, Ok(StreamEvent::MessageStop) | Err(_));
        Some((event, (events, done)))
    });
    Box::pin(stream)
}

/// 读 SSE 流时的错误：连接断了、读超时是 [`LlmError::Interrupted`]（可以重试），数据本身坏了是协议错误。
pub(crate) fn stream_error<E: std::fmt::Display>(error: eventsource_stream::EventStreamError<E>) -> LlmError {
    match error {
        eventsource_stream::EventStreamError::Transport(error) => LlmError::Interrupted(error.to_string()),
        other => LlmError::Protocol(other.to_string()),
    }
}

/// 解析一条 SSE `data`。按 JSON 内的 `type` 分派，`ping` 与未知事件返回空列表。
/// `message_start`、`message_delta` 里带缓存用量时，后面跟一条 [`StreamEvent::Usage`]。
pub fn parse_sse_data(data: &str) -> Result<Vec<StreamEvent>, LlmError> {
    let value: Value = serde_json::from_str(data).map_err(|error| LlmError::Protocol(format!("{error}: {data}")))?;
    let index = || value["index"].as_u64().unwrap_or(0) as usize;
    let text = |pointer: &str| value.pointer(pointer).and_then(Value::as_str).unwrap_or_default().to_owned();

    let event = match value["type"].as_str().unwrap_or_default() {
        "message_start" => StreamEvent::MessageStart {
            model: value.pointer("/message/model").and_then(Value::as_str).map(str::to_owned),
            input_tokens: value.pointer("/message/usage/input_tokens").and_then(Value::as_u64),
        },
        "content_block_start" => {
            let block = &value["content_block"];
            let kind = match block["type"].as_str().unwrap_or_default() {
                "text" => BlockKind::Text,
                "thinking" => BlockKind::Thinking,
                "tool_use" => BlockKind::ToolUse {
                    id: block["id"].as_str().unwrap_or_default().to_owned(),
                    name: block["name"].as_str().unwrap_or_default().to_owned(),
                },
                other => BlockKind::Other(other.to_owned()),
            };
            StreamEvent::BlockStart { index: index(), kind }
        }
        "content_block_delta" => match value.pointer("/delta/type").and_then(Value::as_str) {
            Some("text_delta") => StreamEvent::TextDelta { index: index(), text: text("/delta/text") },
            Some("thinking_delta") => StreamEvent::ThinkingDelta { index: index(), thinking: text("/delta/thinking") },
            Some("signature_delta") => StreamEvent::SignatureDelta { index: index(), signature: text("/delta/signature") },
            Some("input_json_delta") => StreamEvent::ToolInputDelta { index: index(), partial_json: text("/delta/partial_json") },
            _ => return Ok(Vec::new()),
        },
        "content_block_stop" => StreamEvent::BlockStop { index: index() },
        "message_delta" => StreamEvent::MessageDelta {
            stop_reason: value
                .pointer("/delta/stop_reason")
                .filter(|reason| !reason.is_null())
                .map(|reason| serde_json::from_value(reason.clone()).unwrap_or(StopReason::Other)),
            output_tokens: value.pointer("/usage/output_tokens").and_then(Value::as_u64),
        },
        "message_stop" => StreamEvent::MessageStop,
        "error" => {
            let message = value.pointer("/error/message").and_then(Value::as_str).unwrap_or(data);
            return Err(LlmError::Api(message.to_owned()));
        }
        _ => return Ok(Vec::new()),
    };
    let usage = match &event {
        StreamEvent::MessageStart { .. } => usage_update(value.pointer("/message/usage")),
        StreamEvent::MessageDelta { .. } => usage_update(value.get("usage")),
        _ => UsageUpdate::default(),
    };
    let mut events = vec![event];
    if !usage.is_empty() {
        events.push(StreamEvent::Usage(usage));
    }
    Ok(events)
}

/// Messages API 的 `usage` 对象里的各项用量。
fn usage_update(usage: Option<&Value>) -> UsageUpdate {
    let field = |name: &str| usage.and_then(|usage| usage.get(name)).and_then(Value::as_u64);
    UsageUpdate {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_read_input_tokens: field("cache_read_input_tokens"),
        cache_creation_input_tokens: field("cache_creation_input_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accumulate::MessageAccumulator;
    use dshr_core::{ContentBlock, Message};
    use serde_json::json;

    /// 一段典型的 Messages API 流：思考 → 文本 → 工具调用。
    const STREAM: &[&str] = &[
        r#"{"type":"message_start","message":{"model":"deepseek-chat","usage":{"input_tokens":12}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"想一想"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"ping"}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"我来"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"看看。"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_1","name":"list_dir","input":{}}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\".\"}"}}"#,
        r#"{"type":"content_block_stop","index":2}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":30}}"#,
        r#"{"type":"message_stop"}"#,
    ];

    #[test]
    fn parses_and_assembles_stream() {
        let mut acc = MessageAccumulator::default();
        for data in STREAM {
            for event in parse_sse_data(data).unwrap() {
                acc.push(&event).unwrap();
            }
        }
        let message = acc.finish();
        assert_eq!(message.model.as_deref(), Some("deepseek-chat"));
        assert_eq!(message.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(message.usage.input_tokens, 12);
        assert_eq!(message.usage.output_tokens, 30);
        assert_eq!(
            message.content,
            vec![
                ContentBlock::Thinking { thinking: "想一想".into(), signature: Some("sig".into()) },
                ContentBlock::text("我来看看。"),
                ContentBlock::ToolUse { id: "call_1".into(), name: "list_dir".into(), input: json!({"path": "."}).into() },
            ]
        );
    }

    #[tokio::test]
    async fn stream_ends_at_message_stop_even_if_connection_stays_open() {
        let body: String = STREAM.iter().map(|data| format!("data: {data}\n\n")).collect();
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> = body.as_bytes().chunks(11).map(|chunk| Ok(chunk.to_vec())).collect();
        let stream = sse_events(futures::stream::iter(chunks).chain(futures::stream::pending()));
        let events = tokio::time::timeout(std::time::Duration::from_secs(10), stream.collect::<Vec<_>>())
            .await
            .expect("流应当在 message_stop 处结束");
        assert!(matches!(events.last(), Some(Ok(StreamEvent::MessageStop))));
    }

    #[test]
    fn error_event_is_api_error() {
        let error = parse_sse_data(r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#).unwrap_err();
        assert!(matches!(&error, LlmError::Api(message) if message == "overloaded"));
        assert!(error.is_retryable());
    }

    fn chat(system: Option<&str>, messages: Vec<Message>, tools: Vec<ToolSpec>, effort: Option<ReasoningEffort>) -> ChatRequest {
        ChatRequest { model: "m".into(), system: system.map(Into::into), messages, tools, max_tokens: Some(100), effort }
    }

    #[test]
    fn request_body_shape() {
        let messages = vec![Message { role: Role::User, content: vec![ContentBlock::text("hi")] }];
        let request = chat(Some("sys"), messages, Vec::new(), None);
        assert_eq!(
            serde_json::to_value(WireRequest::new(&request, PromptCache::Off)).unwrap(),
            json!({
                "model": "m",
                "max_tokens": 100,
                "stream": true,
                "system": "sys",
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
            })
        );

        let request = ChatRequest { effort: Some(ReasoningEffort::Max), ..request };
        let value = serde_json::to_value(WireRequest::new(&request, PromptCache::Off)).unwrap();
        assert_eq!(value["thinking"], json!({"type": "adaptive"}));
        assert_eq!(value["output_config"], json!({"effort": "max"}));

        let request = ChatRequest { effort: Some(ReasoningEffort::Off), ..request };
        let value = serde_json::to_value(WireRequest::new(&request, PromptCache::Off)).unwrap();
        assert_eq!(value["thinking"], json!({"type": "disabled"}));
    }

    #[test]
    fn prompt_cache_marks_system_and_conversation_tail() {
        let tool = |name: &str| ToolSpec { name: name.into(), description: "d".into(), input_schema: json!({"type": "object"}) };
        let messages = vec![
            Message { role: Role::User, content: vec![ContentBlock::text("列目录")] },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse { id: "c1".into(), name: "list_dir".into(), input: json!({}).into() }],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult { tool_use_id: "c1".into(), content: "a.txt".into(), is_error: false },
                    ContentBlock::text(""),
                ],
            },
        ];
        let request = chat(Some("sys"), messages, vec![tool("a"), tool("b")], None);

        let five = serde_json::to_value(WireRequest::new(&request, PromptCache::FiveMinutes)).unwrap();
        assert_eq!(five["system"], json!([{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}]));
        // 有系统提示词时工具不单独标记（工具排在系统提示词前面，一起缓存）。
        assert!(five["tools"].as_array().unwrap().iter().all(|tool| tool.get("cache_control").is_none()));
        assert_eq!(five["tools"][1], json!({"name": "b", "description": "d", "input_schema": {"type": "object"}}));
        // 最后一条消息里最后一个能标记的块（空文本块不能标）。
        let last = &five["messages"][2]["content"];
        assert_eq!(last[0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(last[0]["type"], "tool_result");
        assert!(last[1].get("cache_control").is_none());
        assert!(five["messages"][0]["content"][0].get("cache_control").is_none());

        let hour = serde_json::to_value(WireRequest::new(&request, PromptCache::OneHour)).unwrap();
        assert_eq!(hour["system"][0]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
        assert_eq!(hour["messages"][2]["content"][0]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));

        // 没有系统提示词：标在最后一个工具上。
        let request = ChatRequest { system: None, ..request };
        let value = serde_json::to_value(WireRequest::new(&request, PromptCache::OneHour)).unwrap();
        assert!(value.get("system").is_none());
        assert_eq!(value["tools"][1]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
        assert!(value["tools"][0].get("cache_control").is_none());
        // 不缓存时请求体里没有任何 cache_control。
        assert!(!serde_json::to_string(&WireRequest::new(&request, PromptCache::Off)).unwrap().contains("cache_control"));
    }

    #[test]
    fn cache_usage_is_reported() {
        let mut acc = MessageAccumulator::default();
        for data in [
            r#"{"type":"message_start","message":{"model":"claude","usage":{"input_tokens":20,"cache_read_input_tokens":9000,"cache_creation_input_tokens":300}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
        ] {
            for event in parse_sse_data(data).unwrap() {
                acc.push(&event).unwrap();
            }
        }
        let usage = acc.finish().usage;
        assert_eq!((usage.input_tokens, usage.cache_read_input_tokens, usage.cache_creation_input_tokens), (20, 9000, 300));
        assert_eq!(usage.output_tokens, 42);
        assert_eq!(usage.context_tokens(), 20 + 9000 + 300 + 42);
    }

    #[tokio::test]
    async fn dropped_connection_mid_stream_is_retryable() {
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> = vec![
            Ok(format!("data: {}\n\n", STREAM[0]).into_bytes()),
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "operation timed out")),
        ];
        let events: Vec<_> = sse_events(futures::stream::iter(chunks)).collect().await;
        let error = events.into_iter().find_map(Result::err).expect("应当报错");
        assert!(matches!(error, LlmError::Interrupted(_)) && error.is_retryable(), "{error}");
    }

    #[tokio::test]
    async fn http_errors_carry_retry_after() {
        let response = |headers: &[(&str, &str)]| {
            let mut builder = http::Response::builder().status(429);
            for (name, value) in headers {
                builder = builder.header(*name, *value);
            }
            reqwest::Response::from(builder.body("slow down").unwrap())
        };
        let error = LlmError::from_response(response(&[("retry-after", "3")])).await;
        assert!(matches!(&error, LlmError::Http { status: 429, body, .. } if body == "slow down"));
        assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(3)));
        let error = LlmError::from_response(response(&[("retry-after-ms", "1500"), ("retry-after", "9")])).await;
        assert_eq!(error.retry_after(), Some(std::time::Duration::from_millis(1500)));
        // HTTP 日期格式不认，交给退避策略。
        let error = LlmError::from_response(response(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")])).await;
        assert_eq!(error.retry_after(), None);
    }
}
