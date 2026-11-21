//! OAuth 2.0 授权码流程 + PKCE（RFC 7636），给桌面程序用：
//!
//! 1. 在 `127.0.0.1` 的随机端口起一个只收回调的监听（RFC 8252 的回环地址重定向）；
//! 2. 用系统浏览器打开授权页（带回调地址、`state`、PKCE 的 challenge，以及可选的 `resource`（RFC 8707））；
//! 3. 收到回调、核对 `state`，用 code 加 verifier 向 token 接口换 token；
//! 4. 快过期时用 refresh token 换新的。
//!
//! 授权页由用户在浏览器里看，程序碰不到用户的密码。JS 插件的 `host.oauth` 和 MCP 远程服务器的登录共用这里。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// 等用户在浏览器里登录最多这么久。
pub const LOGIN_WAIT: Duration = Duration::from_secs(300);
/// 还剩这么久就过期时先刷新。
const REFRESH_MARGIN: u64 = 60;

/// 一份 token（存进钥匙串的样子）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix 秒；没有时当作不过期。
    #[serde(default)]
    pub expires_at: Option<u64>,
    pub token_url: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    /// 刷新时带上的 `resource`（RFC 8707）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

impl Token {
    /// 快过期了（而且能刷新）。
    pub fn needs_refresh(&self) -> bool {
        self.refresh_token.is_some() && self.expires_at.is_some_and(|expires| expires <= now() + REFRESH_MARGIN)
    }

    /// 已经过期（没法刷新时就只能重新登录）。
    pub fn expired(&self) -> bool {
        self.expires_at.is_some_and(|expires| expires <= now())
    }
}

/// 要发起的一次登录。
#[derive(Debug, Clone)]
pub struct Authorize {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    pub extra_params: Vec<(String, String)>,
    /// RFC 8707 的 `resource`：要访问的服务器（MCP 规范要求带上）。
    pub resource: Option<String>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// 32 字节随机数，URL 安全的 base64（PKCE verifier、`state`）。
///
/// 系统随机源不可用时返回错误：以前忽略错误，得到的是全零、可以预测的 verifier 和 `state`，
/// 等于没有 PKCE 和 CSRF 防护。
pub fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// 用系统随机源填满 `bytes`。
pub fn fill_random(bytes: &mut [u8]) -> Result<(), String> {
    getrandom::fill(bytes).map_err(|error| format!("the system random source is unavailable: {error}"))
}

/// PKCE：(verifier, S256 challenge)。
pub fn pkce() -> Result<(String, String), String> {
    let verifier = random_token()?;
    let challenge = pkce_challenge(&verifier);
    Ok((verifier, challenge))
}

/// verifier 对应的 S256 challenge。
pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::Digest as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()))
}

/// 本机回调：先绑好端口（动态注册客户端时要把回调地址报上去），再等授权页跳回来。
pub struct Callback {
    listener: tokio::net::TcpListener,
    pub redirect_uri: String,
}

impl Callback {
    pub async fn bind() -> Result<Self, String> {
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|error| format!("cannot listen for the login callback: {error}"))?;
        let port = listener.local_addr().map_err(|error| error.to_string())?.port();
        Ok(Self { listener, redirect_uri: format!("http://127.0.0.1:{port}/callback") })
    }

    /// 只认 `GET /callback?code=…&state=…`，`state` 对上才算。回一页“可以关掉了”。
    pub async fn wait_for_code(&self, state: &str) -> Result<String, String> {
        loop {
            let (mut stream, _) = self.listener.accept().await.map_err(|error| error.to_string())?;
            let head = read_head(&mut stream).await.map_err(|error| error.to_string())?;
            let target = head.lines().next().and_then(|line| line.split(' ').nth(1)).unwrap_or_default().to_owned();
            let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).ok();
            let param = |key: &str| {
                url.as_ref().and_then(|url| url.query_pairs().find(|(name, _)| name == key).map(|(_, value)| value.into_owned()))
            };
            if url.as_ref().is_none_or(|url| url.path() != "/callback") {
                let _ = reply(&mut stream, "").await;
                continue;
            }
            if let Some(error) = param("error") {
                let _ = reply(&mut stream, "<p>Sign-in failed. You can close this page.</p>").await;
                return Err(format!("the sign-in failed: {error}"));
            }
            if param("state").as_deref() != Some(state) {
                let _ = reply(&mut stream, "<p>This sign-in link is not valid.</p>").await;
                continue;
            }
            let Some(code) = param("code") else {
                let _ = reply(&mut stream, "<p>No code was returned.</p>").await;
                continue;
            };
            let _ = reply(&mut stream, "<p>Signed in. You can close this page and return to the app.</p>").await;
            return Ok(code);
        }
    }
}

/// 读请求头，直到空行（`\r\n\r\n`）、连接关闭或 8 KB 为止。浏览器的请求不一定一次到齐，
/// 只读一次可能只拿到 `GET `，把真正的回调当成别的请求丢掉，登录就一直等下去。
async fn read_head(stream: &mut tokio::net::TcpStream) -> std::io::Result<String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buffer.len() < 8192 && !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "the callback request did not finish"))??;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// 回一页简单的 HTML。
async fn reply(stream: &mut tokio::net::TcpStream, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await
}

