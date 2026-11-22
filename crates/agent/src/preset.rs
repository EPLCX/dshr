//! Agent 预设（输入卡片的“标准模式”菜单）：在系统提示词后追加一段说明，规划模式另外限制为只读。

use std::sync::Arc;

use dshr_context::{Manifest, Options, Plugin, Scope};
use dshr_llm::ChatRequest;

use dshr_context::ExtensionPoint;

use crate::{AgentConfig, BeforeRequest, Gate, GateRequest, GateTool, READ_ONLY_DENIED};

/// 插件提供的预设（JS 插件的 `presets` 导出），出现在输入卡片的预设菜单里内置四项的后面。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetDef {
    pub plugin: String,
    pub id: String,
    pub label: String,
    pub description: String,
    /// 追加到系统提示词后的说明。
    pub instructions: String,
    /// 只读：拒绝一切写入与命令（和规划模式一样）。
    pub read_only: bool,
    /// 只让模型看到这些工具；`None` 是不限制。
    pub tools: Option<Vec<String>>,
}

/// 扩展点：插件提供的预设。
pub struct PluginPresets;

impl ExtensionPoint for PluginPresets {
    type Item = Arc<PresetDef>;
}

/// 本轮选中的插件预设，放在 [`AgentConfig::options`] 里；优先于 [`Preset`]（选了它时 `Preset` 是标准模式）。
#[derive(Debug, Clone)]
pub struct PluginPreset(pub Arc<PresetDef>);

/// 本轮的预设，放在 [`AgentConfig::options`] 里；没放时是标准模式。
/// 预设说明只在请求时附加到系统提示词后面，不写进日志——同一个会话里可以随时切换。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Preset {
    /// 直接动手完成任务。
    #[default]
    Standard,
    /// 只调查、出方案，不改文件、不运行命令。
    Plan,
    /// 审查现有改动，找问题而不是改代码。
    Review,
    /// 创造模式：按用户的描述写一个插件，加载、试用、改到能用。写法说明和 `plugin_*` 工具由
    /// 插件宿主（`dshr-plugins` 的 `CreatorPlugin`）在这个模式下提供。
    Create,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Standard, Preset::Plan, Preset::Review, Preset::Create];

    /// 主代理在这个预设下追加到系统提示词后的说明。标准模式不追加（基础提示词已经覆盖）；
    /// 创造模式的说明是插件宿主追加的写法指南（`creator.md`），这里不再重复。
    pub fn instructions(self) -> Option<&'static str> {
        match self {
            Preset::Standard | Preset::Create => None,
            Preset::Plan => Some(PLAN),
            Preset::Review => Some(REVIEW),
        }
    }

    /// 调用方给子代理明确指定了这个预设时（`host.agent.run({ preset })`）追加的说明。
    pub fn subagent_instructions(self) -> Option<&'static str> {
        match self {
            Preset::Standard | Preset::Create => None,
            Preset::Plan => Some(SUBAGENT_PLAN),
            Preset::Review => Some(SUBAGENT_REVIEW),
        }
    }

    /// 规划模式强制只读。
    pub fn read_only(self) -> bool {
        self == Preset::Plan
    }

    pub fn of(options: &Options) -> Self {
        options.get::<Self>().copied().unwrap_or_default()
    }
}

const PLAN: &str = r#"Planning mode (read-only): the user wants a plan, not changes yet. Tools that modify files or run commands are unavailable in this mode.
1. Investigate first: read the relevant code with read_file, grep, glob and list_dir (delegate can research a separate area in parallel). Base the plan on what the code actually does.
2. If a requirement is unclear in a way that changes the plan, ask — or state your assumption and continue.
3. Reply with the plan:
   - Goal and approach in a few sentences; say why this approach when the choice isn't obvious.
   - The changes, step by step: files and functions, what changes, in an order that can be implemented and checked.
   - Risks, open questions, and how to verify the result (tests or commands to run).
Be concrete (paths, names) and as short as the task allows. Don't write out the full code; small snippets only where they make a step clear. The user switches modes when they want it implemented."#;

