//! 系统托盘（macOS 是菜单栏）图标。关闭工作区窗口时默认隐藏到这里，后台的会话继续运行；
//! 左键单击图标或菜单里的“显示”恢复窗口，“退出”才真正结束程序。

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui_kit::*;
use rust_i18n::t;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// 托盘图标（64×64，`assets/make-icon.ps1` 生成）。
const TRAY_PNG: &[u8] = include_bytes!("../../../assets/tray.png");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    Show,
    NewSession,
    Quit,
}

/// 托盘图标在程序运行期间一直存在；丢掉它图标就会消失。
struct Tray {
    _icon: TrayIcon,
}

impl Global for Tray {}

/// 是否已经装好托盘图标（失败时关闭窗口就直接退出）。
pub fn is_installed(cx: &App) -> bool {
    cx.has_global::<Tray>()
}

/// 创建托盘图标，菜单与点击转成 [`TrayCommand`] 交给 `handle` 在前台线程处理。
pub fn install(cx: &mut App, handle: impl Fn(TrayCommand, &mut App) + 'static) -> Result<(), String> {
    // 只支持 Windows 和 macOS。在 Linux 上（开发、自动化测试时）托盘库要先初始化 GTK，否则直接崩溃；
    // 这里按“没有托盘”处理：关闭窗口就退出，其它功能照常。
    if !cfg!(any(windows, target_os = "macos")) {
        return Err("this platform has no tray support".into());
    }
    let show = MenuItem::new(t!("tray.show"), true, None);
    let new_session = MenuItem::new(t!("tray.new_session"), true, None);
    let quit = MenuItem::new(t!("tray.quit"), true, None);
    let menu = Menu::new();
    menu.append_items(&[&show, &new_session, &PredefinedMenuItem::separator(), &quit]).map_err(|error| error.to_string())?;

    let image = image::load_from_memory(TRAY_PNG).map_err(|error| error.to_string())?.into_rgba8();
    let (width, height) = image.dimensions();
    let icon = tray_icon::Icon::from_rgba(image.into_raw(), width, height).map_err(|error| error.to_string())?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip(crate::APP_NAME)
        .with_icon(icon)
        .build()
        .map_err(|error| error.to_string())?;

    // 回调可能在任意线程触发，只负责把命令送进通道。
    let (tx, mut commands) = mpsc::unbounded();
    let ids = [show.id().clone(), new_session.id().clone(), quit.id().clone()];
    let menu_tx = tx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let command = match ids.iter().position(|id| *id == event.id) {
            Some(0) => TrayCommand::Show,
            Some(1) => TrayCommand::NewSession,
            Some(2) => TrayCommand::Quit,
            _ => return,
        };
        menu_tx.unbounded_send(command).ok();
    }));
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
            tx.unbounded_send(TrayCommand::Show).ok();
        }
    }));
    cx.spawn(async move |cx| {
        while let Some(command) = commands.next().await {
            cx.update(|cx| handle(command, cx));
        }
    })
    .detach();
    cx.set_global(Tray { _icon: tray });
    Ok(())
}

/// 隐藏窗口（不销毁，视图和后台任务都保留）。
pub fn hide_window(window: &mut Window, cx: &mut App) {
    #[cfg(windows)]
    {
        let _ = cx;
        if let Some(hwnd) = native_window(window) {
            use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
            unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        cx.hide();
    }
}

/// 恢复被隐藏的窗口并放到前台。
pub fn show_window(window: &mut Window, cx: &mut App) {
    #[cfg(windows)]
    {
        let _ = cx;
        if let Some(hwnd) = native_window(window) {
            use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow};
            unsafe {
                ShowWindow(hwnd, if IsIconic(hwnd) != 0 { SW_RESTORE } else { SW_SHOW });
                SetForegroundWindow(hwnd);
            }
        }
    }
    #[cfg(not(windows))]
    cx.activate(true);
    window.activate_window();
}

#[cfg(windows)]
fn native_window(window: &Window) -> Option<windows_sys::Win32::Foundation::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // `Window` 自己也有个同名的 `window_handle`，这里要的是 raw-window-handle 的那个。
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as _),
        _ => None,
    }
}
