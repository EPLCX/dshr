//! 模型提供商：内置目录（适配器）与用户配置。
//!
//! 内置适配器给出默认接口地址、环境变量与默认模型；用户配置只记录与默认值不同的部分，
//! 例如自定义接口地址或自己维护的模型目录。

use serde::{Deserialize, Serialize};

/// 接口协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// Anthropic Messages 兼容（`{base}/v1/messages`）。
    Messages,
    /// OpenAI Chat Completions 兼容（`{base}/chat/completions`）。
    #[serde(rename = "openai")]
    OpenAi,
}

/// Anthropic 的提示词缓存（prompt caching）：缓存写入后保留多久。只对 Messages 协议生效。
///
/// Anthropic 只缓存请求里显式标记过的前缀（`cache_control`）。缓存命中的输入按约十分之一的价格计费；
/// 写入按 1.25 倍（5 分钟）或 2 倍（1 小时）计费。两次请求间隔经常超过 5 分钟（比如边看边改、
/// 等审批）时 1 小时更划算，一直在连续对话时 5 分钟就够了：每次命中都会重新计时。
/// DeepSeek、OpenAI 等是自动缓存，不需要也不认这个设置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PromptCache {
    /// 不标记缓存。
    #[default]
    #[serde(rename = "off")]
    Off,
    /// 写入的缓存保留 5 分钟（每次命中重新计时）。
    #[serde(rename = "5m")]
    FiveMinutes,
    /// 写入的缓存保留 1 小时（每次命中重新计时）。
    #[serde(rename = "1h")]
    OneHour,
}

impl PromptCache {
    pub const ALL: [PromptCache; 3] = [Self::Off, Self::FiveMinutes, Self::OneHour];

    /// `cache_control` 里的 `ttl`；`Off` 时没有。
    pub fn ttl(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::FiveMinutes => Some("5m"),
            Self::OneHour => Some("1h"),
        }
    }

    /// 没有在配置里写 `prompt_cache` 时的默认值：内置的 Anthropic 提供商默认 5 分钟，其余不标记
    /// （DeepSeek 自动缓存；自定义的 Messages 兼容网关不一定认 `cache_control`，要用时手动打开）。
    pub fn default_for(adapter: &str) -> Self {
        if adapter == "anthropic" { Self::FiveMinutes } else { Self::Off }
    }
}

/// 内置适配器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adapter {
    pub id: &'static str,
    pub name: &'static str,
    pub protocol: Protocol,
    pub base_url: &'static str,
    /// 留空 Key 时读取的环境变量；`None` 表示不需要 Key（如本地 Ollama）。
    pub env_key: Option<&'static str>,
    /// 获取可用模型的接口与鉴权方式；`None` 表示按协议从接口地址推导。
    pub models_url: Option<(&'static str, Protocol)>,
    /// (模型 ID, 显示名)。
    pub default_models: &'static [(&'static str, &'static str)],
}

/// 用户自定义接口时的适配器 ID。
pub const CUSTOM_ADAPTER: &str = "custom";

/// 内置 DeepSeek 提供商（API Key 登录）。
pub const DEEPSEEK: &str = "deepseek";
/// 只走账号登录（OAuth）的 DeepSeek 提供商：凭据是钥匙串里的登录令牌（`x-dsh-auth-token`）。
pub const DEEPSEEK_OAUTH: &str = "deepseek-oauth";
pub const DEEPSEEK_MODELS_URL: &str = "https://api.deepseek.com/models";

pub const ADAPTERS: &[Adapter] = &[
    Adapter {
        id: "deepseek",
        name: "DeepSeek",
        protocol: Protocol::Messages,
        base_url: "https://api.deepseek.com/anthropic",
        env_key: Some("DEEPSEEK_API_KEY"),
        models_url: Some((DEEPSEEK_MODELS_URL, Protocol::OpenAi)),
        default_models: &[("deepseek-flash", "DeepSeek-V4.1-Flash"), ("deepseek-v4-pro", "DeepSeek-V4-Pro")],
    },
    Adapter {
        id: "deepseek-oauth",
        name: "DeepSeek OAuth",
        protocol: Protocol::Messages,
        base_url: "https://api.deepseek.com/anthropic",
        env_key: None,
        models_url: Some((DEEPSEEK_MODELS_URL, Protocol::OpenAi)),
        default_models: &[("deepseek-flash", "DeepSeek-V4.1-Flash"), ("deepseek-v4-pro", "DeepSeek-V4-Pro")],
    },
    Adapter {
        id: "anthropic",
        name: "Anthropic",
        protocol: Protocol::Messages,
        base_url: "https://api.anthropic.com",
        env_key: Some("ANTHROPIC_API_KEY"),
        models_url: None,
        default_models: &[],
    },
    Adapter {
        id: "openai",
        name: "OpenAI",
        protocol: Protocol::OpenAi,
        base_url: "https://api.openai.com/v1",
        env_key: Some("OPENAI_API_KEY"),
        models_url: None,
        default_models: &[],
    },
    Adapter {
        id: "moonshot",
        name: "Kimi (Moonshot)",
        protocol: Protocol::OpenAi,
        base_url: "https://api.moonshot.cn/v1",
        env_key: Some("MOONSHOT_API_KEY"),
        models_url: None,
        default_models: &[],
    },
    Adapter {
        id: "openrouter",
        name: "OpenRouter",
        protocol: Protocol::OpenAi,
        base_url: "https://openrouter.ai/api/v1",
        env_key: Some("OPENROUTER_API_KEY"),
        models_url: None,
        default_models: &[],
    },
    Adapter {
        id: "ollama",
        name: "Ollama",
        protocol: Protocol::OpenAi,
        base_url: "http://localhost:11434/v1",
        env_key: None,
        models_url: None,
        default_models: &[],
    },
];

