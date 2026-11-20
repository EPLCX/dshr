//! 插件密钥：清单里 `secrets = ["API_TOKEN"]` 声明名字，值由用户填。
//!
//! 值存在系统钥匙串里（条目名 `plugin:<id>:<名字>`），不写进 `config.toml`，不出现在 `host.config` 里，
//! 也不进插件日志（脚本把它打出来时换成 `***`）。没有钥匙串条目时读环境变量
//! `DSHR_PLUGIN_SECRET_<ID>_<名字>`（id 大写、`-` 换成 `_`），方便命令行和测试。
//!
//! 脚本有两种用法：`host.secrets.get(name)` 拿到明文；或者 `fetch(url, { secretHeaders: { authorization: "API_TOKEN" } })`
//! 让宿主把值填进请求头，脚本不经手明文。

fn account(plugin: &str, name: &str) -> String {
    format!("plugin:{plugin}:{name}")
}

/// 环境变量名：`DSHR_PLUGIN_SECRET_<ID>_<名字>`。
pub fn env_name(plugin: &str, name: &str) -> String {
    format!("DSHR_PLUGIN_SECRET_{}_{name}", plugin.to_ascii_uppercase().replace('-', "_"))
}

/// 测试里用的内存存储：没有系统钥匙串的环境（CI 容器）也能测保存、读取。
#[cfg(test)]
static MEMORY: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// 插件 `plugin` 的密钥 `name`：钥匙串 → 环境变量。都没有时是 `None`；钥匙串不可用按“没有”处理。
pub fn load(plugin: &str, name: &str) -> Option<String> {
    let non_empty = |value: String| Some(value).filter(|value| !value.is_empty());
    #[cfg(test)]
    if let Some((_, value)) = MEMORY.lock().unwrap().iter().find(|(key, _)| *key == account(plugin, name)) {
        return non_empty(value.clone());
    }
    if let Some(value) = dshr_config::credentials::load_secret(&account(plugin, name)).ok().flatten().and_then(non_empty) {
        return Some(value);
    }
    std::env::var(env_name(plugin, name)).ok().and_then(non_empty)
}

/// “这个密钥设置过没有”的缓存：账户名 → 有没有值。设置页刷新插件卡片时会问每个插件的每个密钥，
/// 每次都去系统钥匙串（Windows 凭据管理器、macOS 钥匙串）查很慢，插件多时会让界面卡顿。
/// 只在本进程里的 [`save`] / [`delete`] 时作废；别的进程（命令行）改过的，下次启动时才看到。
static SET: std::sync::Mutex<Option<std::collections::HashMap<String, bool>>> = std::sync::Mutex::new(None);

fn forget(plugin: &str, name: &str) {
    if let Some(cache) = SET.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_mut() {
        cache.remove(&account(plugin, name));
    }
}

/// 密钥有没有设置（钥匙串里或环境变量里有非空值）。结果会缓存，见 [`SET`]；要用值本身时调用 [`load`]。
pub fn is_set(plugin: &str, name: &str) -> bool {
    let key = account(plugin, name);
    if let Some(&set) = SET.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().and_then(|cache| cache.get(&key)) {
        return set;
    }
    // 查钥匙串时不持锁：别的线程问别的密钥不用排队。
    let set = load(plugin, name).is_some();
    SET.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get_or_insert_with(Default::default).insert(key, set);
    set
}

/// 保存密钥（设置页、`dshr plugin secret set`）。
pub fn save(plugin: &str, name: &str, value: &str) -> Result<(), String> {
    forget(plugin, name);
    #[cfg(test)]
    {
        let mut memory = MEMORY.lock().unwrap();
        memory.retain(|(key, _)| *key != account(plugin, name));
        memory.push((account(plugin, name), value.to_owned()));
        return Ok(());
    }
    #[allow(unreachable_code)]
    dshr_config::credentials::save_secret(&account(plugin, name), value).map_err(|error| error.to_string())
}

/// 删除密钥。
pub fn delete(plugin: &str, name: &str) -> Result<(), String> {
    forget(plugin, name);
    dshr_config::credentials::delete_secret(&account(plugin, name)).map_err(|error| error.to_string())
}
