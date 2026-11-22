//! `delegate`：把一项相对独立的子任务交给子代理。子代理有自己的上下文（日志写在父会话目录的
//! `agents/<call_id>.jsonl`），用同样的模型和权限模式，只把最后的报告交回给父代理。
//! 子代理的进度显示在父会话里这张工具卡片的实时输出中，审批请求也挂在这张卡片上。

use std::sync::Arc;

use dshr_context::{Context, Manifest, Plugin, Scope};
use dshr_core::EventBody;
use dshr_tools::{Tool, ToolContext, ToolOutput, Tools};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::preset::preset_read_only;
use crate::system_prompt::SUBAGENT_PROMPT;
use crate::{Agent, AgentConfig, AgentIdentity, ApprovalRequest, Approver, CallEnv, Chunk, Decision, LiveEvent, Preset, Session, Subagent};

pub const DELEGATE: &str = "delegate";

const DESCRIPTION: &str = "Hand a self-contained subtask to a sub-agent that works in its own context with the \
same tools, then returns a written report. Use it for focused research or a well-scoped change whose details you \
do not need to keep in your own context. Describe the task completely: the sub-agent cannot see this conversation. \
Only its report comes back to you.";

/// 子代理每一步工具调用在卡片里显示的参数长度。
const PROGRESS_INPUT_CHARS: usize = 120;

/// 提供 `delegate` 工具。子代理用 `subagent` 里的插件：那里面不放本插件，子代理就不能再委派，
/// 避免层层嵌套。
pub struct DelegatePlugin {
    subagent: Context,
}

impl DelegatePlugin {
    pub fn new(subagent: Context) -> Self {
        Self { subagent }
    }
}

impl Plugin for DelegatePlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("delegate")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<Tools>(Arc::new(Delegate { plugins: self.subagent.clone() }));
        cx.provide::<SubagentRunner>(Arc::new(SubagentRunner { plugins: self.subagent.clone() }));
        Ok(())
    }
}

struct Delegate {
    plugins: Context,
}

#[async_trait::async_trait]
impl Tool for Delegate {
    fn name(&self) -> &str {
        DELEGATE
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"task": {"type": "string", "description": "Complete instructions for the sub-agent."}},
            "required": ["task"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(env) = ctx.options.get::<CallEnv>() else {
            return ToolOutput::error("delegate 只能在 agent 循环里调用");
        };
        let Some(task) = input.get("task").and_then(Value::as_str).filter(|task| !task.trim().is_empty()) else {
            return ToolOutput::error("参数无效: 缺少 task");
        };
        let card = ctx.clone();
        let runner = SubagentRunner { plugins: self.plugins.clone() };
        match runner.run(env, ctx.workspace.clone(), &env.call_id, task, None, move |line| card.emit_output(line)).await {
            Ok(report) => ToolOutput::ok(report),
            Err(error) => ToolOutput::error(error),
        }
    }
}

/// 起一个子代理跑完一项任务：`delegate` 工具和插件（JS 的 `host.agent.run`）共用。由 [`DelegatePlugin`] 作为服务提供，
/// 所以只有主代理那边有；子代理里的插件拿不到它，不会层层嵌套。
#[derive(Clone)]
pub struct SubagentRunner {
    plugins: Context,
}

impl SubagentRunner {
    /// `env` 是发起这次调用的环境（模型、审批、配置、取消）；`log_name` 决定子代理日志的文件名
    /// （`agents/<log_name>.jsonl`）；`progress` 收到子代理每一步的进度行。返回子代理的最终报告。
    /// `preset` 是子代理的预设，给了就附加它的子代理版说明；`None` 时只沿用父代理预设的限制，不带它的说明。
    /// 父代理处在只读的预设里时子代理也只读，不管这里给的是什么。
    pub async fn run(
        &self,
        env: &CallEnv,
        workspace: std::path::PathBuf,
        log_name: &str,
        task: &str,
        preset: Option<Preset>,
        progress: impl Fn(&str) + Send + Sync + 'static,
    ) -> Result<String, String> {
        let mut config = AgentConfig { system_prompt: SUBAGENT_PROMPT.into(), ..(*env.config).clone() };
        // 父代理的预设留在 options 里，只为了它的限制（只读、插件预设的工具名单）；说明不带过来，见 [`Subagent`]。
        let explicit = preset.filter(|_| !preset_read_only(&config.options));
        if let Some(preset) = explicit {
            config.options.insert(preset);
        }
        config.options.insert(Subagent { explicit_preset: explicit.is_some() });
        // 子代理的身份：属于发起它的会话，代理名是它的日志名。
        config.options.insert(AgentIdentity { session: env.identity.session.clone(), agent: log_name.to_owned() });
        let mut session = Session::create(log_name, env.agents_dir.join(format!("{log_name}.jsonl")))
            .map_err(|error| format!("无法创建子代理日志: {error}"))?;
        let sub = Agent::new(
            env.provider.clone(),
            self.plugins.clone(),
            Arc::new(ForwardApprover { parent: env.approver.clone(), call_id: env.call_id.clone() }),
            ToolContext::new(workspace),
            config,
        );

        // 子代理的事件翻译成一行行进度。
        let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
        let forward = tokio::spawn(async move {
            while let Some(event) = sub_rx.recv().await {
                let line = match event {
                    LiveEvent::Durable(event) => match &event.body {
                        EventBody::ToolCall { name, input, .. } => Some(format!("→ {name} {}\n", clip(&input.to_string()))),
                        EventBody::ToolResult { is_error: true, output, .. } => Some(format!("  ✗ {}\n", clip(output))),
                        _ => None,
                    },
                    LiveEvent::Chunk(Chunk::ToolOutput { text, .. }) => Some(text),
                    LiveEvent::Chunk(_) => None,
                };
                if let Some(text) = line {
                    progress(&text);
                }
            }
        });

        let outcome = sub.run_turn(&mut session, task.to_owned(), &sub_tx, &env.cancel).await;
        drop(sub_tx);
        let _ = forward.await;
        match outcome {
            Ok(outcome) if outcome.error.is_none() && !outcome.final_text.trim().is_empty() => Ok(outcome.final_text),
            Ok(outcome) => Err(match outcome.error {
                Some(error) => format!("子代理出错: {error}"),
                None => format!("子代理没有给出报告（{:?}）", outcome.reason),
            }),
            Err(error) => Err(format!("子代理失败: {error}")),
        }
    }
}

/// 把子代理的审批请求挂到父会话的 `delegate` 卡片上。
struct ForwardApprover {
    parent: Arc<dyn Approver>,
    call_id: String,
}

#[async_trait::async_trait]
impl Approver for ForwardApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        let request = ApprovalRequest { call_id: self.call_id.clone(), ..request.clone() };
        self.parent.approve(&request).await
    }
}

fn clip(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(PROGRESS_INPUT_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}
