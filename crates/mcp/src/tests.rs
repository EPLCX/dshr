use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dshr_tools::{Access, EventSink, OutputSink, Tool, ToolContext, ToolOutput};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::mpsc;

use super::*;

/// 1×1 的 PNG。
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

/// 内存里的假服务器共用的状态：测试从这里改工具列表、推通知、断开连接，看服务器收到了什么。
#[derive(Default)]
struct Fake {
    /// 现在的工具（`tools/list` 分两页返回）。
    tools: Mutex<Vec<Value>>,
    /// 建立过几次连接。
    connects: AtomicUsize,
    /// 接下来这么多次连接直接失败。
    fail_next: AtomicUsize,
    /// 收到的 `notifications/cancelled` 的 requestId。
    cancelled: Mutex<Vec<u64>>,
    /// 往现有连接推消息（通知），或者推 `null` 让它断开。
    pushers: Mutex<Vec<mpsc::UnboundedSender<Value>>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        let fake = Arc::new(Self::default());
        *fake.tools.lock().unwrap() = vec![
            json!({"name": "echo", "description": "Echo the text back.", "inputSchema": {
                "type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}),
            json!({"name": "fail"}),
        ];
        fake
    }

    fn push(&self, message: Value) {
        self.pushers.lock().unwrap().retain(|pusher| pusher.send(message.clone()).is_ok());
    }

    fn disconnect_all(&self) {
        self.push(Value::Null);
    }

    fn connect(self: &Arc<Self>) -> Client {
        let (ours, theirs) = tokio::io::duplex(256 * 1024);
        let (pusher, pushed) = mpsc::unbounded_channel();
        self.pushers.lock().unwrap().push(pusher);
        tokio::spawn(fake_server(theirs, self.clone(), pushed));
        let (reader, writer) = tokio::io::split(ours);
        Client::new(reader, writer)
    }
}

/// 假服务器：工具 `echo`（带一张图片）、`fail`（报告错误）、`slow`（先报进度，再一直不回）；
/// 一个资源 `memo://a`、一个 prompt `review(file)`。握手之后先 ping 客户端，再打一行不是 JSON 的日志。
async fn fake_server(stream: DuplexStream, fake: Arc<Fake>, mut pushed: mpsc::UnboundedReceiver<Value>) {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = tokio::select! {
            line = lines.next_line() => match line { Ok(Some(line)) => line, _ => return },
            message = pushed.recv() => match message {
                Some(Value::Null) | None => return,
                Some(message) => {
                    writer.write_all(format!("{message}\n").as_bytes()).await.unwrap();
                    continue;
                }
            },
        };
        let message: Value = serde_json::from_str(&line).unwrap();
        let id = message["id"].clone();
        let result = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let replies = match message["method"].as_str() {
            Some("initialize") => vec![result(json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {"listChanged": true}, "resources": {}, "prompts": {}},
                "serverInfo": {"name": "fake", "version": "1"},
            }))],
            Some("notifications/initialized") => {
                writer.write_all(b"server ready (log line on stdout)\n").await.unwrap();
                vec![json!({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})]
            }
            Some("notifications/cancelled") => {
                fake.cancelled.lock().unwrap().push(message["params"]["requestId"].as_u64().unwrap());
                Vec::new()
            }
            // 分两页返回。
            Some("tools/list") => {
                let tools = fake.tools.lock().unwrap().clone();
                let (first, rest) = tools.split_at(tools.len().min(1));
                if message["params"]["cursor"].is_null() && !rest.is_empty() {
                    vec![result(json!({"tools": first, "nextCursor": "page-2"}))]
                } else if message["params"]["cursor"].is_null() {
                    vec![result(json!({"tools": first}))]
                } else {
                    vec![result(json!({"tools": rest}))]
                }
            }
            Some("tools/call") => match message["params"]["name"].as_str() {
                Some("echo") => vec![result(json!({"content": [
                    {"type": "text", "text": message["params"]["arguments"]["text"]},
                    {"type": "image", "data": PNG, "mimeType": "image/png"},
                    {"type": "image", "data": "…", "mimeType": "image/png"},
                    {"type": "resource", "resource": {"uri": "memo://a", "text": "memo body"}},
                ]}))],
                Some("fail") => vec![result(json!({"content": [{"type": "text", "text": "disk full"}], "isError": true}))],
                Some("slow") => {
                    let token = message["params"]["_meta"]["progressToken"].clone();
                    vec![json!({"jsonrpc": "2.0", "method": "notifications/progress",
                                "params": {"progressToken": token, "progress": 1, "total": 4, "message": "warming up"}})]
                }
                _ => vec![json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "Unknown tool"}})],
            },
            Some("resources/list") => vec![result(json!({"resources": [{"uri": "memo://a", "name": "A", "description": "a memo"}]}))],
            Some("resources/read") => vec![result(json!({"contents": [{"uri": message["params"]["uri"], "text": "memo body"}]}))],
            Some("prompts/list") => vec![result(json!({"prompts": [
                {"name": "review", "description": "Review a file", "arguments": [{"name": "file", "required": true}]},
            ]}))],
            Some("prompts/get") => vec![result(json!({"messages": [
                {"role": "user", "content": {"type": "text", "text": format!("Review {}", message["params"]["arguments"]["file"].as_str().unwrap())}},
            ]}))],
            // 客户端对 ping 的回应等。
            _ => Vec::new(),
        };
        for reply in replies {
            writer.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
        }
    }
}

