use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::event::{EventBody, LogEntry, SessionEvent};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("会话日志第 {line} 行不是合法 JSON: {source}")]
    Json {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("压缩的会话日志只读，不能追加: {0}")]
    CompressedAppend(PathBuf),
    #[error("会话日志正被另一个程序写入（如同时开着的桌面版或命令行）: {0}")]
    Locked(PathBuf),
}

/// 追加式 JSONL 会话日志，每条事件一行。
///
/// - 每条事件整行一次写进文件（进程崩溃不会留下半行，除非系统本身中途断电）；轮次结束、压缩、
///   批量写入后再 `sync_data` 落盘，断电最多丢掉还在进行的这一轮。
/// - 同一份日志同时只有一个写入者：旁边的 `<日志>.lock` 加排他锁（进程退出时由系统释放），
///   CLI 和桌面版同时追加同一个会话会写出重复的 `seq`。锁加在旁边的文件上而不是日志本身：
///   Windows 的文件锁是强制的，锁住日志会让列会话、打开会话这些只读的操作也读不了。
/// - 打开继续追加之前先修好末尾不完整的一行（写到一半断电、磁盘满），免得下一条事件接在半行后面。
pub struct SessionLog {
    path: PathBuf,
    file: File,
    next_seq: u64,
    /// 持有期间独占这份日志；随 `SessionLog` 一起释放。
    _lock: File,
}

impl SessionLog {
    /// 新建日志；父目录不存在时自动创建，文件已存在则报错。
    pub fn create(path: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let path = path.into();
        if is_compressed(&path) {
            return Err(CoreError::CompressedAppend(path));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock = lock_log(&path)?;
        let file = OpenOptions::new().create_new(true).append(true).open(&path)?;
        Ok(Self { path, file, next_seq: 1, _lock: lock })
    }

    /// 打开已有日志继续追加，同时返回已有内容。
    pub fn open(path: impl Into<PathBuf>) -> Result<(Self, Vec<LogEntry>), CoreError> {
        let path = path.into();
        if is_compressed(&path) {
            return Err(CoreError::CompressedAppend(path));
        }
        fs::metadata(&path)?; // 不存在时直接报错，不在旁边留下锁文件。
        let lock = lock_log(&path)?;
        repair_tail(&path)?;
        let entries = read_entries(&path)?;
        let last_seq = entries
            .iter()
            .filter_map(|entry| match entry {
                LogEntry::Known(event) => Some(event.seq),
                LogEntry::Unknown(value) => value.get("seq").and_then(|seq| seq.as_u64()),
            })
            .max()
            .unwrap_or(0);
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok((Self { path, file, next_seq: last_seq + 1, _lock: lock }, entries))
    }

    /// 只为追加而打开：逐行只取 `seq`，不解析事件内容，也不把日志留在内存里。
    /// 用于重命名这类只写一条事件的操作。
    pub fn open_append(path: impl Into<PathBuf>) -> Result<Self, CoreError> {
        #[derive(serde::Deserialize)]
        struct SeqOnly {
            seq: Option<u64>,
        }

        let path = path.into();
        if is_compressed(&path) {
            return Err(CoreError::CompressedAppend(path));
        }
        fs::metadata(&path)?; // 不存在时直接报错，不在旁边留下锁文件。
        let lock = lock_log(&path)?;
        repair_tail(&path)?;
        let mut last_seq = 0;
        let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, File::open(&path)?);
        let mut line = Vec::new();
        let mut number = 0;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            number += 1;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let entry: SeqOnly = serde_json::from_slice(&line).map_err(|source| CoreError::Json { line: number, source })?;
            last_seq = last_seq.max(entry.seq.unwrap_or(0));
        }
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Self { path, file, next_seq: last_seq + 1, _lock: lock })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, body: EventBody) -> Result<SessionEvent, CoreError> {
        let event = SessionEvent { seq: self.next_seq, time: now_millis(), body };
        let mut line = serde_json::to_vec(&event).expect("SessionEvent 总能序列化");
        line.push(b'\n');
        self.file.write_all(&line)?;
        // 一轮结束、压缩这些节点落盘；其余事件交给系统缓存（每条都 fsync 太慢，流式输出时一步就有好几条）。
        if matches!(event.body, EventBody::TurnEnd { .. } | EventBody::Compaction { .. } | EventBody::SessionTitle { .. }) {
            self.file.sync_data()?;
        }
        self.next_seq += 1;
        Ok(event)
    }

    /// 一次追加多条事件：先在内存里拼好，只写一次文件（分叉会话时复制成百上千条事件）。
    pub fn append_all(&mut self, bodies: impl IntoIterator<Item = EventBody>) -> Result<(), CoreError> {
        let time = now_millis();
        let mut buffer = Vec::new();
        let mut next_seq = self.next_seq;
        for body in bodies {
            let event = SessionEvent { seq: next_seq, time, body };
            serde_json::to_writer(&mut buffer, &event).expect("SessionEvent 总能序列化");
            buffer.push(b'\n');
            next_seq += 1;
        }
        self.file.write_all(&buffer)?;
        self.file.sync_data()?;
        self.next_seq = next_seq;
        Ok(())
    }
}

