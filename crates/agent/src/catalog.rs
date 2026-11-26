use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use dshr_core::{EventBody, SessionEvent, SessionLog, read_entries};

use crate::session::{rename_title, with_closed_log};

/// 会话列表中的一项。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    /// 最后一条 `session/title`；还没有标题时用首条用户输入的第一行。
    pub title: String,
    pub modified: Option<SystemTime>,
    pub log: PathBuf,
    /// 会话所在的工作目录（`session/workspace`）；早期的会话没有记录。
    pub workspace: Option<PathBuf>,
}

const LOG_FILE: &str = "session.v1.jsonl";
const TITLE_CHARS: usize = 60;

/// 列出 `sessions_dir` 下的会话，最近修改的在前。目录不存在时返回空列表。
///
/// 每次都重新扫描所有日志；需要反复刷新的调用方（桌面版侧边栏）用 [`SessionCatalog`]。
pub fn list_sessions(sessions_dir: &Path) -> Vec<SessionSummary> {
    SessionCatalog::default().list(sessions_dir)
}

/// 同 [`list_sessions`]，但沿用这个目录上次读出的标题与工作目录（进程里每个目录一份 [`SessionCatalog`]）。
/// 给反复列会话的调用方（插件的 `sessions.list`）用：只重读有变化的日志，不必每次把所有日志扫一遍。
pub fn list_sessions_cached(sessions_dir: &Path) -> Vec<SessionSummary> {
    static CATALOGS: std::sync::LazyLock<Mutex<HashMap<PathBuf, std::sync::Arc<SessionCatalog>>>> =
        std::sync::LazyLock::new(Default::default);
    let catalog = CATALOGS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).entry(sessions_dir.to_owned()).or_default().clone();
    catalog.list(sessions_dir)
}

/// 带缓存的会话列表：日志的修改时间和大小没变就沿用上次读出的标题与工作目录，
/// 刷新一次只需要对每个会话 `stat` 一下，再重读有变化的那几个（通常只有正在进行的会话）。
#[derive(Default)]
pub struct SessionCatalog {
    cache: Mutex<HashMap<String, CachedSummary>>,
}

struct CachedSummary {
    modified: Option<SystemTime>,
    len: u64,
    scan: SummaryScan,
}

/// 扫描一份日志读出的摘要，以及扫到了哪里：日志只会在末尾追加，下次只要从 `offset` 接着读新增的部分。
#[derive(Clone, Default)]
struct SummaryScan {
    /// 已经读完的完整行（以换行结尾）的总字节数。
    offset: u64,
    title: Option<String>,
    first_user: Option<String>,
    workspace: Option<PathBuf>,
}

impl SummaryScan {
    fn title(&self) -> Option<String> {
        self.title.clone().or_else(|| self.first_user.clone())
    }
}

