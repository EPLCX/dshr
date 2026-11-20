//! 会话事件模型与持久化。
//!
//! 事件名沿用原版 dsh 的 durable session events（`turn/start`、`tool/call` 等），
//! 每条事件都带 `seq`、`time`、`data`。`data` 内的字段目前是暂定形状，
//! M0 对齐原版 schema 后再收紧。

pub mod event;
pub mod log;
pub mod message;
pub mod projection;

pub use event::{ContinueReason, EventBody, ExtKindError, LogEntry, SessionEvent, TurnEndReason, is_core_kind};
pub use log::{CoreError, SessionLog, log_reader, read_entries};
pub use message::{ContentBlock, IMAGE_MEDIA_TYPES, ImageSource, MEDIA_IMAGES, Message, Role, StopReason, Usage, image_media_type};
pub use projection::{COMPACTION_PREFIX, EventProjector, Projection, derive_messages, derive_messages_with};
