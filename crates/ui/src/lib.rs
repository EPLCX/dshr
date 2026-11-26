//! 桌面 UI。信息结构参照原版，视觉按 `docs/design.md` 的设计规范实现：
//! - 欢迎窗：还没有可用凭据时先显示；
//! - 工作区窗口：左侧边栏（新会话、按工作目录分组的会话、设置、账号），右侧对话区；设置以弹窗形式打开。
//!   每个打开的会话有自己的对话视图，切到别的会话时正在运行的那个在后台继续。
//! - 关闭工作区窗口默认隐藏到系统托盘（[`tray`]）。

mod about;
mod assets;
pub use assets::AppAssets;
mod announce;
mod backend;
mod bridge;
mod cards;
mod conversation;
mod design;
mod dropdown;
mod install_dialog;
mod native_theme;
mod permissions;
mod plugin;
mod plugin_page;
mod provider_form;
mod reveal;
mod schema_form;
mod session_search;
mod settings;
pub mod single_instance;
mod smooth_scroll;
pub mod startup;
mod system_a11y;
mod title_bar;
mod touch;
mod transcript;
mod tray;
mod welcome;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use dshr_agent::SessionSummary;
use dshr_config::{MotionPreference, ThemePreference};
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, IconName, Sizable as _, Theme, ThemeMode, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::design::ControlSize as _;

pub use backend::Backend;
pub use cards::{AccountUi, FsUi, PermissionUi, PlanUi, PresetsUi, builtin_ui_plugins};
pub use conversation::{ConversationEvent, ConversationView};
pub use design::Palette;
use design::{
    Tone, Typography as _, brand_mark, heading, hint, hover_fill, list_item, menu_popover, motion, palette, pressable, radius,
    readable_with_id, shadow, size, space, status_dot,
};
pub use plugin::{
    ClickHandler, ComposerControl, ComposerPanel, PageBuilder, PageContent, PanelContext, RowData, SettingsPage, SidebarAction,
    SidebarItem, StatusItem, StatusLine, ToolCard, TranscriptRenderer, UiPlugin, UiRegistry, UiScope, ViewState,
};
use settings::SettingsEvent;
pub use settings::{SettingsUi, SettingsView};
pub use transcript::{ExtRow, ToolRow, ToolStatus};
use tray::TrayCommand;
use welcome::{WELCOME_SIZE, WelcomeView};

rust_i18n::i18n!("locales", fallback = "zh-CN");

/// 桌面版对用户显示的名称。
pub const APP_NAME: &str = "DSHR Desktop";

/// 支持的界面语言：(代码, 显示名)。
pub(crate) const LANGUAGES: &[(&str, &str)] = &[
    ("zh-CN", "中文（简体）"),
    ("zh-HK", "中文（粵語）"),
    ("zh-TW", "中文（繁體）"),
    ("en", "English"),
    ("de", "Deutsch"),
    ("el", "Ελληνικά"),
    ("ja", "日本語"),
    ("es", "Español"),
    ("fr", "Français"),
    ("pt", "Português"),
];

const WORKSPACE_SIZE: (f32, f32) = (1200., 800.);
/// 工作区窗口能缩到的最小尺寸：侧边栏加上一列能放下输入卡片的对话区。
const WORKSPACE_MIN_SIZE: (f32, f32) = (870., 520.);
/// 默认尺寸放不下时，窗口最多占屏幕可用区域的比例。
const MAX_SCREEN_FRACTION: f32 = 0.85;

/// 启动时系统的“减少动态效果”设置（gpui-base 初始化时读入）。用户在设置里切回“跟随系统”时恢复它。
struct SystemMotion(bool);

impl Global for SystemMotion {}

/// 工作区窗口；托盘菜单通过它恢复窗口、新建会话。
struct MainWindow {
    handle: AnyWindowHandle,
    view: Entity<WorkspaceView>,
}

impl Global for MainWindow {}

/// 应用入口：装配界面插件、设置语言与动效偏好、注册快捷键、装托盘图标，按凭据情况打开欢迎窗或工作区窗口。
///
/// `ui_plugins` 是界面这一半的插件，一般是 [`builtin_ui_plugins`] 再加上主程序自己的；
/// 后端那一半没有激活的不会装上。
pub fn launch(backend: Arc<Backend>, workspace: Option<PathBuf>, ui_plugins: Vec<Box<dyn UiPlugin>>, cx: &mut App) {
    startup::mark("launch: 开始");
    UiRegistry::assemble(&ui_plugins, |id| backend.plugin_status(id)).install(cx);
    apply_language(&backend.file_config().ui.language);
    cx.set_global(SystemMotion(cx.reduce_motion()));
    apply_motion(backend.file_config().ui.motion, cx);
    design::set_high_visibility_outlines(backend.file_config().ui.high_visibility_outlines);
    design::set_text_scale(text_scale_percent(backend.file_config().ui.text_scale));
    migrate_high_contrast_palette(&backend);
    bind_keys(&backend.keybindings(), cx);
    startup::mark("launch: 注册表/语言/快捷键完");
    // 插件替用户发消息、写输入框、发通知、装插件前确认……都经过这座桥。
    bridge::install(backend.clone());
    if let Err(error) = apply_palette(&backend, cx) {
        tracing::warn!(error, "配色方案用不了，改用默认配色");
    }
    dropdown::init(cx);
    design::init(cx);
    conversation::init(cx);
    startup::mark("launch: 桥/配色/组件初始化完");
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
    // 没有托盘（如系统不支持）时关闭窗口就退出，不影响其它功能。
    if let Err(error) = tray::install(cx, handle_tray_command) {
        eprintln!("{APP_NAME}：无法创建托盘图标：{error}");
    }
    startup::mark("launch: 托盘完");
    if backend.needs_welcome() {
        startup::mark("launch: 开欢迎窗");
        open_welcome_window(backend, workspace, cx);
    } else {
        startup::mark("launch: 开工作区窗");
        open_workspace_window(backend, workspace, cx);
    }
    startup::mark("launch: 窗口已打开");
    cx.activate(true);
}

/// 恢复隐藏到托盘的窗口（macOS 点击程序坞图标时由入口调用）。
pub fn show_main_window(cx: &mut App) {
    handle_tray_command(TrayCommand::Show, cx);
}

fn handle_tray_command(command: TrayCommand, cx: &mut App) {
    if command == TrayCommand::Quit {
        cx.quit();
        return;
    }
    let Some((handle, view)) = cx.try_global::<MainWindow>().map(|main| (main.handle, main.view.clone())) else {
        // 还在欢迎窗：把它调到前台。
        if let Some(window) = cx.windows().first().copied() {
            window.update(cx, |_, window, cx| tray::show_window(window, cx)).ok();
        }
        return;
    };
    handle
        .update(cx, |_, window, cx| {
            tray::show_window(window, cx);
            view.update(cx, |view, cx| match command {
                TrayCommand::NewSession => view.new_session(None, window, cx),
                _ => view.focus_active(window, cx),
            });
        })
        .ok();
}

/// 窗口选项：默认尺寸超出屏幕可用区域时按比例缩小，避免一打开就撑满屏幕。
fn window_options(size: (f32, f32), min_size: Option<(f32, f32)>, resizable: bool, cx: &App) -> WindowOptions {
    let available = cx.primary_display().map(|display| display.visible_bounds().size);
    let fit =
        |wanted: f32, available: Option<Pixels>| available.map_or(wanted, |available| wanted.min(available.as_f32() * MAX_SCREEN_FRACTION));
    let width = fit(size.0, available.map(|size| size.width));
    let height = fit(size.1, available.map(|size| size.height));
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, gpui_kit::size(px(width), px(height)), cx))),
        // Windows 上标题栏自己画（客户区延伸到标题栏），见 `title_bar`。
        titlebar: Some(title_bar::options()),
        app_owns_titlebar_drag: title_bar::app_owns_drag(),
        is_resizable: resizable,
        window_min_size: min_size.map(|(width, height)| gpui_kit::size(px(width), px(height))),
        // Windows：先建成隐藏的窗口，等 GPUI 装好无障碍适配器再显示（见 `show_when_accessible`）。
        show: !cfg!(windows),
        ..Default::default()
    }
}

/// 显示用 [`window_options`] 打开的窗口。
///
/// Windows 上 GPUI 在创建窗口时（`CreateWindowExW` 之后、`SetWindowPlacement`）就把它显示出来，无障碍适配器
/// 要到 `open_window` 稍后的 `a11y_init` 才装上。讲述人开着时，窗口一出现它就来要无障碍树（`WM_GETOBJECT`），
/// 这时还没有适配器，系统给它一个只有窗口本身、没有任何控件的默认对象，讲述人记住这个对象，什么都读不到，
/// 直到窗口隐藏再显示（最小化再恢复）才重新来要。AccessKit 也要求窗口在适配器建好之后再显示。
/// 所以 Windows 上窗口先不显示，`open_window` 返回（适配器已经装好）之后再在这里显示并激活。
/// 其它平台窗口打开时就已经显示，这里什么也不做。
fn show_when_accessible(handle: AnyWindowHandle, cx: &mut App) {
    if cfg!(windows) {
        handle.update(cx, |_, window, _| window.activate_window()).ok();
    }
}

fn open_welcome_window(backend: Arc<Backend>, workspace: Option<PathBuf>, cx: &mut App) {
    let theme = backend.file_config().ui.theme;
    let (handle, _) = gpui_kit::open_window(window_options(WELCOME_SIZE, None, false, cx), cx, |window, cx| {
        apply_theme(theme, window, cx);
        touch::install(window, cx);
        cx.new(|cx| WelcomeView::new(backend, workspace, window, cx))
    })
    .expect("无法打开欢迎窗口");
    show_when_accessible(handle, cx);
}

pub(crate) fn open_workspace_window(backend: Arc<Backend>, workspace: Option<PathBuf>, cx: &mut App) {
    let theme = backend.file_config().ui.theme;
    let options = window_options(WORKSPACE_SIZE, Some(WORKSPACE_MIN_SIZE), true, cx);
    let (handle, view) = gpui_kit::open_window(options, cx, |window, cx| {
        apply_theme(theme, window, cx);
        touch::install(window, cx);
        let close_backend = backend.clone();
        // 关闭按钮默认只是隐藏到托盘：后台的会话继续运行，从托盘恢复。
        window.on_window_should_close(cx, move |window, cx| {
            if close_backend.file_config().ui.close_to_tray && tray::is_installed(cx) {
                tray::hide_window(window, cx);
                false
            } else {
                true
            }
        });
        cx.new(|cx| WorkspaceView::new(backend, workspace, window, cx))
    })
    .expect("无法打开工作区窗口");
    show_when_accessible(handle, cx);
    cx.set_global(MainWindow { handle, view });
}

pub(crate) fn apply_motion(preference: MotionPreference, cx: &mut App) {
    let reduce = match preference {
        MotionPreference::System => cx.try_global::<SystemMotion>().is_some_and(|system| system.0),
        MotionPreference::Reduce => true,
        MotionPreference::Full => false,
    };
    cx.set_reduce_motion(reduce);
    cx.refresh_windows();
}

actions!(
    dshr,
    [
        /// 新会话。
        NewSession,
        /// 打开或关闭设置。
        ToggleSettings,
        /// 把焦点放回输入框。
        FocusComposer,
        /// 收起或展开侧边栏。
        ToggleSidebar,
        /// 按标题搜索会话。
        SearchSessions,
    ]
);

/// 可以在设置里修改的快捷键。
pub(crate) struct EditableShortcut {
    /// `keybindings.json` 里的动作名。
    pub action: &'static str,
    /// 文案 key。
    pub label: &'static str,
    pub mac: &'static str,
    pub other: &'static str,
}

pub(crate) const EDITABLE_SHORTCUTS: &[EditableShortcut] = &[
    EditableShortcut { action: "new_session", label: "shortcuts.new_session", mac: "cmd-n", other: "ctrl-n" },
    EditableShortcut { action: "settings", label: "shortcuts.settings", mac: "cmd-,", other: "ctrl-," },
    EditableShortcut { action: "focus_composer", label: "shortcuts.focus_composer", mac: "cmd-l", other: "ctrl-l" },
    EditableShortcut { action: "toggle_sidebar", label: "shortcuts.toggle_sidebar", mac: "cmd-b", other: "ctrl-b" },
    EditableShortcut { action: "search_sessions", label: "shortcuts.search_sessions", mac: "cmd-k", other: "ctrl-k" },
];

/// 生效中的按键：用户改过的（且写法有效）优先，否则用默认值。
pub(crate) fn current_keystroke(bindings: &dshr_config::Keybindings, shortcut: &EditableShortcut) -> String {
    let default = if cfg!(target_os = "macos") { shortcut.mac } else { shortcut.other };
    bindings.get(shortcut.action).filter(|keys| is_valid_keystroke(keys)).unwrap_or(default).to_owned()
}