struct FakeConnector(Arc<Fake>);

#[async_trait::async_trait]
impl Connector for FakeConnector {
    async fn connect(&self, _launch: &Launch, _logs: &Arc<LogBuffer>) -> Result<Connection, McpError> {
        self.0.connects.fetch_add(1, Ordering::SeqCst);
        if self.0.fail_next.load(Ordering::SeqCst) > 0 {
            self.0.fail_next.fetch_sub(1, Ordering::SeqCst);
            return Err(McpError::Unreachable("server is down".into()));
        }
        Ok(Connection { client: self.0.connect(), process: None })
    }
}

fn fake_set(fake: &Arc<Fake>, names: &[&str]) -> ServerSet {
    let set = ServerSet::with_connector(None, Arc::new(FakeConnector(fake.clone())));
    set.configure(names.iter().map(|name| (name.to_string(), ServerConfig::command("fake", Vec::new()))).collect());
    set
}

fn names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
    tools.iter().map(|tool| tool.name().to_owned()).collect()
}

type Recorded<T> = Arc<Mutex<T>>;
type Events = Recorded<Vec<(String, Value)>>;

/// 带实时输出和事件收集的工具环境。
fn recording_context() -> (ToolContext, Recorded<String>, Events) {
    let output = Arc::new(Mutex::new(String::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut ctx = ToolContext::new(PathBuf::from("/ws"));
    let sink = output.clone();
    ctx.output = Some(OutputSink(Arc::new(move |text: &str| sink.lock().unwrap().push_str(text))));
    let sink = events.clone();
    ctx.events = Some(EventSink(Arc::new(move |kind: String, data: Value| sink.lock().unwrap().push((kind, data)))));
    (ctx, output, events)
}

/// 等到条件成立（最多 5 秒）。
async fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met in time");
}

#[tokio::test]
async fn handshake_lists_and_calls_tools() {
    let fake = Fake::new();
    let client = fake.connect();
    client.initialize().await.unwrap();
    assert!(client.has_capability("resources") && client.has_capability("prompts"));

    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["echo", "fail"]);
    assert_eq!(tools[0].description, "Echo the text back.");

    let result = client.call_tool("echo", json!({"text": "你好"})).await.unwrap();
    assert_eq!(result.text, "你好\n[image content omitted]\n[resource memo://a]\nmemo body");
    assert_eq!(result.images, [ImageContent { media_type: "image/png".into(), data: PNG.into() }]);
    let result = client.call_tool("fail", json!({})).await.unwrap();
    assert_eq!((result.text.as_str(), result.is_error), ("disk full", true));
    assert!(matches!(client.call_tool("nope", json!({})).await, Err(McpError::Rpc(message)) if message == "Unknown tool"));
}

#[tokio::test]
async fn closed_connection_fails_pending_requests() {
    let (ours, theirs) = tokio::io::duplex(1024);
    let (reader, writer) = tokio::io::split(ours);
    let client = Client::new(reader, writer);
    drop(theirs);
    assert!(matches!(client.initialize().await, Err(McpError::Closed | McpError::Io(_))));
    eventually(|| client.is_closed()).await;
}

