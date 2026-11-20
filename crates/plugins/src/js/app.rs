//! JS 插件里用到主程序服务（`crate::app`）的小工具。

use dshr_agent::LlmAccess;
use dshr_tools::ToolContext;

/// 用户亲手触发的场合（页面动作、命令、斜杠命令）没有会话：补上主程序给的默认模型，`host.llm` 才能用。
pub(super) fn user_context(mut ctx: ToolContext) -> ToolContext {
    if ctx.options.get::<LlmAccess>().is_none()
        && let Some(llm) = crate::app::app_services().llm.and_then(|source| source())
    {
        ctx.options.insert(llm);
    }
    ctx
}
