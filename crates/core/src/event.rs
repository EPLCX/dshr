use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::{ContentBlock, StopReason, Usage};

/// 会话日志中的一条持久事件：`{"seq":..,"time":..,"type":"..","data":{..}}`。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionEvent {
    pub seq: u64,
    /// Unix 毫秒时间戳。
    pub time: u64,
    #[serde(flatten)]
    pub body: EventBody,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
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
    /// `input` 用 `Arc` 共享：写 `write_file` 这类调用时参数就是整份文件内容，记进日志、推给界面都只加引用计数
    /// （序列化形状不变）。
    #[serde(rename = "tool/call")]
    ToolCall { call_id: String, name: String, input: Arc<Value> },
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
        #[serde(rename = "type")]
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
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum LogEntry {
    Known(SessionEvent),
    Unknown(Value),
}

impl<'de> Deserialize<'de> for LogEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match SessionEvent::deserialize(&value) {
            Ok(event) => Ok(Self::Known(event)),
            Err(_) => Ok(Self::Unknown(value)),
        }
    }
}

impl LogEntry {
    /// 解析日志的一行。先直接按事件解析（不经过中间结构）；不是事件形状的行再解析成 `Value` 原样保留。
    pub fn parse(line: &str) -> serde_json::Result<Self> {
        match serde_json::from_str::<SessionEvent>(line) {
            Ok(event) => Ok(Self::Known(event)),
            Err(_) => serde_json::from_str(line).map(Self::Unknown),
        }
    }

    pub fn known(&self) -> Option<&SessionEvent> {
        match self {
            Self::Known(event) => Some(event),
            Self::Unknown(_) => None,
        }
    }
}

/// 核心事件的 `data` 怎么解析：和 [`EventBody`] 的变体一一对应（`remote` 派生会逐个字段对照类型，对不上编译不过）。
/// 单独派生是因为 `EventBody` 里有 `#[serde(untagged)]` 的 `Ext`：serde 碰到它会先把整行缓存成中间结构再逐个试，
/// 读日志慢一截。这里只有带标签的变体，由 [`SessionEvent`] 的反序列化按 `type` 直接把 `data` 解析进对应的变体。
#[derive(Deserialize)]
#[serde(remote = "EventBody")]
enum CoreBody {
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
    #[serde(rename = "system/message")]
    SystemMessage { text: Arc<str> },
    #[serde(rename = "user/message")]
    UserMessage { text: Arc<str> },
    #[serde(rename = "user/attachment")]
    UserAttachment {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Arc<str>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
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
    #[serde(rename = "assistant/attempt")]
    AssistantAttempt { attempt: u32, error: String },
    #[serde(rename = "tool/call")]
    ToolCall { call_id: String, name: String, input: Arc<Value> },
    #[serde(rename = "tool/result")]
    ToolResult {
        call_id: String,
        output: Arc<str>,
        #[serde(default)]
        is_error: bool,
    },
    #[serde(rename = "request/header")]
    RequestHeader { provider: String, model: String },
    #[serde(rename = "session/title")]
    SessionTitle { title: String },
    #[serde(rename = "session/workspace")]
    SessionWorkspace { path: String },
    #[serde(rename = "session/options")]
    SessionOptions {
        preset: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plugin_preset: Option<String>,
        permission: String,
        model_provider: String,
        model: String,
    },
    #[serde(rename = "session/forked")]
    SessionForked { from: String, turn: u32 },
    #[serde(rename = "context/compaction")]
    Compaction { summary: Arc<str>, upto_seq: u64 },
    #[serde(rename = "turn/continue")]
    TurnContinue { reason: ContinueReason },
    #[serde(skip_deserializing)]
    #[allow(dead_code)]
    Ext { kind: String, data: Value },
}

impl<'de> Deserialize<'de> for SessionEvent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(EventVisitor)
    }
}

