//! 流式响应和 WebSocket：`host.fetch(url, { stream: true })`、`host.websocket(url)`。
//!
//! 打开的流和连接按编号记在 [`super::ops::Ops`] 的资源表里，属于发起它的那次调用：调用结束（包括常驻服务
//! 被停下）时全部关掉。每读一块、每收一条消息都是一个慢操作，交给 IO 运行时，不占插件线程。

use std::sync::Arc;

use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

use super::host::State;
use super::ops::Resource;

/// 一条 WebSocket 消息最大多少。
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 一个 WebSocket 连接：收发各自加锁，可以一边等消息一边发。
pub(crate) struct Socket {
    write: Mutex<futures::stream::SplitSink<WsStream, Message>>,
    read: Mutex<futures::stream::SplitStream<WsStream>>,
}

impl State {
    /// 流式 fetch：结果是 `{ status, url, headers, stream: <编号> }`。
    pub(super) fn fetch_stream(&self, url: &str, options: &str) -> Result<u64, String> {
        self.require_net()?;
        let net = self.net()?;
        let request = net.prepare_stream(&self.capabilities, url, options, |name| {
            self.secret_value(name)?.ok_or_else(|| format!("the secret {name} has not been set"))
        })?;
        // 响应头到了就把正文的接收端登记成资源：要在插件线程上登记，所以先把接收端放进一个共享的槽里。
        let slot: Arc<std::sync::Mutex<Option<super::BodyReceiver>>> = Arc::default();
        let filled = slot.clone();
        let work = async move {
            let (head, receiver) = request.await?;
            *filled.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(receiver);
            Ok(head)
        };
        let id = self.ops.start(work, self.remaining())?;
        self.pending_streams.borrow_mut().push((id, slot));
        Ok(id)
    }

    /// 某个流式 fetch 的操作完成后，`prelude.js` 来取它的正文编号。
    pub(super) fn claim_stream(&self, op: u64) -> Result<u64, String> {
        let position = self.pending_streams.borrow().iter().position(|(id, _)| *id == op).ok_or("unknown stream")?;
        let (_, slot) = self.pending_streams.borrow_mut().remove(position);
        let receiver = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take().ok_or("the stream is not open")?;
        self.ops.add_resource(Resource::Stream(Arc::new(Mutex::new(receiver))))
    }

    /// 读下一块：结果是 base64，正文读完时是 `null`。
    pub(super) fn read_chunk(&self, stream: u64) -> Result<u64, String> {
        let Some(Resource::Stream(receiver)) = self.ops.resource(stream) else {
            return Err("the stream is closed".into());
        };
        let work = async move {
            match receiver.lock().await.recv().await {
                Some(Ok(chunk)) => Ok(json!(base64::engine::general_purpose::STANDARD.encode(chunk)).to_string()),
                Some(Err(error)) => Err(error),
                None => Ok("null".into()),
            }
        };
        self.ops.start(work, self.remaining())
    }

