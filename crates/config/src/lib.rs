//! 数据目录与配置。
//!
//! 数据目录默认 `~/.dshr`，可用 `DSHR_HOME` 覆盖；与原版的 `~/.dsh` 分开，
//! 原版数据通过导入器（M5）迁移。
//!
//! `config.toml` 只保存用户在文件或设置页里写下的值；环境变量覆盖只在运行时生效
//! （[`Config::with_env`]），不会被设置页写回文件。API Key 存在系统钥匙串里（[`credentials`]）。

pub mod account;
pub mod credentials;
pub mod keybindings;
pub mod providers;
pub mod tls;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::{env, fs, io};

use serde::{Deserialize, Serialize};

pub use keybindings::Keybindings;
pub use providers::{ModelEntry, ModelRef, PLUGIN_ADAPTER, PromptCache, Protocol, ProviderConfig, ResolvedProvider};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("找不到用户主目录，请设置 DSHR_HOME")]
    NoHome,
    #[error("无法读取 {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("无法写入 {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} 格式错误: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("{path} 格式错误: {source}")]
    ParseJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self, ConfigError> {
        let home = match env::var_os("DSHR_HOME") {
            Some(home) if !home.is_empty() => PathBuf::from(home),
            _ => dirs::home_dir().ok_or(ConfigError::NoHome)?.join(".dshr"),
        };
        Ok(Self { home })
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    pub fn keybindings_file(&self) -> PathBuf {
        self.home.join("keybindings.json")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.home.join("sessions")
    }

    /// 脚本插件所在目录：每个插件一个子文件夹。
    pub fn plugins_dir(&self) -> PathBuf {
        self.home.join("plugins")
    }

    /// 与原版 v1 日志同名：`sessions/<id>/session.v1.jsonl`。
    pub fn session_log(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(session_id).join("session.v1.jsonl")
    }
}

/// 新会话的默认模型，与原版默认路由一致。
pub const DEFAULT_MODEL: &str = "deepseek/deepseek-flash";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    /// 新会话默认使用的模型，`provider/model`。
    pub default: String,
    /// 新会话默认的思考等级；不写时不发送，用模型自己的默认值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// 模型上下文窗口（tokens），超过 3/4 时压缩较早的轮次。
    pub context_window: u32,
}

/// 思考等级，按 OpenAI 协议的 `reasoning_effort` 发送（DeepSeek 认 `off` / `low` / `high` / `max`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Off,
    Low,
    High,
    Max,
}

impl ReasoningEffort {
    pub const ALL: [ReasoningEffort; 4] = [Self::Off, Self::Low, Self::High, Self::Max];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self { default: DEFAULT_MODEL.into(), effort: None, context_window: 128_000 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

/// 动态效果偏好（辅助功能）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotionPreference {
    /// 跟随系统的“动画效果”设置。
    #[default]
    System,
    /// 减少动态效果：过渡与动画直接跳到结束状态。
    Reduce,
    /// 始终播放完整动画。
    Full,
}

/// 高对比度（辅助功能）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HighContrastPreference {
    Off,
    On,
    #[default]
    /// 跟随系统的对比度主题（Windows 的“对比度主题”、macOS 的“增强对比度”）。
    System,
}

/// 以前这一项是开关（`true` / `false`），旧的配置文件照样能读。
impl<'de> Deserialize<'de> for HighContrastPreference {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Name(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Bool(true) => Ok(Self::On),
            Raw::Bool(false) => Ok(Self::Off),
            Raw::Name(name) => match name.as_str() {
                "off" => Ok(Self::Off),
                "on" => Ok(Self::On),
                "system" => Ok(Self::System),
                other => Err(serde::de::Error::unknown_variant(other, &["off", "on", "system"])),
            },
        }
    }
}

/// 界面文字大小（辅助功能）：跟随系统，或固定的百分比（100 是默认大小）。
/// 配置文件里写 `"system"` 或数字（以前只有数字）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TextScalePreference {
    /// 跟随系统的文字大小（Windows：设置 → 辅助功能 → 文本大小）。
    #[default]
    System,
    Percent(u16),
}

impl Serialize for TextScalePreference {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::System => serializer.serialize_str("system"),
            Self::Percent(percent) => serializer.serialize_u16(*percent),
        }
    }
}

