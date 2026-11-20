//! 上下文压缩：上下文用量超过上下文窗口的一定比例时，把较早的对话摘要化。
//! 用量以模型报告的为准（见 [`context_tokens`]），压缩点可以在一轮之中（步骤边界）。

use std::sync::Arc;

use dshr_context::{JsonSchema, Manifest, Plugin, Scope};
use dshr_core::{ContentBlock, EventBody, Message, Projection, Role};
use dshr_llm::ChatRequest;

use crate::{AgentError, BeforeStep, BeforeStepHook, TurnContext};

const COMPACTION_INSTRUCTION: &str = r#"The conversation so far will be replaced by your summary, so write one that lets the work continue seamlessly:
- The user's requests and constraints (quote the latest request word for word) and preferences they stated.
- What has been done: decisions and why, files changed (paths, what changed), commands run and key results, errors and how they were resolved.
- Current state: what is in progress, remaining steps of the plan, and the exact next step.
- Details you will need again: file paths, function and type names, short code snippets.
Use the user's language. Reply with the summary only."#;

/// `[plugins.compaction]`。
#[derive(serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(default)]
struct Config {
    /// 估算用量达到上下文窗口的这个比例时压缩。
    #[schemars(range(min = 0.1, max = 1.0))]
    threshold: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self { threshold: 0.75 }
    }
}

pub struct CompactionPlugin;

impl Plugin for CompactionPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("compaction").config::<Config>()
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        let Config { threshold } = cx.config();
        // 手改配置文件可能写出范围之外的值：0 会让每一步都压缩，大于 1 等于关掉。
        let threshold = if threshold.is_finite() { threshold.clamp(0.1, 1.0) } else { Config::default().threshold };
        cx.hook::<BeforeStep>(0, Arc::new(Compactor { threshold }));
        Ok(())
    }
}

struct Compactor {
    threshold: f64,
}

/// 本轮里摘要请求失败过：上下文快满时摘要请求最容易失败，每一步都重发一遍整段上下文
/// 只会白白多花一次请求，所以失败一次就不再尝试。
#[derive(Default)]
struct Failed(bool);

#[async_trait::async_trait]
impl BeforeStepHook for Compactor {
    /// 把最近一个完整结束的轮次或步骤及之前的对话摘要化，写入 `context/compaction`。
    /// 只在轮次、步骤的边界压缩，避免拆开工具调用与结果；失败时跳过。
    async fn before_step(&self, turn: &mut TurnContext<'_>) -> Result<(), AgentError> {
        if turn.state.get_or_default::<Failed>().0 {
            return Ok(());
        }
        let limit = f64::from(turn.config.context_window) * self.threshold;
        if (context_tokens(turn) as f64) < limit {
            return Ok(());
        }
        let events = turn.session().events();
        let compacted_upto = events
            .iter()
            .rev()
            .find_map(|event| match event.body {
                EventBody::Compaction { upto_seq, .. } => Some(upto_seq),
                _ => None,
            })
            .unwrap_or(0);
        // 压缩到最近一个“调用和结果都已成对”的位置：一轮结束，或者一步结束（这一轮还在进行时也可以）。
        // 只在轮次边界压缩的话，第一轮、或一轮里连续几十步工具调用时上下文满了也压不了。
        let Some(upto) = events
            .iter()
            .rev()
            .find(|event| matches!(event.body, EventBody::TurnEnd { .. } | EventBody::StepEnd { .. }))
            .map(|event| event.seq)
            .filter(|upto| *upto > compacted_upto)
        else {
            return Ok(());
        };

        let mut earlier = turn.derive(events.iter().filter(|event| event.seq <= upto));
        let instruction = ContentBlock::text(COMPACTION_INSTRUCTION);
        match earlier.messages.last_mut() {
            Some(last) if last.role == Role::User => last.content.push(instruction),
            _ => earlier.messages.push(Message { role: Role::User, content: vec![instruction] }),
        }
        let request = ChatRequest {
            model: turn.config.model.clone(),
            system: earlier.system.map(|system| system.to_string()),
            messages: earlier.messages,
            tools: Vec::new(),
            max_tokens: None,
            effort: None,
        };
        // 摘要请求可能要等很久，用户点停止时立刻放弃；随后的模型请求会发现已取消并结束本轮。
        let summary = tokio::select! {
            biased;
            _ = turn.cancel.cancelled() => return Ok(()),
            summary = turn.complete_text(request) => summary,
        };
        let Some(summary) = summary.filter(|text| !text.trim().is_empty()) else {
            tracing::warn!("上下文压缩失败，本轮不再尝试");
            turn.state.get_or_default::<Failed>().0 = true;
            return Ok(());
        };
        turn.append(EventBody::Compaction { summary: summary.trim().into(), upto_seq: upto })
    }
}

