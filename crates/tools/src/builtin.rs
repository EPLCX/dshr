use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{self, BufRead as _, Read as _};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;

use crate::edit::change_report_blocking;
use crate::{Access, MAX_OUTPUT_CHARS, ShellCommand, Tool, ToolContext, ToolOutput, WrapCommand, parse, truncate, write_access};

pub struct ReadFile;

#[derive(Deserialize)]
struct ReadFileInput {
    path: String,
    /// 起始行（从 1 开始）。
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file; lines are numbered from 1. For large files, read the part you need with offset \
         (first line) and limit (line count)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to the workspace or absolute."},
                "offset": {"type": "integer", "minimum": 1},
                "limit": {"type": "integer", "minimum": 1}
            },
            "required": ["path"]
        })
    }

    fn access(&self, input: &Value, ctx: &ToolContext) -> Access {
        input.get("path").and_then(Value::as_str).map_or(Access::Read, |path| ctx.read_access(path))
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let input: ReadFileInput = match parse(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let path = ctx.resolve(&input.path);
        let start = input.offset.unwrap_or(1).max(1);
        let limit = input.limit.unwrap_or(usize::MAX);
        let read = tokio::task::spawn_blocking(move || read_numbered(&path, start, limit)).await;
        match read {
            Ok(Ok(text)) => ToolOutput::ok(text),
            Ok(Err(error)) => ToolOutput::error(format!("无法读取 {}: {error}", input.path)),
            Err(error) => ToolOutput::error(format!("无法读取 {}: {error}", input.path)),
        }
    }
}

/// 单行最多读入的字节数。更长的行（如压缩过的 JS、没有换行的数据文件）只取开头，
/// 不会因为一行就把整个文件读进内存。
const MAX_LINE_BYTES: usize = MAX_OUTPUT_CHARS * 4;

/// 流式读出带行号的文本：从第 `start` 行起最多 `limit` 行，凑够 [`MAX_OUTPUT_CHARS`] 就停。
/// 内存占用只和输出上限有关，与文件大小无关；跳过的行只扫描、不保留。
fn read_numbered(path: &Path, start: usize, limit: usize) -> io::Result<String> {
    let mut reader = io::BufReader::with_capacity(64 * 1024, std::fs::File::open(path)?);
    let mut out = String::new();
    let mut chars = 0;
    let mut line = Vec::new();
    let (mut number, mut emitted) = (0, 0);
    let mut truncated = false;
    while emitted < limit {
        line.clear();
        let read = (&mut reader).take(MAX_LINE_BYTES as u64).read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        number += 1;
        let overlong = line.last() != Some(&b'\n') && read == MAX_LINE_BYTES;
        if overlong {
            // 这一行还没完：扫过剩余部分，下一轮从下一行开始。
            reader.skip_until(b'\n')?;
        }
        if number < start {
            continue;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        }
        let text = match std::str::from_utf8(&line) {
            Ok(text) => text,
            // 截断处可能切在一个多字节字符中间，只丢掉那半个字符。
            Err(error) if overlong && error.error_len().is_none() => std::str::from_utf8(&line[..error.valid_up_to()]).unwrap_or_default(),
            Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "stream did not contain valid UTF-8")),
        };
        if emitted > 0 {
            out.push('\n');
            chars += 1;
        }
        let before = out.len();
        let _ = write!(out, "{number:>6}\t");
        chars += out.len() - before + text.chars().count();
        out.push_str(text);
        emitted += 1;
        if overlong || chars > MAX_OUTPUT_CHARS {
            truncated = true;
            break;
        }
    }
    if truncated {
        if let Some((cut, _)) = out.char_indices().nth(MAX_OUTPUT_CHARS) {
            out.truncate(cut);
        }
        let _ = write!(out, "\n… [输出过长，已在第 {number} 行截断；用 offset / limit 分段读取其余部分]");
    }
    Ok(out)
}

pub struct WriteFile;

