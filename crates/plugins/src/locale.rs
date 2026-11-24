//! 插件的多语言：界面当前语言、插件自带的语言文件（`locales/<语言>.json`），以及按语言回退的查找。
//!
//! 语言文件是 key → 文案的 JSON 对象，可以嵌套（`{"config": {"count": {"title": "数量"}}}` 等于
//! `{"config.count.title": "数量"}`）。查找时依次试：完整的语言代码 → 同一语种的其它写法（繁体中文之间互相优先）
//! → 插件的默认语言（清单里的 `default_locale`，默认 `en`）→ `en`。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::RwLock;

use serde_json::Value;

/// 一个插件的全部文案：语言代码 → (key → 文案)。
pub type Messages = BTreeMap<String, BTreeMap<String, String>>;

/// 插件文件夹里放语言文件的子目录。
pub const LOCALES_DIR: &str = "locales";

/// 没写 `default_locale` 时的默认语言。
pub const DEFAULT_LOCALE: &str = "en";

/// 单个语言文件的上限：文案不该有这么大。
const MAX_FILE_BYTES: u64 = 1024 * 1024;

static CURRENT: RwLock<String> = RwLock::new(String::new());

/// 设置界面当前语言（桌面版切换语言时调用）。插件读 `host.i18n.locale` 拿到的就是它。
pub fn set_locale(code: &str) {
    *CURRENT.write().unwrap_or_else(std::sync::PoisonError::into_inner) = code.to_owned();
}

/// 界面当前语言；没设置过时是 `zh-CN`（和配置的默认值一样）。
pub fn current_locale() -> String {
    let current = CURRENT.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    if current.is_empty() { "zh-CN".to_owned() } else { current.clone() }
}

/// 读插件文件夹里的 `locales/*.json`。没有这个目录时是空的；某个文件写错了就报错（和清单写错一样）。
pub fn load(dir: &Path) -> Result<Messages, String> {
    let mut messages = Messages::new();
    let Ok(entries) = std::fs::read_dir(dir.join(LOCALES_DIR)) else {
        return Ok(messages);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(code) = path.file_stem().and_then(|stem| stem.to_str()).map(str::to_owned) else { continue };
        if !valid_code(&code) {
            return Err(format!("语言文件 {LOCALES_DIR}/{code}.json 的名字不是语言代码（如 en、zh-CN）"));
        }
        if entry.metadata().is_ok_and(|metadata| metadata.len() > MAX_FILE_BYTES) {
            return Err(format!("语言文件 {LOCALES_DIR}/{code}.json 太大（上限 1 MB）"));
        }
        let text = std::fs::read_to_string(&path).map_err(|error| format!("读不了 {LOCALES_DIR}/{code}.json：{error}"))?;
        let value: Value = serde_json::from_str(&text).map_err(|error| format!("{LOCALES_DIR}/{code}.json 不是合法的 JSON：{error}"))?;
        if !value.is_object() {
            return Err(format!("{LOCALES_DIR}/{code}.json 应当是一个对象（key → 文案）"));
        }
        let mut flat = BTreeMap::new();
        flatten("", &value, &mut flat);
        messages.insert(code, flat);
    }
    Ok(messages)
}

/// 语言代码：字母开头，字母、数字、`-`、`_`，最长 16。
pub fn valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 16
        && code.starts_with(|c: char| c.is_ascii_alphabetic())
        && code.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn flatten(prefix: &str, value: &Value, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let key = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                flatten(&key, value, out);
            }
        }
        Value::String(text) => {
            out.insert(prefix.to_owned(), text.clone());
        }
        Value::Number(_) | Value::Bool(_) => {
            out.insert(prefix.to_owned(), value.to_string());
        }
        Value::Null | Value::Array(_) => {}
    }
}

fn primary(code: &str) -> String {
    code.split(['-', '_']).next().unwrap_or(code).to_ascii_lowercase()
}

/// 繁体中文的几种写法（香港、台湾、`zh-Hant`）互相优先于简体。
fn traditional(code: &str) -> bool {
    let lower = code.to_ascii_lowercase().replace('_', "-");
    matches!(lower.as_str(), "zh-hk" | "zh-tw" | "zh-mo" | "zh-hant") || lower.starts_with("zh-hant-")
}