impl<'de> Deserialize<'de> for EventBody {
    /// 单独的事件内容：`{"type":..,"data":..}`。
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(deserializer.deserialize_map(EventVisitor)?.body)
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "lowercase")]
enum Field {
    Seq,
    Time,
    Type,
    Data,
    #[serde(other)]
    Other,
}

/// 逐个字段读一行事件。`type` 在 `data` 前面时（本程序写的日志都是这样）`data` 直接解析进对应的变体；
/// 反过来时先把 `data` 读成 `Value`。单独解析 [`EventBody`] 时没有 `seq`、`time`，记为 0。
struct EventVisitor;

impl<'de> serde::de::Visitor<'de> for EventVisitor {
    type Value = SessionEvent;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a session event")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<SessionEvent, A::Error> {
        use serde::de::Error as _;
        let (mut seq, mut time, mut kind, mut body, mut early_data) = (None, None, None::<String>, None, None);
        while let Some(field) = map.next_key::<Field>()? {
            match field {
                Field::Seq => seq = Some(map.next_value()?),
                Field::Time => time = Some(map.next_value()?),
                Field::Type => kind = Some(map.next_value()?),
                Field::Data => match &kind {
                    Some(kind) => body = Some(map.next_value_seed(DataSeed(kind))?),
                    None => early_data = Some(map.next_value::<Value>()?),
                },
                Field::Other => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }
        let kind = kind.ok_or_else(|| A::Error::missing_field("type"))?;
        let body = match body {
            Some(body) => body,
            None => {
                serde::de::DeserializeSeed::deserialize(DataSeed(&kind), early_data.unwrap_or(Value::Null)).map_err(A::Error::custom)?
            }
        };
        Ok(SessionEvent { seq: seq.unwrap_or_default(), time: time.unwrap_or_default(), body })
    }
}

/// 按已经读到的 `type` 解析 `data`：核心事件解析进对应的变体（对不上就是错误，整行留给 [`LogEntry::Unknown`]），
/// 其余的是插件事件，`data` 原样保留。
struct DataSeed<'a>(&'a str);

impl<'de> serde::de::DeserializeSeed<'de> for DataSeed<'_> {
    type Value = EventBody;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<EventBody, D::Error> {
        if is_core_kind(self.0) {
            CoreBody::deserialize(Tagged { kind: self.0, data: deserializer })
        } else {
            Ok(EventBody::Ext { kind: self.0.to_owned(), data: Value::deserialize(deserializer)? })
        }
    }
}

/// 把“变体名 + 内容”交给按外部标签派生的 [`CoreBody`]：变体名是 `type`，内容由 `data` 的反序列化器直接提供。
struct Tagged<'a, D> {
    kind: &'a str,
    data: D,
}

impl<'de, D: serde::Deserializer<'de>> serde::Deserializer<'de> for Tagged<'_, D> {
    type Error = D::Error;

    fn deserialize_any<V: serde::de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
        visitor.visit_enum(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option unit
        unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier ignored_any
    }
}

impl<'de, D: serde::Deserializer<'de>> serde::de::EnumAccess<'de> for Tagged<'_, D> {
    type Error = D::Error;
    type Variant = Content<D>;

    fn variant_seed<V: serde::de::DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Content<D>), D::Error> {
        let kind = serde::de::value::StrDeserializer::<D::Error>::new(self.kind);
        Ok((seed.deserialize(kind)?, Content(self.data)))
    }
}

struct Content<D>(D);

impl<'de, D: serde::Deserializer<'de>> serde::de::VariantAccess<'de> for Content<D> {
    type Error = D::Error;

    fn unit_variant(self) -> Result<(), D::Error> {
        serde::de::IgnoredAny::deserialize(self.0).map(|_| ())
    }

    fn newtype_variant_seed<T: serde::de::DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, D::Error> {
        seed.deserialize(self.0)
    }

    fn tuple_variant<V: serde::de::Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_tuple(len, visitor)
    }

    fn struct_variant<V: serde::de::Visitor<'de>>(self, fields: &'static [&'static str], visitor: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_struct("data", fields, visitor)
    }
}