impl SessionCatalog {
    /// 同 [`list_sessions`]。可以在后台线程调用。
    pub fn list(&self, sessions_dir: &Path) -> Vec<SessionSummary> {
        let Ok(entries) = std::fs::read_dir(sessions_dir) else {
            return Vec::new();
        };
        // 整个扫描期间持锁：两次刷新撞在一起时排队进行，后一次能用上前一次刚读出的结果。
        // 扫描完整体换成新表，顺带丢掉已删除会话的条目。
        let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut previous = std::mem::take(&mut *cache);
        let mut next = HashMap::with_capacity(previous.len());
        let mut sessions: Vec<_> = entries
            .flatten()
            .filter_map(|entry| {
                let log = entry.path().join(LOG_FILE);
                let metadata = log.metadata().ok()?;
                let (modified, len) = (metadata.modified().ok(), metadata.len());
                let id = entry.file_name().to_string_lossy().into_owned();
                let cached = match previous.remove(&id) {
                    // 没有修改时间可比时不信任缓存。
                    Some(cached) if modified.is_some() && cached.modified == modified && cached.len == len => cached,
                    // 变长了（正在进行的会话又追加了事件）：只读新增的部分。
                    Some(cached) if modified.is_some() && len >= cached.len && len >= cached.scan.offset => {
                        CachedSummary { modified, len, scan: read_summary(&log, cached.scan) }
                    }
                    // 新会话，或者日志变短、被换掉了：从头读。
                    _ => CachedSummary { modified, len, scan: read_summary(&log, SummaryScan::default()) },
                };
                let title = cached.scan.title().unwrap_or_else(|| "（空会话）".into());
                let workspace = cached.scan.workspace.clone();
                next.insert(id.clone(), cached);
                Some(SessionSummary { id, title, modified, log, workspace })
            })
            .collect();
        *cache = next;
        drop(cache);
        sessions.sort_by_key(|session| std::cmp::Reverse(session.modified));
        sessions
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    Log(#[from] dshr_core::CoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("第 {0} 轮还没有结束，不能从这里分叉")]
    TurnNotFinished(u32),
}

/// 会话日志的位置。
pub fn session_log_path(sessions_dir: &Path, id: &str) -> PathBuf {
    sessions_dir.join(id).join(LOG_FILE)
}

/// 重命名：追加一条 `session/title`（以最后一条为准）。
pub fn rename_session(sessions_dir: &Path, id: &str, title: &str) -> Result<(), CatalogError> {
    rename_title(&session_log_path(sessions_dir, id), title.trim().to_owned())?;
    Ok(())
}

/// 删除：移到 `sessions/.trash/` 下，可以手动找回。
pub fn delete_session(sessions_dir: &Path, id: &str) -> Result<(), CatalogError> {
    with_closed_log(&session_log_path(sessions_dir, id), || {
        let trash = sessions_dir.join(TRASH_DIR);
        std::fs::create_dir_all(&trash)?;
        let mut target = trash.join(id);
        if target.exists() {
            target = trash.join(format!("{id}-{}", uuid_suffix()));
        }
        std::fs::rename(sessions_dir.join(id), target)
    })?;
    Ok(())
}

/// 从第 `turn` 轮结束处分叉出新会话：复制到该轮 `turn/end` 为止的事件，并记下来源。返回新会话 ID。
pub fn fork_session(sessions_dir: &Path, id: &str, turn: u32, new_id: &str) -> Result<String, CatalogError> {
    let entries = read_entries(session_log_path(sessions_dir, id))?;
    let events: Vec<_> = entries.iter().filter_map(|entry| entry.known()).collect();
    let end = events
        .iter()
        .position(|event| matches!(event.body, EventBody::TurnEnd { turn: t, .. } if t == turn))
        .ok_or(CatalogError::TurnNotFinished(turn))?;
    // 标题取自刚读进来的事件，不再把源日志读第二遍。
    let title = title_from(events.iter().copied());

    let forked = std::iter::once(EventBody::SessionForked { from: id.to_owned(), turn })
        .chain(
            events[..=end]
                .iter()
                // 标题和压缩记录按新会话重新生成；压缩记录的 seq 在新日志里不再对应。
                .filter(|event| !matches!(event.body, EventBody::SessionTitle { .. } | EventBody::Compaction { .. }))
                .map(|event| event.body.clone()),
        )
        .chain(title.map(|title| EventBody::SessionTitle { title: format!("{title} · 分支") }));
    // 所有事件拼好后一次写入，而不是每条事件写一次文件。
    SessionLog::create(session_log_path(sessions_dir, new_id))?.append_all(forked)?;
    Ok(new_id.to_owned())
}

const TRASH_DIR: &str = ".trash";

fn uuid_suffix() -> String {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_millis().to_string()).unwrap_or_default()
}

/// 标题与工作目录。日志里绝大部分内容是工具输出，和这两项无关：逐行先看事件类型，
/// 只解析标题、工作目录和第一条用户消息这几种行，其余的行不做 JSON 解析。
///
/// 从 `scan.offset` 接着读（从头读时传默认值）。读不下去的日志没有摘要，下次从头再读。
fn read_summary(log: &Path, scan: SummaryScan) -> SummaryScan {
    read_summary_from(log, scan).unwrap_or_default()
}

fn read_summary_from(log: &Path, mut scan: SummaryScan) -> Option<SummaryScan> {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(log).ok()?;
    file.seek(SeekFrom::Start(scan.offset)).ok()?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line = Vec::new();
    loop {
        line.clear();
        // 先只读这一行的开头认出类型；用不着的行把剩下的部分跳过去，不拷进缓冲区。
        // 不然缓冲区会被撑到最长那一行那么大（一整块工具输出常常就在一行里）。
        let complete = match read_line_start(&mut reader, &mut line, KIND_PREFIX).ok()? {
            Some(complete) => complete,
            None => break,
        };
        let wanted = match event_kind(&line) {
            Some(kind) => {
                kind == b"session/title"
                    || (kind == b"session/workspace" && scan.workspace.is_none())
                    || (kind == b"user/message" && scan.first_user.is_none())
            }
            // 看不出类型（字段顺序不同的日志、空行）：交给完整解析判断。
            None => true,
        };
        let mut length = line.len() as u64;
        let mut ended = line.last() == Some(&b'\n');
        if !complete {
            if wanted {
                reader.read_until(b'\n', &mut line).ok()?;
                length = line.len() as u64;
                ended = line.last() == Some(&b'\n');
            } else {
                let (skipped, newline) = skip_line(&mut reader).ok()?;
                length += skipped;
                ended = newline;
            }
        }
        // 以换行结尾的行才算读完；最后半行（写入者正在写）照常看一眼，下次再从它开头读。
        if ended {
            scan.offset += length;
        }
        if !wanted {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<SessionEvent>(&line) else {
            continue;
        };
        match event.body {
            EventBody::SessionTitle { title: latest } => scan.title = Some(latest),
            EventBody::SessionWorkspace { path } if scan.workspace.is_none() => scan.workspace = Some(PathBuf::from(path)),
            EventBody::UserMessage { text } if scan.first_user.is_none() => scan.first_user = Some(first_line_title(&text)),
            _ => {}
        }
    }
    Some(scan)
}

/// 不解析 JSON，直接从一行日志的开头取出顶层的 `type`。
///
/// 本程序写出的行形如 `{"seq":1,"time":2,"type":"tool/result","data":{…}}`，`type` 总在 `data` 之前，
/// 所以开头一小段里、`"data":` 之前出现的第一个 `"type":"` 一定是顶层的那个。
/// 找不到（其它程序写的、字段顺序不同的行）时返回 `None`，由调用方完整解析。
fn event_kind(line: &[u8]) -> Option<&[u8]> {
    const TYPE_KEY: &[u8] = b"\"type\":\"";
    const DATA_KEY: &[u8] = b"\"data\":";
    let prefix = &line[..line.len().min(KIND_PREFIX)];
    let find = |needle: &[u8]| prefix.windows(needle.len()).position(|window| window == needle);
    let start = find(TYPE_KEY)?;
    if find(DATA_KEY).is_some_and(|data| data < start) {
        return None;
    }
    let kind = &prefix[start + TYPE_KEY.len()..];
    let end = kind.iter().position(|byte| *byte == b'"')?;
    Some(&kind[..end])
}

/// [`event_kind`] 只看一行开头这么多字节。
const KIND_PREFIX: usize = 160;

/// 读一行的开头到 `line` 末尾：读到换行符为止（返回 `Some(true)`，整行都在里面了），
/// 或者凑够 `limit` 字节为止（返回 `Some(false)`，这一行还有没读的部分）。文件已经读完时返回 `None`。
fn read_line_start(reader: &mut impl BufRead, line: &mut Vec<u8>, limit: usize) -> std::io::Result<Option<bool>> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(true));
        }
        let take = available.len().min(limit.saturating_sub(line.len()));
        if let Some(newline) = available[..take].iter().position(|byte| *byte == b'\n') {
            line.extend_from_slice(&available[..=newline]);
            reader.consume(newline + 1);
            return Ok(Some(true));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.len() >= limit {
            return Ok(Some(false));
        }
    }
}

