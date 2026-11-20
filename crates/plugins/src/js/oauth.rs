//! `host.oauth`（要 `oauth`）：用系统浏览器登录第三方账号（授权码 + PKCE），token 存进插件密钥。
//!
//! 流程：在 `127.0.0.1` 的随机端口起一个只收一次请求的回调监听 → 用浏览器打开授权页（地址里带回调地址、
//! `state` 和 PKCE 的 challenge）→ 收到回调、核对 `state` → 用 code 向 token 接口换 token（token 接口的主机
//! 必须在 `net:` 里）→ 把 `{ access_token, refresh_token, expires_at, token_url, client_id }` 存成清单里声明的
//! 一个密钥。之后 `host.oauth.token(name)` 取 access token，快过期时自动用 refresh token 换新的。
//! 授权页由用户在浏览器里看，插件碰不到用户的密码。

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use super::host::State;

impl State {
    fn require_oauth(&self) -> Result<(), String> {
        if self.capabilities.oauth { Ok(()) } else { Err("this plugin does not declare the `oauth` capability in plugin.toml".into()) }
    }

    /// 密钥名要在清单里声明过。
    fn oauth_secret_name(&self, name: &str) -> Result<(), String> {
        self.secret_value(name).map(|_| ())
    }

    /// `{ authorizeUrl, tokenUrl, clientId, clientSecret?, scopes?, extraParams?, secret }`：登录，token 存进 `secret`。
    pub(super) fn oauth_authorize(&self, request: &str) -> Result<u64, String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Request {
            authorize_url: String,
            token_url: String,
            client_id: String,
            #[serde(default)]
            client_secret: Option<String>,
            #[serde(default)]
            scopes: Vec<String>,
            #[serde(default)]
            extra_params: std::collections::BTreeMap<String, String>,
            secret: String,
        }
        self.require_oauth()?;
        self.with_permits(|permits| permits.net || permits.act, "logging in is only available in tools, page actions and commands")?;
        let request: Request = serde_json::from_str(request).map_err(|error| format!("invalid oauth.authorize request: {error}"))?;
        self.oauth_secret_name(&request.secret)?;
        let authorize = reqwest::Url::parse(&request.authorize_url).map_err(|error| format!("invalid authorizeUrl: {error}"))?;
        if authorize.scheme() != "https" && authorize.host_str() != Some("127.0.0.1") && authorize.host_str() != Some("localhost") {
            return Err("the authorize URL must use https".into());
        }
        let token_url = reqwest::Url::parse(&request.token_url).map_err(|error| format!("invalid tokenUrl: {error}"))?;
        super::net::permitted(&self.capabilities, &token_url)?;
        let opener = crate::app::app_services().open_url;
        let (plugin, client) = (self.plugin, self.net()?.client());
        // 实时输出的回调不是 Sync：包一层锁，登录流程要跨 await 拿着它。
        let output = std::sync::Mutex::new(self.output_sink());
        let work = async move {
            let open = move |url: &str| -> Result<(), String> {
                (output.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))(&format!("Open this address to sign in: {url}\n"));
                match &opener {
                    Some(open) => open(url),
                    None => {
                        tracing::info!(plugin, url, "插件请求登录：在浏览器里打开这个地址");
                        Ok(())
                    }
                }
            };
            let login = dshr_auth::Authorize {
                authorize_url: request.authorize_url,
                token_url: request.token_url,
                client_id: request.client_id,
                client_secret: request.client_secret,
                scopes: request.scopes,
                extra_params: request.extra_params.into_iter().collect(),
                resource: None,
            };
            let stored = dshr_auth::authorize(&client, login, None, &open).await?;
            crate::secrets::save(plugin, &request.secret, &serde_json::to_string(&stored).map_err(|error| error.to_string())?)?;
            Ok(json!({ "expiresAt": stored.expires_at }).to_string())
        };
        self.ops.start(work, dshr_auth::LOGIN_WAIT + Duration::from_secs(30))
    }

    /// 取 access token；快过期时先用 refresh token 换新的。没登录过时是 `null`。
    pub(super) fn oauth_token(&self, name: &str) -> Result<u64, String> {
        self.require_oauth()?;
        let Some(raw) = self.secret_value(name)? else {
            return self.ops.start(async { Ok("null".into()) }, self.remaining());
        };
        let stored: dshr_auth::Token = serde_json::from_str(&raw).map_err(|_| format!("the secret {name} does not hold an oauth token"))?;
        if !stored.needs_refresh() {
            let token = stored.access_token.clone();
            return self.ops.start(async move { Ok(json!(token).to_string()) }, self.remaining());
        }
        let token_url = reqwest::Url::parse(&stored.token_url).map_err(|error| error.to_string())?;
        super::net::permitted(&self.capabilities, &token_url)?;
        let (plugin, name, client) = (self.plugin, name.to_owned(), self.net()?.client());
        let work = async move {
            let renewed = dshr_auth::refresh(&client, &stored).await?;
            crate::secrets::save(plugin, &name, &serde_json::to_string(&renewed).map_err(|error| error.to_string())?)?;
            Ok(json!(renewed.access_token).to_string())
        };
        self.ops.start(work, self.remaining())
    }
}