impl<'de> Deserialize<'de> for TextScalePreference {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Percent(u16),
            Name(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Percent(percent) => Ok(Self::Percent(percent)),
            Raw::Name(name) if name == "system" => Ok(Self::System),
            Raw::Name(other) => Err(serde::de::Error::unknown_variant(&other, &["system"])),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: ThemePreference,
    pub motion: MotionPreference,
    /// 界面语言，如 `zh-CN`、`en`。
    pub language: String,
    /// 欢迎窗里点过“稍后设置”。
    pub welcome_skipped: bool,
    /// 关闭窗口时隐藏到系统托盘（菜单栏），而不是退出。
    pub close_to_tray: bool,
    /// 高可见性控件轮廓：键盘聚焦和点击控件时画更粗、对比更强的轮廓。
    pub high_visibility_outlines: bool,
    /// 高对比度：开启（或跟随系统且系统开着）时使用内置的高对比度配色，此时 `palette` 不起作用。
    pub high_contrast: HighContrastPreference,
    /// 界面文字大小。
    pub text_scale: TextScalePreference,
    /// 侧边栏里的工作区（用户选过的目录），最近使用的在前。
    pub workspaces: Vec<PathBuf>,
    /// 侧边栏收起成一列图标按钮。
    pub sidebar_collapsed: bool,
    /// 配色方案（`<数据目录>/themes/<id>.toml` 或插件的 `themes`）；空的是默认配色。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub palette: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: ThemePreference::System,
            motion: MotionPreference::System,
            language: "zh-CN".into(),
            welcome_skipped: false,
            close_to_tray: true,
            high_visibility_outlines: false,
            high_contrast: HighContrastPreference::System,
            text_scale: TextScalePreference::System,
            workspaces: Vec::new(),
            sidebar_collapsed: false,
            palette: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: ModelConfig,
    pub ui: UiConfig,
    pub providers: Vec<ProviderConfig>,
    /// 各插件的配置，`[plugins.<id>]`。内容由插件自己解释；`enabled = false` 停用该插件。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, toml::Table>,
    /// 插件索引（`[plugin_index] urls = [...]`）：搜索、按 id 安装、查更新。默认没有。
    #[serde(skip_serializing_if = "PluginIndexConfig::is_empty")]
    pub plugin_index: PluginIndexConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginIndexConfig {
    pub urls: Vec<String>,
}

impl PluginIndexConfig {
    pub fn is_empty(&self) -> bool {
        self.urls.is_empty()
    }
}

/// 设置页里显示的一个 MCP 服务器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerEntry {
    pub name: String,
    /// 启动命令和参数，用空格连起来，只用于显示。
    pub command_line: String,
    pub enabled: bool,
}

fn table_enabled(table: &toml::Table) -> bool {
    table.get("enabled").and_then(toml::Value::as_bool).unwrap_or(true)
}

/// 启用是默认值，不写进文件；停用写 `enabled = false`。
fn set_table_enabled(table: &mut toml::Table, enabled: bool) {
    if enabled {
        table.remove("enabled");
    } else {
        table.insert("enabled".into(), toml::Value::Boolean(false));
    }
}

impl Default for Config {
    fn default() -> Self {
        let deepseek = providers::adapter("deepseek").expect("内置 DeepSeek 适配器");
        Self {
            model: ModelConfig::default(),
            ui: UiConfig::default(),
            providers: vec![ProviderConfig::from_adapter(deepseek)],
            plugins: BTreeMap::new(),
            plugin_index: PluginIndexConfig::default(),
        }
    }
}

impl Config {
    /// 读取 `config.toml`，不存在则用默认值。不含环境变量覆盖。
    pub fn load(paths: &Paths) -> Result<Self, ConfigError> {
        let path = paths.config_file();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        toml::from_str(&text).map_err(|source| ConfigError::Parse { path, source })
    }

    /// 写回 `config.toml`：先写临时文件再改名，避免写到一半损坏。
    pub fn save(&self, paths: &Paths) -> Result<(), ConfigError> {
        let path = paths.config_file();
        let write_error = |source| ConfigError::Write { path: path.clone(), source };
        fs::create_dir_all(&paths.home).map_err(write_error)?;
        let text = toml::to_string_pretty(self).expect("Config 总能序列化");
        let temp = path.with_extension("toml.tmp");
        fs::write(&temp, text).map_err(write_error)?;
        fs::rename(&temp, &path).map_err(write_error)
    }