/// 跳过这一行剩下的部分（连同换行符），不拷贝。返回跳过的字节数，以及是不是遇到了换行（否则是读到了文件末尾）。
fn skip_line(reader: &mut impl BufRead) -> std::io::Result<(u64, bool)> {
    let mut skipped = 0;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((skipped, false));
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(newline) => {
                reader.consume(newline + 1);
                return Ok((skipped + newline as u64 + 1, true));
            }
            None => {
                let consumed = available.len();
                reader.consume(consumed);
                skipped += consumed as u64;
            }
        }
    }
}

fn first_line_title(text: &str) -> String {
    text.lines().next().unwrap_or_default().chars().take(TITLE_CHARS).collect()
}

fn title_from<'a>(events: impl DoubleEndedIterator<Item = &'a SessionEvent> + Clone) -> Option<String> {
    let generated = events.clone().rev().find_map(|event| match &event.body {
        EventBody::SessionTitle { title } => Some(title.clone()),
        _ => None,
    });
    generated.or_else(|| {
        events.clone().find_map(|event| match &event.body {
            EventBody::UserMessage { text } => Some(first_line_title(text)),
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dshr_core::SessionLog;

    #[test]
    fn lists_sessions_with_titles() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create(dir.path().join("a").join(LOG_FILE)).unwrap();
        log.append(EventBody::UserMessage { text: "修复登录 bug\n细节…".into() }).unwrap();
        std::fs::create_dir(dir.path().join("not-a-session")).unwrap();

        let sessions = list_sessions(dir.path());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "a");
        assert_eq!(sessions[0].title, "修复登录 bug");
        assert!(list_sessions(&dir.path().join("missing")).is_empty());

        // 生成的标题优先，且以最后一条为准。
        log.append(EventBody::SessionTitle { title: "登录问题".into() }).unwrap();
        log.append(EventBody::SessionTitle { title: "修复登录超时".into() }).unwrap();
        assert_eq!(list_sessions(dir.path())[0].title, "修复登录超时");

        assert_eq!(list_sessions(dir.path())[0].workspace, None);
        log.append(EventBody::SessionWorkspace { path: "/work/app".into() }).unwrap();
        assert_eq!(list_sessions(dir.path())[0].workspace, Some(PathBuf::from("/work/app")));
    }

    fn summary(path: &Path) -> (Option<String>, Option<PathBuf>) {
        let scan = read_summary(path, SummaryScan::default());
        (scan.title(), scan.workspace)
    }

    #[test]
    fn summary_scan_resumes_after_appends_and_torn_lines() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let path = session_log_path(dir.path(), "s");
        let mut log = SessionLog::create(&path).unwrap();
        log.append(EventBody::ToolResult { call_id: "c".into(), output: "x".repeat(100_000).into(), is_error: false }).unwrap();
        log.append(EventBody::UserMessage { text: "第一问".into() }).unwrap();
        let scan = read_summary(&path, SummaryScan::default());
        assert_eq!(scan.offset, std::fs::metadata(&path).unwrap().len());
        assert_eq!(scan.title().as_deref(), Some("第一问"));

        // 接着读：只看新增的部分，之前读到的保留。
        log.append(EventBody::SessionWorkspace { path: "/w".into() }).unwrap();
        log.append(EventBody::ToolResult { call_id: "d".into(), output: "y".repeat(100_000).into(), is_error: false }).unwrap();
        drop(log);
        let scan = read_summary(&path, scan);
        assert_eq!((scan.title().as_deref(), scan.workspace.as_deref()), (Some("第一问"), Some(Path::new("/w"))));
        assert_eq!(scan.offset, std::fs::metadata(&path).unwrap().len());

        // 写了一半的行（长的、不想要的）：不算读完，补全之后从它开头再读。
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        let line = format!(
            "{{\"seq\":9,\"time\":1,\"type\":\"tool/result\",\"data\":{{\"call_id\":\"e\",\"output\":\"{}\"}}}}\n",
            "z".repeat(1000)
        );
        file.write_all(&line.as_bytes()[..500]).unwrap();
        let torn = read_summary(&path, scan.clone());
        assert_eq!(torn.offset, scan.offset);
        file.write_all(&line.as_bytes()[500..]).unwrap();
        file.write_all(b"{\"seq\":10,\"time\":1,\"type\":\"session/title\",\"data\":{\"title\":\"t\"}}\n").unwrap();
        let scan = read_summary(&path, torn);
        assert_eq!(scan.offset, std::fs::metadata(&path).unwrap().len());
        assert_eq!(scan.title().as_deref(), Some("t"));

        // 侧边栏：变长的日志只读新增部分，结果和从头读一样。
        let catalog = SessionCatalog::default();
        assert_eq!(catalog.list(dir.path())[0].title, "t");
        file.write_all(b"{\"seq\":11,\"time\":1,\"type\":\"session/title\",\"data\":{\"title\":\"u\"}}\n").unwrap();
        assert_eq!(catalog.list(dir.path())[0].title, "u");
        assert_eq!(catalog.list(dir.path())[0].workspace.as_deref(), Some(Path::new("/w")));
    }

    #[test]
    fn summary_scan_matches_full_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_log_path(dir.path(), "s");
        let mut log = SessionLog::create(&path).unwrap();
        log.append(EventBody::SessionWorkspace { path: "/work/a".into() }).unwrap();
        // 内容里出现事件类型字样的行不能被当成标题或工作目录。
        log.append(EventBody::ToolResult {
            call_id: "c".into(),
            output: r#"{"seq":1,"time":1,"type":"session/title","data":{"title":"假标题"}}"#.into(),
            is_error: false,
        })
        .unwrap();
        log.append(EventBody::UserMessage { text: "真正的第一问\n第二行".into() }).unwrap();
        log.append(EventBody::UserMessage { text: "第二问".into() }).unwrap();
        log.append(EventBody::SessionWorkspace { path: "/work/b".into() }).unwrap();
        drop(log);
        assert_eq!(summary(&path), (Some("真正的第一问".into()), Some(PathBuf::from("/work/a"))));

        // 字段顺序不同、带空格的行（其它程序写的）走完整解析，结果一样；坏行跳过。
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n{\"data\": {\"title\": \"other\"}, \"type\": \"session/title\", \"time\": 1, \"seq\": 9}\nnot json\n")
            .unwrap();
        assert_eq!(summary(&path).0.as_deref(), Some("other"));

        assert_eq!(event_kind(br#"{"seq":1,"time":2,"type":"tool/result","data":{"type":"x"}}"#), Some(&b"tool/result"[..]));
        assert_eq!(event_kind(br#"{"seq":1,"data":{"type":"text"},"type":"user/message"}"#), None);
        assert_eq!(event_kind(b""), None);
    }

    #[test]
    fn summary_scan_skips_long_lines_and_reads_long_wanted_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_log_path(dir.path(), "s");
        let mut log = SessionLog::create(&path).unwrap();
        // 一行远超缓冲区的工具输出：只读开头，其余跳过；后面的行照样认得出。
        log.append(EventBody::ToolResult { call_id: "c".into(), output: "x".repeat(200_000).into(), is_error: false }).unwrap();
        // 想要的行比只看的开头（KIND_PREFIX）长：要把整行读完再解析。
        let long_title = "长".repeat(400);
        log.append(EventBody::UserMessage { text: format!("{long_title}\n第二行").into() }).unwrap();
        log.append(EventBody::SessionWorkspace { path: format!("/work/{}", "d".repeat(300)) }).unwrap();
        log.append(EventBody::ToolResult { call_id: "d".into(), output: "y".repeat(100).into(), is_error: false }).unwrap();
        drop(log);
        let expected_title: String = long_title.chars().take(TITLE_CHARS).collect();
        assert_eq!(summary(&path), (Some(expected_title), Some(PathBuf::from(format!("/work/{}", "d".repeat(300))))));
        assert_eq!(list_sessions_cached(dir.path())[0].workspace, summary(&path).1);
    }

    #[test]
    fn catalog_rereads_only_changed_logs() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = SessionLog::create(session_log_path(dir.path(), "a")).unwrap();
        a.append(EventBody::UserMessage { text: "会话 A".into() }).unwrap();
        let mut b = SessionLog::create(session_log_path(dir.path(), "b")).unwrap();
        b.append(EventBody::UserMessage { text: "会话 B".into() }).unwrap();

        let catalog = SessionCatalog::default();
        let titles = |catalog: &SessionCatalog| {
            let mut titles: Vec<_> = catalog.list(dir.path()).into_iter().map(|s| (s.id, s.title)).collect();
            titles.sort();
            titles
        };
        assert_eq!(titles(&catalog), [("a".into(), "会话 A".into()), ("b".into(), "会话 B".into())]);

        // 把 a 的缓存换成一个记号：日志没变时应当沿用缓存而不是重读。
        catalog.cache.lock().unwrap().get_mut("a").unwrap().scan.title = Some("来自缓存".into());
        assert_eq!(titles(&catalog)[0].1, "来自缓存");

        // 日志有了新内容（大小变了）就重读；没变的 b 不受影响。
        a.append(EventBody::SessionTitle { title: "新标题".into() }).unwrap();
        assert_eq!(titles(&catalog), [("a".into(), "新标题".into()), ("b".into(), "会话 B".into())]);

        // 删除的会话从列表和缓存里一起消失。
        drop(b);
        delete_session(dir.path(), "b").unwrap();
        assert_eq!(titles(&catalog).len(), 1);
        assert_eq!(catalog.cache.lock().unwrap().len(), 1);
    }

    #[test]
    fn rename_fork_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create(session_log_path(dir.path(), "s")).unwrap();
        for (turn, text) in [(1, "第一问"), (2, "第二问")] {
            log.append(EventBody::TurnStart { turn }).unwrap();
            log.append(EventBody::UserMessage { text: text.into() }).unwrap();
            log.append(EventBody::TurnEnd { turn, reason: dshr_core::TurnEndReason::Completed, error: None }).unwrap();
        }
        drop(log);

        rename_session(dir.path(), "s", " 原会话 ").unwrap();
        assert_eq!(list_sessions(dir.path())[0].title, "原会话");

        fork_session(dir.path(), "s", 1, "f").unwrap();
        let forked: Vec<_> = read_entries(session_log_path(dir.path(), "f"))
            .unwrap()
            .into_iter()
            .filter_map(|entry| entry.known().map(|event| event.body.kind().to_owned()))
            .collect();
        assert_eq!(forked, ["session/forked", "turn/start", "user/message", "turn/end", "session/title"]);
        assert!(list_sessions(dir.path()).iter().any(|s| s.id == "f" && s.title == "原会话 · 分支"));
        assert!(matches!(fork_session(dir.path(), "s", 9, "g"), Err(CatalogError::TurnNotFinished(9))));

        delete_session(dir.path(), "s").unwrap();
        let ids: Vec<_> = list_sessions(dir.path()).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["f"]);
        assert!(dir.path().join(".trash").join("s").join(LOG_FILE).exists());
    }
}
