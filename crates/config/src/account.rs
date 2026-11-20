//! DeepSeek 账号登录：浏览器授权 + PKCE，对齐原版的 `deepseek-account` 服务
//! （`packages/account/deepseek-account/src/index.ts`，Python 参考 `demo/deepseek_account_login.py`）。
//!
//! 流程：
//! 1. `POST {origin}/auth-api/v0/dsh/auth_init`：S256 PKCE challenge、随机 `state`、回环 `redirect_uri`，
//!    应答里带 `authorize_url`；
//! 2. 浏览器打开 `authorize_url`，用户批准后平台跳回 `http://127.0.0.1:<port>/oauth/callback?code=…&state=…`；
//! 3. `POST {origin}/auth-api/v0/dsh/auth_exchange` 用 code 与 verifier 换令牌；
//! 4. `GET {origin}/auth-api/v0/users/current`（`x-dsh-auth-token`，无 `Bearer` 前缀）读账号信息。
//!
//! 令牌以 JSON 存在系统钥匙串（条目 `deepseek-account-grant`），不是 API Key；推理请求把它放到
//! `x-dsh-auth-token` 头里，API 网关同样认得。

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::credentials;

/// 平台源：登录、令牌交换与账号信息都走它。
pub const PLATFORM_ORIGIN: &str = "https://platform.deepseek.com";
/// 平台允许令牌带去的推理源（原版 `inferenceOrigin`）。
pub const INFERENCE_ORIGIN: &str = "https://api.deepseek.com";

/// 授权回调路径，与原版一致。
const CALLBACK_PATH: &str = "/oauth/callback";
const AUTHORIZE_PATH: &str = "/dsh/authorize";
/// 单条平台响应的体积上限（与原版 `BODY_LIMIT` 相同）。
const BODY_LIMIT: u64 = 65_536;
/// 浏览器批准的等待上限（原版 `attemptTimeoutMs` 默认 10 分钟）。
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(600);
/// 单个平台 HTTP 请求的超时（原版 `requestTimeoutMs` 默认 30 秒）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 携带授权令牌时模型请求带上的请求头名。
pub const ACCOUNT_TOKEN_HEADER: &str = "x-dsh-auth-token";

/// 浏览器 UA：平台对通用 HTTP 库的 UA 回 429，这里与 Web 端保持一致。
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 钥匙串条目：账号授权（JSON：token + issuer）。
const GRANT_SECRET: &str = "deepseek-account-grant";

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// 网络不通、连接被重置等。
    #[error("网络请求失败: {0}")]
    Network(String),
    /// 平台的应答不符合预期（HTTP 错误码、非成功信封、字段缺失）。
    #[error("平台应答异常: {0}")]
    Protocol(String),
    /// 令牌被平台拒绝（HTTP 401 或信封 `code == 40003`）。
    #[error("登录已过期，请重新登录")]
    Expired,
    /// 规定时间内浏览器没有完成批准。
    #[error("浏览器登录超时，请重试")]
    TimedOut,
    /// 系统的随机源不可用（极少见：沙箱、精简过的容器）。
    #[error("系统随机源不可用: {0}")]
    Random(String),
}

/// 存在钥匙串里的账号授权。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Grant {
    pub token: String,
    /// 签发源：换了平台源（开发代理等）时旧授权不能用。
    pub issuer: String,
}

/// 登录成功后读到的账号信息与完成页。
#[derive(Debug)]
pub struct SignInOutcome {
    pub grant: Grant,
    /// `/users/current` 的原始应答（名称、头像、邮箱等）。
    pub user: serde_json::Value,
    /// 平台的“已授权”页面地址。
    pub authorized_url: String,
}

/// 已保存的账号令牌（换过平台源时视为没有）。
pub fn account_token() -> Option<String> {
    let raw = credentials::load_secret(GRANT_SECRET).ok().flatten()?;
    let grant: Grant = serde_json::from_str(&raw).ok()?;
    (grant.issuer == PLATFORM_ORIGIN && !grant.token.trim().is_empty()).then_some(grant.token)
}

