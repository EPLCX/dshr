//! 第五轮接口的测试：会话的流式输出、`host.llm.stream`、运行时显隐入口、会话的模型与预设、
//! 带图片发消息、插件提供的权限模式、插件代为审批、完全访问下改写已授权的插件。

use super::*;

use dshr_agent::{ApprovalRequest, Approver, Decision, PluginPermission, PluginPermissions, Preset};
use dshr_core::StopReason;
use dshr_llm::{BlockKind, StreamEvent};

/// 会话控制是进程级的服务：改它的测试一个一个来。
pub(super) static SESSION_SERVICE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn agent_context(f: &Fixture) -> Context {
    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    plugins.add_group(AgentPlugins).unwrap();
    plugins.add_group(JsPlugins::discover(&f.plugins_dir())).unwrap();
    plugins
}

/// 一条分成几段流式输出的文字回复。
fn streamed_reply(parts: &[&str]) -> Vec<StreamEvent> {
    let mut events = vec![
        StreamEvent::MessageStart { model: Some("mock".into()), input_tokens: Some(1) },
        StreamEvent::BlockStart { index: 0, kind: BlockKind::Text },
    ];
    events.extend(parts.iter().map(|text| StreamEvent::TextDelta { index: 0, text: (*text).into() }));
    events.extend([
        StreamEvent::BlockStop { index: 0 },
        StreamEvent::MessageDelta { stop_reason: Some(StopReason::EndTurn), output_tokens: Some(3) },
        StreamEvent::MessageStop,
    ]);
    events
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

async fn run_turn(
    plugins: Context,
    f: &Fixture,
    replies: Vec<Vec<StreamEvent>>,
    approver: Arc<dyn Approver>,
    config: AgentConfig,
) -> Session {
    let provider = Arc::new(ScriptedProvider::new(replies));
    let agent = Agent::new(provider, plugins, approver, f.tool_context(), config);
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut session = Session::create(format!("r5-{run}"), f.home.path().join(format!("log5-{run}")).join("session.v1.jsonl")).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut session, "go".into(), &tx, &CancellationToken::new()).await.unwrap();
    session
}

#[tokio::test]
async fn llm_stream_yields_pieces_and_full_text() {
    let f = Fixture::new();
    f.plugin(
        "streamer",
        "capabilities = [\"llm\"]\n",
        r#"export const tools = [{ name: "pieces", async execute(_input, host) {
            const stream = host.llm.stream("hi");
            const parts = [];
            for await (const piece of stream) parts.push(`${piece.type}:${piece.text}`);
            return `${parts.join("|")} usage=${stream.usage === null ? "none" : "some"}`;
        } }, { name: "whole", async execute(_input, host) {
            return await host.llm.stream({ prompt: "hi" }).text();
        } }];"#,
    );
    f.plugin(
        "no-llm",
        "",
        r#"export const tools = [{ name: "sneaky", async execute(_i, host) {
        try { host.llm.stream("hi"); return "streamed"; } catch (error) { return error.message; }
    } }];"#,
    );
    let provider = Arc::new(ScriptedProvider::new([streamed_reply(&["Hel", "lo"]), streamed_reply(&["ab", "cd"])]));
    let cx = f.context();
    let mut ctx = f.tool_context();
    ctx.options.insert(dshr_agent::LlmAccess { provider: provider.clone(), model: "chat".into() });
    assert_eq!(f.tool(&cx, "pieces").execute(json!({}), &ctx).await.content, "text:Hel|text:lo usage=some");
    assert_eq!(f.tool(&cx, "whole").execute(json!({}), &ctx).await.content, "abcd");
    assert!(f.tool(&cx, "sneaky").execute(json!({}), &ctx).await.content.contains("`llm` capability"));
}