#[tokio::test]
async fn resources_and_prompts() {
    let fake = Fake::new();
    let client = fake.connect();
    client.initialize().await.unwrap();
    let resources = client.list_resources().await.unwrap();
    assert_eq!((resources[0].uri.as_str(), resources[0].name.as_str()), ("memo://a", "A"));
    let contents = client.read_resource("memo://a").await.unwrap();
    assert_eq!(contents[0].text.as_deref(), Some("memo body"));
    let prompts = client.list_prompts().await.unwrap();
    assert_eq!(prompts[0].name, "review");
    assert!(prompts[0].arguments[0].required);
    let mut arguments = serde_json::Map::new();
    arguments.insert("file".into(), json!("a.rs"));
    assert_eq!(client.get_prompt("review", &arguments).await.unwrap(), "Review a.rs");
}

#[tokio::test]
async fn server_tools_become_ordinary_tools() {
    let fake = Fake::new();
    let set = fake_set(&fake, &["my server"]);
    let tools = set.tools().await;
    // 服务器提供资源：多出两个读资源的工具。
    assert_eq!(names(&tools), ["mcp__my_server__echo", "mcp__my_server__fail", "mcp_list_resources", "mcp_read_resource"]);
    let (ctx, _, events) = recording_context();

    let echo = &tools[0];
    assert_eq!(echo.access(&json!({}), &ctx), Access::External);
    assert_eq!(echo.input_schema()["required"], json!(["text"]));
    let output = echo.execute(json!({"text": "hi"}), &ctx).await;
    assert_eq!(output, ToolOutput::ok("hi\n[image content omitted]\n[resource memo://a]\nmemo body"));
    // 图片写成 `media/images` 事件，和 JS 插件交回的图片一样进上下文。
    let events = events.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, dshr_core::MEDIA_IMAGES);
    assert_eq!(events[0].1["images"][0], json!({"media_type": "image/png", "data": PNG}));

    // 没有说明和 schema 的工具补上默认值；服务器报告的错误原样交给模型。
    let fail = &tools[1];
    assert!(fail.description().contains("my server"));
    assert_eq!(fail.input_schema(), json!({"type": "object", "properties": {}}));
    assert_eq!(fail.execute(json!({}), &ctx).await, ToolOutput::error("disk full"));

    // 资源工具跨组找服务器。
    let list = tools[2].execute(json!({"server": "my server"}), &ctx).await;
    assert!(list.content.contains("memo://a (A): a memo"), "{}", list.content);
    let read = tools[3].execute(json!({"server": "my server", "uri": "memo://a"}), &ctx).await;
    assert_eq!(read, ToolOutput::ok("memo body"));
    // prompts 跨组可见。
    assert!(servers::all_prompts().iter().any(|(server, prompt)| server == "my server" && prompt.name == "review"));
}

#[tokio::test]
async fn progress_is_shown_and_abandoned_calls_are_cancelled() {
    let fake = Fake::new();
    fake.tools.lock().unwrap().push(json!({"name": "slow"}));
    let set = ServerSet::with_connector(None, Arc::new(FakeConnector(fake.clone())));
    let mut config = ServerConfig::command("fake", Vec::new());
    config.timeout_ms = Some(300);
    set.configure(BTreeMap::from([("s".to_owned(), config)]));
    let tools = set.tools().await;
    let slow = tools.iter().find(|tool| tool.name() == "mcp__s__slow").unwrap();
    let (ctx, output, _) = recording_context();
    let result = slow.execute(json!({}), &ctx).await;
    assert!(result.is_error && result.content.contains("超时"), "{result:?}");
    assert_eq!(output.lock().unwrap().as_str(), "[25%] warming up\n");
    // 超时放弃的调用：告诉服务器不用再做了。
    eventually(|| !fake.cancelled.lock().unwrap().is_empty()).await;
}