/// 注册全局快捷键。设置里改了之后再次调用：先移除本程序的旧绑定，组件库自带的绑定保持不变。
/// （`KeyBinding::new` 不认 `secondary-` 别名，按平台直接写修饰键。）
pub(crate) fn bind_keys(bindings: &dshr_config::Keybindings, cx: &mut App) {
    let keymap = cx.key_bindings();
    let others: Vec<KeyBinding> = keymap
        .borrow()
        .bindings()
        .filter(|binding| {
            let action = binding.action().as_any();
            !(action.is::<NewSession>()
                || action.is::<ToggleSettings>()
                || action.is::<FocusComposer>()
                || action.is::<ToggleSidebar>()
                || action.is::<SearchSessions>())
        })
        .cloned()
        .collect();
    {
        let mut keymap = keymap.borrow_mut();
        keymap.clear();
        keymap.add_bindings(others);
    }
    // `KeyBinding::new` 要 `&'static str`；只在启动和用户改快捷键时调用，泄漏的几个短字符串可以忽略。
    let key = |index: usize| -> &'static str { Box::leak(current_keystroke(bindings, &EDITABLE_SHORTCUTS[index]).into_boxed_str()) };
    cx.bind_keys([
        KeyBinding::new(key(0), NewSession, None),
        KeyBinding::new(key(1), ToggleSettings, None),
        KeyBinding::new(key(2), FocusComposer, None),
        KeyBinding::new(key(3), ToggleSidebar, None),
        KeyBinding::new(key(4), SearchSessions, None),
    ]);
}

const MODIFIERS: &[&str] = &["ctrl", "alt", "shift", "cmd", "fn"];

/// 手改过的 `keybindings.json` 可能写错；写错的项按默认值处理，避免启动时崩溃。
fn is_valid_keystroke(keys: &str) -> bool {
    let Some((key, modifiers)) = split_keystroke(keys) else {
        return false;
    };
    !key.is_empty() && !modifiers.is_empty() && modifiers.iter().all(|modifier| MODIFIERS.contains(modifier))
}

/// `ctrl-shift--` 这样的写法里最后一个 `-` 是按键本身。
fn split_keystroke(keys: &str) -> Option<(&str, Vec<&str>)> {
    let (modifiers, key) = match keys.strip_suffix("--") {
        Some(rest) => (rest, "-"),
        None => keys.rsplit_once('-')?,
    };
    Some((key, modifiers.split('-').collect()))
}

/// 录制到的按键转成 GPUI 写法；只按了修饰键或没有修饰键时返回 `None`（全局快捷键至少要一个修饰键）。
pub(crate) fn keystroke_string(keystroke: &Keystroke) -> Option<String> {
    let modifiers = &keystroke.modifiers;
    let key = keystroke.key.as_str();
    if matches!(key, "control" | "alt" | "shift" | "platform" | "function" | "") || !modifiers.modified() {
        return None;
    }
    let mut parts = Vec::new();
    for (on, name) in [
        (modifiers.control, "ctrl"),
        (modifiers.alt, "alt"),
        (modifiers.shift, "shift"),
        (modifiers.platform, "cmd"),
        (modifiers.function, "fn"),
    ] {
        if on {
            parts.push(name);
        }
    }
    parts.push(key);
    Some(parts.join("-"))
}

/// 显示用：macOS 用符号（⌘⇧N），其它平台用 Ctrl+Shift+N。
pub(crate) fn display_keystroke(keys: &str) -> String {
    let Some((key, modifiers)) = split_keystroke(keys) else {
        return keys.to_owned();
    };
    let mac = cfg!(target_os = "macos");
    let mut parts: Vec<String> = modifiers
        .iter()
        .map(|modifier| {
            match (*modifier, mac) {
                ("ctrl", true) => "⌃",
                ("alt", true) => "⌥",
                ("shift", true) => "⇧",
                ("cmd", true) => "⌘",
                ("ctrl", false) => "Ctrl",
                ("alt", false) => "Alt",
                ("shift", false) => "Shift",
                ("cmd", false) => "Win",
                (other, _) => other,
            }
            .to_owned()
        })
        .collect();
    let mut chars = key.chars();
    let key = match (chars.next(), chars.next()) {
        (Some(c), None) => c.to_uppercase().to_string(),
        _ => key[..1].to_uppercase() + &key[1..],
    };
    parts.push(key);
    parts.join(if mac { "" } else { "+" })
}

/// 设置页“键盘快捷键”列表里只展示、不能修改的条目：(按键, macOS 按键, 文案 key)。
pub(crate) const FIXED_SHORTCUTS: &[(&str, &str, &str)] = &[
    ("Enter", "Enter", "shortcuts.send"),
    ("Shift+Enter", "Shift+Enter", "shortcuts.newline"),
    ("Tab / Shift+Tab", "Tab / Shift+Tab", "shortcuts.tab"),
    ("Enter / Space", "Enter / Space", "shortcuts.activate"),
    ("Esc", "Esc", "shortcuts.escape"),
];

pub(crate) fn apply_language(code: &str) {
    rust_i18n::set_locale(code);
    // 插件读 `host.i18n.locale`、设置页按语言显示插件配置项的标题，都看这个。
    dshr_plugins::locale::set_locale(code);
}

/// 套用配置里选的配色方案（`ui.palette`）；空的或找不到时用默认配色。方案不合规（颜色写错、对比度太低）时报错，
/// 也用默认配色。
pub(crate) fn apply_palette(backend: &Backend, cx: &mut App) -> Result<(), String> {
    let ui = backend.ui_config();
    let high_contrast = high_contrast_active(&ui);
    cx.set_global(SystemHighContrast(system_a11y::high_contrast()));
    let key = ui.palette;
    // 打开了高对比度（或跟随系统、系统开着）：用内置的高对比度配色，不管配色方案选的是什么。
    let theme = if high_contrast {
        Some(backend::builtin_high_contrast_theme())
    } else {
        (!key.is_empty()).then(|| backend.themes().into_iter().find(|theme| Backend::theme_key(theme) == key)).flatten()
    };
    let result = match theme {
        Some(theme) => design::PaletteOverride::parse(&theme.light, &theme.dark).map(|mut over| {
            over.high_contrast = high_contrast;
            over
        }),
        None => Ok(design::PaletteOverride::default()),
    };
    let result = match result {
        Ok(over) => {
            cx.set_global(over);
            Ok(())
        }
        Err(error) => {
            cx.set_global(design::PaletteOverride::default());
            Err(error)
        }
    };
    sync_theme(cx);
    result
}

/// 以前高对比度是配色方案里的一项；现在只在“辅助功能”里开关。选过它的，改成“开启高对比度 + 默认配色”。
fn migrate_high_contrast_palette(backend: &Backend) {
    let key = Backend::theme_key(&backend::builtin_high_contrast_theme());
    if backend.file_config().ui.palette == key {
        backend
            .update_config(|config| {
                config.ui.palette.clear();
                if config.ui.high_contrast == dshr_config::HighContrastPreference::Off {
                    config.ui.high_contrast = dshr_config::HighContrastPreference::On;
                }
            })
            .ok();
    }
}

/// 文字大小的百分比：固定值，或跟随系统时系统现在的文字大小。
pub(crate) fn text_scale_percent(preference: dshr_config::TextScalePreference) -> u16 {
    match preference {
        dshr_config::TextScalePreference::System => system_a11y::text_scale(),
        dshr_config::TextScalePreference::Percent(percent) => percent,
    }
}

/// 高对比度现在是否生效：开启，或跟随系统且系统开着。
pub(crate) fn high_contrast_active(ui: &dshr_config::UiConfig) -> bool {
    match ui.high_contrast {
        dshr_config::HighContrastPreference::On => true,
        dshr_config::HighContrastPreference::Off => false,
        dshr_config::HighContrastPreference::System => system_a11y::high_contrast(),
    }
}

/// 上次读到的系统高对比度设置（跟随系统时，变了才重新套配色）。
struct SystemHighContrast(bool);

impl Global for SystemHighContrast {}

/// 配色方案、高对比度变了之后：重新套配色，再按明暗重新载入主题（组件库的颜色先回到原样，
/// 再由 [`sync_theme`] 按新的配色覆盖）。
pub(crate) fn refresh_palette(backend: &Backend, window: &mut Window, cx: &mut App) -> Result<(), String> {
    let result = apply_palette(backend, cx);
    apply_theme(backend.ui_config().theme, window, cx);
    result
}

/// 高对比度、文字大小跟随系统时，系统的设置可能变了（窗口外观变化、回到前台时调用）：变了就重新套上。
pub(crate) fn follow_system_settings(backend: &Backend, window: &mut Window, cx: &mut App) {
    let ui = backend.ui_config();
    if ui.text_scale == dshr_config::TextScalePreference::System {
        let before = design::text_scale();
        design::set_text_scale(system_a11y::text_scale());
        if design::text_scale() != before {
            sync_theme(cx);
            cx.refresh_windows();
        }
    }
    // 减少动态效果：组件库只在启动时读一次系统设置（Windows、macOS 改了不通知）。只在“跟随系统”时重读：
    // 选了“减少”或“完整”时用户的选择优先，不能让系统设置覆盖。
    if ui.motion == dshr_config::MotionPreference::System {
        let before = cx.reduce_motion();
        gpui_kit::base::apply_system_reduce_motion(cx);
        let now = cx.reduce_motion();
        cx.set_global(SystemMotion(now));
        if now != before {
            cx.refresh_windows();
        }
    }
    if ui.high_contrast != dshr_config::HighContrastPreference::System {
        return;
    }
    let now = system_a11y::high_contrast();
    if cx.try_global::<SystemHighContrast>().is_some_and(|last| last.0 == now) {
        return;
    }
    refresh_palette(backend, window, cx).ok();
    cx.refresh_windows();
}

/// 把辅助功能设置同步到组件库的主题上。配色方案、明暗、高可见性轮廓、文字大小任何一个变了都要调用
/// （切换明暗会重新载入主题，这些改动要重新加上）。
///
/// - 组件库自己的控件（开关、输入框、组件库按钮）用主题的 `ring` 画焦点框：跟着配色方案的焦点色走；
///   高可见性轮廓开启时改用正文色，和 [`design::pressable`] 的外圈一样对比最强。
/// - 组件库的字号（和窗口的 rem）按文字大小缩放：输入框、组件库按钮、Markdown 回复里的字跟着变大。
pub(crate) fn sync_theme(cx: &mut App) {
    if !cx.has_global::<Theme>() {
        return;
    }
    let p = design::palette(cx);
    let ring = if design::high_visibility_outlines() { p.text } else { p.focus_ring };
    let scale = design::text_scale();
    let (font, mono) = (px((16. * scale).round()), px((13. * scale).round()));
    let theme = gpui_kit::component::ActiveTheme::theme(cx);
    let dark = theme.is_dark();
    // 用了配色方案（包括高对比度）时，组件库自己的控件（开关、输入框、组件库按钮、Markdown 里的文字）
    // 也换成这套颜色。不然它们还是默认配色：高对比度下输入框的边框只有 1.2:1，几乎看不见。
    let overridden = cx.try_global::<design::PaletteOverride>().is_some_and(|over| !over.is_empty(dark));
    let theme = gpui_kit::component::ActiveTheme::theme(cx);
    let needs_colors = overridden
        && (theme.foreground != p.text || theme.background != p.surface || theme.border != p.border || theme.switch != p.text_secondary);
    let needs_controls = !design::control_theme_applied(&p, theme);
    if theme.ring != ring || theme.font_size != font || theme.mono_font_size != mono || needs_colors || needs_controls {
        Theme::update(cx, |theme| {
            if overridden {
                design::apply_palette_to_theme(&p, &mut theme.colors);
            }
            // 圆角、按钮与输入框的状态色：所有主题都按调色板设（`Theme::change` 每次都会把它们换回原始值）。
            design::control_theme(&p, theme);
            theme.colors.ring = ring;
            theme.font_size = font;
            theme.mono_font_size = mono;
        });
    }
}

/// 主题是全局的，切换后所有窗口一起变。“跟随系统”按窗口当前外观决定明暗。
/// 系统画的部分（标题栏、托盘菜单）也一起改：程序里手动选了深色，系统是浅色时它们也是深色。
pub(crate) fn apply_theme(preference: ThemePreference, window: &mut Window, cx: &mut App) {
    let mode = match preference {
        ThemePreference::Light => ThemeMode::Light,
        ThemePreference::Dark => ThemeMode::Dark,
        ThemePreference::System => match window.appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeMode::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeMode::Light,
        },
    };
    Theme::change(mode, Some(window), cx);
    if mode.is_dark() {
        Theme::update(cx, design::soften_dark);
    }
    sync_theme(cx);
    let dark = mode.is_dark();
    native_theme::set_menus(dark);
    native_theme::set_title_bar(window, dark);
    let current = window.window_handle();
    for handle in cx.windows() {
        if handle != current {
            handle.update(cx, |_, other, _| native_theme::set_title_bar(other, dark)).ok();
        }
    }
    window.refresh();
}

