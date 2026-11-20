//! 沙箱插件：提供 [`Sandbox`] 服务，并在“工作区内修改”模式下把命令放进平台沙箱。

use std::sync::Arc;

use dshr_context::{Manifest, Plugin, Scope};
use dshr_sandbox::{Platform, Sandbox};
use dshr_tools::{ShellCommand, ToolContext, WrapCommand};

use crate::PermissionMode;

pub struct SandboxPlugin;

impl Plugin for SandboxPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("sandbox")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.provide::<dyn Sandbox>(Arc::new(Platform));
        cx.hook::<WrapCommand>(
            0,
            Arc::new(|command: &mut ShellCommand, ctx: &ToolContext| {
                // 只有“工作区内修改”需要沙箱：手动确认、自动审查的命令都经过把关，完全访问不加限制。
                if PermissionMode::of(&ctx.options) == PermissionMode::Workspace && Platform.confines_writes() {
                    let (program, args) = dshr_sandbox::shell_command(&command.script, &ctx.workspace, true);
                    command.program = program;
                    command.args = args;
                }
            }),
        );
        Ok(())
    }
}
