use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::{ContentBlock, StopReason, Usage};

/// 会话日志中的一条持久事件：`{"seq":..,"time":..,"type":"..","data":{..}}`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub seq: u64,
    /// Unix 毫秒时间戳。
    pub time: u64,
    #[serde(flatten)]
    pub body: EventBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum EventBody {
    #[serde(rename = "turn/start")]
    TurnStart { turn: u32 },
    #[serde(rename = "turn/end")]
    TurnEnd {
        turn: u32,
        reason: TurnEndReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "step/start")]
    StepStart { turn: u32, step: u32 },
    #[serde(rename = "step/end")]
    StepEnd { turn: u32, step: u32 },
    // 大段文本用 `Arc<str>`：投影把事件内容复制进上下文时只加引用计数，不再深拷贝。
    #[serde(rename = "system/message")]
    SystemMessage { text: Arc<str> },
    #[serde(rename = "user/message")]
    UserMessage { text: Arc<str> },
    /// 随上一条用户消息附上的文件（输入卡片的“+”）。内容在发送时读入并写进日志，之后文件变了也不影响上下文。
    /// 图片附件只有 `path`、`media_type`（`image/...`）和 base64 后的 `data`，没有 `content`。
    #[serde(rename = "user/attachment")]
    UserAttachment {
        path: String,
        /// 文本附件的全部内容；图片附件为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Arc<str>>,
        /// 图片附件的类型（`image/png` 等）；文本附件为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        /// 图片附件的 base64 内容；文本附件为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Arc<str>>,
    },
    #[serde(rename = "assistant/message")]
    AssistantMessage {
        model: String,
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// 一次失败的模型请求；成功的那次直接记为 `assistant/message`。
    #[serde(rename = "assistant/attempt")]
    AssistantAttempt { attempt: u32, error: String },
    #[serde(rename = "tool/call")]
    ToolCall { call_id: String, name: String, input: Value },
    #[serde(rename = "tool/result")]
    ToolResult {
        call_id: String,
        output: Arc<str>,
        #[serde(default)]
        is_error: bool,
    },
    #[serde(rename = "request/header")]
    RequestHeader { provider: String, model: String },
    /// 会话标题（模型生成或用户重命名）；以最后一条为准。
    #[serde(rename = "session/title")]
    SessionTitle { title: String },
    /// 会话所在的工作目录（第一轮开始时写入）；侧边栏按它把会话归到工作区下。
    #[serde(rename = "session/workspace")]
    SessionWorkspace { path: String },
    /// 会话的模型与交互选项（输入卡片上选的模式、权限、模型）；每次变化时写入，以最后一条为准。
    /// 只用于下次打开会话时恢复界面状态，不影响模型上下文。
    #[serde(rename = "session/options")]
    SessionOptions {
        /// 内置预设：`standard` / `plan` / `review` / `create`。
        preset: String,
        /// 插件预设：`<插件>/<id>`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plugin_preset: Option<String>,
        /// 权限模式：`manual` / `workspace` / `auto_review` / `full_access`（旧版本的 `read_only` 按 `manual` 读）。
        permission: String,
        /// 模型的服务商。
        model_provider: String,
        /// 模型 id。
        model: String,
    },
    /// 从另一个会话分叉而来（写在新会话开头）。
    #[serde(rename = "session/forked")]
    SessionForked { from: String, turn: u32 },
    /// 上下文压缩：`upto_seq` 及之前的对话由 `summary` 代替，原事件仍保留在日志里。
    #[serde(rename = "context/compaction")]
    Compaction { summary: Arc<str>, upto_seq: u64 },
    /// 本轮没有结束、自动接着请求模型（如回复被输出上限截断）。投影成一条提示模型继续的 user 文本。
    #[serde(rename = "turn/continue")]
    TurnContinue { reason: ContinueReason },
    /// 插件自定义的事件：`type` 是带命名空间的名字（如 `todo/item`），`data` 的形状由插件自己定。
    /// 怎么进模型上下文由 [`crate::EventProjector`] 决定；对应的插件不在时事件仍留在日志里，只是不参与投影。
    /// 用 [`EventBody::ext`] 构造，它会检查名字。
    #[serde(untagged)]
    Ext {
        #[serde(rename = "type", deserialize_with = "deserialize_ext_kind")]
        kind: String,
        #[serde(default)]
        data: Value,
    },
}