pub fn save_grant(grant: &Grant) -> Result<(), String> {
    let raw = serde_json::to_string(grant).map_err(|error| error.to_string())?;
    credentials::save_secret(GRANT_SECRET, &raw).map_err(|error| error.to_string())
}

/// 退出登录：删除本地授权。平台侧的吊销（auth_revoke）这里不做；令牌本身会过期。
pub fn sign_out() -> Result<(), String> {
    credentials::delete_secret(GRANT_SECRET).map_err(|error| error.to_string())
}

/// 完整的浏览器登录。`open_browser` 拿到授权地址（打开系统浏览器）；期间一直阻塞，
/// 应放到后台线程里调用。
pub fn sign_in(open_browser: impl FnOnce(&str) + Send) -> Result<SignInOutcome, LoginError> {
    sign_in_from(PLATFORM_ORIGIN, open_browser)
}

fn sign_in_from(origin: &str, open_browser: impl FnOnce(&str) + Send) -> Result<SignInOutcome, LoginError> {
    // 和 MCP 远程服务器的 OAuth 登录共用同一套 PKCE 与随机数（`dshr-auth`）。
    let (verifier, challenge) = dshr_auth::pkce().map_err(LoginError::Random)?;
    let state = dshr_auth::random_token().map_err(LoginError::Random)?;
    let device_id = uuid_v4()?;

    // 回环回调服务器：平台把浏览器跳回这里带来授权码。先绑定好端口再发起 auth_init。
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| LoginError::Network(error.to_string()))?;
    listener.set_nonblocking(true).map_err(|error| LoginError::Network(error.to_string()))?;
    let port = listener.local_addr().map_err(|error| LoginError::Network(error.to_string()))?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");

    let init = platform_request(
        origin,
        "/auth-api/v0/dsh/auth_init",
        Some(&serde_json::json!({
            "code_challenge": challenge,
            "code_challenge_method": "S256",
            "state": state,
            "redirect_uri": redirect_uri,
            "locale": "zh_CN",
            "login_source": "desktop",
        })),
        None,
    )?;
    let authorize_url = browser_url(origin, init.get("authorize_url").and_then(serde_json::Value::as_str), AUTHORIZE_PATH)?;
    let authorize_id = init
        .get("authorize_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| LoginError::Protocol("缺少 authorize_id".into()))?
        .to_owned();
    let expires_in = init
        .get("expires_in")
        .and_then(serde_json::Value::as_f64)
        .filter(|seconds| *seconds > 0.0)
        .ok_or_else(|| LoginError::Protocol("缺少 expires_in".into()))?;
    // 平台给的有效期可能比本地上限短；取较小值。
    let deadline = Instant::now() + ATTEMPT_TIMEOUT.min(Duration::from_secs_f64(expires_in));

    open_browser(&authorize_url);

    // 回调和批准是同一条浏览器连接：另起线程等回调，主线程保持阻塞语义。
    let wait_state = state.clone();
    let (callback_tx, callback_rx) = mpsc::channel();
    std::thread::spawn(move || callback_tx.send(wait_for_code(&listener, &wait_state, deadline)));

    let code = match callback_rx.recv_timeout(ATTEMPT_TIMEOUT) {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => {
            cancel_quietly(origin, &authorize_id, &verifier);
            return Err(error);
        }
        Err(_) => {
            cancel_quietly(origin, &authorize_id, &verifier);
            return Err(LoginError::TimedOut);
        }
    };

    let result = platform_request(
        origin,
        "/auth-api/v0/dsh/auth_exchange",
        Some(&serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "redirect_uri": redirect_uri,
            "device_id": device_id,
            "device_model": format!("{}-{}", node_platform(), node_arch()),
            "os_version": format!("{} {}", node_platform(), os_release()),
        })),
        None,
    )
    .inspect_err(|_| cancel_quietly(origin, &authorize_id, &verifier))?;

    let token = result
        .get("token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| token.chars().all(|ch| matches!(ch, '!'..='~')))
        .map(str::to_owned)
        .ok_or_else(|| LoginError::Protocol("令牌无效".into()))?;
    let authorized_url = browser_url(origin, result.get("authorized_url").and_then(serde_json::Value::as_str), "/dsh/authorized")?;

    let user = platform_request(origin, "/auth-api/v0/users/current", None, Some(&token))?;
    Ok(SignInOutcome { grant: Grant { token, issuer: origin.to_owned() }, user, authorized_url })
}