    /// 叠加环境变量：`DSHR_MODEL`（`provider/model`）覆盖默认模型，
    /// `DSHR_BASE_URL` 覆盖 DeepSeek 接口地址。API Key 见 [`credentials::resolve_api_key`]。
    pub fn with_env(self) -> Self {
        self.with_overrides(|name| env::var(name).ok())
    }

    fn with_overrides(mut self, lookup: impl Fn(&str) -> Option<String>) -> Self {
        let lookup = |name| lookup(name).filter(|value| !value.trim().is_empty());
        if let Some(url) = lookup("DSHR_BASE_URL")
            && let Some(deepseek) = self.providers.iter_mut().find(|provider| provider.adapter == "deepseek")
        {
            deepseek.base_url = url;
        }
        if let Some(model) = lookup("DSHR_MODEL") {
            self.model.default = match ModelRef::parse(&model) {
                Some(reference) => reference.to_string(),
                None => format!("deepseek/{model}"),
            };
        }
        self
    }

    /// 各插件的配置，转成 JSON 交给插件容器。
    pub fn plugin_configs(&self) -> Vec<(String, serde_json::Value)> {
        self.plugins.iter().filter_map(|(id, table)| Some((id.clone(), serde_json::to_value(table).ok()?))).collect()
    }

    /// 插件是否启用：`[plugins.<id>]` 里没写 `enabled = false` 就是启用。
    pub fn plugin_enabled(&self, id: &str) -> bool {
        self.plugins.get(id).is_none_or(table_enabled)
    }

    pub fn set_plugin_enabled(&mut self, id: &str, enabled: bool) {
        set_table_enabled(self.plugins.entry(id.to_owned()).or_default(), enabled);
        // 只为了停用而建的空表不留在文件里。
        if self.plugins.get(id).is_some_and(toml::Table::is_empty) {
            self.plugins.remove(id);
        }
    }

    /// `[plugins.<id>]` 里某一项的值（转成 JSON）；没写时是 `None`，插件用它的默认值。
    pub fn plugin_value(&self, id: &str, key: &str) -> Option<serde_json::Value> {
        serde_json::to_value(self.plugins.get(id)?.get(key)?).ok()
    }

    /// 设置 `[plugins.<id>]` 里的一项；`None` 把它从文件里去掉，回到插件的默认值。
    /// TOML 表示不了的值（`null`）同样当作去掉。
    pub fn set_plugin_value(&mut self, id: &str, key: &str, value: Option<serde_json::Value>) {
        match value.and_then(|value| toml::Value::try_from(value).ok()) {
            Some(value) => {
                self.plugins.entry(id.to_owned()).or_default().insert(key.to_owned(), value);
            }
            None => {
                if let Some(table) = self.plugins.get_mut(id) {
                    table.remove(key);
                }
            }
        }
        // 清空了的表不留在文件里。
        if self.plugins.get(id).is_some_and(toml::Table::is_empty) {
            self.plugins.remove(id);
        }
    }