/// 订阅 `stream/*` 的插件收到合并后的增量和结束通知；没有 `session:read` 的、只订阅 `*` 的都收不到。
#[tokio::test]
async fn stream_subscribers_receive_merged_deltas() {
    let f = Fixture::new();
    let recorder = r#"
        export const on = {
          "stream/text": async ({ text, session }, host) => { await host.appendData("log.txt", `T[${text}]`); },
          "stream/end": async (_event, host) => { await host.appendData("log.txt", "END;"); },
          "*": async ({ type }, host) => { await host.appendData("all.txt", `${type};`); },
        };
        export const tools = [{ name: "TOOL", readOnly: true, async execute(_i, host) {
          return `${(await host.readData("log.txt")) ?? ""}#${(await host.readData("all.txt")) ?? ""}`;
        } }];"#;
    // `dshr:sessions:read` 让 `*` 收得到内核事件：用来确认它收不到流式输出。
    f.plugin("listener", "capabilities = [\"session:read\", \"dshr:sessions:read\"]\n", &recorder.replace("TOOL", "listened"));
    f.plugin("deaf", "", &recorder.replace("TOOL", "deaf_log"));
    let plugins = agent_context(&f);
    run_turn(plugins.clone(), &f, vec![streamed_reply(&["Hel", "lo", " world"])], Arc::new(AutoApprove), AgentConfig::default()).await;
    // 工具调用排在已经排进队列的事件通知后面：轮到它时通知都处理完了。
    let cx = plugins;
    let listened = f.call(&cx, "listened", json!({})).await.content;
    let (stream, all) = listened.split_once('#').unwrap();
    // 增量在 100 ms 内到齐，合并成一段；之后是结束通知。
    assert_eq!(stream, "T[Hello world]END;", "{listened}");
    assert!(!all.contains("stream/"), "`*` must not receive stream events: {all}");
    assert!(all.contains("user/message"), "{all}");
    let deaf = f.call(&cx, "deaf_log", json!({})).await.content;
    assert!(deaf.starts_with('#'), "a plugin without session:read gets no stream events: {deaf}");
}

#[tokio::test]
async fn ui_entries_can_be_hidden_and_shown_at_run_time() {
    use dshr_agent::EntryPlace;

    let f = Fixture::new();
    f.plugin(
        "toggler",
        "",
        r#"export const ui = {
            pages: { main: () => "hi" },
            sidebar: [{ id: "todo", label: "Todo", page: "main" }],
            settings: [{ id: "cfg", label: "Cfg", page: "main" }],
            panels: [{ id: "note", label: "Note", page: "main" }],
            hide: ["status:account"],
        };
        export const tools = [{ name: "toggle", async execute({ target, hidden }, host) {
            try {
                if (hidden) await host.ui.hide(target); else await host.ui.show(target);
                return (await host.ui.hidden()).join(",");
            } catch (error) { return `error: ${error.message}`; }
        } }];"#,
    );
    let cx = f.context();
    let surface = || cx.snapshot().items::<dshr_agent::UiSurfaces>().iter().find(|surface| surface.plugin() == "toggler").unwrap().clone();
    assert_eq!(surface().hidden(), ["status:account"]);
    assert!(!surface().entry_hidden(EntryPlace::Sidebar, "todo"));

    let before = dshr_agent::entries_generation();
    let toggle = |target: &str, hidden: bool| f.call(&cx, "toggle", json!({ "target": target, "hidden": hidden }));
    assert_eq!(toggle("sidebar:todo", true).await.content, "sidebar:todo,status:account");
    assert_eq!(toggle("panel:note", true).await.content, "panel:note,sidebar:todo,status:account");
    assert_eq!(toggle("composer:presets", true).await.content, "panel:note,sidebar:todo,composer:presets,status:account");
    assert!(dshr_agent::entries_generation() > before);
    assert!(surface().entry_hidden(EntryPlace::Sidebar, "todo"));
    assert!(surface().entry_hidden(EntryPlace::Panel, "note"));
    assert!(!surface().entry_hidden(EntryPlace::Settings, "cfg"));
    assert_eq!(surface().hidden(), ["composer:presets", "status:account"]);

    assert_eq!(toggle("sidebar:todo", false).await.content, "panel:note,composer:presets,status:account");
    assert_eq!(toggle("status:account", false).await.content, "panel:note,composer:presets");
    assert!(!surface().entry_hidden(EntryPlace::Sidebar, "todo"));
    // 不认识的、不能藏的入口报错，什么都不改。
    assert!(toggle("panel:missing", true).await.content.starts_with("error:"));
    assert!(toggle("settings:extensions", true).await.content.starts_with("error:"));
    assert_eq!(surface().hidden(), ["composer:presets"]);
}