/// 逐行扫描日志时的缓冲区大小；工具结果一行常有几十 KB。
const READ_BUFFER_BYTES: usize = 64 * 1024;

/// 按行读会话日志的原文：`.zstd` / `.zst` 压缩的日志边读边解压。只想顺序扫一遍、不需要整份解析时用它。
pub fn log_reader(path: impl AsRef<Path>) -> io::Result<Box<dyn BufRead>> {
    let path = path.as_ref();
    let file = File::open(path)?;
    let reader: Box<dyn Read> = if is_compressed(path) { Box::new(zstd::stream::read::Decoder::new(file)?) } else { Box::new(file) };
    Ok(Box::new(BufReader::with_capacity(READ_BUFFER_BYTES, reader)))
}

/// 读取会话日志，支持原版的 `session.vN.jsonl` 与 `.zstd` 压缩文件。空行会被跳过。
///
/// 最后一行没有换行、又解析不了时当作没写完的一行跳过：写入者正在写（打开一个进行中的会话、列会话），
/// 或者上次写到一半断电了。中间的坏行仍然报错。
pub fn read_entries(path: impl AsRef<Path>) -> Result<Vec<LogEntry>, CoreError> {
    let mut entries = Vec::new();
    let mut reader = log_reader(path)?;
    let mut line = Vec::new();
    let mut number = 0;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        number += 1;
        let complete = line.last() == Some(&b'\n');
        let text = std::str::from_utf8(&line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        let text = match text {
            Ok(text) => text,
            Err(_) if !complete => break,
            Err(error) => return Err(error.into()),
        };
        if text.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(text) {
            Ok(entry) => entries.push(entry),
            Err(_) if !complete => break,
            Err(source) => return Err(CoreError::Json { line: number, source }),
        }
    }
    Ok(entries)
}

