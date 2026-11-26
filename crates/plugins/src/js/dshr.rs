//! `host.dshr`：受控地读写宿主自己的数据——程序设置、Skills、全部会话、MCP 服务器。
//!
//! 每一组都有自己的能力（`dshr:config:read` / `dshr:config:write`、`dshr:skills`、`dshr:sessions:read` /
//! `dshr:sessions:write`、`dshr:mcp`）。改东西的操作还要这次调用允许“替用户做事”（工具没标 `readOnly`、
//! 页面动作、异步钩子），工具按“交给外部”把关。API Key 在系统钥匙串里，这里碰不到。
//! 实现都复用现有的函数：`dshr_config::Config`、`dshr_agent` 的会话目录函数、`dshr_core::SessionLog`。

use std::path::{Path, PathBuf};

use dshr_config::{Config, Paths};
use dshr_core::{EventBody, SessionLog};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest::Capabilities;

/// 一次最多读出的会话事件、导入的事件。
const MAX_EVENTS: usize = 2000;
const MAX_SKILL_BYTES: usize = 256 * 1024;

pub(super) struct Dshr<'a> {
    pub capabilities: &'a Capabilities,
    /// 数据目录（`~/.dshr`）。
    pub home: &'a Path,
    /// 这次调用能不能改东西。
    pub may_act: bool,
    pub plugin: &'a str,
}

