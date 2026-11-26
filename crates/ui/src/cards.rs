//! 内置插件的界面那一半：文件改动卡片、计划面板、输入卡片上的菜单、账号状态。
//! 主程序用 [`builtin_ui_plugins`] 取整个列表，和后端的插件列表一样是显式装配的。

use std::rc::Rc;

use dshr_tools::{Plan, StepStatus, Tool as _, UPDATE_PLAN};
use gpui_kit::component::{Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use serde_json::Value;

use crate::design::{Palette, Tone, Typography as _, card, hint, motion, palette, pressable, radius, readable_with_id, size, space};
use crate::plugin::{
    ComposerControl, ComposerPanel, PanelContext, RowData, StatusItem, StatusLine, ToolCard, UiPlugin, UiScope, ViewState,
};
use crate::settings::SettingsUi;
use crate::transcript::{ChangeSummary, DiffLineKind, ToolRow, parse_change_report};

/// 桌面版自带的界面插件。顺序决定设置页、面板、状态行的先后。
pub fn builtin_ui_plugins() -> Vec<Box<dyn UiPlugin>> {
    vec![Box::new(SettingsUi), Box::new(AccountUi), Box::new(FsUi), Box::new(PlanUi), Box::new(PresetsUi), Box::new(PermissionUi)]
}

/// `fs` 的界面：`edit_file` / `write_file` 的卡片在标题里显示改动行数，展开后是按行着色的 diff。
pub struct FsUi;

impl UiPlugin for FsUi {
    fn id(&self) -> &'static str {
        "fs"
    }

    fn apply_ui(&self, cx: &mut UiScope<'_>) {
        cx.tool_card(dshr_tools::EditFile.name(), ChangeCard);
        cx.tool_card(dshr_tools::WriteFile.name(), ChangeCard);
    }
}

/// 输出是文件改动报告（首行 `Edited path (+a -r)`，后面是统一 diff）的工具。
struct ChangeCard;

fn change_summary(tool: &ToolRow) -> Option<&ChangeSummary> {
    tool.data.as_ref()?.downcast_ref()
}

impl ToolCard for ChangeCard {
    /// 报告在结果到达时解析一次；调用失败或输出不是报告时按普通输出显示。
    fn on_result(&self, output: &str, is_error: bool) -> Option<RowData> {
        if is_error {
            return None;
        }
        let summary: RowData = Rc::new(parse_change_report(output)?.to_owned());
        Some(summary)
    }

    fn render_summary(&self, tool: &ToolRow, p: &Palette) -> Option<AnyElement> {
        let report = change_summary(tool)?;
        Some(
            h_flex()
                .gap(px(space::XS))
                .text_caption()
                .child(div().text_color(p.success).child(format!("+{}", report.added)))
                .child(div().text_color(p.danger).child(format!("−{}", report.removed)))
                .into_any_element(),
        )
    }

    /// 文件改动按行着色：新增绿底、删除红底、分块标题次要色。连续同类的行是一段、画成一个元素（见 [`ChangeSummary`]）。
    fn render_output(&self, tool: &ToolRow, cx: &mut App) -> Option<AnyElement> {
        let report = change_summary(tool)?;
        let p = palette(cx);
        let lines = report.blocks.iter().enumerate().map(|(block_index, (kind, text))| {
            let row = div().px(px(space::M)).whitespace_nowrap().child(readable_with_id(block_index, text.clone()));
            match kind {
                DiffLineKind::Added => row.bg(p.success.opacity(0.12)),
                DiffLineKind::Removed => row.bg(p.danger.opacity(0.12)),
                DiffLineKind::Hunk => row.text_color(p.text_secondary),
                DiffLineKind::Context => row,
            }
        });
        Some(div().py(px(space::XS)).children(lines).into_any_element())
    }
}

/// `plan` 的界面：`update_plan` 不显示成工具卡片，最近一次的计划显示在输入卡片上方的计划面板里。
pub struct PlanUi;

impl UiPlugin for PlanUi {
    fn id(&self) -> &'static str {
        "plan"
    }

    fn apply_ui(&self, cx: &mut UiScope<'_>) {
        cx.tool_card(UPDATE_PLAN, PlanCard);
        cx.composer_panel(PlanPanel);
    }
}

struct PlanCard;

impl ToolCard for PlanCard {
    fn visible(&self) -> bool {
        false
    }

    /// 计划就是最近一次调用的参数；参数不对的调用不动现有的计划。
    fn on_call(&self, input: &Value, state: &mut ViewState) {
        if let Some(plan) = Plan::from_input(input) {
            state.insert(plan);
        }
    }
}

/// 计划面板：最近一次 `update_plan` 的步骤；可以折叠成只显示进度和当前步骤。
struct PlanPanel;