/// 给日志加写入锁（见 [`SessionLog`]）。
fn lock_log(path: &Path) -> Result<File, CoreError> {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".lock");
    let lock_path = path.with_file_name(name);
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(fs::TryLockError::WouldBlock) => Err(CoreError::Locked(path.to_owned())),
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// 末尾不是换行时修一下：那一段是完整的 JSON（别的程序写的、少了换行）就补上换行；
/// 不是（写到一半断电、磁盘满）就截掉它。不修的话，下一条事件会接在这半行后面，两条一起坏掉。
fn repair_tail(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    // 从末尾往前按块找最后一个换行。
    const CHUNK: u64 = 64 * 1024;
    let mut tail: Vec<u8> = Vec::new();
    let mut end = len;
    let start_of_tail = loop {
        let start = end.saturating_sub(CHUNK);
        let mut chunk = vec![0; (end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        if end == len && chunk.last() == Some(&b'\n') {
            return Ok(());
        }
        let newline = chunk.iter().rposition(|byte| *byte == b'\n');
        chunk.extend_from_slice(&tail);
        tail = chunk;
        if let Some(newline) = newline {
            tail.drain(..=newline);
            break start + newline as u64 + 1;
        }
        if start == 0 {
            break 0;
        }
        end = start;
    };
    if serde_json::from_slice::<serde_json::Value>(&tail).is_ok() {
        file.seek(SeekFrom::End(0))?;
        file.write_all(b"\n")?;
    } else {
        file.set_len(start_of_tail)?;
    }
    file.sync_data()
}

fn is_compressed(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "zstd" || ext == "zst")
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnEndReason;

    #[test]
    fn event_wire_shape() {
        let event = SessionEvent { seq: 3, time: 1000, body: EventBody::UserMessage { text: "hi".into() } };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json, serde_json::json!({"seq": 3, "time": 1000, "type": "user/message", "data": {"text": "hi"}}));
        let back: SessionEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn append_then_reopen_continues_seq() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s").join("session.v1.jsonl");
        let mut log = SessionLog::create(&path).unwrap();
        log.append(EventBody::TurnStart { turn: 1 }).unwrap();
        log.append(EventBody::TurnEnd { turn: 1, reason: TurnEndReason::Completed, error: None }).unwrap();
        drop(log);

        // 混入一行本程序不认识的事件（插件写的，或原版才有的）和一行内容对不上的核心事件：
        // 都原样保留，且不影响 seq。
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(
                b"{\"seq\":3,\"time\":1,\"type\":\"future/event\",\"data\":{\"n\":1}}\n\n\
                  {\"seq\":4,\"time\":1,\"type\":\"turn/start\",\"data\":{\"bogus\":true}}\n",
            )
            .unwrap();

        let (mut log, entries) = SessionLog::open(&path).unwrap();
        assert_eq!(entries.len(), 4);
        assert!(matches!(
            entries[2].known().map(|event| &event.body),
            Some(EventBody::Ext { kind, data }) if kind == "future/event" && data["n"] == 1
        ));
        assert!(matches!(entries[3], LogEntry::Unknown(_)));
        let event = log.append(EventBody::TurnStart { turn: 2 }).unwrap();
        assert_eq!(event.seq, 5);
    }

    #[test]
    fn ext_event_wire_shape_and_kind_rules() {
        let body = EventBody::ext("todo/item", serde_json::json!({"title": "x"})).unwrap();
        assert_eq!(body.kind(), "todo/item");
        let event = SessionEvent { seq: 7, time: 9, body };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json, serde_json::json!({"seq": 7, "time": 9, "type": "todo/item", "data": {"title": "x"}}));
        assert_eq!(serde_json::from_value::<SessionEvent>(json).unwrap(), event);
        // 没有 data 的行也算事件。
        let bare: SessionEvent = serde_json::from_str(r#"{"seq":1,"time":2,"type":"x/y"}"#).unwrap();
        assert_eq!(bare.body, EventBody::Ext { kind: "x/y".into(), data: serde_json::Value::Null });

        use crate::event::ExtKindError;
        for kind in ["todo", "/item", "todo/", "Todo/item", "to do/item"] {
            assert_eq!(EventBody::ext(kind, serde_json::Value::Null), Err(ExtKindError::Malformed(kind.into())));
        }
        // 核心事件的名字和命名空间都不能用。
        for kind in ["turn/start", "session/note", "tool/progress"] {
            assert_eq!(EventBody::ext(kind, serde_json::Value::Null), Err(ExtKindError::Reserved(kind.into())));
        }
        // 每个核心事件的 kind() 都在保留名单里。
        assert!(crate::is_core_kind(EventBody::TurnStart { turn: 1 }.kind()));
        assert!(crate::is_core_kind(EventBody::Compaction { summary: "".into(), upto_seq: 0 }.kind()));
        assert!(!crate::is_core_kind("todo/item"));
    }

    #[test]
    fn open_append_and_append_all_keep_seq_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v1.jsonl");
        let mut log = SessionLog::create(&path).unwrap();
        log.append_all([
            EventBody::TurnStart { turn: 1 },
            EventBody::UserMessage { text: "hi".into() },
            EventBody::TurnEnd { turn: 1, reason: TurnEndReason::Completed, error: None },
        ])
        .unwrap();
        assert_eq!(log.append(EventBody::TurnStart { turn: 2 }).unwrap().seq, 4);
        drop(log);

        // 插件事件也参与 seq 计数。
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"seq\":9,\"time\":1,\"type\":\"future/event\",\"data\":{}}\n\n")
            .unwrap();
        let mut log = SessionLog::open_append(&path).unwrap();
        assert_eq!(log.append(EventBody::SessionTitle { title: "t".into() }).unwrap().seq, 10);

        let seqs: Vec<u64> = read_entries(&path).unwrap().iter().filter_map(|e| e.known().map(|e| e.seq)).collect();
        assert_eq!(seqs, [1, 2, 3, 4, 9, 10]);
    }

    #[test]
    fn torn_last_line_is_skipped_and_repaired_before_appending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v1.jsonl");
        let mut log = SessionLog::create(&path).unwrap();
        log.append(EventBody::TurnStart { turn: 1 }).unwrap();
        drop(log);
        // 写到一半断电：最后一行只有半截。
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"seq\":2,\"time\":1,\"ty").unwrap();

        // 只读的一方（列会话、打开进行中的会话）跳过半行，不再整份打不开。
        assert_eq!(read_entries(&path).unwrap().len(), 1);
        // 中间的坏行仍然报错。
        let bad = dir.path().join("bad.jsonl");
        fs::write(&bad, "{\"seq\":1\n{\"seq\":2,\"time\":1,\"type\":\"x/y\"}\n").unwrap();
        assert!(matches!(read_entries(&bad), Err(CoreError::Json { line: 1, .. })));

        // 继续追加：先截掉半行，新事件自成一行。
        let (mut log, entries) = SessionLog::open(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(log.append(EventBody::TurnStart { turn: 2 }).unwrap().seq, 2);
        drop(log);
        let entries = read_entries(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(fs::read(&path).unwrap().ends_with(b"\n"));

        // 完整但少了换行的最后一行：补上换行，保留这条事件。
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"seq\":7,\"time\":1,\"type\":\"future/event\"}").unwrap();
        let mut log = SessionLog::open_append(&path).unwrap();
        assert_eq!(log.append(EventBody::TurnStart { turn: 3 }).unwrap().seq, 8);
        drop(log);
        assert_eq!(read_entries(&path).unwrap().len(), 4);
    }

    #[test]
    fn one_writer_per_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v1.jsonl");
        let log = SessionLog::create(&path).unwrap();
        assert!(matches!(SessionLog::open(&path), Err(CoreError::Locked(_))));
        assert!(matches!(SessionLog::open_append(&path), Err(CoreError::Locked(_))));
        // 读不受影响。
        assert!(read_entries(&path).unwrap().is_empty());
        drop(log);
        assert!(SessionLog::open_append(&path).is_ok());
        // 打开不存在的日志：报错，也不留下锁文件。
        let missing = dir.path().join("missing.jsonl");
        assert!(SessionLog::open(&missing).is_err());
        assert!(!dir.path().join("missing.jsonl.lock").exists());
    }

    #[test]
    fn reads_zstd_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v1.jsonl.zstd");
        let line = b"{\"seq\":1,\"time\":5,\"type\":\"user/message\",\"data\":{\"text\":\"x\"}}\n";
        fs::write(&path, zstd::encode_all(&line[..], 3).unwrap()).unwrap();
        let entries = read_entries(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].known().is_some());
        assert!(matches!(SessionLog::open(&path), Err(CoreError::CompressedAppend(_))));
    }
}
