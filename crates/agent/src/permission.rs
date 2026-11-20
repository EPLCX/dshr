//! 权限模式：按工具的访问类型决定放行、询问还是拒绝（规则见 `docs/permissions.md`）。

use std::sync::Arc;
use std::time::Duration;

use dshr_context::{Manifest, Options, Plugin, Scope};
use dshr_core::{ContentBlock, Message, Role};
use dshr_llm::{ChatRequest, MessageAccumulator, StreamEvent};
use dshr_sandbox::Sandbox;
use dshr_tools::{Access, ToolContext};
use futures::StreamExt;

use crate::{CallEnv, GateRequest, GateTool, LlmAccess, ReviewTool, ReviewToolHook, collect_text};

/// 本轮的权限模式，放在 [`crate::AgentConfig::options`] 里；没放时是“工作区内修改”。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PermissionMode {
    /// 手动确认：除了读取，每一次写入、运行命令、调用外部服务都先问用户。
    Manual,
    /// 工作区内写入直接放行；工作区外写入与执行命令需要询问。
    #[default]
    Workspace,
    /// 自动审查：工作区内写入直接放行；其余会改东西的操作先让模型判断风险，低风险直接放行，
    /// 有风险的再问用户（附上判断的理由）。
    AutoReview,
    /// 全部放行，不再询问。
    FullAccess,
}

impl PermissionMode {
    pub fn of(options: &Options) -> Self {
        options.get::<Self>().copied().unwrap_or_default()
    }

    /// 写进会话日志（`session/options`）的名字。
    pub fn id(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Workspace => "workspace",
            Self::AutoReview => "auto_review",
            Self::FullAccess => "full_access",
        }
    }

    /// 从 [`Self::id`] 读回来。旧版本的“只读”（`read_only`）已经去掉，按最谨慎的“手动确认”恢复。
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "manual" | "read_only" => Some(Self::Manual),
            "workspace" => Some(Self::Workspace),
            "auto_review" => Some(Self::AutoReview),
            "full_access" => Some(Self::FullAccess),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Allow,
    Ask,
    /// 询问用户，并在审批处附上一段说明（如自动审查给出的风险理由）。
    Confirm(String),
    /// 拒绝，附上给模型的说明。
    Deny(String),
    /// 必须由用户亲自确认（可附一段说明）：不看权限模式（完全访问也问），审查钩子（自动审查、
    /// 插件的 `reviewTool`）不能改判，界面上也没有“本会话不再询问”。用于把权力交出去的操作，
    /// 比如给插件授予能力——这类决定只能是用户自己做的。插件的 `gate` 钩子不能给出这个结论。
    Require(Option<String>),
}

impl Gate {
    /// 结论是要问用户（[`Gate::Ask`]、[`Gate::Confirm`] 或 [`Gate::Require`]）。
    pub fn asks(&self) -> bool {
        matches!(self, Self::Ask | Self::Confirm(_) | Self::Require(_))
    }

    /// 只能由用户亲自确认（[`Gate::Require`]）。
    pub fn mandatory(&self) -> bool {
        matches!(self, Self::Require(_))
    }
}

/// `confined`：命令在沙箱里运行、写不出工作区。
pub fn gate(mode: PermissionMode, access: &Access, ctx: &ToolContext, confined: bool) -> Gate {
    match (mode, access) {
        (_, Access::Read) => Gate::Allow,
        (PermissionMode::FullAccess, _) => Gate::Allow,
        (PermissionMode::Manual, _) => Gate::Ask,
        (PermissionMode::Workspace | PermissionMode::AutoReview, Access::Write(path)) if ctx.is_inside_workspace(path) => Gate::Allow,
        // 命令在沙箱里运行、写不出工作区时直接放行（macOS）；否则影响范围无法事先判断，询问。
        (PermissionMode::Workspace, Access::Execute) if confined => Gate::Allow,
        // 自动审查模式下这里的“询问”会先交给 [`AutoReviewer`]，它判断为低风险时直接放行。
        (PermissionMode::Workspace | PermissionMode::AutoReview, _) => Gate::Ask,
    }
}

/// 这一轮是不是只读：选了只读的预设（规划模式、只读的插件预设）。
/// 只读时写入和命令一律被拒绝，所以运行环境说明会告诉模型，请求里也不再带会改东西的工具。
pub fn read_only_turn(options: &Options) -> bool {
    crate::preset::preset_read_only(options)
}