/// 测试里用的会话控制：一个打开着的会话，记下改动和发出的消息。
#[derive(Default)]
struct Sessions {
    changes: Mutex<Vec<(Option<String>, crate::app::OptionChange)>>,
    sent: Mutex<Vec<(String, Vec<String>)>>,
}

impl crate::app::SessionControl for Sessions {
    fn send(&self, request: crate::app::SendRequest) -> Result<String, String> {
        let images = request
            .images
            .iter()
            .map(|image| match image {
                dshr_agent::UserAttachment::Image { path, media_type, .. } => format!("{path}:{media_type}"),
                dshr_agent::UserAttachment::Text(path, _) => path.clone(),
            })
            .collect();
        self.sent.lock().unwrap().push((request.text, images));
        Ok("s1".into())
    }

    fn stop(&self, _session: Option<String>) -> Result<(), String> {
        Ok(())
    }

    fn list(&self) -> Vec<crate::app::OpenSession> {
        vec![crate::app::OpenSession {
            id: "s1".into(),
            title: "One".into(),
            workspace: None,
            running: false,
            current: true,
            model: "deepseek/flash".into(),
            effort: None,
            preset: "standard".into(),
            permission: "workspace".into(),
        }]
    }

    fn set_options(&self, session: Option<String>, change: crate::app::OptionChange) -> Result<(), String> {
        if let crate::app::OptionChange::Preset(preset) = &change
            && preset == "nope"
        {
            return Err("unknown preset nope".into());
        }
        self.changes.lock().unwrap().push((session, change));
        Ok(())
    }

    fn models(&self) -> Vec<crate::app::ModelInfo> {
        vec![crate::app::ModelInfo {
            model: "deepseek/flash".into(),
            provider: "deepseek".into(),
            name: "Flash".into(),
            provider_name: "DeepSeek".into(),
        }]
    }
}

#[tokio::test]
async fn session_model_preset_and_images() {
    use crate::app::OptionChange;

    let _serial = SESSION_SERVICE.lock().await;
    let sessions = Arc::new(Sessions::default());
    let shared = sessions.clone();
    crate::app::set_app_services(|services| services.session = Some(shared));
    let f = Fixture::new();
    f.plugin(
        "driver",
        "capabilities = [\"session:control\"]\n",
        r#"export const tools = [{ name: "drive", async execute({ step }, host) {
            try {
                switch (step) {
                    case "read": {
                        const model = await host.session.model();
                        const models = (await host.session.models()).map((m) => m.model).join(",");
                        return `${model.model}/${model.effort}|${await host.session.preset()}|${await host.session.permission()}|${models}`;
                    }
                    case "model": await host.session.setModel("deepseek/pro", { effort: "high" }); return "ok";
                    case "effort": await host.session.setModel(null, { effort: null, session: "s1" }); return "ok";
                    case "preset": await host.session.setPreset("plan"); return "ok";
                    case "bad-preset": await host.session.setPreset("nope"); return "ok";
                    case "bad-effort": await host.session.setModel(null, { effort: "huge" }); return "ok";
                    case "send": return await host.session.send("look", {
                        images: [{ data: new Uint8Array([1, 2, 3]), mediaType: "image/png", name: "shot.png" }, { data: "AAAA", mediaType: "image/jpeg" }],
                    });
                    case "bad-image": return await host.session.send("x", { images: [{ data: "AAAA", mediaType: "image/bmp" }] });
                }
            } catch (error) { return `error: ${error.message}`; }
        } }, { name: "peek", readOnly: true, async execute(_i, host) {
            try { await host.session.setPreset("plan"); return "changed"; } catch (error) { return `error: ${error.message}`; }
        } }];"#,
    );
    f.plugin(
        "reader",
        "capabilities = [\"session:read\"]\n",
        r#"export const tools = [{ name: "read_only_model", async execute(_i, host) {
        const model = (await host.session.model()).model;
        try { await host.session.setModel("x/y"); return `${model} changed`; } catch (error) { return `${model} ${error.message}`; }
    } }];"#,
    );
    let cx = f.context();
    let drive = |step: &str| f.call(&cx, "drive", json!({ "step": step }));
    assert_eq!(drive("read").await.content, "deepseek/flash/null|standard|workspace|deepseek/flash");
    assert_eq!(drive("model").await.content, "ok");
    assert_eq!(drive("effort").await.content, "ok");
    assert_eq!(drive("preset").await.content, "ok");
    assert_eq!(drive("bad-preset").await.content, "error: unknown preset nope");
    assert!(drive("bad-effort").await.content.contains("unknown effort"));
    assert!(drive("bad-image").await.content.contains("unsupported image type image/bmp"));
    assert_eq!(drive("send").await.content, "s1");
    assert!(f.call(&cx, "peek", json!({})).await.content.starts_with("error: changing the session's model"));
    let read_only = f.call(&cx, "read_only_model", json!({})).await.content;
    assert!(read_only.starts_with("deepseek/flash ") && read_only.contains("session:control"), "{read_only}");

    let changes = sessions.changes.lock().unwrap().clone();
    assert_eq!(
        changes,
        [
            (None, OptionChange::Model { model: Some("deepseek/pro".into()), effort: Some(Some("high".into())) }),
            (Some("s1".into()), OptionChange::Model { model: None, effort: Some(None) }),
            (None, OptionChange::Preset("plan".into())),
        ]
    );
    let sent = sessions.sent.lock().unwrap().clone();
    assert_eq!(sent, [("look".to_owned(), vec!["shot.png:image/png".to_owned(), "plugin-image-2:image/jpeg".to_owned()])]);
}

