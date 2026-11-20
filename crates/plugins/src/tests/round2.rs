//! 第二轮接口的测试：会话控制、子代理、审查钩子、上下文压缩与改写、预设与路由、流式网络与 WebSocket、
//! 常驻服务、工具图片、宿主数据、安装、OAuth、插件服务商。

use super::*;

use std::collections::VecDeque;
use std::io::{Read as _, Write as _};

use dshr_agent::{ApprovalRequest, Approver, Decision, DelegatePlugin, ModelChoices, PluginPreset};

/// 一个很小的 HTTP 服务器：每个连接交给 `handler(方法, 路径, 请求头, 正文)`，它返回完整的响应文本。
fn http_server(handler: impl Fn(&str, &str, &str, &str) -> Vec<u8> + Send + Sync + 'static) -> u16 {
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
                let mut first = head.lines().next().unwrap_or_default().split(' ');
                let (method, path) = (first.next().unwrap_or_default().to_owned(), first.next().unwrap_or_default().to_owned());
                let response = handler(&method, &path, &head, &body);
                let _ = stream.write_all(&response);
            });
        }
    });
    port
}

fn respond(status: &str, extra: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}", body.len()).into_bytes()
}

/// 一直拒绝、并数被问了几次的审批。
#[derive(Default)]
struct CountingDeny(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl Approver for CountingDeny {
    async fn approve(&self, _request: &ApprovalRequest) -> Decision {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Decision::Deny
    }
}

struct Run {
    session: Session,
    provider: Arc<ScriptedProvider>,
}

/// 跑一轮：`plugins` 已经装好，模型依次给出 `replies`。
async fn run(
    f: &Fixture,
    plugins: Context,
    replies: Vec<Vec<dshr_llm::StreamEvent>>,
    approver: Arc<dyn Approver>,
    config: AgentConfig,
) -> Run {
    let provider = Arc::new(ScriptedProvider::new(replies));
    let agent = Agent::new(provider.clone(), plugins, approver, f.tool_context(), config);
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut session = Session::create(format!("s{run}"), f.home.path().join(format!("log-{run}")).join("session.v1.jsonl")).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut session, "go".into(), &tx, &CancellationToken::new()).await.unwrap();
    Run { session, provider }
}

fn agent_context(f: &Fixture) -> Context {
    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    plugins.add_group(AgentPlugins).unwrap();
    plugins.add_group(JsPlugins::discover(&f.plugins_dir())).unwrap();
    plugins
}

fn tool_results(session: &Session) -> Vec<(String, bool)> {
    session
        .events()
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolResult { output, is_error, .. } => Some((output.to_string(), *is_error)),
            _ => None,
        })
        .collect()
}

/// 测试里用的会话控制：记下收到的消息。
#[derive(Default)]
struct RecordingSessions(Mutex<Vec<(String, String)>>);

impl crate::app::SessionControl for RecordingSessions {
    fn send(&self, request: crate::app::SendRequest) -> Result<String, String> {
        self.0.lock().unwrap().push((request.plugin, request.text));
        Ok("current".into())
    }

    fn stop(&self, _session: Option<String>) -> Result<(), String> {
        Ok(())
    }

    fn list(&self) -> Vec<crate::app::OpenSession> {
        Vec::new()
    }
}

#[tokio::test]
async fn session_control_sends_with_rate_limit() {
    let recorder = Arc::new(RecordingSessions::default());
    let shared = recorder.clone();
    crate::app::set_app_services(|services| services.session = Some(shared));
    let f = Fixture::new();
    f.plugin(
        "im-bridge",
        "capabilities = [\"session:control\"]\n",
        r#"export const tools = [{ name: "relay", async execute({ text }, host) {
            await host.session.send(text);
            try { await host.session.send("again"); return "no limit"; } catch (error) { return error.message; }
        } }, { name: "relay_ro", readOnly: true, async execute(_input, host) {
            try { await host.session.send("x"); return "sent"; } catch (error) { return error.message; }
        } }];"#,
    );
    f.plugin(
        "quiet",
        "",
        r#"export const tools = [{ name: "quiet_send", async execute(_i, host) {
        try { await host.session.send("x"); return "sent"; } catch (error) { return error.message; } } }];"#,
    );
    let cx = f.context();
    let output = f.call(&cx, "relay", json!({ "text": "来自群聊的任务" })).await;
    assert!(output.content.contains("at most one message"), "{output:?}");
    assert!(recorder.0.lock().unwrap().contains(&("im-bridge".to_owned(), "来自群聊的任务".to_owned())));
    assert!(f.call(&cx, "relay_ro", json!({})).await.content.contains("not declared readOnly"));
    assert!(f.call(&cx, "quiet_send", json!({})).await.content.contains("`session:control`"));
    // 能替用户发消息的工具按“交给外部”把关。
    assert_eq!(f.tool(&cx, "relay").access(&json!({}), &f.tool_context()), Access::External);
}