#[tokio::test]
async fn tool_list_changes_are_picked_up() {
    let fake = Fake::new();
    let set = fake_set(&fake, &["s"]);
    assert_eq!(set.tools().await.len(), 4);
    fake.tools.lock().unwrap().push(json!({"name": "added"}));
    fake.push(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
    let set2 = set.clone();
    for _ in 0..500 {
        if names(&set2.tools().await).contains(&"mcp__s__added".to_owned()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(names(&set.tools().await).contains(&"mcp__s__added".to_owned()));
    assert_eq!(fake.connects.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn disconnected_servers_reconnect_with_backoff() {
    let fake = Fake::new();
    let set = fake_set(&fake, &["s"]);
    assert_eq!(set.tools().await.len(), 4);
    let tool = set.tools().await.into_iter().next().unwrap();

    fake.disconnect_all();
    eventually(|| set.status()[0].state == ServerState::Failed).await;
    // 断开期间：工具还在快照里，调用时报一句说明；这一步不等重连。
    let (ctx, _, _) = recording_context();
    assert!(tool.execute(json!({"text": "x"}), &ctx).await.content.contains("没有连接"));
    assert!(set.tools().await.is_empty());

    // 1 秒后在后台重连上，同一个工具对象又能用了。
    tokio::time::sleep(Duration::from_millis(1500)).await;
    eventually(|| set.status()[0].state == ServerState::Ready).await;
    assert_eq!(fake.connects.load(Ordering::SeqCst), 2);
    assert_eq!(tool.execute(json!({"text": "back"}), &ctx).await.content.lines().next(), Some("back"));
}

#[tokio::test(start_paused = true)]
async fn repeated_failures_stop_until_reconnected_by_hand() {
    let fake = Fake::new();
    fake.fail_next.store(100, Ordering::SeqCst);
    let set = fake_set(&fake, &["s"]);
    assert!(set.tools().await.is_empty());
    assert_eq!(set.status()[0].state, ServerState::Failed);
    // 1 + 2 + 4 + 8 秒后是第 5 次失败：停下。
    tokio::time::sleep(Duration::from_secs(20)).await;
    eventually(|| set.status()[0].state == ServerState::Stopped).await;
    assert_eq!(fake.connects.load(Ordering::SeqCst), MAX_ATTEMPTS as usize);
    assert!(set.status()[0].error.as_deref().unwrap().contains("server is down"));
    assert!(set.logs("s").iter().any(|line| line.contains("不再重连")));
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(fake.connects.load(Ordering::SeqCst), MAX_ATTEMPTS as usize);

    // 手动重连：从头算。
    fake.fail_next.store(0, Ordering::SeqCst);
    set.reconnect("s").unwrap();
    eventually(|| set.status()[0].state == ServerState::Ready).await;
}

#[tokio::test]
async fn reconfiguring_only_touches_changed_servers() {
    let fake = Fake::new();
    let set = fake_set(&fake, &["a", "b"]);
    assert_eq!(set.tools().await.len(), 6);
    assert_eq!(fake.connects.load(Ordering::SeqCst), 2);

    // b 改了参数、新增 c、a 不变：只连 b 和 c。
    let mut servers = BTreeMap::new();
    servers.insert("a".to_owned(), ServerConfig::command("fake", Vec::new()));
    servers.insert("b".to_owned(), ServerConfig::command("fake", vec!["--v2".into()]));
    servers.insert("c".to_owned(), ServerConfig::command("fake", Vec::new()));
    set.configure(servers.clone());
    assert_eq!(set.tools().await.len(), 8);
    assert_eq!(fake.connects.load(Ordering::SeqCst), 4);

    // 停用 b、删掉 c。
    servers.get_mut("b").unwrap().enabled = false;
    servers.remove("c");
    set.configure(servers);
    let tools = set.tools().await;
    assert_eq!(names(&tools), ["mcp__a__echo", "mcp__a__fail", "mcp_list_resources", "mcp_read_resource"]);
    let states: Vec<ServerState> = set.status().into_iter().map(|status| status.state).collect();
    assert_eq!(states, [ServerState::Ready, ServerState::Disabled]);
    assert_eq!(fake.connects.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn missing_command_fails_without_blocking() {
    let set = ServerSet::new(None);
    set.configure(BTreeMap::from([("broken".to_owned(), ServerConfig::command("dshr-no-such-mcp-server", Vec::new()))]));
    assert!(set.tools().await.is_empty());
    let status = &set.status()[0];
    assert_eq!(status.state, ServerState::Failed);
    assert!(status.error.as_deref().unwrap().contains("dshr-no-such-mcp-server"));
}

#[tokio::test]
async fn invalid_config_is_stopped_with_a_reason() {
    let set = ServerSet::new(None);
    let mut both = ServerConfig::command("x", Vec::new());
    both.url = Some("https://example.com/mcp".into());
    set.configure(BTreeMap::from([("both".to_owned(), both), ("neither".to_owned(), ServerConfig::command("", Vec::new()))]));
    assert!(set.tools().await.is_empty());
    let errors: Vec<String> = set.status().into_iter().map(|status| status.error.unwrap()).collect();
    assert!(errors[0].contains("只能写一个") && errors[1].contains("要写"), "{errors:?}");
}

#[test]
fn tool_names_are_safe_for_model_apis() {
    assert_eq!(tool_name("files", "read_file"), "mcp__files__read_file");
    assert_eq!(tool_name("我的 server", "a.b/c"), "mcp_____server__a_b_c");
    let long = tool_name("s", &"x".repeat(100));
    assert_eq!(long.len(), MAX_TOOL_NAME_FOR_TESTS);
    // 截断后加了哈希：前缀相同的两个长名字不会撞。
    assert_ne!(long, tool_name("s", &format!("{}y", "x".repeat(100))));
}

const MAX_TOOL_NAME_FOR_TESTS: usize = 64;

#[test]
fn config_parses_from_plugin_table() {
    let config: Config = serde_json::from_value(json!({
        "servers": {"files": {"command": "npx", "args": ["-y", "pkg"], "env": {"TOKEN": "t"}},
                    "off": {"command": "x", "enabled": false, "timeout_ms": 500},
                    "remote": {"url": "https://mcp.example.com/mcp", "headers": {"Authorization": "Bearer ${env:T}"}}},
    }))
    .unwrap();
    assert_eq!(config.servers["files"].args, ["-y", "pkg"]);
    assert!(config.servers["files"].enabled);
    assert!(!config.servers["off"].enabled);
    assert_eq!(config.servers["off"].timeout_ms, Some(500));
    assert_eq!(config.servers["remote"].describe(), "https://mcp.example.com/mcp");
    assert_eq!(config.servers["files"].describe(), "npx -y pkg");
    assert_eq!(config.servers["remote"].placeholders(), [("env".to_owned(), "T".to_owned())]);
}

#[test]
fn placeholders_are_expanded() {
    let lookup = |kind: &str, name: &str| -> Result<String, String> {
        match (kind, name) {
            ("env", "HOME") => Ok("/home/me".into()),
            ("secret", "TOKEN") => Ok("s3cret".into()),
            ("plugin", "") => Ok("/plugins/x".into()),
            _ => Err(format!("unknown {kind}:{name}")),
        }
    };
    assert_eq!(config::expand("${plugin}/server.js", &lookup).unwrap(), "/plugins/x/server.js");
    assert_eq!(config::expand("Bearer ${secret:TOKEN}", &lookup).unwrap(), "Bearer s3cret");
    assert_eq!(config::expand("$${env:HOME} is ${env:HOME}", &lookup).unwrap(), "${env:HOME} is /home/me");
    assert!(config::expand("${env:NOPE}", &lookup).unwrap_err().contains("unknown"));
    assert!(config::expand("${env:HOME", &lookup).is_err());

    let mut server = ServerConfig::http("https://x/${env:HOME}");
    server.headers.insert("Authorization".into(), "Bearer ${secret:TOKEN}".into());
    assert_eq!(
        server.launch(&lookup).unwrap(),
        Launch::Http { url: "https://x//home/me".into(), headers: vec![("Authorization".into(), "Bearer s3cret".into())] }
    );
}

/// 一个很小的 HTTP 服务器：每个连接交给 `handler(方法, 请求头, 正文)`，它返回完整的响应。
fn http_server(handler: impl Fn(&str, &str, &str) -> String + Send + Sync + 'static) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = handler.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head, body) = loop {
                    let read = stream.read(&mut chunk).unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buffer).into_owned();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
                    let length = head
                        .lines()
                        .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                        .unwrap_or(0usize);
                    if body.len() >= length {
                        break (head.to_owned(), body.to_owned());
                    }
                };
                let method = head.split(' ').next().unwrap_or_default().to_owned();
                let _ = stream.write_all(handler(&method, &head.to_ascii_lowercase(), &body).as_bytes());
            });
        }
    });
    port
}

