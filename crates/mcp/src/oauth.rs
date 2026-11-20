//! 远程 MCP 服务器的登录（MCP 2025-06-18 的授权规范）：
//!
//! 1. 不带 token 连服务器，得到 401 和 `WWW-Authenticate: Bearer resource_metadata="…"`；
//! 2. 取受保护资源的元数据（RFC 9728）→ 授权服务器；取授权服务器的元数据（RFC 8414，退一步用 OpenID 的）；
//! 3. 配置里没给 `oauth.client_id` 时动态注册客户端（RFC 7591）；
//! 4. 授权码 + PKCE（`dshr-auth`），请求带 `resource`（RFC 8707）；token 存进系统钥匙串（条目 `mcp:<服务器>:oauth`），
//!    之后每次连接带上 `Authorization: Bearer …`，快过期时先刷新。
//!
//! 登录只在用户点“登录”（设置页、`dshr mcp login`）时进行：agent 循环里不弹浏览器。

use serde_json::{Value, json};

use crate::config::ServerConfig;

/// 钥匙串里的条目名。`server` 是跨组唯一的名字（插件自带的是 `<插件>/<名字>`）。
fn account(server: &str) -> String {
    format!("mcp:{server}:oauth")
}

#[cfg(test)]
static MEMORY: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// 存着的 token。
pub fn load_token(server: &str) -> Option<dshr_auth::Token> {
    #[cfg(test)]
    {
        let memory = MEMORY.lock().unwrap();
        return memory.iter().find(|(key, _)| *key == account(server)).and_then(|(_, value)| serde_json::from_str(value).ok());
    }
    #[allow(unreachable_code)]
    dshr_config::credentials::load_secret(&account(server)).ok().flatten().and_then(|text| serde_json::from_str(&text).ok())
}

pub fn save_token(server: &str, token: &dshr_auth::Token) -> Result<(), String> {
    let text = serde_json::to_string(token).map_err(|error| error.to_string())?;
    #[cfg(test)]
    {
        let mut memory = MEMORY.lock().unwrap();
        memory.retain(|(key, _)| *key != account(server));
        memory.push((account(server), text));
        return Ok(());
    }
    #[allow(unreachable_code)]
    dshr_config::credentials::save_secret(&account(server), &text).map_err(|error| format!("存不进系统钥匙串：{error}"))
}

/// 退出登录：删掉存着的 token。
pub fn forget_token(server: &str) {
    #[cfg(test)]
    {
        MEMORY.lock().unwrap().retain(|(key, _)| *key != account(server));
        return;
    }
    #[allow(unreachable_code)]
    let _ = dshr_config::credentials::delete_secret(&account(server));
}

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder().user_agent("dshr").build().map_err(|error| error.to_string())
}

/// 连接前：有存着的 token 就带上（快过期时先刷新、存回去）。刷新失败时不带，连接会得到 401，状态变成“需要登录”。
pub(crate) async fn bearer(server: &str) -> Option<String> {
    let token = load_token(server)?;
    if !token.needs_refresh() {
        return (!token.expired()).then_some(token.access_token);
    }
    let client = http().ok()?;
    match dshr_auth::refresh(&client, &token).await {
        Ok(renewed) => {
            let _ = save_token(server, &renewed);
            Some(renewed.access_token)
        }
        Err(error) => {
            tracing::warn!(server, error, "MCP 服务器的 token 刷新失败，要重新登录");
            None
        }
    }
}

/// 授权服务器的元数据里用得到的部分。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AuthServer {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes: Vec<String>,
    /// 要访问的资源（RFC 8707 的 `resource`）。
    pub resource: String,
}