/// 阻塞到浏览器把授权码送回回环地址；校验 `state`，只接受 `CALLBACK_PATH`。
fn wait_for_code(listener: &TcpListener, state: &str, deadline: Instant) -> Result<String, LoginError> {
    loop {
        if Instant::now() >= deadline {
            return Err(LoginError::TimedOut);
        }
        let (mut stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25));
                continue;
            }
            Err(error) => return Err(LoginError::Network(error.to_string())),
        };
        let mut reader = BufReader::new(stream.try_clone().map_err(|error| LoginError::Network(error.to_string()))?);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            continue;
        }
        let target = request_line.split_whitespace().nth(1).unwrap_or_default();
        let mut reply = |status: &str, body: &str| -> std::io::Result<()> {
            write!(stream, "HTTP/1.1 {status}\r\ncache-control: no-store\r\ncontent-length: {}\r\n\r\n{body}", body.len())?;
            stream.flush()
        };
        let Some(path) = target.split('?').next() else { continue };
        if path != CALLBACK_PATH {
            let _ = reply("404 Not Found", "");
            continue;
        }
        let query = target.split_once('?').map(|x| x.1).unwrap_or_default();
        let params = parse_query(query);
        let codes = params.iter().filter(|(name, _)| name == "code").map(|(_, value)| value).collect::<Vec<_>>();
        let states = params.iter().filter(|(name, _)| name == "state").map(|(_, value)| value).collect::<Vec<_>>();
        if codes.len() == 1 && states.len() == 1 && constant_time_eq(states[0], state) && !codes[0].is_empty() {
            let _ = reply("200 OK", "<title>Signed in</title><p>You can close this tab.</p>");
            return Ok(codes[0].to_owned());
        }
        let _ = reply("400 Bad Request", "bad callback");
    }
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (url_decode(name), url_decode(value))
        })
        .collect()
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = |byte: u8| (byte as char).to_digit(16);
                match (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push((high * 16 + low) as u8);
                        index += 3;
                    }
                    _ => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 平台信封：`{code: 0, data: {biz_code: 0, biz_data: …}}`。`token` 非空时带 `x-dsh-auth-token`。
