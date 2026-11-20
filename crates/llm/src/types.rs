use std::pin::Pin;

use dshr_core::{Message, StopReason};
use futures::Stream;
use serde::Serialize;
use serde_json::Value;

pub use dshr_config::ReasoningEffort;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    /// 输出上限；`None` 不限制（不发送，由服务端决定）。
    pub max_tokens: Option<u32>,
    /// 思考等级；`None` 不发送，用模型的默认值。
    pub effort: Option<ReasoningEffort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Text,
    Thinking,
    ToolUse {
        id: String,
        name: String,
    },
    /// Provider 新增、本地还不认识的块类型，内容会被忽略。
    Other(String),
}

/// 归一后的流式事件，对应 Messages API 的 SSE 事件序列。
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    MessageStart {
        model: Option<String>,
        input_tokens: Option<u64>,
    },
    BlockStart {
        index: usize,
        kind: BlockKind,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ThinkingDelta {
        index: usize,
        thinking: String,
    },
    SignatureDelta {
        index: usize,
        signature: String,
    },
    ToolInputDelta {
        index: usize,
        partial_json: String,
    },
    BlockStop {
        index: usize,
    },
    MessageDelta {
        stop_reason: Option<StopReason>,
        output_tokens: Option<u64>,
    },
    /// 用量（含缓存的命中与写入）。出现的字段覆盖之前的值，没出现的不变。
    Usage(UsageUpdate),
    MessageStop,
}

/// [`StreamEvent::Usage`] 的内容。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageUpdate {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

impl UsageUpdate {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("未配置 API Key")]
    MissingApiKey,
    #[error("网络错误: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("HTTP {status}: {body}")]
    Http {
        status: u16,
        body: String,
        /// 服务端在 `retry-after`（或 `retry-after-ms`）里要求的等待时间。
        retry_after: Option<std::time::Duration>,
    },
    /// 流式响应读到一半连接断了或超时（[`crate::HttpClient`] 的读超时）。可以重试。
    #[error("连接中断: {0}")]
    Interrupted(String),
    #[error("模型返回错误: {0}")]
    Api(String),
    #[error("无法解析流式响应: {0}")]
    Protocol(String),
    #[error("没有启用 {0:?} 协议的 Provider 插件")]
    UnsupportedProtocol(dshr_config::Protocol),
}

impl LlmError {
    /// 是否值得重试（网络错误、限流、5xx）。
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Http { status, .. } => *status == 408 || *status == 429 || *status >= 500,
            Self::Interrupted(_) => true,
            Self::Api(message) => message.contains("overloaded"),
            Self::MissingApiKey | Self::Protocol(_) | Self::UnsupportedProtocol(_) => false,
        }
    }
}

impl LlmError {
    /// 服务端要求重试前等多久；没有要求时是 `None`，由调用方按退避策略决定。
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// 非 2xx 的响应：带上状态码、正文和 `retry-after`。
    pub(crate) async fn from_response(response: reqwest::Response) -> Self {
        let status = response.status().as_u16();
        let header = |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok()).map(str::trim).map(str::to_owned);
        let retry_after = header("retry-after-ms")
            .and_then(|ms| ms.parse::<f64>().ok())
            .map(|ms| ms / 1000.0)
            .or_else(|| header("retry-after").and_then(|seconds| seconds.parse::<f64>().ok()))
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .map(std::time::Duration::from_secs_f64);
        let body = response.text().await.unwrap_or_default();
        Self::Http { status, body, retry_after }
    }
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send>>;

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// 写入 `request/header` 事件时使用的 Provider 标识。
    fn id(&self) -> &str;

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError>;
}