/// 侧边栏底部的状态行：各界面插件的 `StatusItem` 现在的状态。
/// 插件藏起来的（`status:<id>`）不读。
fn read_status(ui: &UiRegistry, backend: &Backend) -> Vec<StatusLine> {
    let hidden = backend.hidden_entries();
    ui.status_items()
        .iter()
        .filter(|item| !hidden.contains(&format!("status:{}", item.id)))
        .filter_map(|item| (item.read)(backend))
        .collect()
}

/// 插件加在侧边栏的一个入口。
struct PluginEntry {
    plugin: String,
    entry: dshr_agent::UiEntry,
}

/// 侧边栏入口打开的插件页面，以弹窗盖在工作区上。
struct OpenPluginPage {
    title: String,
    view: Entity<plugin_page::PluginPageView>,
    /// 弹窗自己的焦点：打开时移进来，Tab 只在弹窗里循环，Esc 才能收到。
    focus: FocusHandle,
}

/// 排队等着显示的安装确认：请求和回答用的发送端。
type PendingInstall = (Box<dshr_plugins::app::InstallRequest>, std::sync::mpsc::SyncSender<dshr_plugins::app::InstallAnswer>);

/// 打开着的安装确认。
struct OpenInstallDialog {
    view: Entity<install_dialog::InstallDialog>,
    /// 打开前焦点在哪里（设置弹窗里的“安装”按钮等）：回答后还回去。
    restore: Option<FocusHandle>,
    _subscription: Subscription,
}

/// 插件在状态栏放了内容时，多久重画一次状态行（内容可能随时间变化）。
const STATUS_REFRESH: std::time::Duration = std::time::Duration::from_secs(10);

/// 快捷键的统一写法：`ctrl-alt-shift-cmd-<键>`（只写出按下的修饰键）。
fn shortcut_key(key: &str, control: bool, alt: bool, shift: bool, platform: bool) -> String {
    let mut parts = Vec::new();
    for (on, name) in [(control, "ctrl"), (alt, "alt"), (shift, "shift"), (platform, "cmd")] {
        if on {
            parts.push(name);
        }
    }
    let key = key.to_ascii_lowercase();
    parts.push(&key);
    parts.join("-")
}

/// 把 `Ctrl+Shift+U`、`ctrl-shift-u`、`cmd-k` 这类写法换成 [`shortcut_key`] 的样子；写错的是 `None`。
fn normalize_shortcut(keys: &str) -> Option<String> {
    let parts: Vec<String> = keys.split(['-', '+']).map(|part| part.trim().to_ascii_lowercase()).filter(|part| !part.is_empty()).collect();
    let (key, modifiers) = parts.split_last()?;
    let has = |names: &[&str]| modifiers.iter().any(|modifier| names.contains(&modifier.as_str()));
    Some(shortcut_key(
        key,
        has(&["ctrl", "control"]),
        has(&["alt", "option"]),
        has(&["shift"]),
        has(&["cmd", "super", "win", "platform", "secondary"]),
    ))
}

/// 节点树里的第一段文字：状态行用它。
fn first_text(tree: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    match tree {
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("text" | "badge" | "markdown") => object.get("text").and_then(Value::as_str).map(str::to_owned),
            _ => object.get("children").and_then(Value::as_array)?.iter().find_map(first_text),
        },
        _ => None,
    }
}

/// 一个打开着的对话视图。
struct OpenConversation {
    view: Entity<ConversationView>,
    /// 上次通知侧边栏时的（运行中，等待审批）；变化时才重绘侧边栏，流式输出不会带着整个窗口重绘。
    state: (bool, bool),
    _subscriptions: [Subscription; 2],
}

/// 侧边栏里的一个工作区分组。
struct WorkspaceGroup {
    /// `None` 是没有记录工作目录的早期会话（“其他会话”）。
    path: Option<PathBuf>,
    /// 在 `self.sessions` 里的下标，最近修改的在前。
    sessions: Vec<usize>,
    /// 用户选过、记在配置里的工作区；没有会话时可以从列表移除。
    remembered: bool,
}

/// 侧边栏的外壳：内容仍由 [`WorkspaceView`] 生成（状态都在那里），但作为独立的视图就可以被缓存——
/// 对话区流式输出、每帧重绘时，侧边栏沿用上一帧的结果，只有它自己的内容变了才重新生成。
struct SidebarView {
    workspace: WeakEntity<WorkspaceView>,
}

impl Render for SidebarView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.workspace
            .update(cx, |workspace, cx| workspace.render_sidebar(window, cx).into_any_element())
            .unwrap_or_else(|_| div().into_any_element())
    }
}

/// 标题栏里的会话标题和运行状态。内容仍由 [`WorkspaceView`] 生成，但单独做成一个视图：运行中的转圈是循环动画，
/// 每一帧通知它所在的视图。直接画在工作区视图里时，被通知的是工作区视图，再经它的 `observe_self` 让侧边栏
/// 重新生成——一轮运行多久，侧边栏（连同其中每一行会话）就每秒重建 30 次。放在子视图里，动画只通知这个子视图；
/// 工作区视图作为祖先照样重画，但不会触发 `observe_self`，缓存着的侧边栏沿用上一帧。
struct TitleView {
    workspace: WeakEntity<WorkspaceView>,
}

impl Render for TitleView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.workspace.update(cx, |workspace, cx| workspace.render_title(cx)).unwrap_or_else(|_| div().into_any_element())
    }
}

/// 侧边栏每个工作区分组一开始显示几个会话；更早的点“显示更多”再显示这么多。侧边栏不是虚拟列表，
/// 每次重画（悬停、菜单、会话列表刷新）都要为显示着的每一行建元素：会话攒到几百个时不能全画出来。
const SIDEBAR_PAGE: usize = 30;