fn platform_request(
    origin: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    token: Option<&str>,
) -> Result<serde_json::Value, LoginError> {
    fn headers<B>(mut builder: ureq::RequestBuilder<B>, token: Option<&str>) -> ureq::RequestBuilder<B> {
        builder = builder.config().timeout_global(Some(REQUEST_TIMEOUT)).build();
        for (name, value) in [
            ("user-agent", USER_AGENT),
            ("x-client-bundle-id", ""),
            ("x-client-platform", "web"),
            ("x-client-version", "0.0.0"),
            ("x-client-locale", "zh_CN"),
            ("x-client-timezone-offset", "0"),
        ] {
            builder = builder.header(name, value);
        }
        if let Some(token) = token {
            builder = builder.header(ACCOUNT_TOKEN_HEADER, token);
        }
        builder
    }
    crate::tls::install_crypto_provider();
    let url = format!("{origin}{path}");
    let result = match body {
        Some(body) => headers(ureq::post(&url), token)
            .header("content-type", "application/json")
            .send(serde_json::to_string(body).unwrap_or_default()),
        None => headers(ureq::get(&url), token).call(),
    };
    let mut response = match result {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(401)) if token.is_some() => return Err(LoginError::Expired),
        Err(ureq::Error::StatusCode(status)) => return Err(LoginError::Protocol(format!("HTTP {status} ({path})"))),
        Err(error) => return Err(LoginError::Network(error.to_string())),
    };
    let raw =
        response.body_mut().with_config().limit(BODY_LIMIT + 1).read_to_vec().map_err(|error| LoginError::Network(error.to_string()))?;
    if raw.len() as u64 > BODY_LIMIT {
        return Err(LoginError::Protocol("应答超过 64 KiB".into()));
    }
    let payload: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| LoginError::Protocol("应答不是 JSON".into()))?;
    if payload.get("code").and_then(serde_json::Value::as_i64) == Some(40003) && token.is_some() {
        return Err(LoginError::Expired);
    }
    if payload.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
        return Err(LoginError::Protocol("非成功信封".into()));
    }
    let data = payload.get("data").ok_or_else(|| LoginError::Protocol("缺少 data".into()))?;
    if data.get("biz_code").and_then(serde_json::Value::as_i64) != Some(0) {
        let biz_msg = data.get("biz_msg").and_then(serde_json::Value::as_str).unwrap_or_default();
        return Err(LoginError::Protocol(format!("biz_msg: {biz_msg}")));
    }
    Ok(data.get("biz_data").cloned().unwrap_or(serde_json::Value::Null))
}

/// 交换失败时释放平台上挂起的授权，别让下一次登录撞见旧尝试。
fn cancel_quietly(origin: &str, authorize_id: &str, verifier: &str) {
    let _ = platform_request(
        origin,
        "/auth-api/v0/dsh/auth_cancel",
        Some(&serde_json::json!({"authorize_id": authorize_id, "code_verifier": verifier})),
        None,
    );
}

/// 只接受配置源上固定路径的 URL，平台回别的（跳转钓鱼）直接拒掉。
fn browser_url(origin: &str, value: Option<&str>, path: &str) -> Result<String, LoginError> {
    let value = value.ok_or_else(|| LoginError::Protocol("缺少浏览器地址".into()))?;
    let rest = value
        .strip_prefix(origin)
        .filter(|_| !value.contains('@'))
        .ok_or_else(|| LoginError::Protocol(format!("浏览器地址不在 {origin} 上")))?;
    let (rest_path, _query) = rest.split_once('?').unwrap_or((rest, ""));
    if rest_path != path || rest.contains('#') {
        return Err(LoginError::Protocol(format!("意外的浏览器地址 {value}")));
    }
    Ok(value.to_owned())
}

fn uuid_v4() -> Result<String, LoginError> {
    let mut bytes = [0u8; 16];
    dshr_auth::fill_random(&mut bytes).map_err(LoginError::Random)?;
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    let mut out = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// 常量时间比较，防 `state` 被逐字节探测。
fn constant_time_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// Node 的 `process.platform` 名，对应原版 `device_model` 前半段。
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        "linux" => "linux",
        other => other,
    }
}