#[tokio::test]
async fn sub_agents_report_back() {
    let f = Fixture::new();
    f.plugin(
        "team",
        "capabilities = [\"agent:run\"]\n",
        r#"export const tools = [{ name: "dispatch", async execute({ task }, host) {
            const report = await host.agent.run({ task, preset: "review" });
            return `lead got: ${report}`;
        } }];"#,
    );
    let sub = Context::new();
    sub.add_group(dshr_tools::BuiltinTools).unwrap();
    sub.add_group(AgentPlugins).unwrap();
    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    plugins.add_group(AgentPlugins).unwrap();
    plugins.add(DelegatePlugin::new(sub)).unwrap();
    plugins.add_group(JsPlugins::discover(&f.plugins_dir())).unwrap();
    let replies = vec![
        ScriptedProvider::tool_reply("c1", "dispatch", json!({ "task": "check the README" })),
        // 子代理的回复。
        ScriptedProvider::text_reply("README is fine"),
        ScriptedProvider::text_reply("done"),
    ];
    let result = run(&f, plugins, replies, Arc::new(AutoApprove), AgentConfig::default()).await;
    assert_eq!(tool_results(&result.session)[0], ("lead got: README is fine".to_owned(), false));
    // 子代理的请求带着“审查模式”的说明。
    assert!(result.provider.requests()[1].system.as_deref().unwrap_or_default().contains("Review mode"));
}

#[tokio::test]
async fn review_hook_decides_before_asking() {
    let f = Fixture::new();
    f.plugin(
        "reviewer",
        "capabilities = [\"hooks:review\"]\n",
        r#"export const hooks = {
          reviewTool: async ({ tool, input }, host) => {
            await host.writeData("called", "yes");
            if (tool !== "run_shell") return undefined;
            return input.command.startsWith("echo") ? "allow" : { decision: "deny", reason: "only echo is allowed" };
          },
        };"#,
    );
    let approver = Arc::new(CountingDeny::default());
    let replies = vec![
        ScriptedProvider::tool_reply("c1", "run_shell", json!({ "command": "echo reviewed" })),
        ScriptedProvider::tool_reply("c2", "run_shell", json!({ "command": "rm -rf x" })),
        ScriptedProvider::text_reply("done"),
    ];
    let result = run(&f, agent_context(&f), replies, approver.clone(), AgentConfig::default().with(PermissionMode::Workspace)).await;
    let results = tool_results(&result.session);
    if !dshr_sandbox::confines_writes() {
        assert!(results[0].0.contains("reviewed") && !results[0].1, "{results:?}");
        assert_eq!(results[1], ("only echo is allowed".to_owned(), true));
        assert_eq!(approver.0.load(std::sync::atomic::Ordering::Relaxed), 0, "审查给了结论就不再询问");
        let kinds: Vec<String> = result.session.events().iter().map(|event| event.body.kind().to_owned()).collect();
        assert_eq!(kinds.iter().filter(|kind| *kind == "plugins/reviewed").count(), 2);
    }

    // 只读模式下命令本来就会被拒绝：不问审查钩子。
    let f = Fixture::new();
    f.plugin(
        "reviewer",
        "capabilities = [\"hooks:review\"]\n",
        r#"export const hooks = { reviewTool: async (_c, host) => { await host.writeData("called", "yes"); return "allow"; } };"#,
    );
    let replies =
        vec![ScriptedProvider::tool_reply("c1", "run_shell", json!({ "command": "echo x" })), ScriptedProvider::text_reply("done")];
    let result = run(&f, agent_context(&f), replies, Arc::new(AutoApprove), AgentConfig::default().with(dshr_agent::Preset::Plan)).await;
    assert!(tool_results(&result.session)[0].1);
    assert!(!f.home.path().join("plugin-data/reviewer/called").exists());
}

