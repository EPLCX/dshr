//! 系统的辅助功能设置（设置 → 辅助功能里选“跟随系统”时用）。
//!
//! 高对比度：
//! - Windows：“对比度主题”（`SystemParametersInfoW(SPI_GETHIGHCONTRAST)` 的 `HCF_HIGHCONTRASTON`）；
//! - macOS：“增强对比度”（`NSWorkspace.accessibilityDisplayShouldIncreaseContrast`）；
//! - 其他平台没有统一的设置，当作关着。
//!
//! 高对比度的颜色：Windows 的对比度主题开着时，按系统颜色（`GetSysColor`）画，和系统里选的主题（高对比白、黑、
//! 水生、沙漠或自定义）一致；macOS 的“增强对比度”没有可读的颜色，用内置的高对比度配色。
//!
//! 文字大小：
//! - Windows：设置 → 辅助功能 → 文本大小（注册表 `HKCU\Software\Microsoft\Accessibility` 的 `TextScaleFactor`，100–225）；
//! - 其他平台没有全局的文字大小设置，当作 100%。
//!
//! 系统设置改了不会通知到这里：窗口外观变化、窗口回到前台时重新读一次（见 `WorkspaceView`）。

/// 系统的文字大小，百分比（100 是默认）。
pub fn text_scale() -> u16 {
    #[cfg(windows)]
    {
        windows::text_scale()
    }
    #[cfg(not(windows))]
    {
        100
    }
}

/// Windows 对比度主题里用户可以改的颜色（设置 → 辅助功能 → 对比度主题 → 编辑），`0xRRGGBB`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemColors {
    /// 背景。
    pub window: u32,
    /// 文本。
    pub window_text: u32,
    /// 选定文本的底色、文字色。
    pub highlight: u32,
    pub highlight_text: u32,
    /// 超链接。
    pub hot_light: u32,
    /// 失效文本。
    pub gray_text: u32,
    /// 按钮的底色、文字色。
    pub button_face: u32,
    pub button_text: u32,
}

/// 系统的对比度主题开着时，它的颜色；关着、或者系统没有可读的颜色（macOS、Linux）时为 `None`。
pub fn high_contrast_colors() -> Option<SystemColors> {
    #[cfg(windows)]
    {
        windows::high_contrast().then(windows::system_colors)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 系统现在是否开着高对比度。
pub fn high_contrast() -> bool {
    #[cfg(windows)]
    {
        windows::high_contrast()
    }
    #[cfg(target_os = "macos")]
    {
        macos::high_contrast()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETHIGHCONTRAST, SystemParametersInfoW};

    /// “文本大小”滑块，没设过时没有这个值（就是 100%）。
    pub fn text_scale() -> u16 {
        let key: Vec<u16> = "Software\\Microsoft\\Accessibility".encode_utf16().chain([0]).collect();
        let name: Vec<u16> = "TextScaleFactor".encode_utf16().chain([0]).collect();
        let mut value: u32 = 0;
        let mut size = size_of::<u32>() as u32;
        // SAFETY: 两个名字都以 0 结尾；只读一个 DWORD，缓冲区大小和类型都对得上。
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&raw mut value).cast(),
                &mut size,
            )
        };
        if status == 0 && (100..=225).contains(&value) { value as u16 } else { 100 }
    }

    /// `HIGHCONTRASTW`（自己声明，免得为一个结构体多开一个 windows-sys 的 feature）。
    #[repr(C)]
    struct HighContrast {
        size: u32,
        flags: u32,
        default_scheme: *mut u16,
    }

    /// `HCF_HIGHCONTRASTON`。
    const HIGH_CONTRAST_ON: u32 = 0x1;

    /// 按 `COLOR_*` 读系统颜色。`GetSysColor` 返回 `0x00BBGGRR`，换成 `0xRRGGBB`。
    pub fn system_colors() -> super::SystemColors {
        use windows_sys::Win32::Graphics::Gdi::GetSysColor;
        let color = |index: i32| {
            // SAFETY: 只读一个系统颜色，下标都是 `COLOR_*` 里有定义的值。
            let bgr = unsafe { GetSysColor(index as _) };
            ((bgr & 0xFF) << 16) | (bgr & 0xFF00) | ((bgr >> 16) & 0xFF)
        };
        // `COLOR_*` 的值（winuser.h）。
        const WINDOW: i32 = 5;
        const WINDOW_TEXT: i32 = 8;
        const HIGHLIGHT: i32 = 13;
        const HIGHLIGHT_TEXT: i32 = 14;
        const BUTTON_FACE: i32 = 15;
        const GRAY_TEXT: i32 = 17;
        const BUTTON_TEXT: i32 = 18;
        const HOT_LIGHT: i32 = 26;
        super::SystemColors {
            window: color(WINDOW),
            window_text: color(WINDOW_TEXT),
            highlight: color(HIGHLIGHT),
            highlight_text: color(HIGHLIGHT_TEXT),
            hot_light: color(HOT_LIGHT),
            gray_text: color(GRAY_TEXT),
            button_face: color(BUTTON_FACE),
            button_text: color(BUTTON_TEXT),
        }
    }

    pub fn high_contrast() -> bool {
        let mut info = HighContrast { size: size_of::<HighContrast>() as u32, flags: 0, default_scheme: std::ptr::null_mut() };
        // SAFETY: SPI_GETHIGHCONTRAST 要一个 cbSize 填好的 HIGHCONTRASTW，结构体布局与之相同，调用期间一直有效。
        let ok = unsafe { SystemParametersInfoW(SPI_GETHIGHCONTRAST, info.size, (&raw mut info).cast(), 0) };
        ok != 0 && info.flags & HIGH_CONTRAST_ON != 0
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void};

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    pub fn high_contrast() -> bool {
        // SAFETY: AppKit 已由 GPUI 链接进来；`sharedWorkspace` 和 `accessibilityDisplayShouldIncreaseContrast`
        // 都是不带参数的方法，前者返回对象，后者返回 BOOL（一个字节），按对应的签名调用 objc_msgSend。
        unsafe {
            let class = objc_getClass(c"NSWorkspace".as_ptr());
            if class.is_null() {
                return false;
            }
            let send_object: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let workspace = send_object(class, sel_registerName(c"sharedWorkspace".as_ptr()));
            if workspace.is_null() {
                return false;
            }
            let send_bool: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i8 =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            send_bool(workspace, sel_registerName(c"accessibilityDisplayShouldIncreaseContrast".as_ptr())) != 0
        }
    }
}