#[derive(Deserialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create a file, or replace a file's entire content (parent directories are created). To change part of an \
         existing file, use edit_file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }

    fn access(&self, input: &Value, ctx: &ToolContext) -> Access {
        write_access(input, ctx)
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let input: WriteFileInput = match parse(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let path = ctx.resolve(&input.path);
        if let Some(parent) = path.parent()
            && let Err(error) = tokio::fs::create_dir_all(parent).await
        {
            return ToolOutput::error(format!("无法创建目录 {}: {error}", parent.display()));
        }
        // 覆盖已有文件时一并返回差异，新文件视为从空内容开始。原文件太大时不读进内存、不比较
        // （日志、数据文件常有几百 MB），只从开头取样判断换行风格。
        let size = tokio::fs::metadata(&path).await.ok().filter(|meta| meta.is_file()).map(|meta| meta.len());
        let large = size.is_some_and(|size| size > MAX_DIFF_BYTES);
        let existing = if large { None } else { tokio::fs::read_to_string(&path).await.ok() };
        let crlf = match &existing {
            Some(existing) => crate::is_crlf(existing),
            None if large => sample_is_crlf(&path).await,
            None => false,
        };
        // 覆盖纯 CRLF 的文件、给的内容只有 LF（模型从 read_file 看到的就是 LF）：保持原来的换行，
        // 不然整个文件每一行都算改动，Windows 上的仓库还会多出一大片换行差异。
        let content = if crlf && !input.content.contains('\r') { crate::to_crlf(&input.content) } else { input.content };
        // 插件的 `beforeWrite`：可以拒绝，也可以改写要写的内容（差异按最终内容算）。原文件很大时和以前一样不问
        // （钩子本来就跳过超过 `MAX_WRITE_INTENT_BYTES` 的内容）。
        let (content, changed_by) = if large {
            (content, Vec::new())
        } else {
            match crate::apply_write_intents(ctx, "write_file", &input.path, existing.as_deref(), content) {
                Ok(result) => result,
                Err(reason) => return ToolOutput::error(format!("没有写入 {}：{reason}", input.path)),
            }
        };
        let before = existing.unwrap_or_default();
        match crate::write_atomic(path, content.clone()).await {
            Ok(()) if large => ToolOutput::ok(format!(
                "Wrote {} ({} bytes; the previous version was {} bytes, too large to diff)",
                input.path,
                content.len(),
                size.unwrap_or_default(),
            )),
            Ok(()) => {
                let report = change_report_blocking("Wrote", input.path, before, content).await + &crate::adjusted_note(&changed_by);
                ToolOutput::ok(truncate(report))
            }
            Err(error) => ToolOutput::error(format!("无法写入 {}: {error}", input.path)),
        }
    }
}

/// 原文件超过这个大小时，`write_file` 不读进内存比较差异。
const MAX_DIFF_BYTES: u64 = 1024 * 1024;

/// 大文件只看开头一段判断是不是纯 CRLF（取样截在最后一个换行处，免得切在 `\r\n` 中间）。
async fn sample_is_crlf(path: &Path) -> bool {
    let Ok(mut file) = tokio::fs::File::open(path).await else { return false };
    let mut sample = vec![0; 64 * 1024];
    let Ok(read) = file.read(&mut sample).await else { return false };
    sample.truncate(read);
    let end = sample.iter().rposition(|byte| *byte == b'\n').map_or(0, |index| index + 1);
    crate::is_crlf(&String::from_utf8_lossy(&sample[..end]))
}

pub struct ListDir;

#[derive(Deserialize)]
struct ListDirInput {
    #[serde(default = "current_dir")]
    path: String,
}

fn current_dir() -> String {
    ".".into()
}

