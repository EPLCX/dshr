//! `host.setConfig(patch)`：插件写自己的配置项（`config.toml` 的 `[plugins.<id>]`）。
//!
//! 只能写清单 `[config.properties]` 里声明过的键，值按 schema 检查（类型、枚举、范围）；`enabled`、`timeout_ms`
//! 由宿主解释，插件不能改。与默认值相同的值不写进文件（和设置页一样），`null` 去掉这一项、回到默认值。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

/// 宿主自己解释的键：插件不能写。
const RESERVED: [&str; 2] = ["enabled", "timeout_ms"];

/// 正在运行的插件看到的配置（`host.config`）：插件写入、用户在设置页修改后就地换掉，脚本按版本号重新读。
pub(crate) struct LiveConfig {
    version: AtomicU64,
    json: Mutex<String>,
}

impl LiveConfig {
    pub fn new(json: String) -> Self {
        Self { version: AtomicU64::new(0), json: Mutex::new(json) }
    }

    pub fn set(&self, config: &Value) {
        *self.json.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = config.to_string();
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn json(&self) -> String {
        self.json.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

/// 检查 `patch` 里的每一项，返回要写的改动：`(键, Some(值))` 写入，`(键, None)` 去掉。有一项不合规就整批拒绝。
pub(crate) fn check_patch(schema: Option<&Value>, patch: &Value) -> Result<Vec<(String, Option<Value>)>, String> {
    let patch = patch.as_object().ok_or("setConfig expects an object of key → value")?;
    let properties = schema.and_then(|schema| schema.get("properties")).and_then(Value::as_object);
    let mut changes = Vec::with_capacity(patch.len());
    for (key, value) in patch {
        if RESERVED.contains(&key.as_str()) {
            return Err(format!("`{key}` is set by the user, not by the plugin"));
        }
        let property = properties
            .and_then(|properties| properties.get(key))
            .ok_or_else(|| format!("`{key}` is not declared in [config] of plugin.toml"))?;
        if value.is_null() {
            changes.push((key.clone(), None));
            continue;
        }
        check_value(key, property, value)?;
        let same_as_default = match (value, property.get("default")) {
            (Value::Number(a), Some(Value::Number(b))) => a.as_f64() == b.as_f64(),
            (value, Some(default)) => value == default,
            (_, None) => false,
        };
        changes.push((key.clone(), (!same_as_default).then(|| value.clone())));
    }
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(changes)
}

fn check_value(key: &str, property: &Value, value: &Value) -> Result<(), String> {
    let kind = property.get("type").and_then(Value::as_str);
    let fits = match kind {
        Some("boolean") => value.is_boolean(),
        Some("integer") => value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|number| number.fract() == 0.0),
        Some("number") => value.is_number(),
        Some("string") => value.is_string(),
        Some("array") => value.is_array(),
        Some("object") => value.is_object(),
        _ => true,
    };
    if !fits {
        return Err(format!("`{key}` must be of type {}", kind.unwrap_or_default()));
    }
    let choices: Option<Vec<&Value>> =
        property.get("enum").and_then(Value::as_array).map(|choices| choices.iter().collect()).or_else(|| {
            property.get("oneOf").and_then(Value::as_array).map(|items| items.iter().filter_map(|item| item.get("const")).collect())
        });
    if let Some(choices) = choices.filter(|choices| !choices.is_empty())
        && !choices.contains(&value)
    {
        let listed: Vec<String> = choices.iter().map(|choice| choice.to_string()).collect();
        return Err(format!("`{key}` must be one of {}", listed.join(", ")));
    }
    if let Some(number) = value.as_f64() {
        let bound = |name: &str| property.get(name).and_then(Value::as_f64);
        if bound("minimum").is_some_and(|minimum| number < minimum) || bound("maximum").is_some_and(|maximum| number > maximum) {
            return Err(format!(
                "`{key}` must be between {} and {}",
                bound("minimum").map_or("-∞".to_owned(), |minimum| minimum.to_string()),
                bound("maximum").map_or("∞".to_owned(), |maximum| maximum.to_string())
            ));
        }
    }
    Ok(())
}

/// 交给脚本的配置：`[plugins.<id>]` 去掉 `enabled`。
pub(crate) fn script_view(raw: Option<&Map<String, Value>>) -> Value {
    let mut config = raw.cloned().unwrap_or_default();
    config.remove("enabled");
    Value::Object(config)
}

#[cfg(test)]
mod tests {
    use super::check_patch;
    use serde_json::json;

    #[test]
    fn checks_declared_keys_types_choices_and_ranges() {
        let schema = json!({ "type": "object", "properties": {
            "count": { "type": "integer", "minimum": 1, "maximum": 10, "default": 3 },
            "level": { "type": "string", "enum": ["low", "high"] },
            "mode": { "type": "string", "oneOf": [{ "const": "a" }, { "const": "b" }] },
            "on": { "type": "boolean" },
        }});
        let changes = check_patch(Some(&schema), &json!({ "count": 5, "level": "high", "on": true, "mode": null })).unwrap();
        assert_eq!(
            changes,
            [
                ("count".to_owned(), Some(json!(5))),
                ("level".to_owned(), Some(json!("high"))),
                ("mode".to_owned(), None),
                ("on".to_owned(), Some(json!(true))),
            ]
        );
        // 与默认值相同：不写进文件。
        assert_eq!(check_patch(Some(&schema), &json!({ "count": 3.0 })).unwrap(), [("count".to_owned(), None)]);
        for bad in [
            json!({ "count": 0 }),
            json!({ "count": 2.5 }),
            json!({ "count": "5" }),
            json!({ "level": "mid" }),
            json!({ "mode": "c" }),
            json!({ "other": 1 }),
            json!({ "enabled": false }),
            json!({ "timeout_ms": 1 }),
            json!([1]),
        ] {
            assert!(check_patch(Some(&schema), &bad).is_err(), "{bad}");
        }
        assert!(check_patch(None, &json!({ "count": 1 })).is_err());
    }
}