impl Dshr<'_> {
    fn paths(&self) -> Paths {
        Paths { home: self.home.to_owned() }
    }

    fn need(&self, granted: bool, capability: &str) -> Result<(), String> {
        if granted { Ok(()) } else { Err(format!("this plugin does not declare the `{capability}` capability in plugin.toml")) }
    }

    fn need_act(&self) -> Result<(), String> {
        if self.may_act {
            Ok(())
        } else {
            Err("changing the app's data is only available to tools that are not declared readOnly, page actions and async hooks".into())
        }
    }

    /// `method` 是 `config.get` 这样的名字，`args` 是 JSON 数组。返回 JSON 文本。
    pub fn call(&self, method: &str, args: &str) -> Result<String, String> {
        let args: Vec<Value> = serde_json::from_str(args).map_err(|error| format!("invalid arguments: {error}"))?;
        let arg = |index: usize| args.get(index).cloned().unwrap_or(Value::Null);
        let text = |index: usize| -> Result<String, String> {
            arg(index).as_str().map(str::to_owned).ok_or_else(|| format!("{method}: argument {} must be a string", index + 1))
        };
        let session_id = |index: usize| -> Result<String, String> {
            let id = text(index)?;
            if crate::manifest::valid_session_id(&id) { Ok(id) } else { Err(format!("invalid session id {id}")) }
        };
        let caps = self.capabilities;
        let result = match method {
            "config.get" => {
                self.need(caps.config_read || caps.config_write, "dshr:config:read")?;
                serde_json::to_value(self.load_config()?).map_err(|error| error.to_string())?
            }
            "config.patch" => {
                self.need(caps.config_write, "dshr:config:write")?;
                self.need_act()?;
                self.patch_config(arg(0))?
            }
            "skills.list" => {
                self.need(caps.skills, "dshr:skills")?;
                self.list_skills()
            }
            "skills.read" => {
                self.need(caps.skills, "dshr:skills")?;
                let path = self.skill_file(&text(0)?)?;
                match std::fs::read_to_string(&path) {
                    Ok(text) => Value::String(text),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
                    Err(error) => return Err(format!("cannot read the skill: {error}")),
                }
            }
            "skills.write" => {
                self.need(caps.skills, "dshr:skills")?;
                self.need_act()?;
                let (path, content) = (self.skill_file(&text(0)?)?, text(1)?);
                if content.len() > MAX_SKILL_BYTES {
                    return Err(format!("the skill is too large (the limit is {MAX_SKILL_BYTES} bytes)"));
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::write(&path, content).map_err(|error| format!("cannot write the skill: {error}"))?;
                Value::Null
            }
            "skills.remove" => {
                self.need(caps.skills, "dshr:skills")?;
                self.need_act()?;
                let dir = self.skill_file(&text(0)?)?.parent().map(Path::to_owned).unwrap_or_default();
                match std::fs::remove_dir_all(&dir) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.to_string()),
                    _ => Value::Null,
                }
            }
            "sessions.list" => {
                self.need(caps.sessions_read || caps.sessions_write, "dshr:sessions:read")?;
                self.list_sessions()
            }
            "sessions.read" => {
                self.need(caps.sessions_read || caps.sessions_write, "dshr:sessions:read")?;
                let last = arg(1).get("last").and_then(Value::as_u64).map_or(MAX_EVENTS, |last| last as usize).min(MAX_EVENTS);
                self.read_session(&session_id(0)?, last)?
            }
            "sessions.rename" => {
                self.need_sessions_write()?;
                dshr_agent::rename_session(&self.paths().sessions_dir(), &session_id(0)?, &text(1)?).map_err(|error| error.to_string())?;
                Value::Null
            }
            "sessions.remove" => {
                self.need_sessions_write()?;
                dshr_agent::delete_session(&self.paths().sessions_dir(), &session_id(0)?).map_err(|error| error.to_string())?;
                Value::Null
            }
            "sessions.fork" => {
                self.need_sessions_write()?;
                let turn = arg(1).as_u64().ok_or("sessions.fork: the turn must be a number")? as u32;
                let id = new_session_id();
                dshr_agent::fork_session(&self.paths().sessions_dir(), &session_id(0)?, turn, &id).map_err(|error| error.to_string())?;
                Value::String(id)
            }
            "sessions.import" => {
                self.need_sessions_write()?;
                self.import_session(arg(0))?
            }
            "mcp.list" => {
                self.need(caps.mcp, "dshr:mcp")?;
                let servers = self.load_config()?.mcp_servers();
                Value::Array(
                    servers
                        .into_iter()
                        .map(|server| json!({ "name": server.name, "command": server.command_line, "enabled": server.enabled }))
                        .collect(),
                )
            }
            "mcp.add" | "mcp.remove" | "mcp.setEnabled" => {
                self.need(caps.mcp, "dshr:mcp")?;
                self.need_act()?;
                let mut config = self.load_config()?;
                let name = text(0)?;
                match method {
                    "mcp.add" => {
                        let server = arg(1);
                        let command = server.get("command").and_then(Value::as_str).ok_or("mcp.add: `command` is required")?;
                        let args: Vec<String> = server
                            .get("args")
                            .and_then(Value::as_array)
                            .map(|args| args.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                            .unwrap_or_default();
                        config.add_mcp_server(&name, command, args);
                    }
                    "mcp.remove" => config.remove_mcp_server(&name),
                    _ => config.set_mcp_server_enabled(&name, arg(1).as_bool().unwrap_or(true)),
                }
                config.save(&self.paths()).map_err(|error| error.to_string())?;
                Value::Null
            }
            other => return Err(format!("unknown host.dshr method {other}")),
        };
        if matches!(
            method,
            "config.patch" | "skills.write" | "skills.remove" | "sessions.rename" | "sessions.remove" | "sessions.fork" | "sessions.import"
        ) || method.starts_with("mcp.") && method != "mcp.list"
        {
            tracing::info!(plugin = self.plugin, method, "插件改了宿主的数据");
        }
        // 改了配置文件：告诉主程序重新读（MCP 服务器的增删即时生效）。
        if (method == "config.patch" || method.starts_with("mcp.") && method != "mcp.list")
            && let Some(changed) = crate::app::app_services().config_changed
        {
            changed();
        }
        Ok(result.to_string())
    }

    fn need_sessions_write(&self) -> Result<(), String> {
        self.need(self.capabilities.sessions_write, "dshr:sessions:write")?;
        self.need_act()
    }

    fn load_config(&self) -> Result<Config, String> {
        Config::load(&self.paths()).map_err(|error| format!("cannot read the settings: {error}"))
    }

    /// 把 `patch`（JSON 对象）合并进现在的设置，检查合并结果还是合法的设置，再写回 `config.toml`。
    fn patch_config(&self, patch: Value) -> Result<Value, String> {
        if !patch.is_object() {
            return Err("config.patch expects an object".into());
        }
        let mut current = serde_json::to_value(self.load_config()?).map_err(|error| error.to_string())?;
        merge(&mut current, patch);
        let config: Config = serde_json::from_value(current).map_err(|error| format!("the patched settings are not valid: {error}"))?;
        config.save(&self.paths()).map_err(|error| format!("cannot save the settings: {error}"))?;
        serde_json::to_value(config).map_err(|error| error.to_string())
    }

    fn skills_dir(&self) -> PathBuf {
        self.home.join("skills")
    }

    /// `skills/<名字>/SKILL.md`；名字只能是小写字母、数字、`-`、`_`。
    fn skill_file(&self, name: &str) -> Result<PathBuf, String> {
        if !crate::manifest::valid_id(name) {
            return Err(format!("invalid skill name {name}: use lowercase letters, digits, `-` and `_`"));
        }
        Ok(self.skills_dir().join(name).join("SKILL.md"))
    }

    fn list_skills(&self) -> Value {
        let Ok(entries) = std::fs::read_dir(self.skills_dir()) else { return json!([]) };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.path().join("SKILL.md").is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        Value::Array(names.into_iter().map(Value::String).collect())
    }

    fn list_sessions(&self) -> Value {
        let sessions = dshr_agent::list_sessions_cached(&self.paths().sessions_dir());
        Value::Array(
            sessions
                .into_iter()
                .map(|session| {
                    let modified = session
                        .modified
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|since| since.as_millis() as u64);
                    json!({
                        "id": session.id,
                        "title": session.title,
                        "modified": modified,
                        "workspace": session.workspace.map(|path| path.display().to_string()),
                    })
                })
                .collect(),
        )
    }

    fn read_session(&self, id: &str, last: usize) -> Result<Value, String> {
        if !crate::manifest::valid_session_id(id) {
            return Err(format!("invalid session id {id}"));
        }
        let path = dshr_agent::session_log_path(&self.paths().sessions_dir(), id);
        // 从文件末尾往前一行行读，凑够 `last` 条就停：长会话里只读、只解析用得着的那一截，
        // 不再把整份日志解析一遍、再把每条事件都转成 JSON 值。解析不了的行（没写完的最后一行、坏行）跳过。
        let failed = |error: std::io::Error| format!("cannot read session {id}: {error}");
        let mut lines = super::host::ReverseLines::open(&path).map_err(failed)?;
        let mut events = Vec::new();
        while events.len() < last {
            let Some(line) = lines.next_line().map_err(failed)? else { break };
            if let Ok(dshr_core::LogEntry::Known(event)) = serde_json::from_str::<dshr_core::LogEntry>(&line)
                && let Ok(value) = serde_json::to_value(&event)
            {
                events.push(value);
            }
        }
        events.reverse();
        Ok(Value::Array(events))
    }

    /// `{ title, workspace, events: [{ type, data }] }` → 一个新会话，返回它的 id。只接受普通的对话事件。
    fn import_session(&self, input: Value) -> Result<Value, String> {
        #[derive(Deserialize)]
        struct Import {
            title: Option<String>,
            workspace: Option<String>,
            events: Vec<Value>,
        }
        let import: Import = serde_json::from_value(input).map_err(|error| format!("invalid import: {error}"))?;
        if import.events.len() > MAX_EVENTS * 10 {
            return Err("too many events to import".into());
        }
        let mut bodies = Vec::new();
        if let Some(workspace) = import.workspace {
            bodies.push(EventBody::SessionWorkspace { path: workspace });
        }
        if let Some(title) = import.title {
            bodies.push(EventBody::SessionTitle { title });
        }
        for (index, event) in import.events.into_iter().enumerate() {
            let body: EventBody = serde_json::from_value(event).map_err(|error| format!("event {index} is not a valid event: {error}"))?;
            bodies.push(body);
        }
        let id = new_session_id();
        let paths = self.paths();
        let mut log = SessionLog::create(paths.session_log(&id)).map_err(|error| format!("cannot create the session: {error}"))?;
        log.append_all(bodies).map_err(|error| format!("cannot write the session: {error}"))?;
        Ok(Value::String(id))
    }
}

/// JSON 合并：对象逐键合并，其余直接替换；`null` 删掉这一项。
fn merge(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                if value.is_null() {
                    target.remove(&key);
                } else {
                    merge(target.entry(key).or_insert(Value::Null), value);
                }
            }
        }
        (target, patch) => *target = patch,
    }
}

fn new_session_id() -> String {
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    format!("{}-{:08x}", time.as_millis(), u32::from_le_bytes(bytes))
}