/// 一直不回答的审批（界面上没人点）。
struct Silent;

#[async_trait::async_trait]
impl Approver for Silent {
    async fn approve(&self, _request: &ApprovalRequest) -> Decision {
        std::future::pending().await
    }
}

/// 一直拒绝、并记下问了什么的审批。
#[derive(Default)]
struct DenyAndRecord(Mutex<Vec<String>>);

#[async_trait::async_trait]
impl Approver for DenyAndRecord {
    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        self.0.lock().unwrap().push(request.tool.clone());
        Decision::Deny
    }
}

#[tokio::test]
async fn plugin_permission_mode_decides_tool_calls() {
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("a.txt"), "x").unwrap();
    f.plugin(
        "strict",
        "capabilities = [\"permissions:mode\"]\n",
        r#"export const permissions = [{
            id: "careful", label: "Careful", description: "Only listing runs directly", icon: "eye",
            decide: ({ tool }) => tool === "write_file" ? { decision: "deny", reason: "no writes here" } : tool === "list_dir" ? "allow" : undefined,
        }];"#,
    );
    f.write_plugin("ungranted-mode", "capabilities = []\n", "export const permissions = [{ id: 'x', label: 'X', decide: () => 'allow' }];");
    let plugins = agent_context(&f);
    let snapshot = plugins.snapshot();
    let modes = snapshot.items::<PluginPermissions>();
    assert_eq!(modes.len(), 1);
    assert_eq!((modes[0].key(), modes[0].label.as_str(), modes[0].icon.as_deref()), ("strict/careful".to_owned(), "Careful", Some("eye")));
    assert!(failure(&plugins, "ungranted-mode").contains("permissions:mode"));

    let approver = Arc::new(DenyAndRecord::default());
    // 内置的权限选的是完全访问，但插件模式优先：没给结论的调用按手动确认问用户。
    let config = AgentConfig::default().with(PermissionMode::FullAccess).with(PluginPermission(modes[0].clone()));
    let replies = vec![
        ScriptedProvider::tool_reply("c1", "write_file", json!({ "path": "b.txt", "content": "y" })),
        ScriptedProvider::tool_reply("c2", "list_dir", json!({ "path": "." })),
        ScriptedProvider::tool_reply("c3", "edit_file", json!({ "path": "a.txt", "old_string": "x", "new_string": "z" })),
        ScriptedProvider::text_reply("done"),
    ];
    let session = run_turn(plugins.clone(), &f, replies, approver.clone(), config).await;
    let results = tool_results(&session);
    assert!(results[0].1 && results[0].0.contains("no writes here"), "{results:?}");
    assert!(!results[1].1 && results[1].0.contains("a.txt"), "{results:?}");
    assert!(results[2].1 && results[2].0.contains("denied"), "{results:?}");
    assert_eq!(*approver.0.lock().unwrap(), ["edit_file"]);
    assert!(!f.workspace.path().join("b.txt").exists());
    assert_eq!(std::fs::read_to_string(f.workspace.path().join("a.txt")).unwrap(), "x");
}

