//! 密钥输入框的“显示 / 隐藏”按钮。
//!
//! 输入框默认以圆点显示。点眼睛按钮时：
//! - [`Guard::None`]：直接切换（欢迎页里用户自己刚输入、还没保存的密钥）。
//! - [`Guard::Verify`]：先经系统验证当前登录用户（Windows 凭据对话框，可用 PIN / Windows Hello；
//!   macOS 用 Touch ID 或登录密码），通过后才显示。输入框为空、密钥已存在钥匙串里时，
//!   验证通过后再从钥匙串读出填入；重新隐藏时，若值没被改过就清空，不让明文留在输入框里。
//!
//! 验证通过后 [`GRACE`] 内再次显示不用重新验证。其他平台（Linux）没有可用的系统验证：
//! 只能显示用户本次输入的内容，不从钥匙串回显。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::InputState;
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::*;
use rust_i18n::t;

use crate::announce::announce;

/// 从存储里读出已保存的密钥（在后台线程调用）。
pub type Loader = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// 显示密钥前要不要验证。
#[derive(Clone)]
pub enum Guard {
    /// 不验证，直接切换。
    None,
    /// 先经系统验证；带 [`Loader`] 时输入框为空就从存储读出已保存的值。
    Verify(Option<Loader>),
}

/// 验证通过后多久内再显示不必重新验证。
const GRACE: Duration = Duration::from_secs(60);

thread_local! {
    static VERIFIED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    /// 每个输入框（按实体 id）的显示状态。
    static STATES: RefCell<HashMap<EntityId, RevealState>> = RefCell::new(HashMap::new());
}

fn with_state<R>(input: &Entity<InputState>, f: impl FnOnce(&mut RevealState) -> R) -> R {
    STATES.with(|states| {
        let mut states = states.borrow_mut();
        let result = f(states.entry(input.entity_id()).or_default());
        states.retain(|_, state| state.busy || state.loaded.is_some() || state.error.is_some());
        result
    })
}

/// 输入框最近一次验证失败的原因（给调用方显示成输入框下方的提示）。
pub fn error(input: &Entity<InputState>) -> Option<SharedString> {
    STATES.with(|states| states.borrow().get(&input.entity_id()).and_then(|state| state.error.clone()))
}

fn set_error(input: &Entity<InputState>, error: Option<SharedString>, cx: &mut App) {
    if let Some(error) = &error {
        announce(error.clone(), cx);
    }
    with_state(input, |state| state.error = error);
    cx.refresh_windows();
}

fn recently_verified() -> bool {
    VERIFIED_AT.with(|at| at.get().is_some_and(|at| at.elapsed() < GRACE))
}

#[derive(Default)]
struct RevealState {
    /// 从存储读出并填进输入框的值；隐藏时若输入框仍是它就清空。
    loaded: Option<SharedString>,
    /// 最近一次验证的错误，显示在输入框下方；下次点击时清除。
    error: Option<SharedString>,
    /// 正在等系统验证，避免重复弹窗。
    busy: bool,
}

/// 放在 `Input::suffix` 里的眼睛按钮。`id` 在同一窗口里要唯一。
pub fn toggle(id: impl Into<ElementId>, input: &Entity<InputState>, guard: Guard, cx: &App) -> impl IntoElement {
    let masked = input.read(cx).presentation().is_masked();
    let label = if masked { t!("secret.show") } else { t!("secret.hide") }.to_string();
    let input = input.clone();
    Button::new(id.into())
        .icon(if masked { IconName::Eye } else { IconName::EyeOff })
        .accessibility_label(label.clone())
        .tooltip(label)
        .xsmall()
        .ghost()
        .on_click(move |_, window, cx| on_click(&input, &guard, window, cx))
}