const REVIEW: &str = r#"Review mode: look for problems in the code rather than writing new code.
- Scope: what the user names (files, a commit, a branch); otherwise the uncommitted changes. Get them with one run_shell call such as `git status --short && git diff HEAD` (it may need approval), and read new untracked files with read_file. If there is nothing to review, say so.
- Read enough surrounding code (callers, edge cases) to confirm each problem is real. A few verified findings beat many guesses.
- Look for, in this order: wrong behavior and crashes, security issues, data loss, races and leaks, performance, then confusing code. Skip style nits unless asked.
- Report findings most severe first: `path:line`, what is wrong, a concrete input or situation that triggers it, and a suggested fix. If nothing significant turns up, say so plainly.
- Don't edit files unless the user asks you to fix something."#;

const SUBAGENT_PLAN: &str = "Planning mode (read-only): investigate what the task concerns and reply with a concrete plan — \
the changes step by step (files, functions), risks, and how to verify. Don't write out the full code.";

const SUBAGENT_REVIEW: &str = "Review mode: review the code or changes the task names (the uncommitted changes if it names \
none). Confirm each problem by reading the surrounding code, and report findings most severe first: `path:line`, what is \
wrong, how it fails, a fix. Don't edit files.";

/// 子代理从只读的父代理那里继承了只读限制、调用方又没指定预设时追加的一行。
const SUBAGENT_READ_ONLY: &str = "Read-only: you can't modify files or run commands for this task. Investigate with the read \
and search tools and report what you found.";

/// 子代理的标记，由 [`crate::SubagentRunner`] 放进子代理的 [`AgentConfig::options`]。
///
/// 子代理的 options 是从父代理复制来的，里面有父代理的预设。那些预设的说明是写给主代理的（“审查当前改动”
/// “给用户写计划”），子代理照着做就跑偏了，所以继承来的预设只保留限制（只读、插件预设的工具名单），不附加说明；
/// 调用方明确指定了预设（`explicit_preset`）时附加它的子代理版说明（[`Preset::subagent_instructions`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subagent {
    pub explicit_preset: bool,
}

/// 选中的预设要求只读：规划模式，或者只读的插件预设。不含权限模式（那个由权限插件管）。
pub(crate) fn preset_read_only(options: &Options) -> bool {
    match options.get::<PluginPreset>() {
        Some(preset) => preset.0.read_only,
        None => Preset::of(options).read_only(),
    }
}

/// 本轮要追加到系统提示词后的预设说明。
fn instructions(options: &Options) -> Option<String> {
    if let Some(subagent) = options.get::<Subagent>() {
        let text = if subagent.explicit_preset {
            Preset::of(options).subagent_instructions()
        } else {
            crate::read_only_turn(options).then_some(SUBAGENT_READ_ONLY)
        };
        return text.map(str::to_owned);
    }
    match options.get::<PluginPreset>() {
        Some(preset) => Some(preset.0.instructions.clone()).filter(|text| !text.trim().is_empty()),
        None => Preset::of(options).instructions().map(str::to_owned),
    }
}

/// 把本轮预设的说明追加到请求的系统提示词后面；规划模式下拒绝一切写入与命令，不管权限模式是什么。
pub struct PresetsPlugin;

impl Plugin for PresetsPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("presets")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.hook::<BeforeRequest>(
            0,
            Arc::new(|request: &mut ChatRequest, config: &AgentConfig, _: &dshr_tools::ToolContext| {
                if let Some(extra) = instructions(&config.options) {
                    request.system = Some(match request.system.take() {
                        Some(system) => format!("{system}\n\n{extra}"),
                        None => extra,
                    });
                }
                // 插件预设限定了工具：模型只看到这些（子代理也一样）。
                if let Some(tools) = config.options.get::<PluginPreset>().and_then(|preset| preset.0.tools.as_ref()) {
                    request.tools.retain(|tool| tools.contains(&tool.name));
                }
            }),
        );
        // 排在权限插件前面。
        cx.hook::<GateTool>(
            -100,
            Arc::new(|request: &GateRequest<'_>| {
                let plugin = request.ctx.options.get::<PluginPreset>();
                let read_only = preset_read_only(&request.ctx.options);
                // 限定了工具的插件预设：名单外的工具（模型硬调的）拒绝。
                if let Some(tools) = plugin.and_then(|preset| preset.0.tools.as_ref())
                    && !tools.iter().any(|name| name == request.tool)
                {
                    return Some(Gate::Deny(format!("The tool {} is not available in this preset.", request.tool)));
                }
                // 只读的一轮照样可以读工作区外的东西（权限模式再决定问不问）。
                let blocked = read_only && !request.access.is_read();
                blocked.then(|| Gate::Deny(READ_ONLY_DENIED.into()))
            }),
        );
        Ok(())
    }
}