fn response(status: &str, headers: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len())
}

/// HTTP 上的假服务器：`initialize` 用 JSON 回并给一个会话 id；`tools/list` 用 SSE 回；没带会话 id 或会话被作废时 404；
/// GET 回 405（不支持长连接）。
fn http_fake(sessions: Arc<AtomicUsize>, expired: Arc<Mutex<bool>>) -> u16 {
    http_server(move |method, head, body| {
        if method == "GET" {
            return response("405 Method Not Allowed", "", "");
        }
        if method == "DELETE" {
            return response("200 OK", "", "");
        }
        let message: Value = serde_json::from_str(body).unwrap();
        let session = format!("session-{}", sessions.load(Ordering::SeqCst));
        let id = message["id"].clone();
        match message["method"].as_str() {
            Some("initialize") => {
                let session = format!("session-{}", sessions.fetch_add(1, Ordering::SeqCst) + 1);
                *expired.lock().unwrap() = false;
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "http", "version": "1"}}});
                response("200 OK", &format!("Content-Type: application/json\r\nMcp-Session-Id: {session}\r\n"), &reply.to_string())
            }
            _ if *expired.lock().unwrap() || !head.contains(&format!("mcp-session-id: {session}")) => response("404 Not Found", "", ""),
            _ if !head.contains("mcp-protocol-version: 2025-06-18") => response("400 Bad Request", "", "missing version"),
            Some("notifications/initialized") => response("202 Accepted", "", ""),
            Some("tools/list") => {
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "remote_echo"}]}});
                let events = format!(": keep-alive\n\nevent: message\ndata: {reply}\n\n");
                response("200 OK", "Content-Type: text/event-stream\r\n", &events)
            }
            Some("tools/call") => {
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": "from http"}]}});
                response("200 OK", "Content-Type: application/json\r\n", &reply.to_string())
            }
            _ => response("400 Bad Request", "", ""),
        }
    })
}