fn on_click(input: &Entity<InputState>, guard: &Guard, window: &mut Window, cx: &mut App) {
    if error(input).is_some() {
        set_error(input, None, cx);
    }
    if !input.read(cx).presentation().is_masked() {
        hide(input, window, cx);
        return;
    }
    let loader = match guard {
        Guard::None => {
            show(input, None, window, cx);
            return;
        }
        Guard::Verify(loader) => loader.clone(),
    };
    if with_state(input, |state| state.busy) {
        return;
    }
    let empty = input.read(cx).value().is_empty();
    let loader = loader.filter(|_| empty);
    if empty && loader.is_none() {
        return;
    }
    if !auth::SUPPORTED {
        // 没有系统验证：只显示用户自己输入的内容。
        if empty {
            set_error(input, Some(t!("secret.unavailable").to_string().into()), cx);
        } else {
            show(input, None, window, cx);
        }
        return;
    }
    let verified = recently_verified();
    let parent = auth::parent(window);
    with_state(input, |state| state.busy = true);
    let input = input.clone();
    let executor = cx.background_executor().clone();
    window
        .spawn(cx, async move |cx| {
            let result = if verified {
                Ok(true)
            } else {
                let (caption, message) = (t!("secret.verify_title").to_string(), t!("secret.verify_reason").to_string());
                executor.spawn(async move { auth::verify(parent, &caption, &message) }).await
            };
            let value = match (&result, loader) {
                (Ok(true), Some(loader)) => executor.spawn(async move { loader() }).await,
                _ => None,
            };
            cx.update(|window, cx| {
                with_state(&input, |state| state.busy = false);
                match result {
                    Ok(true) => {
                        VERIFIED_AT.with(|at| at.set(Some(Instant::now())));
                        if input.read(cx).value().is_empty() && value.is_none() {
                            set_error(&input, Some(t!("secret.not_found").to_string().into()), cx);
                            return;
                        }
                        let value = value.map(SharedString::from);
                        with_state(&input, |state| state.loaded = value.clone());
                        show(&input, value, window, cx);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, "身份验证失败");
                        set_error(&input, Some(error.into()), cx);
                    }
                }
            })
            .ok();
        })
        .detach();
}

fn show(input: &Entity<InputState>, value: Option<SharedString>, window: &mut Window, cx: &mut App) {
    input.update(cx, |input, cx| {
        if let Some(value) = value {
            input.set_value(value, window, cx);
        }
        input.set_masked(false, window, cx);
    });
}

fn hide(input: &Entity<InputState>, window: &mut Window, cx: &mut App) {
    let loaded = with_state(input, |state| state.loaded.take());
    input.update(cx, |input, cx| {
        input.set_masked(true, window, cx);
        if loaded.is_some_and(|loaded| loaded == input.value()) {
            input.set_value("", window, cx);
        }
    });
}

