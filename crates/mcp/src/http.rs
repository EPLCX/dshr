//! streamable HTTP 传输（MCP 2025-06-18）。
//!
//! - 每条消息 POST 到服务器地址，`Accept: application/json, text/event-stream`；响应是一条 JSON（或一组），
//!   或者一条 SSE 流（流里可能先有通知、服务器请求，最后是响应）。通知和响应得到 202，没有正文。
//! - `initialize` 的响应头里有 `Mcp-Session-Id` 时，之后的请求都带上它，并带 `MCP-Protocol-Version`。
//! - 握手之后 GET 同一个地址，开一条长连接收服务器主动发的消息；服务器回 405 就是不支持，不用它。
//! - 带着会话 id 收到 404：会话已失效，按连接断开处理（重新连接时重新初始化）。
//! - 断开时 DELETE 告诉服务器结束会话。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::McpError;
use crate::transport::{Incoming, Transport};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// 错误响应的正文最多留这么多字符。
const MAX_ERROR_BODY: usize = 500;
const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";

pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
    incoming: mpsc::UnboundedSender<Value>,
    /// 读 SSE 响应、长连接的任务；断开时一起停掉。
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// 建立 HTTP 传输。`headers` 是配置里写的请求头（占位符已经换好）。不发任何请求：第一条消息（`initialize`）
/// 才真正连接。
pub fn connect(url: &str, headers: &[(String, String)]) -> Result<(Arc<dyn Transport>, Incoming), McpError> {
    let parsed = reqwest::Url::parse(url).map_err(|error| McpError::Config(format!("地址 `{url}` 不对：{error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(McpError::Config(format!("地址 `{url}` 只能是 http 或 https")));
    }
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| McpError::Config(format!("请求头名 `{name}` 不对")))?;
        let value = HeaderValue::from_str(value).map_err(|_| McpError::Config(format!("请求头 `{name}` 的值里有不能用的字符")))?;
        map.insert(name, value);
    }
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .map_err(|error| McpError::Unreachable(error.to_string()))?;
    let (sender, incoming) = mpsc::unbounded_channel();
    let transport = HttpTransport {
        client,
        url: url.to_owned(),
        headers: map,
        session: Mutex::new(None),
        protocol: Mutex::new(None),
        incoming: sender,
        tasks: Mutex::new(Vec::new()),
    };
    Ok((Arc::new(transport), incoming))
}

impl HttpTransport {
    fn request(&self, method: reqwest::Method) -> reqwest::RequestBuilder {
        let mut request = self.client.request(method, &self.url).headers(self.headers.clone());
        if let Some(session) = self.session.lock().expect("session 锁中毒").as_deref() {
            request = request.header(SESSION_HEADER, session);
        }
        if let Some(version) = self.protocol.lock().expect("protocol 锁中毒").as_deref() {
            request = request.header(VERSION_HEADER, version);
        }
        request
    }

    fn has_session(&self) -> bool {
        self.session.lock().expect("session 锁中毒").is_some()
    }

    /// 把一条 SSE 流里的消息逐条交给客户端（在后台读）。
    fn read_events(&self, response: reqwest::Response) {
        let sender = self.incoming.clone();
        let task = tokio::spawn(async move {
            let mut events = response.bytes_stream().eventsource();
            while let Some(Ok(event)) = events.next().await {
                if event.data.trim().is_empty() {
                    continue;
                }
                let Ok(message) = serde_json::from_str::<Value>(&event.data) else { continue };
                if !deliver(&sender, message) {
                    break;
                }
            }
        });
        let mut tasks = self.tasks.lock().expect("tasks 锁中毒");
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }
}

/// 一条 JSON 或一组（batch）交给客户端。客户端已经不在时返回 `false`。
fn deliver(sender: &mpsc::UnboundedSender<Value>, message: Value) -> bool {
    match message {
        Value::Array(messages) => messages.into_iter().all(|message| sender.send(message).is_ok()),
        message => sender.send(message).is_ok(),
    }
}

fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim_start().to_ascii_lowercase().starts_with("text/event-stream"))
}

fn network_error(error: reqwest::Error) -> McpError {
    McpError::Unreachable(error.to_string())
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn send(&self, message: &Value) -> Result<(), McpError> {
        let request = self
            .request(reqwest::Method::POST)
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .body(message.to_string());
        let response = request.send().await.map_err(network_error)?;
        if let Some(session) = response.headers().get(SESSION_HEADER).and_then(|value| value.to_str().ok()) {
            *self.session.lock().expect("session 锁中毒") = Some(session.to_owned());
        }
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND && self.has_session() {
            return Err(McpError::Closed);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let challenge = response.headers().get("www-authenticate").and_then(|value| value.to_str().ok()).map(str::to_owned);
            return Err(McpError::Unauthorized { challenge });
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let body: String = body.chars().take(MAX_ERROR_BODY).collect();
            return Err(McpError::Http { status: status.as_u16(), body });
        }
        if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::NO_CONTENT {
            return Ok(());
        }
        if is_event_stream(&response) {
            self.read_events(response);
            return Ok(());
        }
        let body = response.bytes().await.map_err(network_error)?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let message: Value = serde_json::from_slice(&body).map_err(|error| McpError::Protocol(format!("响应不是 JSON：{error}")))?;
        if !deliver(&self.incoming, message) {
            return Err(McpError::Closed);
        }
        Ok(())
    }

    fn set_protocol_version(&self, version: &str) {
        *self.protocol.lock().expect("protocol 锁中毒") = Some(version.to_owned());
    }

    async fn initialized(&self) {
        // 长连接：收服务器主动发来的通知、请求。不支持（405）或出错都不影响正常使用。
        let request = self.request(reqwest::Method::GET).header(ACCEPT, "text/event-stream");
        let sender = self.incoming.clone();
        let task = tokio::spawn(async move {
            let Ok(response) = request.send().await else { return };
            if !response.status().is_success() || !is_event_stream(&response) {
                return;
            }
            let mut events = response.bytes_stream().eventsource();
            while let Some(Ok(event)) = events.next().await {
                let Ok(message) = serde_json::from_str::<Value>(&event.data) else { continue };
                if !deliver(&sender, message) {
                    break;
                }
            }
        });
        self.tasks.lock().expect("tasks 锁中毒").push(task);
    }

    async fn close(&self) {
        for task in self.tasks.lock().expect("tasks 锁中毒").drain(..) {
            task.abort();
        }
        if self.has_session() {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.request(reqwest::Method::DELETE).send()).await;
        }
    }
}

impl Drop for HttpTransport {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().expect("tasks 锁中毒").drain(..) {
            task.abort();
        }
    }
}
