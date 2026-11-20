//! 随用户消息发送的附件（[`Agent::run_turn_with`] 的参数、写进日志的 `user/attachment` 事件）。

/// 一条用户附件：文本或图片。内容在发送时读入并写进日志，之后文件变了也不影响上下文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserAttachment {
    /// （路径，文件内容）；投影成 `<attachment path="…">` 文本块。
    Text(String, String),
    /// （路径，media type，base64 内容）；投影成 [`dshr_core::ContentBlock::Image`]，
    /// 能看图的模型直接看到，不能看图的收到一句说明。
    Image { path: String, media_type: String, data: String },
}