pub fn adapter(id: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().find(|adapter| adapter.id == id)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

impl ModelEntry {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self { id: id.into(), name: name.into() }
    }

    pub fn display_name(&self) -> &str {
        if self.name.is_empty() { &self.id } else { &self.name }
    }
}

/// `config.toml` 里的一个 `[[providers]]`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// 唯一标识；也用于模型引用（`id/model`）和钥匙串条目名。
    pub id: String,
    /// 内置适配器 ID，或 [`CUSTOM_ADAPTER`]。
    pub adapter: String,
    /// 自定义提供商的显示名。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// 自定义提供商的协议。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// 留空使用适配器默认地址。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// 留空使用适配器默认模型。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelEntry>,
    /// `adapter = "plugin"` 时：提供它的插件服务商，`<插件 id>/<服务商 id>`。地址和鉴权由插件给。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// Messages 协议的提示词缓存：`"off"` / `"5m"` / `"1h"`。不写时用 [`PromptCache::default_for`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<PromptCache>,
}

/// 插件提供的服务商的适配器 ID（[`ProviderConfig::plugin`]）。
pub const PLUGIN_ADAPTER: &str = "plugin";

impl ProviderConfig {
    /// 使用某个内置适配器、全部取默认值。
    pub fn from_adapter(adapter: &Adapter) -> Self {
        Self {
            id: adapter.id.into(),
            adapter: adapter.id.into(),
            name: String::new(),
            protocol: None,
            base_url: String::new(),
            models: Vec::new(),
            plugin: None,
            prompt_cache: None,
        }
    }

    pub fn resolve(&self) -> ResolvedProvider {
        let adapter = adapter(&self.adapter);
        let protocol = adapter.map(|adapter| adapter.protocol).or(self.protocol).unwrap_or(Protocol::OpenAi);
        let base_url = if !self.base_url.trim().is_empty() {
            self.base_url.trim().trim_end_matches('/').to_owned()
        } else {
            adapter.map(|adapter| adapter.base_url.to_owned()).unwrap_or_default()
        };
        let models_url = match adapter.and_then(|adapter| adapter.models_url) {
            // 改了接口地址时，适配器自带的模型列表地址不再适用，改为按协议推导。
            Some((url, auth)) if self.base_url.trim().is_empty() => (url.to_owned(), auth),
            _ => match protocol {
                Protocol::Messages => (format!("{base_url}/v1/models"), Protocol::Messages),
                Protocol::OpenAi => (format!("{base_url}/models"), Protocol::OpenAi),
            },
        };
        let default_models: Vec<ModelEntry> = adapter
            .map(|adapter| adapter.default_models.iter().map(|(id, name)| ModelEntry::new(*id, *name)).collect())
            .unwrap_or_default();
        ResolvedProvider {
            id: self.id.clone(),
            name: match adapter {
                Some(adapter) => adapter.name.to_owned(),
                None if !self.name.is_empty() => self.name.clone(),
                None => self.id.clone(),
            },
            protocol,
            base_url,
            env_key: adapter.and_then(|adapter| adapter.env_key),
            requires_key: self.adapter != PLUGIN_ADAPTER
                && (adapter.is_none_or(|adapter| adapter.env_key.is_some()) || self.id == DEEPSEEK_OAUTH),
            models_url,
            using_default_models: self.models.is_empty(),
            models: if self.models.is_empty() { default_models } else { self.models.clone() },
            plugin: (self.adapter == PLUGIN_ADAPTER).then(|| self.plugin.clone()).flatten(),
            extra_headers: Vec::new(),
            prompt_cache: match protocol {
                Protocol::Messages => self.prompt_cache.unwrap_or_else(|| PromptCache::default_for(&self.adapter)),
                Protocol::OpenAi => PromptCache::Off,
            },
        }
    }
}

