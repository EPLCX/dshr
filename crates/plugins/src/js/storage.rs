//! 插件自己的结构化存储与归档：`host.db`（SQLite）、`host.zip` / `host.unzip`，以及会话相关的两个小函数
//! （`host.session.endTurn`、`host.session.current`）。
//!
//! - 数据库文件放在插件的数据目录里，和 `readData` / `writeData` 一样不需要能力、在任何场合都能用。
//!   连接在插件线程上一直开着（按文件名），一次调用结束时还没提交的事务回滚。
//! - 查询跑在插件线程上：SQLite 的进度回调按这次调用的时限和取消标记打断长查询。
//! - zip 是纯计算（内存里打包、解包），不需要能力；解包限制条目数和解压后的总大小，防止压缩炸弹。

use std::io::{Read as _, Write as _};
use std::time::Instant;

use base64::Engine as _;
use dshr_agent::{AgentIdentity, TurnControl};
use rusqlite::types::{Value as SqlValue, ValueRef};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::host::{MAX_DATA_BYTES, State, confine_data};

/// 一个数据库文件最多这么大（SQLite 的 `max_page_count` 按页大小换算）。
const MAX_DB_BYTES: u64 = 256 * 1024 * 1024;
/// 一次查询最多返回这么多行、结果 JSON 最多这么大。
const MAX_ROWS: usize = 10_000;
const MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
/// 另一个运行时（常驻服务）正在写同一个库时最多等这么久。
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// zip：打包的输入、解包的输入和解压后的总大小、条目数。
const MAX_ZIP_BYTES: usize = 32 * 1024 * 1024;
const MAX_UNZIPPED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 10_000;

fn b64() -> &'static base64::engine::GeneralPurpose {
    &base64::engine::general_purpose::STANDARD
}

impl State {
    // ---- host.db ----

    /// 打开（或复用）数据目录里的一个数据库。`name` 是数据目录里的相对路径，默认 `db.sqlite`。
    fn with_db<T>(&self, name: &str, run: impl FnOnce(&rusqlite::Connection) -> Result<T, String>) -> Result<T, String> {
        let name = if name.is_empty() { "db.sqlite" } else { name };
        let mut databases = self.databases.borrow_mut();
        if !databases.contains_key(name) {
            let path = confine_data(&self.data_dir, name)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| format!("cannot open database {name}: {error}"))?;
            }
            if !path.exists() && super::host::data_usage(&self.data_dir) >= MAX_DATA_BYTES {
                return Err(format!("the plugin's data folder is full ({} MB)", MAX_DATA_BYTES / 1024 / 1024));
            }
            let connection = rusqlite::Connection::open(&path).map_err(|error| format!("cannot open database {name}: {error}"))?;
            connection.busy_timeout(BUSY_TIMEOUT).map_err(|error| error.to_string())?;
            // WAL：常驻服务和工具（两个运行时）同时用一个库时，读不挡写。
            let _ = connection.pragma_update(None, "journal_mode", "WAL");
            let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0)).unwrap_or(4096);
            let _ = connection.pragma_update(None, "max_page_count", MAX_DB_BYTES as i64 / page_size.max(512));
            let _ = connection.pragma_update(None, "foreign_keys", true);
            databases.insert(name.to_owned(), connection);
        }
        let connection = databases.get_mut(name).ok_or("the database is not open")?;
        // 按这次调用的时限、取消打断长查询：返回 true 表示停下。
        let (deadline, cancelled) = self.interrupt_handle();
        let _ = connection.progress_handler(
            10_000,
            Some(move || {
                deadline.is_some_and(|deadline| Instant::now() >= deadline)
                    || cancelled.as_ref().is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
            }),
        );
        let result = run(connection);
        let _ = connection.progress_handler(0, None::<fn() -> bool>);
        result
    }

    /// 执行语句：没有参数时可以是用 `;` 隔开的多条；返回 `{ changes, lastInsertRowid }`。
    pub(super) fn db_exec(&self, name: &str, sql: &str, params: &str) -> Result<String, String> {
        let params = Params::parse(params)?;
        self.with_db(name, |connection| {
            if params.is_empty() {
                connection.execute_batch(sql).map_err(describe)?;
            } else {
                let mut statement = connection.prepare(sql).map_err(describe)?;
                params.bind(&mut statement)?;
                statement.raw_execute().map_err(describe)?;
            }
            Ok(json!({ "changes": connection.changes(), "lastInsertRowid": connection.last_insert_rowid() }).to_string())
        })
    }

    /// 查询：返回 `[{ 列名: 值 }]`。BLOB 是 `{ "$blob": base64 }`（`prelude.js` 换成 `Uint8Array`）。
    pub(super) fn db_query(&self, name: &str, sql: &str, params: &str) -> Result<String, String> {
        let params = Params::parse(params)?;
        self.with_db(name, |connection| {
            let mut statement = connection.prepare(sql).map_err(describe)?;
            params.bind(&mut statement)?;
            let columns: Vec<String> = statement.column_names().map(str::to_owned).collect();
            let mut rows = statement.raw_query();
            let mut found = Vec::new();
            let mut size = 0;
            while let Some(row) = rows.next().map_err(describe)? {
                if found.len() >= MAX_ROWS {
                    return Err(format!("the query returned more than {MAX_ROWS} rows; add LIMIT or narrow it down"));
                }
                let mut object = Map::new();
                for (index, column) in columns.iter().enumerate() {
                    let value = match row.get_ref(index).map_err(describe)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(number) => json!(number),
                        ValueRef::Real(number) => json!(number),
                        ValueRef::Text(text) => Value::String(String::from_utf8_lossy(text).into_owned()),
                        ValueRef::Blob(bytes) => json!({ "$blob": b64().encode(bytes) }),
                    };
                    size += column.len() + value_size(&value);
                    object.insert(column.clone(), value);
                }
                if size > MAX_RESULT_BYTES {
                    return Err(format!(
                        "the query result is larger than {} MB; add LIMIT or select fewer columns",
                        MAX_RESULT_BYTES >> 20
                    ));
                }
                found.push(Value::Object(object));
            }
            Ok(Value::Array(found).to_string())
        })
    }

    /// 关掉一个数据库（之后再用会重新打开）。
    pub(super) fn db_close(&self, name: &str) -> Result<(), String> {
        let name = if name.is_empty() { "db.sqlite" } else { name };
        if let Some(connection) = self.databases.borrow_mut().remove(name) {
            connection.close().map_err(|(_, error)| describe(error))?;
        }
        Ok(())
    }

    /// 一次调用结束时：没提交的事务回滚（脚本忘了提交、抛了异常、超时被打断）。
    pub(super) fn rollback_open_transactions(&self) {
        for (name, connection) in self.databases.borrow().iter() {
            if !connection.is_autocommit() {
                if let Err(error) = connection.execute_batch("ROLLBACK") {
                    tracing::warn!(plugin = self.plugin, database = name, %error, "回滚插件没提交的事务失败");
                }
                self.logs.push("warn", &format!("database {name}: a transaction was left open when the call ended and was rolled back"));
            }
        }
    }

    // ---- 会话 ----

    /// `host.session.endTurn()`：这一步的工具调用执行完后结束本轮，不再请求模型。
    pub(super) fn end_turn(&self) -> Result<(), String> {
        self.with_call_pub(|call| match call.ctx.options.get::<TurnControl>() {
            Some(control) => {
                control.end_turn();
                Ok(())
            }
            None => Err("host.session.endTurn() can only be used in a tool call or an afterTool hook during a turn".into()),
        })
    }

    /// `host.session.current()`：`{ session, agent }`，不在会话里时都是 `null`。
    pub(super) fn identity(&self) -> String {
        self.with_call_pub(|call| {
            let options = &call.ctx.options;
            let known = options.get::<AgentIdentity>().is_some() || options.get::<dshr_agent::SessionInfo>().is_some();
            let identity = AgentIdentity::of(options, None);
            Ok(if known {
                json!({ "session": identity.session, "agent": identity.agent })
            } else {
                json!({ "session": null, "agent": null })
            })
        })
        .unwrap_or_else(|_: String| json!({ "session": null, "agent": null }))
        .to_string()
    }
}