/// Node 的 `process.arch` 名。
fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn os_release() -> &'static str {
    std::env::consts::OS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn url_decoding_handles_plus_and_percent() {
        assert_eq!(url_decode("a+b%20c%2Fd"), "a b c/d");
        assert_eq!(url_decode("%zz"), "%zz");
    }

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn browser_url_accepts_only_configured_origin_and_path() {
        assert!(
            browser_url("https://platform.deepseek.com", Some("https://platform.deepseek.com/dsh/authorize?x=1"), "/dsh/authorize").is_ok()
        );
        assert!(browser_url("https://platform.deepseek.com", Some("https://evil.example/dsh/authorize"), "/dsh/authorize").is_err());
        assert!(browser_url("https://platform.deepseek.com", Some("https://platform.deepseek.com/other"), "/dsh/authorize").is_err());
        assert!(browser_url("https://platform.deepseek.com", None, "/dsh/authorize").is_err());
    }

    #[test]
    fn uuid_has_version_4_shape() {
        let id = uuid_v4().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().nth(14), Some('4'));
        assert!(matches!(id.chars().nth(19), Some('8' | '9' | 'a' | 'b')));
    }

    /// 端到端：本地起一个最小平台替身（与 demo 的 MockPlatform 相同语义），跑完整登录。
    #[test]
    fn sign_in_round_trip_against_loopback_mock() {
        let platform = TcpListener::bind(("127.0.0.1", 0)).expect("起平台替身");
        let origin = format!("http://127.0.0.1:{}", platform.local_addr().unwrap().port());
        let handle = std::thread::spawn({
            let origin = origin.clone();
            move || mock_platform(platform, origin)
        });

        // 没有浏览器：起个线程像浏览器跟随跳转一样访问授权地址（ureq 默认跟随 302 到回调）。
        let outcome = sign_in_from(&origin, move |url| {
            let url = url.to_owned();
            std::thread::spawn(move || {
                let _ = ureq::get(&url).call();
            });
        });

        let outcome = outcome.expect("登录应当成功");
        assert_eq!(outcome.grant.token, "dsh_mock_login_token");
        assert_eq!(outcome.grant.issuer, origin);
        assert_eq!(outcome.user["id"], "mock-user");
        let _ = handle.join();
    }

    /// 最小平台替身：auth_init / 授权跳转 / auth_exchange / users/current。
    fn mock_platform(listener: TcpListener, origin: String) {
        let mut redirect = String::new();
        let mut state = String::new();
        for _ in 0..4 {
            let Ok((mut stream, _)) = listener.accept() else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).unwrap();
            let path = request_line.split_whitespace().nth(1).unwrap_or_default().to_owned();
            let respond = |stream: &mut std::net::TcpStream, status: &str, content: &str| {
                write!(stream, "HTTP/1.1 {status}\r\ncontent-length: {}\r\ncontent-type: application/json\r\n\r\n{content}", content.len())
                    .unwrap();
                stream.flush().unwrap();
            };
            let envelope = |biz: serde_json::Value| {
                serde_json::json!({"code": 0, "msg": "", "data": {"biz_code": 0, "biz_msg": "", "biz_data": biz}}).to_string()
            };
            match (request_line.split_whitespace().next(), path.as_str()) {
                (Some("POST"), "/auth-api/v0/dsh/auth_init") => {
                    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    redirect = payload["redirect_uri"].as_str().unwrap().to_owned();
                    state = payload["state"].as_str().unwrap().to_owned();
                    respond(
                        &mut stream,
                        "200 OK",
                        &envelope(serde_json::json!({
                            "authorize_url": format!("{origin}/dsh/authorize?authorize_id=mock-authorize"),
                            "authorize_id": "mock-authorize",
                            "expires_in": 300,
                        })),
                    );
                }
                (Some("GET"), path) if path.starts_with("/dsh/authorize") => {
                    let location = format!("{redirect}?code=mock-login-code&state={state}");
                    write!(stream, "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\n\r\n").unwrap();
                }
                (Some("GET"), _) => {
                    // 登录后的 open_browser 直接 GET 授权地址（demo 的 follow_browser）会走上面的 302；
                    // 这里处理 /users/current 等。
                    if path.starts_with("/auth-api/v0/users/current") {
                        respond(&mut stream, "200 OK", &envelope(serde_json::json!({"id": "mock-user"})));
                    } else {
                        respond(&mut stream, "404 Not Found", "");
                    }
                }
                (Some("POST"), "/auth-api/v0/dsh/auth_exchange") => {
                    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(payload["code"], "mock-login-code");
                    respond(
                        &mut stream,
                        "200 OK",
                        &envelope(serde_json::json!({
                            "token": "dsh_mock_login_token",
                            "authorized_url": format!("{origin}/dsh/authorized"),
                        })),
                    );
                }
                _ => respond(&mut stream, "404 Not Found", ""),
            }
        }
    }
}
