//! `host.fetch`：插件访问网络的唯一出口。
//!
//! 只能访问清单里 `net:<主机>` 声明过的主机，重定向的每一跳也要在范围内；请求和响应的正文都是文本、
//! 都有大小上限（正文可以是二进制，经 base64 传递）。请求不在插件线程上等：交给 `ops.rs`，
//! 受这次调用剩下的时限约束，调用方放弃或调用结束时立刻中止。

use std::collections::BTreeMap;
use std::future::Future;

use reqwest::{Method, Url};
use serde::Deserialize;

use crate::manifest::Capabilities;

/// 请求正文和响应正文各自的上限。
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

/// 脚本给 `fetch` 的第二个参数。
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Options {
    method: Option<String>,
    headers: BTreeMap<String, String>,
    body: Option<String>,
    /// 二进制的请求正文（base64）；和 `body` 只能有一个。
    body_base64: Option<String>,
    /// 由宿主填进请求头的密钥：`{ 头名: 密钥名 }`。脚本不经手明文（见 `secrets.rs`）。
    secret_headers: BTreeMap<String, String>,
}

/// 一个插件的 HTTP 客户端，第一次 `fetch` 时才建。请求在共用的 IO 运行时上跑（`ops.rs`）。
pub(crate) struct Net {
    client: reqwest::Client,
}

impl Net {
    pub fn new(capabilities: &Capabilities) -> Result<Self, String> {
        let allowed = capabilities.clone();
        // 重定向到没声明的主机时不跟过去，把这个 3xx 响应原样交给脚本。
        let redirects = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if permitted(&allowed, attempt.url()).is_ok() {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        // 有的连接器在建客户端时就要一个当前的运行时。
        let _guard = super::ops::io()?.enter();
        let client = reqwest::Client::builder()
            .user_agent("dshr-plugin")
            .redirect(redirects)
            .build()
            .map_err(|error| format!("cannot create the HTTP client: {error}"))?;
        Ok(Self { client })
    }

    /// 按脚本给的参数建一个请求：检查主机、方法、正文大小，填上请求头和密钥。
    fn build(
        &self,
        capabilities: &Capabilities,
        url: &str,
        options: &str,
        secret: impl Fn(&str) -> Result<String, String>,
    ) -> Result<reqwest::RequestBuilder, String> {
        use base64::Engine as _;
        let options: Options = serde_json::from_str(options).map_err(|error| format!("invalid fetch options: {error}"))?;
        let url = Url::parse(url).map_err(|error| format!("invalid URL {url}: {error}"))?;
        permitted(capabilities, &url)?;
        let method = match options.method.as_deref().unwrap_or("GET").to_ascii_uppercase().as_str() {
            "GET" => Method::GET,
            "HEAD" => Method::HEAD,
            "POST" => Method::POST,
            "PUT" => Method::PUT,
            "PATCH" => Method::PATCH,
            "DELETE" => Method::DELETE,
            other => return Err(format!("unsupported HTTP method {other}")),
        };
        let mut request = self.client.request(method, url);
        for (name, value) in &options.headers {
            request = request.header(name, value);
        }
        for (header, name) in &options.secret_headers {
            request = request.header(header, secret(name)?);
        }
        let body = match (options.body, options.body_base64) {
            (Some(_), Some(_)) => return Err("give either body or bodyBase64, not both".into()),
            (Some(text), None) => Some(text.into_bytes()),
            (None, Some(encoded)) => Some(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|error| format!("bodyBase64 is not valid base64: {error}"))?,
            ),
            (None, None) => None,
        };
        if let Some(body) = body {
            if body.len() > MAX_BODY_BYTES {
                return Err(format!("the request body is too large ({} bytes, the limit is {MAX_BODY_BYTES})", body.len()));
            }
            request = request.body(body);
        }
        Ok(request)
    }

