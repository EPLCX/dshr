//! 内置工具按功能分成的插件。

use std::sync::Arc;

use dshr_context::{Context, ContextError, Manifest, Plugin, PluginGroup, Scope};

use crate::{EditFile, Glob, Grep, ListDir, ReadFile, RunShell, Tools, UpdatePlan, WriteFile};

/// 读写文件与列目录。
pub struct FsPlugin;

impl Plugin for FsPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("fs")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<Tools>(Arc::new(ReadFile));
        cx.contribute::<Tools>(Arc::new(WriteFile));
        cx.contribute::<Tools>(Arc::new(EditFile));
        cx.contribute::<Tools>(Arc::new(ListDir));
        Ok(())
    }
}

/// `grep` 与 `glob`。
pub struct SearchPlugin;

impl Plugin for SearchPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("search")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<Tools>(Arc::new(Grep));
        cx.contribute::<Tools>(Arc::new(Glob));
        Ok(())
    }
}

/// 运行命令。
pub struct ShellPlugin;

impl Plugin for ShellPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("shell")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<Tools>(Arc::new(RunShell));
        Ok(())
    }
}

/// `update_plan`。
pub struct PlanPlugin;

impl Plugin for PlanPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("plan")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<Tools>(Arc::new(UpdatePlan));
        Ok(())
    }
}

/// 全部内置工具。
pub struct BuiltinTools;

impl PluginGroup for BuiltinTools {
    fn add_to(self, cx: &Context) -> Result<(), ContextError> {
        cx.add(FsPlugin)?;
        cx.add(SearchPlugin)?;
        cx.add(ShellPlugin)?;
        cx.add(PlanPlugin)
    }
}