/// 按回退顺序排好的候选语言：都是 `available` 里有的，不重复。
pub fn candidates<'a>(locale: &str, available: impl IntoIterator<Item = &'a str> + Clone, default: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |code: &str| {
        if !out.iter().any(|known| known == code) {
            out.push(code.to_owned());
        }
    };
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b) || a.replace('_', "-").eq_ignore_ascii_case(&b.replace('_', "-"));
    let language = primary(locale);
    // 1. 完全一样（不分大小写，`_` 和 `-` 一样）。
    for code in available.clone() {
        if same(code, locale) {
            push(code);
        }
    }
    // 2. 同一语种：繁体之间互相优先，然后是只写语种的（`zh`），再是其它地区。
    let mut related: Vec<&str> = available.clone().into_iter().filter(|code| primary(code) == language).collect();
    related.sort_by_key(|code| {
        let rank = if traditional(locale) && traditional(code) {
            0
        } else if !code.contains(['-', '_']) {
            1
        } else {
            2
        };
        (rank, code.to_owned())
    });
    for code in related {
        push(code);
    }
    // 3. 插件的默认语言，最后是英文。
    for fallback in [default, DEFAULT_LOCALE] {
        for code in available.clone() {
            if same(code, fallback) {
                push(code);
            }
        }
        let language = primary(fallback);
        for code in available.clone() {
            if primary(code) == language {
                push(code);
            }
        }
    }
    out
}

/// 在 `messages` 里按回退顺序找 `key`。
pub fn lookup<'a>(messages: &'a Messages, locale: &str, default: &str, key: &str) -> Option<&'a str> {
    candidates(locale, messages.keys().map(String::as_str), default)
        .iter()
        .find_map(|code| messages.get(code)?.get(key))
        .map(String::as_str)
}

/// 从“语言代码 → 值”的对象里按回退顺序挑一个（`{ "zh-CN": "数量", "en": "Count" }`）。
pub fn pick<'a>(table: &'a serde_json::Map<String, Value>, locale: &str, default: &str) -> Option<&'a Value> {
    candidates(locale, table.keys().map(String::as_str), default).iter().find_map(|code| table.get(code))
}

/// 把 `{name}` 换成 `vars` 里的值；没有对应的值时原样保留。
pub fn format(template: &str, vars: &serde_json::Map<String, Value>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if vars.contains_key(&after[..end]) => {
                match &vars[&after[..end]] {
                    Value::String(text) => out.push_str(text),
                    other => out.push_str(&other.to_string()),
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(list: &[&str]) -> Vec<String> {
        list.iter().map(|code| (*code).to_owned()).collect()
    }

    #[test]
    fn exact_then_same_language_then_default() {
        let available = ["en", "zh-CN", "zh-TW", "ja"];
        assert_eq!(candidates("zh-HK", available, "en"), codes(&["zh-TW", "zh-CN", "en"]));
        assert_eq!(candidates("zh-CN", available, "en"), codes(&["zh-CN", "zh-TW", "en"]));
        assert_eq!(candidates("fr", available, "ja"), codes(&["ja", "en"]));
        assert_eq!(candidates("en", ["EN_us", "de"], "de"), codes(&["EN_us", "de"]));
    }

    #[test]
    fn bare_language_beats_other_regions() {
        assert_eq!(candidates("pt-BR", ["pt-PT", "pt"], "en"), codes(&["pt", "pt-PT"]));
    }

    #[test]
    fn nested_files_are_flattened() {
        let dir = std::env::temp_dir().join(format!("dshr-locale-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(LOCALES_DIR)).unwrap();
        std::fs::write(dir.join("locales/zh-CN.json"), r#"{"config": {"count": {"title": "数量"}}, "hello": "你好，{name}"}"#).unwrap();
        std::fs::write(dir.join("locales/en.json"), r#"{"hello": "Hello, {name}", "n": 3}"#).unwrap();
        let messages = load(&dir).unwrap();
        assert_eq!(lookup(&messages, "zh-HK", "en", "config.count.title"), Some("数量"));
        assert_eq!(lookup(&messages, "de", "en", "config.count.title"), None);
        assert_eq!(lookup(&messages, "de", "en", "n"), Some("3"));
        let vars = serde_json::json!({ "name": "dshr" });
        assert_eq!(format(lookup(&messages, "en", "en", "hello").unwrap(), vars.as_object().unwrap()), "Hello, dshr");
        std::fs::write(dir.join("locales/bad name.json"), "{}").unwrap();
        assert!(load(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn format_keeps_unknown_placeholders() {
        let vars = serde_json::json!({ "a": 1 });
        assert_eq!(format("{a} {b} {", vars.as_object().unwrap()), "1 {b} {");
    }
}