/// 现在的上下文有多少 tokens。
///
/// 以最近一次回复报告的用量为准：那次请求的全部输入（含缓存）加上回复本身，就是当时完整的上下文，
/// 工具定义、追加的系统提示词都在里面，也是按模型自己的分词算的。之后新增的事件（工具结果等）再按
/// [`estimate_tokens`] 的办法估算。没有用量（旧日志、服务商不报告）或者之后压缩过时，整段估算。
fn context_tokens(turn: &mut TurnContext<'_>) -> usize {
    let events = turn.session().events();
    let last_compaction = events.iter().rposition(|event| matches!(event.body, EventBody::Compaction { .. }));
    let reported = events.iter().enumerate().rev().find_map(|(index, event)| match &event.body {
        EventBody::AssistantMessage { usage: Some(usage), .. } if usage.total_input_tokens() > 0 => Some((index, *usage)),
        _ => None,
    });
    match reported {
        Some((index, usage)) if last_compaction.is_none_or(|compaction| compaction < index) => {
            usage.context_tokens() as usize + estimate_events(&events[index + 1..])
        }
        _ => estimate_tokens(turn.projection()),
    }
}

/// 粗略估算这些事件进上下文后有多少 tokens，规则同 [`estimate_tokens`]。
fn estimate_events(events: &[dshr_core::SessionEvent]) -> usize {
    let mut bytes = 0;
    let mut images = 0;
    for event in events {
        bytes += match &event.body {
            EventBody::UserMessage { text } => text.len(),
            EventBody::UserAttachment { content, data, .. } => {
                images += usize::from(data.is_some());
                content.as_ref().map_or(0, |content| content.len())
            }
            EventBody::AssistantMessage { content, .. } => content.iter().map(block_len).sum(),
            EventBody::ToolResult { output, .. } => output.len(),
            EventBody::Ext { kind, data } if kind == dshr_core::MEDIA_IMAGES => {
                images += data.get("images").and_then(serde_json::Value::as_array).map_or(0, Vec::len);
                0
            }
            EventBody::Ext { data, .. } => json_len(data),
            _ => 0,
        };
    }
    bytes / 3 + images * IMAGE_TOKENS
}

fn block_len(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.len(),
        ContentBlock::Thinking { thinking, signature } => thinking.len() + signature.as_ref().map_or(0, |signature| signature.len()),
        ContentBlock::ToolUse { input, .. } => json_len(input),
        ContentBlock::ToolResult { content, .. } => content.len(),
        ContentBlock::Image { .. } => 0,
    }
}

/// 一张图片按这么多 tokens 算。模型按像素计费，与 base64 的长度无关：常见接口把图缩到约一百万像素以内，
/// 一张图大约 1.5k tokens。按 base64 字节数算的话，一张两三百 KB 的截图就会超过默认的压缩阈值。
const IMAGE_TOKENS: usize = 1_600;

/// 粗略估算上下文 tokens：文本按 UTF-8 字节数 / 3（中文约 1 字 1 token，英文偏保守），图片按 [`IMAGE_TOKENS`]。
/// 直接遍历消息结构把文本长度加起来，不做序列化：每一步都会调用，序列化整个投影太贵。
/// 数值与序列化后 JSON 的字节数同一量级（引号、转义与字段名的开销都在 / 3 的粗略范围内）。
pub fn estimate_tokens(projection: &Projection) -> usize {
    let mut bytes = projection.system.as_ref().map_or(0, |system| system.len());
    let mut images = 0;
    for message in &projection.messages {
        for block in &message.content {
            images += usize::from(matches!(block, ContentBlock::Image { .. }));
            bytes += block_len(block);
        }
    }
    bytes / 3 + images * IMAGE_TOKENS
}

/// 值序列化成 JSON 有多少字节：只数不存，每一步估算时不必为每个工具参数（常常是整份文件内容）再拷一份字符串。
fn json_len(value: &serde_json::Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}