    /// `[plugins.mcp.servers.<名字>]` 里配置的 MCP 服务器，按名字排序。
    pub fn mcp_servers(&self) -> Vec<McpServerEntry> {
        let Some(servers) = self.plugins.get("mcp").and_then(|mcp| mcp.get("servers")).and_then(toml::Value::as_table) else {
            return Vec::new();
        };
        let mut entries: Vec<McpServerEntry> = servers
            .iter()
            .filter_map(|(name, server)| {
                let server = server.as_table()?;
                let args = server.get("args").and_then(toml::Value::as_array).into_iter().flatten();
                let command = server.get("command").and_then(toml::Value::as_str).into_iter();
                let command_line: Vec<&str> = command.chain(args.filter_map(toml::Value::as_str)).collect();
                // HTTP 服务器没有命令，显示它的地址。
                let command_line = match server.get("url").and_then(toml::Value::as_str) {
                    Some(url) if command_line.is_empty() => url.to_owned(),
                    _ => command_line.join(" "),
                };
                Some(McpServerEntry { name: name.clone(), command_line, enabled: table_enabled(server) })
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }

    /// 添加（或覆盖同名的）MCP 服务器。
    pub fn add_mcp_server(&mut self, name: &str, command: &str, args: Vec<String>) {
        let mut server = toml::Table::new();
        server.insert("command".into(), toml::Value::String(command.to_owned()));
        if !args.is_empty() {
            server.insert("args".into(), toml::Value::Array(args.into_iter().map(toml::Value::String).collect()));
        }
        self.mcp_servers_mut().insert(name.to_owned(), toml::Value::Table(server));
    }

    pub fn remove_mcp_server(&mut self, name: &str) {
        self.mcp_servers_mut().remove(name);
    }

    pub fn set_mcp_server_enabled(&mut self, name: &str, enabled: bool) {
        if let Some(server) = self.mcp_servers_mut().get_mut(name).and_then(toml::Value::as_table_mut) {
            set_table_enabled(server, enabled);
        }
    }

    /// `[plugins.mcp.servers]`，没有就建；那里原来不是表时换成空表。
    fn mcp_servers_mut(&mut self) -> &mut toml::Table {
        let servers =
            self.plugins.entry("mcp".to_owned()).or_default().entry("servers").or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !servers.is_table() {
            *servers = toml::Value::Table(toml::Table::new());
        }
        servers.as_table_mut().expect("刚确认过是表")
    }

    pub fn providers(&self) -> Vec<ResolvedProvider> {
        self.providers.iter().map(ProviderConfig::resolve).collect()
    }

    pub fn provider(&self, id: &str) -> Option<ResolvedProvider> {
        self.providers.iter().find(|provider| provider.id == id).map(ProviderConfig::resolve)
    }

    /// 默认模型；配置的默认值失效（提供商被删、模型不在目录里）时退回第一个可用模型。
    pub fn default_model(&self) -> Option<ModelRef> {
        let configured = ModelRef::parse(&self.model.default);
        if let Some(reference) = &configured
            && self.provider(&reference.provider).is_some_and(|provider| provider.models.iter().any(|model| model.id == reference.model))
        {
            return configured;
        }
        self.providers()
            .into_iter()
            .find_map(|provider| provider.models.first().map(|model| ModelRef::new(&provider.id, &model.id)))
            .or(configured)
    }

    /// 模型的显示名，如 `DeepSeek-V4.1-Flash`；不在目录里时显示 ID。
    pub fn model_display_name(&self, reference: &ModelRef) -> String {
        self.provider(&reference.provider)
            .and_then(|provider| provider.models.into_iter().find(|model| model.id == reference.model))
            .map(|model| model.display_name().to_owned())
            .unwrap_or_else(|| reference.model.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_contrast_reads_old_switch_and_new_choices() {
        let read = |value: &str| toml::from_str::<UiConfig>(&format!("high_contrast = {value}")).unwrap().high_contrast;
        assert_eq!(read("true"), HighContrastPreference::On);
        assert_eq!(read("false"), HighContrastPreference::Off);
        assert_eq!(read("\"system\""), HighContrastPreference::System);
        let written = toml::to_string(&UiConfig { high_contrast: HighContrastPreference::System, ..Default::default() }).unwrap();
        assert!(written.contains("high_contrast = \"system\""), "{written}");
    }

    #[test]
    fn text_scale_reads_numbers_and_system() {
        let read = |value: &str| toml::from_str::<UiConfig>(&format!("text_scale = {value}")).unwrap().text_scale;
        assert_eq!(read("115"), TextScalePreference::Percent(115));
        assert_eq!(read("\"system\""), TextScalePreference::System);
        assert_eq!(UiConfig::default().text_scale, TextScalePreference::System);
        assert_eq!(UiConfig::default().high_contrast, HighContrastPreference::System);
        let written = toml::to_string(&UiConfig { text_scale: TextScalePreference::Percent(130), ..Default::default() }).unwrap();
        assert!(written.contains("text_scale = 130"), "{written}");
    }

    #[test]
    fn missing_file_uses_defaults_and_partial_file_merges() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { home: dir.path().to_owned() };
        assert_eq!(Config::load(&paths).unwrap(), Config::default());

        fs::write(paths.config_file(), "[model]\neffort = \"max\"\n").unwrap();
        let config = Config::load(&paths).unwrap();
        assert_eq!(config.model.effort, Some(ReasoningEffort::Max));
        assert_eq!(config.model.default, DEFAULT_MODEL);
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.ui.language, "zh-CN");
    }

    #[test]
    fn save_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { home: dir.path().join("nested") };
        let mut config = Config::default();
        config.ui.theme = ThemePreference::Dark;
        config.providers.push(ProviderConfig {
            id: "proxy".into(),
            adapter: providers::CUSTOM_ADAPTER.into(),
            name: "Proxy".into(),
            protocol: Some(Protocol::OpenAi),
            base_url: "https://proxy.example.com/v1".into(),
            models: vec![ModelEntry::new("gpt-x", "GPT X")],
            plugin: None,
            prompt_cache: Some(providers::PromptCache::OneHour),
        });
        config.save(&paths).unwrap();
        assert_eq!(Config::load(&paths).unwrap(), config);
    }

    #[test]
    fn plugin_tables_round_trip_and_convert_to_json() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { home: dir.path().to_owned() };
        fs::write(
            paths.config_file(),
            "[plugins.compaction]\nthreshold = 0.5\n\n[plugins.mcp.servers.files]\ncommand = \"npx\"\nargs = [\"-y\", \"server\"]\n",
        )
        .unwrap();
        let config = Config::load(&paths).unwrap();
        let configs: BTreeMap<_, _> = config.plugin_configs().into_iter().collect();
        assert_eq!(configs["compaction"], serde_json::json!({"threshold": 0.5}));
        assert_eq!(configs["mcp"]["servers"]["files"]["args"], serde_json::json!(["-y", "server"]));

        config.save(&paths).unwrap();
        assert_eq!(Config::load(&paths).unwrap(), config);
    }

