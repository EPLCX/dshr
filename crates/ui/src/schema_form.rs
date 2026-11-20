//! 按 JSON Schema 生成设置表单的数据部分：从插件声明的配置 schema（`Manifest::config`）里取出能用表单编辑的
//! 项，并把用户输入的文字解析、校验成要写进 `[plugins.<id>]` 的值。
//!
//! 与 GPUI 无关，便于单独测试；画控件在 `settings.rs` 里。只认 schema 的一个子集：顶层对象里的布尔、
//! 整数、小数、字符串和字符串枚举。嵌套的表、数组（如 MCP 的服务器列表）不在表单里显示，那些配置要么有
//! 专门的界面，要么直接改配置文件。WASM 插件以后声明的也是同一种 schema。

use serde_json::Value;

/// 插件开关，设置页用单独的开关显示，不算配置项。
const ENABLED_KEY: &str = "enabled";

#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Bool,
    Integer,
    Number,
    Text,
    /// 几个固定的字符串里选一个。
    Choice(Vec<String>),
}

/// 表单里的一项。
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// `[plugins.<id>]` 里的键。
    pub key: String,
    /// schema 里的说明（Rust 插件是字段的文档注释）。
    pub description: Option<String>,
    pub kind: FieldKind,
    /// 没写这一项时插件用的值。
    pub default: Option<Value>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldError {
    NotANumber,
    NotAnInteger,
    /// 超出范围；两头都可能没有限制。
    OutOfRange {
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    NotAChoice,
}

/// schema 顶层对象里能用表单编辑的项，按键名排序。
pub fn fields(schema: &Value) -> Vec<Field> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut fields: Vec<Field> = properties
        .iter()
        .filter(|(key, _)| key.as_str() != ENABLED_KEY)
        .filter_map(|(key, property)| {
            // 引用的定义（如单独定义的枚举类型）先展开；说明和默认值写在引用处的优先。
            let target = resolve(schema, property);
            let pick = |name: &str| property.get(name).or_else(|| target.get(name));
            Some(Field {
                key: key.clone(),
                description: pick("description").and_then(Value::as_str).map(str::to_owned),
                kind: kind_of(target)?,
                default: pick("default").filter(|value| !value.is_null()).cloned(),
                minimum: pick("minimum").and_then(Value::as_f64),
                maximum: pick("maximum").and_then(Value::as_f64),
            })
        })
        .collect();
    fields.sort_by(|a, b| a.key.cmp(&b.key));
    fields
}

/// `{"$ref": "#/$defs/名字"}` 指向的定义；不是引用或找不到时原样返回。
fn resolve<'a>(schema: &'a Value, property: &'a Value) -> &'a Value {
    property
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix("#/"))
        .and_then(|path| path.split('/').try_fold(schema, |node, part| node.get(part)))
        .unwrap_or(property)
}

fn kind_of(property: &Value) -> Option<FieldKind> {
    if let Some(choices) = choices_of(property) {
        return Some(FieldKind::Choice(choices));
    }
    // `Option<T>` 的类型写成 `["integer", "null"]`：按 T 处理，留空就是不设置。
    let type_name = match property.get("type")? {
        Value::String(name) => name.as_str(),
        Value::Array(names) => names.iter().filter_map(Value::as_str).find(|name| *name != "null")?,
        _ => return None,
    };
    match type_name {
        "boolean" => Some(FieldKind::Bool),
        "integer" => Some(FieldKind::Integer),
        "number" => Some(FieldKind::Number),
        "string" => Some(FieldKind::Text),
        _ => None,
    }
}

/// 字符串枚举的两种写法：`enum: [..]`，或每个取值各带说明的 `oneOf: [{const: ..}, ..]`。
fn choices_of(property: &Value) -> Option<Vec<String>> {
    let strings = |values: &Vec<Value>, pick: fn(&Value) -> Option<&Value>| -> Option<Vec<String>> {
        values.iter().map(|value| pick(value)?.as_str().map(str::to_owned)).collect()
    };
    let choices = match (property.get("enum"), property.get("oneOf")) {
        (Some(Value::Array(values)), _) => strings(values, |value| Some(value))?,
        (None, Some(Value::Array(variants))) => strings(variants, |variant| variant.get("const"))?,
        _ => return None,
    };
    (!choices.is_empty()).then_some(choices)
}