fn describe(error: rusqlite::Error) -> String {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) if failure.code == rusqlite::ErrorCode::OperationInterrupted => {
            "the database query was interrupted (the call ran out of time or was cancelled)".into()
        }
        rusqlite::Error::SqliteFailure(failure, _) if failure.code == rusqlite::ErrorCode::DiskFull => {
            format!("the database is full (at most {} MB)", MAX_DB_BYTES >> 20)
        }
        other => other.to_string(),
    }
}

fn value_size(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Object(object) => object.values().map(value_size).sum(),
        _ => 8,
    }
}

/// 语句的参数：数组按位置（`?`、`?1`），对象按名字（`:name`、`@name`、`$name`，键可以带也可以不带前缀）。
enum Params {
    None,
    Positional(Vec<SqlValue>),
    Named(Vec<(String, SqlValue)>),
}

impl Params {
    fn parse(json: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(json).map_err(|error| format!("invalid query parameters: {error}"))?;
        Ok(match value {
            Value::Null => Self::None,
            Value::Array(items) => Self::Positional(items.iter().map(sql_value).collect::<Result<_, _>>()?),
            Value::Object(object) => {
                Self::Named(object.iter().map(|(key, value)| Ok((key.clone(), sql_value(value)?))).collect::<Result<_, String>>()?)
            }
            _ => return Err("query parameters must be an array or an object".into()),
        })
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::Positional(items) => items.is_empty(),
            Self::Named(items) => items.is_empty(),
        }
    }

    fn bind(&self, statement: &mut rusqlite::Statement<'_>) -> Result<(), String> {
        match self {
            Self::None => {}
            Self::Positional(items) => {
                if items.len() != statement.parameter_count() {
                    return Err(format!("the statement takes {} parameters, {} were given", statement.parameter_count(), items.len()));
                }
                for (index, value) in items.iter().enumerate() {
                    statement.raw_bind_parameter(index + 1, value).map_err(describe)?;
                }
            }
            Self::Named(items) => {
                for (key, value) in items {
                    let index = if key.starts_with([':', '@', '$']) {
                        statement.parameter_index(key).map_err(describe)?
                    } else {
                        [':', '@', '$'].iter().find_map(|prefix| statement.parameter_index(&format!("{prefix}{key}")).ok().flatten())
                    };
                    let index = index.ok_or_else(|| format!("the statement has no parameter named {key}"))?;
                    statement.raw_bind_parameter(index, value).map_err(describe)?;
                }
            }
        }
        Ok(())
    }
}