pub struct WorkspaceView {
    backend: Arc<Backend>,
    /// 标题栏里的标题（见 [`TitleView`]）。
    title: Entity<TitleView>,
    sessions: Vec<SessionSummary>,
    /// 每次刷新会话列表加一；后台读完时如果已经有更新的刷新在路上，就丢掉这次的结果。
    sessions_generation: u64,
    sidebar: Entity<SidebarView>,
    /// 当前显示的对话，加上在后台运行的对话。空闲的后台对话会被释放，再打开时从日志重建。
    conversations: Vec<OpenConversation>,
    active: Entity<ConversationView>,
    /// 新会话输入卡片上的模式、权限、模型：空会话被切走时、新会话发出第一条消息时记下，
    /// 之后每个新会话都沿用（只在这次运行期间有效，不写盘；关掉再开回到配置里的默认值）。
    blank_options: Option<backend::TurnOptions>,
    /// 空会话被切走时输入框里没发出去的字，切回空会话时放回。
    blank_draft: String,
    /// 各个会话输入框里没发出去的字，按会话 id：切走（视图可能随后被释放）再切回来时放回。只在内存里。
    drafts: HashMap<String, String>,
    /// 折叠起来的工作区分组。
    collapsed: HashSet<Option<PathBuf>>,
    /// 侧边栏收起成一列图标按钮（`ui.sidebar_collapsed`，重启后保留）。
    sidebar_collapsed: bool,
    /// 侧边栏现在的宽度（收起、展开时有过渡）：每帧在 `render` 里算好，侧边栏视图按它画。
    sidebar_width: std::cell::Cell<f32>,
    /// 打开着的“搜索会话”弹窗。
    session_search: Option<(Entity<session_search::SessionSearchView>, Subscription)>,
    /// 点过“显示更多”的分组现在显示几个会话；没有记录的分组显示 [`SIDEBAR_PAGE`] 个。
    sidebar_limits: HashMap<Option<PathBuf>, usize>,
    /// 侧边栏会话列表的滚动：键盘焦点移到看不见的行时把它滚进来（见 `reveal::reveal_in`）。
    sidebar_scroll: ScrollHandle,
    /// 打开设置时存在；关闭时丢弃，下次打开重新读取配置。
    settings: Option<(Entity<SettingsView>, Subscription)>,
    /// 插件加在侧边栏的入口，和上次读它们时插件的组成。
    plugin_entries: Vec<PluginEntry>,
    plugins_generation: usize,
    /// JS 插件的状态行（`ui.status`）：(在 `plugin_entries` 里的下标, 页面画出来的第一段文字)。在后台画。
    plugin_status: Vec<(usize, String)>,
    /// 打开着的插件页面。
    plugin_page: Option<OpenPluginPage>,
    /// 正在显示的安装确认（盖在设置、插件页面之上）。
    install_dialog: Option<OpenInstallDialog>,
    /// 排队等着显示的安装确认（同时来了好几个时一个一个问）。
    install_queue: std::collections::VecDeque<PendingInstall>,
    /// 上次设给系统的窗口标题（任务栏、Alt+Tab、读屏软件读的）；变了才重新设。
    window_title: String,
    /// 界面插件登记的侧边栏入口和状态行。
    ui: Rc<UiRegistry>,
    /// 侧边栏底部的状态行；打开窗口和保存设置时各读一次，不在每帧读。
    status: Vec<StatusLine>,
    /// 正在打开“…”菜单的会话。
    session_menu: Option<String>,
    /// 点过一次“删除”、等待确认的会话。
    pending_delete: Option<String>,
    /// 正在原地重命名的会话。
    renaming: Option<(String, Entity<InputState>, [Subscription; 2])>,
    /// 最近一次侧边栏操作失败的原因。
    sidebar_error: Option<String>,
    /// 整个窗口的平滑滚动与中键自动滚动。
    scroll: smooth_scroll::SmoothScroll,
    /// 窗口里最先的两个 Tab 停靠点：“跳到对话内容”“跳到输入框”，平时看不见，键盘走到时才显示。
    /// 不用先 Tab 过侧边栏里一长串会话、标题栏和消息列表。
    skip_content: FocusHandle,
    skip_input: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl WorkspaceView {
    pub fn new(backend: Arc<Backend>, workspace: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 没有从命令行指定目录时，接着用最近一次的工作区，打开就能直接输入、发送。
        let workspace = workspace.or_else(|| backend.file_config().ui.workspaces.into_iter().find(|path| path.is_dir()));
        if let Some(path) = &workspace {
            backend.remember_workspace(path).ok();
        }
        let sidebar_collapsed = backend.file_config().ui.sidebar_collapsed;
        let first = Self::open_conversation(&backend, workspace, window, cx);
        let active = first.view.clone();
        // 等窗口第一次绘制后再把焦点放进输入框；构造期间设的焦点在窗口激活时会丢。
        cx.defer_in(window, |this, window, cx| this.focus_active(window, cx));
        let workspace_view = cx.weak_entity();
        let title = cx.new(|_| TitleView { workspace: workspace_view.clone() });
        let sidebar = cx.new(|_| SidebarView { workspace: workspace_view });
        let ui = UiRegistry::global(cx);
        let subscriptions = vec![
            // 回到前台时看一眼系统的高对比度设置（跟随系统时）：在系统设置里改完切回来就生效。
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    follow_system_settings(&this.backend, window, cx);
                }
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                follow_system_settings(&this.backend, window, cx);
                let theme = this.backend.ui_config().theme;
                // GPUI 在通知之后才按系统外观重设标题栏：推迟到那之后再按程序里的选择设一遍，
                // 否则手动选了深色、系统切到浅色时标题栏会变回浅色。
                cx.defer_in(window, move |_, window, cx| apply_theme(theme, window, cx));
            }),
            // 侧边栏是缓存的视图：本视图的状态一变就让它重新生成。
            cx.observe_self(|this, cx| this.sidebar.update(cx, |_, cx| cx.notify())),
        ];
        let mut view = Self {
            // 会话列表要读磁盘，在后台加载，窗口先显示出来。
            sessions: Vec::new(),
            sessions_generation: 0,
            title,
            sidebar,
            status: read_status(&ui, &backend),
            ui,
            backend,
            conversations: vec![first],
            active,
            blank_options: None,
            blank_draft: String::new(),
            drafts: HashMap::new(),
            collapsed: HashSet::new(),
            sidebar_collapsed,
            sidebar_width: std::cell::Cell::new(if sidebar_collapsed { size::SIDEBAR_RAIL_WIDTH } else { size::SIDEBAR_WIDTH }),
            session_search: None,
            sidebar_limits: HashMap::new(),
            sidebar_scroll: ScrollHandle::new(),
            settings: None,
            plugin_entries: Vec::new(),
            plugins_generation: 0,
            plugin_status: Vec::new(),
            plugin_page: None,
            install_dialog: None,
            install_queue: Default::default(),
            window_title: String::new(),
            session_menu: None,
            pending_delete: None,
            renaming: None,
            sidebar_error: None,
            scroll: Default::default(),
            skip_content: cx.focus_handle(),
            skip_input: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        let mut title_changes = dshr_agent::title_changes();
        cx.spawn(async move |this, cx| {
            while title_changes.changed().await.is_ok() {
                if this
                    .update(cx, |this, cx| {
                        for open in &this.conversations {
                            open.view.update(cx, |view, cx| view.refresh_title(cx));
                        }
                        this.refresh_sessions(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        view.refresh_sessions(cx);
        view.refresh_plugin_entries(cx);
        // 插件经桥发来的请求（替用户发消息、写输入框、确认安装……）：来了就被叫醒，在主线程上处理。
        // 订阅之前已经排着的请求先处理一遍。
        let mut wakes = bridge::UiBridge::global().subscribe();
        cx.spawn_in(window, async move |this, cx| {
            use futures::StreamExt as _;
            if this.update_in(cx, |this, window, cx| this.process_bridge(window, cx)).is_err() {
                return;
            }
            while wakes.next().await.is_some() {
                if this.update_in(cx, |this, window, cx| this.process_bridge(window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        // 插件变了（装上、卸下、热重载、启动后在后台加载完）时刷新入口、状态行和输入卡片。等的是通知，
        // 没有变化时不会被叫醒；只有插件在状态栏放了内容时，才每隔 STATUS_REFRESH 重画一次状态行。
        let mut changes = view.backend.plugin_changes();
        cx.spawn(async move |this, cx| {
            use futures::FutureExt as _;
            loop {
                let Ok(has_status) =
                    this.update(cx, |this, _| this.plugin_entries.iter().any(|entry| entry.entry.place == dshr_agent::EntryPlace::Status))
                else {
                    break;
                };
                let changed = if has_status {
                    futures::select_biased! {
                        changed = changes.changed().fuse() => Some(changed.is_ok()),
                        _ = cx.background_executor().timer(STATUS_REFRESH).fuse() => None,
                    }
                } else {
                    Some(changes.changed().await.is_ok())
                };
                let updated = this.update(cx, |this, cx| match changed {
                    None => this.refresh_plugin_status(cx),
                    Some(_) if this.backend.plugins_generation() != this.plugins_generation => {
                        this.refresh_plugin_entries(cx);
                        this.status = read_status(&this.ui, &this.backend);
                        // 选的是插件带的配色方案时，插件刚装上（启动后在后台加载）才找得到它。
                        if !this.backend.ui_config().palette.is_empty() {
                            let _ = apply_palette(&this.backend, cx);
                            cx.refresh_windows();
                        }
                        for open in &this.conversations {
                            open.view.update(cx, |_, cx| cx.notify());
                        }
                        cx.notify();
                    }
                    Some(_) => {}
                });
                if updated.is_err() || changed == Some(false) {
                    break;
                }
            }
        })
        .detach();
        view
    }

    /// 处理插件经桥发来的请求，再写一份会话和输入框的快照给插件读。
    fn process_bridge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let bridge = bridge::UiBridge::global();
        for request in bridge.take() {
            match request {
                bridge::BridgeRequest::Send(request) => {
                    let target = match &request.target {
                        dshr_plugins::app::SessionTarget::Current => Some(self.active.clone()),
                        dshr_plugins::app::SessionTarget::Id(id) => self.find_conversation(id, cx),
                        dshr_plugins::app::SessionTarget::New => {
                            // 新会话在后台开：不打断用户正在看的对话，侧边栏里能看到它在运行。
                            let workspace = request.workspace.clone().or_else(|| self.active.read(cx).workspace().cloned());
                            let open = Self::open_conversation(&self.backend, workspace, window, cx);
                            let view = open.view.clone();
                            self.conversations.push(open);
                            Some(view)
                        }
                    };
                    match target {
                        Some(view) => view.update(cx, |view, cx| view.plugin_send(request.text, &request.plugin, cx)),
                        None => {
                            tracing::warn!(plugin = request.plugin, "插件要发消息的会话没有打开")
                        }
                    }
                }
                bridge::BridgeRequest::Stop(session) => {
                    let view = match session {
                        Some(id) => self.find_conversation(&id, cx),
                        None => Some(self.active.clone()),
                    };
                    if let Some(view) = view {
                        view.update(cx, |view, cx| view.plugin_stop(cx));
                    }
                }
                bridge::BridgeRequest::SetComposer(text) => {
                    self.active.update(cx, |view, cx| view.set_composer_text(&text, window, cx));
                }
                bridge::BridgeRequest::OpenUrl(url) => cx.open_url(&url),
                bridge::BridgeRequest::Install(request, answer) => {
                    self.install_queue.push_back((request, answer));
                    self.show_next_install(window, cx);
                }
            }
        }
        self.publish_bridge_snapshot(cx);
    }

    /// 给插件读的快照：打开着的会话、输入框里的字。每次重画时写（窗口重画说明有东西变了），
    /// 处理完插件的请求、会话列表变了（窗口藏在托盘里不重画时）也写。
    fn publish_bridge_snapshot(&self, cx: &App) {
        let bridge = bridge::UiBridge::global();
        let active = self.active.clone();
        let sessions = self
            .conversations
            .iter()
            .filter_map(|open| {
                let view = open.view.read(cx);
                Some(dshr_plugins::app::OpenSession {
                    id: view.session_id()?.to_owned(),
                    title: view.title(),
                    workspace: view.workspace().map(|path| path.display().to_string()),
                    running: view.is_running(),
                    current: open.view == active,
                })
            })
            .collect();
        let composer = Some(self.active.read(cx).composer_text(cx));
        bridge.set_snapshot(sessions, composer);
    }

    fn refresh_plugin_entries(&mut self, cx: &mut Context<Self>) {
        self.plugins_generation = self.backend.plugins_generation();
        self.plugin_entries = self
            .backend
            .surfaces()
            .iter()
            .flat_map(|surface| {
                let plugin = surface.plugin().to_owned();
                surface
                    .entries()
                    .iter()
                    .filter(|entry| matches!(entry.place, dshr_agent::EntryPlace::Sidebar | dshr_agent::EntryPlace::Status))
                    .map(move |entry| PluginEntry { plugin: plugin.clone(), entry: entry.clone() })
                    .collect::<Vec<_>>()
            })
            .collect();
        self.refresh_plugin_status(cx);
        cx.notify();
    }

    /// 在后台画一遍 JS 插件的状态行页面，取每页的第一段文字。
    fn refresh_plugin_status(&mut self, cx: &mut Context<Self>) {
        let wanted: Vec<(usize, String, String)> = self
            .plugin_entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.entry.place == dshr_agent::EntryPlace::Status)
            .map(|(index, entry)| (index, entry.plugin.clone(), entry.entry.page.clone()))
            .collect();
        if wanted.is_empty() {
            if !self.plugin_status.is_empty() {
                self.plugin_status.clear();
                cx.notify();
            }
            return;
        }
        let backend = self.backend.clone();
        let workspace = self.active.read(cx).workspace().cloned();
        cx.spawn(async move |this, cx| {
            let lines = cx
                .background_executor()
                .spawn(async move {
                    wanted
                        .into_iter()
                        .map(|(index, plugin, page)| {
                            let text = backend
                                .surface(&plugin)
                                .ok_or_else(String::new)
                                .and_then(|surface| surface.render(&page, workspace.as_deref()))
                                .map_or_else(|error| error, |tree| first_text(&tree).unwrap_or_default());
                            (index, text)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                if this.plugin_status != lines {
                    this.plugin_status = lines;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn open_plugin_page(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.plugin_entries.get(index) else {
            return;
        };
        let (plugin, page, title) = (entry.plugin.clone(), entry.entry.page.clone(), entry.entry.label.clone());
        let workspace = self.active.read(cx).workspace().cloned();
        let backend = self.backend.clone();
        let view = cx.new(|cx| plugin_page::PluginPageView::new(backend, plugin, page, workspace, window, cx));
        self.close_settings(window, cx);
        // 焦点移进弹窗：否则焦点留在侧边栏，Esc 关不掉，Tab 会走到遮罩后面的控件，读屏软件也不知道弹窗开了。
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.plugin_page = Some(OpenPluginPage { title, view, focus });
        cx.notify();
    }

    /// 按下的键是不是某个插件命令的快捷键；是就执行它。
    fn on_plugin_shortcut(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = &event.keystroke.modifiers;
        if !(modifiers.control || modifiers.alt || modifiers.platform) {
            return;
        }
        let pressed = shortcut_key(&event.keystroke.key, modifiers.control, modifiers.alt, modifiers.shift, modifiers.platform);
        let builtin = self.backend.keybindings();
        if builtin.bindings.values().any(|keys| normalize_shortcut(keys).as_deref() == Some(pressed.as_str())) {
            return;
        }
        for surface in self.backend.surfaces() {
            let hit = surface
                .commands()
                .iter()
                .find(|command| command.keys.as_deref().and_then(normalize_shortcut).as_deref() == Some(pressed.as_str()))
                .map(|command| command.id.clone());
            if let Some(id) = hit {
                let plugin = surface.plugin().to_owned();
                self.active.update(cx, |view, cx| view.run_command(plugin, id, window, cx));
                cx.stop_propagation();
                return;
            }
        }
    }

    /// 以弹窗打开插件的一页（命令的 `{ open }`、通知点开）。
    fn open_page(&mut self, plugin: String, page: String, title: String, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.active.read(cx).workspace().cloned();
        let backend = self.backend.clone();
        let view = cx.new(|cx| plugin_page::PluginPageView::new(backend, plugin, page, workspace, window, cx));
        self.close_settings(window, cx);
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.plugin_page = Some(OpenPluginPage { title, view, focus });
        cx.notify();
    }

    /// 没有正在显示的安装确认时，显示队列里的下一个。
    fn show_next_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.install_dialog.is_some() {
            return;
        }
        let Some((request, answer)) = self.install_queue.pop_front() else {
            return;
        };
        let restore = window.focused(cx);
        let view = cx.new(|cx| install_dialog::InstallDialog::new(*request, answer, window, cx));
        let subscription = cx.subscribe_in(&view, window, |this, _, event: &install_dialog::InstallDialogEvent, window, cx| match event {
            install_dialog::InstallDialogEvent::Closed => this.close_install_dialog(window, cx),
        });
        self.install_dialog = Some(OpenInstallDialog { view, restore, _subscription: subscription });
        cx.notify();
    }

    /// 安装确认回答完了：关上，焦点还回去，再看队列里还有没有。
    fn close_install_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.install_dialog.take() else {
            return;
        };
        match open.restore {
            Some(handle) => handle.focus(window, cx),
            None => self.focus_active(window, cx),
        }
        // 关上即移除，下一个直接弹出。
        if !self.install_queue.is_empty() {
            self.show_next_install(window, cx);
        }
        cx.notify();
    }

    fn close_plugin_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugin_page.take().is_some() {
            // 关上即移除，焦点回到输入框。
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn open_conversation(
        backend: &Arc<Backend>,
        workspace: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> OpenConversation {
        let view = cx.new(|cx| ConversationView::new(backend.clone(), workspace, window, cx));
        let events = cx.subscribe_in(&view, window, |this, _, event: &ConversationEvent, window, cx| match event {
            ConversationEvent::SessionsChanged => {
                this.refresh_sessions(cx);
                this.publish_bridge_snapshot(cx);
                // 后台的一轮结束后释放它（延后到事件处理完，不在它自己的回调里丢掉它）。
                let workspace = cx.entity().downgrade();
                cx.defer(move |cx| {
                    workspace.update(cx, |this, cx| this.prune(cx)).ok();
                });
            }
            ConversationEvent::WorkspaceChosen(path) => {
                if let Err(error) = this.backend.remember_workspace(path) {
                    this.sidebar_error = Some(t!("sidebar.action_failed", error = error).to_string());
                }
                cx.notify();
            }
            ConversationEvent::OpenPluginPage { plugin, page, title } => {
                this.open_page(plugin.clone(), page.clone(), title.clone(), window, cx);
            }
            ConversationEvent::NewSessionStarted(options) => {
                this.blank_options = Some(options.clone());
            }
        });
        let observer = cx.observe(&view, |this, view, cx| {
            let state = {
                let conversation = view.read(cx);
                (conversation.is_running(), conversation.needs_attention())
            };
            if let Some(open) = this.conversations.iter_mut().find(|open| open.view == view)
                && open.state != state
            {
                open.state = state;
                cx.notify();
            }
        });
        OpenConversation { view, state: (false, false), _subscriptions: [events, observer] }
    }

    pub(crate) fn focus_active(&self, window: &mut Window, cx: &mut App) {
        self.active.update(cx, |conversation, cx| conversation.focus_input(window, cx));
    }

    fn activate(&mut self, view: Entity<ConversationView>, window: &mut Window, cx: &mut Context<Self>) {
        // 切走时记下输入卡片的状态，切回时恢复（只在这次运行期间，不写盘）：
        // 空会话记模式、权限、模型和没发出去的字；已有会话只记字（选项已经写在它的日志里）。
        if self.active != view {
            self.stash_composer(cx);
            self.restore_composer(&view, window, cx);
        }
        self.active = view;
        self.prune(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// 记下当前对话输入卡片上的状态（见 [`Self::activate`]）。
    fn stash_composer(&mut self, cx: &App) {
        let conversation = self.active.read(cx);
        let draft = conversation.draft(cx);
        if conversation.is_blank() {
            self.blank_options = Some(conversation.turn_options().clone());
            self.blank_draft = draft;
        } else if let Some(id) = conversation.session_id() {
            if draft.is_empty() {
                self.drafts.remove(id);
            } else {
                self.drafts.insert(id.to_owned(), draft);
            }
        }
    }

    /// 把记下的状态放回要切到的对话。
    fn restore_composer(&mut self, view: &Entity<ConversationView>, window: &mut Window, cx: &mut Context<Self>) {
        let conversation = view.read(cx);
        if conversation.is_blank() {
            let options = self.blank_options.clone();
            let draft = std::mem::take(&mut self.blank_draft);
            view.update(cx, |conversation, cx| {
                if let Some(options) = options {
                    conversation.set_turn_options(options, cx);
                }
                conversation.restore_draft(&draft, window, cx);
            });
        } else if let Some(draft) = conversation.session_id().and_then(|id| self.drafts.remove(id)) {
            view.update(cx, |conversation, cx| conversation.restore_draft(&draft, window, cx));
        }
    }

    /// 只保留当前对话和正在运行的对话。
    fn prune(&mut self, cx: &mut Context<Self>) {
        let active = self.active.clone();
        self.conversations.retain(|open| open.view == active || open.view.read(cx).is_running());
    }

    fn find_conversation(&self, session_id: &str, cx: &App) -> Option<Entity<ConversationView>> {
        self.conversations.iter().map(|open| &open.view).find(|view| view.read(cx).session_id() == Some(session_id)).cloned()
    }

    /// 某个会话的（运行中，等待审批）状态。
    fn session_state(&self, session_id: &str, cx: &App) -> (bool, bool) {
        self.find_conversation(session_id, cx).map_or((false, false), |view| {
            let conversation = view.read(cx);
            (conversation.is_running(), conversation.needs_attention())
        })
    }

    fn open_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.find_conversation(id, cx) {
            self.activate(view, window, cx);
            return;
        }
        let workspace = self
            .sessions
            .iter()
            .find(|session| session.id == id)
            .and_then(|session| session.workspace.clone())
            .or_else(|| self.active.read(cx).workspace().cloned());
        let open = Self::open_conversation(&self.backend, workspace, window, cx);
        open.view.update(cx, |conversation, cx| conversation.open_session(id, cx));
        let view = open.view.clone();
        self.conversations.push(open);
        self.activate(view, window, cx);
    }

    /// 新会话：`workspace` 为空时沿用当前对话的工作区。正在运行的会话不受影响。
    pub(crate) fn new_session(&mut self, workspace: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_settings(window, cx);
        let workspace = workspace.or_else(|| self.active.read(cx).workspace().cloned());
        let blank = self.conversations.iter().map(|open| open.view.clone()).find(|view| view.read(cx).is_blank());
        let view = match blank {
            Some(view) => {
                if let Some(path) = workspace {
                    view.update(cx, |conversation, cx| conversation.set_workspace(path, cx));
                }
                view
            }
            None => {
                let open = Self::open_conversation(&self.backend, workspace, window, cx);
                let view = open.view.clone();
                self.conversations.push(open);
                view
            }
        };
        self.activate(view, window, cx);
    }

    /// 选一个目录加进工作区列表，并在里面开一个新会话。
    fn add_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("sidebar.add_workspace").to_string().into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = picked.await
                && let Some(path) = paths.pop()
            {
                this.update_in(cx, |this, window, cx| {
                    if let Err(error) = this.backend.remember_workspace(&path) {
                        this.sidebar_error = Some(t!("sidebar.action_failed", error = error).to_string());
                    }
                    this.new_session(Some(path), window, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// 在后台线程重新读取会话列表，读完再更新侧边栏；界面线程不等磁盘。
    fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        self.sessions_generation += 1;
        let generation = self.sessions_generation;
        let backend = self.backend.clone();
        let listing = cx.background_spawn(async move { backend.list_sessions() });
        cx.spawn(async move |this, cx| {
            let sessions = listing.await;
            this.update(cx, |this, cx| {
                if this.sessions_generation == generation {
                    this.sessions = sessions;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.take().is_some() {
            // 关闭弹窗后把焦点还给输入框，否则键盘用户会“丢失”焦点。
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn on_new_session(&mut self, _: &NewSession, window: &mut Window, cx: &mut Context<Self>) {
        self.new_session(None, window, cx);
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_sidebar(cx);
    }

    fn on_search_sessions(&mut self, _: &SearchSessions, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_search.is_some() {
            self.close_session_search(window, cx);
        } else {
            self.open_session_search(window, cx);
        }
    }

    /// 收起或展开侧边栏，记进配置。
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        let collapsed = self.sidebar_collapsed;
        if let Err(error) = self.backend.update_config(|config| config.ui.sidebar_collapsed = collapsed) {
            self.sidebar_error = Some(t!("sidebar.action_failed", error = error).to_string());
        }
        self.session_menu = None;
        cx.notify();
    }

    /// 打开“搜索会话”弹窗：拿当前的会话列表，焦点移进输入框。
    fn open_session_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_search.is_some() {
            return;
        }
        let sessions = self.sessions.clone();
        let view = cx.new(|cx| session_search::SessionSearchView::new(sessions, window, cx));
        let subscription = cx.subscribe_in(&view, window, |this, _, event: &session_search::SessionSearchEvent, window, cx| match event {
            session_search::SessionSearchEvent::Open(id) => {
                let id = id.clone();
                this.close_session_search(window, cx);
                this.open_session(&id, window, cx);
            }
            session_search::SessionSearchEvent::Close => this.close_session_search(window, cx),
        });
        // 焦点等弹窗画出来再进搜索框（见 `SessionSearchView::focus_pending`）。
        self.session_search = Some((view, subscription));
        cx.notify();
    }

    fn close_session_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_search.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn on_toggle_settings(&mut self, _: &ToggleSettings, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.close_settings(window, cx);
        } else {
            self.open_settings(window, cx);
        }
    }

    /// “跳到对话内容”“跳到输入框”：Tab 顺序里排在整个窗口最前面（`tab_index(-1)`），画在最上层。
    /// 没有焦点时移到窗口外面：仍在 Tab 顺序里，但看不见，鼠标也点不到。
    fn render_skip_links(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let p = palette(cx);
        let shown = self.skip_content.is_focused(window) || self.skip_input.is_focused(window);
        let link = |id: &'static str, label: String, focus: &FocusHandle| {
            design::pressable(id, &p)
                .track_focus(focus)
                .tab_index(-1)
                .accessibility_label(label.clone())
                .h(px(size::CONTROL_MD))
                .px(px(space::L))
                .rounded(px(radius::MD))
                .bg(p.raised)
                .border_color(p.border)
                .shadow(design::shadow::menu(&p))
                .text_label()
                .cursor_pointer()
                .child(label)
        };
        h_flex()
            .absolute()
            .top(px(space::S))
            .when(shown, |links| links.left(px(space::S)))
            .when(!shown, |links| links.left(px(-10_000.)))
            .gap(px(space::S))
            .child(link("skip-to-content", t!("a11y.skip_to_content").to_string(), &self.skip_content).on_click(cx.listener(
                |this, _, window, cx| {
                    this.close_settings(window, cx);
                    this.active.update(cx, |view, cx| view.focus_content(window, cx));
                },
            )))
            .child(link("skip-to-input", t!("a11y.skip_to_input").to_string(), &self.skip_input).on_click(cx.listener(
                |this, _, window, cx| {
                    this.close_settings(window, cx);
                    this.focus_active(window, cx);
                },
            )))
    }

    fn on_focus_composer(&mut self, _: &FocusComposer, window: &mut Window, cx: &mut Context<Self>) {
        self.close_settings(window, cx);
        self.focus_active(window, cx);
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn start_rename(&mut self, id: String, title: String, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(title));
        let session_id = id.clone();
        let subscription = cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.finish_rename(&session_id, cx);
            }
        });
        // 侧边栏是缓存的视图，输入框的状态不是视图：光标、选区变化时手动让侧边栏重绘。
        let repaint = cx.observe(&input, |this, _, cx| this.sidebar.update(cx, |_, cx| cx.notify()));
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.sidebar_error = None;
        self.session_menu = None;
        self.renaming = Some((id, input, [subscription, repaint]));
        cx.notify();
    }

    fn finish_rename(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some((renaming_id, input, _)) = self.renaming.take() else {
            return;
        };
        if renaming_id != id {
            return;
        }
        let title = input.read(cx).value().trim().to_owned();
        let unchanged = self.sessions.iter().any(|session| session.id == id && session.title == title);
        if title.is_empty() || unchanged {
            self.refresh_sessions(cx);
            return;
        }
        // 先把新标题显示出来。写日志放到后台：没打开的会话要先把整份日志扫一遍才能接着写（几十 MB 的日志
        // 要几十毫秒），不能卡住界面。写完（或失败）再从磁盘刷新列表。
        if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
            session.title = title.clone();
        }
        cx.notify();
        let (backend, id) = (self.backend.clone(), id.to_owned());
        let renamed = cx.background_executor().spawn(async move { backend.rename_session(&id, &title) });
        cx.spawn(async move |this, cx| {
            let result = renamed.await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.sidebar_error = Some(t!("sidebar.action_failed", error = error).to_string());
                }
                this.refresh_sessions(cx);
            })
            .ok();
        })
        .detach();
    }

    fn delete_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_error = None;
        self.pending_delete = None;
        self.session_menu = None;
        if let Some(view) = self.find_conversation(id, cx) {
            if view.read(cx).is_running() {
                self.sidebar_error = Some(t!("sidebar.busy").to_string());
                cx.notify();
                return;
            }
            // 先释放这个会话的视图和日志文件句柄（Windows 上打开的文件所在目录无法移动）。
            if view == self.active {
                self.new_session(None, window, cx);
            }
            self.conversations.retain(|open| open.view != view);
        }
        // 列表先把这一行去掉。真正移动日志放到这次更新之后：GPUI 在一轮更新结束时才释放没人引用的视图，
        // 视图里的会话（和它开着的日志文件）这时才关掉；Windows 上目录里有打开的文件时移动会被拒绝。
        // 后台任务可能还握着句柄一小会儿，所以被拒绝时稍等再试几次。
        self.sessions.retain(|session| session.id != id);
        self.drafts.remove(id);
        cx.notify();
        let (backend, id) = (self.backend.clone(), id.to_owned());
        cx.spawn(async move |this, cx| {
            let mut result = backend.delete_session(&id);
            for _ in 0..10 {
                if result.is_ok() {
                    break;
                }
                cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
                result = backend.delete_session(&id);
            }
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.sidebar_error = Some(t!("sidebar.action_failed", error = error).to_string());
                }
                this.refresh_sessions(cx);
            })
            .ok();
        })
        .detach();
    }

    fn forget_workspace(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Err(error) = self.backend.forget_workspace(path) {
            self.sidebar_error = Some(t!("sidebar.action_failed", error = error).to_string());
        }
        cx.notify();
    }

    /// 按工作目录归类会话：先是记住的工作区（最近使用的在前），再是只在会话里出现过的目录，
    /// 最后是没有记录目录的“其他会话”。
    fn groups(&self) -> Vec<WorkspaceGroup> {
        let mut groups: Vec<WorkspaceGroup> = self
            .backend
            .workspaces()
            .into_iter()
            .map(|path| WorkspaceGroup { path: Some(path), sessions: Vec::new(), remembered: true })
            .collect();
        for (index, session) in self.sessions.iter().enumerate() {
            match groups.iter_mut().find(|group| group.path == session.workspace) {
                Some(group) => group.sessions.push(index),
                None => groups.push(WorkspaceGroup { path: session.workspace.clone(), sessions: vec![index], remembered: false }),
            }
        }
        groups.sort_by_key(|group| group.path.is_none());
        groups
    }

    /// 会话行右侧的“…”菜单：重命名、删除（点两次确认）。
    fn render_session_menu(&self, index: usize, session: &SessionSummary, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let id = session.id.clone();
        let title = session.title.clone();
        let touch = design::touch_mode(cx);
        let open = self.session_menu.as_deref() == Some(id.as_str());
        let confirming = self.pending_delete.as_deref() == Some(id.as_str());
        let selected = self.active.read(cx).session_id() == Some(id.as_str());
        let view = cx.entity().downgrade();
        let menu_id = id.clone();
        // 菜单打开的那一刻焦点留在这个按钮上，菜单画出来再移进去（见 `menu_popover`）。
        let trigger_focus = crate::design::menu_trigger_focus(("session-menu", index), window, cx);
        let trigger = crate::design::framed_trigger_with(
            trigger_focus.clone(),
            Button::new(("session-more", index))
                .track_focus(&trigger_focus)
                .ghost()
                .control_xs()
                .icon(IconName::Ellipsis)
                .accessibility_label(t!("sidebar.session_actions", title = session.title).to_string()),
        );
        // 菜单向下展开。
        let body = move |menu: &design::MenuFocus, _: &mut Window, cx: &mut App| {
            let p = palette(cx);
            let (rename_view, delete_view) = (view.clone(), view.clone());
            let (rename_id, delete_id, title) = (id.clone(), id.clone(), title.clone());
            let delete_label = if confirming { t!("sidebar.confirm_delete") } else { t!("sidebar.delete") };
            let rename = design::menu_item("session-rename", None, t!("sidebar.rename").to_string(), false, &p)
                .focus_on_hover()
                .track_focus(&menu.item(0, cx))
                .on_click(move |_, window, cx| {
                    let (id, title) = (rename_id.clone(), title.clone());
                    rename_view.update(cx, |this, cx| this.start_rename(id, title, window, cx)).ok();
                });
            v_flex()
                .min_w(px(200.))
                .p(px(space::XS))
                .child(rename)
                .child(
                    design::menu_item("session-delete", None, delete_label.to_string(), false, &p)
                        .focus_on_hover()
                        .track_focus(&menu.item(1, cx))
                        .text_color(p.danger)
                        .on_click(move |_, window, cx| {
                            let id = delete_id.clone();
                            delete_view
                                .update(cx, |this, cx| {
                                    if this.pending_delete.as_deref() == Some(id.as_str()) {
                                        this.delete_session(&id, window, cx);
                                    } else {
                                        this.pending_delete = Some(id);
                                        cx.notify();
                                    }
                                })
                                .ok();
                        }),
                )
                .into_any_element()
        };
        let popover = menu_popover(
            ("session-menu", index),
            open,
            Anchor::TopRight,
            trigger,
            cx.listener(move |this, open: &bool, _, cx| {
                this.session_menu = open.then(|| menu_id.clone());
                if !*open {
                    this.pending_delete = None;
                }
                cx.notify();
            }),
            0,
            body,
            window,
            cx,
        );
        // 平时淡一些，悬停该行时完全显示；当前会话和菜单打开时始终完全显示。
        // 不能完全透明：按钮仍在 Tab 顺序里，键盘走到这里时只看到焦点框围着一块空白（按焦点显示不可靠：
        // 侧边栏是缓存的视图，焦点变了不一定重绘）。触屏上没有悬停，一直完全显示。
        div()
            .flex_none()
            .when(!open && !selected && !touch, |slot| slot.opacity(0.45).group_hover("session-row", |style| style.opacity(1.)))
            .child(popover)
            .into_any_element()
    }

    fn render_session_row(&self, index: usize, session: &SessionSummary, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        if let Some((renaming_id, input, _)) = &self.renaming
            && *renaming_id == session.id
        {
            let id = renaming_id.clone();
            // 点击不可聚焦的区域不会让输入框失焦，所以在框外按下鼠标也视为提交。
            let editor = div()
                .pl(px(space::L))
                .on_mouse_down_out(cx.listener(move |this, _, _, cx| this.finish_rename(&id, cx)))
                // Esc 放弃重命名。
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.cancel_rename(window, cx);
                    }
                }))
                .child(crate::design::framed_handle(
                    gpui_kit::Focusable::focus_handle(input.read(cx), cx),
                    Input::new(input).small().aria_label(t!("sidebar.rename").to_string()),
                ));
            return editor.into_any_element();
        }
        let id = session.id.clone();
        let selected = self.active.read(cx).session_id() == Some(session.id.as_str());
        let (running, waiting) = self.session_state(&session.id, cx);
        // 状态只用图标表示时，读屏软件另外读到文字。
        let label = if waiting {
            t!("sidebar.session_waiting", title = session.title)
        } else if running {
            t!("sidebar.session_running", title = session.title)
        } else {
            session.title.clone().into()
        };
        let status: Option<AnyElement> = if waiting {
            Some(Icon::new(IconName::CircleAlert).size(px(size::ICON_SM)).text_color(p.warning).into_any_element())
        } else if running {
            // 静止的强调色圆点，不转圈：侧边栏里的循环动画每一帧都会让整个侧边栏重新生成，一轮运行多久就重建多久。
            // 正在看的会话在标题栏里转圈（见 `TitleView`）；读屏软件读到的名称里带着“运行中”。
            Some(
                div()
                    .size(px(size::STATUS_DOT))
                    .m(px((size::ICON_SM - size::STATUS_DOT) / 2.))
                    .flex_none()
                    .rounded_full()
                    .bg(p.accent)
                    .into_any_element(),
            )
        } else {
            None
        };
        let menu = self.render_session_menu(index, session, window, cx);
        let row = h_flex()
            .group("session-row")
            .pl(px(space::L))
            .child(
                // 当前会话：强调色的浅底加左边一道强调色竖条，和悬停的灰底一眼分得开（竖条不靠颜色深浅，
                // 高对比度配色下浅底不明显时也看得出来）。
                design::list_item_with_fill(("session", index), None, session.title.clone(), selected, p.accent_soft, &p)
                    .when(selected, |item| {
                        item.child(
                            div().absolute().left(px(0.)).top(px(space::S)).bottom(px(space::S)).w(px(3.)).rounded_full().bg(p.accent),
                        )
                    })
                    // 列表项 + 已选择：读屏软件读出哪个是当前会话（按钮角色不会读“已选择”）。
                    .role(Role::ListItem)
                    .aria_selected(selected)
                    .accessibility_label(label.to_string())
                    .flex_1()
                    .min_w(px(0.))
                    .children(status)
                    // 右键、触屏上长按：打开这一行的“…”菜单（重命名、删除）。
                    .on_aux_click(cx.listener({
                        let id = id.clone();
                        move |this, event: &ClickEvent, _, cx| {
                            if event.is_secondary() {
                                this.pending_delete = None;
                                this.session_menu = Some(id.clone());
                                cx.notify();
                            }
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_session(&id, window, cx))),
            )
            .child(menu);
        row.into_any_element()
    }

    fn render_group_header(&self, index: usize, group: &WorkspaceGroup, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let name = match &group.path {
            Some(path) => folder_name(path),
            None => t!("sidebar.other_sessions").to_string(),
        };
        let collapsed = self.collapsed.contains(&group.path);
        let key = group.path.clone();
        let header = pressable(("workspace", index), &p)
            // 只读文件夹名；完整路径很长，放在描述里最后读。
            .accessibility_label(name.clone())
            .when_some(group.path.as_ref(), |header, path| header.aria_description(path.display().to_string()))
            .aria_expanded(!collapsed)
            .flex_1()
            .min_w(px(0.))
            .h(px(size::LIST_ITEM))
            .px(px(space::S))
            .flex()
            .items_center()
            .gap(px(space::S))
            .rounded(px(radius::MD))
            .cursor_pointer()
            .text_label()
            .relative()
            .child(hover_fill(&p, radius::MD, false))
            .child(motion::turn(
                Icon::new(IconName::ChevronRight).size(px(size::ICON_SM)).text_color(p.text_secondary),
                ("workspace-turn", index),
                !collapsed,
                motion::QUARTER_TURN,
                window,
                cx,
            ))
            .child(Icon::new(IconName::Folder).size(px(size::ICON_MD)).text_color(p.text_secondary))
            .child(div().flex_1().min_w(px(0.)).truncate().child(name.clone()))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed.remove(&key) {
                    this.collapsed.insert(key.clone());
                }
                cx.notify();
            }));
        let mut row = h_flex().gap(px(2.)).child(header);
        if let Some(path) = group.path.clone() {
            let new_path = path.clone();
            row = row.child(crate::design::framed(
                ("workspace-new", index),
                Button::new(("workspace-new", index))
                    .ghost()
                    .control_xs()
                    .icon(IconName::Plus)
                    .tooltip(path.display().to_string())
                    .accessibility_label(t!("sidebar.new_in_workspace", name = name).to_string())
                    .on_click(cx.listener(move |this, _, window, cx| this.new_session(Some(new_path.clone()), window, cx))),
            ));
            if group.remembered && group.sessions.is_empty() {
                row = row.child(crate::design::framed(
                    ("workspace-remove", index),
                    Button::new(("workspace-remove", index))
                        .ghost()
                        .control_xs()
                        .icon(IconName::Close)
                        .accessibility_label(t!("sidebar.remove_workspace", name = name).to_string())
                        .on_click(cx.listener(move |this, _, _, cx| this.forget_workspace(&path, cx))),
                ));
            }
        }
        row.into_any_element()
    }

    /// 当前会话的标题；还没有会话时是“新会话”。
    fn active_title(&self, cx: &App) -> SharedString {
        let session_id = self.active.read(cx).session_id();
        match session_id.and_then(|id| self.sessions.iter().find(|session| session.id == id)) {
            Some(session) => session.title.clone().into(),
            None => t!("sidebar.new_session").to_string().into(),
        }
    }

    /// 系统的窗口标题（任务栏、Alt+Tab、读屏软件读的）跟着当前会话走。在 `render` 里调用：
    /// 不用自绘标题栏时 [`TitleView`] 不画，也要设。
    fn sync_window_title(&mut self, window: &mut Window, cx: &App) {
        let window_title = format!("{} - {APP_NAME}", self.active_title(cx));
        if self.window_title != window_title {
            window.set_window_title(&window_title);
            self.window_title = window_title;
        }
    }

    /// 标题栏里的当前位置：工作区 › 会话标题，运行中时后面带加载圈，等待处理时带提醒图标。
    /// 由 [`TitleView`] 画；系统的窗口标题见 [`Self::sync_window_title`]。
    fn render_title(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let (session_id, workspace) = {
            let active = self.active.read(cx);
            (active.session_id().map(str::to_owned), active.workspace().cloned())
        };
        let title = self.active_title(cx);
        let (running, waiting) = session_id.as_deref().map_or((false, false), |id| self.session_state(id, cx));
        let status: Option<AnyElement> = if waiting {
            Some(Icon::new(IconName::CircleAlert).size(px(size::ICON_SM)).text_color(p.warning).into_any_element())
        } else if running {
            Some(motion::spinner("title-spinner", size::ICON_SM, p.accent).into_any_element())
        } else {
            None
        };
        let crumb = workspace.map(|path| {
            h_flex()
                .flex_none()
                .gap(px(space::XS))
                .text_color(p.text_secondary)
                .child(Icon::new(IconName::Folder).size(px(size::ICON_SM)))
                .child(folder_name(&path))
                .child(Icon::new(IconName::ChevronRight).size(px(size::ICON_SM)))
        });
        let row = h_flex()
            .min_w(px(0.))
            .gap(px(space::XS))
            .items_center()
            .text_body()
            .children(crumb)
            .child(div().min_w(px(0.)).truncate().font_weight(FontWeight::MEDIUM).child(title.clone()))
            .children(status.map(|status| div().flex_none().pl(px(space::XS)).child(status)));
        row.into_any_element()
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            return;
        }
        let view = cx.new(|cx| SettingsView::new(self.backend.clone(), window, cx));
        let subscription = cx.subscribe_in(&view, window, |this, _, event: &SettingsEvent, window, cx| match event {
            SettingsEvent::Changed => {
                this.status = read_status(&this.ui, &this.backend);
                for open in &this.conversations {
                    open.view.update(cx, |conversation, cx| conversation.reload_settings(window, cx));
                }
                cx.notify();
            }
            SettingsEvent::Close => this.close_settings(window, cx),
        });
        // 焦点移进弹窗：Tab 只在弹窗内循环，Esc 可以立即关闭。
        view.update(cx, |view, cx| view.focus(window, cx));
        self.settings = Some((view, subscription));
        cx.notify();
    }

    /// 侧边栏图标按钮（收起后的一列、展开时品牌行右边的“收起”）：悬停显示名字。
    fn sidebar_icon_button(
        &self,
        id: &'static str,
        icon: impl Into<Icon>,
        label: String,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        crate::design::framed(
            id,
            Button::new(id)
                .ghost()
                .icon(icon)
                .tooltip(label.clone())
                .accessibility_label(label)
                .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx))),
        )
        .into_any_element()
    }

    /// 收起后的侧边栏：一列图标按钮（展开、新会话、添加工作区、搜索；底部是插件入口和设置）。
    fn render_sidebar_rail(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let hidden = self.backend.hidden_entries();
        // macOS 自绘标题栏时左上角是红黄绿按钮：品牌标志让开，那一行只留着拖动窗口。
        let traffic_lights = title_bar::brand_offset(0., window) > 0.;
        let brand = title_bar::drag_region("sidebar-brand", window, cx)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .w_full()
            .h(px(size::TITLE_BAR))
            .when(!traffic_lights, |row| row.child(brand_mark(size::ICON_LG, &p)));
        let top = v_flex()
            .items_center()
            .gap(px(space::XS))
            .child(self.sidebar_icon_button(
                "sidebar-expand",
                IconName::PanelLeftOpen,
                t!("sidebar.expand").to_string(),
                |this, _, cx| this.toggle_sidebar(cx),
                cx,
            ))
            .child(self.sidebar_icon_button(
                "rail-new-session",
                gpui_kit::assets::IconName::SquarePen,
                t!("sidebar.new_session").to_string(),
                |this, window, cx| this.new_session(None, window, cx),
                cx,
            ))
            .child(self.sidebar_icon_button(
                "rail-add-workspace",
                gpui_kit::assets::IconName::FolderPlus,
                t!("sidebar.add_workspace").to_string(),
                |this, window, cx| this.add_workspace(window, cx),
                cx,
            ))
            .child(self.sidebar_icon_button(
                "rail-search",
                IconName::Search,
                t!("sidebar.search").to_string(),
                |this, window, cx| this.open_session_search(window, cx),
                cx,
            ));
        let mut bottom: Vec<AnyElement> = Vec::new();
        for item in self.ui.sidebar_items().iter().filter(|item| !hidden.contains(&format!("sidebar:{}", item.id))) {
            let on_click = item.on_click.clone();
            bottom.push(
                crate::design::framed(
                    item.id,
                    Button::new(item.id)
                        .ghost()
                        .icon(item.icon.clone())
                        .tooltip((item.label)())
                        .accessibility_label((item.label)())
                        .on_click(move |_, window, cx| on_click(window, cx)),
                )
                .into_any_element(),
            );
        }
        for (index, entry) in self.plugin_entries.iter().enumerate() {
            if entry.entry.place != dshr_agent::EntryPlace::Sidebar {
                continue;
            }
            let id = ElementId::NamedInteger("rail-plugin-entry".into(), index as u64);
            bottom.push(
                crate::design::framed(
                    id.clone(),
                    Button::new(id)
                        .ghost()
                        .icon(plugin_page::entry_icon(entry.entry.icon.as_deref()))
                        .tooltip(entry.entry.label.clone())
                        .accessibility_label(entry.entry.label.clone())
                        .on_click(cx.listener(move |this, _, window, cx| this.open_plugin_page(index, window, cx))),
                )
                .into_any_element(),
            );
        }
        bottom.push(self.sidebar_icon_button(
            "rail-settings",
            IconName::Settings,
            t!("sidebar.settings").to_string(),
            |this, window, cx| this.open_settings(window, cx),
            cx,
        ));
        v_flex()
            .id("sidebar-rail")
            .role(Role::Navigation)
            .w(px(size::SIDEBAR_RAIL_WIDTH))
            .h_full()
            .flex_none()
            .items_center()
            .pb(px(space::M))
            .when(!title_bar::custom(), |rail| rail.pt(px(space::M)))
            .gap(px(space::XS))
            .child(brand)
            .child(top)
            .child(div().flex_1())
            .child(v_flex().items_center().pt(px(space::S)).gap(px(space::XS)).border_t_1().border_color(p.border).children(bottom))
            .into_any_element()
    }

    fn render_sidebar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let width = self.sidebar_width.get();
        // 宽度在过渡：外层按当前宽度裁切，里面按收起或展开时的宽度排版，内容不会被挤得换行。
        let inner = if self.sidebar_collapsed { self.render_sidebar_rail(window, cx) } else { self.render_sidebar_full(window, cx) };
        div()
            .w(px(width))
            .h_full()
            .flex_none()
            .overflow_hidden()
            .bg(p.sidebar)
            .when(!title_bar::custom(), |sidebar| sidebar.border_r_1().border_color(p.border))
            .child(inner)
            .into_any_element()
    }

    fn render_sidebar_full(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let hidden = self.backend.hidden_entries();
        let mut list: Vec<AnyElement> = Vec::new();
        let scroll = &self.sidebar_scroll;
        for (group_index, group) in self.groups().iter().enumerate() {
            // 每一行都跟随焦点：Tab 走到列表下面看不见的地方时，列表跟着滚。
            list.push(
                reveal::reveal_in(scroll, ("reveal-workspace", group_index), self.render_group_header(group_index, group, window, cx))
                    .into_any_element(),
            );
            // 折叠、展开即时完成（布局变化不做动画）。
            if self.collapsed.contains(&group.path) {
                continue;
            }
            let mut rows: Vec<AnyElement> = Vec::new();
            if group.sessions.is_empty() {
                rows.push(
                    div()
                        .pl(px(space::XXL + space::S))
                        .py(px(space::XS))
                        .child(hint(t!("sidebar.no_sessions").to_string(), Tone::Neutral, &p))
                        .into_any_element(),
                );
            }
            // 每组先显示最近的 `SIDEBAR_PAGE` 个；正在看的、在运行的会话不管多早都显示。
            let limit = self.sidebar_limits.get(&group.path).copied().unwrap_or(SIDEBAR_PAGE);
            let active_id = self.active.read(cx).session_id().map(str::to_owned);
            let mut hidden = 0;
            for (position, &index) in group.sessions.iter().enumerate() {
                let session = &self.sessions[index];
                let keep = position < limit
                    || active_id.as_deref() == Some(session.id.as_str())
                    || self.session_state(&session.id, cx) != (false, false);
                if keep {
                    let row = self.render_session_row(index, session, window, cx);
                    rows.push(
                        reveal::reveal_in(scroll, ElementId::Name(format!("reveal-session-{}", session.id).into()), row).into_any_element(),
                    );
                } else {
                    hidden += 1;
                }
            }
            if hidden > 0 {
                let path = group.path.clone();
                rows.push(
                    reveal::reveal_in(
                        scroll,
                        ("reveal-more", group_index),
                        h_flex().pl(px(space::L)).child(
                            list_item(("sessions-more", group_index), None, t!("sidebar.show_more", count = hidden).to_string(), false, &p)
                                .flex_1()
                                .min_w(px(0.))
                                .text_color(p.text_secondary)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    *this.sidebar_limits.entry(path.clone()).or_insert(SIDEBAR_PAGE) += SIDEBAR_PAGE;
                                    cx.notify();
                                })),
                        ),
                    )
                    .into_any_element(),
                );
            }
            list.push(v_flex().gap(px(2.)).children(rows).into_any_element());
        }

        let new_session = list_item("new-session", Some(IconName::Plus), t!("sidebar.new_session").to_string(), false, &p)
            .on_click(cx.listener(|this, _, window, cx| this.new_session(None, window, cx)));
        let search =
            list_item("search-sessions", Some(IconName::Search), t!("sidebar.search").to_string(), self.session_search.is_some(), &p)
                .on_click(cx.listener(|this, _, window, cx| this.open_session_search(window, cx)));
        let collapse = self.sidebar_icon_button(
            "sidebar-collapse",
            IconName::PanelLeftClose,
            t!("sidebar.collapse").to_string(),
            |this, _, cx| this.toggle_sidebar(cx),
            cx,
        );

        v_flex()
            .w(px(size::SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .p(px(space::M))
            // 自绘标题栏时侧边栏一直通到窗口顶上：品牌行贴着顶边，和对话区上方的标题栏一样高，也能拖动窗口。
            .when(title_bar::custom(), |sidebar| sidebar.pt_0())
            .gap(px(space::XS))
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .child(
                        title_bar::drag_region("sidebar-brand", window, cx)
                            .flex()
                            .flex_1()
                            .min_w(px(0.))
                            .items_center()
                            .h(px(size::TITLE_BAR))
                            .px(px(space::M))
                            // macOS：品牌放在红黄绿按钮右边。
                            .pl(px(space::M + title_bar::brand_offset(space::M * 2., window)))
                            .gap(px(space::S))
                            .child(brand_mark(size::ICON_LG, &p))
                            .child(div().text_heading().child(APP_NAME)),
                    )
                    // 收起按钮放在拖动区外面：Windows 上拖动区整块交给系统，里面的按钮点不到。
                    .child(collapse),
            )
            .child(new_session)
            .child(search)
            .child(
                h_flex()
                    .pl(px(space::M))
                    .pt(px(space::L))
                    .pb(px(space::XS))
                    .child(
                        div().flex_1().text_caption().text_color(p.text_secondary).child(heading(t!("sidebar.workspaces").to_string(), 2)),
                    )
                    .child(crate::design::framed(
                        "add-workspace",
                        Button::new("add-workspace")
                            .ghost()
                            .control_xs()
                            .icon(IconName::FolderOpen)
                            .tooltip(t!("sidebar.add_workspace").to_string())
                            .accessibility_label(t!("sidebar.add_workspace").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.add_workspace(window, cx))),
                    )),
            )
            .child(
                v_flex()
                    .id("session-list")
                    .role(Role::List)
                    .aria_label(t!("sidebar.workspaces").to_string())
                    .track_scroll(&self.sidebar_scroll)
                    .flex_1()
                    .min_h(px(0.))
                    .gap(px(2.))
                    .overflow_y_scroll()
                    .children(list),
            )
            .children(
                self.sidebar_error
                    .clone()
                    .map(|error| motion::fade_in(div().px(px(space::M)).child(hint(error, Tone::Danger, &p)), "sidebar-error")),
            )
            .child(
                v_flex()
                    .pt(px(space::S))
                    .gap(px(space::XS))
                    .border_t_1()
                    .border_color(p.border)
                    // 界面插件登记的入口排在“设置”上面；插件藏起来的（`sidebar:<id>`）不显示。
                    .children(self.ui.sidebar_items().iter().filter(|item| !hidden.contains(&format!("sidebar:{}", item.id))).map(|item| {
                        let on_click = item.on_click.clone();
                        list_item(item.id, Some(item.icon.clone()), (item.label)(), false, &p)
                            .on_click(move |_, window, cx| on_click(window, cx))
                    }))
                    // JS 插件加的入口。
                    .children(
                        self.plugin_entries
                            .iter()
                            .enumerate()
                            .filter(|(_, entry)| entry.entry.place == dshr_agent::EntryPlace::Sidebar)
                            .map(|(index, entry)| {
                                let open = self.plugin_page.as_ref().is_some_and(|page| page.title == entry.entry.label);
                                list_item(
                                    ("plugin-entry", index),
                                    Some(plugin_page::entry_icon(entry.entry.icon.as_deref())),
                                    entry.entry.label.clone(),
                                    open,
                                    &p,
                                )
                                .on_click(cx.listener(move |this, _, window, cx| this.open_plugin_page(index, window, cx)))
                            }),
                    )
                    .child(
                        list_item("settings", Some(IconName::Settings), t!("sidebar.settings").to_string(), self.settings.is_some(), &p)
                            .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
                    )
                    .children(self.status.iter().enumerate().map(|(index, line)| {
                        h_flex().px(px(space::M)).py(px(space::XS)).gap(px(space::S)).child(status_dot(line.ok, &p)).child(
                            div()
                                .text_caption()
                                .text_color(p.text_secondary)
                                .child(readable_with_id(("sidebar-status", index), line.label.clone())),
                        )
                    }))
                    // JS 插件的状态行：点了打开整页。
                    .children(self.plugin_status.iter().filter(|(_, text)| !text.is_empty()).map(|(index, text)| {
                        let index = *index;
                        pressable(("plugin-status", index), &p)
                            .accessibility_label(text.clone())
                            .flex()
                            .items_center()
                            .px(px(space::M))
                            .py(px(space::XS))
                            .gap(px(space::S))
                            .rounded(px(radius::MD))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| this.open_plugin_page(index, window, cx)))
                            .child(status_dot(true, &p))
                            .child(div().flex_1().min_w(px(0.)).truncate().text_caption().text_color(p.text_secondary).child(text.clone()))
                    })),
            )
            .into_any_element()
    }
}