    /// 流式请求：返回的 future 完成时给出 `{status, url, headers}` 的 JSON 和正文的接收端（一块一块，最多 64 KB 一块）。
    pub fn prepare_stream(
        &self,
        capabilities: &Capabilities,
        url: &str,
        options: &str,
        secret: impl Fn(&str) -> Result<String, String>,
    ) -> Result<impl Future<Output = Result<(String, super::BodyReceiver), String>> + Send + 'static, String> {
        let request = self.build(capabilities, url, options, secret)?;
        Ok(async move {
            let mut response = request.send().await.map_err(|error| describe(&error))?;
            let head = serde_json::json!({ "status": response.status().as_u16(), "url": response.url().to_string(), "headers": header_map(&response) })
                .to_string();
            let (sender, receiver) = tokio::sync::mpsc::channel(8);
            tokio::spawn(async move {
                loop {
                    match response.chunk().await {
                        Ok(Some(chunk)) => {
                            for piece in chunk.chunks(64 * 1024) {
                                if sender.send(Ok(piece.to_vec())).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Ok(None) => return,
                        Err(error) => {
                            let _ = sender.send(Err(describe(&error))).await;
                            return;
                        }
                    }
                }
            });
            Ok((head, receiver))
        })
    }

    /// 检查并准备一次请求；返回的 future 完成时给出 `{status, url, headers, body, bodyBase64}` 的 JSON。
    /// `secret` 按名字取插件的密钥（用于 `secretHeaders`）。
    pub fn prepare(
        &self,
        capabilities: &Capabilities,
        url: &str,
        options: &str,
        secret: impl Fn(&str) -> Result<String, String>,
    ) -> Result<impl Future<Output = Result<String, String>> + Send + 'static, String> {
        use base64::Engine as _;
        let request = self.build(capabilities, url, options, secret)?;

        Ok(async move {
            let mut response = request.send().await.map_err(|error| describe(&error))?;
            let status = response.status().as_u16();
            let url = response.url().to_string();
            let headers = header_map(&response);
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| describe(&error))? {
                if body.len() + chunk.len() > MAX_BODY_BYTES {
                    return Err(format!("the response body is larger than the limit ({MAX_BODY_BYTES} bytes)"));
                }
                body.extend_from_slice(&chunk);
            }
            let text = String::from_utf8_lossy(&body);
            let encoded = base64::engine::general_purpose::STANDARD.encode(&body);
            Ok(serde_json::json!({ "status": status, "url": url, "headers": headers, "body": text, "bodyBase64": encoded }).to_string())
        })
    }
}

impl Net {
    /// 下载一个文件（装插件用）：主机按 `net:` 检查，最多 `limit` 字节，非 2xx 是错误。
    pub fn download(
        &self,
        capabilities: &Capabilities,
        url: &str,
        limit: usize,
    ) -> Result<impl Future<Output = Result<Vec<u8>, String>> + Send + 'static, String> {
        let url = Url::parse(url).map_err(|error| format!("invalid URL {url}: {error}"))?;
        permitted(capabilities, &url)?;
        let request = self.client.get(url);
        Ok(async move {
            let mut response = request.send().await.map_err(|error| describe(&error))?;
            if !response.status().is_success() {
                return Err(format!("download failed: HTTP {}", response.status().as_u16()));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| describe(&error))? {
                if body.len() + chunk.len() > limit {
                    return Err(format!("the download is larger than {} MB", limit / 1024 / 1024));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
    }
}

impl Net {
    /// 底层的 HTTP 客户端（重定向策略同样只跟进声明过的主机）。
    pub fn client(&self) -> reqwest::Client {
        self.client.clone()
    }
}

/// 响应头：同名的合并成一项，名字是小写。
fn header_map(response: &reqwest::Response) -> BTreeMap<String, String> {
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in response.headers() {
        let value = String::from_utf8_lossy(value.as_bytes());
        headers
            .entry(name.as_str().to_owned())
            .and_modify(|joined| {
                joined.push_str(", ");
                joined.push_str(&value);
            })
            .or_insert_with(|| value.into_owned());
    }
    headers
}

/// 这个 URL 能不能访问：只有 http / https，主机要在声明过的范围里。
pub(super) fn permitted(capabilities: &Capabilities, url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("only http and https URLs can be fetched, not {}:", url.scheme()));
    }
    let host = url.host_str().unwrap_or_default();
    if capabilities.allows_host(host) {
        Ok(())
    } else {
        Err(format!("this plugin may not access {host}: add `net:{host}` to capabilities in plugin.toml"))
    }
}

/// reqwest 的错误信息只有最外面一层（"error sending request"），把原因链接上。
fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
