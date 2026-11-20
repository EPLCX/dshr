//! 启动计时：设 `DSHR_STARTUP_LOG=<文件路径>` 时，把各阶段相对进程启动的毫秒数追加写进这个文件。
//! 没设时不分配、不写文件（环境变量只在 `init` 时读一次）。临时诊断用，稳定后可以整个删掉。

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();
/// 日志文件；`None` 表示没开启。
static LOG: OnceLock<Option<PathBuf>> = OnceLock::new();

/// 进程入口尽早调用：记下起点，读一次 `DSHR_STARTUP_LOG`。
pub fn init() {
    START.get_or_init(Instant::now);
    LOG.get_or_init(log_path);
}

/// 相对路径按启动时的工作目录换成绝对路径，之后工作目录变了也写到同一个文件。
fn log_path() -> Option<PathBuf> {
    let value = std::env::var_os("DSHR_STARTUP_LOG").filter(|value| !value.is_empty())?;
    std::path::absolute(PathBuf::from(value)).ok()
}

#[inline]
pub fn mark(stage: &str) {
    let Some(Some(path)) = LOG.get() else { return };
    let elapsed = START.get().map(Instant::elapsed).unwrap_or_default();
    let line = format!("{:>6} ms  {stage}\n", elapsed.as_millis());
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}