#[async_trait::async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List the entries of a directory. Directories end with '/'."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string", "default": "."}}
        })
    }

    fn access(&self, input: &Value, ctx: &ToolContext) -> Access {
        ctx.read_access(input.get("path").and_then(Value::as_str).unwrap_or("."))
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let input: ListDirInput = match parse(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let mut reader = match tokio::fs::read_dir(ctx.resolve(&input.path)).await {
            Ok(reader) => reader,
            Err(error) => return ToolOutput::error(format!("无法列出 {}: {error}", input.path)),
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = reader.next_entry().await {
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                name.push('/');
            }
            names.push(name);
        }
        names.sort();
        ToolOutput::ok(truncate(names.join("\n")))
    }
}

pub struct RunShell;

#[derive(Deserialize)]
struct RunShellInput {
    command: String,
    timeout_ms: Option<u64>,
}

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// `CREATE_NO_WINDOW`：子进程不创建控制台窗口。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[async_trait::async_trait]
impl Tool for RunShell {
    fn name(&self) -> &str {
        "run_shell"
    }

    fn description(&self) -> &str {
        if cfg!(windows) {
            "Run a command with `cmd /C` in the workspace directory and return exit code, stdout and stderr (long \
             output is truncated). There is no terminal or input; commands that don't finish are killed at timeout_ms \
             (default 120 s). Use read_file, list_dir, grep and glob to read and search files."
        } else {
            "Run a command with `sh -c` in the workspace directory and return exit code, stdout and stderr (long \
             output is truncated). There is no terminal or input; commands that don't finish are killed at timeout_ms \
             (default 120 s). Use read_file, list_dir, grep and glob to read and search files."
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_ms": {"type": "integer", "minimum": 1, "default": DEFAULT_TIMEOUT_MS}
            },
            "required": ["command"]
        })
    }

    fn access(&self, _input: &Value, _ctx: &ToolContext) -> Access {
        Access::Execute
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let input: RunShellInput = match parse(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        // 插件可以改写要启动的进程：沙箱插件在 macOS 的“工作区内修改”模式下把它放进 Seatbelt。
        let mut shell = ShellCommand::new(&input.command);
        for wrap in ctx.plugins.hooks::<WrapCommand>() {
            wrap(&mut shell, ctx);
        }
        let mut command = Command::new(shell.program);
        command.args(shell.args);
        command.current_dir(&ctx.workspace).kill_on_drop(true);
        // 桌面版没有控制台，不加这个标志每条命令都会弹出一个黑色控制台窗口。
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // macOS / Linux：命令自成一个进程组，超时或中断时整组结束（不只是 sh 本身）。
        #[cfg(unix)]
        command.process_group(0);

        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let deadline = tokio::time::Instant::now() + timeout;
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return ToolOutput::error(format!("无法启动命令: {error}")),
        };
        // 命令和它启动的子进程归在一起：Windows 是 Job Object，Unix 是进程组。超时、中断（这个 future 被丢弃）时
        // 一起结束；正常结束后放开，命令自己留在后台的程序（如启动的服务）不跟着结束。
        #[cfg(windows)]
        let tree = dshr_sandbox::ProcessTree::new()
            .and_then(|tree| child.raw_handle().map_or(Ok(()), |handle| tree.assign(handle)).map(|()| tree))
            .ok();
        #[cfg(unix)]
        let mut tree = child.id().map(dshr_sandbox::ProcessTree::adopt);
        // stdout 与 stderr 边读边推给界面（终端面板），同时各自收集起来交给模型。
        let stdout = child.stdout.take().expect("stdout 已设为 piped");
        let stderr = child.stderr.take().expect("stderr 已设为 piped");
        let (mut out, mut err) = (Capture::default(), Capture::default());
        let waited = {
            let pumps = async {
                tokio::join!(pump(stdout, ctx, &mut out), pump(stderr, ctx, &mut err));
            };
            tokio::pin!(pumps);
            let finished = async {
                let mut drained = false;
                let status = tokio::select! {
                    status = child.wait() => status,
                    () = &mut pumps => {
                        drained = true;
                        child.wait().await
                    }
                };
                // 命令已经结束，但它留在后台的进程（`npm run dev &` 之类）继承了管道，管道要等它们退出才会关。
                // 只再读一小会儿把已经写出的内容收完，不等到超时。
                if !drained {
                    let _ = tokio::time::timeout(DRAIN_AFTER_EXIT, &mut pumps).await;
                }
                status
            };
            tokio::time::timeout_at(deadline, finished).await
        };
        let status = match waited {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => return ToolOutput::error(format!("等待命令结束失败: {error}")),
            Err(_) => {
                // 超时：结束整棵进程树（Windows 随 `tree` 丢弃时结束；child 本身由 kill_on_drop 结束）。
                #[cfg(unix)]
                if let Some(tree) = &mut tree {
                    tree.kill();
                }
                let (out_budget, err_budget) = split_budget(out.total, err.total);
                let text = format!(
                    "命令超时（{} ms），已结束\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    timeout.as_millis(),
                    out.render(out_budget),
                    err.render(err_budget),
                );
                return ToolOutput::error(truncate(text));
            }
        };
        if let Some(tree) = tree {
            tree.release();
        }
        let (stdout, stderr) = (out, err);
        let code = status.code().map_or("无（被信号终止）".to_owned(), |code| code.to_string());
        let (stdout_budget, stderr_budget) = split_budget(stdout.total, stderr.total);
        let text = format!(
            "exit code: {code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            stdout.render(stdout_budget),
            stderr.render(stderr_budget),
        );
        ToolOutput { content: truncate(text), is_error: !status.success() }
    }
}