/// 叠加适配器默认值之后、运行时使用的提供商。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub env_key: Option<&'static str>,
    pub requires_key: bool,
    /// (地址, 鉴权方式)。
    pub models_url: (String, Protocol),
    pub using_default_models: bool,
    pub models: Vec<ModelEntry>,
    /// 插件提供的服务商（`<插件 id>/<服务商 id>`）：连接时向插件要地址和鉴权。
    pub plugin: Option<String>,
    /// 每个请求都带上的额外请求头（插件服务商的鉴权）。
    pub extra_headers: Vec<(String, String)>,
    /// 生效的提示词缓存设置；OpenAI 协议总是 `Off`。
    pub prompt_cache: PromptCache,
}

/// 对某个提供商下某个模型的引用，序列化为 `provider/model`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl ModelRef {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self { provider: provider.into(), model: model.into() }
    }

    /// 模型 ID 里可能带 `/`（如 OpenRouter），所以只按第一个 `/` 切分。
    pub fn parse(text: &str) -> Option<Self> {
        let (provider, model) = text.split_once('/')?;
        (!provider.is_empty() && !model.is_empty()).then(|| Self::new(provider, model))
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

/// 自定义提供商名称 → ID：小写字母数字与 `-`，并避开已有 ID。
pub fn custom_provider_id(name: &str, existing: &[ProviderConfig]) -> String {
    let mut slug: String = name
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        slug = "custom".into();
    }
    let taken = |id: &str| existing.iter().any(|provider| provider.id == id) || adapter(id).is_some();
    if !taken(&slug) {
        return slug;
    }
    (2..).map(|n| format!("{slug}-{n}")).find(|id| !taken(id)).expect("总能找到空闲 ID")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_defaults_and_overrides() {
        let deepseek = ProviderConfig::from_adapter(adapter("deepseek").unwrap()).resolve();
        assert_eq!(deepseek.protocol, Protocol::Messages);
        assert_eq!(deepseek.models_url.0, "https://api.deepseek.com/models");
        assert!(deepseek.using_default_models);
        assert_eq!(deepseek.models[0].display_name(), "DeepSeek-V4.1-Flash");

        let custom = ProviderConfig {
            base_url: "https://proxy.example.com/v1/".into(),
            models: vec![ModelEntry::new("m1", "")],
            ..ProviderConfig::from_adapter(adapter("openai").unwrap())
        }
        .resolve();
        assert_eq!(custom.base_url, "https://proxy.example.com/v1");
        assert_eq!(custom.models_url.0, "https://proxy.example.com/v1/models");
        assert_eq!(custom.models[0].display_name(), "m1");
        assert!(!custom.using_default_models);

        let ollama = ProviderConfig::from_adapter(adapter("ollama").unwrap()).resolve();
        assert!(!ollama.requires_key);
    }

    #[test]
    fn prompt_cache_defaults_and_overrides() {
        let resolve = |adapter_id: &str, cache: Option<PromptCache>| {
            ProviderConfig { prompt_cache: cache, ..ProviderConfig::from_adapter(adapter(adapter_id).unwrap()) }.resolve().prompt_cache
        };
        assert_eq!(resolve("anthropic", None), PromptCache::FiveMinutes);
        assert_eq!(resolve("anthropic", Some(PromptCache::OneHour)), PromptCache::OneHour);
        assert_eq!(resolve("anthropic", Some(PromptCache::Off)), PromptCache::Off);
        assert_eq!(resolve("deepseek", None), PromptCache::Off);
        // OpenAI 协议不认 cache_control，设了也不生效。
        assert_eq!(resolve("openrouter", Some(PromptCache::OneHour)), PromptCache::Off);

        let custom: ProviderConfig = toml::from_str(
            "id = \"gw\"\nadapter = \"custom\"\nprotocol = \"messages\"\nbase_url = \"https://gw\"\nprompt_cache = \"1h\"\n",
        )
        .unwrap();
        assert_eq!(custom.prompt_cache, Some(PromptCache::OneHour));
        assert_eq!(custom.resolve().prompt_cache, PromptCache::OneHour);
        assert!(toml::to_string(&custom).unwrap().contains("prompt_cache = \"1h\""));
    }

    #[test]
    fn model_ref_parsing() {
        assert_eq!(ModelRef::parse("openrouter/anthropic/claude"), Some(ModelRef::new("openrouter", "anthropic/claude")));
        assert_eq!(ModelRef::parse("nope"), None);
        assert_eq!(ModelRef::new("deepseek", "deepseek-flash").to_string(), "deepseek/deepseek-flash");
    }

    #[test]
    fn custom_ids_are_unique_slugs() {
        let existing = vec![ProviderConfig {
            id: "my-proxy".into(),
            adapter: CUSTOM_ADAPTER.into(),
            name: "My Proxy".into(),
            protocol: Some(Protocol::OpenAi),
            base_url: "https://x".into(),
            models: vec![],
            plugin: None,
            prompt_cache: None,
        }];
        assert_eq!(custom_provider_id("My Proxy", &existing), "my-proxy-2");
        assert_eq!(custom_provider_id("公司网关", &existing), "custom");
        assert_eq!(custom_provider_id("OpenAI", &existing), "openai-2");
    }
}
