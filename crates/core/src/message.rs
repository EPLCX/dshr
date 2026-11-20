use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// 内容块。序列化形状与 Messages API 的线上格式一致，因此既用于会话日志，也直接用于请求体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    // 大段文本都用 `Arc<str>`：投影（`derive_messages_with`）把事件内容复制进上下文时只加引用计数，
    // 不再逐块深拷贝（`Arc<str>` 的 serde 形状与 `String` 相同）。
    Text {
        text: Arc<str>,
    },
    Thinking {
        thinking: Arc<str>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<Arc<str>>,
    },
    ToolUse {
        id: String,
        name: String,
        /// 工具参数。`write_file` / `edit_file` 的参数就是整份文件内容，而每一步都要把历史消息投影一遍、
        /// 拼进请求：用 `Arc` 共享，克隆只加引用计数（序列化形状不变）。
        input: Arc<Value>,
    },
    ToolResult {
        tool_use_id: String,
        content: Arc<str>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// 图片（Messages API 的写法：`{"type":"image","source":{"type":"base64","media_type":..,"data":..}}`）。
    /// 现在只出现在发给模型的请求里：工具交回的图片（[`MEDIA_IMAGES`] 事件）投影成它。
    Image {
        source: ImageSource,
    },
}

/// 图片附件的格式：按扩展名判断，也是 `media/images` 事件接受的类型。
pub const IMAGE_MEDIA_TYPES: [(&str, &str); 5] =
    [("png", "image/png"), ("jpg", "image/jpeg"), ("jpeg", "image/jpeg"), ("gif", "image/gif"), ("webp", "image/webp")];

/// 路径的扩展名是不是支持的图片格式；是则返回对应的 media type。
pub fn image_media_type(path: &std::path::Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_MEDIA_TYPES.iter().find(|(ext, _)| *ext == extension).map(|(_, media_type)| *media_type)
}

/// 图片的来源：base64 编码的数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSource {
    /// 总是 `base64`。
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: String,
    /// base64 内容，通常是消息里最大的一段数据，用 `Arc<str>` 让投影克隆只加引用计数。
    pub data: Arc<str>,
}

impl ImageSource {
    pub fn base64(media_type: impl Into<String>, data: impl Into<Arc<str>>) -> Self {
        Self { kind: "base64".into(), media_type: media_type.into(), data: data.into() }
    }
}

/// 工具交回的图片：一条插件事件，数据是 `{ call_id, images: [{ media_type, data }] }`（data 是 base64），
/// 紧跟在那次调用的 `tool/result` 后面写进日志，投影时变成 [`ContentBlock::Image`] 接在工具结果后面。
pub const MEDIA_IMAGES: &str = "media/images";

impl ContentBlock {
    pub fn text(text: impl Into<Arc<str>>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    #[serde(other)]
    Other,
}

/// 一次请求的用量（Messages API 的口径）：`input_tokens` 是没有命中、也没有写入缓存的那部分输入，
/// 命中缓存和写入缓存的输入分别记在 `cache_read_input_tokens`、`cache_creation_input_tokens`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_read_input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    /// 这次请求的输入一共多少 tokens（含缓存命中与写入）。
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }

    /// 这次请求之后上下文有多大：全部输入加上这次的输出（输出会作为下一次请求的输入）。
    pub fn context_tokens(&self) -> u64 {
        self.total_input_tokens() + self.output_tokens
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}