impl Field {
    /// 输入框里显示的文字：配置文件里写的值，没写时是默认值。
    pub fn display(&self, value: Option<&Value>) -> String {
        match value.or(self.default.as_ref()) {
            Some(Value::String(text)) => text.clone(),
            Some(value) => value.to_string(),
            None => String::new(),
        }
    }

    /// 开关的状态：配置文件里写的值，没写时是默认值。
    pub fn checked(&self, value: Option<&Value>) -> bool {
        value.or(self.default.as_ref()).and_then(Value::as_bool).unwrap_or(false)
    }

    /// 把输入的文字变成要写进配置的值。留空、或与默认值相同时返回 `None`：这一项不写进文件，
    /// 以后默认值变了也跟着变。
    pub fn parse(&self, text: &str) -> Result<Option<Value>, FieldError> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let value = match &self.kind {
            FieldKind::Bool => Value::Bool(matches!(text, "true" | "on" | "1")),
            FieldKind::Integer => {
                let number: i64 = text.parse().map_err(|_| FieldError::NotAnInteger)?;
                self.check_range(number as f64)?;
                Value::from(number)
            }
            FieldKind::Number => {
                let number: f64 = text.parse().ok().filter(|number: &f64| number.is_finite()).ok_or(FieldError::NotANumber)?;
                self.check_range(number)?;
                Value::from(number)
            }
            FieldKind::Text => Value::String(text.to_owned()),
            FieldKind::Choice(choices) => {
                if !choices.iter().any(|choice| choice == text) {
                    return Err(FieldError::NotAChoice);
                }
                Value::String(text.to_owned())
            }
        };
        Ok(self.normalize(value))
    }

    /// 与默认值相同的值不必写进文件。
    pub fn normalize(&self, value: Value) -> Option<Value> {
        let same = match (&value, &self.default) {
            // 0.75 与 0.750、3 与 3.0 都算相同。
            (Value::Number(a), Some(Value::Number(b))) => a.as_f64() == b.as_f64(),
            (value, Some(default)) => value == default,
            (_, None) => false,
        };
        (!same).then_some(value)
    }

    fn check_range(&self, number: f64) -> Result<(), FieldError> {
        let below = self.minimum.is_some_and(|minimum| number < minimum);
        let above = self.maximum.is_some_and(|maximum| number > maximum);
        if below || above {
            return Err(FieldError::OutOfRange { minimum: self.minimum, maximum: self.maximum });
        }
        Ok(())
    }
}