#[tokio::test]
async fn compaction_and_request_rewriting() {
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("a.txt"), "x".repeat(200)).unwrap();
    f.plugin(
        "squeeze",
        "capabilities = [\"context:compact\", \"context:rewrite\"]\n",
        r#"let compacted = false;
        export const hooks = {
          beforeStep: ({ step, lastSeq, estimatedTokens, contextWindow }) => {
            if (step < 2 || compacted) return undefined;
            compacted = true;
            return { compact: { summary: `SUMMARY (${estimatedTokens > 0}, ${contextWindow > 0})`, uptoSeq: lastSeq } };
          },
          transformRequest: ({ system, messages }) => ({ system: `${system}\n[rewritten ${messages.length}]` }),
        };"#,
    );
    f.plugin("noisy", "", r#"export const hooks = { transformRequest: () => ({ system: "HIJACKED" }) };"#);
    let replies = vec![ScriptedProvider::tool_reply("c1", "read_file", json!({ "path": "a.txt" })), ScriptedProvider::text_reply("done")];
    let result = run(&f, agent_context(&f), replies, Arc::new(AutoApprove), AgentConfig::default()).await;
    let requests = result.provider.requests();
    // 第二次请求：之前的对话（含工具结果）被摘要代替。
    let second = &requests[1];
    let first_text = match &second.messages[0].content[0] {
        dshr_core::ContentBlock::Text { text } => text.clone(),
        other => panic!("{other:?}"),
    };
    assert!(first_text.contains("SUMMARY (true, true)"), "{first_text}");
    assert!(
        !second
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, dshr_core::ContentBlock::ToolResult { .. }))
    );
    // 有 context:rewrite 的改写生效；没有能力的被忽略。
    let system = requests[0].system.as_deref().unwrap();
    assert!(system.contains("[rewritten 1]") && !system.contains("HIJACKED"), "{system}");
    assert!(result.session.events().iter().any(|event| matches!(event.body, EventBody::Compaction { .. })));
}

#[tokio::test]
async fn plugin_presets_and_model_routing() {
    let f = Fixture::new();
    f.plugin(
        "router",
        "capabilities = [\"model:route\"]\n",
        r#"export const presets = [{ id: "terse", label: "Terse", instructions: "Answer in one line.", tools: ["read_file"] }];
        export const hooks = { beforeRequest: ({ model }) => ({ append: "routed", model: model === "deepseek-flash" ? "deepseek-pro" : "unknown-model", effort: "low" }) };"#,
    );
    let plugins = agent_context(&f);
    let preset = plugins.snapshot().items::<dshr_agent::PluginPresets>().first().cloned().expect("插件预设");
    assert_eq!((preset.plugin.as_str(), preset.label.as_str()), ("router", "Terse"));
    let config = AgentConfig::default().with(ModelChoices(vec!["deepseek-flash".into(), "deepseek-pro".into()])).with(PluginPreset(preset));
    let result = run(&f, plugins, vec![ScriptedProvider::text_reply("ok")], Arc::new(AutoApprove), config).await;
    let request = &result.provider.requests()[0];
    assert_eq!(request.model, "deepseek-pro");
    assert_eq!(request.effort.map(|effort| effort.as_str()), Some("low"));
    let system = request.system.as_deref().unwrap();
    assert!(system.contains("Answer in one line.") && system.ends_with("routed"), "{system}");
    assert_eq!(request.tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["read_file"]);

    // 不在目录里的模型不换。
    let plugins = agent_context(&f);
    let config = AgentConfig { model: "deepseek-pro".into(), ..AgentConfig::default() }.with(ModelChoices(vec!["deepseek-pro".into()]));
    let result = run(&f, plugins, vec![ScriptedProvider::text_reply("ok")], Arc::new(AutoApprove), config).await;
    assert_eq!(result.provider.requests()[0].model, "deepseek-pro");
}

