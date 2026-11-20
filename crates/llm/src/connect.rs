//! 按提供商配置创建客户端，以及“获取可用模型”。两种内置协议各是一个插件。

use std::sync::Arc;

use dshr_config::{ModelEntry, Protocol, ResolvedProvider};
use dshr_context::{Context, ContextError, ExtensionPoint, Manifest, Plugin, PluginGroup, Scope, Snapshot};
use serde_json::Value;

use crate::messages::{API_VERSION, MessagesProvider};
use crate::openai::OpenAiProvider;
use crate::types::{ChatRequest, EventStream, LlmError, Provider};

/// 扩展点：线上协议的实现。提供商配置里的 [`Protocol`] 决定用哪一个；同一协议后注册的生效。
pub struct Providers;

impl ExtensionPoint for Providers {
    type Item = Arc<dyn ProviderFactory>;
}

#[async_trait::async_trait]
pub trait ProviderFactory: Send + Sync {
    fn protocol(&self) -> Protocol;

    fn connect(&self, client: &HttpClient, provider: &ResolvedProvider, api_key: Option<String>) -> Arc<dyn Provider>;

    /// 用这种协议的鉴权方式请求模型列表接口，返回模型（按 ID 字母序）；接口给了显示名时一并带上。
    async fn list_models(&self, client: &HttpClient, url: &str, api_key: Option<&str>) -> Result<Vec<ModelEntry>, LlmError>;
}

/// 可以在多个 Provider 之间共用的 HTTP 客户端。克隆只增加引用计数，连接池是同一个：
/// 长时间运行的程序（桌面版）持有一个，每轮对话就不必重新建连接、重新 TLS 握手。
///
/// 连接池里的连接挂在发起请求的 tokio 运行时上，所以一个 `HttpClient` 只在同一个运行时里使用。
#[derive(Clone)]
pub struct HttpClient(pub(crate) reqwest::Client);

/// 连接（含 TLS 握手）最多等多久。
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// 两次读到数据之间最多等多久。每读到一块数据就重新计时，所以不限制长回复本身的时长；
/// 服务端在思考、排队时也会定时发 `ping` / 注释行保活，远小于这个值。超时按网络错误处理，会自动重试。
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

impl Default for HttpClient {
    /// 带超时的客户端：网关半开连接、流卡住时不会一直停在“生成中”。
    fn default() -> Self {
        Self(http_client_builder().build().unwrap_or_default())
    }
}

fn http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT).read_timeout(READ_TIMEOUT).tcp_keepalive(std::time::Duration::from_secs(30))
}

/// 用 `plugins` 里对应协议的实现创建客户端。
pub fn connect(
    plugins: &Snapshot,
    client: &HttpClient,
    provider: &ResolvedProvider,
    api_key: Option<String>,
) -> Result<Arc<dyn Provider>, LlmError> {
    if let Some(id) = &provider.plugin {
        let auth = plugins
            .items::<ProviderAuths>()
            .iter()
            .find(|auth| auth.id() == id)
            .cloned()
            .ok_or_else(|| LlmError::Api(format!("提供服务商 {id} 的插件没有在运行")))?;
        let factory = factory(plugins, provider.protocol)?.clone();
        return Ok(Arc::new(PluginProvider { auth, factory, client: client.clone(), provider: provider.clone() }));
    }
    if provider.requires_key && api_key.as_deref().is_none_or(|key| key.trim().is_empty()) {
        return Err(LlmError::MissingApiKey);
    }
    Ok(factory(plugins, provider.protocol)?.connect(client, provider, api_key))
}

/// 插件服务商给出的连接信息：可以随时变（token 过期刷新），所以每个请求前问一次。
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    /// 覆盖配置里的地址。
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
}

/// 插件提供的服务商的鉴权（JS 插件的 `providers` 导出）。`id` 是 `<插件 id>/<服务商 id>`，对应配置里的 `plugin`。
#[async_trait::async_trait]
pub trait ProviderAuth: Send + Sync {
    fn id(&self) -> &str;
    /// 显示名、协议、默认地址：设置页“添加提供商”里列出来。
    fn describe(&self) -> PluginProviderInfo;
    async fn credentials(&self) -> Result<Credentials, LlmError>;
    /// 可用的模型 ID。
    async fn models(&self) -> Result<Vec<String>, LlmError>;
}

/// 插件服务商的基本信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginProviderInfo {
    /// `<插件 id>/<服务商 id>`。
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
}