/// 完整的登录：起回调（或者用调用方已经绑好的）、打开授权页、等回调、换 token。`open` 打开浏览器（或者把地址告诉用户）。
pub async fn authorize(
    client: &reqwest::Client,
    request: Authorize,
    callback: Option<Callback>,
    open: &(dyn Fn(&str) -> Result<(), String> + Send + Sync),
) -> Result<Token, String> {
    let callback = match callback {
        Some(callback) => callback,
        None => Callback::bind().await?,
    };
    let (verifier, challenge) = pkce()?;
    let state = random_token()?;
    let mut url = reqwest::Url::parse(&request.authorize_url).map_err(|error| format!("invalid authorize URL: {error}"))?;
    if url.scheme() != "https" && !matches!(url.host_str(), Some("127.0.0.1" | "localhost")) {
        return Err("the authorize URL must use https".into());
    }
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &request.client_id)
            .append_pair("redirect_uri", &callback.redirect_uri)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        if !request.scopes.is_empty() {
            query.append_pair("scope", &request.scopes.join(" "));
        }
        if let Some(resource) = &request.resource {
            query.append_pair("resource", resource);
        }
        for (key, value) in &request.extra_params {
            query.append_pair(key, value);
        }
    }
    open(url.as_str())?;
    let code = tokio::time::timeout(LOGIN_WAIT, callback.wait_for_code(&state))
        .await
        .map_err(|_| "the sign-in was not finished within 5 minutes".to_owned())??;
    let mut form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code),
        ("redirect_uri", callback.redirect_uri.clone()),
        ("client_id", request.client_id.clone()),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = &request.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    if let Some(resource) = &request.resource {
        form.push(("resource", resource.clone()));
    }
    let mut token = exchange(client, &request.token_url, &form, None).await?;
    token.client_id = request.client_id;
    token.client_secret = request.client_secret;
    token.resource = request.resource;
    Ok(token)
}

/// 用 refresh token 换新的 access token。
pub async fn refresh(client: &reqwest::Client, token: &Token) -> Result<Token, String> {
    let refresh_token = token.refresh_token.clone().ok_or("there is no refresh token; sign in again")?;
    let mut form =
        vec![("grant_type", "refresh_token".to_owned()), ("refresh_token", refresh_token), ("client_id", token.client_id.clone())];
    if let Some(secret) = &token.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    if let Some(resource) = &token.resource {
        form.push(("resource", resource.clone()));
    }
    let mut renewed = exchange(client, &token.token_url, &form, token.refresh_token.clone()).await?;
    renewed.client_id = token.client_id.clone();
    renewed.client_secret = token.client_secret.clone();
    renewed.resource = token.resource.clone();
    Ok(renewed)
}

/// 向 token 接口换 token（表单编码）。
pub async fn exchange(
    client: &reqwest::Client,
    token_url: &str,
    form: &[(&str, String)],
    previous_refresh: Option<String>,
) -> Result<Token, String> {
    let response = client
        .post(token_url)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(form))
        .send()
        .await
        .map_err(|error| format!("the token request failed: {error}"))?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|error| format!("the token response is not JSON: {error}"))?;
    if !status.is_success() {
        return Err(format!("the token request failed: HTTP {} {}", status.as_u16(), body));
    }
    let access_token = body.get("access_token").and_then(Value::as_str).ok_or("the token response has no access_token")?.to_owned();
    Ok(Token {
        access_token,
        refresh_token: body.get("refresh_token").and_then(Value::as_str).map(str::to_owned).or(previous_refresh),
        expires_at: body.get("expires_in").and_then(Value::as_u64).map(|seconds| now() + seconds),
        token_url: token_url.to_owned(),
        client_id: String::new(),
        client_secret: None,
        resource: None,
    })
}

/// `application/x-www-form-urlencoded` 的正文。
pub fn form_body(form: &[(&str, String)]) -> String {
    let mut url = reqwest::Url::parse("http://form.invalid/").expect("固定的地址");
    url.query_pairs_mut().extend_pairs(form.iter().map(|(key, value)| (*key, value.as_str())));
    url.query().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc_7636_and_tokens_are_random() {
        // 期望值用另一套实现（Python 的 hashlib + base64.urlsafe_b64encode，去掉 `=`）算出。
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K9uhvq1JzNqmvQazoq9bHdQK9M"), "M1PhMvhVIFGvFb1DSAW2Zbpj3bpJGnugy4EzzT1Dvzs");
        let (verifier, challenge) = pkce().unwrap();
        assert_eq!((verifier.len(), challenge), (43, pkce_challenge(&verifier)));
        assert_ne!(random_token().unwrap(), random_token().unwrap());
    }

    /// 回调请求分几次到达（中间还隔了一会儿）时也要认出来。
    #[tokio::test]
    async fn callback_waits_for_the_whole_request() {
        let callback = Callback::bind().await.unwrap();
        let address = callback.redirect_uri.trim_start_matches("http://").trim_end_matches("/callback").to_owned();
        let browser = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            for part in ["GET ", "/callback?code=abc&state=s1", " HTTP/1.1\r\nHost: x\r\n", "\r\n"] {
                stream.write_all(part.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let mut reply = String::new();
            stream.read_to_string(&mut reply).await.unwrap();
            reply
        });
        let code = tokio::time::timeout(Duration::from_secs(5), callback.wait_for_code("s1")).await.expect("waited forever");
        assert_eq!(code.unwrap(), "abc");
        assert!(browser.await.unwrap().contains("Signed in"));
    }
}