/// 每个输出流在内存里保留的开头与结尾字节数；中间的部分只计数、不保留。
/// 不管命令打印多少，占用都不超过这个上限（原来是全部攒在内存里，结束后才截断）。
const CAPTURE_HEAD_BYTES: usize = 32 * 1024;
const CAPTURE_TAIL_BYTES: usize = 32 * 1024;
/// 交给模型的 stdout + stderr 总字节数，留出一点余量给标题行，不超过 [`MAX_OUTPUT_CHARS`]。
const RESULT_BUDGET_BYTES: usize = MAX_OUTPUT_CHARS - 2_000;
/// 命令结束后管道最多再读多久：留在后台的进程会一直占着管道，不能等它们关。
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(300);
/// 实时输出合并推送的间隔：输出再快，界面每个流每秒也只收到约 20 条消息。
const LIVE_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
/// 两次推送之间最多攒的实时输出（终端面板本来就只显示最后一小段）。
const LIVE_PENDING_BYTES: usize = 64 * 1024;

/// 一个输出流的有界收集：保留开头和结尾。
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}

impl Capture {
    fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len();
        let room = CAPTURE_HEAD_BYTES - self.head.len();
        let (head, rest) = bytes.split_at(room.min(bytes.len()));
        self.head.extend_from_slice(head);
        if rest.len() >= CAPTURE_TAIL_BYTES {
            self.tail.clear();
            self.tail.extend(&rest[rest.len() - CAPTURE_TAIL_BYTES..]);
        } else if !rest.is_empty() {
            let overflow = (self.tail.len() + rest.len()).saturating_sub(CAPTURE_TAIL_BYTES);
            self.tail.drain(..overflow);
            self.tail.extend(rest);
        }
    }

    /// 在 `budget` 字节内给出文本：放得下就是全文，否则是开头 + 省略说明 + 结尾
    /// （命令的报错通常在最后，所以结尾占一半以上）。
    fn render(&self, budget: usize) -> String {
        let kept: Vec<u8> = self.head.iter().chain(&self.tail).copied().collect();
        if self.total <= budget && self.total == kept.len() {
            return String::from_utf8_lossy(&kept).into_owned();
        }
        let head = &kept[..(budget * 2 / 5).min(kept.len())];
        let tail = &kept[kept.len() - (budget - head.len()).min(kept.len() - head.len())..];
        // 切口对齐到字符边界，免得多出替换字符。
        let head = &head[..head.len() - incomplete_suffix(head)];
        let tail = &tail[tail.iter().take_while(|byte| (**byte & 0xC0) == 0x80).count()..];
        format!(
            "{}\n… [中间省略 {} 字节] …\n{}",
            String::from_utf8_lossy(head),
            self.total - head.len() - tail.len(),
            String::from_utf8_lossy(tail),
        )
    }
}