/// 铺满父元素、内容居中的一层（弹窗卡片放在里面）。
fn centered() -> Div {
    div().size_full().flex().items_center().justify_center()
}

/// 弹窗的遮罩：跟着弹窗的进度淡入淡出。
fn scrim(phase: motion::Phase, p: &design::Palette) -> Div {
    div().absolute().inset_0().bg(p.scrim).opacity(phase.visibility())
}

/// 目录的显示名：最后一级目录名，根目录等没有名字时显示完整路径。
fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 先按最近一次的输入方式定下触屏模式：下面和子视图取的调色板都带着它。
        design::update_touch_mode(window, cx);
        // 按键之后晚一两帧才移过去的焦点（打开菜单、弹窗）也算键盘的：在子视图画焦点框之前定下来。
        design::settle_focus_source(window, cx);
        let p = palette(cx);
        // 侧边栏和对话区各自缓存：一边重绘（流式输出、悬停、菜单）不会带着另一边重新生成。
        // 缓存的视图不按内容测量，尺寸由这里给出（两个视图的根元素自己也撑满，不缓存时布局一样）。
        //
        // 读屏软件连着时不缓存：GPUI 沿用缓存时只回放绘制结果，不回放无障碍节点和无障碍动作，
        // 另一边一重绘，没重绘的这一边就从无障碍树里整个消失（例如一输入文字，侧边栏就读不到了）。
        // 只有读屏软件连着时才有这份开销；它连上时 GPUI 会强制重绘一次，这里随之切换。
        let cache = !window.is_a11y_active();
        // 收起、展开侧边栏即时切换宽度（布局变化不做动画，对话区不用逐帧重排）。
        let sidebar_width = if self.sidebar_collapsed { size::SIDEBAR_RAIL_WIDTH } else { size::SIDEBAR_WIDTH };
        if (self.sidebar_width.get() - sidebar_width).abs() > f32::EPSILON {
            self.sidebar_width.set(sidebar_width);
            // 缓存的侧边栏按新宽度重画。
            self.sidebar.update(cx, |_, cx| cx.notify());
        }
        let sidebar_style = StyleRefinement::default().w(px(sidebar_width)).h_full().flex_none();
        let sidebar = if cache { self.sidebar.clone().cached(sidebar_style) } else { ViewElement::new(self.sidebar.clone()) };
        let active =
            if cache { self.active.clone().cached(StyleRefinement::default().size_full()) } else { ViewElement::new(self.active.clone()) };
        // 切换会话即时显示（导航不做动画）。
        let conversation = div().flex_1().h_full().min_w(px(0.)).child(active);
        // 设置以弹窗形式盖在工作区上：遮罩淡入，卡片淡入并上浮一点；关上时立即移除。
        // 卡片的样子由设置视图自己按进度画（阴影在卡片上，要跟着渐显）。
        let settings_phase = motion::presence("settings-overlay", self.settings.is_some(), motion::SLOW, window, cx);
        let shown_settings = self.settings.as_ref().map(|(view, _)| view.clone());
        let overlay = settings_phase.zip(shown_settings).map(|(phase, settings)| {
            settings.update(cx, |view, _| view.set_phase(phase));
            title_bar::below(div().id("settings-overlay").absolute())
                .occlude()
                .child(scrim(phase, &p))
                // 卡片的最大宽高按遮罩的尺寸算，所以居中层也铺满遮罩。
                .child(centered().child(settings))
        });
        // 插件页面也以弹窗形式打开：标题、关闭按钮，下面是插件画的内容。
        let plugin_phase = motion::presence("plugin-page-overlay", self.plugin_page.is_some(), motion::SLOW, window, cx);
        let shown_page = self.plugin_page.as_ref();
        let plugin_page = plugin_phase.zip(shown_page).map(|(phase, page)| {
            let dialog = v_flex()
                .id("plugin-page")
                .role(Role::Dialog)
                .aria_label(page.title.clone())
                .track_focus(&page.focus)
                .w(px(size::MODAL_CONTENT_MAX_WIDTH))
                .max_w(relative(0.94))
                .max_h(relative(0.9))
                .rounded(px(radius::XL))
                .bg(p.raised)
                .border_1()
                .border_color(p.border)
                .overflow_hidden()
                .child(
                    h_flex()
                        .px(px(space::XXL))
                        .pt(px(space::XL))
                        .pb(px(space::M))
                        .child(div().flex_1().child(heading(page.title.clone(), 1).text_title()))
                        .child(crate::design::framed(
                            "close-plugin-page",
                            Button::new("close-plugin-page")
                                .ghost()
                                .control_sm()
                                .icon(IconName::Close)
                                .accessibility_label(t!("settings.close").to_string())
                                .on_click(cx.listener(|this, _, window, cx| this.close_plugin_page(window, cx))),
                        )),
                )
                .child(
                    div()
                        .id("plugin-page-body")
                        .track_scroll(page.view.read(cx).scroll_handle())
                        .px(px(space::XXL))
                        .pb(px(space::XXL))
                        .overflow_y_scroll()
                        .child(page.view.clone()),
                )
                .map(|dialog| motion::pop(dialog, motion::Edge::Bottom, motion::SHORT, phase, shadow::modal(&p)))
                .focus_trap("plugin-page-trap", &page.focus);
            title_bar::below(div().id("plugin-page-overlay").absolute())
                .occlude()
                .child(scrim(phase, &p))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.close_plugin_page(window, cx);
                    }
                }))
                .child(centered().child(dialog))
        });
        // 搜索会话：靠上居中，像命令面板。点遮罩关上。
        let search_phase = motion::presence("session-search-overlay", self.session_search.is_some(), motion::FAST, window, cx);
        let shown_search = self.session_search.as_ref().map(|(view, _)| view.clone());
        let search_overlay = search_phase.zip(shown_search).map(|(phase, view)| {
            view.update(cx, |view, _| view.set_phase(phase));
            title_bar::below(div().id("session-search-overlay").absolute())
                .occlude()
                .child(
                    scrim(phase, &p)
                        .id("session-search-scrim")
                        .on_click(cx.listener(|this, _, window, cx| this.close_session_search(window, cx))),
                )
                .child(div().size_full().flex().justify_center().items_start().pt(relative(0.08)).child(view))
        });
        // 安装确认盖在最上面（从设置页装插件时设置弹窗正开着）。点遮罩不关：要用户明确回答。
        let install_phase = motion::presence("install-dialog-overlay", self.install_dialog.is_some(), motion::SLOW, window, cx);
        let shown_install = self.install_dialog.as_ref().map(|open| open.view.clone());
        let install_overlay = install_phase.zip(shown_install).map(|(phase, dialog)| {
            dialog.update(cx, |view, _| view.set_phase(phase));
            title_bar::below(div().id("install-dialog-overlay").absolute())
                .occlude()
                .child(scrim(phase, &p))
                .child(centered().child(dialog))
        });
        let framed = title_bar::custom();
        self.sync_window_title(window, cx);
        self.publish_bridge_snapshot(cx);
        // macOS 上侧边栏收起后红黄绿按钮伸到标题栏里：标题往右让开。
        let title = h_flex()
            .min_w(px(0.))
            .pl(px(title_bar::brand_offset(self.sidebar_width.get(), window)))
            .child(self.title.clone())
            .into_any_element();
        // 焦点框按真正触发的设备显示（见 `design::focus_from_keyboard`）。
        design::track_focus_source(div())
            .relative()
            .size_full()
            .key_context("Workspace")
            .on_action(cx.listener(Self::on_new_session))
            .on_action(cx.listener(Self::on_toggle_settings))
            .on_action(cx.listener(Self::on_focus_composer))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_search_sessions))
            // 插件命令的快捷键（和内置快捷键冲突的不生效）。
            .on_key_down(cx.listener(Self::on_plugin_shortcut))
            .text_color(p.text)
            // 平滑滚动层放在最前：捕获阶段按绘制顺序派发，它得先于对话区的滚动容器拿到滚轮事件。
            .child(self.scroll.layer(cx))
            .child(
                // 自绘标题栏时：侧边栏和标题栏铺同一个“框架”色，对话区是嵌在里面的一块面板（左上圆角、描边），
                // 框架和内容一眼分得开，标题栏也不再是一条孤零零的空白（见 `title_bar` 模块说明）。
                h_flex().size_full().when(framed, |row| row.bg(p.sidebar)).child(sidebar).child(
                    // 标题栏只盖在对话区上方；侧边栏通到窗口顶上，顶部不留空。
                    v_flex().flex_1().min_w(px(0.)).h_full().children(title_bar::title_bar(true, Some(title), window, cx)).child(
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .w_full()
                            .bg(p.surface)
                            .when(framed, |panel| {
                                panel
                                    .rounded_tl(px(title_bar::PANEL_RADIUS))
                                    .border_t_1()
                                    .border_l_1()
                                    .border_color(p.border)
                                    .overflow_hidden()
                            })
                            .child(conversation),
                    ),
                ),
            )
            // 盖在侧边栏和对话区上面，但在弹窗下面：弹窗开着时焦点陷阱把 Tab 留在弹窗里。
            .child(self.render_skip_links(window, cx))
            .children(overlay)
            .children(plugin_page)
            .children(search_overlay)
            .children(install_overlay)
            // 给读屏软件的播报（发送后“正在回复”、保存结果等），见 `announce`。
            .child(announce::live_region(cx))
    }
}