    /// 打开 WebSocket：`{ headers, secretHeaders, protocols }`，结果是连接编号（经 `claim_socket` 取）。
    pub(super) fn websocket(&self, url: &str, options: &str) -> Result<u64, String> {
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Options {
            headers: std::collections::BTreeMap<String, String>,
            secret_headers: std::collections::BTreeMap<String, String>,
            protocols: Vec<String>,
        }
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
        self.require_net()?;
        let options: Options = serde_json::from_str(options).map_err(|error| format!("invalid websocket options: {error}"))?;
        let parsed = reqwest::Url::parse(url).map_err(|error| format!("invalid URL {url}: {error}"))?;
        if !matches!(parsed.scheme(), "ws" | "wss") {
            return Err(format!("websocket URLs start with ws:// or wss://, not {}:", parsed.scheme()));
        }
        let host = parsed.host_str().unwrap_or_default();
        if !self.capabilities.allows_host(host) {
            return Err(format!("this plugin may not access {host}: add `net:{host}` to capabilities in plugin.toml"));
        }
        let mut request = url.into_client_request().map_err(|error| format!("invalid websocket request: {error}"))?;
        let mut header = |name: &str, value: &str| -> Result<(), String> {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| error.to_string())?;
            let value = reqwest::header::HeaderValue::from_str(value).map_err(|error| error.to_string())?;
            request.headers_mut().insert(name, value);
            Ok(())
        };
        for (name, value) in &options.headers {
            header(name, value)?;
        }
        for (name, secret) in &options.secret_headers {
            let value = self.secret_value(secret)?.ok_or_else(|| format!("the secret {secret} has not been set"))?;
            header(name, &value)?;
        }
        if !options.protocols.is_empty() {
            header("sec-websocket-protocol", &options.protocols.join(", "))?;
        }
        header("user-agent", "dshr-plugin")?;
        let slot: Arc<std::sync::Mutex<Option<Arc<Socket>>>> = Arc::default();
        let filled = slot.clone();
        let work = async move {
            let (stream, response) = connect(request).await?;
            let protocol = response.headers().get("sec-websocket-protocol").and_then(|value| value.to_str().ok()).map(str::to_owned);
            let (write, read) = stream.split();
            *filled.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(Arc::new(Socket { write: Mutex::new(write), read: Mutex::new(read) }));
            Ok(json!({ "protocol": protocol }).to_string())
        };
        let id = self.ops.start(work, self.remaining())?;
        self.pending_sockets.borrow_mut().push((id, slot));
        Ok(id)
    }

    pub(super) fn claim_socket(&self, op: u64) -> Result<u64, String> {
        let position = self.pending_sockets.borrow().iter().position(|(id, _)| *id == op).ok_or("unknown socket")?;
        let (_, slot) = self.pending_sockets.borrow_mut().remove(position);
        let socket = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take().ok_or("the socket is not open")?;
        self.ops.add_resource(Resource::Socket(socket))
    }

    fn socket(&self, id: u64) -> Result<Arc<Socket>, String> {
        match self.ops.resource(id) {
            Some(Resource::Socket(socket)) => Ok(socket),
            _ => Err("the websocket is closed".into()),
        }
    }

    /// 发一条消息：`binary` 时 `data` 是 base64。
    pub(super) fn ws_send(&self, id: u64, data: &str, binary: bool) -> Result<u64, String> {
        let socket = self.socket(id)?;
        let message = if binary {
            Message::Binary(base64::engine::general_purpose::STANDARD.decode(data).map_err(|error| error.to_string())?.into())
        } else {
            Message::Text(data.to_owned().into())
        };
        let work = async move {
            socket.write.lock().await.send(message).await.map_err(|error| format!("cannot send: {error}"))?;
            Ok("null".into())
        };
        self.ops.start(work, self.remaining())
    }

    /// 下一条消息：`{ type: "text" | "binary", data }`，连接关了是 `null`。ping / pong 由库处理。
    pub(super) fn ws_next(&self, id: u64) -> Result<u64, String> {
        let socket = self.socket(id)?;
        let work = async move {
            let mut read = socket.read.lock().await;
            loop {
                let message = match read.next().await {
                    None | Some(Ok(Message::Close(_))) => return Ok("null".into()),
                    Some(Err(error)) => return Err(format!("the websocket failed: {error}")),
                    Some(Ok(message)) => message,
                };
                if message.len() > MAX_MESSAGE_BYTES {
                    return Err(format!("a websocket message is larger than {} MB", MAX_MESSAGE_BYTES / 1024 / 1024));
                }
                match message {
                    Message::Text(text) => return Ok(json!({ "type": "text", "data": text.as_str() }).to_string()),
                    Message::Binary(bytes) => {
                        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        return Ok(json!({ "type": "binary", "data": data }).to_string());
                    }
                    _ => continue,
                }
            }
        };
        self.ops.start(work, self.remaining())
    }

    pub(super) fn ws_close(&self, id: u64) -> Result<u64, String> {
        let socket = self.socket(id)?;
        self.ops.remove_resource(id);
        let work = async move {
            let _ = socket.write.lock().await.close().await;
            Ok("null".into())
        };
        self.ops.start(work, self.remaining())
    }
}

/// 连 WebSocket。`wss://` 要用进程级的 rustls 后端（见 `dshr_config::tls`），先确保装上。
async fn connect(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
) -> Result<(WsStream, tokio_tungstenite::tungstenite::handshake::client::Response), String> {
    dshr_config::tls::install_crypto_provider();
    tokio_tungstenite::connect_async(request).await.map_err(|error| format!("cannot connect: {error}"))
}

#[cfg(test)]
mod tests {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    /// `wss://` 要构建 rustls 的 ClientConfig：以前两个加密后端同时打开时这里会 panic。
    /// 对面只是个不说 TLS 的 TCP 监听，握手失败、返回错误就对了。
    #[tokio::test]
    async fn wss_handshake_fails_cleanly_instead_of_panicking() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });
        let request = format!("wss://127.0.0.1:{port}/").into_client_request().unwrap();
        let result = tokio::spawn(super::connect(request)).await.expect("连接 wss:// 不应当 panic");
        assert!(result.is_err());
    }
}
