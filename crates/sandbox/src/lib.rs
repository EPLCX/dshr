//! 平台沙箱，配合权限模式使用：
//! - macOS：命令放进 `sandbox-exec`（Seatbelt），只允许写工作区、临时目录和常见的工具缓存目录。
//!   写入范围有了保证，“工作区内修改”模式下执行命令就不必再逐条询问。
//! - Windows：命令放进 Job Object。超时或被用户中断时整棵进程树一起结束，不会留下孤儿进程；
//!   Job Object 不限制文件写入，所以命令仍然需要询问。
//! - macOS / Linux：命令自成一个进程组，超时或被中断时整组结束（[`ProcessTree`] 的 Unix 版）。

use std::ffi::OsString;
use std::path::Path;

/// 当前平台能否把命令的写入限制在工作区内。
pub fn confines_writes() -> bool {
    cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).exists()
}

/// 沙箱对外提供的能力。权限策略据此决定命令要不要询问；没有沙箱时按“不限制写入”处理。
pub trait Sandbox: Send + Sync {
    /// 命令的写入是否被限制在工作区内。
    fn confines_writes(&self) -> bool;
}

/// 当前平台自带的沙箱。
pub struct Platform;

impl Sandbox for Platform {
    fn confines_writes(&self) -> bool {
        confines_writes()
    }
}

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Seatbelt profile：默认放行，再收回所有写权限，只开放工作区、临时目录与工具缓存。
const PROFILE: &str = r#"(version 1)
(allow default)
(deny file-write*)
(allow file-write*
  (subpath (param "WORKSPACE"))
  (subpath "/private/tmp")
  (subpath "/private/var/folders")
  (subpath (string-append (param "HOME") "/.cargo"))
  (subpath (string-append (param "HOME") "/.rustup"))
  (subpath (string-append (param "HOME") "/.cache"))
  (subpath (string-append (param "HOME") "/.npm"))
  (subpath (string-append (param "HOME") "/Library/Caches"))
  (literal "/dev/null")
  (literal "/dev/zero")
  (regex #"^/dev/tty")
  (regex #"^/dev/fd/"))
"#;

/// 运行一条 shell 命令的程序与参数。`confine` 为真时（仅 macOS）包进 `sandbox-exec`。
pub fn shell_command(command: &str, workspace: &Path, confine: bool) -> (OsString, Vec<OsString>) {
    if cfg!(windows) {
        return ("cmd".into(), vec!["/C".into(), command.into()]);
    }
    if !confine {
        return ("sh".into(), vec!["-c".into(), command.into()]);
    }
    // Seatbelt 按真实路径匹配（/var 实际是 /private/var），先解析符号链接。
    let workspace = workspace.canonicalize().unwrap_or_else(|_| workspace.to_owned());
    let home = std::env::var_os("HOME").unwrap_or_default();
    let mut workspace_param = OsString::from("WORKSPACE=");
    workspace_param.push(workspace.as_os_str());
    let mut home_param = OsString::from("HOME=");
    home_param.push(home);
    (
        SANDBOX_EXEC.into(),
        vec![
            "-p".into(),
            PROFILE.into(),
            "-D".into(),
            workspace_param,
            "-D".into(),
            home_param,
            "/bin/sh".into(),
            "-c".into(),
            command.into(),
        ],
    )
}

#[cfg(windows)]
pub use windows::ProcessTree;

#[cfg(unix)]
pub use unix::ProcessTree;

#[cfg(unix)]
mod unix {
    /// 一条命令及其所有子进程：命令要以自己为组长启动（`Command::process_group(0)`），这里记下组号。
    /// 丢弃时（超时、中断）给整组发 `SIGKILL`；正常结束后调用 [`ProcessTree::release`]，
    /// 命令自己启动的后台程序可以继续运行。与 Windows 的 Job Object 版本用法相同。
    pub struct ProcessTree {
        group: Option<libc::pid_t>,
    }

    impl ProcessTree {
        /// `pid`：以自己为组长启动的命令的进程号，也就是组号。
        pub fn adopt(pid: u32) -> Self {
            Self { group: libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0) }
        }

        /// 命令正常结束：不再在丢弃时结束剩下的进程。
        pub fn release(mut self) {
            self.group = None;
        }

        /// 现在就结束整组进程。
        pub fn kill(&mut self) {
            if let Some(group) = self.group.take() {
                // 组里已经没有进程时返回 ESRCH，忽略即可。
                unsafe { libc::killpg(group, libc::SIGKILL) };
            }
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            self.kill();
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::RawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    /// 一条命令及其所有子进程。丢弃时（超时、中断）结束整棵进程树；正常结束后调用
    /// [`ProcessTree::release`]，命令自己启动的后台程序可以继续运行。
    pub struct ProcessTree(HANDLE);

    // 句柄只在创建它的工具调用里使用，Job Object 句柄本身可以跨线程。
    unsafe impl Send for ProcessTree {}
    unsafe impl Sync for ProcessTree {}

    impl ProcessTree {
        pub fn new() -> io::Result<Self> {
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let tree = Self(job);
            tree.set_kill_on_close(true)?;
            Ok(tree)
        }

        /// 把进程加进来；它之后启动的子进程自动属于同一个 Job。
        pub fn assign(&self, process: RawHandle) -> io::Result<()> {
            if unsafe { AssignProcessToJobObject(self.0, process as HANDLE) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// 命令正常结束：不再在关闭时结束剩下的进程。
        pub fn release(self) {
            let _ = self.set_kill_on_close(false);
        }

        fn set_kill_on_close(&self, kill: bool) -> io::Result<()> {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            if kill {
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            }
            let ok = unsafe {
                SetInformationJobObject(
                    self.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_command_per_platform() {
        let (program, args) = shell_command("echo hi", Path::new("/ws"), false);
        if cfg!(windows) {
            assert_eq!(program, "cmd");
            assert_eq!(args, ["/C", "echo hi"]);
        } else {
            assert_eq!(program, "sh");
            assert_eq!(args, ["-c", "echo hi"]);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn confined_command_uses_sandbox_exec() {
        let (program, args) = shell_command("touch x", Path::new("/tmp"), true);
        assert_eq!(program, SANDBOX_EXEC);
        assert!(args.iter().any(|arg| arg.to_string_lossy().starts_with("WORKSPACE=/private/tmp")));
    }

    #[cfg(windows)]
    #[test]
    fn process_tree_can_be_created_and_released() {
        ProcessTree::new().unwrap().release();
    }
}
