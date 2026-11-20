//! `update_plan`：模型维护的步骤清单。计划本身就是最近一次调用的参数，界面据此显示计划面板。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolOutput, parse};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub title: String,
    pub status: StepStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
}

impl Plan {
    /// 从 `update_plan` 的参数解析计划。
    pub fn from_input(input: &Value) -> Option<Self> {
        serde_json::from_value(input.clone()).ok()
    }

    pub fn completed(&self) -> usize {
        self.steps.iter().filter(|step| step.status == StepStatus::Completed).count()
    }
}

pub const UPDATE_PLAN: &str = "update_plan";

pub struct UpdatePlan;

#[async_trait::async_trait]
impl Tool for UpdatePlan {
    fn name(&self) -> &str {
        UPDATE_PLAN
    }

    fn description(&self) -> &str {
        "Create or update the plan for a task with several distinct steps; skip it for small tasks. Send the whole \
         list each time, with at most one step `in_progress`. Update it when a step is done or the plan changes, in \
         the same reply as your next tool calls rather than as a separate step."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string", "description": "Short imperative step."},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["title", "status"]
                    }
                }
            },
            "required": ["steps"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolOutput {
        let plan: Plan = match parse(input) {
            Ok(plan) => plan,
            Err(error) => return error,
        };
        if plan.steps.is_empty() {
            return ToolOutput::error("计划至少需要一个步骤");
        }
        let running = plan.steps.iter().filter(|step| step.status == StepStatus::InProgress).count();
        if running > 1 {
            return ToolOutput::error("同一时间只能有一个步骤处于 in_progress");
        }
        ToolOutput::ok(format!("Plan updated: {}/{} steps completed.", plan.completed(), plan.steps.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn validates_and_summarizes() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let input = json!({"steps": [
            {"title": "读代码", "status": "completed"},
            {"title": "改代码", "status": "in_progress"},
            {"title": "跑测试", "status": "pending"}
        ]});
        let out = UpdatePlan.execute(input.clone(), &ctx).await;
        assert_eq!(out.content, "Plan updated: 1/3 steps completed.");
        assert_eq!(Plan::from_input(&input).unwrap().steps[1].status, StepStatus::InProgress);

        let two_running = json!({"steps": [
            {"title": "a", "status": "in_progress"},
            {"title": "b", "status": "in_progress"}
        ]});
        assert!(UpdatePlan.execute(two_running, &ctx).await.is_error);
        assert!(UpdatePlan.execute(json!({"steps": []}), &ctx).await.is_error);
    }
}