#[tokio::test]
async fn streaming_fetch_sse_and_websocket() {
    let port = http_server(|_, path, _, _| match path {
        "/sse" => {
            let body = "event: tick\ndata: 一\n\ndata: two\ndata: lines\n\n: comment\n\nid: 3\ndata: last\n\n";
            respond("200 OK", "Content-Type: text/event-stream\r\n", body)
        }
        "/big" => respond("200 OK", "", &"y".repeat(3 * 1024 * 1024)),
        _ => respond("404 Not Found", "", ""),
    });
    // WebSocket 回声服务器。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use futures::{SinkExt as _, StreamExt as _};
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else { return };
                while let Some(Ok(message)) = socket.next().await {
                    if message.is_close() {
                        break;
                    }
                    let _ = socket.send(message).await;
                }
            });
        }
    });
    let f = Fixture::new();
    f.plugin(
        "streams",
        "capabilities = [\"net:127.0.0.1\"]\ntimeout_ms = 10000\n",
        r#"export const tools = [{ name: "stream", async execute({ base, ws }, host) {
            const events = [];
            const sse = await host.fetch(`${base}/sse`, { stream: true });
            for await (const event of sse.events()) events.push([event.event, event.data, event.id ?? null]);
            const big = await host.fetch(`${base}/big`, { stream: true });
            let total = 0, chunks = 0;
            for await (const chunk of big.body) { total += chunk.length; chunks++; }
            const socket = await host.websocket(ws);
            await socket.send("hello");
            await socket.send(new Uint8Array([1, 2, 3]));
            const text = await socket.next();
            const bytes = await socket.next();
            await socket.close();
            return { events, total, many: chunks > 1, text, bytes: [...bytes.data] };
        } }];"#,
    );
    let cx = f.context();
    let output =
        f.call(&cx, "stream", json!({ "base": format!("http://127.0.0.1:{port}"), "ws": format!("ws://127.0.0.1:{ws_port}") })).await;
    assert!(!output.is_error, "{output:?}");
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["events"], json!([["tick", "一", null], ["message", "two\nlines", null], ["message", "last", "3"]]));
    // 流式读取没有 2 MB 的上限。
    assert_eq!((value["total"].as_u64(), value["many"].as_bool()), (Some(3 * 1024 * 1024), Some(true)));
    assert_eq!(value["text"], json!({ "type": "text", "data": "hello" }));
    assert_eq!(value["bytes"], json!([1, 2, 3]));
}

#[tokio::test]
async fn services_restart_with_backoff_and_need_background() {
    let f = Fixture::new();
    f.plugin(
        "watcher",
        "capabilities = [\"background\"]\n",
        r#"export const services = [{ id: "poll", run: async (host) => {
            const count = Number((await host.readData("count")) ?? "0") + 1;
            await host.writeData("count", String(count));
            throw new Error(`boom ${count}`);
        } }];"#,
    );
    f.plugin("lazy", "", r#"export const services = [{ id: "x", run: async () => {} }];"#);
    let scripts = JsPlugins::discover(&f.plugins_dir());
    let cx = Context::new();
    cx.add_group(dshr_tools::BuiltinTools).unwrap();
    cx.add_group(scripts.clone()).unwrap();
    assert!(failure(&cx, "lazy").contains("background"));
    // 第一次立刻跑，失败后 1 秒、再 2 秒重启。
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let count: u32 = std::fs::read_to_string(f.home.path().join("plugin-data/watcher/count")).unwrap().parse().unwrap();
    assert!(count >= 2, "count = {count}");
    let card = scripts.card("watcher").unwrap();
    assert!(
        card.services.iter().any(|(id, status)| id == "poll" && (status.contains("出错") || status.contains("运行中"))),
        "{:?}",
        card.services
    );
    assert!(card.provides.contains(&("services", 1)));
}

