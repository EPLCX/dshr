//! 设置弹窗。有哪些页由界面插件登记（[`SettingsPage`]）；自带四页：通用（语言、主题、快捷键）、
//! 辅助功能（高对比度、文字大小、高可见性轮廓、减少动态效果）、
//! 模型（提供商、模型目录、新会话默认值）、扩展（说明与 Skills、MCP 服务器、插件开关、配置与删除）。
//! 最后固定一页“关于”（见 `about`）。
//! 插件的配置项不是逐个手写的界面，而是按插件声明的 schema 生成（见 `schema_form`）。

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use dshr_config::{
    Config, HighContrastPreference, Keybindings, McpServerEntry, ModelRef, MotionPreference, ProviderConfig, ReasoningEffort,
    TextScalePreference, ThemePreference,
};
use dshr_context::Status;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::design::ControlSize as _;

use crate::announce::announce;
use crate::backend::Backend;
use crate::conversation::{EFFORT_CHOICES, effort_label, permission_label, preset_id, preset_label};
use crate::design::TEXT_SCALES;
use crate::design::motion::Edge;
use crate::design::{
    Tone, Typography as _, card, divider, field, heading, hint, in_section, mono_font, motion, nav_item, palette, panel, radius,
    readable_with_id, section_header, setting_list, setting_row, shadow, size, space, status_dot,
};
use crate::dropdown::{DropdownChanged, DropdownState};
use crate::permissions;
use crate::plugin::{PageContent, SettingsPage, UiPlugin, UiRegistry, UiScope};
use crate::provider_form::{FormEvent, ProviderForm};
use crate::reveal::reveal;
use crate::schema_form::{self, Field, FieldError, FieldKind};
use crate::{
    EDITABLE_SHORTCUTS, FIXED_SHORTCUTS, LANGUAGES, apply_language, apply_motion, apply_theme, bind_keys, current_keystroke,
    display_keystroke, keystroke_string,
};
use dshr_agent::{PermissionMode, Preset};

/// 设置弹窗自带的三页。只有界面，没有后端那一半。
pub struct SettingsUi;

impl UiPlugin for SettingsUi {
    fn id(&self) -> &'static str {
        "settings"
    }

    fn apply_ui(&self, cx: &mut UiScope<'_>) {
        cx.settings_page(SettingsPage {
            id: "general",
            icon: IconName::Settings.into(),
            label: Rc::new(|| t!("settings.general").to_string()),
            content: PageContent::Builtin(SettingsView::render_general),
        });
        cx.settings_page(SettingsPage {
            id: "accessibility",
            icon: crate::design::AppIcon::UniversalAccess.into(),
            label: Rc::new(|| t!("settings.accessibility").to_string()),
            content: PageContent::Builtin(SettingsView::render_accessibility),
        });
        cx.settings_page(SettingsPage {
            id: "models",
            icon: IconName::Cpu.into(),
            label: Rc::new(|| t!("settings.models").to_string()),
            content: PageContent::Builtin(SettingsView::render_models),
        });
        cx.settings_page(SettingsPage {
            id: "extensions",
            icon: IconName::SquareTerminal.into(),
            label: Rc::new(|| t!("settings.extensions").to_string()),
            content: PageContent::Builtin(SettingsView::render_extensions),
        });
    }
}

/// “扩展”页里的一个插件。
struct PluginRow {
    id: &'static str,
    /// 配置文件里是否启用（重启后生效）。
    enabled: bool,
    /// 这次运行里它没能激活的原因（激活失败、缺依赖）；正常或被停用时没有。
    problem: Option<String>,
    /// JS 插件清单里声明了、用户还没允许过的能力；不为空时显示“允许”按钮。
    grant: Vec<String>,
    /// 热重载时改过的新版本没能加载、仍在用旧版本的原因。
    kept: Option<String>,
    /// 连续失败、这次运行里被停下的原因。
    stopped: Option<String>,
    /// 按它声明的 schema 生成的配置项。
    fields: Vec<FieldState>,
    /// 最近一次保存它的配置的结果：(文字, 是否出错)。
    message: Option<(String, bool)>,
    /// JS 插件清单里声明的密钥（`secrets`）：值存在系统钥匙串里。
    secrets: Vec<SecretState>,
    /// JS 插件的卡片信息（名字、版本、声明的能力、提供的东西、服务状态）；内置插件没有。
    card: Option<dshr_plugins::PluginCard>,
    /// 卡片的“详情”展开着。
    expanded: bool,
    /// 用户刚拨过开关、还没在后台生效的目标状态。开关按它画，生效前不被刷新改回去。
    want: Option<bool>,
    /// 后台正在启停它（见 [`SettingsView::toggle_plugin`]）。
    busy: bool,
    /// 可以回滚到的旧版本（归档里最新的一个）。刷新时在后台读好，绘制时不再读磁盘。
    rollback_to: Option<String>,
    /// 后台正在授权（[`SettingsView::grant_plugin`]）：做完之前再点“允许”不重复做。
    granting: bool,
}

impl PluginRow {
    /// 开关现在该画成什么样：拨过还没生效的按用户拨的算。
    fn shown_enabled(&self) -> bool {
        self.want.unwrap_or(self.enabled)
    }
}

struct SecretState {
    name: String,
    /// 已经设置过（钥匙串里或环境变量里有值）。
    set: bool,
    /// 新值的输入框（掩码显示）；回车或“保存”写进钥匙串，空着保存是删除。
    input: Entity<InputState>,
}

struct FieldState {
    field: Field,
    /// 配置文件里写的值；没写时是 `None`，显示默认值。
    value: Option<serde_json::Value>,
    /// 数字、文本项的输入框；开关和选项不需要。
    input: Option<Entity<InputState>>,
    /// 几选一的项：下拉框。
    choice: Option<Entity<DropdownState>>,
}

pub enum SettingsEvent {
    /// 配置已保存，需要刷新依赖它的界面。
    Changed,
    /// 关闭弹窗。
    Close,
}

/// 提供商列表里的一行。
struct ProviderCard {
    config: ProviderConfig,
    name: String,
    ready: bool,
}

/// 装上插件后，等文件监视引起的那次重载落定再对一次插件列表（见 `refresh_after_install`）。
const INSTALL_SETTLE: std::time::Duration = std::time::Duration::from_millis(600);

/// “安装插件”从哪里装。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallSource {
    /// 安装包文件：`.dshrplugin`、`.zip`、`.tar.gz`。
    Package,
    /// 插件文件夹。
    Folder,
}