#[tokio::test]
async fn streamable_http_with_sessions_and_reinitialization() {
    let (sessions, expired) = (Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(false)));
    let port = http_fake(sessions.clone(), expired.clone());
    let set = ServerSet::new(None);
    set.configure(BTreeMap::from([("web".to_owned(), ServerConfig::http(format!("http://127.0.0.1:{port}/mcp")))]));
    let tools = set.tools().await;
    assert_eq!(names(&tools), ["mcp__web__remote_echo"]);
    let (ctx, _, _) = recording_context();
    assert_eq!(tools[0].execute(json!({}), &ctx).await, ToolOutput::ok("from http"));

    // 会话被服务器作废：这次调用报断开，后台重新初始化，拿到新的会话。
    *expired.lock().unwrap() = true;
    assert!(tools[0].execute(json!({}), &ctx).await.content.contains("断开"));
    eventually(|| set.status()[0].state == ServerState::Ready && sessions.load(Ordering::SeqCst) == 2).await;
    assert_eq!(tools[0].execute(json!({}), &ctx).await, ToolOutput::ok("from http"));
}

#[tokio::test]
async fn http_errors_are_reported() {
    let port = http_server(|_, _, _| response("401 Unauthorized", "WWW-Authenticate: Bearer resource_metadata=\"x\"\r\n", ""));
    let set = ServerSet::new(None);
    set.configure(BTreeMap::from([("locked".to_owned(), ServerConfig::http(format!("http://127.0.0.1:{port}/mcp")))]));
    assert!(set.tools().await.is_empty());
    // 401 是“要登录”：不退避重连，停下等登录。
    let status = &set.status()[0];
    assert_eq!(status.state, ServerState::NeedsLogin);
    assert!(status.error.as_deref().unwrap().contains("dshr mcp login locked"));

    let port = http_server(|_, _, _| response("500 Internal Server Error", "", "boom"));
    let set = ServerSet::new(None);
    set.configure(BTreeMap::from([("broken-http".to_owned(), ServerConfig::http(format!("http://127.0.0.1:{port}/mcp")))]));
    assert!(set.tools().await.is_empty());
    assert_eq!(set.status()[0].state, ServerState::Failed);
    assert!(set.status()[0].error.as_deref().unwrap().contains("500"));
}