#[tokio::test]
async fn tool_images_reach_models_directly() {
    let f = Fixture::new();
    f.plugin(
        "camera",
        "",
        r#"export const tools = [{ name: "snap", readOnly: true, execute: () => ({ content: "took a picture", images: [{ data: new Uint8Array([137, 80, 78, 71]), mediaType: "image/png" }] }) }];"#,
    );
    let replies = || vec![ScriptedProvider::tool_reply("c1", "snap", json!({})), ScriptedProvider::text_reply("done")];
    let blocks = |run: &Run| -> Vec<dshr_core::ContentBlock> {
        run.provider.requests()[1].messages.iter().flat_map(|message| message.content.clone()).collect()
    };

    let run_result = run(&f, agent_context(&f), replies(), Arc::new(AutoApprove), AgentConfig::default()).await;
    assert_eq!(tool_results(&run_result.session)[0].0, "took a picture");
    assert!(run_result.session.events().iter().any(|event| event.body.kind() == dshr_core::MEDIA_IMAGES));
    let blocks = blocks(&run_result);
    assert!(blocks.iter().any(|block| matches!(block, dshr_core::ContentBlock::Image { source } if source.media_type == "image/png" && &*source.data == "iVBORw==")));
    assert!(!blocks.iter().any(|block| matches!(block, dshr_core::ContentBlock::Text { text } if text.contains("cannot see images"))));
}

#[tokio::test]
async fn host_data_access_is_scoped() {
    let f = Fixture::new();
    f.plugin(
        "keeper",
        "capabilities = [\"dshr:config:read\", \"dshr:config:write\", \"dshr:skills\", \"dshr:sessions:read\", \"dshr:sessions:write\", \"dshr:mcp\"]\n",
        r#"export const tools = [{ name: "manage", async execute(_input, host) {
            await host.dshr.config.patch({ ui: { language: "en" } });
            const config = await host.dshr.config.get();
            await host.dshr.skills.write("notes", "---\nname: notes\n---\nTake notes.");
            const imported = await host.dshr.sessions.import({
              title: "Imported", workspace: "/tmp/x",
              events: [{ type: "user/message", data: { text: "hi" } }],
            });
            const sessions = await host.dshr.sessions.list();
            const events = await host.dshr.sessions.read(imported, { last: 5 });
            await host.dshr.mcp.add("files", { command: "npx", args: ["-y", "server"] });
            let traversal = null;
            try { await host.dshr.sessions.read("../etc"); } catch (error) { traversal = error.message; }
            return {
              language: config.ui.language,
              skills: await host.dshr.skills.list(),
              skill: await host.dshr.skills.read("notes"),
              imported: sessions.some((session) => session.id === imported && session.title === "Imported"),
              kinds: events.map((event) => event.type),
              mcp: (await host.dshr.mcp.list()).map((server) => server.name),
              traversal,
            };
        } }, { name: "peek", readOnly: true, async execute(_input, host) {
            try { await host.dshr.config.patch({}); return "patched"; } catch (error) { return error.message; }
        } }];"#,
    );
    f.plugin(
        "nosy2",
        "",
        r#"export const tools = [{ name: "nosy2", async execute(_i, host) {
        try { await host.dshr.config.get(); return "read"; } catch (error) { return error.message; } } }];"#,
    );
    let cx = f.context();
    let output = f.call(&cx, "manage", json!({})).await;
    assert!(!output.is_error, "{output:?}");
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["language"], "en");
    assert_eq!(value["skills"], json!(["notes"]));
    assert!(value["skill"].as_str().unwrap().contains("Take notes."));
    assert_eq!(value["imported"], true);
    assert_eq!(value["kinds"], json!(["session/workspace", "session/title", "user/message"]));
    assert_eq!(value["mcp"], json!(["files"]));
    assert!(value["traversal"].as_str().unwrap().contains("invalid session id"));
    assert!(f.call(&cx, "peek", json!({})).await.content.contains("not declared readOnly"));
    assert!(f.call(&cx, "nosy2", json!({})).await.content.contains("dshr:config:read"));
}

/// 把一个插件文件夹打成 tar.gz（顶层一个 `repo-main/` 文件夹，像 GitHub 的源码包）。
fn tarball(files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
    for (path, content) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, format!("repo-main/{path}"), content.as_bytes()).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