/// 扩展点：插件提供的服务商。
pub struct ProviderAuths;

impl ExtensionPoint for ProviderAuths {
    type Item = Arc<dyn ProviderAuth>;
}

/// 插件服务商：每个请求前向插件要地址和鉴权，再用对应协议的内置客户端发请求。
struct PluginProvider {
    auth: Arc<dyn ProviderAuth>,
    factory: Arc<dyn ProviderFactory>,
    client: HttpClient,
    provider: ResolvedProvider,
}

#[async_trait::async_trait]
impl Provider for PluginProvider {
    fn id(&self) -> &str {
        &self.provider.id
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        let credentials = self.auth.credentials().await?;
        let mut provider = self.provider.clone();
        if let Some(base_url) = credentials.base_url {
            provider.base_url = base_url.trim_end_matches('/').to_owned();
        }
        provider.extra_headers = credentials.headers;
        self.factory.connect(&self.client, &provider, credentials.api_key).stream(request).await
    }
}

/// 插件服务商的模型列表。插件只给模型 ID，显示名留空。
pub async fn plugin_models(plugins: &Snapshot, id: &str) -> Result<Vec<ModelEntry>, LlmError> {
    let auth = plugins
        .items::<ProviderAuths>()
        .iter()
        .find(|auth| auth.id() == id)
        .cloned()
        .ok_or_else(|| LlmError::Api(format!("提供服务商 {id} 的插件没有在运行")))?;
    let mut models = auth.models().await?;
    models.sort();
    models.dedup();
    Ok(models.into_iter().map(|id| ModelEntry::new(id, "")).collect())
}

/// 调用提供商的模型列表接口，返回模型（按 ID 字母序）；接口给了显示名时一并带上。
pub async fn list_models(
    plugins: &Snapshot,
    client: &HttpClient,
    provider: &ResolvedProvider,
    api_key: Option<&str>,
) -> Result<Vec<ModelEntry>, LlmError> {
    if let Some(id) = &provider.plugin {
        return plugin_models(plugins, id).await;
    }
    let (url, auth) = &provider.models_url;
    let api_key = api_key.filter(|key| !key.trim().is_empty());
    factory(plugins, *auth)?.list_models(client, url, api_key).await
}

/// 使用账号令牌获取 DeepSeek 官方模型目录，不跟随重定向，避免把令牌转发到其他地址。
pub async fn list_deepseek_account_models(token: &str) -> Result<Vec<ModelEntry>, LlmError> {
    let client = http_client_builder().redirect(reqwest::redirect::Policy::none()).build()?;
    fetch_model_list(deepseek_account_models_request(&client, token)?).await
}

fn deepseek_account_models_request(client: &reqwest::Client, token: &str) -> Result<reqwest::RequestBuilder, LlmError> {
    use dshr_config::{account::ACCOUNT_TOKEN_HEADER, providers::DEEPSEEK_MODELS_URL};
    let mut token = reqwest::header::HeaderValue::from_str(token).map_err(|_| LlmError::Api("DeepSeek 账号令牌格式无效".into()))?;
    token.set_sensitive(true);
    Ok(client.get(DEEPSEEK_MODELS_URL).header(ACCOUNT_TOKEN_HEADER, token))
}

fn factory(plugins: &Snapshot, protocol: Protocol) -> Result<&Arc<dyn ProviderFactory>, LlmError> {
    plugins.items::<Providers>().iter().rev().find(|factory| factory.protocol() == protocol).ok_or(LlmError::UnsupportedProtocol(protocol))
}

struct MessagesFactory;

#[async_trait::async_trait]
impl ProviderFactory for MessagesFactory {
    fn protocol(&self) -> Protocol {
        Protocol::Messages
    }

    fn connect(&self, client: &HttpClient, provider: &ResolvedProvider, api_key: Option<String>) -> Arc<dyn Provider> {
        Arc::new(
            MessagesProvider::with_client(client.clone(), &provider.id, &provider.base_url, api_key)
                .with_headers(provider.extra_headers.clone())
                .with_prompt_cache(provider.prompt_cache),
        )
    }

    async fn list_models(&self, client: &HttpClient, url: &str, api_key: Option<&str>) -> Result<Vec<ModelEntry>, LlmError> {
        let separator = if url.contains('?') { '&' } else { '?' };
        let mut builder = client.0.get(format!("{url}{separator}limit=1000")).header("anthropic-version", API_VERSION);
        if let Some(key) = api_key {
            builder = builder.header("x-api-key", key);
        }
        fetch_model_list(builder).await
    }
}