/// 要求 OAuth 的远程服务器，连同它的授权服务器：没带 token 时 401；登录走资源元数据 → 授权服务器元数据 → 动态注册 →
/// 授权（测试里的“浏览器”直接跳回本机回调）→ 换 token（1 秒后过期）；连接时 token 快过期，先刷新再用。
#[tokio::test]
async fn oauth_login_registers_authorizes_and_refreshes() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    let port = Arc::new(std::sync::OnceLock::<u16>::new());
    let base_port = port.clone();
    let actual = http_server(move |method, head, body| {
        let base = format!("http://127.0.0.1:{}", base_port.get().copied().unwrap_or(0));
        let path = head.lines().next().and_then(|line| line.split(' ').nth(1)).unwrap_or_default().to_owned();
        log.lock().unwrap().push(format!("{method} {path}"));
        let json = |value: Value| response("200 OK", "Content-Type: application/json\r\n", &value.to_string());
        match (method, path.as_str()) {
            ("GET", "/.well-known/oauth-protected-resource/mcp") | ("GET", "/.well-known/oauth-protected-resource") => {
                json(json!({ "resource": format!("{base}/mcp"), "authorization_servers": [base] }))
            }
            ("GET", "/.well-known/oauth-authorization-server") => json(json!({
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
            })),
            ("POST", "/register") => json(json!({ "client_id": "client-1" })),
            ("POST", "/token") if body.contains("grant_type=authorization_code") => {
                assert!(body.contains("code=c1") && body.contains("code_verifier=") && body.contains("client_id=client-1"), "{body}");
                assert!(body.contains("resource="), "token request carries the resource: {body}");
                json(json!({ "access_token": "token-1", "refresh_token": "r1", "expires_in": 1 }))
            }
            ("POST", "/token") if body.contains("grant_type=refresh_token") => {
                assert!(body.contains("refresh_token=r1"), "{body}");
                json(json!({ "access_token": "token-2", "expires_in": 3600 }))
            }
            ("GET", "/mcp") => response("405 Method Not Allowed", "", ""),
            ("POST", "/mcp") => {
                if !head.contains("authorization: bearer token-2") {
                    let challenge =
                        format!("WWW-Authenticate: Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\"\r\n");
                    return response("401 Unauthorized", &challenge, "");
                }
                let message: Value = serde_json::from_str(body).unwrap();
                let id = message["id"].clone();
                match message["method"].as_str() {
                    Some("initialize") => json(json!({"jsonrpc": "2.0", "id": id, "result": {
                        "protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "secure", "version": "1"}}})),
                    Some("tools/list") => json(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "secret_tool"}]}})),
                    _ => response("202 Accepted", "", ""),
                }
            }
            _ => response("404 Not Found", "", ""),
        }
    });
    port.set(actual).unwrap();

    let set = ServerSet::new(None);
    set.configure(BTreeMap::from([("secure-test".to_owned(), ServerConfig::http(format!("http://127.0.0.1:{actual}/mcp")))]));
    assert!(set.tools().await.is_empty());
    let status = &set.status()[0];
    assert_eq!(status.state, ServerState::NeedsLogin);
    assert!(status.error.as_deref().unwrap().contains("dshr mcp login secure-test"));

    // “浏览器”：看一眼授权地址，然后带着 code 跳回本机回调。
    let authorize_url = Arc::new(Mutex::new(String::new()));
    let opened = authorize_url.clone();
    let open = move |url: &str| -> Result<(), String> {
        *opened.lock().unwrap() = url.to_owned();
        let url = reqwest::Url::parse(url).unwrap();
        let param = |key: &str| url.query_pairs().find(|(name, _)| name == key).map(|(_, value)| value.into_owned()).unwrap();
        let callback = reqwest::Url::parse(&format!("{}?code=c1&state={}", param("redirect_uri"), param("state"))).unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap())).unwrap();
            let request = format!("GET {}?{} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", callback.path(), callback.query().unwrap());
            stream.write_all(request.as_bytes()).unwrap();
            let mut reply = String::new();
            let _ = stream.read_to_string(&mut reply);
        });
        Ok(())
    };
    set.login("secure-test", &open).await.unwrap();
    let url = authorize_url.lock().unwrap().clone();
    assert!(url.contains("client_id=client-1") && url.contains("code_challenge_method=S256") && url.contains("resource="), "{url}");

    // 登录后重连：token-1 快过期，先换成 token-2 再连。
    eventually(|| set.status()[0].state == ServerState::Ready).await;
    assert_eq!(names(&set.tools().await), ["mcp__secure-test__secret_tool"]);
    assert_eq!(oauth::load_token("secure-test").unwrap().access_token, "token-2");
    assert!(set.status()[0].signed_in);
    let requests = seen.lock().unwrap().clone();
    assert!(requests.contains(&"POST /register".to_owned()), "{requests:?}");

    // 退出登录：token 删掉，又要登录。
    set.logout("secure-test").unwrap();
    eventually(|| set.status()[0].state == ServerState::NeedsLogin).await;
    assert!(oauth::load_token("secure-test").is_none());
}
