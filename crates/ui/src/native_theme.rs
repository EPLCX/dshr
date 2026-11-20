//! 让系统自己画的部分跟着程序里选的明暗走：窗口标题栏、托盘菜单和标题栏的系统菜单。
//!
//! GPUI 只按**系统**的外观设置标题栏（创建窗口时、系统外观变化时），程序里手动选了深色而系统是浅色时，
//! 标题栏还是浅色的；系统画的弹出菜单（托盘菜单）默认总是浅色。这里在 Windows 上：
//!
//! - 标题栏：`DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`，每个窗口设一次；
//! - 弹出菜单：`uxtheme.dll` 里没有名字、只有序号的 `SetPreferredAppMode`（135）和 `FlushMenuThemes`（136），
//!   Windows 10 1903 起有；找不到时什么都不做。
//!
//! macOS 上标题栏和菜单跟着 `NSAppearance` 走，GPUI 已经处理；其他平台什么都不做。

use gpui_kit::Window;

/// 把这个窗口的标题栏设成深色或浅色。
pub fn set_title_bar(window: &Window, dark: bool) {
    #[cfg(windows)]
    windows::set_title_bar(window, dark);
    #[cfg(not(windows))]
    let _ = (window, dark);
}

/// 让程序里系统画的弹出菜单（托盘菜单等）用深色或浅色。
pub fn set_menus(dark: bool) {
    #[cfg(windows)]
    windows::set_menus(dark);
    #[cfg(not(windows))]
    let _ = dark;
}

#[cfg(windows)]
mod windows {
    use gpui_kit::Window;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    pub fn set_title_bar(window: &Window, dark: bool) {
        let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
        let hwnd = handle.hwnd.get() as HWND;
        let value: i32 = dark.into();
        // SAFETY: hwnd 是 GPUI 这个窗口的句柄，窗口在这次调用期间一直存在；value 的大小与类型（BOOL）对得上。
        unsafe {
            DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, (&raw const value).cast(), size_of::<i32>() as u32);
        }
    }

    /// `PreferredAppMode`：0 默认、1 允许深色、2 强制深色、3 强制浅色。
    type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
    type FlushMenuThemes = unsafe extern "system" fn();

    pub fn set_menus(dark: bool) {
        let name: Vec<u16> = "uxtheme.dll".encode_utf16().chain([0]).collect();
        // SAFETY: 名字以 0 结尾；uxtheme.dll 是系统库，加载后不释放（进程里本来就有它）。
        // 两个序号函数的签名按 Windows 10 1903 起的实现；系统里没有它们时 GetProcAddress 返回空，什么都不做。
        unsafe {
            let module = LoadLibraryW(name.as_ptr());
            if module.is_null() {
                return;
            }
            let Some(set_mode) = GetProcAddress(module, 135 as *const u8) else { return };
            let set_mode: SetPreferredAppMode = std::mem::transmute(set_mode);
            set_mode(if dark { 2 } else { 3 });
            if let Some(flush) = GetProcAddress(module, 136 as *const u8) {
                let flush: FlushMenuThemes = std::mem::transmute(flush);
                flush();
            }
        }
    }
}