struct OpenAiFactory;

#[async_trait::async_trait]
impl ProviderFactory for OpenAiFactory {
    fn protocol(&self) -> Protocol {
        Protocol::OpenAi
    }

    fn connect(&self, client: &HttpClient, provider: &ResolvedProvider, api_key: Option<String>) -> Arc<dyn Provider> {
        Arc::new(
            OpenAiProvider::with_client(client.clone(), &provider.id, &provider.base_url, api_key)
                .with_headers(provider.extra_headers.clone()),
        )
    }

    async fn list_models(&self, client: &HttpClient, url: &str, api_key: Option<&str>) -> Result<Vec<ModelEntry>, LlmError> {
        let mut builder = client.0.get(url);
        if let Some(key) = api_key {
            builder = builder.bearer_auth(key);
        }
        fetch_model_list(builder).await
    }
}

/// Anthropic Messages 兼容协议（DeepSeek、Anthropic）。
pub struct MessagesPlugin;

impl Plugin for MessagesPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("provider-messages")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<Providers>(Arc::new(MessagesFactory));
        Ok(())
    }
}

/// OpenAI Chat Completions 兼容协议（OpenAI、Kimi、OpenRouter、Ollama 等）。
pub struct OpenAiPlugin;

impl Plugin for OpenAiPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("provider-openai")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<Providers>(Arc::new(OpenAiFactory));
        Ok(())
    }
}

/// 全部内置协议。
pub struct BuiltinProviders;

impl PluginGroup for BuiltinProviders {
    fn add_to(self, cx: &Context) -> Result<(), ContextError> {
        cx.add(MessagesPlugin)?;
        cx.add(OpenAiPlugin)
    }
}

async fn fetch_model_list(builder: reqwest::RequestBuilder) -> Result<Vec<ModelEntry>, LlmError> {
    let response = builder.send().await?;
    if !response.status().is_success() {
        return Err(LlmError::from_response(response).await);
    }
    let body = response.text().await?;
    parse_model_list(&body)
}

