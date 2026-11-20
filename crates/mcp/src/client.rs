//! MCP 客户端：任意 [`Transport`] 上的 JSON-RPC 2.0。
//!
//! 实现用得到的部分：初始化；工具的列出与调用（带进度、可取消）；资源与资源模板的列出、读取；prompts 的列出与
//! 获取；回应服务器发来的 `ping`。服务器的通知（`notifications/tools/list_changed` 等）交给订阅者。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::JoinHandle;

use crate::transport::{Incoming, Transport};

/// 我们实现的协议版本；服务器回什么版本都接受，用到的方法各版本一致。
pub(crate) const PROTOCOL_VERSION: &str = "2025-06-18";

/// 一次调用最多交回几张图片、每张多大（和 JS 插件的 `toolImages` 一样）。
const MAX_IMAGES: usize = 4;
const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("无法启动 {command}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("写入失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("连接已关闭")]
    Closed,
    #[error("连不上服务器: {0}")]
    Unreachable(String),
    #[error("服务器要求登录（HTTP 401）")]
    Unauthorized { challenge: Option<String> },
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("服务器返回错误: {0}")]
    Rpc(String),
    #[error("响应格式无法识别: {0}")]
    Protocol(String),
    #[error("配置有误: {0}")]
    Config(String),
}

impl McpError {
    /// 这个错误说明连接已经不能用了（要重连）。
    pub fn is_disconnect(&self) -> bool {
        matches!(self, Self::Closed | Self::Unreachable(_))
    }
}

/// 服务器提供的一个工具。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// 工具交回的一张图片（base64）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageContent {
    pub media_type: String,
    pub data: String,
}

/// 一次工具调用的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallResult {
    pub text: String,
    pub is_error: bool,
    pub images: Vec<ImageContent>,
}

/// 服务器上的一个资源（`resources/list`）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ResourceInfo {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// 资源模板（`resources/templates/list`）：`uri_template` 是 RFC 6570 写法。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ResourceTemplate {
    #[serde(rename = "uriTemplate")]
    pub uri_template: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `resources/read` 读到的一段内容：文本或 base64 二进制。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceContent {
    pub uri: String,
    pub mime_type: Option<String>,
    pub text: Option<String>,
    pub blob: Option<String>,
}