/// 只读预设拒绝时给模型的说明。
pub const READ_ONLY_DENIED: &str = "Blocked: the session is in read-only mode, so files cannot be modified and commands cannot run. \
     Describe the change instead, or ask the user to switch the mode.";

/// 这一轮里用户最近说的话（首条消息，或中途插队的消息）。放在工具调用的 [`ToolContext::options`] 里，
/// 自动审查据此判断一个操作是不是用户要的。
#[derive(Debug, Clone)]
pub struct UserIntent(pub Arc<str>);

/// 按权限模式给每次工具调用下结论。有沙箱服务且它能限制写入时，命令不必询问。
/// 自动审查模式下，本来要问用户的调用先交给模型判断风险（[`AutoReviewer`]）。
pub struct PermissionPlugin;

/// 自动审查排在插件的 `reviewTool`（100）后面：插件给了结论就以插件的为准。
const AUTO_REVIEW_PRIORITY: i32 = 200;

impl Plugin for PermissionPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("permission").optional::<dyn Sandbox>()
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        let sandbox = cx.get::<dyn Sandbox>();
        cx.hook::<GateTool>(
            0,
            Arc::new(move |request: &GateRequest<'_>| {
                let confined = sandbox.as_ref().is_some_and(|sandbox| sandbox.confines_writes());
                Some(gate(PermissionMode::of(&request.ctx.options), request.access, request.ctx, confined))
            }),
        );
        cx.hook::<ReviewTool>(AUTO_REVIEW_PRIORITY, Arc::new(AutoReviewer));
        Ok(())
    }
}

/// 一次审查最多等多久；超时按“有风险”处理，交给用户决定。
const REVIEW_TIMEOUT: Duration = Duration::from_secs(45);
/// 交给审查模型的参数最多多少字符。
const REVIEW_INPUT_CHARS: usize = 6000;
/// 交给审查模型的用户原话最多多少字符。
const REVIEW_INTENT_CHARS: usize = 2000;

const REVIEW_PROMPT: &str = "You are the safety reviewer of a coding agent. The agent wants to run one tool call \
that the permission system could not approve on its own. Decide whether it can run without asking the user.

Answer \"allow\" only when the call is clearly low risk: routine development work that matches what the user \
asked for and whose effects are confined to the project or easy to undo. Examples: building, testing, linting \
or formatting the project; read-only commands (ls, cat, grep, git status/diff/log); creating or editing files \
the task needs; installing dependencies the project already declares; local git operations such as add, commit \
or creating a branch.

Answer \"ask\" when the call could cause harm or surprise the user. Examples: deleting or overwriting data \
outside the project, or deleting many files; recursive or forced removal (rm -rf, del /s); git push, force \
operations, rewriting history, resetting or discarding uncommitted work; publishing, deploying or releasing; \
changing system settings, shell profiles, global configuration or files in the home directory; using sudo or \
admin rights; reading or sending credentials, keys or tokens; uploading data or piping downloaded scripts into \
a shell; killing processes the agent did not start; destructive database operations; calls to external \
services that act on the user's behalf (sending messages, creating or deleting remote resources, payments); \
anything unrelated to the user's request; anything you cannot judge from the information given.