#[cfg(test)]
mod tests {
    // 不用 `super::*`：那样会把 GPUI 的 `test` 宏一起引进来，盖过标准的 `#[test]`。
    use super::{display_keystroke, is_valid_keystroke};

    #[test]
    fn keystroke_validation_and_display() {
        assert!(is_valid_keystroke("ctrl-shift-n"));
        assert!(is_valid_keystroke("ctrl--"));
        assert!(!is_valid_keystroke("n"));
        assert!(!is_valid_keystroke("hyper-n"));
        if !cfg!(target_os = "macos") {
            assert_eq!(display_keystroke("ctrl-shift-n"), "Ctrl+Shift+N");
            assert_eq!(display_keystroke("ctrl-,"), "Ctrl+,");
            assert_eq!(display_keystroke("alt-enter"), "Alt+Enter");
        }
    }
}

/// 标题栏的转圈为什么要放进子视图（[`TitleView`]）：GPUI 里循环动画每一帧通知它所在的视图。
/// 被通知的若是工作区视图本身，它的 `observe_self` 就会让缓存的侧边栏重新生成；放在子视图里时，
/// 工作区视图只是作为祖先重画，`observe_self` 不触发，侧边栏沿用上一帧。
#[cfg(test)]
mod view_cache_tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::TestAppContext;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _, Render, StyleRefinement, Styled as _,
        Subscription, Window, WindowBounds, WindowOptions, black, div, point, px, size,
    };

    use crate::design::motion;

    /// 代替侧边栏：缓存的视图，记下自己被画了几次。
    struct Sidebar(Rc<Cell<usize>>);

    impl Render for Sidebar {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.0.set(self.0.get() + 1);
            div().size_full()
        }
    }

    struct Spinner;

    impl Render for Spinner {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            motion::spinner("spinner", 14., black())
        }
    }

    /// 代替工作区视图：自己一被通知就通知侧边栏（`observe_self`）；转圈画在自己里面，或者放在子视图里。
    struct Workspace {
        sidebar: Entity<Sidebar>,
        spinner: Option<Entity<Spinner>>,
        _subscription: Subscription,
    }

    impl Render for Workspace {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let spinner = match &self.spinner {
                Some(view) => view.clone().into_any_element(),
                None => motion::spinner("spinner", 14., black()).into_any_element(),
            };
            div().size_full().child(self.sidebar.clone().cached(StyleRefinement::default().w(px(200.)).h(px(400.)))).child(spinner)
        }
    }

    fn sidebar_renders_while_spinning(cx: &mut TestAppContext, spinner_in_child_view: bool) -> usize {
        let count = Rc::new(Cell::new(0));
        let window: AnyWindowHandle = cx
            .update(|cx| {
                gpui_kit::init(cx);
                let options = WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds { origin: point(px(0.), px(0.)), size: size(px(800.), px(600.)) })),
                    ..Default::default()
                };
                let count = count.clone();
                gpui_kit::open_window(options, cx, move |_, cx| {
                    cx.new(|cx| {
                        let sidebar = cx.new(|_| Sidebar(count));
                        let subscription = cx.observe_self(|this: &mut Workspace, cx| this.sidebar.update(cx, |_, cx| cx.notify()));
                        let spinner = spinner_in_child_view.then(|| cx.new(|_| Spinner));
                        Workspace { sidebar, spinner, _subscription: subscription }
                    })
                })
                .map(|(window, _)| window)
            })
            .unwrap();
        let frame = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx)).unwrap();
            cx.run_until_parked();
        };
        frame(cx);
        frame(cx);
        let before = count.get();
        for _ in 0..10 {
            cx.executor().advance_clock(std::time::Duration::from_millis(40));
            cx.run_until_parked();
            frame(cx);
        }
        count.get() - before
    }

    #[gpui::test]
    fn spinner_in_the_parent_view_rebuilds_the_cached_sidebar(cx: &mut TestAppContext) {
        assert!(sidebar_renders_while_spinning(cx, false) >= 5, "对照：转圈画在父视图里时，侧边栏每一帧都重画");
    }

    #[gpui::test]
    fn spinner_in_a_child_view_leaves_the_cached_sidebar_alone(cx: &mut TestAppContext) {
        assert_eq!(sidebar_renders_while_spinning(cx, true), 0);
    }
}