pub struct SettingsView {
    backend: Arc<Backend>,
    /// 界面插件登记的页，按登记顺序；`page` 是当前页的下标。
    pages: Vec<SettingsPage>,
    page: usize,
    /// 插件自己的页面视图，第一次切过去时建。
    page_views: HashMap<&'static str, AnyView>,
    language: Entity<DropdownState>,
    providers: Vec<ProviderCard>,
    /// “添加提供商”按钮和各提供商“编辑”按钮的焦点：表单收起后焦点回到打开它的按钮上，不会丢。
    add_provider_focus: FocusHandle,
    edit_provider_focus: HashMap<String, FocusHandle>,
    form: Entity<ProviderForm>,
    /// 提供商表单打开在哪里：`None` 收起，`Some(None)` 在列表末尾添加，`Some(Some(id))` 在这个提供商下面编辑。
    form_open: Option<Option<String>>,
    default_model: Entity<DropdownState>,
    /// 常规页的主题、减少动态效果，模型页的默认思考等级：选项类设置都是下拉框。
    theme_select: Entity<DropdownState>,
    /// 配色方案：第一项是默认配色，其余是 `Backend::themes()`。
    palette_select: Entity<DropdownState>,
    palette_keys: Vec<String>,
    /// 下拉框里的配色方案（顺序同 `palette_keys[1..]`）。打开设置页、插件变化时读一次：
    /// `Backend::themes()` 要读 `themes/` 目录、逐个解析文件，不能在每次重画时调用。
    palette_themes: Vec<dshr_agent::ThemeDef>,
    palette_error: Option<String>,
    motion_select: Entity<DropdownState>,
    /// 辅助功能页的高对比度、文字大小。
    high_contrast_select: Entity<DropdownState>,
    text_scale_select: Entity<DropdownState>,
    effort_select: Entity<DropdownState>,
    /// 新会话默认的权限模式、预设（选项按 [`PERMISSION_CHOICES`]、`Preset::ALL` 的顺序）。
    permission_select: Entity<DropdownState>,
    preset_select: Entity<DropdownState>,
    default_choices: Vec<ModelRef>,
    /// 新会话默认的思考等级（保存前的草稿）。
    effort_default: Option<ReasoningEffort>,
    defaults_message: Option<(String, bool)>,
    /// 用户改过的快捷键。打开设置时读一次文件，之后随保存更新；渲染时不再读盘。
    bindings: Keybindings,
    /// 正在录制新按键的快捷键动作。
    recording: Option<&'static str>,
    /// 最近一次保存快捷键失败的原因。
    shortcut_error: Option<String>,
    /// “扩展”页：主程序装配的插件、是否启用（来自配置文件，重启后生效）和它们的配置项。
    plugin_rows: Vec<PluginRow>,
    mcp_rows: Vec<McpServerEntry>,
    /// MCP 服务器的连接状态（和 `mcp_rows` 按名字对应）。
    mcp_status: Vec<dshr_mcp::ServerStatus>,
    /// 展开了日志的 MCP 服务器。
    mcp_open_log: Option<String>,
    /// “添加服务器”表单是否展开。
    mcp_form_open: bool,
    /// 弹窗卡片的进场进度（由工作区按弹窗的状态设）。
    phase: motion::Phase,
    /// “添加服务器”按钮的焦点：表单收起、服务器被移除后焦点回到这里。
    mcp_add_focus: FocusHandle,
    mcp_name: Entity<InputState>,
    mcp_command: Entity<InputState>,
    /// 最近一次保存扩展设置失败的原因。
    extension_error: Option<String>,
    /// 后台正在收集“扩展”页的数据（[`SettingsView::refresh_extensions`]）。
    extensions_refreshing: bool,
    /// 收集期间又有人要求刷新：这一次做完再做一次。
    extensions_refresh_again: bool,
    /// JS 插件的 id：这些插件的行有“日志”按钮。
    script_ids: Vec<&'static str>,
    /// 展开了日志的插件。
    open_log: Option<&'static str>,
    /// 点过一次“删除”、等待确认的插件。
    pending_delete: Option<&'static str>,
    /// “新建 JS 插件”的 id 输入框，和上一次生成的结果。
    new_plugin: Entity<InputState>,
    /// 插件索引：搜索框、结果、结果上方的一句话。
    browse_query: Entity<InputState>,
    browse_results: Vec<dshr_plugins::index::IndexEntry>,
    browse_message: Option<(String, bool)>,
    new_plugin_message: Option<(String, bool)>,
    /// “关于”页打开第三方声明失败的原因。
    pub(crate) about_error: Option<String>,
    /// 正文的滚动：键盘焦点走到视口外时跟着滚（见 `reveal`）。
    body_scroll: ScrollHandle,
    /// 弹窗自身的焦点：打开时聚焦它，Tab 被限制在弹窗内，Esc 关闭。
    focus_handle: FocusHandle,
    _form_subscription: Subscription,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl SettingsView {
    pub fn new(backend: Arc<Backend>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config = backend.file_config();
        let names: Vec<SharedString> = LANGUAGES.iter().map(|(_, name)| SharedString::from(*name)).collect();
        let current = LANGUAGES.iter().position(|(code, _)| *code == config.ui.language).unwrap_or(0);
        let language = cx.new(|cx| DropdownState::new(names, Some(current), cx).width(200.));
        let default_model = cx.new(|cx| DropdownState::new(Vec::new(), None, cx).width(280.));
        let theme_select =
            cx.new(|cx| DropdownState::new(theme_labels(), THEMES.iter().position(|theme| *theme == config.ui.theme), cx).width(200.));
        let themes = backend.themes();
        let palette_keys: Vec<String> = std::iter::once(String::new()).chain(themes.iter().map(Backend::theme_key)).collect();
        let palette_labels: Vec<SharedString> = std::iter::once(t!("settings.palette_default").to_string().into())
            .chain(themes.iter().map(|theme| palette_theme_name(theme).into()))
            .collect();
        let palette_index = palette_keys.iter().position(|key| *key == config.ui.palette).or(Some(0));
        let palette_select = cx.new(|cx| DropdownState::new(palette_labels, palette_index, cx).width(200.));
        let motion_select =
            cx.new(|cx| DropdownState::new(motion_labels(), MOTIONS.iter().position(|motion| *motion == config.ui.motion), cx).width(200.));
        let high_contrast_select = cx.new(|cx| {
            let index = HIGH_CONTRASTS.iter().position(|choice| *choice == config.ui.high_contrast);
            DropdownState::new(high_contrast_labels(), index, cx).width(200.)
        });
        let text_scale_select = cx.new(|cx| {
            let index = TEXT_SCALE_CHOICES.iter().position(|choice| *choice == config.ui.text_scale).or(Some(0));
            DropdownState::new(text_scale_labels(), index, cx).width(200.)
        });
        let effort_select = cx.new(|cx| {
            let labels = EFFORT_CHOICES.into_iter().map(|level| effort_label(level).into()).collect();
            DropdownState::new(labels, EFFORT_CHOICES.iter().position(|level| *level == config.model.effort), cx).width(200.)
        });
        let permission_select = cx.new(|cx| {
            let current = crate::conversation::default_permission(&config);
            DropdownState::new(permission_labels(), PERMISSION_CHOICES.iter().position(|mode| *mode == current), cx).width(200.)
        });
        let preset_select = cx.new(|cx| {
            let current = crate::conversation::default_preset(&config);
            DropdownState::new(preset_labels(), Preset::ALL.iter().position(|preset| *preset == current), cx).width(200.)
        });

        let form = cx.new(|cx| ProviderForm::add(backend.clone(), window, cx));
        let form_subscription = Self::subscribe_form(&form, window, cx);

        let mut subscriptions =
            vec![cx.subscribe_in(&language, window, |this, _, DropdownChanged(index): &DropdownChanged, window, cx| {
                let Some((code, _)) = LANGUAGES.get(*index) else {
                    return;
                };
                if this.backend.update_config(|config| config.ui.language = (*code).to_owned()).is_ok() {
                    apply_language(code);
                    // 文案随语言变化的输入框占位文字需要重建。
                    this.reset_form(window, cx);
                    cx.emit(SettingsEvent::Changed);
                    window.refresh();
                }
            })];

        subscriptions.push(cx.subscribe_in(&theme_select, window, |this, _, DropdownChanged(index): &DropdownChanged, window, cx| {
            if let Some(theme) = THEMES.get(*index) {
                this.set_theme(*theme, window, cx);
            }
        }));
        subscriptions.push(cx.subscribe_in(&palette_select, window, |this, _, DropdownChanged(index): &DropdownChanged, window, cx| {
            let Some(key) = this.palette_keys.get(*index).cloned() else { return };
            if this.backend.update_config(|config| config.ui.palette = key).is_ok() {
                this.palette_error = crate::refresh_palette(&this.backend, window, cx).err();
                window.refresh();
                cx.notify();
            }
        }));
        subscriptions.push(cx.subscribe(&motion_select, |this, _, DropdownChanged(index): &DropdownChanged, cx| {
            if let Some(motion) = MOTIONS.get(*index) {
                this.set_motion(*motion, cx);
            }
        }));
        subscriptions.push(cx.subscribe_in(
            &high_contrast_select,
            window,
            |this, _, DropdownChanged(index): &DropdownChanged, window, cx| {
                let Some(choice) = HIGH_CONTRASTS.get(*index).copied() else { return };
                if this.backend.update_config(|config| config.ui.high_contrast = choice).is_ok() {
                    this.palette_error = crate::refresh_palette(&this.backend, window, cx).err();
                    cx.refresh_windows();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe(&text_scale_select, |this, _, DropdownChanged(index): &DropdownChanged, cx| {
            if let Some(scale) = TEXT_SCALE_CHOICES.get(*index) {
                this.set_text_scale(*scale, cx);
            }
        }));
        subscriptions.push(cx.subscribe(&effort_select, |this, _, DropdownChanged(index): &DropdownChanged, cx| {
            if let Some(level) = EFFORT_CHOICES.get(*index) {
                this.effort_default = *level;
                this.save_defaults(cx);
            }
        }));
        subscriptions.push(cx.subscribe(&default_model, |this, _, _: &DropdownChanged, cx| this.save_defaults(cx)));
        subscriptions.push(cx.subscribe(&permission_select, |this, _, _: &DropdownChanged, cx| this.save_defaults(cx)));
        subscriptions.push(cx.subscribe(&preset_select, |this, _, _: &DropdownChanged, cx| this.save_defaults(cx)));
        let plugin_rows = Self::build_plugin_rows(&backend, &mut subscriptions, window, cx);
        let backend_scripts = backend.script_ids();
        let new_plugin = cx.new(|cx| InputState::new(window, cx).placeholder("my-plugin"));
        let browse_query = cx.new(|cx| InputState::new(window, cx));
        subscriptions.push(cx.subscribe_in(&new_plugin, window, |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.create_plugin(window, cx);
            }
        }));
        let mut view = Self {
            bindings: backend.keybindings(),
            pages: settings_pages(&backend, cx),
            page: 0,
            page_views: HashMap::new(),
            backend,
            language,
            providers: Vec::new(),
            add_provider_focus: cx.focus_handle(),
            edit_provider_focus: HashMap::new(),
            form,
            form_open: None,
            default_model,
            theme_select,
            palette_select,
            palette_keys,
            palette_themes: themes,
            palette_error: None,
            motion_select,
            high_contrast_select,
            text_scale_select,
            effort_select,
            permission_select,
            preset_select,
            default_choices: Vec::new(),
            effort_default: config.model.effort,
            defaults_message: None,
            recording: None,
            shortcut_error: None,
            plugin_rows,
            mcp_rows: Vec::new(),
            mcp_status: Vec::new(),
            mcp_open_log: None,
            mcp_form_open: false,
            phase: motion::Phase::SETTLED,
            mcp_add_focus: cx.focus_handle(),
            mcp_name: cx.new(|cx| InputState::new(window, cx)),
            mcp_command: cx.new(|cx| InputState::new(window, cx)),
            extension_error: None,
            extensions_refreshing: false,
            extensions_refresh_again: false,
            about_error: None,
            script_ids: backend_scripts,
            open_log: None,
            pending_delete: None,
            new_plugin,
            browse_query,
            browse_results: Vec::new(),
            browse_message: None,
            new_plugin_message: None,
            body_scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _form_subscription: form_subscription,
            _subscriptions: subscriptions,
        };
        view.refresh_providers(window, cx);
        view.refresh_extensions(cx);
        view.poll_mcp_status(cx);
        view.follow_plugin_changes(window, cx);
        // 还没有提供商时直接打开“添加”表单。
        if view.providers.is_empty() {
            view.form_open = Some(None);
        }
        view
    }

    /// “扩展”页的插件列表：每个插件声明的配置 schema 变成一组配置项，数字和文本项各带一个输入框。
    /// 插件和它们的 schema 在运行期间不变，所以只在打开弹窗时建一次；值由 [`Self::refresh_extensions`] 填。
    fn build_plugin_rows(
        backend: &Arc<Backend>,
        subscriptions: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<PluginRow> {
        let config = backend.file_config();
        let notes = PluginNotes::read(backend);
        // 全部卡片一次取完（等授权的清单只读一遍）；不对每个插件各读一遍插件目录。
        let mut cards = backend.plugin_cards();
        let mut rows = Vec::new();
        for (row_index, plugin) in backend.plugins().into_iter().enumerate() {
            let card = cards.remove(plugin.id);
            let (problem, grant, kept, stopped) = notes.of(plugin.id, &plugin.status);
            let fields = plugin.config.as_deref().map(schema_form::fields).unwrap_or_default();
            let fields = fields
                .into_iter()
                .enumerate()
                .map(|(field_index, field)| {
                    let value = config.plugin_value(plugin.id, &field.key);
                    let input = matches!(field.kind, FieldKind::Integer | FieldKind::Number | FieldKind::Text).then(|| {
                        let text = field.display(value.as_ref());
                        let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
                        // 回车保存这一项。
                        subscriptions.push(cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, _, cx| {
                            if matches!(event, InputEvent::PressEnter { .. }) {
                                this.submit_plugin_field(row_index, field_index, cx);
                            }
                        }));
                        input
                    });
                    let choice = match &field.kind {
                        FieldKind::Choice(choices) => {
                            let current = field.display(value.as_ref());
                            let locale = dshr_plugins::locale::current_locale();
                            let labels: Vec<SharedString> = field
                                .choice_labels(&locale, |key| card.as_ref().and_then(|card| card.message(&locale, key)))
                                .into_iter()
                                .map(SharedString::from)
                                .collect();
                            debug_assert_eq!(labels.len(), choices.len());
                            let selected = choices.iter().position(|choice| *choice == current);
                            let select = cx.new(|cx| DropdownState::new(labels, selected, cx).width(200.));
                            subscriptions.push(cx.subscribe(&select, move |this, _, DropdownChanged(index): &DropdownChanged, cx| {
                                let field = &this.plugin_rows[row_index].fields[field_index].field;
                                let FieldKind::Choice(choices) = &field.kind else { return };
                                if let Some(choice) = choices.get(*index) {
                                    let value = field.normalize(serde_json::Value::String(choice.clone()));
                                    this.save_plugin_field(row_index, field_index, value, cx);
                                }
                            }));
                            Some(select)
                        }
                        _ => None,
                    };
                    FieldState { field, value, input, choice }
                })
                .collect();
            let secrets = card
                .as_ref()
                .map(|card| card.secrets.clone())
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(secret_index, (name, set))| {
                    let input = cx.new(|cx| InputState::new(window, cx).masked(true));
                    subscriptions.push(cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, window, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            this.save_plugin_secret(row_index, secret_index, window, cx);
                        }
                    }));
                    SecretState { name, set, input }
                })
                .collect();
            rows.push(PluginRow {
                id: plugin.id,
                enabled: config.plugin_enabled(plugin.id),
                problem,
                grant,
                kept,
                stopped,
                fields,
                message: None,
                secrets,
                card,
                expanded: false,
                want: None,
                busy: false,
                // 归档的旧版本在后台刷新时读（见 `refresh_extensions`），打开弹窗时不等它。
                rollback_to: None,
                granting: false,
            });
        }
        rows
    }

    /// 重新读取配置文件里的插件开关、配置项的值与 MCP 服务器，以及插件现在的状态（授权、热重载之后会变）。
    ///
    /// 配置文件里的值在内存里，马上更新；要读插件目录、授权记录、归档、钥匙串的部分在后台收集
    /// （[`Backend::extensions_snapshot`]），收好了再在界面线程上赋值。插件多时界面不会卡住。
    /// 收集期间又要求刷新的（连续发布的快照、连续拨开关）合并成做完之后再做一次。
    fn refresh_extensions(&mut self, cx: &mut Context<Self>) {
        self.refresh_extension_config();
        if self.extensions_refreshing {
            self.extensions_refresh_again = true;
            return;
        }
        self.extensions_refreshing = true;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let data = cx.background_executor().spawn(async move { backend.extensions_snapshot() }).await;
            this.update(cx, |this, cx| {
                this.extensions_refreshing = false;
                this.apply_extensions(data);
                cx.notify();
                if std::mem::take(&mut this.extensions_refresh_again) {
                    this.refresh_extensions(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// 只看内存里的配置：插件开关、配置项的值、MCP 服务器列表。很便宜，界面线程上直接做。
    fn refresh_extension_config(&mut self) {
        let config = self.backend.file_config();
        for row in &mut self.plugin_rows {
            row.enabled = config.plugin_enabled(row.id);
            for state in &mut row.fields {
                state.value = config.plugin_value(row.id, &state.field.key);
            }
        }
        self.mcp_rows = config.mcp_servers();
    }

    /// 把后台收集好的数据填进各行（[`Self::refresh_extensions`]）。
    fn apply_extensions(&mut self, mut data: crate::backend::ExtensionsData) {
        let notes = PluginNotes { grants: data.grants, kept: data.kept, stopped: data.stopped };
        for row in &mut self.plugin_rows {
            row.enabled = data.config.plugin_enabled(row.id);
            let status =
                data.plugins.iter().find(|plugin| plugin.id == row.id).map(|plugin| plugin.status.clone()).unwrap_or(Status::Disabled);
            (row.problem, row.grant, row.kept, row.stopped) = notes.of(row.id, &status);
            row.card = data.cards.remove(row.id);
            row.rollback_to = data.rollback.remove(row.id);
            if let Some(card) = &row.card {
                for secret in &mut row.secrets {
                    if let Some((_, set)) = card.secrets.iter().find(|(name, _)| *name == secret.name) {
                        secret.set = *set;
                    }
                }
            }
            for state in &mut row.fields {
                state.value = data.config.plugin_value(row.id, &state.field.key);
            }
        }
        self.mcp_rows = data.config.mcp_servers();
        self.mcp_status = data.mcp_status;
    }

    /// 插件容器发布新快照（热重载、装上新插件、开关、授权）时刷新插件列表：新装的插件马上出现，不用重新打开设置。
    fn follow_plugin_changes(&self, window: &mut Window, cx: &mut Context<Self>) {
        // 插件在运行时藏起、显示设置页（`host.ui.hide("settings:<id>")`）：重新算导航，停在原来那一页上。
        let mut entry_changes = dshr_agent::entry_changes();
        cx.spawn_in(window, async move |this, cx| {
            while entry_changes.changed().await.is_ok() {
                let updated = this.update(cx, |this, cx| {
                    let current = this.pages.get(this.page).map(|page| page.id);
                    this.pages = settings_pages(&this.backend, cx);
                    this.page = current.and_then(|id| this.pages.iter().position(|page| page.id == id)).unwrap_or(0);
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
        let mut changes = self.backend.plugin_changes();
        cx.spawn_in(window, async move |this, cx| {
            while changes.changed().await.is_ok() {
                // 一次开关、热重载会在每个 `Context` 里各发布一两次快照：稍等一下，合并成一次刷新。
                cx.background_executor().timer(std::time::Duration::from_millis(80)).await;
                let _ = changes.borrow_and_update();
                let updated = this.update_in(cx, |this, window, cx| {
                    this.refresh_palettes();
                    this.sync_plugin_rows(false, window, cx);
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// 让插件列表跟上插件容器：插件增减了（或者 `rebuild`）就重建列表（每个插件的配置输入框跟着重建），再刷新各行的状态。
    ///
    /// 哪些是 JS 插件（`script_ids`）也要跟着换：它决定一行显示在“JS 插件”下面；只在打开设置时读一次的话，
    /// 新装的插件有了行也显示不出来，一直是“暂无 JS 插件”，要关掉设置再打开才看得到。
    /// 重新读一次可选的配色方案（插件装上、卸下时，插件带的方案跟着变）。
    fn refresh_palettes(&mut self) {
        let themes = self.backend.themes();
        self.palette_keys = std::iter::once(String::new()).chain(themes.iter().map(Backend::theme_key)).collect();
        self.palette_themes = themes;
    }

    fn sync_plugin_rows(&mut self, rebuild: bool, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<&str> = self.backend.plugins().iter().map(|plugin| plugin.id).collect();
        let known: Vec<&str> = self.plugin_rows.iter().map(|row| row.id).collect();
        let scripts = self.backend.script_ids();
        let scripts_changed = scripts != self.script_ids;
        self.script_ids = scripts;
        if rebuild || scripts_changed || ids != known {
            // 插件增减了：重建列表（每个插件的配置输入框跟着重建）。
            let mut subscriptions = Vec::new();
            self.plugin_rows = Self::build_plugin_rows(&self.backend, &mut subscriptions, window, cx);
            self._subscriptions.extend(subscriptions);
            // 行重建了，等确认的“删除”跟着作废。
            self.pending_delete = None;
        }
        self.refresh_extensions(cx);
        cx.notify();
    }

    /// 装上、更新、回滚插件之后刷新扩展页：新插件马上出现，新版本的配置项、权限跟着换。
    ///
    /// 不只靠 [`Self::follow_plugin_changes`] 的通知：同一个 id 的更新、回滚不会让插件增减，通知也可能和别的变化
    /// 合并掉。文件监视引起的那次重载会稍晚一点才到，过一会儿再对一次。`rebuild` 时无论插件增减都重建列表。
    fn refresh_after_install(&mut self, rebuild: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_plugin_rows(rebuild, window, cx);
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(INSTALL_SETTLE).await;
            this.update_in(cx, |this, window, cx| this.sync_plugin_rows(false, window, cx)).ok();
        })
        .detach();
    }

    /// 选一个插件安装包（`.dshrplugin`、`.zip`、`.tar.gz`）或插件文件夹来安装：在后台暂存（包签了名就验签）、
    /// 弹出程序内的确认框（来源、签名、权限和风险），同意后装上。
    ///
    /// 安装包和文件夹分开选：Windows 的系统选择框只能二选一（同时允许时只能选文件夹，选不中安装包）。
    fn install_plugin(&mut self, kind: InstallSource, window: &mut Window, cx: &mut Context<Self>) {
        let (files, prompt) = match kind {
            InstallSource::Package => (true, t!("settings.plugin_install_package_prompt").to_string()),
            InstallSource::Folder => (false, t!("settings.plugin_install_folder").to_string()),
        };
        let picked =
            cx.prompt_for_paths(gpui::PathPromptOptions { files, directories: !files, multiple: false, prompt: Some(prompt.into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = picked.await else { return };
            let Some(path) = paths.pop() else { return };
            this.update_in(cx, |this, window, cx| this.install_plugin_from(path, window, cx)).ok();
        })
        .detach();
    }

    /// 从选好（或拖进来）的路径安装：文件按安装包装，文件夹按插件文件夹装。
    fn install_plugin_from(&mut self, path: std::path::PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if path.is_file() && !dshr_plugins::install::is_package_name(&path.to_string_lossy()) {
            let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            let text = t!("settings.plugin_not_package", name = name).to_string();
            announce(text.clone(), cx);
            self.new_plugin_message = Some((text, true));
            cx.notify();
            return;
        }
        let backend = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_executor().spawn(async move { backend.install_plugin(&path) }).await;
            this.update_in(cx, |this, window, cx| {
                // 覆盖安装时同一个 id 换了新版本：配置项、权限可能变了，整个列表重建。
                let replaced = result.as_ref().is_ok_and(|id| this.plugin_rows.iter().any(|row| row.id == id.as_str()));
                this.refresh_after_install(replaced, window, cx);
                this.new_plugin_message = Some(match result {
                    Ok(id) => (t!("settings.plugin_installed", id = id).to_string(), false),
                    Err(error) => (error, true),
                });
                if let Some((text, _)) = &this.new_plugin_message {
                    announce(text.clone(), cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 在后台跑一件插件管理的事（更新、回滚：要联网、要等确认），结果写在这个插件下面。
    fn run_plugin_job(
        &mut self,
        id: &'static str,
        job: impl FnOnce(&Backend) -> Result<String, String> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let backend = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_executor().spawn(async move { job(&backend) }).await;
            this.update_in(cx, |this, window, cx| {
                // 换了版本：先重建列表（新版本的配置项、权限），再把结果写到重建后的这一行上。
                this.refresh_after_install(result.is_ok(), window, cx);
                if let Some(row) = this.plugin_rows.iter().position(|row| row.id == id) {
                    this.plugin_rows[row].message = Some(match result {
                        Ok(text) => (text, false),
                        Err(error) => (error, true),
                    });
                    this.announce_plugin_message(row, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 拨插件的开关。开关马上滑过去；写配置、JS 插件就地启停（要执行脚本、看插件文件）在后台做，
    /// 不卡界面。后台还在做时又拨了：做完这一次再按最后拨到的状态做一次。失败时开关退回去，原因写在上面。
    fn toggle_plugin(&mut self, id: &'static str, checked: bool, cx: &mut Context<Self>) {
        let Some(row) = self.plugin_rows.iter_mut().find(|row| row.id == id) else { return };
        row.want = Some(checked);
        self.extension_error = None;
        cx.notify();
        if !row.busy {
            self.apply_plugin_toggle(id, cx);
        }
    }

    fn apply_plugin_toggle(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(row) = self.plugin_rows.iter_mut().find(|row| row.id == id) else { return };
        let Some(checked) = row.want else { return };
        row.busy = true;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            // JS 插件就地启停（同时写文件）；内置插件只写文件，重启生效。
            let result = cx.background_executor().spawn(async move { backend.set_plugin_enabled(id, checked) }).await;
            this.update(cx, |this, cx| {
                this.refresh_extensions(cx);
                let Some(row) = this.plugin_rows.iter_mut().find(|row| row.id == id) else { return };
                row.busy = false;
                match result {
                    Ok(()) if row.want != Some(checked) => {
                        // 做的这段时间里又拨了：按最后的状态再做一次。
                        this.apply_plugin_toggle(id, cx);
                    }
                    Ok(()) => row.want = None,
                    Err(error) => {
                        row.want = None;
                        announce(error.clone(), cx);
                        this.extension_error = Some(error);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn update_plugin(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.run_plugin_job(
            id,
            move |backend| backend.install_from_index(id).map(|_| t!("settings.plugin_updated").to_string()),
            window,
            cx,
        );
    }

    fn rollback_plugin(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.run_plugin_job(
            id,
            move |backend| backend.rollback_plugin(id).map(|version| t!("settings.plugin_rolled_back", version = version).to_string()),
            window,
            cx,
        );
    }

    /// 删除（卸载）JS 插件：第一次点变成“确认删除”，再点才删。删掉插件文件夹和它的授权，
    /// 热重载之后这一行跟着消失。
    fn delete_plugin(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if self.pending_delete != Some(id) {
            self.pending_delete = Some(id);
            cx.notify();
            return;
        }
        self.pending_delete = None;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { backend.uninstall_plugin(id) }).await;
            this.update(cx, |this, cx| {
                this.refresh_extensions(cx);
                if let Err(error) = result {
                    if let Some(row) = this.plugin_rows.iter().position(|row| row.id == id) {
                        this.plugin_rows[row].message = Some((error, true));
                        this.announce_plugin_message(row, cx);
                    } else {
                        announce(error, cx);
                    }
                } else {
                    let text = t!("settings.plugin_deleted", id = id).to_string();
                    announce(text.clone(), cx);
                    this.new_plugin_message = Some((text, false));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 在插件索引里搜索（后台联网）。
    fn browse(&mut self, cx: &mut Context<Self>) {
        let query = self.browse_query.read(cx).value().trim().to_owned();
        let backend = self.backend.clone();
        self.browse_message = Some((t!("settings.plugin_browse_searching").to_string(), false));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { backend.browse_plugins(&query) }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(entries) => {
                        this.browse_message = entries.is_empty().then(|| (t!("settings.plugin_browse_empty").to_string(), false));
                        this.browse_results = entries;
                    }
                    Err(error) => {
                        this.browse_results.clear();
                        this.browse_message = Some((error, true));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn install_from_index(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_executor().spawn(async move { backend.install_from_index(&id) }).await;
            this.update_in(cx, |this, window, cx| {
                let replaced = result.as_ref().is_ok_and(|id| this.plugin_rows.iter().any(|row| row.id == id.as_str()));
                this.refresh_after_install(replaced, window, cx);
                this.browse_message = Some(match result {
                    Ok(id) => (t!("settings.plugin_installed", id = id).to_string(), false),
                    Err(error) => (error, true),
                });
                if let Some((text, _)) = &this.browse_message {
                    announce(text.clone(), cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 插件索引：搜索框和结果（配置了 `[plugin_index]` 才显示）。
    fn render_browse(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.backend.plugin_index_urls().is_empty() {
            return None;
        }
        let p = palette(cx);
        let rows = self.browse_results.iter().enumerate().map(|(index, entry)| {
            let version = entry.best_version().map(|version| version.version.clone()).unwrap_or_default();
            let signed = entry.publisher.as_ref().is_some_and(|publisher| publisher.key.is_some());
            let id = entry.id.clone();
            h_flex()
                .gap(px(space::M))
                .py(px(space::XS))
                .child(div().flex_none().text_label().child(format!("{} {version}", entry.id)))
                .child(div().flex_1().min_w(px(0.)).truncate().text_caption().text_color(p.text_secondary).child(format!(
                    "{}{}",
                    entry.description.clone().or_else(|| entry.name.clone()).unwrap_or_default(),
                    if signed { t!("settings.plugin_browse_signed").to_string() } else { String::new() }
                )))
                .child(crate::design::framed(
                    ("browse-install", index),
                    Button::new(("browse-install", index))
                        .control_xs()
                        .label(t!("settings.plugin_browse_install").to_string())
                        .accessibility_label(t!("settings.plugin_browse_install_named", id = entry.id).to_string())
                        .on_click(cx.listener(move |this, _, window, cx| this.install_from_index(id.clone(), window, cx))),
                ))
        });
        Some(
            field(
                t!("settings.plugin_browse").to_string(),
                h_flex()
                    .items_center()
                    .gap(px(space::S))
                    .child(crate::design::framed_handle(
                        gpui_kit::Focusable::focus_handle(self.browse_query.read(cx), cx),
                        Input::new(&self.browse_query)
                            .w(px(200.))
                            .aria_label(in_section(t!("settings.scripts"), t!("settings.plugin_browse"))),
                    ))
                    .child(crate::design::framed(
                        "browse-search",
                        Button::new("browse-search")
                            .label(t!("settings.plugin_browse_search").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.browse(cx))),
                    )),
            )
            .children(self.browse_message.clone().map(|(text, error)| hint(text, if error { Tone::Danger } else { Tone::Neutral }, &p)))
            .child(v_flex().children(rows))
            .into_any_element(),
        )
    }

    /// 取消链接开发中的插件：要重载插件，在后台做。
    fn unlink_plugin(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { backend.unlink_plugin(id) }).await;
            this.update(cx, |this, cx| {
                this.extension_error = result.err();
                if let Some(error) = &this.extension_error {
                    announce(error.clone(), cx);
                }
                this.refresh_extensions(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 设置弹窗开着时，每 2 秒看一次 MCP 服务器的状态（连接、重连都在后台进行），变了就重画。
    fn poll_mcp_status(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(2)).await;
                let Ok(()) = this.update(cx, |this, cx| {
                    let status = this.backend.mcp_status();
                    if status != this.mcp_status {
                        this.mcp_status = status;
                        cx.notify();
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();
    }

    /// 一个 MCP 服务器状态的一句话和语气。
    fn mcp_state_text(&self, name: &str) -> Option<(String, Tone)> {
        let status = self.mcp_status.iter().find(|status| status.name == name)?;
        use dshr_mcp::ServerState;
        Some(match status.state {
            ServerState::Disabled => return None,
            ServerState::Idle => (t!("settings.mcp_state_idle").to_string(), Tone::Neutral),
            ServerState::Connecting => (t!("settings.mcp_state_connecting").to_string(), Tone::Neutral),
            ServerState::Ready => {
                let mut text = t!("settings.mcp_state_ready", tools = status.tools).to_string();
                if status.resources {
                    text.push_str(t!("settings.mcp_state_resources").as_ref());
                }
                if status.prompts > 0 {
                    text.push_str(t!("settings.mcp_state_prompts", count = status.prompts).as_ref());
                }
                (text, Tone::Success)
            }
            ServerState::Failed => (
                t!("settings.mcp_state_failed", error = status.error.clone().unwrap_or_default(), seconds = status.retry_in.unwrap_or(0))
                    .to_string(),
                Tone::Danger,
            ),
            ServerState::Stopped => {
                (t!("settings.mcp_state_stopped", error = status.error.clone().unwrap_or_default()).to_string(), Tone::Danger)
            }
            ServerState::NeedsLogin => (t!("settings.mcp_state_needs_login").to_string(), Tone::Danger),
        })
    }

    /// 登录 / 退出登录一个远程 MCP 服务器（登录要等用户在浏览器里完成，在后台进行）。
    fn mcp_sign_in(&mut self, name: String, sign_out: bool, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { if sign_out { backend.mcp_logout(&name) } else { backend.mcp_login(&name) } })
                .await;
            this.update(cx, |this, cx| {
                this.extension_error = result.err();
                if let Some(error) = &this.extension_error {
                    announce(error.clone(), cx);
                }
                this.mcp_status = this.backend.mcp_status();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn reconnect_mcp(&mut self, name: String, cx: &mut Context<Self>) {
        self.extension_error = self.backend.mcp_reconnect(&name).err();
        self.mcp_status = self.backend.mcp_status();
        cx.notify();
    }

    /// 按输入框里的 id 生成一个新的 JS 插件，并打开它的文件夹。
    fn create_plugin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.new_plugin.read(cx).value().trim().to_owned();
        if id.is_empty() {
            return;
        }
        self.new_plugin_message = Some(match self.backend.create_plugin(&id) {
            Ok(dir) => {
                self.new_plugin.update(cx, |input, cx| input.set_value("", window, cx));
                cx.open_with_system(&dir);
                (t!("settings.plugin_created", path = dir.display().to_string()).to_string(), false)
            }
            Err(error) => (error, true),
        });
        if let Some((text, _)) = &self.new_plugin_message {
            announce(text.clone(), cx);
        }
        cx.notify();
    }

    fn open_plugins_dir(&mut self, cx: &mut Context<Self>) {
        let dir = self.backend.plugins_dir();
        let _ = std::fs::create_dir_all(&dir);
        cx.open_with_system(&dir);
    }

    /// 允许插件声明的能力；结果显示在这个插件下面。
    ///
    /// 授权之后要重载这个插件（执行它的脚本），在后台做；做完之前再点不重复授权。
    fn grant_plugin(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(plugin) = self.plugin_rows.get_mut(row) else {
            return;
        };
        if plugin.granting {
            return;
        }
        plugin.granting = true;
        let id = plugin.id;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { backend.grant_plugin(id) }).await;
            this.update(cx, |this, cx| {
                this.refresh_extensions(cx);
                let Some(row) = this.plugin_rows.iter().position(|row| row.id == id) else { return };
                this.plugin_rows[row].granting = false;
                this.plugin_rows[row].message = Some(match result {
                    Ok(()) => (t!("settings.plugin_granted").to_string(), false),
                    Err(error) => (error, true),
                });
                this.announce_plugin_message(row, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 把输入框里的值存成插件的密钥（空着是删除），结果显示在这个插件下面。输入框随即清空。
    fn save_plugin_secret(&mut self, row: usize, secret: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, name, input)) = self.plugin_rows.get(row).and_then(|plugin| {
            let state = plugin.secrets.get(secret)?;
            Some((plugin.id, state.name.clone(), state.input.clone()))
        }) else {
            return;
        };
        let value = input.read(cx).value().to_string();
        input.update(cx, |input, cx| input.set_value("", window, cx));
        // 写系统钥匙串可能要等好一会儿（有的系统会弹出确认），在后台做。
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let (result, set, value, name) = cx
                .background_executor()
                .spawn(async move {
                    let result = backend.set_plugin_secret(id, &name, &value);
                    let set = backend.plugin_secrets(id).into_iter().any(|(other, set)| other == name && set);
                    (result, set, value, name)
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(row) = this.plugin_rows.iter().position(|row| row.id == id) else { return };
                if let Some(state) = this.plugin_rows[row].secrets.get_mut(secret) {
                    state.set = set;
                }
                this.plugin_rows[row].message = Some(match result {
                    Ok(()) if value.is_empty() => (t!("settings.plugin_secret_deleted", name = name).to_string(), false),
                    Ok(()) => (t!("settings.plugin_secret_saved", name = name).to_string(), false),
                    Err(error) => (error, true),
                });
                this.announce_plugin_message(row, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn has_plugin(&self, id: &str) -> bool {
        self.plugin_rows.iter().any(|row| row.id == id)
    }

    /// 把一个配置项写进 `[plugins.<id>]`（`None` 是去掉这一项，回到默认值），结果显示在这个插件下面。
    fn save_plugin_field(&mut self, row: usize, field: usize, value: Option<serde_json::Value>, cx: &mut Context<Self>) {
        let Some((id, key)) = self.plugin_rows.get(row).and_then(|plugin| Some((plugin.id, plugin.fields.get(field)?.field.key.clone())))
        else {
            return;
        };
        let result = self.backend.update_config(|config| config.set_plugin_value(id, &key, value));
        self.refresh_extensions(cx);
        self.plugin_rows[row].message = Some(match result {
            Ok(()) => (t!("settings.plugin_saved").to_string(), false),
            Err(error) => (error, true),
        });
        self.announce_plugin_message(row, cx);
        cx.notify();
    }

    fn announce_plugin_message(&self, row: usize, cx: &mut Context<Self>) {
        if let Some((text, _)) = self.plugin_rows.get(row).and_then(|plugin| plugin.message.clone()) {
            announce(text, cx);
        }
    }

    /// 保存输入框里的值；不合规时只提示，不写文件。
    fn submit_plugin_field(&mut self, row: usize, field: usize, cx: &mut Context<Self>) {
        let Some(state) = self.plugin_rows.get(row).and_then(|plugin| plugin.fields.get(field)) else {
            return;
        };
        let Some(input) = &state.input else {
            return;
        };
        match state.field.parse(&input.read(cx).value()) {
            Ok(value) => self.save_plugin_field(row, field, value, cx),
            Err(error) => {
                self.plugin_rows[row].message = Some((field_error_text(&error), true));
                self.announce_plugin_message(row, cx);
                cx.notify();
            }
        }
    }

    /// 修改扩展相关的配置并写回文件。
    fn update_extensions(&mut self, edit: impl FnOnce(&mut Config), cx: &mut Context<Self>) {
        self.extension_error = self.backend.update_config(edit).err();
        if let Some(error) = &self.extension_error {
            announce(error.clone(), cx);
        }
        self.refresh_extensions(cx);
        cx.notify();
    }

    /// 展开“添加服务器”表单，焦点放到第一个输入框。
    fn open_mcp_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mcp_form_open = true;
        self.extension_error = None;
        self.mcp_name.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// 收起表单并清空输入，焦点回到“添加服务器”按钮。
    fn close_mcp_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mcp_form_open = false;
        self.extension_error = None;
        self.mcp_name = cx.new(|cx| InputState::new(window, cx));
        self.mcp_command = cx.new(|cx| InputState::new(window, cx));
        self.mcp_add_focus.focus(window, cx);
        cx.notify();
    }

    fn add_mcp_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.mcp_name.read(cx).value().trim().to_owned();
        let line = self.mcp_command.read(cx).value().trim().to_owned();
        let mut parts = line.split_whitespace().map(str::to_owned);
        let Some(command) = parts.next().filter(|_| !name.is_empty()) else {
            let error = t!("settings.mcp_invalid").to_string();
            announce(error.clone(), cx);
            self.extension_error = Some(error);
            // 缺哪一项就回到哪一项。
            let missing = if name.is_empty() { &self.mcp_name } else { &self.mcp_command };
            missing.read(cx).focus_handle(cx).focus(window, cx);
            cx.notify();
            return;
        };
        let args: Vec<String> = parts.collect();
        let added = name.clone();
        self.update_extensions(move |config| config.add_mcp_server(&name, &command, args), cx);
        if self.extension_error.is_none() {
            self.close_mcp_form(window, cx);
            announce(t!("a11y.mcp_added", name = added).to_string(), cx);
        }
    }

    /// 移除一个服务器。它这一行（连同刚按下的按钮）会消失，焦点移到“添加服务器”按钮，不留在空处。
    fn remove_mcp_server(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let removed = name.clone();
        self.update_extensions(move |config| config.remove_mcp_server(&name), cx);
        if self.extension_error.is_none() {
            self.mcp_add_focus.focus(window, cx);
            announce(t!("a11y.mcp_removed", name = removed).to_string(), cx);
        }
    }

    /// 全局说明文件；还不存在时先建一个空的，保证能打开。
    fn open_global_instructions(&mut self, cx: &mut Context<Self>) {
        let home = self.backend.home();
        let path = home.join("AGENTS.md");
        if !path.exists() {
            let _ = std::fs::create_dir_all(home);
            let _ = std::fs::write(&path, "");
        }
        cx.open_with_system(&path);
    }

    fn open_skills_dir(&mut self, cx: &mut Context<Self>) {
        let dir = self.backend.home().join("skills");
        let _ = std::fs::create_dir_all(&dir);
        cx.open_with_system(&dir);
    }

    /// 弹窗卡片的进场进度。工作区每帧按弹窗的状态设，卡片据此画出淡入、上浮的样子。
    pub(crate) fn set_phase(&mut self, phase: motion::Phase) {
        self.phase = phase;
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus_handle.focus(window, cx);
    }

    /// 录制中按下的组合键：Esc 取消，单独的修饰键继续等待，其余保存为新快捷键。
    fn record_keystroke(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(action) = self.recording else {
            return;
        };
        let keystroke = &event.keystroke;
        if keystroke.key == "escape" && !keystroke.modifiers.modified() {
            self.recording = None;
            cx.notify();
            return;
        }
        let Some(keys) = keystroke_string(keystroke) else {
            return;
        };
        self.recording = None;
        self.save_shortcut(
            |bindings| {
                bindings.bindings.insert(action.to_owned(), keys);
            },
            cx,
        );
    }

    fn save_shortcut(&mut self, edit: impl FnOnce(&mut Keybindings), cx: &mut Context<Self>) {
        let mut bindings = self.bindings.clone();
        edit(&mut bindings);
        match self.backend.save_keybindings(&bindings) {
            Ok(()) => {
                self.shortcut_error = None;
                bind_keys(&bindings, cx);
                self.bindings = bindings;
                announce(t!("settings.saved").to_string(), cx);
            }
            Err(error) => {
                announce(error.clone(), cx);
                self.shortcut_error = Some(error);
            }
        }
        cx.notify();
    }

    fn set_text_scale(&mut self, scale: TextScalePreference, cx: &mut Context<Self>) {
        if self.backend.update_config(|config| config.ui.text_scale = scale).is_ok() {
            crate::design::set_text_scale(crate::text_scale_percent(scale));
            crate::sync_theme(cx);
            cx.refresh_windows();
        }
    }

    fn set_motion(&mut self, motion: MotionPreference, cx: &mut Context<Self>) {
        if self.backend.update_config(|config| config.ui.motion = motion).is_ok() {
            apply_motion(motion, cx);
            cx.notify();
        }
    }

    fn subscribe_form(form: &Entity<ProviderForm>, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe_in(form, window, |this, _, event: &FormEvent, window, cx| {
            if let FormEvent::AccountChanged = event {
                // 登录/退出只影响凭据与就绪状态：刷新列表，不收起正在编辑的表单。
                this.refresh_providers(window, cx);
                cx.emit(SettingsEvent::Changed);
                cx.notify();
                return;
            }
            if let FormEvent::Saved = event {
                cx.emit(SettingsEvent::Changed);
                announce(t!("a11y.provider_saved").to_string(), cx);
            }
            // 表单收起后它里面的控件都没了：焦点回到打开它的按钮（编辑的提供商被删掉时回到“添加提供商”）。
            let opened_from = this.form_open.clone().flatten();
            this.reset_form(window, cx);
            this.refresh_providers(window, cx);
            let target = opened_from
                .filter(|id| this.providers.iter().any(|provider| &provider.config.id == id))
                .and_then(|id| this.edit_provider_focus.get(&id).cloned())
                .unwrap_or_else(|| this.add_provider_focus.clone());
            target.focus(window, cx);
        })
    }

    /// 收起表单（保存、取消之后，切换语言时），并换上一份新的“添加”表单。
    fn reset_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        self.form = cx.new(|cx| ProviderForm::add(backend, window, cx));
        self._form_subscription = Self::subscribe_form(&self.form, window, cx);
        self.form_open = None;
        cx.notify();
    }

    /// 展开“添加提供商”表单。按下的按钮会被表单替换掉，焦点直接放进表单的第一个控件，不会飘走。
    fn add_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_form(window, cx);
        self.form_open = Some(None);
        self.form.update(cx, |form, cx| form.focus_first(window, cx));
    }

    fn edit_provider(&mut self, provider: ProviderConfig, window: &mut Window, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let id = provider.id.clone();
        self.form = cx.new(|cx| ProviderForm::edit(backend, provider, window, cx));
        self._form_subscription = Self::subscribe_form(&self.form, window, cx);
        self.form_open = Some(Some(id));
        self.form.update(cx, |form, cx| form.focus_first(window, cx));
        cx.notify();
    }

    /// 重新读取提供商列表与默认模型候选项。
    fn refresh_providers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = self.backend.config();
        self.providers = config
            .providers
            .iter()
            .map(|provider| {
                let resolved = provider.resolve();
                // 账号登录的提供商（deepseek-oauth）的就绪状态看登录令牌，不看 API Key。
                let ready = if resolved.id == dshr_config::providers::DEEPSEEK_OAUTH {
                    // 自定义接口地址时不能带账号令牌（可能发给第三方），同 `Backend::account_token_for`。
                    let official = dshr_config::providers::adapter(dshr_config::providers::DEEPSEEK_OAUTH)
                        .is_some_and(|adapter| adapter.base_url == resolved.base_url);
                    official && self.backend.is_signed_in()
                } else {
                    self.backend.is_ready(&resolved)
                };
                ProviderCard { config: provider.clone(), ready, name: resolved.name }
            })
            .collect();

        for provider in &self.providers {
            self.edit_provider_focus.entry(provider.config.id.clone()).or_insert_with(|| cx.focus_handle());
        }
        let _ = window;

        let choices = self.backend.model_choices();
        // 按提供商分组：列表里提供商是组名，选项只写模型名。拼成“提供商 · 模型”的话，名字长的提供商
        // （DeepSeek OAuth）会把真正区分选项的模型名截掉，几项看起来一模一样。
        let labels: Vec<(SharedString, SharedString)> =
            choices.iter().map(|choice| (choice.provider_name.clone().into(), choice.label.clone().into())).collect();
        let current = config.default_model();
        let selected = current.and_then(|current| choices.iter().position(|choice| choice.reference == current));
        self.default_choices = choices.into_iter().map(|choice| choice.reference).collect();
        self.default_model.update(cx, |select, cx| select.set_grouped_items(labels, selected, cx));
        cx.notify();
    }

    fn set_theme(&mut self, theme: ThemePreference, window: &mut Window, cx: &mut Context<Self>) {
        if self.backend.update_config(|config| config.ui.theme = theme).is_ok() {
            apply_theme(theme, window, cx);
            cx.notify();
        }
    }

    fn save_defaults(&mut self, cx: &mut Context<Self>) {
        let default =
            self.default_model.read(cx).selected_index().and_then(|index| self.default_choices.get(index)).map(ModelRef::to_string);
        let effort = self.effort_default;
        let permission = self.permission_select.read(cx).selected_index().and_then(|index| PERMISSION_CHOICES.get(index)).copied();
        let preset = self.preset_select.read(cx).selected_index().and_then(|index| Preset::ALL.get(index)).copied();
        let result = self.backend.update_config(|config| {
            if let Some(default) = default {
                config.model.default = default;
            }
            config.model.effort = effort;
            // 选的是内置默认值时不写，配置文件里只留改过的。
            config.model.permission = permission.filter(|mode| *mode != PermissionMode::default()).map(|mode| mode.id().to_owned());
            config.model.preset = preset.filter(|preset| *preset != Preset::default()).map(|preset| preset_id(preset).to_owned());
        });
        self.defaults_message = Some(match result {
            Ok(()) => {
                cx.emit(SettingsEvent::Changed);
                (t!("settings.saved").to_string(), false)
            }
            Err(error) => (error, true),
        });
        if let Some((text, _)) = &self.defaults_message {
            announce(text.clone(), cx);
        }
        cx.notify();
    }

    fn open_config_file(&mut self, cx: &mut Context<Self>) {
        // 文件还不存在时先写一份当前配置，保证能打开。
        let path = self.backend.config_path();
        if !path.exists() {
            let _ = self.backend.update_config(|_| {});
        }
        cx.open_with_system(&path);
    }

    /// 导航：界面插件登记的页，按登记顺序。
    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let p = palette(cx);
        let items: Vec<_> = self
            .pages
            .iter()
            .enumerate()
            .map(|(index, page)| {
                nav_item(("settings-nav", index), page.icon.clone(), (page.label)(), self.page == index, &p).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if this.page != index {
                            this.page = index;
                        }
                        cx.notify();
                    },
                ))
            })
            .collect();
        v_flex()
            .id("settings-nav")
            .role(Role::TabList)
            .aria_label(t!("settings.title").to_string())
            .aria_orientation(Orientation::Vertical)
            // 弹窗变窄（窗口窄）时导航一起变窄，给正文留出空间：约为弹窗宽度的四分之一，140–220。
            .w(relative(0.24))
            .min_w(px(140.))
            .max_w(px(size::MODAL_NAV_WIDTH))
            .flex_none()
            .px(px(space::S))
            .gap(px(space::XS))
            .children(items)
    }

    /// 当前页的内容。插件自己的页面第一次显示时才建视图。
    fn render_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(page) = self.pages.get(self.page).cloned() else {
            return div().into_any_element();
        };
        match page.content {
            PageContent::Builtin(render) => render(self, cx),
            PageContent::View(build) => {
                let backend = self.backend.clone();
                self.page_views.entry(page.id).or_insert_with(|| build(&backend, window, cx)).clone().into_any_element()
            }
        }
    }

    /// 一个插件按 schema 生成的配置项：布尔是开关，几选一是分段按钮，数字和文本是输入框加“保存”。
    fn render_plugin_fields(&self, row_index: usize, row: &PluginRow, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = palette(cx);
        row.fields
            .iter()
            .enumerate()
            .map(|(field_index, state)| {
                let field = &state.field;
                let label = plugin_field_label(row.id, row.card.as_ref(), field);
                // 元素 id：同一页里每个插件的每一项都不同。
                let element_id = SharedString::from(format!("plugin-field-{}-{}", row.id, field.key));
                let control: AnyElement = match (&field.kind, &state.input) {
                    (FieldKind::Bool, _) => reveal(SharedString::from(format!("reveal-{element_id}")), {
                        let checked = field.checked(state.value.as_ref());
                        h_flex()
                            .gap(px(space::S))
                            .child(
                                crate::design::switch(element_id.clone(), checked, &p)
                                    .accessibility_label(in_section(row.id, &label))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let value = this.plugin_rows[row_index].fields[field_index]
                                            .field
                                            .normalize(serde_json::Value::Bool(!checked));
                                        this.save_plugin_field(row_index, field_index, value, cx);
                                    })),
                            )
                            .child(div().text_body().child(label.clone()))
                    })
                    .into_any_element(),
                    (FieldKind::Choice(choices), _) => {
                        let current = field.display(state.value.as_ref());
                        let selected = choices.iter().position(|choice| *choice == current);
                        let name = in_section(row.id, &label);
                        match &state.choice {
                            Some(select) => {
                                select.update(cx, |select, cx| {
                                    select.set_label(name, cx);
                                    select.set_selected(selected, cx);
                                });
                                field_row(label.clone(), select.clone()).into_any_element()
                            }
                            None => div().into_any_element(),
                        }
                    }
                    (_, Some(input)) => field_row(
                        label.clone(),
                        h_flex()
                            .items_center()
                            .gap(px(space::S))
                            .child(crate::design::framed_handle(
                                gpui_kit::Focusable::focus_handle(input.read(cx), cx),
                                Input::new(input).w(px(200.)).aria_label(in_section(row.id, &label)),
                            ))
                            .child(crate::design::framed(
                                SharedString::from(format!("{element_id}-save")),
                                Button::new(SharedString::from(format!("{element_id}-save")))
                                    .label(t!("settings.save").to_string())
                                    .accessibility_label(t!("settings.plugin_save_field", name = label).to_string())
                                    .on_click(cx.listener(move |this, _, _, cx| this.submit_plugin_field(row_index, field_index, cx))),
                            )),
                    )
                    .into_any_element(),
                    // 数字和文本项一定有输入框。
                    (_, None) => div().into_any_element(),
                };
                v_flex()
                    .gap(px(space::XS))
                    .child(control)
                    .children(plugin_field_hint(row.id, row.card.as_ref(), field).map(|text| hint(text, Tone::Neutral, &p)))
                    .into_any_element()
            })
            .collect()
    }

    fn render_extensions(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        // 这几块各属于一个插件：主程序没装这个插件时不显示。
        let open_buttons = h_flex()
            .gap(px(space::S))
            .children(self.has_plugin("instructions").then(|| {
                reveal(
                    "reveal-open-instructions",
                    crate::design::framed(
                        "open-instructions",
                        Button::new("open-instructions")
                            .control_sm()
                            .label(t!("settings.open_instructions").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.open_global_instructions(cx))),
                    ),
                )
            }))
            .children(self.has_plugin("skills").then(|| {
                reveal(
                    "reveal-open-skills",
                    crate::design::framed(
                        "open-skills",
                        Button::new("open-skills")
                            .control_sm()
                            .label(t!("settings.open_skills").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.open_skills_dir(cx))),
                    ),
                )
            }));
        let instructions = (self.has_plugin("instructions") || self.has_plugin("skills")).then(|| {
            card(&p).p(px(space::XL)).child(
                v_flex()
                    .gap(px(space::M))
                    .child(section_header(t!("settings.instructions").to_string(), None, &p))
                    .child(hint(t!("settings.instructions_hint").to_string(), Tone::Neutral, &p))
                    .child(open_buttons),
            )
        });

        let mcp = self.has_plugin("mcp").then(|| self.render_mcp(cx));

        // 内置插件和 JS 插件分开列：前者是开关和配置项，后者还有授权、日志和热重载的状态。
        let (scripts, builtin): (Vec<usize>, Vec<usize>) =
            (0..self.plugin_rows.len()).partition(|&index| self.script_ids.contains(&self.plugin_rows[index].id));
        // 每个插件一张卡片，两列流式排布；窄的时候一列。
        let rows = |indices: &[usize], cx: &mut Context<Self>| -> Vec<AnyElement> {
            if indices.is_empty() {
                return Vec::new();
            }
            let cards: Vec<AnyElement> = indices
                .iter()
                .map(|&index| {
                    div()
                        .flex_1()
                        .min_w(px(300.))
                        .child(reveal(("reveal-plugin", index), self.render_plugin_row(index, cx)))
                        .into_any_element()
                })
                .collect();
            vec![h_flex().flex_wrap().items_start().gap(px(space::M)).children(cards).into_any_element()]
        };
        let builtin_rows = rows(&builtin, cx);
        let builtin = card(&p).px(px(space::XL)).py(px(space::L)).child(
            v_flex()
                .child(
                    v_flex()
                        .gap(px(space::XS))
                        .pb(px(space::S))
                        .child(section_header(t!("settings.plugins").to_string(), None, &p))
                        .child(hint(t!("settings.plugins_hint").to_string(), Tone::Neutral, &p)),
                )
                .children(builtin_rows),
        );

        let script_rows = rows(&scripts, cx);
        let no_scripts = script_rows.is_empty().then(|| hint(t!("settings.scripts_empty").to_string(), Tone::Neutral, &p));
        let scripts = self.backend.has_scripts().then(|| {
            card(&p).px(px(space::XL)).py(px(space::L)).child(
                v_flex()
                    .child(
                        h_flex()
                            .pb(px(space::S))
                            .gap(px(space::M))
                            .items_start()
                            .child(
                                // min_w(0)：说明文字在卡片里换行，不把右边的按钮挤出去。
                                v_flex()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .gap(px(space::XS))
                                    .child(section_header(t!("settings.scripts").to_string(), None, &p))
                                    .child(hint(t!("settings.scripts_hint").to_string(), Tone::Neutral, &p)),
                            )
                            .child(reveal(
                                "reveal-open-plugins-dir",
                                crate::design::framed(
                                    "open-plugins-dir",
                                    Button::new("open-plugins-dir")
                                        .subtle(cx)
                                        .control_sm()
                                        .flex_none()
                                        .icon(IconName::FolderOpen)
                                        .label(t!("settings.open_plugins_dir").to_string())
                                        .on_click(cx.listener(|this, _, _, cx| this.open_plugins_dir(cx))),
                                ),
                            )),
                    )
                    .children(no_scripts)
                    .children(script_rows)
                    .child(div().pt(px(space::L)).child(self.render_new_plugin(cx))),
            )
        });

        v_flex()
            .gap(px(space::XL))
            .child(section_header(t!("settings.extensions").to_string(), Some(t!("settings.extensions_desc").to_string().into()), &p))
            .children(scripts.map(|scripts| reveal("reveal-scripts", scripts)))
            .children(mcp.map(|mcp| reveal("reveal-mcp-section", mcp)))
            .children(instructions.map(|instructions| reveal("reveal-instructions", instructions)))
            // 内置功能平时很少改，放在最后：常用的 JS 插件、MCP 和项目说明在前面。
            .child(reveal("reveal-builtin", builtin))
            .into_any_element()
    }

    /// MCP 服务器：标题和“添加服务器”按钮在上，已添加的服务器列在下面；表单只在添加时展开（和提供商一样）。
    ///
    /// 每个控件的名称都带上“MCP 服务器”：按 Tab 走到这里时，读屏软件读出的不只是“名称”“启动命令”。
    fn render_mcp(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let p = palette(cx);
        let section = t!("settings.mcp_servers").to_string();
        let servers: Vec<AnyElement> = self
            .mcp_rows
            .iter()
            .enumerate()
            .map(|(index, server)| {
                let (toggle_name, remove_name) = (server.name.clone(), server.name.clone());
                let (reconnect_name, log_name, login_name) = (server.name.clone(), server.name.clone(), server.name.clone());
                let login = self.mcp_status.iter().find(|status| status.name == server.name).and_then(|status| {
                    if status.state == dshr_mcp::ServerState::NeedsLogin { Some(false) } else { status.signed_in.then_some(true) }
                });
                let state = server.enabled.then(|| self.mcp_state_text(&server.name)).flatten();
                let log_open = self.mcp_open_log.as_deref() == Some(server.name.as_str());
                let log_lines = log_open.then(|| self.backend.mcp_logs(&server.name));
                let status_row = state.map(|(text, tone)| {
                    h_flex()
                        .pl(px(space::XL))
                        .gap(px(space::S))
                        .child(div().flex_1().min_w(px(0.)).child(hint(text, tone, &p)))
                        .children(login.map(|sign_out| {
                            let name = login_name.clone();
                            crate::design::framed(
                                ("mcp-login", index),
                                Button::new(("mcp-login", index))
                                    .control_xs()
                                    .when(sign_out, |button| button.subtle(cx))
                                    .when(!sign_out, |button| button.primary())
                                    .label(if sign_out { t!("settings.mcp_logout") } else { t!("settings.mcp_login") }.to_string())
                                    .on_click(cx.listener(move |this, _, _, cx| this.mcp_sign_in(name.clone(), sign_out, cx))),
                            )
                        }))
                        .child(crate::design::framed(
                            ("mcp-reconnect", index),
                            Button::new(("mcp-reconnect", index))
                                .subtle(cx)
                                .control_xs()
                                .label(t!("settings.mcp_reconnect").to_string())
                                .accessibility_label(t!("settings.mcp_reconnect_server", name = server.name).to_string())
                                .on_click(cx.listener(move |this, _, _, cx| this.reconnect_mcp(reconnect_name.clone(), cx))),
                        ))
                        .child(crate::design::framed(
                            ("mcp-log", index),
                            Button::new(("mcp-log", index))
                                .subtle(cx)
                                .control_xs()
                                .label(if log_open { t!("settings.plugin_logs_hide") } else { t!("settings.plugin_logs") }.to_string())
                                .accessibility_label(t!("settings.plugin_logs_named", name = server.name).to_string())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.mcp_open_log =
                                        if this.mcp_open_log.as_deref() == Some(log_name.as_str()) { None } else { Some(log_name.clone()) };
                                    cx.notify();
                                })),
                        ))
                });
                let log = log_lines.map(|lines| {
                    let start = lines.len().saturating_sub(40);
                    let text = if lines.is_empty() { t!("settings.plugin_logs_empty").to_string() } else { lines[start..].join("\n") };
                    div()
                        .pl(px(space::XL))
                        .text_caption()
                        .text_mono(mono_font(cx))
                        .text_color(p.text_secondary)
                        .child(readable_with_id(("mcp-log-text", index), text))
                });
                let row = h_flex()
                    .id(("mcp-row", index))
                    .gap(px(space::M))
                    .py(px(space::XS))
                    .child(
                        crate::design::switch(("mcp-server", index), server.enabled, &p)
                            .accessibility_label(server.name.clone())
                            .aria_description(server.command_line.clone())
                            .on_click(cx.listener({
                                let checked = !server.enabled;
                                move |this, _, _, cx| {
                                    let name = toggle_name.clone();
                                    this.update_extensions(move |config| config.set_mcp_server_enabled(&name, checked), cx);
                                }
                            })),
                    )
                    .child(div().flex_none().text_body().child(server.name.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_mono(mono_font(cx))
                            .text_color(p.text_secondary)
                            .child(readable_with_id(("mcp-command", index), server.command_line.clone())),
                    )
                    .child(crate::design::framed(
                        ("remove-mcp", index),
                        Button::new(("remove-mcp", index))
                            .subtle(cx)
                            .control_xs()
                            .label(t!("settings.mcp_remove").to_string())
                            .accessibility_label(t!("settings.mcp_remove_server", name = server.name).to_string())
                            .on_click(cx.listener(move |this, _, window, cx| this.remove_mcp_server(remove_name.clone(), window, cx))),
                    ));
                v_flex().child(row).children(status_row).children(log).into_any_element()
            })
            .collect();
        let no_servers = servers.is_empty().then(|| hint(t!("settings.mcp_empty").to_string(), Tone::Neutral, &p));

        // 表单展开、收起即时完成。
        let form = self.mcp_form_open.then(|| {
            let name_label = t!("settings.mcp_name").to_string();
            let command_label = t!("settings.mcp_command").to_string();
            div().pt(px(space::M)).child(
                panel(&p).child(
                    v_flex()
                        .id("mcp-form")
                        .role(Role::Group)
                        .aria_label(t!("a11y.mcp_new").to_string())
                        .gap(px(space::L))
                        .child(heading(t!("a11y.mcp_new").to_string(), 3).text_label())
                        .child(field(
                            name_label.clone(),
                            crate::design::framed_handle(
                                gpui_kit::Focusable::focus_handle(self.mcp_name.read(cx), cx),
                                Input::new(&self.mcp_name).w(px(240.)).aria_label(in_section(&section, &name_label)),
                            ),
                        ))
                        .child(
                            field(
                                command_label.clone(),
                                // 命令的格式说明直接写进名称：读屏软件在这个输入框上就能听到，不用再去找下面的提示。
                                crate::design::framed_handle(
                                    gpui_kit::Focusable::focus_handle(self.mcp_command.read(cx), cx),
                                    Input::new(&self.mcp_command)
                                        .aria_label(in_section(in_section(&section, &command_label), t!("settings.mcp_command_hint"))),
                                ),
                            )
                            .child(hint(t!("settings.mcp_command_hint").to_string(), Tone::Neutral, &p)),
                        )
                        .child(
                            h_flex()
                                .gap(px(space::S))
                                .children(
                                    self.extension_error
                                        .clone()
                                        .map(|error| motion::fade_in(hint(error, Tone::Danger, &p), "extension-error-enter")),
                                )
                                .child(div().flex_1())
                                .child(reveal(
                                    "reveal-cancel-mcp",
                                    crate::design::framed(
                                        "cancel-mcp",
                                        Button::new("cancel-mcp")
                                            .label(t!("settings.cancel").to_string())
                                            .on_click(cx.listener(|this, _, window, cx| this.close_mcp_form(window, cx))),
                                    ),
                                ))
                                .child(reveal(
                                    "reveal-add-mcp",
                                    crate::design::framed(
                                        "add-mcp",
                                        Button::new("add-mcp")
                                            .primary()
                                            .label(t!("settings.mcp_add").to_string())
                                            .accessibility_label(in_section(&section, t!("settings.mcp_add")))
                                            .on_click(cx.listener(|this, _, window, cx| this.add_mcp_server(window, cx))),
                                    ),
                                )),
                        )
                        // 表单里按 Esc 收起表单，而不是关掉整个设置弹窗。
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "escape" {
                                cx.stop_propagation();
                                this.close_mcp_form(window, cx);
                            }
                        })),
                ),
            )
        });

        card(&p).id("mcp-section").role(Role::Group).aria_label(section.clone()).px(px(space::XL)).py(px(space::L)).child(
            v_flex()
                .child(h_flex().pb(px(space::S)).child(div().flex_1().child(section_header(section.clone(), None, &p))).when(
                    !self.mcp_form_open,
                    |header| {
                        header.child(reveal(
                            "reveal-open-mcp-form",
                            crate::design::framed_handle(
                                (self.mcp_add_focus).clone(),
                                Button::new("open-mcp-form")
                                    .control_sm()
                                    .icon(IconName::Plus)
                                    .label(t!("a11y.mcp_add_server").to_string())
                                    .accessibility_label(in_section(&section, t!("a11y.mcp_add_server")))
                                    .track_focus(&self.mcp_add_focus)
                                    .on_click(cx.listener(|this, _, window, cx| this.open_mcp_form(window, cx))),
                            )
                            .tint("open-mcp-form"),
                        ))
                    },
                ))
                .children(no_servers)
                .child(v_flex().children(servers.into_iter().enumerate().map(|(index, row)| reveal(("reveal-mcp", index), row))))
                .children(form)
                // 表单收起时的错误（比如移除失败）显示在列表下面。
                .when(!self.mcp_form_open, |column| {
                    column.children(
                        self.extension_error.clone().map(|error| {
                            motion::fade_in(div().pt(px(space::S)).child(hint(error, Tone::Danger, &p)), "extension-error-enter")
                        }),
                    )
                }),
        )
    }

    /// 插件的卡片：名字、版本、状态和开关；一句话说明；**权限**（每项一行，没允许的标“待允许”）和“允许”按钮；
    /// 提供的东西；出问题时的原因；常驻服务的状态。配置项、密钥、日志收在“详情”里。
    fn render_plugin_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let row = &self.plugin_rows[index];
        let id = row.id;
        let script = self.script_ids.contains(&id);
        let plugin_card = row.card.as_ref();
        let name = plugin_card.and_then(|card| card.name.clone()).unwrap_or_else(|| permissions::builtin_name(id));
        let version = plugin_card.and_then(|card| card.version.clone());
        let description = plugin_card.and_then(|card| card.description.clone()).or_else(|| plugin_description(id));

        // 状态：文字加颜色（不只靠颜色）。
        let (status, tone) = if !row.grant.is_empty() {
            (t!("settings.status_needs_grant"), Tone::Warning)
        } else if row.problem.is_some() {
            (t!("settings.status_failed"), Tone::Danger)
        } else if row.stopped.is_some() {
            (t!("settings.status_stopped"), Tone::Danger)
        } else if row.kept.is_some() {
            (t!("settings.status_kept"), Tone::Warning)
        } else if !row.shown_enabled() {
            (t!("settings.status_disabled"), Tone::Neutral)
        } else {
            (t!("settings.status_enabled"), Tone::Success)
        };
        let status_color = tone.color(&p);
        let status_pill = div()
            .flex_none()
            .px(px(space::S))
            .rounded(px(radius::SM))
            .border_1()
            .border_color(status_color)
            .text_caption()
            .text_color(status_color)
            .child(readable_with_id(("plugin-status", index), status.to_string()));

        // 权限：JS 插件是清单里声明的能力，内置插件查表。
        let declared: Vec<String> = plugin_card.map(|card| card.declared.clone()).unwrap_or_default();
        let lines: Vec<(String, Option<String>, bool, bool)> = if script {
            declared
                .iter()
                .map(|name| {
                    let pending = row.grant.contains(name);
                    (permissions::capability_text(name), Some(name.clone()), pending, permissions::capability_sensitive(name))
                })
                .collect()
        } else {
            permissions::builtin_permissions(id).into_iter().map(|text| (text, None, false, false)).collect()
        };
        let permission_rows: Vec<AnyElement> = if lines.is_empty() {
            vec![div().text_caption().text_color(p.text_secondary).child(t!("settings.no_permissions").to_string()).into_any_element()]
        } else {
            lines
                .into_iter()
                .enumerate()
                .map(|(line, (text, raw, pending, sensitive))| {
                    // 读屏软件读到“读取当前对话事件（session:read），待允许”。
                    let spoken = match (&raw, pending) {
                        (Some(raw), true) => format!("{text}（{raw}），{}", t!("settings.pending_grant")),
                        (Some(raw), false) => format!("{text}（{raw}）"),
                        (None, _) => text.clone(),
                    };
                    h_flex()
                        .id(SharedString::from(format!("plugin-{index}-permission-{line}")))
                        .role(Role::Label)
                        .aria_value(spoken)
                        .gap(px(space::S))
                        .items_start()
                        .child(div().flex_none().text_body().text_color(p.text_secondary).child("·"))
                        .child(div().flex_1().min_w(px(0.)).text_body().when(sensitive, |text| text.text_color(p.warning)).child(text))
                        .children(pending.then(|| {
                            div().flex_none().text_caption().text_color(p.warning).child(t!("settings.pending_grant").to_string())
                        }))
                        .into_any_element()
                })
                .collect()
        };

        let provides: Vec<String> = plugin_card
            .map(|card| card.provides.iter().map(|(kind, count)| permissions::provides_text(kind, *count)).collect())
            .unwrap_or_default();
        let services: Vec<String> = plugin_card
            .map(|card| {
                card.services
                    .iter()
                    .map(|(service, status)| t!("settings.service_status", id = service, status = status).to_string())
                    .collect()
            })
            .unwrap_or_default();
        // 索引里的新版本、可以回滚到的旧版本。
        let update = self.backend.plugin_update(row.id);
        let rollback_to = row.rollback_to.clone();
        // 链接到插件目录之外的文件夹（开发中）。
        let linked = plugin_card.and_then(|card| card.linked.as_ref()).map(|folder| folder.display().to_string());
        // 删除只在装在插件目录里的 JS 插件上出现：内置插件在程序里，链接的开发中插件只取消链接。
        let deletable = script && linked.is_none();
        // 插件自带的 MCP 服务器：启动方式和连接状态。
        let mcp_lines: Vec<(String, Tone)> = plugin_card
            .map(|card| {
                card.mcp_servers
                    .iter()
                    .map(|(name, launch, status)| {
                        use dshr_mcp::ServerState;
                        let (state, tone) = match status.as_ref().map(|status| (&status.state, status)) {
                            Some((ServerState::Ready, status)) => {
                                (t!("settings.mcp_state_ready", tools = status.tools).to_string(), Tone::Success)
                            }
                            Some((ServerState::Failed | ServerState::Stopped, status)) => (
                                t!("settings.mcp_state_stopped", error = status.error.clone().unwrap_or_default()).to_string(),
                                Tone::Danger,
                            ),
                            Some((ServerState::Connecting, _)) => (t!("settings.mcp_state_connecting").to_string(), Tone::Neutral),
                            _ => (t!("settings.mcp_state_idle").to_string(), Tone::Neutral),
                        };
                        (format!("MCP {name}：{launch} — {state}"), tone)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let fields = self.render_plugin_fields(index, row, cx);
        let has_details = !fields.is_empty() || !row.secrets.is_empty() || script;
        let expanded = row.expanded;

        let header = h_flex()
            .gap(px(space::S))
            .items_center()
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(space::S))
                    .items_baseline()
                    .child(
                        div()
                            .id(("plugin-name", index))
                            .role(Role::Heading)
                            .aria_level(3)
                            .aria_label(name.clone())
                            .text_label()
                            .truncate()
                            .child(name.clone()),
                    )
                    .children(
                        version.map(|version| div().flex_none().text_caption().text_color(p.text_secondary).child(format!("v{version}"))),
                    ),
            )
            .child(status_pill)
            .child(
                crate::design::switch(("plugin", index), row.shown_enabled(), &p)
                    .accessibility_label(name.clone())
                    .when_some(description.clone(), |switch, description| switch.aria_description(description))
                    .on_click(cx.listener({
                        let checked = !row.shown_enabled();
                        move |this, _, _, cx| this.toggle_plugin(id, checked, cx)
                    })),
            );

        let mut body = v_flex().gap(px(space::S)).child(header).children(description.map(|text| hint(text, Tone::Neutral, &p))).child(
            v_flex()
                .gap(px(2.))
                .child(div().text_caption().text_color(p.text_secondary).child(t!("settings.permissions").to_string()))
                .children(permission_rows)
                .children(
                    (!script).then(|| div().text_caption().text_color(p.text_secondary).child(t!("settings.builtin_note").to_string())),
                ),
        );
        if !row.grant.is_empty() {
            body = body.child(
                h_flex()
                    .gap(px(space::S))
                    .items_center()
                    .p(px(space::S))
                    .rounded(px(radius::MD))
                    .bg(p.accent_soft)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_caption()
                            .child(readable_with_id(("plugin-grant-needed", index), t!("settings.plugin_grant_needed").to_string())),
                    )
                    .child(reveal(
                        ("reveal-grant-plugin", index),
                        crate::design::framed(
                            ("grant-plugin", index),
                            Button::new(("grant-plugin", index))
                                .primary()
                                .control_sm()
                                .flex_none()
                                .label(t!("settings.plugin_grant").to_string())
                                .accessibility_label(t!("settings.plugin_grant_named", name = name).to_string())
                                .on_click(cx.listener(move |this, _, _, cx| this.grant_plugin(index, cx))),
                        ),
                    )),
            );
        }
        body = body
            .children(
                (!provides.is_empty()).then(|| hint(t!("settings.provides", items = provides.join(" · ")).to_string(), Tone::Neutral, &p)),
            )
            // 这次运行里没能激活的插件：说明原因（状态不只靠徽标表达）。
            .children(row.problem.clone().map(|problem| hint(problem, Tone::Danger, &p)))
            .children(row.kept.clone().map(|kept| hint(t!("settings.plugin_kept", error = kept).to_string(), Tone::Danger, &p)))
            .children(row.stopped.clone().map(|stopped| hint(t!("settings.plugin_stopped", error = stopped).to_string(), Tone::Danger, &p)))
            .children(services.into_iter().map(|line| hint(line, Tone::Neutral, &p)))
            .children(mcp_lines.into_iter().map(|(line, tone)| hint(line, tone, &p)))
            .children(update.map(|version| {
                let id = row.id;
                h_flex()
                    .gap(px(space::S))
                    .child(div().flex_1().min_w(px(0.)).child(hint(
                        t!("settings.plugin_update_available", version = version).to_string(),
                        Tone::Neutral,
                        &p,
                    )))
                    .child(crate::design::framed(
                        ("update-plugin", index),
                        Button::new(("update-plugin", index))
                            .control_xs()
                            .label(t!("settings.plugin_update").to_string())
                            .on_click(cx.listener(move |this, _, window, cx| this.update_plugin(id, window, cx))),
                    ))
            }))
            .children(rollback_to.map(|version| {
                let id = row.id;
                h_flex().child(crate::design::framed(
                    ("rollback-plugin", index),
                    Button::new(("rollback-plugin", index))
                        .subtle(cx)
                        .control_xs()
                        .label(t!("settings.plugin_rollback", version = version).to_string())
                        .on_click(cx.listener(move |this, _, window, cx| this.rollback_plugin(id, window, cx))),
                ))
            }))
            .children(deletable.then(|| {
                let id = row.id;
                let confirming = self.pending_delete == Some(id);
                let (label, accessibility) = if confirming {
                    (
                        t!("settings.plugin_delete_confirm").to_string(),
                        t!("settings.plugin_delete_confirm_named", name = name.clone()).to_string(),
                    )
                } else {
                    (t!("settings.plugin_delete").to_string(), t!("settings.plugin_delete_named", name = name.clone()).to_string())
                };
                h_flex().child(crate::design::framed(
                    ("delete-plugin", index),
                    Button::new(("delete-plugin", index))
                        .danger()
                        .control_xs()
                        .label(label)
                        .accessibility_label(accessibility)
                        .on_click(cx.listener(move |this, _, _, cx| this.delete_plugin(id, cx))),
                ))
            }))
            .children(linked.map(|folder| {
                let id = row.id;
                h_flex()
                    .gap(px(space::S))
                    .child(div().flex_1().min_w(px(0.)).child(hint(
                        t!("settings.plugin_linked", path = folder).to_string(),
                        Tone::Neutral,
                        &p,
                    )))
                    .child(crate::design::framed(
                        ("unlink-plugin", index),
                        Button::new(("unlink-plugin", index))
                            .subtle(cx)
                            .control_xs()
                            .label(t!("settings.plugin_unlink").to_string())
                            .on_click(cx.listener(move |this, _, _, cx| this.unlink_plugin(id, cx))),
                    ))
            }))
            .children(row.message.clone().map(|(text, error)| {
                motion::fade_in(hint(text, if error { Tone::Danger } else { Tone::Success }, &p), ("plugin-message-enter", index))
            }));
        if has_details {
            body = body.child(
                div().child(reveal(
                    ("reveal-plugin-details", index),
                    crate::design::framed(
                        ("plugin-details", index),
                        Button::new(("plugin-details", index))
                            .subtle(cx)
                            .control_xs()
                            .icon(if expanded { IconName::ChevronUp } else { IconName::ChevronDown })
                            .label(if expanded { t!("settings.details_hide") } else { t!("settings.details_show") }.to_string())
                            .accessibility_label(t!("settings.details_named", name = name).to_string())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(row) = this.plugin_rows.get_mut(index) {
                                    row.expanded = !row.expanded;
                                }
                                cx.notify();
                            })),
                    ),
                )),
            );
        }
        if expanded {
            let secrets = row.secrets.iter().enumerate().map(|(secret_index, state)| {
                let status = if state.set { t!("settings.plugin_secret_set") } else { t!("settings.plugin_secret_unset") };
                let label = format!("{}（{status}）", state.name);
                v_flex()
                    .gap(px(space::XS))
                    .child(
                        div()
                            .text_body()
                            .child(readable_with_id(SharedString::from(format!("plugin-{index}-secret-{secret_index}")), label.clone())),
                    )
                    .child(reveal(
                        SharedString::from(format!("reveal-secret-{index}-{secret_index}")),
                        h_flex()
                            .gap(px(space::S))
                            .child(div().flex_1().min_w(px(120.)).child(crate::design::framed_handle(
                                gpui_kit::Focusable::focus_handle(state.input.read(cx), cx),
                                Input::new(&state.input).aria_label(label).suffix(crate::secret_reveal::toggle(
                                    SharedString::from(format!("plugin-{index}-secret-toggle-{secret_index}")),
                                    &state.input,
                                    crate::secret_reveal::Guard::Verify(state.set.then(|| {
                                        let (plugin, name) = (row.id, state.name.clone());
                                        Arc::new(move || dshr_plugins::secrets::load(plugin, &name)) as crate::secret_reveal::Loader
                                    })),
                                    cx,
                                )),
                            )))
                            .child(crate::design::framed(
                                SharedString::from(format!("plugin-{index}-secret-save-{secret_index}")),
                                Button::new(SharedString::from(format!("plugin-{index}-secret-save-{secret_index}")))
                                    .control_sm()
                                    .label(t!("settings.save").to_string())
                                    .on_click(
                                        cx.listener(move |this, _, window, cx| this.save_plugin_secret(index, secret_index, window, cx)),
                                    ),
                            )),
                    ))
                    .children(crate::secret_reveal::error(&state.input).map(|error| hint(error.to_string(), Tone::Danger, &p)))
            });
            body = body
                .child(divider(&p))
                .when(!fields.is_empty(), |column| column.child(v_flex().gap(px(space::M)).children(fields)))
                .when(!row.secrets.is_empty(), |column| {
                    column.child(
                        v_flex()
                            .gap(px(space::S))
                            .child(hint(t!("settings.plugin_secrets_hint").to_string(), Tone::Neutral, &p))
                            .children(secrets),
                    )
                })
                .children(script.then(|| self.render_plugin_log(index, id, cx)));
        }
        card(&p).flex_1().min_w(px(300.)).p(px(space::L)).child(body).into_any_element()
    }

    /// JS 插件的“日志”按钮，展开后是最近的日志（最多 40 行，新的在下）。
    fn render_plugin_log(&self, index: usize, id: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let open = self.open_log == Some(id);
        let toggle = reveal(
            ("reveal-plugin-log", index),
            crate::design::framed(
                ("plugin-log", index),
                Button::new(("plugin-log", index))
                    .subtle(cx)
                    .control_xs()
                    .label(if open { t!("settings.plugin_logs_hide") } else { t!("settings.plugin_logs") }.to_string())
                    .accessibility_label(t!("settings.plugin_logs_named", name = id).to_string())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_log = if this.open_log == Some(id) { None } else { Some(id) };
                        cx.notify();
                    })),
            ),
        );
        let mut column = v_flex().pl(px(space::XL)).gap(px(space::XS)).child(div().child(toggle));
        if open {
            let lines = self.backend.plugin_logs(id);
            if lines.is_empty() {
                column = column.child(hint(t!("settings.plugin_logs_empty").to_string(), Tone::Neutral, &p));
            } else {
                let start = lines.len().saturating_sub(40);
                let text: String =
                    lines[start..].iter().map(|line| format!("[{}] {}", line.level, line.text)).collect::<Vec<_>>().join("\n");
                column = column.child(
                    div()
                        .text_caption()
                        .text_mono(mono_font(cx))
                        .text_color(p.text_secondary)
                        .child(readable_with_id(("plugin-log-text", index), text)),
                );
            }
        }
        column.into_any_element()
    }

    /// “新建 JS 插件”：id 输入框、生成按钮、打开插件目录。
    fn render_new_plugin(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        field(
            t!("settings.plugin_new").to_string(),
            // 按钮和输入框一样高、垂直居中对齐。
            h_flex()
                .items_center()
                .gap(px(space::S))
                .child(crate::design::framed_handle(
                    gpui_kit::Focusable::focus_handle(self.new_plugin.read(cx), cx),
                    Input::new(&self.new_plugin).w(px(200.)).aria_label(in_section(t!("settings.scripts"), t!("settings.plugin_new"))),
                ))
                .child(crate::design::framed(
                    "create-plugin",
                    Button::new("create-plugin")
                        .label(t!("settings.plugin_create").to_string())
                        .on_click(cx.listener(|this, _, window, cx| this.create_plugin(window, cx))),
                ))
                .child(crate::design::framed(
                    "install-plugin-package",
                    Button::new("install-plugin-package")
                        .label(t!("settings.plugin_install_package").to_string())
                        .on_click(cx.listener(|this, _, window, cx| this.install_plugin(InstallSource::Package, window, cx))),
                ))
                .child(crate::design::framed(
                    "install-plugin-folder",
                    Button::new("install-plugin-folder")
                        .label(t!("settings.plugin_install_folder").to_string())
                        .on_click(cx.listener(|this, _, window, cx| this.install_plugin(InstallSource::Folder, window, cx))),
                )),
        )
        .child(hint(t!("settings.plugin_new_hint").to_string(), Tone::Neutral, &p))
        .children(self.new_plugin_message.clone().map(|(text, error)| {
            motion::fade_in(hint(text, if error { Tone::Danger } else { Tone::Success }, &p), "new-plugin-message-enter")
        }))
        .children(self.render_browse(cx))
        .into_any_element()
    }

    fn render_general(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        // 一次渲染只读一次界面设置。
        let ui = self.backend.ui_config();
        self.sync_general_selects(cx);
        let mac = cfg!(target_os = "macos");
        let bindings = &self.bindings;
        let editable = EDITABLE_SHORTCUTS.iter().enumerate().map(|(index, shortcut)| {
            let label = t!(shortcut.label).to_string();
            let recording = self.recording == Some(shortcut.action);
            let keys =
                if recording { t!("settings.press_keys").to_string() } else { display_keystroke(&current_keystroke(bindings, shortcut)) };
            let action = shortcut.action;
            h_flex()
                .gap(px(space::M))
                .child(
                    div()
                        .w(px(160.))
                        .flex_none()
                        .text_mono(mono_font(cx))
                        .text_color(if recording { p.accent } else { p.text_secondary })
                        .child(readable_with_id(("shortcut-keys", index), keys.clone())),
                )
                .child(div().flex_1().text_body().child(readable_with_id(("shortcut-label", index), label.clone())))
                .child(crate::design::framed(
                    ("edit-shortcut", index),
                    Button::new(("edit-shortcut", index))
                        .control_xs()
                        .label(if recording { t!("settings.cancel") } else { t!("settings.change") }.to_string())
                        // 读出现在的按键：只听“修改快捷键”不知道它现在是什么。
                        .accessibility_label(
                            t!("a11y.shortcut_row", label = t!("settings.change_shortcut", name = label), keys = keys).to_string(),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.recording = if this.recording == Some(action) { None } else { Some(action) };
                            if this.recording.is_some() {
                                announce(t!("settings.press_keys").to_string(), cx);
                            }
                            // 焦点回到弹窗本身，按键才会被录制而不是触发按钮。
                            this.focus_handle.focus(window, cx);
                            cx.notify();
                        })),
                ))
        });
        let shortcuts = v_flex()
            .gap(px(space::S))
            .children(editable.enumerate().map(|(index, row)| reveal(("reveal-shortcut", index), row)))
            .children(FIXED_SHORTCUTS.iter().enumerate().map(|(index, (keys, mac_keys, label))| {
                let keys = if mac { *mac_keys } else { *keys };
                let label = t!(*label).to_string();
                h_flex()
                    .id(("shortcut", index))
                    .role(Role::Label)
                    .aria_value(format!("{keys}：{label}"))
                    .gap(px(space::M))
                    .child(div().w(px(160.)).flex_none().text_mono(mono_font(cx)).text_color(p.text_secondary).child(keys))
                    .child(div().text_body().child(label))
            }));
        let general = t!("settings.general").to_string();
        let rows = vec![
            setting_row("row-language", t!("settings.language").to_string(), None, self.language.clone(), &p).into_any_element(),
            setting_row("row-theme", t!("settings.theme").to_string(), None, self.theme_select.clone(), &p).into_any_element(),
            setting_row(
                "row-palette",
                t!("settings.palette").to_string(),
                Some(
                    if crate::high_contrast_active(&ui) {
                        t!("settings.palette_locked").to_string()
                    } else {
                        self.palette_error.clone().unwrap_or_else(|| t!("settings.palette_hint").to_string())
                    }
                    .into(),
                ),
                self.palette_select.clone(),
                &p,
            )
            .into_any_element(),
            setting_row(
                "row-close-to-tray",
                t!("settings.close_to_tray").to_string(),
                Some(t!("settings.close_to_tray_hint").to_string().into()),
                {
                    let checked = ui.close_to_tray;
                    crate::design::switch("close-to-tray", checked, &p)
                        .accessibility_label(t!("settings.close_to_tray").to_string())
                        .aria_description(t!("settings.close_to_tray_hint").to_string())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let checked = !checked;
                            if this.backend.update_config(|config| config.ui.close_to_tray = checked).is_ok() {
                                cx.notify();
                            }
                        }))
                },
                &p,
            )
            .into_any_element(),
        ];
        let shortcuts_title = t!("settings.shortcuts").to_string();
        v_flex()
            .gap(px(space::XL))
            .child(section_header(general.clone(), None, &p))
            .child(reveal("reveal-general", setting_list(rows, &p)))
            .child(
                v_flex()
                    .id("shortcuts-section")
                    .role(Role::Group)
                    .aria_label(shortcuts_title.clone())
                    .gap(px(space::M))
                    .child(section_header(shortcuts_title, None, &p))
                    .child(card(&p).p(px(space::XL)).child(shortcuts))
                    .children(
                        self.shortcut_error.clone().map(|error| motion::fade_in(hint(error, Tone::Danger, &p), "shortcut-error-enter")),
                    )
                    .child(
                        h_flex().child(reveal(
                            "reveal-reset-shortcuts",
                            crate::design::framed(
                                "reset-shortcuts",
                                Button::new("reset-shortcuts")
                                    .subtle(cx)
                                    .control_sm()
                                    .label(t!("settings.reset_shortcuts").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.recording = None;
                                        this.save_shortcut(|bindings| bindings.bindings.clear(), cx);
                                    })),
                            ),
                        )),
                    ),
            )
            .into_any_element()
    }

    /// 辅助功能页：高对比度、文字大小、高可见性控件轮廓、减少动态效果。
    fn render_accessibility(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let ui = self.backend.ui_config();
        self.sync_general_selects(cx);
        let section = t!("settings.accessibility").to_string();
        let outlines_hint = t!("settings.high_visibility_outlines_hint").to_string();
        let rows = vec![
            setting_row(
                "row-high-contrast",
                t!("settings.high_contrast").to_string(),
                Some(t!("settings.high_contrast_hint").to_string().into()),
                self.high_contrast_select.clone(),
                &p,
            )
            .into_any_element(),
            setting_row(
                "row-text-scale",
                t!("settings.text_scale").to_string(),
                Some(t!("settings.text_scale_hint").to_string().into()),
                self.text_scale_select.clone(),
                &p,
            )
            .into_any_element(),
            setting_row(
                "row-high-visibility-outlines",
                t!("settings.high_visibility_outlines").to_string(),
                Some(outlines_hint.clone().into()),
                {
                    let checked = ui.high_visibility_outlines;
                    crate::design::switch("high-visibility-outlines", checked, &p)
                        .accessibility_label(t!("settings.high_visibility_outlines").to_string())
                        .aria_description(outlines_hint)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let checked = !checked;
                            if this.backend.update_config(|config| config.ui.high_visibility_outlines = checked).is_ok() {
                                crate::design::set_high_visibility_outlines(checked);
                                crate::sync_theme(cx);
                                cx.refresh_windows();
                            }
                        }))
                },
                &p,
            )
            .into_any_element(),
            setting_row(
                "row-motion",
                t!("settings.motion").to_string(),
                Some(t!("settings.motion_hint").to_string().into()),
                self.motion_select.clone(),
                &p,
            )
            .into_any_element(),
        ];
        v_flex()
            .gap(px(space::XL))
            .child(section_header(section, Some(t!("settings.accessibility_desc").to_string().into()), &p))
            .child(reveal("reveal-accessibility", setting_list(rows, &p)))
            .into_any_element()
    }

    /// 通用、辅助功能两页的下拉框：名称、说明、选项文字随语言变，选中项随配置变（托盘菜单等别处也可能改）。
    ///
    /// 名称只是这一行的标题，不带页名：讲述人先读“减少动态效果，跟随系统，组合框，已折叠”，再读说明。
    fn sync_general_selects(&self, cx: &mut Context<Self>) {
        let ui = self.backend.ui_config();
        let language_label = t!("settings.language").to_string();
        self.language.update(cx, |select, cx| select.set_label(language_label, cx));
        let theme_index = THEMES.iter().position(|theme| *theme == ui.theme);
        self.theme_select.update(cx, |select, cx| {
            select.set_label(t!("settings.theme").to_string(), cx);
            select.set_item_labels(theme_labels(), cx);
            select.set_selected(theme_index, cx);
        });
        // 配色方案同样随语言刷新：内置方案的名字是翻译的。
        let palette_index = self.palette_keys.iter().position(|key| *key == ui.palette).or(Some(0));
        let palette_labels: Vec<SharedString> = std::iter::once(t!("settings.palette_default").to_string().into())
            .chain(self.palette_themes.iter().map(|theme| palette_theme_name(theme).into()))
            .collect();
        let locked = crate::high_contrast_active(&ui).then(|| SharedString::from(t!("settings.palette_locked").to_string()));
        let palette_hint = self.palette_error.clone().unwrap_or_else(|| t!("settings.palette_hint").to_string());
        self.palette_select.update(cx, |select, cx| {
            select.set_disabled(locked, cx);
            select.set_label(t!("settings.palette").to_string(), cx);
            select.set_description(palette_hint, cx);
            select.set_item_labels(palette_labels, cx);
            select.set_selected(palette_index, cx);
        });
        let contrast_index = HIGH_CONTRASTS.iter().position(|choice| *choice == ui.high_contrast);
        self.high_contrast_select.update(cx, |select, cx| {
            select.set_label(t!("settings.high_contrast").to_string(), cx);
            select.set_description(t!("settings.high_contrast_hint").to_string(), cx);
            select.set_item_labels(high_contrast_labels(), cx);
            select.set_selected(contrast_index, cx);
        });
        let scale_index = TEXT_SCALE_CHOICES.iter().position(|choice| *choice == ui.text_scale).or(Some(0));
        self.text_scale_select.update(cx, |select, cx| {
            select.set_label(t!("settings.text_scale").to_string(), cx);
            select.set_description(t!("settings.text_scale_hint").to_string(), cx);
            select.set_item_labels(text_scale_labels(), cx);
            select.set_selected(scale_index, cx);
        });
        let motion_index = MOTIONS.iter().position(|motion| *motion == ui.motion);
        self.motion_select.update(cx, |select, cx| {
            select.set_label(t!("settings.motion").to_string(), cx);
            select.set_description(t!("settings.motion_hint").to_string(), cx);
            select.set_item_labels(motion_labels(), cx);
            select.set_selected(motion_index, cx);
        });
    }

    /// 模型页：最常改的“新会话默认设置”在上面；下面是提供商列表，表单只在添加或编辑时展开，
    /// 编辑时就展开在那个提供商下面。
    fn render_models(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);

        let defaults_title = t!("settings.defaults").to_string();
        let model_label = in_section(&defaults_title, t!("settings.default_model"));
        self.default_model.update(cx, |select, cx| select.set_label(model_label, cx));
        let effort_label_text = in_section(&defaults_title, t!("settings.effort_default"));
        let effort_index = EFFORT_CHOICES.iter().position(|level| *level == self.effort_default);
        self.effort_select.update(cx, |select, cx| {
            select.set_label(effort_label_text, cx);
            select.set_item_labels(EFFORT_CHOICES.into_iter().map(|level| effort_label(level).into()).collect(), cx);
            select.set_selected(effort_index, cx);
        });
        // 名字、选项跟着界面语言重写一遍；选中哪一项由下拉框自己记着。
        self.permission_select.update(cx, |select, cx| {
            select.set_label(in_section(&defaults_title, t!("settings.default_permission")), cx);
            select.set_item_labels(permission_labels(), cx);
        });
        self.preset_select.update(cx, |select, cx| {
            select.set_label(in_section(&defaults_title, t!("settings.default_preset")), cx);
            select.set_item_labels(preset_labels(), cx);
        });
        // 改了就保存，和常规页一样，不再另按“保存”。
        let defaults = v_flex()
            .id("defaults-section")
            .role(Role::Group)
            .aria_label(defaults_title.clone())
            .gap(px(space::M))
            .child(section_header(defaults_title.clone(), None, &p))
            .child(setting_list(
                vec![
                    setting_row("row-default-model", t!("settings.default_model").to_string(), None, self.default_model.clone(), &p)
                        .into_any_element(),
                    setting_row(
                        "row-effort-default",
                        t!("settings.effort_default").to_string(),
                        Some(t!("conversation.effort_hint").to_string().into()),
                        self.effort_select.clone(),
                        &p,
                    )
                    .into_any_element(),
                    setting_row(
                        "row-default-permission",
                        t!("settings.default_permission").to_string(),
                        Some(t!("settings.default_permission_hint").to_string().into()),
                        self.permission_select.clone(),
                        &p,
                    )
                    .into_any_element(),
                    setting_row(
                        "row-default-preset",
                        t!("settings.default_preset").to_string(),
                        Some(t!("settings.default_preset_hint").to_string().into()),
                        self.preset_select.clone(),
                        &p,
                    )
                    .into_any_element(),
                ],
                &p,
            ))
            .children(self.defaults_message.clone().map(|(text, error)| {
                motion::fade_in(hint(text, if error { Tone::Danger } else { Tone::Success }, &p), "defaults-message-enter")
            }));

        // 表单画在它展开的位置（`form_open`），展开、收起即时完成。
        let form = |at: Option<String>| {
            (self.form_open.as_ref() == Some(&at)).then(|| div().pt(px(space::M)).child(panel(&p).child(self.form.clone())))
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        for (index, provider) in self.providers.iter().enumerate() {
            if index > 0 {
                rows.push(divider(&p).into_any_element());
            }
            let config = provider.config.clone();
            let editing = self.form_open.as_ref().is_some_and(|open| open.as_deref() == Some(provider.config.id.as_str()));
            let status = if provider.ready { t!("settings.ready") } else { t!("settings.not_ready") }.to_string();
            let edit_focus = self.edit_provider_focus.get(&provider.config.id).cloned();
            rows.push(
                reveal(
                    ("reveal-provider", index),
                    v_flex()
                        // 带 id，各行里相同的“已就绪”等文字在无障碍树里才不会重复。
                        .id(("provider-row", index))
                        .role(Role::Group)
                        .aria_label(t!("a11y.provider_row", name = provider.name, status = status).to_string())
                        .py(px(space::M))
                        .child(
                            h_flex()
                                .gap(px(space::S))
                                .child(status_dot(provider.ready, &p))
                                .child(div().text_label().child(provider.name.clone()))
                                // 状态不只靠颜色表达。
                                .child(hint(
                                    if provider.ready { t!("settings.ready") } else { t!("settings.not_ready") }.to_string(),
                                    Tone::Neutral,
                                    &p,
                                ))
                                .child(div().flex_1())
                                .when(!editing, |row| {
                                    row.child(crate::design::framed_with(
                                        ("edit-provider", index),
                                        edit_focus.clone(),
                                        Button::new(("edit-provider", index))
                                            .control_sm()
                                            .label(t!("settings.edit").to_string())
                                            // 名称里带上状态：Tab 到按钮上就知道这个提供商能不能用。
                                            .accessibility_label(in_section(t!("settings.edit_provider", name = provider.name), &status))
                                            .when_some(edit_focus, |button, focus| button.track_focus(&focus))
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| this.edit_provider(config.clone(), window, cx)),
                                            ),
                                    ))
                                }),
                        )
                        .children(form(Some(provider.config.id.clone()))),
                )
                .into_any_element(),
            );
        }
        let adding = self.form_open == Some(None);
        let add_form = form(None);
        let empty = self.providers.is_empty().then(|| hint(t!("settings.providers_empty").to_string(), Tone::Neutral, &p));
        let providers_title = t!("settings.providers").to_string();
        // “添加”表单展开在标题正下方、按钮原来的位置附近，而不是列表末尾：看的人不用去找，
        // 焦点也直接放进表单（见 `add_provider`）。
        let add_form = add_form.map(|form| {
            v_flex()
                .id("add-provider-form")
                .role(Role::Group)
                .aria_label(t!("a11y.add_provider_form").to_string())
                .pb(px(space::M))
                .child(form)
        });
        let providers = card(&p)
            .id("providers-section")
            .role(Role::Group)
            .aria_label(providers_title.clone())
            .px(px(space::XL))
            .py(px(space::L))
            .child(
                v_flex()
                    .child(h_flex().pb(px(space::S)).child(div().flex_1().child(section_header(providers_title.clone(), None, &p))).when(
                        !adding,
                        |header| {
                            header.child(reveal(
                                "reveal-add-provider",
                                crate::design::framed_handle(
                                    (self.add_provider_focus).clone(),
                                    Button::new("add-provider")
                                        .control_sm()
                                        .icon(IconName::Plus)
                                        .label(t!("settings.add_provider").to_string())
                                        .track_focus(&self.add_provider_focus)
                                        .on_click(cx.listener(|this, _, window, cx| this.add_provider(window, cx))),
                                )
                                .tint("add-provider"),
                            ))
                        },
                    ))
                    .children(add_form)
                    .children(empty)
                    .children(rows),
            );

        // 插件提供的服务商：加进来之后和别的提供商一样编辑、获取模型。
        let plugin_providers = self.backend.plugin_providers();
        let plugin_section = (!plugin_providers.is_empty()).then(|| {
            let rows = plugin_providers.into_iter().enumerate().map(|(index, (info, added))| {
                let (name, plugin) = (info.name.clone(), info.id.split('/').next().unwrap_or_default().to_owned());
                h_flex()
                    .gap(px(space::M))
                    .py(px(space::XS))
                    .child(v_flex().flex_1().min_w(px(0.)).child(div().text_label().child(name.clone())).child(hint(
                        t!("settings.plugin_provider_from", plugin = plugin, url = info.base_url).to_string(),
                        Tone::Neutral,
                        &p,
                    )))
                    .child(if added {
                        div()
                            .text_caption()
                            .text_color(p.text_secondary)
                            .child(t!("settings.plugin_provider_added").to_string())
                            .into_any_element()
                    } else {
                        crate::design::framed(
                            ("add-plugin-provider", index),
                            Button::new(("add-plugin-provider", index))
                                .control_sm()
                                .label(t!("settings.add_provider").to_string())
                                .accessibility_label(t!("settings.plugin_provider_add_named", name = name).to_string())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if let Err(error) = this.backend.add_plugin_provider(&info) {
                                        tracing::warn!(error, "加不了插件服务商");
                                    }
                                    this.refresh_providers(window, cx);
                                    cx.notify();
                                }))
                                .into_any_element(),
                        )
                        .into_any_element()
                    })
            });
            card(&p).px(px(space::XL)).py(px(space::L)).child(
                v_flex()
                    .gap(px(space::S))
                    .child(section_header(t!("settings.plugin_providers").to_string(), None, &p))
                    .child(hint(t!("settings.plugin_providers_hint").to_string(), Tone::Neutral, &p))
                    .children(rows),
            )
        });

        v_flex()
            .gap(px(space::XL))
            .child(section_header(t!("settings.models").to_string(), Some(t!("settings.models_desc").to_string().into()), &p))
            .child(reveal("reveal-defaults", defaults))
            .child(reveal("reveal-providers", providers))
            .children(plugin_section.map(|section| reveal("reveal-plugin-providers", section)))
            .into_any_element()
    }
}
/// “新会话默认设置”里权限模式的选项顺序（和输入卡片上的权限菜单一样）。
const PERMISSION_CHOICES: [PermissionMode; 4] =
    [PermissionMode::Manual, PermissionMode::Workspace, PermissionMode::AutoReview, PermissionMode::FullAccess];

fn permission_labels() -> Vec<SharedString> {
    PERMISSION_CHOICES.into_iter().map(|mode| permission_label(mode).into()).collect()
}

fn preset_labels() -> Vec<SharedString> {
    Preset::ALL.into_iter().map(|preset| preset_label(preset).into()).collect()
}

const THEMES: [ThemePreference; 3] = [ThemePreference::System, ThemePreference::Light, ThemePreference::Dark];
const MOTIONS: [MotionPreference; 3] = [MotionPreference::System, MotionPreference::Reduce, MotionPreference::Full];

fn theme_labels() -> Vec<SharedString> {
    [t!("settings.theme_system"), t!("settings.theme_light"), t!("settings.theme_dark")]
        .into_iter()
        .map(|label| label.to_string().into())
        .collect()
}

/// 高对比度下拉框的选项，和 [`high_contrast_labels`] 一一对应。
const HIGH_CONTRASTS: [HighContrastPreference; 3] =
    [HighContrastPreference::Off, HighContrastPreference::On, HighContrastPreference::System];

fn high_contrast_labels() -> Vec<SharedString> {
    vec![
        t!("settings.high_contrast_off").to_string().into(),
        t!("settings.high_contrast_on").to_string().into(),
        t!("settings.high_contrast_system").to_string().into(),
    ]
}

/// 文字大小下拉框的选项：跟随系统，再是 [`TEXT_SCALES`] 里的固定大小。和 [`text_scale_labels`] 一一对应。
const TEXT_SCALE_CHOICES: [TextScalePreference; 5] = [
    TextScalePreference::System,
    TextScalePreference::Percent(TEXT_SCALES[0]),
    TextScalePreference::Percent(TEXT_SCALES[1]),
    TextScalePreference::Percent(TEXT_SCALES[2]),
    TextScalePreference::Percent(TEXT_SCALES[3]),
];

fn text_scale_labels() -> Vec<SharedString> {
    TEXT_SCALE_CHOICES
        .iter()
        .map(|choice| {
            match choice {
                TextScalePreference::System => t!("settings.high_contrast_system"),
                TextScalePreference::Percent(90) => t!("settings.text_scale_small"),
                TextScalePreference::Percent(100) => t!("settings.text_scale_default"),
                TextScalePreference::Percent(115) => t!("settings.text_scale_large"),
                TextScalePreference::Percent(_) => t!("settings.text_scale_larger"),
            }
            .to_string()
            .into()
        })
        .collect()
}

/// 配色方案在下拉框里的名字：内置方案（`name` 为空）用当前语言的翻译，文件、插件方案用它们自己的名字。
fn palette_theme_name(theme: &dshr_agent::ThemeDef) -> String {
    if theme.name.is_empty() { t!("settings.palette_high_contrast").to_string() } else { theme.name.clone() }
}

fn motion_labels() -> Vec<SharedString> {
    [t!("settings.theme_system"), t!("settings.motion_reduce"), t!("settings.motion_full")]
        .into_iter()
        .map(|label| label.to_string().into())
        .collect()
}

/// 一项配置：标签在上，控件在下（比页面上的大项矮一级，间距小一些）。
fn field_row(label: String, control: impl IntoElement) -> Div {
    v_flex()
        .gap(px(space::XS))
        .child(div().text_body().child(readable_with_id(SharedString::from(label.clone()), label.clone())))
        .child(reveal(SharedString::from(format!("reveal-{label}")), control))
}

/// 文案里有这个 key 时取它；没有时 `t!` 原样返回 key，这里当作没有。
fn translated(key: &str) -> Option<String> {
    let text = t!(key);
    (text != key).then(|| text.to_string())
}

/// 插件的一句话说明：自带插件写在文案里，别的插件没有。
fn plugin_description(id: &str) -> Option<String> {
    translated(&format!("plugin_desc.{id}"))
}

/// 配置项的名字：内置插件的文案 `plugin_config.<插件>.<键>` 优先；其次是插件自己写的标题（语言文件、
/// `i18n.<语言>.title`、`title`，见 [`Field::label`]）；都没有时直接显示键名。
fn plugin_field_label(plugin: &str, card: Option<&dshr_plugins::PluginCard>, field: &Field) -> String {
    let locale = dshr_plugins::locale::current_locale();
    translated(&format!("plugin_config.{plugin}.{}", field.key))
        .unwrap_or_else(|| field.label(&locale, |key| card.and_then(|card| card.message(&locale, key))))
}

/// 配置项下面的说明：文案（`…_desc`）优先，其次是插件写的说明（按界面语言）；后面跟上范围和默认值。
fn plugin_field_hint(plugin: &str, card: Option<&dshr_plugins::PluginCard>, field: &Field) -> Option<String> {
    let locale = dshr_plugins::locale::current_locale();
    let description = translated(&format!("plugin_config.{plugin}.{}_desc", field.key))
        .or_else(|| field.localized_description(&locale, |key| card.and_then(|card| card.message(&locale, key))));
    let range = match (field.minimum, field.maximum) {
        (Some(minimum), Some(maximum)) => {
            Some(t!("settings.field_range", min = schema_form::format_bound(minimum), max = schema_form::format_bound(maximum)).to_string())
        }
        (Some(minimum), None) => Some(t!("settings.field_min", min = schema_form::format_bound(minimum)).to_string()),
        (None, Some(maximum)) => Some(t!("settings.field_max", max = schema_form::format_bound(maximum)).to_string()),
        (None, None) => None,
    };
    // 开关和选项的默认值看控件就知道；输入框要写出来，留空就是用它。
    let default = matches!(field.kind, FieldKind::Integer | FieldKind::Number | FieldKind::Text)
        .then(|| field.default.as_ref().map(|_| t!("settings.field_default", value = field.display(None)).to_string()))
        .flatten();
    let mut text = String::new();
    for part in [description, range, default].into_iter().flatten() {
        // 英文句子之间空一格；中文句号后面不用。
        if text.chars().next_back().is_some_and(|last| last.is_ascii()) {
            text.push(' ');
        }
        text.push_str(&part);
    }
    (!text.is_empty()).then_some(text)
}

fn field_error_text(error: &FieldError) -> String {
    match error {
        FieldError::NotANumber => t!("settings.field_not_a_number"),
        FieldError::NotAnInteger => t!("settings.field_not_an_integer"),
        FieldError::NotAChoice => t!("settings.field_not_a_choice"),
        FieldError::OutOfRange { minimum, maximum } => match (minimum, maximum) {
            (Some(minimum), Some(maximum)) => {
                t!("settings.field_error_range", min = schema_form::format_bound(*minimum), max = schema_form::format_bound(*maximum))
            }
            (Some(minimum), None) => t!("settings.field_error_min", min = schema_form::format_bound(*minimum)),
            (None, Some(maximum)) => t!("settings.field_error_max", max = schema_form::format_bound(*maximum)),
            (None, None) => t!("settings.field_not_a_number"),
        },
    }
    .to_string()
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::reveal::follow_focus_in(&self.body_scroll, cx);
        let nav = self.render_nav(cx);
        let body = self.render_page(window, cx);
        let p = palette(cx);
        let header = h_flex()
            .px(px(space::XXL))
            .pt(px(space::XL))
            .pb(px(space::M))
            .gap(px(space::S))
            .child(div().flex_1().child(heading(t!("settings.title").to_string(), 1).text_title()))
            .child(crate::design::framed(
                "open-config",
                Button::new("open-config")
                    .control_sm()
                    .label(t!("settings.open_config").to_string())
                    .on_click(cx.listener(|this, _, _, cx| this.open_config_file(cx))),
            ))
            .child(crate::design::framed(
                "close-settings",
                Button::new("close-settings")
                    .subtle(cx)
                    .control_sm()
                    .icon(IconName::Close)
                    .accessibility_label(t!("settings.close").to_string())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Close))),
            ));

        v_flex()
            .id("settings-card")
            // 读屏软件据此播报“设置 对话框”。
            .role(Role::Dialog)
            .aria_label(t!("settings.title").to_string())
            .track_focus(&self.focus_handle)
            // 录制快捷键时先于其它处理拿到按键。
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.recording.is_some() {
                    cx.stop_propagation();
                    this.record_keystroke(event, cx);
                }
            }))
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    cx.emit(SettingsEvent::Close);
                }
            }))
            .occlude()
            .w(px(size::MODAL_WIDTH))
            .max_w(relative(0.94))
            .h(px(size::MODAL_HEIGHT))
            .max_h(relative(0.92))
            .rounded(px(radius::XL))
            .bg(p.raised)
            .border_1()
            .border_color(p.border)
            .text_color(p.text)
            // 卡片淡入并上浮一点，阴影跟着渐显（见 `motion::pop`）；关上时立即移除。
            .map(|card| motion::pop(card, Edge::Bottom, motion::SHORT, self.phase, shadow::modal(&p)))
            .overflow_hidden()
            .child(header)
            .child(
                h_flex().flex_1().min_h(px(0.)).items_start().pl(px(space::S)).child(nav).child(
                    div()
                        .id("settings-body")
                        .track_scroll(&self.body_scroll)
                        .flex_1()
                        .min_w(px(0.))
                        .h_full()
                        .overflow_y_scroll()
                        .pl(px(space::L))
                        .pr(px(space::XXL))
                        .pb(px(space::XXL))
                        // 切换页面即时显示（导航不做动画）。
                        .child(div().w_full().max_w(px(size::MODAL_CONTENT_MAX_WIDTH)).child(body)),
                ),
            )
            .focus_trap("settings-trap", &self.focus_handle)
    }
}