impl ComposerPanel for PlanPanel {
    fn id(&self) -> &'static str {
        "plan"
    }

    fn render(&self, state: &ViewState, panel: &PanelContext, cx: &mut App) -> Option<AnyElement> {
        let plan = state.get::<Plan>()?;
        let p = palette(cx);
        let expanded = panel.expanded;
        let toggle = panel.toggle.clone();
        let current = plan.steps.iter().find(|step| step.status == StepStatus::InProgress);
        let header = pressable("plan-header", &p)
            .accessibility_label(t!("conversation.plan").to_string())
            .aria_expanded(expanded)
            .flex()
            .items_center()
            .gap(px(space::S))
            .rounded(px(radius::SM))
            .cursor_pointer()
            .on_click(move |event, window, cx| toggle(event, window, cx))
            .child(div().text_label().child(t!("conversation.plan").to_string()))
            .child(hint(t!("conversation.plan_progress", done = plan.completed(), total = plan.steps.len()).to_string(), Tone::Neutral, &p))
            .when(!expanded, |header| {
                header.children(
                    current.map(|step| {
                        div().flex_1().min_w(px(0.)).truncate().text_body().text_color(p.text_secondary).child(step.title.clone())
                    }),
                )
            })
            .child(div().flex_1())
            .child(Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronUp }).size(px(size::ICON_SM)));

        let steps = plan.steps.iter().enumerate().map(|(step_index, step)| {
            // 步骤状态只用图标表示，读屏软件另外读到文字状态。
            let status = match step.status {
                StepStatus::Completed => t!("conversation.step_completed"),
                StepStatus::InProgress => t!("conversation.step_in_progress"),
                StepStatus::Pending => t!("conversation.step_pending"),
            };
            let marker: AnyElement = match step.status {
                StepStatus::Completed => Icon::new(IconName::Check).size(px(size::ICON_SM)).text_color(p.success).into_any_element(),
                // 进行中的一步：这一轮还在运行时转圈（减少动态效果时是静止的图标，读屏软件另外读到状态）。
                // 停下来以后（被中断、出错，或模型没把最后一步标完成，或重新打开这样的会话）画一个静止的实心点：
                // 转圈是循环动画，一直播会让整个对话区每秒重画几十次。
                StepStatus::InProgress if panel.running => {
                    motion::spinner(("plan-step-spinner", step_index), size::ICON_SM, p.accent).into_any_element()
                }
                StepStatus::InProgress => div().size(px(10.)).m(px(2.)).rounded_full().bg(p.accent).into_any_element(),
                StepStatus::Pending => {
                    div().size(px(10.)).m(px(2.)).rounded_full().border_1().border_color(p.text_secondary).into_any_element()
                }
            };
            h_flex().gap(px(space::S)).child(div().flex_none().w(px(size::ICON_SM)).flex().justify_center().child(marker)).child(
                div()
                    .id(("plan-step", step_index))
                    .role(Role::Label)
                    .aria_value(format!("{}（{status}）", step.title))
                    .text_body()
                    .when(step.status == StepStatus::Completed, |text| text.text_color(p.text_secondary).line_through())
                    .when(step.status == StepStatus::InProgress, |text| text.font_weight(FontWeight::MEDIUM))
                    .child(step.title.clone()),
            )
        });

        Some(
            card(&p)
                .px(px(space::L))
                .py(px(space::M))
                .child(v_flex().gap(px(space::S)).child(header).when(expanded, |panel| panel.children(steps)))
                .into_any_element(),
        )
    }
}

/// `presets` 的界面：输入卡片上的预设菜单。停用预设插件后菜单不显示，每一轮都按标准模式进行。
pub struct PresetsUi;

impl UiPlugin for PresetsUi {
    fn id(&self) -> &'static str {
        "presets"
    }

    fn apply_ui(&self, cx: &mut UiScope<'_>) {
        cx.composer_control(ComposerControl::PresetMenu);
    }
}

/// `permission` 的界面：输入卡片上的权限模式菜单。停用权限插件后每次工具调用都询问，选模式没有意义。
pub struct PermissionUi;

impl UiPlugin for PermissionUi {
    fn id(&self) -> &'static str {
        "permission"
    }

    fn apply_ui(&self, cx: &mut UiScope<'_>) {
        cx.composer_control(ComposerControl::PermissionMenu);
    }
}

/// 侧边栏底部的账号状态：有没有可用的提供商。只有界面，没有后端那一半。
pub struct AccountUi;

impl UiPlugin for AccountUi {
    fn id(&self) -> &'static str {
        "account"
    }

    fn apply_ui(&self, cx: &mut UiScope<'_>) {
        cx.status_item(StatusItem {
            id: "account",
            // 一切正常（已配好可用的提供商）时不占一行：只在演示模式、还没配置时提醒。
            read: Rc::new(|backend| {
                if backend.is_demo() {
                    Some(StatusLine { label: t!("sidebar.account_demo").to_string(), ok: backend.has_ready_provider() })
                } else if backend.has_ready_provider() {
                    None
                } else {
                    Some(StatusLine { label: t!("sidebar.account_missing").to_string(), ok: false })
                }
            }),
        });
    }
}