Reply with one JSON object and nothing else: {\"decision\": \"allow\" or \"ask\", \"risk\": \"low\", \"medium\" \
or \"high\", \"reason\": \"one short sentence\"}. Write the reason in the same language as the user's request.";

/// 自动审查：本来要问用户的调用先让当前会话的模型判断风险。低风险放行；有风险、判断不了或审查失败时
/// 仍然问用户，并把理由附在审批处。审查不会直接拒绝：拿不准的事交给用户。
pub struct AutoReviewer;

#[async_trait::async_trait]
impl ReviewToolHook for AutoReviewer {
    async fn review(&self, request: &GateRequest<'_>) -> Option<Gate> {
        if PermissionMode::of(&request.ctx.options) != PermissionMode::AutoReview {
            return None;
        }
        let llm = request.ctx.options.get::<LlmAccess>()?.clone();
        let cancel = request.ctx.options.get::<CallEnv>().map(|env| env.cancel.clone()).unwrap_or_default();
        let intent = request.ctx.options.get::<UserIntent>().map(|intent| intent.0.clone());
        let chat = ChatRequest {
            model: llm.model.clone(),
            system: Some(REVIEW_PROMPT.into()),
            messages: vec![Message { role: Role::User, content: vec![ContentBlock::text(review_message(request, intent.as_deref()))] }],
            tools: Vec::new(),
            // 有的模型即使不要求也会先输出思考，不限制输出长度。
            max_tokens: None,
            effort: None,
        };
        let work = async {
            let mut stream = llm.provider.stream(chat).await.ok()?;
            let mut accumulator = MessageAccumulator::default();
            while let Some(event) = stream.next().await {
                let event = event.ok()?;
                accumulator.push(&event).ok()?;
                if event == StreamEvent::MessageStop {
                    break;
                }
            }
            Some(collect_text(&accumulator.finish().content))
        };
        let reply = tokio::select! {
            biased;
            // 用户点了停止：没有结论，外面的循环会按中断处理。
            _ = cancel.cancelled() => return None,
            reply = tokio::time::timeout(REVIEW_TIMEOUT, work) => reply.ok().flatten(),
        };
        let verdict = reply.as_deref().and_then(parse_verdict);
        Some(match verdict {
            Some(Verdict { allow: true, .. }) => Gate::Allow,
            Some(Verdict { allow: false, reason }) => Gate::Confirm(reason),
            None => {
                tracing::warn!(tool = request.tool, "自动审查没有给出结论，改为询问用户");
                Gate::Confirm(REVIEW_FAILED.into())
            }
        })
    }
}

/// 审查失败（请求出错、超时、回答看不懂）时附在审批处的说明。
pub const REVIEW_FAILED: &str = "Automatic review could not decide; please confirm this action yourself.";

fn review_message(request: &GateRequest<'_>, intent: Option<&str>) -> String {
    let clip = |text: &str, limit: usize| {
        let mut clipped: String = text.chars().take(limit).collect();
        if clipped.len() < text.len() {
            clipped.push_str(" …(truncated)");
        }
        clipped
    };
    let access = match request.access {
        Access::Read => "read only".to_owned(),
        Access::ReadOutside(path) => format!("reads {} (OUTSIDE the workspace)", path.display()),
        Access::Write(path) => {
            let place = if request.ctx.is_inside_workspace(path) { "inside" } else { "OUTSIDE" };
            format!("writes {} ({place} the workspace)", path.display())
        }
        Access::Execute => "runs a shell command".to_owned(),
        Access::External => "hands the call to an external program or service".to_owned(),
    };
    let input = serde_json::to_string_pretty(request.input).unwrap_or_else(|_| request.input.to_string());
    format!(
        "Workspace: {}\nOperating system: {}\nUser's request: {}\n\nTool: {}\nEffect: {access}\nInput:\n{}",
        request.ctx.workspace.display(),
        std::env::consts::OS,
        intent.map_or_else(|| "(unknown)".to_owned(), |intent| clip(intent, REVIEW_INTENT_CHARS)),
        request.tool,
        clip(&input, REVIEW_INPUT_CHARS),
    )
}

#[derive(Debug, PartialEq)]
struct Verdict {
    allow: bool,
    reason: String,
}

/// 读审查模型的回答：先找 JSON 对象；模型没按格式回答时退回看有没有 `allow` / `ask` 这个词。
fn parse_verdict(reply: &str) -> Option<Verdict> {
    let object = reply
        .find('{')
        .zip(reply.rfind('}'))
        .filter(|(start, end)| start < end)
        .and_then(|(start, end)| serde_json::from_str::<serde_json::Value>(&reply[start..=end]).ok());
    if let Some(object) = object {
        let decision = object.get("decision").and_then(serde_json::Value::as_str)?.trim().to_ascii_lowercase();
        let reason = object.get("reason").and_then(serde_json::Value::as_str).unwrap_or_default().trim().to_owned();
        let risk = object.get("risk").and_then(serde_json::Value::as_str).unwrap_or("low").trim().to_ascii_lowercase();
        // 说放行、却又说风险高：按有风险处理。
        let allow = decision == "allow" && risk != "high";
        let reason = if reason.is_empty() { REVIEW_FAILED.to_owned() } else { reason };
        return Some(Verdict { allow, reason });
    }
    let lower = reply.to_ascii_lowercase();
    match (lower.contains("allow"), lower.contains("ask")) {
        (true, false) => Some(Verdict { allow: true, reason: String::new() }),
        (_, true) => Some(Verdict { allow: false, reason: REVIEW_FAILED.to_owned() }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn gates_by_mode_and_location() {
        let ctx = ToolContext::new(PathBuf::from("/ws"));
        let inside = Access::Write(ctx.resolve("src/a.rs"));
        let outside = Access::Write(ctx.resolve("../elsewhere/a.rs"));

        assert_eq!(gate(PermissionMode::Manual, &Access::Read, &ctx, false), Gate::Allow);
        assert_eq!(gate(PermissionMode::Manual, &inside, &ctx, false), Gate::Ask);
        assert_eq!(gate(PermissionMode::Manual, &Access::Execute, &ctx, true), Gate::Ask);
        assert_eq!(gate(PermissionMode::Manual, &Access::External, &ctx, true), Gate::Ask);

        assert_eq!(gate(PermissionMode::Workspace, &inside, &ctx, false), Gate::Allow);
        assert_eq!(gate(PermissionMode::Workspace, &outside, &ctx, false), Gate::Ask);
        assert_eq!(gate(PermissionMode::Workspace, &Access::Execute, &ctx, false), Gate::Ask);

        assert_eq!(gate(PermissionMode::AutoReview, &inside, &ctx, false), Gate::Allow);
        assert_eq!(gate(PermissionMode::AutoReview, &outside, &ctx, false), Gate::Ask);
        assert_eq!(gate(PermissionMode::AutoReview, &Access::Execute, &ctx, true), Gate::Ask);

        assert_eq!(gate(PermissionMode::FullAccess, &outside, &ctx, false), Gate::Allow);
        assert_eq!(gate(PermissionMode::FullAccess, &Access::Execute, &ctx, false), Gate::Allow);

        assert_eq!(gate(PermissionMode::Workspace, &Access::Execute, &ctx, true), Gate::Allow);
        assert_eq!(gate(PermissionMode::Workspace, &outside, &ctx, true), Gate::Ask);

        // 读工作区外的文件：只有完全访问模式直接放行；沙箱（confined）只管命令，不放行这个。
        let read_outside = Access::ReadOutside(PathBuf::from("/home/u/.ssh/id_rsa"));
        for mode in [PermissionMode::Manual, PermissionMode::Workspace, PermissionMode::AutoReview] {
            assert_eq!(gate(mode, &read_outside, &ctx, true), Gate::Ask);
        }
        assert_eq!(gate(PermissionMode::FullAccess, &read_outside, &ctx, false), Gate::Allow);
    }

    #[test]
    fn mode_ids_round_trip_and_read_only_becomes_manual() {
        for mode in [PermissionMode::Manual, PermissionMode::Workspace, PermissionMode::AutoReview, PermissionMode::FullAccess] {
            assert_eq!(PermissionMode::from_id(mode.id()), Some(mode));
        }
        assert_eq!(PermissionMode::from_id("read_only"), Some(PermissionMode::Manual));
        assert_eq!(PermissionMode::from_id("nope"), None);
    }

    #[test]
    fn parses_review_verdicts() {
        let allow = parse_verdict("```json\n{\"decision\": \"allow\", \"risk\": \"low\", \"reason\": \"runs tests\"}\n```");
        assert_eq!(allow, Some(Verdict { allow: true, reason: "runs tests".into() }));
        let ask = parse_verdict("{\"decision\":\"ask\",\"risk\":\"high\",\"reason\":\"删除工作区外的文件\"}");
        assert_eq!(ask, Some(Verdict { allow: false, reason: "删除工作区外的文件".into() }));
        // 自相矛盾：说放行又说高风险，按有风险处理。
        let odd = parse_verdict("{\"decision\":\"allow\",\"risk\":\"high\",\"reason\":\"x\"}");
        assert!(matches!(odd, Some(Verdict { allow: false, .. })));
        assert!(matches!(parse_verdict("I would ask the user."), Some(Verdict { allow: false, .. })));
        assert!(matches!(parse_verdict("allow"), Some(Verdict { allow: true, .. })));
        assert_eq!(parse_verdict("hmm"), None);
    }
}