/// 插件列表里状态之外的说明：哪些 JS 插件在等授权、哪些热重载时留着旧版本。打开设置和每次修改后读一次。
struct PluginNotes {
    grants: Vec<dshr_plugins::GrantRequest>,
    kept: Vec<(String, String)>,
    stopped: Vec<(String, String)>,
}

impl PluginNotes {
    fn read(backend: &Backend) -> Self {
        Self { grants: backend.grant_requests(), kept: backend.kept_plugins(), stopped: backend.stopped_plugins() }
    }

    /// (没能激活的原因, 还没允许的能力, 仍在用旧版本的原因, 被停下的原因)。在等授权时只显示授权那一块：
    /// “需要授权”的失败原因和“新版本加载不了”说的都是同一件事。
    fn of(&self, id: &str, status: &Status) -> (Option<String>, Vec<String>, Option<String>, Option<String>) {
        let grant = self.grants.iter().find(|request| request.id == id).map(|request| request.missing.clone()).unwrap_or_default();
        let problem = match status {
            Status::Active | Status::Disabled => None,
            Status::Failed(_) if !grant.is_empty() => None,
            Status::Failed(error) => Some(t!("settings.plugin_failed", error = error).to_string()),
            Status::Pending(missing) => Some(t!("settings.plugin_pending", services = missing.join(", ")).to_string()),
        };
        let kept = if grant.is_empty() { self.kept.iter().find(|(kept, _)| kept == id).map(|(_, reason)| reason.clone()) } else { None };
        let stopped = self.stopped.iter().find(|(stopped, _)| stopped == id).map(|(_, reason)| reason.clone());
        (problem, grant, kept, stopped)
    }
}