/// 服务器上的一个 prompt（`prompts/list`）。
#[derive(Debug, Clone, PartialEq)]
pub struct PromptInfo {
    pub name: String,
    pub description: String,
    pub arguments: Vec<PromptArgument>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromptArgument {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// 服务器发来的一条通知。
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

/// 工具调用期间收到的进度：`notifications/progress` 的 `params`。
pub type ProgressFn = Arc<dyn Fn(&Value) + Send + Sync>;

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>;

struct Shared {
    pending: Pending,
    progress: Mutex<HashMap<u64, ProgressFn>>,
    notifications: broadcast::Sender<Notification>,
    closed: watch::Sender<bool>,
}

impl Shared {
    fn close(&self) {
        self.closed.send_replace(true);
        self.pending.lock().expect("pending 锁中毒").clear();
        self.progress.lock().expect("progress 锁中毒").clear();
    }
}

pub struct Client {
    transport: Arc<dyn Transport>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    reader: JoinHandle<()>,
    capabilities: Mutex<Value>,
}

impl Client {
    /// 在一对读写流上建立 stdio 客户端，并启动读消息的后台任务（需要在 tokio 运行时里调用）。
    pub fn new(reader: impl AsyncRead + Send + Unpin + 'static, writer: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        let (transport, incoming) = crate::transport::stdio(reader, writer);
        Self::with_transport(transport, incoming)
    }

    /// 在任意传输上建立客户端（需要在 tokio 运行时里调用）。
    pub fn with_transport(transport: Arc<dyn Transport>, incoming: Incoming) -> Self {
        let shared = Arc::new(Shared {
            pending: Pending::default(),
            progress: Mutex::default(),
            notifications: broadcast::channel(64).0,
            closed: watch::channel(false).0,
        });
        let reader = tokio::spawn(read_loop(incoming, transport.clone(), shared.clone()));
        Self { transport, shared, next_id: AtomicU64::new(1), reader, capabilities: Mutex::new(Value::Null) }
    }

    /// 握手：`initialize` 加上 `notifications/initialized`。服务器声明的能力记下来（[`Self::capabilities`]）。
    pub async fn initialize(&self) -> Result<(), McpError> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "dshr", "version": env!("CARGO_PKG_VERSION")},
        });
        let result = self.request("initialize", params).await?;
        *self.capabilities.lock().expect("capabilities 锁中毒") = result["capabilities"].clone();
        self.transport.set_protocol_version(result["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION));
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await?;
        self.transport.initialized().await;
        Ok(())
    }

    /// 服务器在握手时声明的能力（`tools`、`resources`、`prompts`……）。
    pub fn capabilities(&self) -> Value {
        self.capabilities.lock().expect("capabilities 锁中毒").clone()
    }

    /// 服务器声明了某项能力。
    pub fn has_capability(&self, name: &str) -> bool {
        self.capabilities.lock().expect("capabilities 锁中毒").get(name).is_some_and(|value| !value.is_null())
    }

    /// 服务器发来的通知（订阅之后的）。
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.shared.notifications.subscribe()
    }

    /// 连接断开时变成 `true`。
    pub fn closed_signal(&self) -> watch::Receiver<bool> {
        self.shared.closed.subscribe()
    }

    pub fn is_closed(&self) -> bool {
        *self.shared.closed.borrow()
    }

    /// 主动断开。
    pub async fn close(&self) {
        self.transport.close().await;
        self.shared.close();
    }

    /// 服务器提供的全部工具（跟着 `nextCursor` 翻页）。
    pub async fn list_tools(&self) -> Result<Vec<ToolInfo>, McpError> {
        let pages = self.list_all("tools/list", "tools").await?;
        Ok(pages
            .iter()
            .filter_map(|tool| {
                Some(ToolInfo {
                    name: tool["name"].as_str()?.to_owned(),
                    description: tool["description"].as_str().unwrap_or_default().to_owned(),
                    input_schema: tool["inputSchema"].clone(),
                })
            })
            .collect())
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallResult, McpError> {
        self.call_tool_with(name, arguments, None).await
    }

    /// 调用工具；给了 `progress` 时请求带上进度令牌，服务器的进度通知交给它。
    pub async fn call_tool_with(&self, name: &str, arguments: Value, progress: Option<ProgressFn>) -> Result<CallResult, McpError> {
        let result = self.request_with("tools/call", json!({"name": name, "arguments": arguments}), progress).await?;
        Ok(call_result(&result))
    }

    /// 全部资源。
    pub async fn list_resources(&self) -> Result<Vec<ResourceInfo>, McpError> {
        let items = self.list_all("resources/list", "resources").await?;
        Ok(items
            .iter()
            .filter_map(|item| {
                let uri = item["uri"].as_str()?.to_owned();
                Some(ResourceInfo {
                    name: item["title"].as_str().or(item["name"].as_str()).unwrap_or(&uri).to_owned(),
                    uri,
                    description: item["description"].as_str().map(str::to_owned),
                    mime_type: item["mimeType"].as_str().map(str::to_owned),
                })
            })
            .collect())
    }

    /// 全部资源模板。
    pub async fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        let items = self.list_all("resources/templates/list", "resourceTemplates").await?;
        Ok(items
            .iter()
            .filter_map(|item| {
                let uri_template = item["uriTemplate"].as_str()?.to_owned();
                Some(ResourceTemplate {
                    name: item["title"].as_str().or(item["name"].as_str()).unwrap_or(&uri_template).to_owned(),
                    uri_template,
                    description: item["description"].as_str().map(str::to_owned),
                })
            })
            .collect())
    }

    /// 读一个资源。
    pub async fn read_resource(&self, uri: &str) -> Result<Vec<ResourceContent>, McpError> {
        let result = self.request("resources/read", json!({"uri": uri})).await?;
        let contents =
            result["contents"].as_array().ok_or_else(|| McpError::Protocol(format!("resources/read 没有 contents: {result}")))?;
        Ok(contents
            .iter()
            .map(|item| ResourceContent {
                uri: item["uri"].as_str().unwrap_or(uri).to_owned(),
                mime_type: item["mimeType"].as_str().map(str::to_owned),
                text: item["text"].as_str().map(str::to_owned),
                blob: item["blob"].as_str().map(str::to_owned),
            })
            .collect())
    }

    /// 全部 prompts。
    pub async fn list_prompts(&self) -> Result<Vec<PromptInfo>, McpError> {
        let items = self.list_all("prompts/list", "prompts").await?;
        Ok(items
            .iter()
            .filter_map(|item| {
                let arguments = item["arguments"].as_array().map(Vec::as_slice).unwrap_or_default();
                Some(PromptInfo {
                    name: item["name"].as_str()?.to_owned(),
                    description: item["title"].as_str().or(item["description"].as_str()).unwrap_or_default().to_owned(),
                    arguments: arguments
                        .iter()
                        .filter_map(|argument| {
                            Some(PromptArgument {
                                name: argument["name"].as_str()?.to_owned(),
                                description: argument["description"].as_str().unwrap_or_default().to_owned(),
                                required: argument["required"].as_bool().unwrap_or(false),
                            })
                        })
                        .collect(),
                })
            })
            .collect())
    }

    /// 取一个 prompt，把它的消息拼成一段文字（文本内容原样，资源内容带上 URI，其余留个记号）。
    pub async fn get_prompt(&self, name: &str, arguments: &Map<String, Value>) -> Result<String, McpError> {
        let result = self.request("prompts/get", json!({"name": name, "arguments": arguments})).await?;
        let messages = result["messages"].as_array().ok_or_else(|| McpError::Protocol(format!("prompts/get 没有 messages: {result}")))?;
        let parts: Vec<String> = messages.iter().map(|message| content_text(&message["content"])).filter(|text| !text.is_empty()).collect();
        Ok(parts.join("\n\n"))
    }

    /// 跟着 `nextCursor` 把一个列表方法的所有页取完，返回 `key` 下的全部条目。
    async fn list_all(&self, method: &str, key: &str) -> Result<Vec<Value>, McpError> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        // 防止服务器一直给同一个游标。
        for _ in 0..1000 {
            let params = cursor.as_ref().map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
            let result = self.request(method, params).await?;
            let page = result[key].as_array().ok_or_else(|| McpError::Protocol(format!("{method} 没有 {key}: {result}")))?;
            items.extend(page.iter().cloned());
            cursor = result["nextCursor"].as_str().filter(|cursor| !cursor.is_empty()).map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(items)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        self.request_with(method, params, None).await
    }

    async fn request_with(&self, method: &str, mut params: Value, progress: Option<ProgressFn>) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (respond, response) = oneshot::channel();
        self.shared.pending.lock().expect("pending 锁中毒").insert(id, respond);
        // 先登记、再看连接是否已断：[`Shared::close`] 先置“已断开”、再清空等待表。登记之后才断的，清空时
        // 会丢掉我们的发送端，下面的等待立刻失败；反过来先检查再登记的话，检查和登记之间断开，
        // 这一项就没人清，请求会一直挂着。
        if self.is_closed() {
            self.shared.pending.lock().expect("pending 锁中毒").remove(&id);
            return Err(McpError::Closed);
        }
        if let Some(progress) = progress
            && let Some(params) = params.as_object_mut()
        {
            params.entry("_meta").or_insert_with(|| json!({}))["progressToken"] = json!(id);
            self.shared.progress.lock().expect("progress 锁中毒").insert(id, progress);
        }
        // 调用方中途放弃（超时、用户中断）时把等待项清掉，并告诉服务器不用再做了。
        let _guard = Forget { shared: &self.shared, transport: &self.transport, id, cancellable: method != "initialize" };
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(error) = self.transport.send(&message).await {
            if error.is_disconnect() {
                self.shared.close();
            }
            return Err(error);
        }
        let result = response.await.map_err(|_| McpError::Closed)?;
        if let Err(error) = &result
            && error.is_disconnect()
        {
            self.shared.close();
        }
        result
    }

    async fn send(&self, message: Value) -> Result<(), McpError> {
        let sent = self.transport.send(&message).await;
        if let Err(error) = &sent
            && error.is_disconnect()
        {
            self.shared.close();
        }
        sent
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
        self.shared.closed.send_replace(true);
    }
}

