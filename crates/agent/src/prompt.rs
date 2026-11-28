//! 系统提示词的扩展：插件往后面追加内容（项目说明、Skills 目录等），
//! 以及两个内置插件：说明运行环境的 [`EnvironmentPlugin`]、读取 `AGENTS.md` 的 [`InstructionsPlugin`]。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dshr_context::{ExtensionPoint, Manifest, Options, Plugin, Scope};
use dshr_core::{EventBody, EventProjector};
use dshr_sandbox::Sandbox;
use serde_json::{Value, json};

use crate::{AgentError, BeforeStep, BeforeStepHook, EventKinds, PermissionMode, TurnContext, read_only_turn};

/// 扩展点：追加到系统提示词后面的内容。每轮开始时各问一次，一轮之内不变；
/// 只附加在请求上，不写进日志。
pub struct PromptParts;

impl ExtensionPoint for PromptParts {
    type Item = Arc<dyn PromptContributor>;
}

pub trait PromptContributor: Send + Sync {
    /// 这一轮要追加的内容；没有就返回 `None`。
    fn contribute(&self, cx: &PromptContext<'_>) -> Option<String>;
}

pub struct PromptContext<'a> {
    pub workspace: &'a Path,
    pub options: &'a Options,
    /// 这一轮的插件快照（插件包带的 Skills 从这里取）；测试里可以没有。
    pub plugins: Option<&'a dshr_context::Snapshot>,
}

/// 告诉模型它在什么环境里：工作区（是不是 git 仓库）、系统和 `run_shell` 用的 shell、日期、这一轮的权限模式。
/// 省得它先跑 `pwd`、`uname` 去试，或者在 Windows 上写出 `sh` 的语法；知道权限模式，它也就知道哪些操作要等用户审批。
///
/// 不变的部分（工作区、系统）放在系统提示词里；会变的部分（日期、权限模式）写成 [`TURN_ENVIRONMENT`] 事件
/// 追加进会话，只在和上一次不同时写一条。系统提示词是请求最前面的一段，它一变，整段对话的提示词缓存都失效
/// （用户切一次权限、过一次零点就要全价重算整个会话）；追加的事件只影响它后面的部分。
pub struct EnvironmentPlugin;

/// 日期和权限模式（[`EnvironmentPlugin`]）的事件类型。数据是 `{date, permission}`。
pub const TURN_ENVIRONMENT: &str = "environment/turn";

impl Plugin for EnvironmentPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("environment").optional::<dyn Sandbox>()
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        let sandbox = cx.get::<dyn Sandbox>();
        cx.contribute::<PromptParts>(Arc::new(Environment));
        // 排在上下文压缩（优先级 0）之后：压缩掉的旧事件里的说明，紧接着补一条。
        cx.hook::<BeforeStep>(10, Arc::new(TurnEnvironment { sandbox }));
        cx.contribute::<EventKinds>(Arc::new(TurnEnvironmentProjector));
        Ok(())
    }
}

/// 每一步请求之前：模型能看到的最近一条 [`TURN_ENVIRONMENT`] 和现在不一样（第一轮、换了权限模式、过了零点、
/// 旧的那条被压缩掉了）时追加一条。
struct TurnEnvironment {
    sandbox: Option<Arc<dyn Sandbox>>,
}

#[async_trait::async_trait]
impl BeforeStepHook for TurnEnvironment {
    async fn before_step(&self, turn: &mut TurnContext<'_>) -> Result<(), AgentError> {
        let confined = self.sandbox.as_ref().is_some_and(|sandbox| sandbox.confines_writes());
        let current = json!({
            "date": utc_date(SystemTime::now()),
            "permission": permission_line(&turn.config.options, confined),
        });
        if latest_visible(turn.session().events()) == Some(&current) {
            return Ok(());
        }
        turn.append_ext(TURN_ENVIRONMENT, current)
    }
}

/// 投影里还看得到的最近一条（在最近一次压缩之后）。
fn latest_visible(events: &[dshr_core::SessionEvent]) -> Option<&Value> {
    let compacted = events
        .iter()
        .rev()
        .find_map(|event| match event.body {
            EventBody::Compaction { upto_seq, .. } => Some(upto_seq),
            _ => None,
        })
        .unwrap_or(0);
    events.iter().rev().take_while(|event| event.seq > compacted).find_map(|event| match &event.body {
        EventBody::Ext { kind, data } if kind == TURN_ENVIRONMENT => Some(data),
        _ => None,
    })
}

struct TurnEnvironmentProjector;

