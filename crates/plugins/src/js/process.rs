//! `host.spawn(program, args, { cwd, env })`：启动一个程序，边跑边读它的输出、往它的标准输入写东西。
//!
//! 和 `exec` 一样要 `exec:<程序>` 能力、只在没标 `readOnly` 的工具和页面动作里能用、经过命令钩子（沙箱）。进程是这次
//! 调用打开的资源（[`super::ops::Resource`]）：调用结束（返回、超时、被中断）时资源被丢掉，进程随之结束。
//! 输出按行读；读的一方跟不上时缓冲最多 [`MAX_BUFFERED_LINES`] 行，满了就暂停读管道（程序写满管道后自己等着）。

use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::Mutex;

use super::host::State;
use super::ops::Resource;
use dshr_tools::{ShellCommand, WrapCommand};

/// 还没读走的输出最多缓冲多少行（每行最多 [`MAX_LINE_BYTES`]）。
const MAX_BUFFERED_LINES: usize = 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;

/// 一个启动了的程序。
pub(crate) struct Process {
    lines: Mutex<tokio::sync::mpsc::Receiver<(&'static str, String)>>,
    stdin: Mutex<Option<tokio::process::ChildStdin>>,
    child: Mutex<tokio::process::Child>,
}

impl State {
    pub(super) fn spawn(&self, program: &str, args: &str, options: &str) -> Result<u64, String> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Options {
            cwd: Option<String>,
            env: std::collections::BTreeMap<String, String>,
        }
        if self.capabilities.exec.is_empty() {
            return Err("this plugin does not declare any `exec:<program>` capability in plugin.toml".into());
        }
        if !self.capabilities.allows_program(program) {
            return Err(format!("this plugin may not run {program}: add `exec:{program}` to capabilities in plugin.toml"));
        }
        let args: Vec<String> = serde_json::from_str(args).map_err(|_| "spawn arguments must be an array of strings".to_owned())?;
        let options: Options = serde_json::from_str(options).map_err(|error| format!("invalid spawn options: {error}"))?;
        let (command, cwd) = self.with_call_pub(|call| {
            if !call.permits.exec {
                return Err("running programs is only available to tools that are not declared readOnly and to page actions".into());
            }
            let cwd = super::host::confine_pub(&call.ctx, options.cwd.as_deref().unwrap_or("."))?;
            let mut shell = ShellCommand {
                script: std::iter::once(program).chain(args.iter().map(String::as_str)).collect::<Vec<_>>().join(" "),
                program: program.into(),
                args: args.iter().map(Into::into).collect(),
            };
            for wrap in call.ctx.plugins.hooks::<WrapCommand>() {
                wrap(&mut shell, &call.ctx);
            }
            Ok((shell, cwd))
        })?;
        let runtime = super::ops::io()?;
        let _entered = runtime.enter();
        let mut process = tokio::process::Command::new(&command.program);
        process
            .args(&command.args)
            .current_dir(&cwd)
            .envs(&options.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        super::host::hide_window(&mut process);
        let mut child = process.spawn().map_err(|error| format!("cannot start {}: {error}", command.script))?;
        let (sender, receiver) = tokio::sync::mpsc::channel(MAX_BUFFERED_LINES);
        for (name, pipe) in [
            ("stdout", child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)),
            ("stderr", child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)),
        ] {
            let Some(pipe) = pipe else { continue };
            let sender = sender.clone();
            runtime.spawn(async move {
                let mut reader = tokio::io::BufReader::new(pipe);
                let mut buffer = Vec::new();
                loop {
                    buffer.clear();
                    match reader.read_until(b'\n', &mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            while buffer.last().is_some_and(|byte| matches!(byte, b'\n' | b'\r')) {
                                buffer.pop();
                            }
                            buffer.truncate(MAX_LINE_BYTES);
                            let line = String::from_utf8_lossy(&buffer).into_owned();
                            if sender.send((name, line)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
        drop(sender);
        let process = Process { lines: Mutex::new(receiver), stdin: Mutex::new(child.stdin.take()), child: Mutex::new(child) };
        self.ops.add_resource(Resource::Process(Arc::new(process)))
    }

    fn process(&self, id: u64) -> Result<Arc<Process>, String> {
        match self.ops.resource(id) {
            Some(Resource::Process(process)) => Ok(process),
            _ => Err("the process is not running (it belongs to a call that has ended)".into()),
        }
    }

    /// 下一行输出：`{ stream, line }`；两路输出都结束时是 `null`。
    pub(super) fn proc_read(&self, id: u64) -> Result<u64, String> {
        let process = self.process(id)?;
        let work = async move {
            match process.lines.lock().await.recv().await {
                Some((stream, line)) => Ok(json!({ "stream": stream, "line": line }).to_string()),
                None => Ok("null".into()),
            }
        };
        self.ops.start(work, self.remaining())
    }

    /// 往标准输入写；`text` 是 `null` 时关掉标准输入（程序读到结尾）。
    pub(super) fn proc_write(&self, id: u64, text: Option<String>) -> Result<u64, String> {
        let process = self.process(id)?;
        let work = async move {
            let mut stdin = process.stdin.lock().await;
            match text {
                None => {
                    stdin.take();
                    Ok("null".into())
                }
                Some(text) => {
                    let pipe = stdin.as_mut().ok_or("the program's stdin is closed")?;
                    pipe.write_all(text.as_bytes()).await.map_err(|error| format!("cannot write to stdin: {error}"))?;
                    pipe.flush().await.map_err(|error| format!("cannot write to stdin: {error}"))?;
                    Ok("null".into())
                }
            }
        };
        self.ops.start(work, self.remaining())
    }

    /// 结束进程。
    pub(super) fn proc_kill(&self, id: u64) -> Result<(), String> {
        let process = self.process(id)?;
        let runtime = super::ops::io()?;
        runtime.spawn(async move {
            let _ = process.child.lock().await.start_kill();
        });
        Ok(())
    }

    /// 等进程结束：`{ code }`（被信号结束时 `code` 是 `null`）。
    pub(super) fn proc_wait(&self, id: u64) -> Result<u64, String> {
        let process = self.process(id)?;
        let work = async move {
            process.stdin.lock().await.take();
            let status = process.child.lock().await.wait().await.map_err(|error| format!("cannot wait for the program: {error}"))?;
            Ok(json!({ "code": status.code() }).to_string())
        };
        self.ops.start(work, self.remaining())
    }
}