#[tokio::test]
async fn install_asks_first_and_never_grants() {
    let archive = tarball(&[
        ("plugin.toml", "id = \"hello\"\nname = \"Hello\"\nversion = \"1.0.0\"\ncapabilities = [\"net:api.example.com\"]\n"),
        ("index.js", "export const tools = [{ name: \"hello\", execute: () => \"hi\" }];"),
    ]);
    let served = archive.clone();
    let port = http_server(move |_, path, _, _| {
        if path == "/hello.tar.gz" {
            let mut response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", served.len()).into_bytes();
            response.extend_from_slice(&served);
            response
        } else {
            respond("404 Not Found", "", "")
        }
    });
    let f = Fixture::new();
    let dir = f.plugins_dir();

    // 直接用安装函数：拒绝时什么都不留下；同意后装进去，但能力没有被允许。
    let staged = crate::install::stage_bytes(&archive, "hello.tar.gz", "test", None, None).unwrap();
    let request = staged.request(&dir, None);
    assert_eq!((request.id.as_str(), request.capabilities.clone()), ("hello", vec!["net:api.example.com".to_owned()]));
    drop(staged);
    assert!(!dir.join("hello").exists());
    let wrong = crate::install::stage_bytes(&archive, "hello.tar.gz", "test", None, Some("00"));
    assert!(wrong.err().unwrap().contains("SHA-256"));

    // 插件市场插件：每次都问；这里先拒绝，再同意。
    let answers = Arc::new(Mutex::new(VecDeque::from([false, true])));
    let asked = answers.clone();
    crate::app::set_app_services(|services| {
        services.install_prompt = Some(Arc::new(move |_request: &crate::app::InstallRequest| {
            if asked.lock().unwrap().pop_front().unwrap_or(false) {
                crate::app::InstallAnswer::Allow
            } else {
                crate::app::InstallAnswer::Deny
            }
        }));
    });
    f.plugin(
        "market",
        "capabilities = [\"plugins:install\", \"net:127.0.0.1\"]\ntimeout_ms = 10000\n",
        r#"export const tools = [{ name: "get_plugin", async execute({ url }, host) {
            try { return `installed ${await host.plugins.install({ url })}`; } catch (error) { return error.message; }
        } }, { name: "list_plugins", readOnly: true, async execute(_i, host) {
            return (await host.plugins.list()).map((plugin) => `${plugin.id}:${plugin.missing.length}`).join(",");
        } }];"#,
    );
    let cx = f.context();
    let url = format!("http://127.0.0.1:{port}/hello.tar.gz");
    assert!(f.call(&cx, "get_plugin", json!({ "url": url })).await.content.contains("declined"));
    assert!(!dir.join("hello").exists());
    assert_eq!(f.call(&cx, "get_plugin", json!({ "url": url })).await.content, "installed hello");
    assert!(dir.join("hello/index.js").is_file());
    // 装好的插件声明的能力还没有允许。
    assert!(f.call(&cx, "list_plugins", json!({})).await.content.contains("hello:1"));
    crate::install::uninstall(&dir, "hello").unwrap();
    assert!(!dir.join("hello").exists());
}

#[tokio::test]
async fn oauth_login_and_refresh() {
    let tokens = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = tokens.clone();
    let port = http_server(move |_, path, _, body| {
        if path != "/token" {
            return respond("404 Not Found", "", "");
        }
        seen.lock().unwrap().push(body.to_owned());
        let reply = if body.contains("grant_type=authorization_code") {
            json!({ "access_token": "first", "refresh_token": "r1", "expires_in": 1 })
        } else {
            json!({ "access_token": "second", "expires_in": 3600 })
        };
        respond("200 OK", "Content-Type: application/json\r\n", &reply.to_string())
    });
    // 测试里的“浏览器”：拿到授权页地址，直接带着 code 和 state 访问回调地址。
    crate::app::set_app_services(|services| {
        services.open_url = Some(Arc::new(|url: &str| {
            let url = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
            let param =
                |key: &str| url.query_pairs().find(|(name, _)| name == key).map(|(_, value)| value.into_owned()).unwrap_or_default();
            let callback = format!("{}?code=the-code&state={}", param("redirect_uri"), param("state"));
            std::thread::spawn(move || {
                let target = reqwest::Url::parse(&callback).unwrap();
                let mut stream = std::net::TcpStream::connect((target.host_str().unwrap(), target.port().unwrap())).unwrap();
                let _ = write!(stream, "GET {}?{} HTTP/1.1\r\nHost: x\r\n\r\n", target.path(), target.query().unwrap_or_default());
                let _ = stream.read(&mut [0u8; 1024]);
            });
            Ok(())
        }));
    });
    let f = Fixture::new();
    f.plugin(
        "login",
        "capabilities = [\"oauth\", \"net:127.0.0.1\"]\nsecrets = [\"ACCOUNT\"]\ntimeout_ms = 20000\n",
        &format!(
            r#"export const tools = [{{ name: "login", async execute(_i, host) {{
                const result = await host.oauth.authorize({{
                  authorizeUrl: "http://127.0.0.1:{port}/authorize", tokenUrl: "http://127.0.0.1:{port}/token",
                  clientId: "dshr-test", scopes: ["chat"], secret: "ACCOUNT",
                }});
                const first = await host.oauth.token("ACCOUNT");
                await new Promise((resolve) => setTimeout(resolve, 1100));
                return {{ hasExpiry: typeof result.expiresAt === "number", first, second: await host.oauth.token("ACCOUNT") }};
            }} }}];"#
        ),
    );
    let cx = f.context();
    let output = f.call(&cx, "login", json!({})).await;
    assert!(!output.is_error, "{output:?}");
    let value: Value = serde_json::from_str(&output.content).unwrap();
    // 第一个 token 1 秒后过期（在刷新余量之内），第二次取就用 refresh token 换了新的。
    assert_eq!(value, json!({ "hasExpiry": true, "first": "second", "second": "second" }));
    let bodies = tokens.lock().unwrap().clone();
    assert!(bodies[0].contains("code=the-code") && bodies[0].contains("code_verifier="), "{bodies:?}");
    assert!(bodies.iter().any(|body| body.contains("grant_type=refresh_token") && body.contains("refresh_token=r1")));
}