impl EventProjector for TurnEnvironmentProjector {
    fn kind(&self) -> &str {
        TURN_ENVIRONMENT
    }

    fn project(&self, data: &Value) -> Option<String> {
        let date = data.get("date")?.as_str()?;
        let permission = data.get("permission")?.as_str()?;
        Some(format!("<environment>\n- Date (UTC): {date}\n- Permission mode: {permission}\n</environment>"))
    }
}

struct Environment;

impl PromptContributor for Environment {
    fn contribute(&self, cx: &PromptContext<'_>) -> Option<String> {
        let git = if in_git_repository(cx.workspace) { "a git repository" } else { "not a git repository" };
        Some(format!(
            "# Environment\n- Workspace: {} ({git})\n- System: {}\n- The current date and permission mode are in the latest \
             <environment> note in the conversation.",
            cx.workspace.display(),
            system_line(),
        ))
    }
}

fn system_line() -> String {
    match std::env::consts::OS {
        "windows" => "Windows; run_shell uses `cmd /C` by default, so use Windows command syntax. For PowerShell syntax (`$var`, pipelines) pass shell \"powershell\" instead of nesting `powershell -Command \"...\"` in cmd.".to_owned(),
        "macos" => "macOS; run_shell uses `sh -c`.".to_owned(),
        "linux" => "Linux; run_shell uses `sh -c`.".to_owned(),
        other => format!("{other}; run_shell uses `sh -c`."),
    }
}

/// 权限模式对模型意味着什么。规则和 [`crate::gate`] 一致。
fn permission_line(options: &Options, confined: bool) -> &'static str {
    if read_only_turn(options) {
        return "read-only — you can read and search, but files can't be changed and commands can't run. \
                If the task needs changes, describe them instead.";
    }
    match PermissionMode::of(options) {
        PermissionMode::FullAccess => "full access — nothing asks for approval, so take extra care with destructive commands.",
        PermissionMode::Manual => {
            "manual — reading and searching run directly; every edit, run_shell command and external call waits for \
             the user to approve it, so batch related changes and avoid needless calls."
        }
        PermissionMode::AutoReview => {
            "auto review — edits inside the workspace apply directly; run_shell commands, external calls and writes \
             outside the workspace are checked by a reviewer that runs low-risk ones directly and asks the user about \
             risky ones."
        }
        _ if confined => {
            "workspace — edits inside the workspace and run_shell commands (sandboxed to the workspace) run directly; \
             writes outside the workspace ask the user first."
        }
        _ => {
            "workspace — edits inside the workspace apply directly; run_shell commands and writes outside the workspace \
             ask the user first."
        }
    }
}

/// 工作区或它的上级目录里有 `.git`（目录，或者 worktree、子模块里的文件）。
fn in_git_repository(workspace: &Path) -> bool {
    workspace.ancestors().any(|dir| dir.join(".git").exists())
}

/// `YYYY-MM-DD`（UTC）。不为一个日期引入时间库：按公历从天数推算（Howard Hinnant 的 civil_from_days）。
fn utc_date(now: SystemTime) -> String {
    let days = now.duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs() / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// 单个说明文件最多读这么多字节，超出的部分截掉。
const MAX_INSTRUCTION_BYTES: usize = 32 * 1024;

/// 把 `AGENTS.md` 里的说明交给模型：数据目录下的对所有工作区生效，工作区根目录下的只对该工作区生效。
pub struct InstructionsPlugin {
    home: PathBuf,
}

impl InstructionsPlugin {
    /// `home`：dshr 的数据目录（`~/.dshr`）。
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }
}

impl Plugin for InstructionsPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("instructions")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<PromptParts>(Arc::new(Instructions { home: self.home.clone() }));
        Ok(())
    }
}

struct Instructions {
    home: PathBuf,
}

impl PromptContributor for Instructions {
    fn contribute(&self, cx: &PromptContext<'_>) -> Option<String> {
        let sections: Vec<String> = [(self.home.as_path(), "the user's global"), (cx.workspace, "this project's")]
            .into_iter()
            .filter_map(|(dir, whose)| {
                let path = dir.join("AGENTS.md");
                let text = read_clipped(&path)?;
                Some(format!("Instructions from {whose} AGENTS.md ({}):\n\n{text}", path.display()))
            })
            .collect();
        (!sections.is_empty()).then(|| sections.join("\n\n"))
    }
}