/// 读模型列表接口的应答。认这几种写法：
/// - OpenAI：`{"data": [{"id": …}]}`，没有显示名；
/// - Anthropic：`{"data": [{"id": …, "display_name": "Claude …"}]}`；
/// - OpenRouter 等：`{"data": [{"id": …, "name": …}]}`；
/// - Ollama 原生接口：`{"models": [{"name": …}]}`（只有名字，当作 ID）。
///
/// 很多兼容网关（中转、代理）即使转发的是 Claude，也按 OpenAI 的写法只给 ID。没有显示名时由界面用
/// [`readable_model_name`] 从 ID 推一个。
fn parse_model_list(body: &str) -> Result<Vec<ModelEntry>, LlmError> {
    let value: Value = serde_json::from_str(body).map_err(|error| LlmError::Protocol(error.to_string()))?;
    let items = value["data"]
        .as_array()
        .or_else(|| value["models"].as_array())
        .ok_or_else(|| LlmError::Protocol(format!("模型列表格式无法识别: {body}")))?;
    let text = |item: &Value, key: &str| item[key].as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned);
    let mut models: Vec<ModelEntry> = items
        .iter()
        .filter_map(|item| {
            let (id, name) = match text(item, "id") {
                Some(id) => (id, ["display_name", "displayName", "name", "label"].into_iter().find_map(|key| text(item, key))),
                None => (text(item, "name")?, None),
            };
            // 显示名和 ID 一样时不必再写一遍。
            let name = name.filter(|name| *name != id).unwrap_or_default();
            Some(ModelEntry::new(id, name))
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}

/// 接口没给显示名时，从模型 ID 推一个好读的名字：`claude-opus-4-5-20251101` → `Claude Opus 4.5`，
/// `gpt-4o-mini` → `GPT-4o Mini`，`o3-mini` → `o3 Mini`。末尾的日期去掉，相邻的纯数字段合成版本号。
/// 只处理由小写字母、数字、`-`、`.`、`_` 组成的 ID；别的写法（`vendor/model`、`qwen3:8b`）推不准，返回 `None`。
pub fn readable_model_name(id: &str) -> Option<String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_')) {
        return None;
    }
    let mut parts: Vec<&str> = id.split(['-', '_']).filter(|part| !part.is_empty()).collect();
    // 末尾的日期：`20251001`，或者 `2024-08-06` 三段。
    let digits = |part: &str, len: usize| part.len() == len && part.chars().all(|c| c.is_ascii_digit());
    if parts.last().is_some_and(|last| digits(last, 8)) {
        parts.pop();
    } else if parts.len() >= 3
        && digits(parts[parts.len() - 3], 4)
        && digits(parts[parts.len() - 2], 2)
        && digits(parts[parts.len() - 1], 2)
    {
        parts.truncate(parts.len() - 3);
    }
    if parts.is_empty() {
        return None;
    }
    let numeric = |part: &str| part.chars().all(|c| c.is_ascii_digit() || c == '.');
    let mut words: Vec<String> = Vec::new();
    let mut index = 0;
    while index < parts.len() {
        let part = parts[index];
        if numeric(part) {
            // 相邻的数字段合成版本号：`4`、`5` → `4.5`。
            let mut version = vec![part];
            while index + 1 < parts.len() && numeric(parts[index + 1]) && parts[index + 1].len() <= 2 {
                index += 1;
                version.push(parts[index]);
            }
            words.push(version.join("."));
        } else {
            words.push(match part {
                "gpt" => "GPT".to_owned(),
                "deepseek" => "DeepSeek".to_owned(),
                "glm" => "GLM".to_owned(),
                "llm" => "LLM".to_owned(),
                // OpenAI 的推理模型（o1、o3、o4）保持小写。
                _ if part.len() <= 3 && part.starts_with('o') && part[1..].chars().all(|c| c.is_ascii_digit()) => part.to_owned(),
                _ => {
                    let mut chars = part.chars();
                    let first = chars.next().map(|c| c.to_ascii_uppercase()).into_iter();
                    first.chain(chars).collect()
                }
            });
        }
        index += 1;
    }
    // `GPT` 和紧跟的版本连写：`GPT-4o`、`GPT-5`。
    if words.first().is_some_and(|word| word == "GPT") && words.len() > 1 {
        let version = words.remove(1);
        words[0] = format!("GPT-{version}");
    }
    let name = words.join(" ");
    (name != id).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dshr_config::ProviderConfig;
    use dshr_config::providers::adapter;

    #[test]
    fn deepseek_account_model_list_uses_native_token_header() {
        let client = HttpClient::default();
        let request = deepseek_account_models_request(&client.0, "test-account-token").unwrap().build().unwrap();
        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(request.url().as_str(), "https://api.deepseek.com/models");
        let token = &request.headers()["x-dsh-auth-token"];
        assert_eq!(token, "test-account-token");
        assert!(token.is_sensitive());
        assert!(!request.headers().contains_key(reqwest::header::AUTHORIZATION));
        assert!(!request.headers().contains_key("x-api-key"));
    }

    #[test]
    fn parses_openai_and_anthropic_lists() {
        let openai = r#"{"object":"list","data":[{"id":"deepseek-v4-pro","object":"model"},{"id":"deepseek-flash","object":"model"}]}"#;
        assert_eq!(parse_model_list(openai).unwrap(), [ModelEntry::new("deepseek-flash", ""), ModelEntry::new("deepseek-v4-pro", "")]);
        let anthropic = r#"{"data":[{"type":"model","id":"claude-x","display_name":"Claude X"}],"has_more":false}"#;
        assert_eq!(parse_model_list(anthropic).unwrap(), [ModelEntry::new("claude-x", "Claude X")]);
        let openrouter = r#"{"data":[{"id":"vendor/m","name":"Vendor: M"},{"id":"same","name":"same"}]}"#;
        assert_eq!(parse_model_list(openrouter).unwrap(), [ModelEntry::new("same", ""), ModelEntry::new("vendor/m", "Vendor: M")]);
        let camel = r#"{"data":[{"id":"m","displayName":"Model M"}]}"#;
        assert_eq!(parse_model_list(camel).unwrap(), [ModelEntry::new("m", "Model M")]);
        let ollama = r#"{"models":[{"name":"qwen3:8b"}]}"#;
        assert_eq!(parse_model_list(ollama).unwrap(), [ModelEntry::new("qwen3:8b", "")]);
        assert!(parse_model_list("{}").is_err());
    }

    /// 端到端：按 Anthropic 文档里的真实应答形状（字段很多、带嵌套的 capabilities）起一个本地服务，
    /// 走 `list_models` 的完整路径（请求、鉴权头、解析），显示名要能拿到。
    #[tokio::test]
    async fn anthropic_model_list_keeps_display_names() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let body = r#"{"data":[{"id":"claude-opus-5-5","capabilities":{"batch":{"supported":true},"effort":{"high":{"supported":true},"supported":true}},"created_at":"2026-09-21T00:00:00Z","display_name":"Claude Opus 5.5","lifecycle":"active","line":"opus","max_input_tokens":0,"max_tokens":0,"type":"model"},{"id":"claude-haiku-4-5-20251001","created_at":"2025-10-01T00:00:00Z","display_name":"Claude Haiku 4.5","type":"model"}],"first_id":"claude-opus-5-5","has_more":false,"last_id":"claude-haiku-4-5-20251001"}"#;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 8192];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });
        let plugins = Context::new();
        plugins.add_group(BuiltinProviders).unwrap();
        let provider =
            ProviderConfig { base_url: format!("http://127.0.0.1:{port}"), ..ProviderConfig::from_adapter(adapter("anthropic").unwrap()) }
                .resolve();
        let models = list_models(&plugins.snapshot(), &HttpClient::default(), &provider, Some("sk-test")).await.unwrap();
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /v1/models?limit=1000 "), "{request}");
        assert!(request.contains("x-api-key: sk-test"), "{request}");
        assert_eq!(
            models,
            [ModelEntry::new("claude-haiku-4-5-20251001", "Claude Haiku 4.5"), ModelEntry::new("claude-opus-5-5", "Claude Opus 5.5")]
        );
    }

    #[test]
    fn readable_names_from_ids() {
        let name = |id| readable_model_name(id);
        assert_eq!(name("claude-opus-4-5-20251101").as_deref(), Some("Claude Opus 4.5"));
        assert_eq!(name("claude-haiku-4-5-20251001").as_deref(), Some("Claude Haiku 4.5"));
        assert_eq!(name("claude-sonnet-5-5").as_deref(), Some("Claude Sonnet 5.5"));
        assert_eq!(name("claude-opus-4-6").as_deref(), Some("Claude Opus 4.6"));
        assert_eq!(name("claude-fable-5").as_deref(), Some("Claude Fable 5"));
        assert_eq!(name("claude-3-5-sonnet-20241022").as_deref(), Some("Claude 3.5 Sonnet"));
        assert_eq!(name("gpt-4o-mini").as_deref(), Some("GPT-4o Mini"));
        assert_eq!(name("gpt-4o-2024-08-06").as_deref(), Some("GPT-4o"));
        assert_eq!(name("gpt-5").as_deref(), Some("GPT-5"));
        assert_eq!(name("gpt-4.1").as_deref(), Some("GPT-4.1"));
        assert_eq!(name("o3-mini").as_deref(), Some("o3 Mini"));
        assert_eq!(name("deepseek-v4-pro").as_deref(), Some("DeepSeek V4 Pro"));
        assert_eq!(name("kimi-k2-turbo-preview").as_deref(), Some("Kimi K2 Turbo Preview"));
        assert_eq!(name("anthropic/claude-x"), None);
        assert_eq!(name("qwen3:8b"), None);
        assert_eq!(name(""), None);
    }

    #[test]
    fn connect_requires_key_only_when_needed() {
        let plugins = Context::new();
        plugins.add_group(BuiltinProviders).unwrap();
        let (plugins, client) = (plugins.snapshot(), HttpClient::default());
        let deepseek = ProviderConfig::from_adapter(adapter("deepseek").unwrap()).resolve();
        assert!(matches!(connect(&plugins, &client, &deepseek, None), Err(LlmError::MissingApiKey)));
        assert_eq!(connect(&plugins, &client, &deepseek, Some("k".into())).unwrap().id(), "deepseek");

        let ollama = ProviderConfig::from_adapter(adapter("ollama").unwrap()).resolve();
        assert!(connect(&plugins, &client, &ollama, None).is_ok());
    }

    #[test]
    fn connect_needs_the_protocol_plugin() {
        let plugins = Context::new();
        plugins.add_group(BuiltinProviders).unwrap();
        plugins.disable("provider-openai").unwrap();
        let (plugins, client) = (plugins.snapshot(), HttpClient::default());
        let ollama = ProviderConfig::from_adapter(adapter("ollama").unwrap()).resolve();
        assert!(matches!(connect(&plugins, &client, &ollama, None), Err(LlmError::UnsupportedProtocol(Protocol::OpenAi))));
        let deepseek = ProviderConfig::from_adapter(adapter("deepseek").unwrap()).resolve();
        assert!(connect(&plugins, &client, &deepseek, Some("k".into())).is_ok());
    }
}
