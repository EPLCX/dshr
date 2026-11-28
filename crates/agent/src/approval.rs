use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    /// 对应 `tool/call` 事件的 `call_id`，界面据此把审批挂到工具卡片上。
    pub call_id: String,
    pub tool: String,
    pub input: std::sync::Arc<Value>,
    /// 附在审批处的说明，如自动审查判断有风险的理由。
    pub note: Option<String>,
    /// 必须由用户亲自确认（[`crate::Gate::Require`]）：界面不提供“本会话不再询问”，
    /// 之前对同一个工具选过“不再询问”也不算数。
    pub mandatory: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

/// 审批入口：CLI 在终端询问，桌面版弹出审批卡片。
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, request: &ApprovalRequest) -> Decision;
}

/// 全部放行（对应 `--yes`）。
pub struct AutoApprove;

#[async_trait::async_trait]
impl Approver for AutoApprove {
    async fn approve(&self, _request: &ApprovalRequest) -> Decision {
        Decision::Allow
    }
}