fn sql_value(value: &Value) -> Result<SqlValue, String> {
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::Bool(flag) => SqlValue::Integer(i64::from(*flag)),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => SqlValue::Integer(integer),
            None => SqlValue::Real(number.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(text) => SqlValue::Text(text.clone()),
        Value::Object(object) if object.len() == 1 && object.contains_key("$blob") => {
            let encoded = object["$blob"].as_str().ok_or("a $blob parameter must be base64 text")?;
            SqlValue::Blob(b64().decode(encoded).map_err(|error| format!("invalid $blob parameter: {error}"))?)
        }
        // 数组、对象存成 JSON 文本（SQLite 的 json_* 函数能直接用）。
        other => SqlValue::Text(other.to_string()),
    })
}

// ---- zip ----

#[derive(Deserialize)]
struct ZipEntry {
    path: String,
    /// base64 的内容；没有时是一个文件夹。
    data: Option<String>,
    /// `store`（不压缩，EPUB / ODF 的 `mimetype` 要这样）或 `deflate`（默认）。
    #[serde(default)]
    method: Option<String>,
}

/// `host.zip(entries)`：按给的顺序打包，返回 base64。
pub(super) fn zip(entries: &str) -> Result<String, String> {
    let entries: Vec<ZipEntry> = serde_json::from_str(entries).map_err(|error| format!("invalid zip entries: {error}"))?;
    if entries.len() > MAX_ZIP_ENTRIES {
        return Err(format!("at most {MAX_ZIP_ENTRIES} entries per archive"));
    }
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut total = 0;
    for entry in entries {
        let path = entry.path.replace('\\', "/");
        if path.is_empty() || path.starts_with('/') || path.split('/').any(|part| part == "..") {
            return Err(format!("{}: zip entry paths must be relative and stay inside the archive", entry.path));
        }
        let method = match entry.method.as_deref() {
            None | Some("deflate") => zip::CompressionMethod::Deflated,
            Some("store") => zip::CompressionMethod::Stored,
            Some(other) => return Err(format!("unknown zip method {other} (use store or deflate)")),
        };
        let options = zip::write::SimpleFileOptions::default().compression_method(method).unix_permissions(0o644);
        match entry.data {
            None => writer.add_directory(path.trim_end_matches('/'), options).map_err(|error| error.to_string())?,
            Some(encoded) => {
                let bytes = b64().decode(encoded).map_err(|error| format!("{path}: invalid data: {error}"))?;
                total += bytes.len();
                if total > MAX_ZIP_BYTES {
                    return Err(format!("the archive content is larger than {} MB", MAX_ZIP_BYTES >> 20));
                }
                writer.start_file(path.as_str(), options).map_err(|error| error.to_string())?;
                writer.write_all(&bytes).map_err(|error| error.to_string())?;
            }
        }
    }
    let bytes = writer.finish().map_err(|error| error.to_string())?.into_inner();
    Ok(b64().encode(bytes))
}

/// `host.unzip(bytes)`：`[{ path, data }]`（文件夹是 `{ path, dir: true }`），`data` 是 base64。
pub(super) fn unzip(encoded: &str) -> Result<String, String> {
    let bytes = b64().decode(encoded).map_err(|error| format!("invalid zip data: {error}"))?;
    if bytes.len() > MAX_ZIP_BYTES {
        return Err(format!("the archive is larger than {} MB", MAX_ZIP_BYTES >> 20));
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|error| format!("not a zip archive: {error}"))?;
    if archive.len() > MAX_ZIP_ENTRIES {
        return Err(format!("the archive has more than {MAX_ZIP_ENTRIES} entries"));
    }
    let mut found = Vec::with_capacity(archive.len());
    let mut total: u64 = 0;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|error| error.to_string())?;
        let path = file.name().map_err(|error| error.to_string())?.replace('\\', "/");
        if file.is_dir() {
            found.push(json!({ "path": path.trim_end_matches('/'), "dir": true }));
            continue;
        }
        // 不信声明的大小：按实际读出来的算，超过总量就停。
        let budget = MAX_UNZIPPED_BYTES - total;
        let mut data = Vec::new();
        (&mut file).take(budget + 1).read_to_end(&mut data).map_err(|error| format!("{path}: {error}"))?;
        total += data.len() as u64;
        if total > MAX_UNZIPPED_BYTES {
            return Err(format!("the archive unpacks to more than {} MB", MAX_UNZIPPED_BYTES >> 20));
        }
        found.push(json!({ "path": path, "data": b64().encode(&data) }));
    }
    Ok(Value::Array(found).to_string())
}