#[tokio::test]
async fn approve_hook_answers_approvals() {
    let f = Fixture::new();
    f.plugin(
        "phone",
        "capabilities = [\"permissions:approve\"]\n",
        r#"export const hooks = {
            approve: async ({ tool, input }) => input.path === "ok.txt" ? "allow" : input.path === "no.txt" ? { decision: "deny", reason: "nope" } : undefined,
        };"#,
    );
    f.write_plugin("no-cap", "capabilities = []\n", "export const hooks = { approve: () => 'allow' };");
    let plugins = agent_context(&f);
    assert!(failure(&plugins, "no-cap").contains("permissions:approve"));

    // 界面上没人回答：插件先答复，以它为准。
    let replies = vec![
        ScriptedProvider::tool_reply("c1", "write_file", json!({ "path": "ok.txt", "content": "y" })),
        ScriptedProvider::tool_reply("c2", "write_file", json!({ "path": "no.txt", "content": "y" })),
        ScriptedProvider::text_reply("done"),
    ];
    let config = AgentConfig::default().with(PermissionMode::Manual);
    let session = run_turn(plugins.clone(), &f, replies, Arc::new(Silent), config.clone()).await;
    let results = tool_results(&session);
    assert!(!results[0].1, "{results:?}");
    assert!(results[1].1, "{results:?}");
    assert!(f.workspace.path().join("ok.txt").exists() && !f.workspace.path().join("no.txt").exists());
    let approved: Vec<String> = session
        .events()
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Ext { kind, data } if kind == "plugins/approved" => Some(data["decision"].as_str().unwrap_or_default().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(approved, ["allow", "deny"]);

    // 插件放弃（返回 undefined）：照常等界面。
    let approver = Arc::new(DenyAndRecord::default());
    let replies = vec![
        ScriptedProvider::tool_reply("d1", "write_file", json!({ "path": "other.txt", "content": "y" })),
        ScriptedProvider::text_reply("done"),
    ];
    let session = run_turn(plugins, &f, replies, approver.clone(), config).await;
    assert!(tool_results(&session)[0].1);
    assert_eq!(*approver.0.lock().unwrap(), ["write_file"]);
}

/// 完全访问模式下改写已经允许过能力的插件不再询问；其余模式仍然必须确认。授予能力在任何模式下都要确认。
#[tokio::test]
async fn full_access_edits_granted_plugins_without_asking() {
    let f = Fixture::new();
    f.plugin("granted", "capabilities = [\"fs:workspace:read\"]\n", "export const tools = [];");
    let context = || {
        let scripts = JsPlugins::discover(&f.plugins_dir());
        let plugins = Context::new();
        plugins.add_group(dshr_tools::BuiltinTools).unwrap();
        plugins.add_group(AgentPlugins).unwrap();
        plugins.add(crate::CreatorPlugin::new(scripts.clone())).unwrap();
        plugins.add_group(scripts).unwrap();
        plugins
    };
    let write = || ScriptedProvider::tool_reply("w", "plugin_write", json!({ "id": "granted", "path": "notes.md", "content": "x" }));

    let recorder = Arc::new(super::Recorder::default());
    let config = AgentConfig::default().with(PermissionMode::FullAccess).with(Preset::Create);
    let session = run_turn(context(), &f, vec![write(), ScriptedProvider::text_reply("ok")], recorder.clone(), config).await;
    assert!(!tool_results(&session)[0].1);
    assert!(recorder.0.lock().unwrap().is_empty(), "{:?}", recorder.0.lock().unwrap());

    let config = AgentConfig::default().with(PermissionMode::Workspace).with(Preset::Create);
    run_turn(context(), &f, vec![write(), ScriptedProvider::text_reply("ok")], recorder.clone(), config).await;
    assert_eq!(recorder.0.lock().unwrap().len(), 1);
    assert!(recorder.0.lock().unwrap()[0].starts_with("plugin_write"));
}