/// 核心事件的类型名。插件事件不能用这些名字，也不能用它们的命名空间（`/` 之前的部分）。
const CORE_KINDS: [&str; 18] = [
    "turn/start",
    "turn/end",
    "step/start",
    "step/end",
    "system/message",
    "user/message",
    "user/attachment",
    "assistant/message",
    "assistant/attempt",
    "tool/call",
    "tool/result",
    "request/header",
    "session/title",
    "session/workspace",
    "session/options",
    "session/forked",
    "context/compaction",
    "turn/continue",
];

/// 是不是核心事件的类型名。
pub fn is_core_kind(kind: &str) -> bool {
    CORE_KINDS.contains(&kind)
}

/// 读日志时：类型名是核心事件、但内容对不上的行不当成插件事件，留给 [`LogEntry::Unknown`]。
fn deserialize_ext_kind<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let kind = String::deserialize(deserializer)?;
    if is_core_kind(&kind) {
        return Err(serde::de::Error::custom(format_args!("`{kind}` 事件的内容格式不对")));
    }
    Ok(kind)
}

/// 插件事件的类型名不合规。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtKindError {
    #[error("事件类型 `{0}` 应当写成 `命名空间/名字`，只用小写字母、数字和 `-`、`_`、`.`")]
    Malformed(String),
    #[error("事件类型 `{0}` 用了核心事件的命名空间")]
    Reserved(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinueReason {
    /// 回复达到 `max_tokens`，被截断的工具调用已丢弃。
    MaxTokens,
}

impl EventBody {
    /// 插件自定义事件。`kind` 写成 `命名空间/名字`（如 `todo/item`），命名空间一般就是插件的 id；
    /// 核心事件用的命名空间（`turn`、`session`、`tool` 等）不能用。
    pub fn ext(kind: impl Into<String>, data: Value) -> Result<Self, ExtKindError> {
        let kind = kind.into();
        let part_ok = |part: &str| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        };
        match kind.split_once('/') {
            Some((namespace, name)) if part_ok(namespace) && part_ok(name) => {
                let reserved = CORE_KINDS.iter().any(|core| core.split_once('/').is_some_and(|(ns, _)| ns == namespace));
                if reserved { Err(ExtKindError::Reserved(kind)) } else { Ok(Self::Ext { kind, data }) }
            }
            _ => Err(ExtKindError::Malformed(kind)),
        }
    }

    /// 事件类型名，与序列化后的 `type` 字段相同。
    pub fn kind(&self) -> &str {
        match self {
            Self::TurnStart { .. } => "turn/start",
            Self::TurnEnd { .. } => "turn/end",
            Self::StepStart { .. } => "step/start",
            Self::StepEnd { .. } => "step/end",
            Self::SystemMessage { .. } => "system/message",
            Self::UserMessage { .. } => "user/message",
            Self::UserAttachment { .. } => "user/attachment",
            Self::AssistantMessage { .. } => "assistant/message",
            Self::AssistantAttempt { .. } => "assistant/attempt",
            Self::ToolCall { .. } => "tool/call",
            Self::ToolResult { .. } => "tool/result",
            Self::RequestHeader { .. } => "request/header",
            Self::SessionTitle { .. } => "session/title",
            Self::SessionWorkspace { .. } => "session/workspace",
            Self::SessionOptions { .. } => "session/options",
            Self::SessionForked { .. } => "session/forked",
            Self::Compaction { .. } => "context/compaction",
            Self::TurnContinue { .. } => "turn/continue",
            Self::Ext { kind, .. } => kind,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Interrupted,
    MaxSteps,
    Error,
}

/// 读取日志时的一行：事件解析成 [`SessionEvent`]（本程序不认识的类型，如插件事件、原版新增的事件，
/// 解析成 [`EventBody::Ext`]）；不是事件形状的行、内容对不上的核心事件原样保留。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LogEntry {
    Known(SessionEvent),
    Unknown(Value),
}

impl LogEntry {
    pub fn known(&self) -> Option<&SessionEvent> {
        match self {
            Self::Known(event) => Some(event),
            Self::Unknown(_) => None,
        }
    }
}