#[cfg(windows)]
mod auth {
    //! `CredUIPromptForWindowsCredentialsW` 让当前用户输入密码 / PIN，再用 `LsaLogonUser` 校验，
    //! 并确认登录的就是当前进程的用户。

    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    use gpui_kit::Window;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use rust_i18n::t;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, ERROR_LOGON_FAILURE, HANDLE, HWND, LUID, NO_ERROR};
    use windows_sys::Win32::Security::Authentication::Identity::{
        LSA_STRING, LsaConnectUntrusted, LsaDeregisterLogonProcess, LsaFreeReturnBuffer, LsaLogonUser, LsaNtStatusToWinError,
        SECURITY_LOGON_TYPE,
    };
    use windows_sys::Win32::Security::Credentials::{CREDUI_INFOW, CREDUIWIN_ENUMERATE_CURRENT_USER, CredUIPromptForWindowsCredentialsW};
    use windows_sys::Win32::Security::{
        AllocateLocallyUniqueId, EqualSid, GetTokenInformation, QUOTA_LIMITS, TOKEN_QUERY, TOKEN_SOURCE, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub const SUPPORTED: bool = true;

    /// 对话框的父窗口（句柄按整数传到后台线程）。
    pub fn parent(window: &Window) -> isize {
        match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
            _ => 0,
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    /// 密码输错后最多重新弹出几次对话框。
    const ATTEMPTS: usize = 3;

    /// 阻塞直到用户完成或取消。取消返回 `Ok(false)`；密码错误时带着错误码重新弹出对话框
    /// （对话框里显示“用户名或密码不正确”），多次错误后返回 `Err`（给用户看的文字）。
    pub fn verify(parent: isize, caption: &str, message: &str) -> Result<bool, String> {
        let mut auth_error = 0u32;
        for _ in 0..ATTEMPTS {
            match prompt(parent, caption, message, auth_error)? {
                None => return Ok(false),
                Some(0) => return Ok(true),
                Some(code) => auth_error = code,
            }
        }
        Err(t!("secret.wrong_password").to_string())
    }

    /// 弹一次对话框并校验。`None` 是取消；`Some(0)` 通过；其他是 Win32 错误码（下次弹窗时显示）。
    fn prompt(parent: isize, caption: &str, message: &str, auth_error: u32) -> Result<Option<u32>, String> {
        let (caption, message) = (wide(caption), wide(message));
        let info = CREDUI_INFOW {
            cbSize: size_of::<CREDUI_INFOW>() as u32,
            hwndParent: parent as HWND,
            pszMessageText: message.as_ptr(),
            pszCaptionText: caption.as_ptr(),
            hbmBanner: null_mut(),
        };
        let mut package = 0u32;
        let mut buffer: *mut c_void = null_mut();
        let mut size = 0u32;
        let mut save = 0;
        // SAFETY: 各指针指向本函数里活着的局部变量；字符串以 0 结尾。
        let code = unsafe {
            CredUIPromptForWindowsCredentialsW(
                &info,
                auth_error,
                &mut package,
                null(),
                0,
                &mut buffer,
                &mut size,
                &mut save,
                CREDUIWIN_ENUMERATE_CURRENT_USER,
            )
        };
        match code {
            NO_ERROR => {}
            ERROR_CANCELLED => return Ok(None),
            code => return Err(t!("secret.verify_error", code = code).to_string()),
        }
        // SAFETY: buffer 是 CredUI 分配的 size 字节，用完清零后用 CoTaskMemFree 释放。
        unsafe {
            let result = logon(package, buffer, size).map(Some);
            let bytes = buffer.cast::<u8>();
            for offset in 0..size as usize {
                bytes.add(offset).write_volatile(0);
            }
            CoTaskMemFree(buffer);
            result
        }
    }

    /// 用 CredUI 打包好的凭据登录一次，确认是当前用户。
    /// 返回 0 表示通过，否则是 Win32 错误码。
    unsafe fn logon(package: u32, buffer: *const c_void, size: u32) -> Result<u32, String> {
        // SAFETY: 调用方保证 buffer 有 size 字节；返回的句柄、缓冲区都在本函数里释放。
        unsafe {
            let mut lsa: HANDLE = null_mut();
            let status = LsaConnectUntrusted(&mut lsa);
            if status != 0 {
                return Err(t!("secret.verify_error", code = format!("{status:#x}")).to_string());
            }
            let origin = *b"dshr";
            let origin = LSA_STRING { Length: 4, MaximumLength: 4, Buffer: origin.as_ptr().cast_mut() };
            let mut source =
                TOKEN_SOURCE { SourceName: [b'd' as i8, b's' as i8, b'h' as i8, b'r' as i8, 0, 0, 0, 0], ..Default::default() };
            AllocateLocallyUniqueId(&mut source.SourceIdentifier);
            let mut profile: *mut c_void = null_mut();
            let mut profile_size = 0u32;
            let mut luid = LUID::default();
            let mut token: HANDLE = null_mut();
            let mut quotas = QUOTA_LIMITS::default();
            let mut substatus = 0;
            let status = LsaLogonUser(
                lsa,
                &origin,
                SECURITY_LOGON_TYPE::Interactive,
                package,
                buffer,
                size,
                null(),
                &source,
                &mut profile,
                &mut profile_size,
                &mut luid,
                &mut token,
                &mut quotas,
                &mut substatus,
            );
            if !profile.is_null() {
                LsaFreeReturnBuffer(profile);
            }
            LsaDeregisterLogonProcess(lsa);
            if status != 0 {
                tracing::debug!(status = format!("{status:#x}"), "LsaLogonUser 未通过");
                return Ok(match LsaNtStatusToWinError(status) {
                    0 => ERROR_LOGON_FAILURE,
                    code => code,
                });
            }
            let mut own: HANDLE = null_mut();
            let same = OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut own) != 0 && same_user(token, own);
            if !own.is_null() {
                CloseHandle(own);
            }
            CloseHandle(token);
            Ok(if same { 0 } else { ERROR_LOGON_FAILURE })
        }
    }

    /// 两个令牌是不是同一个用户。
    unsafe fn same_user(a: HANDLE, b: HANDLE) -> bool {
        // SAFETY: 两个句柄都是有效令牌；缓冲区按 u64 对齐，够放 TOKEN_USER。
        unsafe {
            let user = |token: HANDLE| -> Option<Vec<u64>> {
                let mut needed = 0u32;
                GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed);
                let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
                (GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), (buffer.len() * 8) as u32, &mut needed) != 0)
                    .then_some(buffer)
            };
            match (user(a), user(b)) {
                (Some(a), Some(b)) => {
                    let (a, b) = (&*a.as_ptr().cast::<TOKEN_USER>(), &*b.as_ptr().cast::<TOKEN_USER>());
                    EqualSid(a.User.Sid, b.User.Sid) != 0
                }
                _ => false,
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod auth {
    //! LocalAuthentication：`LAPolicyDeviceOwnerAuthentication`（Touch ID、Apple Watch，或登录密码）。

    use block2::RcBlock;
    use gpui_kit::Window;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSError, NSString};
    use objc2_local_authentication::{LAContext, LAError, LAPolicy};
    use rust_i18n::t;

    pub const SUPPORTED: bool = true;

    pub fn parent(_: &Window) -> isize {
        0
    }

    /// 阻塞直到用户完成或取消。取消返回 `Ok(false)`；失败返回 `Err`（给用户看的文字）。
    pub fn verify(_parent: isize, _caption: &str, message: &str) -> Result<bool, String> {
        // SAFETY: LAContext 在本函数里创建、用到回调返回为止；回调只读 NSError。
        unsafe {
            let context = LAContext::new();
            let policy = LAPolicy::DeviceOwnerAuthentication;
            if let Err(error) = context.canEvaluatePolicy_error(policy) {
                return Err(error.localizedDescription().to_string());
            }
            let (sender, receiver) = std::sync::mpsc::channel();
            let reply = RcBlock::new(move |ok: Bool, error: *mut NSError| {
                let result = if ok.as_bool() {
                    Ok(true)
                } else {
                    match error.as_ref() {
                        Some(error)
                            if [LAError::UserCancel, LAError::SystemCancel, LAError::AppCancel].contains(&LAError(error.code())) =>
                        {
                            Ok(false)
                        }
                        // 系统对话框里多次验证失败后才回调到这里：给出明确的“密码错误”提示。
                        Some(error) if LAError(error.code()) == LAError::AuthenticationFailed => {
                            Err(t!("secret.wrong_password").to_string())
                        }
                        Some(error) => Err(error.localizedDescription().to_string()),
                        None => Ok(false),
                    }
                };
                let _ = sender.send(result);
            });
            context.evaluatePolicy_localizedReason_reply(policy, &NSString::from_str(message), &reply);
            let result = receiver.recv().unwrap_or(Ok(false));
            drop(context);
            result
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod auth {
    use gpui_kit::Window;

    pub const SUPPORTED: bool = false;

    pub fn parent(_: &Window) -> isize {
        0
    }

    pub fn verify(_: isize, _: &str, _: &str) -> Result<bool, String> {
        Ok(false)
    }
}