/// 范围两头显示用的数字：整数不带小数点。
pub fn format_bound(bound: f64) -> String {
    if bound.fract() == 0. && bound.abs() < 1e15 { format!("{}", bound as i64) } else { bound.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "threshold": {"type": "number", "format": "double", "default": 0.75, "minimum": 0.1, "maximum": 1.0,
                              "description": "达到这个比例时压缩。"},
                "retries": {"type": "integer", "format": "uint32", "default": 3, "minimum": 0},
                "timeout_ms": {"type": ["integer", "null"], "minimum": 1},
                "verbose": {"type": "boolean", "default": false},
                "label": {"type": "string"},
                "mode": {"$ref": "#/$defs/Mode", "default": "fast", "description": "运行方式。"},
                "level": {"type": "string", "enum": ["low", "high"], "default": "low"},
                // 这些不在表单里。
                "enabled": {"type": "boolean", "default": true},
                "servers": {"type": "object", "additionalProperties": {"type": "object"}},
                "tags": {"type": "array", "items": {"type": "string"}},
                "odd": {"enum": [1, 2]}
            },
            "$defs": {
                "Mode": {"oneOf": [
                    {"const": "fast", "type": "string", "description": "快"},
                    {"const": "careful", "type": "string", "description": "稳"}
                ]}
            }
        })
    }

    fn field(key: &str) -> Field {
        fields(&schema()).into_iter().find(|field| field.key == key).unwrap()
    }

    #[test]
    fn picks_the_fields_a_form_can_edit() {
        let all = fields(&schema());
        let keys: Vec<&str> = all.iter().map(|field| field.key.as_str()).collect();
        assert_eq!(keys, ["label", "level", "mode", "retries", "threshold", "timeout_ms", "verbose"]);

        let threshold = field("threshold");
        assert_eq!(threshold.kind, FieldKind::Number);
        assert_eq!((threshold.minimum, threshold.maximum), (Some(0.1), Some(1.0)));
        assert_eq!(threshold.description.as_deref(), Some("达到这个比例时压缩。"));
        assert_eq!(field("timeout_ms").kind, FieldKind::Integer);
        assert_eq!(field("level").kind, FieldKind::Choice(vec!["low".into(), "high".into()]));
        // 引用的枚举展开成选项，说明和默认值取引用处写的。
        let mode = field("mode");
        assert_eq!(mode.kind, FieldKind::Choice(vec!["fast".into(), "careful".into()]));
        assert_eq!((mode.default, mode.description.as_deref()), (Some(json!("fast")), Some("运行方式。")));

        assert!(fields(&json!({"type": "string"})).is_empty());
        assert!(fields(&json!(null)).is_empty());
    }

    #[test]
    fn shows_the_configured_value_or_the_default() {
        assert_eq!(field("threshold").display(None), "0.75");
        assert_eq!(field("threshold").display(Some(&json!(0.6))), "0.6");
        assert_eq!(field("mode").display(None), "fast");
        assert_eq!(field("timeout_ms").display(None), "");
        assert!(!field("verbose").checked(None));
        assert!(field("verbose").checked(Some(&json!(true))));
    }

    #[test]
    fn parses_and_validates_input() {
        let threshold = field("threshold");
        assert_eq!(threshold.parse(" 0.6 "), Ok(Some(json!(0.6))));
        // 留空或写回默认值：不写进文件。
        assert_eq!(threshold.parse(""), Ok(None));
        assert_eq!(threshold.parse("0.750"), Ok(None));
        assert_eq!(threshold.parse("很多"), Err(FieldError::NotANumber));
        assert_eq!(threshold.parse("NaN"), Err(FieldError::NotANumber));
        assert_eq!(threshold.parse("1.5"), Err(FieldError::OutOfRange { minimum: Some(0.1), maximum: Some(1.0) }));
        assert_eq!(threshold.parse("0"), Err(FieldError::OutOfRange { minimum: Some(0.1), maximum: Some(1.0) }));

        let retries = field("retries");
        assert_eq!(retries.parse("5"), Ok(Some(json!(5))));
        assert_eq!(retries.parse("3"), Ok(None));
        assert_eq!(retries.parse("2.5"), Err(FieldError::NotAnInteger));
        assert_eq!(retries.parse("-1"), Err(FieldError::OutOfRange { minimum: Some(0.), maximum: None }));

        assert_eq!(field("timeout_ms").parse("60000"), Ok(Some(json!(60000))));
        assert_eq!(field("label").parse(" 夜间 "), Ok(Some(json!("夜间"))));
        assert_eq!(field("mode").parse("careful"), Ok(Some(json!("careful"))));
        assert_eq!(field("mode").parse("fast"), Ok(None));
        assert_eq!(field("mode").parse("reckless"), Err(FieldError::NotAChoice));

        // 开关直接给值：与默认相同的不写。
        assert_eq!(field("verbose").normalize(json!(true)), Some(json!(true)));
        assert_eq!(field("verbose").normalize(json!(false)), None);
        assert_eq!(field("label").normalize(json!("x")), Some(json!("x")));
    }

    #[test]
    fn bounds_are_shown_without_noise() {
        assert_eq!(format_bound(1.0), "1");
        assert_eq!(format_bound(0.1), "0.1");
        assert_eq!(format_bound(-3.0), "-3");
    }

    /// 内置插件真正声明的 schema（schemars 生成）也能读出来。
    #[test]
    fn reads_the_schema_of_a_real_plugin() {
        let plugins = dshr_context::Context::new();
        plugins.add_group(dshr_agent::AgentPlugins).unwrap();
        let compaction = plugins.describe().into_iter().find(|plugin| plugin.id == "compaction").unwrap();
        let fields = fields(&compaction.config.unwrap());
        assert_eq!(fields.len(), 1);
        let threshold = &fields[0];
        assert_eq!((threshold.key.as_str(), &threshold.kind), ("threshold", &FieldKind::Number));
        assert_eq!(threshold.default, Some(json!(0.75)));
        assert_eq!((threshold.minimum, threshold.maximum), (Some(0.1), Some(1.0)));
        assert!(threshold.description.is_some());
    }
}