#[tokio::test]
async fn plugin_providers_add_their_auth() {
    let headers = Arc::new(Mutex::new(String::new()));
    let seen = headers.clone();
    let port = http_server(move |_, path, head, _| {
        if path == "/v1/chat/completions" {
            *seen.lock().unwrap() = head.to_ascii_lowercase();
            let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            return respond("200 OK", "Content-Type: text/event-stream\r\n", body);
        }
        respond("404 Not Found", "", "")
    });
    let f = Fixture::new();
    f.plugin(
        "gateway",
        "capabilities = [\"provider\", \"net:127.0.0.1\"]\n",
        &format!(
            r#"export const providers = [{{
              id: "work", name: "Work gateway", protocol: "openai", baseUrl: "http://127.0.0.1:{port}/v1",
              models: async () => ["m1", "m2"],
              auth: async () => ({{ apiKey: "token-123", headers: {{ "x-team": "dshr" }} }}),
            }}];"#
        ),
    );
    f.plugin(
        "bad-gateway",
        "capabilities = [\"provider\"]\n",
        r#"export const providers = [{ id: "x", name: "x", protocol: "openai", baseUrl: "https://elsewhere.example/v1", models: [] }];"#,
    );
    let cx = Context::new();
    cx.add_group(dshr_llm::BuiltinProviders).unwrap();
    cx.add_group(JsPlugins::discover(&f.plugins_dir())).unwrap();
    assert!(failure(&cx, "bad-gateway").contains("net:"));
    let config = dshr_config::ProviderConfig {
        id: "gateway-work".into(),
        adapter: dshr_config::PLUGIN_ADAPTER.into(),
        name: "Work gateway".into(),
        protocol: Some(dshr_config::Protocol::OpenAi),
        base_url: format!("http://127.0.0.1:{port}/v1"),
        models: Vec::new(),
        plugin: Some("gateway/work".into()),
        prompt_cache: None,
    };
    let resolved = config.resolve();
    let snapshot = cx.snapshot();
    let client = dshr_llm::HttpClient::default();
    let ids: Vec<String> =
        dshr_llm::list_models(&snapshot, &client, &resolved, None).await.unwrap().into_iter().map(|model| model.id).collect();
    assert_eq!(ids, ["m1", "m2"]);
    let provider = dshr_llm::connect(&snapshot, &client, &resolved, None).unwrap();
    let request = dshr_llm::ChatRequest {
        model: "m1".into(),
        system: None,
        messages: vec![dshr_core::Message { role: dshr_core::Role::User, content: vec![dshr_core::ContentBlock::text("hi")] }],
        tools: Vec::new(),
        max_tokens: None,
        effort: None,
    };
    let _ = provider.stream(request).await.map(|_| ());
    let head = headers.lock().unwrap().clone();
    assert!(head.contains("authorization: bearer token-123") && head.contains("x-team: dshr"), "{head}");
}
