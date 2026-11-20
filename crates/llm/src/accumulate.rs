use dshr_core::{ContentBlock, StopReason, Usage};
use serde_json::Value;

use crate::types::{BlockKind, LlmError, StreamEvent};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AssembledMessage {
    pub model: Option<String>,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
}

#[derive(Debug)]
enum Partial {
    Text(String),
    Thinking { thinking: String, signature: Option<String> },
    ToolUse { id: String, name: String, json: String },
    Ignored,
}

/// 一条消息最多这么多个内容块。块的序号来自服务端，按序号开数组：不设上限的话，一个异常的序号
/// （网关出错、恶意的兼容服务）就能让这里一次分配几十 GB。实际的回复远远到不了这个数。
const MAX_BLOCKS: usize = 4096;

/// 把 [`StreamEvent`] 序列拼成完整消息。
#[derive(Debug, Default)]
pub struct MessageAccumulator {
    model: Option<String>,
    blocks: Vec<Option<Partial>>,
    stop_reason: Option<StopReason>,
    usage: Usage,
}

impl MessageAccumulator {
    pub fn push(&mut self, event: &StreamEvent) -> Result<(), LlmError> {
        match event {
            StreamEvent::MessageStart { model, input_tokens } => {
                self.model = model.clone();
                self.usage.input_tokens = input_tokens.unwrap_or(0);
            }
            StreamEvent::BlockStart { index, kind } => {
                let partial = match kind {
                    BlockKind::Text => Partial::Text(String::new()),
                    BlockKind::Thinking => Partial::Thinking { thinking: String::new(), signature: None },
                    BlockKind::ToolUse { id, name } => Partial::ToolUse { id: id.clone(), name: name.clone(), json: String::new() },
                    BlockKind::Other(_) => Partial::Ignored,
                };
                if *index >= MAX_BLOCKS {
                    return Err(LlmError::Protocol(format!("内容块序号 {index} 超出上限 {MAX_BLOCKS}")));
                }
                if self.blocks.len() <= *index {
                    self.blocks.resize_with(index + 1, || None);
                }
                self.blocks[*index] = Some(partial);
            }
            StreamEvent::TextDelta { index, text } => {
                if let Some(Partial::Text(buffer)) = self.block(*index)? {
                    buffer.push_str(text);
                }
            }
            StreamEvent::ThinkingDelta { index, thinking } => {
                if let Some(Partial::Thinking { thinking: buffer, .. }) = self.block(*index)? {
                    buffer.push_str(thinking);
                }
            }
            StreamEvent::SignatureDelta { index, signature } => {
                if let Some(Partial::Thinking { signature: slot, .. }) = self.block(*index)? {
                    slot.get_or_insert_with(String::new).push_str(signature);
                }
            }
            StreamEvent::ToolInputDelta { index, partial_json } => {
                if let Some(Partial::ToolUse { json, .. }) = self.block(*index)? {
                    json.push_str(partial_json);
                }
            }
            StreamEvent::MessageDelta { stop_reason, output_tokens } => {
                if stop_reason.is_some() {
                    self.stop_reason = stop_reason.clone();
                }
                if let Some(tokens) = output_tokens {
                    self.usage.output_tokens = *tokens;
                }
            }
            StreamEvent::Usage(update) => {
                let usage = &mut self.usage;
                for (slot, value) in [
                    (&mut usage.input_tokens, update.input_tokens),
                    (&mut usage.output_tokens, update.output_tokens),
                    (&mut usage.cache_read_input_tokens, update.cache_read_input_tokens),
                    (&mut usage.cache_creation_input_tokens, update.cache_creation_input_tokens),
                ] {
                    if let Some(value) = value {
                        *slot = value;
                    }
                }
            }
            StreamEvent::BlockStop { .. } | StreamEvent::MessageStop => {}
        }
        Ok(())
    }

    fn block(&mut self, index: usize) -> Result<Option<&mut Partial>, LlmError> {
        match self.blocks.get_mut(index) {
            Some(Some(partial)) => Ok(Some(partial)),
            _ => Err(LlmError::Protocol(format!("内容块 {index} 未开始就收到增量"))),
        }
    }

    pub fn finish(self) -> AssembledMessage {
        let content = self
            .blocks
            .into_iter()
            .flatten()
            .filter_map(|partial| match partial {
                Partial::Text(text) => Some(ContentBlock::Text { text: text.into() }),
                Partial::Thinking { thinking, signature } => {
                    Some(ContentBlock::Thinking { thinking: thinking.into(), signature: signature.map(Into::into) })
                }
                Partial::ToolUse { id, name, json } => {
                    let input = if json.trim().is_empty() {
                        Value::Object(Default::default())
                    } else {
                        // 不完整的 JSON 原样作为字符串保留，交给工具层报错，而不是丢掉这次调用。
                        serde_json::from_str(&json).unwrap_or(Value::String(json))
                    };
                    Some(ContentBlock::ToolUse { id, name, input: input.into() })
                }
                Partial::Ignored => None,
            })
            .collect();
        AssembledMessage { model: self.model, content, stop_reason: self.stop_reason, usage: self.usage }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absurd_block_index_is_a_protocol_error_not_an_allocation() {
        let mut accumulator = MessageAccumulator::default();
        let start = |index| StreamEvent::BlockStart { index, kind: BlockKind::Text };
        assert!(accumulator.push(&start(3)).is_ok());
        assert!(matches!(accumulator.push(&start(usize::MAX / 2)), Err(LlmError::Protocol(_))));
        assert!(matches!(accumulator.push(&start(MAX_BLOCKS)), Err(LlmError::Protocol(_))));
    }
}