struct Forget<'a> {
    shared: &'a Arc<Shared>,
    transport: &'a Arc<dyn Transport>,
    id: u64,
    cancellable: bool,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.shared.progress.lock().expect("progress 锁中毒").remove(&self.id);
        let abandoned = self.shared.pending.lock().expect("pending 锁中毒").remove(&self.id).is_some();
        // 还在等的请求被放弃：告诉服务器（连接已经断了就不必了）。
        if abandoned && self.cancellable && !*self.shared.closed.borrow() {
            let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
            let transport = self.transport.clone();
            let notice = json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                                "params": {"requestId": self.id, "reason": "cancelled by the client"}});
            runtime.spawn(async move {
                let _ = transport.send(&notice).await;
            });
        }
    }
}

/// 分发服务器发来的消息：响应交给等它的请求；`ping` 直接回应，其他请求回“不支持”；进度交给对应的调用，
/// 其余通知广播给订阅者。接收端结束时（连接断开）让所有还在等的请求失败。
async fn read_loop(mut incoming: Incoming, transport: Arc<dyn Transport>, shared: Arc<Shared>) {
    while let Some(message) = incoming.recv().await {
        let id = &message["id"];
        if let Some(method) = message["method"].as_str() {
            if id.is_null() {
                if method == "notifications/progress" {
                    let token = message["params"]["progressToken"].as_u64();
                    let handler = token.and_then(|token| shared.progress.lock().expect("progress 锁中毒").get(&token).cloned());
                    if let Some(handler) = handler {
                        handler(&message["params"]);
                    }
                    continue;
                }
                let _ = shared.notifications.send(Notification { method: method.to_owned(), params: message["params"].clone() });
                continue;
            }
            let reply = match method {
                "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                _ => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}),
            };
            if let Err(error) = transport.send(&reply).await
                && error.is_disconnect()
            {
                break;
            }
        } else if let Some(id) = id.as_u64() {
            let Some(respond) = shared.pending.lock().expect("pending 锁中毒").remove(&id) else { continue };
            let result = match message.get("error") {
                Some(error) => Err(McpError::Rpc(error["message"].as_str().map_or_else(|| error.to_string(), str::to_owned))),
                None => Ok(message["result"].clone()),
            };
            let _ = respond.send(result);
        }
    }
    shared.close();
}