/// 读文本文件；不存在、不是 UTF-8 或内容为空时返回 `None`。
fn read_clipped(path: &Path) -> Option<String> {
    let mut text = std::fs::read_to_string(path).ok()?;
    if let Some((cut, _)) = text.char_indices().find(|(index, _)| *index >= MAX_INSTRUCTION_BYTES) {
        text.truncate(cut);
        text.push_str("\n… [truncated]");
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_global_then_project_instructions() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let instructions = Instructions { home: home.path().to_owned() };
        let options = Options::new();
        let cx = PromptContext { workspace: workspace.path(), options: &options, plugins: None };
        assert_eq!(instructions.contribute(&cx), None);

        std::fs::write(workspace.path().join("AGENTS.md"), "用 pnpm，不用 npm。\n").unwrap();
        let text = instructions.contribute(&cx).unwrap();
        assert!(text.starts_with("Instructions from this project's AGENTS.md"));
        assert!(text.ends_with("用 pnpm，不用 npm。"));

        std::fs::write(home.path().join("AGENTS.md"), "回答用中文。").unwrap();
        let text = instructions.contribute(&cx).unwrap();
        assert!(text.find("回答用中文。").unwrap() < text.find("用 pnpm").unwrap());
    }

    #[test]
    fn environment_names_workspace_and_shell() {
        let workspace = tempfile::tempdir().unwrap();
        let options = Options::new();
        let text = |cx: &PromptContext<'_>| Environment.contribute(cx).unwrap();
        let default = text(&PromptContext { workspace: workspace.path(), options: &options, plugins: None });
        assert!(default.starts_with("# Environment\n- Workspace: "));
        assert!(default.contains(&format!("{} (not a git repository)", workspace.path().display())));
        assert!(default.contains("run_shell uses"));
        // 会变的部分不在系统提示词里（它一变，整段对话的提示词缓存都失效）。
        assert!(!default.contains("Date (UTC)") && !default.contains("Permission mode:"));

        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        let nested = workspace.path().join("crates/app");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(in_git_repository(&nested));
        assert!(text(&PromptContext { workspace: workspace.path(), options: &options, plugins: None }).contains("(a git repository)"));
    }

    #[test]
    fn permission_lines_follow_mode_preset_and_sandbox() {
        use crate::{PluginPreset, Preset, PresetDef};

        let read_only = |options: Options| permission_line(&options, false).starts_with("read-only — ");
        assert!(!read_only(Options::new().with(PermissionMode::Manual)));
        assert!(permission_line(&Options::new().with(PermissionMode::Manual), false).starts_with("manual — "));
        assert!(permission_line(&Options::new().with(PermissionMode::AutoReview), false).starts_with("auto review — "));
        assert!(read_only(Options::new().with(PermissionMode::FullAccess).with(Preset::Plan)));
        let preset = PresetDef {
            plugin: "p".into(),
            id: "look".into(),
            label: "Look".into(),
            description: String::new(),
            instructions: String::new(),
            read_only: true,
            tools: None,
        };
        assert!(read_only(Options::new().with(PluginPreset(Arc::new(preset)))));
        assert!(!read_only(Options::new().with(Preset::Review)));
        assert!(permission_line(&Options::new().with(PermissionMode::FullAccess), false).starts_with("full access — nothing asks"));
        assert!(permission_line(&Options::new(), false).starts_with("workspace — edits inside the workspace apply directly"));
        assert!(permission_line(&Options::new(), true).contains("run_shell commands (sandboxed to the workspace) run directly"));

        let note = TurnEnvironmentProjector.project(&json!({"date": "2026-11-18", "permission": "full access"})).unwrap();
        assert_eq!(note, "<environment>\n- Date (UTC): 2026-11-18\n- Permission mode: full access\n</environment>");
    }

    #[test]
    fn utc_dates() {
        let at = |seconds: u64| utc_date(UNIX_EPOCH + std::time::Duration::from_secs(seconds));
        assert_eq!(at(0), "1970-01-01");
        assert_eq!(at(951_782_400), "2000-02-29");
        assert_eq!(at(951_868_800), "2000-03-01");
        assert_eq!(at(1_798_761_600 - 1), "2026-12-31");
        assert_eq!(at(1_798_761_600), "2027-01-01");
        assert_eq!(at(1_794_787_200 + 3_600), "2026-11-16");
    }

    #[test]
    fn long_files_are_clipped_on_a_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "字".repeat(MAX_INSTRUCTION_BYTES)).unwrap();
        let text = read_clipped(&path).unwrap();
        assert!(text.len() < MAX_INSTRUCTION_BYTES + 32);
        assert!(text.ends_with("[truncated]"));
    }
}