/// 末尾不完整的 UTF-8 序列的字节数（没有则为 0）。
fn incomplete_suffix(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Err(error) if error.error_len().is_none() => bytes.len() - error.valid_up_to(),
        _ => 0,
    }
}

/// 把结果预算分给 stdout 和 stderr：stderr 最多占一半，stdout 用不完的再让给 stderr。
fn split_budget(stdout: usize, stderr: usize) -> (usize, usize) {
    let stdout_budget = RESULT_BUDGET_BYTES - stderr.min(RESULT_BUDGET_BYTES / 2);
    (stdout_budget, RESULT_BUDGET_BYTES - stdout.min(stdout_budget))
}

/// 读完一个输出流：实时输出按 [`LIVE_FLUSH_INTERVAL`] 合并后推给界面，有界地收集进 `capture`。
/// 收集是边读边写进 `capture` 的：这个 future 被中途丢弃（超时、后台进程占着管道）时，已读到的内容仍在。
async fn pump(mut reader: impl AsyncRead + Unpin, ctx: &ToolContext, capture: &mut Capture) {
    let mut buffer = [0u8; 8192];
    // 一个多字节字符可能被切在两次读取之间，留到下一块一起解码。
    let mut carry: Vec<u8> = Vec::new();
    // 已解码、还没推给界面的文本，以及最晚什么时候推。
    let mut live = String::new();
    let mut flush_at = Instant::now();
    loop {
        let read = if live.is_empty() {
            reader.read(&mut buffer).await
        } else {
            tokio::select! {
                biased;
                read = reader.read(&mut buffer) => read,
                // 输出停顿了：把攒着的先推出去，不等下一块。
                () = tokio::time::sleep_until(flush_at.into()) => {
                    ctx.emit_output(&live);
                    live.clear();
                    continue;
                }
            }
        };
        let read = match read {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        capture.push(&buffer[..read]);
        if ctx.output.is_none() {
            continue;
        }
        carry.extend_from_slice(&buffer[..read]);
        let valid = carry.len() - incomplete_suffix(&carry);
        if live.is_empty() {
            flush_at = Instant::now() + LIVE_FLUSH_INTERVAL;
        }
        live.push_str(&String::from_utf8_lossy(&carry[..valid]));
        carry.drain(..valid);
        // 输出太快时只留最后一段，等待推送的文本不会越攒越多。
        if live.len() > LIVE_PENDING_BYTES {
            let mut cut = live.len() - LIVE_PENDING_BYTES;
            while !live.is_char_boundary(cut) {
                cut += 1;
            }
            live.drain(..cut);
        }
        if Instant::now() >= flush_at {
            ctx.emit_output(&live);
            live.clear();
        }
    }
    live.push_str(&String::from_utf8_lossy(&carry));
    if !live.is_empty() {
        ctx.emit_output(&live);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[tokio::test]
    async fn write_read_list() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_owned());

        let out = WriteFile.execute(json!({"path": "a/b.txt", "content": "one\ntwo\nthree"}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);

        let out = ReadFile.execute(json!({"path": "a/b.txt", "offset": 2, "limit": 1}), &ctx).await;
        assert_eq!(out.content, "     2\ttwo");

        let out = ListDir.execute(json!({}), &ctx).await;
        assert_eq!(out.content, "a/");

        let out = ReadFile.execute(json!({"nope": 1}), &ctx).await;
        assert!(out.is_error);

        // 覆盖 CRLF 文件：给的是 LF 也保持 CRLF，只有真正改了的行算改动。
        std::fs::write(dir.path().join("w.txt"), "one\r\ntwo\r\n").unwrap();
        let out = WriteFile.execute(json!({"path": "w.txt", "content": "one\nTWO\n"}), &ctx).await;
        assert!(out.content.starts_with("Wrote w.txt (+1 -1)"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("w.txt")).unwrap(), "one\r\nTWO\r\n");
    }

    #[tokio::test]
    async fn overwriting_a_huge_file_skips_the_diff_but_keeps_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_owned());
        let huge: String = (0..200_000).map(|n| format!("row {n}\r\n")).collect();
        assert!(huge.len() as u64 > MAX_DIFF_BYTES);
        std::fs::write(dir.path().join("big.log"), &huge).unwrap();
        let out = WriteFile.execute(json!({"path": "big.log", "content": "a\nb\n"}), &ctx).await;
        assert!(!out.is_error && out.content.contains("too large to diff"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("big.log")).unwrap(), "a\r\nb\r\n");
    }

    #[tokio::test]
    async fn read_file_is_bounded_and_keeps_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_owned());

        // 末尾没有换行、带 CRLF、空文件。
        std::fs::write(dir.path().join("crlf.txt"), "a\r\nb\r\nc").unwrap();
        let out = ReadFile.execute(json!({"path": "crlf.txt"}), &ctx).await;
        assert_eq!(out.content, "     1\ta\n     2\tb\n     3\tc");
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        assert_eq!(ReadFile.execute(json!({"path": "empty.txt"}), &ctx).await.content, "");

        // 大文件：输出停在上限附近，并说明从哪里继续。
        let big: String = (1..=20_000).map(|n| format!("line number {n}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();
        let out = ReadFile.execute(json!({"path": "big.txt"}), &ctx).await;
        assert!(!out.is_error);
        assert!(out.content.starts_with("     1\tline number 1\n     2\tline number 2\n"));
        assert!(out.content.chars().count() < crate::MAX_OUTPUT_CHARS + 200);
        assert!(out.content.contains("已在第"));
        let out = ReadFile.execute(json!({"path": "big.txt", "offset": 19_999}), &ctx).await;
        assert_eq!(out.content, " 19999\tline number 19999\n 20000\tline number 20000");

        // 没有换行的超长行只取开头，后面的行号不受影响。
        let long = format!("{}\nnext\n", "é".repeat(MAX_LINE_BYTES));
        std::fs::write(dir.path().join("long.txt"), &long).unwrap();
        let out = ReadFile.execute(json!({"path": "long.txt"}), &ctx).await;
        assert!(!out.is_error && out.content.contains("已在第 1 行截断"));
        let out = ReadFile.execute(json!({"path": "long.txt", "offset": 2}), &ctx).await;
        assert_eq!(out.content, "     2\tnext");

        // 二进制文件仍然报错。
        std::fs::write(dir.path().join("bin"), [0xFFu8, 0xFE, 0x00, 0x01]).unwrap();
        assert!(ReadFile.execute(json!({"path": "bin"}), &ctx).await.is_error);
    }

    #[test]
    fn capture_keeps_head_and_tail_only() {
        let mut capture = Capture::default();
        for chunk in 0..2_000 {
            capture.push(format!("{chunk:07}\n").repeat(64).as_bytes());
        }
        assert_eq!(capture.total, 2_000 * 64 * 8);
        assert!(capture.head.len() + capture.tail.len() <= CAPTURE_HEAD_BYTES + CAPTURE_TAIL_BYTES);

        let text = capture.render(4_000);
        assert!(text.starts_with("0000000\n"));
        assert!(text.ends_with("0001999\n"));
        assert!(text.contains("中间省略"));
        assert!(text.len() < 4_200);

        // 放得下时原样返回。
        let mut small = Capture::default();
        small.push("你好\n".as_bytes());
        assert_eq!(small.render(100), "你好\n");

        // 多字节字符不会被切成两半。
        let mut wide = Capture::default();
        wide.push("好".repeat(10_000).as_bytes());
        assert!(!wide.render(1_000).contains('\u{FFFD}'));

        assert_eq!(split_budget(10, 10), (RESULT_BUDGET_BYTES - 10, RESULT_BUDGET_BYTES - 10));
        let (out, err) = split_budget(1_000_000, 1_000_000);
        assert_eq!(out + err, RESULT_BUDGET_BYTES);
        let (out, err) = split_budget(1_000_000, 100);
        assert_eq!((out, err), (RESULT_BUDGET_BYTES - 100, 100));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_output_flood_stays_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path().to_owned());
        let events = Arc::new(Mutex::new((0usize, 0usize)));
        let sink = events.clone();
        ctx.output = Some(crate::OutputSink(Arc::new(move |text: &str| {
            let mut events = sink.lock().unwrap();
            events.0 += 1;
            events.1 = events.1.max(text.len());
        })));
        // 约 60 MB 输出。
        let out = RunShell
            .execute(json!({"command": "yes 0123456789abcdef0123456789abcdef | head -c 60000000; echo; echo END-OF-OUTPUT"}), &ctx)
            .await;
        assert!(!out.is_error, "{}", &out.content[..200.min(out.content.len())]);
        assert!(out.content.chars().count() <= crate::MAX_OUTPUT_CHARS + 100);
        assert!(out.content.contains("中间省略"));
        assert!(out.content.contains("END-OF-OUTPUT"), "结尾要保留");
        let (count, largest) = *events.lock().unwrap();
        // 原来每 4 KB 一条（约 15000 条）；现在按时间合并。
        assert!(count < 2_000, "实时输出事件太多：{count}");
        assert!(largest <= LIVE_PENDING_BYTES);
    }

    /// 命令留下后台进程（继承了输出管道）时，不等到超时：命令结束后很快返回，已输出的内容也在。
    #[cfg(unix)]
    #[tokio::test]
    async fn background_process_does_not_hold_the_call_until_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_owned());
        let marker = format!("dshr-bg-{}", std::process::id());
        let start = Instant::now();
        let out = RunShell
            .execute(json!({"command": format!("echo started; sleep 30 & echo {marker} >/dev/null"), "timeout_ms": 20_000}), &ctx)
            .await;
        assert!(start.elapsed() < Duration::from_secs(5), "等了 {:?}", start.elapsed());
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("exit code: 0") && out.content.contains("started"), "{}", out.content);
    }

    /// 超时时整组结束：sh 启动的子进程不会留下来。
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_owned());
        let pid_file = dir.path().join("child.pid");
        let command = format!("echo partial; sleep 30 & echo $! > {}; wait", pid_file.display());
        let out = RunShell.execute(json!({"command": command, "timeout_ms": 500}), &ctx).await;
        assert!(out.is_error && out.content.starts_with("命令超时"), "{}", out.content);
        assert!(out.content.contains("partial"), "超时也要带上已有的输出：{}", out.content);
        let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        // 进程不在了，或者只剩僵尸（已经结束、等着被回收）都算结束了。
        let state = std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().unwrap();
        let state = String::from_utf8_lossy(&state.stdout);
        let alive = !state.trim().is_empty() && !state.trim_start().starts_with('Z');
        assert!(!alive, "超时后子进程 {pid} 还在运行");
    }

    #[tokio::test]
    async fn shell_reports_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_owned());
        let out = RunShell.execute(json!({"command": "echo hello"}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("exit code: 0"));
        assert!(out.content.contains("hello"));

        // 实时输出逐块推给订阅方。
        let streamed = Arc::new(Mutex::new(String::new()));
        let mut live = ctx.clone();
        let sink = streamed.clone();
        live.output = Some(crate::OutputSink(Arc::new(move |text: &str| sink.lock().unwrap().push_str(text))));
        RunShell.execute(json!({"command": "echo streamed"}), &live).await;
        assert!(streamed.lock().unwrap().contains("streamed"));

        let out = RunShell.execute(json!({"command": "exit 3"}), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("exit code: 3"));
    }
}