/// `tools/call` 的结果：文本拼起来，图片单独交回（类型、大小、张数不合规的留个记号）。
fn call_result(result: &Value) -> CallResult {
    let mut text = String::new();
    let mut images = Vec::new();
    for item in result["content"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let piece = match item["type"].as_str() {
            Some("image") => match image(item) {
                Some(image) if images.len() < MAX_IMAGES => {
                    images.push(image);
                    continue;
                }
                _ => "[image content omitted]".to_owned(),
            },
            Some(_) => content_text(item),
            None => continue,
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&piece);
    }
    // 只有结构化结果、没有文本时，把结构化结果原样交给模型。
    if text.is_empty() && !result["structuredContent"].is_null() {
        text = result["structuredContent"].to_string();
    }
    CallResult { text, is_error: result["isError"].as_bool().unwrap_or(false), images }
}

/// 一段内容（工具结果或 prompt 消息里的）写成文字。
fn content_text(item: &Value) -> String {
    match item["type"].as_str() {
        Some("text") => item["text"].as_str().unwrap_or_default().to_owned(),
        Some("resource") => {
            let resource = &item["resource"];
            let uri = resource["uri"].as_str().unwrap_or_default();
            match resource["text"].as_str() {
                Some(text) => format!("[resource {uri}]\n{text}"),
                None => {
                    let mime = resource["mimeType"].as_str().unwrap_or("binary");
                    format!("[resource {uri} ({mime}): binary content omitted]")
                }
            }
        }
        Some("resource_link") => {
            let uri = item["uri"].as_str().unwrap_or_default();
            match item["name"].as_str() {
                Some(name) => format!("[resource link: {name} {uri}]"),
                None => format!("[resource link: {uri}]"),
            }
        }
        Some(kind) => format!("[{kind} content omitted]"),
        None => String::new(),
    }
}

/// 能交给模型的图片：类型认识、base64 能解开、不超过大小上限。
fn image(item: &Value) -> Option<ImageContent> {
    let media_type = item["mimeType"].as_str()?;
    if !IMAGE_TYPES.contains(&media_type) {
        return None;
    }
    let data = item["data"].as_str()?;
    if data.len() / 4 * 3 > MAX_IMAGE_BYTES {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    Some(ImageContent { media_type: media_type.to_owned(), data: data.to_owned() })
}