/// `WWW-Authenticate` 里的某个参数（`resource_metadata="…"`）。
fn challenge_param(challenge: &str, name: &str) -> Option<String> {
    let start = challenge.find(&format!("{name}="))? + name.len() + 1;
    let rest = &challenge[start..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => &quoted[..quoted.find('"')?],
        None => rest.split([',', ' ']).next()?,
    };
    Some(value.to_owned())
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<Value, String> {
    let response = client.get(url).header("accept", "application/json").send().await.map_err(|error| format!("{url}：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("{url}：HTTP {}", response.status().as_u16()));
    }
    response.json().await.map_err(|error| format!("{url} 返回的不是 JSON：{error}"))
}

/// `https://host/a/b` → `https://host/.well-known/<name>/a/b`（路径为空时不加斜杠）。
fn well_known(url: &reqwest::Url, name: &str) -> String {
    let path = url.path().trim_end_matches('/');
    format!(
        "{}://{}/.well-known/{name}{path}",
        url.scheme(),
        url.host_str()
            .map(|host| match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            })
            .unwrap_or_default()
    )
}

/// 找授权服务器：先看受保护资源的元数据，再看授权服务器的元数据。
pub(crate) async fn discover(client: &reqwest::Client, mcp_url: &str, challenge: Option<&str>) -> Result<AuthServer, String> {
    let url = reqwest::Url::parse(mcp_url).map_err(|error| error.to_string())?;
    let mut candidates: Vec<String> = challenge.and_then(|challenge| challenge_param(challenge, "resource_metadata")).into_iter().collect();
    candidates.push(well_known(&url, "oauth-protected-resource"));
    let origin = format!("{}://{}", url.scheme(), url.authority());
    candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));
    let mut resource = mcp_url.to_owned();
    let mut issuer = origin.clone();
    let mut scopes = Vec::new();
    for candidate in candidates {
        if let Ok(metadata) = get_json(client, &candidate).await {
            if let Some(found) = metadata["authorization_servers"].as_array().and_then(|servers| servers.first()).and_then(Value::as_str) {
                issuer = found.trim_end_matches('/').to_owned();
            }
            if let Some(found) = metadata["resource"].as_str() {
                resource = found.to_owned();
            }
            scopes = metadata["scopes_supported"]
                .as_array()
                .map(|list| list.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            break;
        }
    }
    let issuer_url = reqwest::Url::parse(&issuer).map_err(|error| format!("授权服务器地址不对：{error}"))?;
    let candidates = [
        well_known(&issuer_url, "oauth-authorization-server"),
        well_known(&issuer_url, "openid-configuration"),
        format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/')),
    ];
    let mut problems = Vec::new();
    for candidate in candidates {
        match get_json(client, &candidate).await {
            Ok(metadata) => {
                let endpoint = |key: &str| metadata[key].as_str().map(str::to_owned);
                let (Some(authorization_endpoint), Some(token_endpoint)) = (endpoint("authorization_endpoint"), endpoint("token_endpoint"))
                else {
                    problems.push(format!("{candidate} 里没有授权或 token 接口"));
                    continue;
                };
                if scopes.is_empty() {
                    scopes = metadata["scopes_supported"]
                        .as_array()
                        .map(|list| list.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                        .unwrap_or_default();
                }
                return Ok(AuthServer {
                    authorization_endpoint,
                    token_endpoint,
                    registration_endpoint: endpoint("registration_endpoint"),
                    scopes,
                    resource,
                });
            }
            Err(error) => problems.push(error),
        }
    }
    Err(format!("找不到授权服务器的元数据（{}）", problems.join("；")))
}

/// 动态注册一个公开客户端（不带 secret，靠 PKCE）。
pub(crate) async fn register(client: &reqwest::Client, endpoint: &str, redirect_uri: &str) -> Result<(String, Option<String>), String> {
    let request = json!({
        "client_name": "dshr",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let response = client.post(endpoint).json(&request).send().await.map_err(|error| format!("注册客户端失败：{error}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|error| format!("注册客户端的响应不是 JSON：{error}"))?;
    if !status.is_success() {
        return Err(format!("注册客户端失败：HTTP {} {body}", status.as_u16()));
    }
    let client_id = body["client_id"].as_str().ok_or("注册客户端的响应里没有 client_id")?.to_owned();
    Ok((client_id, body["client_secret"].as_str().map(str::to_owned)))
}

/// 不带 token 发一次 `initialize`，看服务器要不要登录：要的话返回 `WWW-Authenticate`（可能是空的）。
async fn probe(client: &reqwest::Client, url: &str, headers: &[(String, String)]) -> Result<Option<String>, String> {
    let mut request = client
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": crate::client::PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "dshr", "version": env!("CARGO_PKG_VERSION")}}}).to_string());
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("authorization") {
            request = request.header(name.as_str(), value.as_str());
        }
    }
    let response = request.send().await.map_err(|error| format!("连不上 {url}：{error}"))?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        let challenge = response.headers().get("www-authenticate").and_then(|value| value.to_str().ok()).unwrap_or_default().to_owned();
        return Ok(Some(challenge));
    }
    Ok(None)
}

/// 登录一个 HTTP 服务器：发现授权服务器、（需要时）注册客户端、打开浏览器授权、存 token。
pub(crate) async fn login(
    server: &str,
    config: &ServerConfig,
    url: &str,
    headers: &[(String, String)],
    open: &(dyn Fn(&str) -> Result<(), String> + Send + Sync),
) -> Result<(), String> {
    let client = http()?;
    let challenge = probe(&client, url, headers).await?;
    let Some(challenge) = challenge else {
        return Err(format!("{server} 不需要登录"));
    };
    let auth = discover(&client, url, Some(&challenge)).await?;
    let callback = dshr_auth::Callback::bind().await?;
    let preset = config.oauth.as_ref();
    let (client_id, client_secret) = match preset.and_then(|oauth| oauth.client_id.clone()) {
        Some(client_id) => (client_id, preset.and_then(|oauth| oauth.client_secret.clone())),
        None => {
            let endpoint = auth.registration_endpoint.as_deref().ok_or("授权服务器不支持动态注册客户端：在配置里写 oauth.client_id")?;
            register(&client, endpoint, &callback.redirect_uri).await?
        }
    };
    let scopes = preset.map(|oauth| oauth.scopes.clone()).filter(|scopes| !scopes.is_empty()).unwrap_or(auth.scopes.clone());
    let request = dshr_auth::Authorize {
        authorize_url: auth.authorization_endpoint,
        token_url: auth.token_endpoint,
        client_id,
        client_secret,
        scopes,
        extra_params: Vec::new(),
        resource: Some(auth.resource),
    };
    let token = dshr_auth::authorize(&client, request, Some(callback), open).await?;
    save_token(server, &token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_challenges_and_builds_well_known_urls() {
        let challenge = r#"Bearer error="invalid_token", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource""#;
        assert_eq!(
            challenge_param(challenge, "resource_metadata").as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(challenge_param("Bearer realm=x, scope=a", "scope").as_deref(), Some("a"));
        let url = reqwest::Url::parse("https://as.example.com/tenant1").unwrap();
        assert_eq!(well_known(&url, "oauth-authorization-server"), "https://as.example.com/.well-known/oauth-authorization-server/tenant1");
        let url = reqwest::Url::parse("http://127.0.0.1:8080/").unwrap();
        assert_eq!(well_known(&url, "oauth-authorization-server"), "http://127.0.0.1:8080/.well-known/oauth-authorization-server");
    }
}