/// 设置弹窗的页：界面插件登记的（插件藏起来的 `settings:<id>` 除外），再加上 JS 插件加的页，
/// 最后是固定的“关于”页（不能被插件隐藏）。
fn settings_pages(backend: &Arc<Backend>, cx: &App) -> Vec<SettingsPage> {
    let hidden = backend.hidden_entries();
    let mut pages: Vec<SettingsPage> =
        UiRegistry::global(cx).settings_pages().iter().filter(|page| !hidden.contains(&format!("settings:{}", page.id))).cloned().collect();
    for surface in backend.surfaces() {
        let entries = surface.entries().iter().filter(|entry| entry.place == dshr_agent::EntryPlace::Settings);
        for entry in entries.filter(|entry| !surface.entry_hidden(entry.place, &entry.id)) {
            let (plugin, page, label) = (surface.plugin().to_owned(), entry.page.clone(), entry.label.clone());
            pages.push(SettingsPage {
                id: intern(&format!("plugin:{plugin}:{}", entry.id)),
                icon: crate::plugin_page::entry_icon(entry.icon.as_deref()).into(),
                label: Rc::new(move || label.clone()),
                content: PageContent::View(Rc::new(move |backend, window, cx| {
                    let (plugin, page) = (plugin.clone(), page.clone());
                    let backend = backend.clone();
                    cx.new(|cx| crate::plugin_page::PluginPageView::new(backend, plugin, page, None, window, cx)).into()
                })),
            });
        }
    }
    pages.push(SettingsPage {
        id: "about",
        icon: IconName::Info.into(),
        label: Rc::new(|| t!("settings.about").to_string()),
        content: PageContent::Builtin(SettingsView::render_about),
    });
    pages
}

/// 设置页的 id 是 `&'static str`，插件的页运行时才知道：每个不同的 id 只泄漏一次。
fn intern(id: &str) -> &'static str {
    use std::sync::Mutex;
    static IDS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut ids = IDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = ids.iter().find(|existing| **existing == id) {
        return existing;
    }
    let leaked: &'static str = Box::leak(id.to_owned().into_boxed_str());
    ids.push(leaked);
    leaked
}