    #[test]
    fn plugins_and_mcp_servers_can_be_edited() {
        let mut config = Config::default();
        assert!(config.plugin_enabled("title"));
        config.set_plugin_enabled("title", false);
        assert!(!config.plugin_enabled("title"));
        config.set_plugin_enabled("title", true);
        assert!(config.plugins.is_empty());

        config.add_mcp_server("files", "npx", vec!["-y".into(), "pkg".into()]);
        config.add_mcp_server("bare", "server", Vec::new());
        config.set_mcp_server_enabled("files", false);
        assert_eq!(
            config.mcp_servers(),
            [
                McpServerEntry { name: "bare".into(), command_line: "server".into(), enabled: true },
                McpServerEntry { name: "files".into(), command_line: "npx -y pkg".into(), enabled: false },
            ]
        );
        config.remove_mcp_server("bare");
        config.set_mcp_server_enabled("files", true);
        assert_eq!(config.mcp_servers().len(), 1);
        assert!(config.mcp_servers()[0].enabled);
        // 停用整个 mcp 插件不影响服务器列表。
        config.set_plugin_enabled("mcp", false);
        assert_eq!(config.mcp_servers().len(), 1);
    }

    #[test]
    fn plugin_values_can_be_set_and_cleared() {
        let mut config = Config::default();
        assert_eq!(config.plugin_value("compaction", "threshold"), None);
        config.set_plugin_value("compaction", "threshold", Some(serde_json::json!(0.6)));
        config.set_plugin_value("compaction", "verbose", Some(serde_json::json!(true)));
        config.set_plugin_enabled("compaction", false);
        assert_eq!(config.plugin_value("compaction", "threshold"), Some(serde_json::json!(0.6)));
        let configs: BTreeMap<_, _> = config.plugin_configs().into_iter().collect();
        assert_eq!(configs["compaction"], serde_json::json!({"threshold": 0.6, "verbose": true, "enabled": false}));

        // 清掉一项不动别的；全部清掉后表也不留。
        config.set_plugin_value("compaction", "threshold", None);
        config.set_plugin_value("compaction", "verbose", Some(serde_json::Value::Null));
        assert!(!config.plugin_enabled("compaction"));
        config.set_plugin_enabled("compaction", true);
        assert!(config.plugins.is_empty());
        config.set_plugin_value("missing", "key", None);
        assert!(config.plugins.is_empty());
    }

    #[test]
    fn default_model_falls_back_to_first_available() {
        let mut config = Config::default();
        assert_eq!(config.default_model(), Some(ModelRef::new("deepseek", "deepseek-flash")));
        assert_eq!(config.model_display_name(&config.default_model().unwrap()), "DeepSeek-V4.1-Flash");

        config.model.default = "gone/whatever".into();
        assert_eq!(config.default_model(), Some(ModelRef::new("deepseek", "deepseek-flash")));
    }

    #[test]
    fn env_overrides_apply_at_runtime_only() {
        let config = Config::default().with_overrides(|name| match name {
            "DSHR_MODEL" => Some("deepseek-v4-pro".into()),
            "DSHR_BASE_URL" => Some("https://proxy.example.com".into()),
            _ => None,
        });
        assert_eq!(config.model.default, "deepseek/deepseek-v4-pro");
        assert_eq!(config.providers[0].base_url, "https://proxy.example.com");
    }
}
