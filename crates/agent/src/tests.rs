use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use dshr_core::{Message, Projection, Role};
use dshr_llm::EventStream;
use dshr_llm::mock::ScriptedProvider;
use serde_json::json;
use tokio::sync::{Notify, mpsc, watch};

use super::*;

struct DenyAll;

#[async_trait::async_trait]
impl Approver for DenyAll {
    async fn approve(&self, _request: &ApprovalRequest) -> Decision {
        Decision::Deny
    }
}

/// 子代理用的插件：内置工具加上 agent 自带的插件。
fn subagent_plugins() -> Context {
    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    plugins.add_group(AgentPlugins).unwrap();
    plugins
}

/// 主代理用的插件，和主程序的装配一样，只是不生成标题（免得每个脚本都要多写一条回复）。
fn builtin_plugins() -> Context {
    let plugins = subagent_plugins();
    plugins.add(DelegatePlugin::new(subagent_plugins())).unwrap();
    plugins
}

struct Harness {
    dir: tempfile::TempDir,
    session: Session,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::create("s1", dir.path().join("log").join("session.v1.jsonl")).unwrap();
        Self { dir, session }
    }

    fn agent(&self, provider: Arc<dyn Provider>, approver: Arc<dyn Approver>) -> Agent {
        self.agent_with(provider, approver, PermissionMode::Workspace)
    }

    fn agent_with(&self, provider: Arc<dyn Provider>, approver: Arc<dyn Approver>, permission: PermissionMode) -> Agent {
        let config = AgentConfig { retry_delay: Duration::from_millis(1), ..AgentConfig::default() }.with(permission);
        Agent::new(provider, builtin_plugins(), approver, ToolContext::new(self.dir.path().to_owned()), config)
    }

    fn kinds(&self) -> Vec<&str> {
        self.session.events().iter().map(|event| event.body.kind()).collect()
    }
}

#[tokio::test]
async fn tool_call_then_answer() {
    let mut h = Harness::new();
    std::fs::write(h.dir.path().join("hello.txt"), "hi").unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "list_dir", json!({"path": "."})),
        ScriptedProvider::text_reply("只有 hello.txt"),
    ]));
    let agent = h.agent(provider.clone(), Arc::new(AutoApprove));
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = agent.run_turn(&mut h.session, "有哪些文件？".into(), &tx, &CancellationToken::new()).await.unwrap();

    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert_eq!(outcome.final_text, "只有 hello.txt");
    assert_eq!(
        h.kinds(),
        [
            "session/workspace",
            "system/message",
            "turn/start",
            "user/message",
            "step/start",
            // 日期和权限模式：第一步之前写一条，之后没变就不再写。
            "environment/turn",
            "request/header",
            "assistant/message",
            "tool/call",
            "tool/result",
            "step/end",
            "step/start",
            "request/header",
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );

    // 第二次请求带上了工具结果。
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let last = requests[1].messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(matches!(
        &last.content[0],
        ContentBlock::ToolResult { content, .. } if content.contains("hello.txt")
    ));

    // 订阅方收到了流式文本，且持久事件与日志一致。
    drop(tx);
    let mut chunks = String::new();
    let mut durable = 0;
    while let Some(event) = rx.recv().await {
        match event {
            LiveEvent::Chunk(Chunk::Text(text)) => chunks.push_str(&text),
            LiveEvent::Chunk(Chunk::Thinking(_) | Chunk::ToolOutput { .. }) => {}
            LiveEvent::Durable(_) => durable += 1,
        }
    }
    assert_eq!(chunks, "只有 hello.txt");
    assert_eq!(durable, h.session.events().len());

    // 日志可以重新打开并继续下一轮。
    let reopened = Session::open("s1", h.dir.path().join("log").join("session.v1.jsonl")).unwrap();
    assert_eq!(reopened.events(), h.session.events());
}

fn tool_results(session: &Session) -> Vec<(bool, String)> {
    session
        .events()
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolResult { is_error, output, .. } => Some((*is_error, output.to_string())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn denied_command_is_reported_to_model() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "run_shell", json!({"command": "echo hi > x.txt"})),
        ScriptedProvider::text_reply("好的，不运行了"),
    ]));
    // 手动确认模式：工作区内修改模式在 macOS 上有 Seatbelt 沙箱，命令会直接放行，不经过审批。
    let agent = h.agent_with(provider, Arc::new(DenyAll), PermissionMode::Manual);
    let (tx, _rx) = mpsc::unbounded_channel();

    agent.run_turn(&mut h.session, "运行命令".into(), &tx, &CancellationToken::new()).await.unwrap();

    assert!(!h.dir.path().join("x.txt").exists());
    assert!(matches!(&tool_results(&h.session)[..], [(true, output)] if output.contains("denied")));
}

#[tokio::test]
async fn workspace_mode_allows_writes_inside_without_asking() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "write_file", json!({"path": "x.txt", "content": "x"})),
        ScriptedProvider::text_reply("写好了"),
    ]));
    // DenyAll 不会被询问到：工作区内写入直接放行。
    let agent = h.agent(provider, Arc::new(DenyAll));
    let (tx, _rx) = mpsc::unbounded_channel();

    agent.run_turn(&mut h.session, "写个文件".into(), &tx, &CancellationToken::new()).await.unwrap();

    assert_eq!(std::fs::read_to_string(h.dir.path().join("x.txt")).unwrap(), "x");
}

#[tokio::test]
async fn manual_mode_asks_even_inside_the_workspace() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "write_file", json!({"path": "x.txt", "content": "x"})),
        ScriptedProvider::text_reply("改不了"),
    ]));
    let agent = h.agent_with(provider, Arc::new(DenyAll), PermissionMode::Manual);
    let (tx, _rx) = mpsc::unbounded_channel();

    agent.run_turn(&mut h.session, "写个文件".into(), &tx, &CancellationToken::new()).await.unwrap();

    assert!(!h.dir.path().join("x.txt").exists());
    assert!(matches!(&tool_results(&h.session)[..], [(true, output)] if output == "User denied this tool call."));
}

/// 记下每次审批附带的说明，然后拒绝。
#[derive(Default)]
struct NoteTaker(std::sync::Mutex<Vec<Option<String>>>);

#[async_trait::async_trait]
impl Approver for NoteTaker {
    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        self.0.lock().unwrap().push(request.note.clone());
        Decision::Deny
    }
}

#[tokio::test]
async fn auto_review_runs_low_risk_commands_without_asking() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "run_shell", json!({"command": "echo reviewed"})),
        // 审查用的是同一个模型：这一条回复给审查。
        ScriptedProvider::text_reply(r#"{"decision": "allow", "risk": "low", "reason": "只是输出一行字"}"#),
        ScriptedProvider::text_reply("运行了"),
    ]));
    let approver = Arc::new(NoteTaker::default());
    let agent = h.agent_with(provider.clone(), approver.clone(), PermissionMode::AutoReview);
    let (tx, _rx) = mpsc::unbounded_channel();

    agent.run_turn(&mut h.session, "打个招呼".into(), &tx, &CancellationToken::new()).await.unwrap();

    assert!(approver.0.lock().unwrap().is_empty(), "低风险的不问用户");
    assert!(matches!(&tool_results(&h.session)[..], [(false, output)] if output.contains("reviewed")));
    // 审查请求里有用户的原话和要执行的命令，不带工具。
    let review = &provider.requests()[1];
    assert!(review.tools.is_empty());
    let text = request_text(review);
    assert!(text.contains("打个招呼") && text.contains("echo reviewed"), "{text}");
}

#[tokio::test]
async fn auto_review_asks_about_risky_calls_with_its_reason() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "run_shell", json!({"command": "rm -rf ~/projects"})),
        ScriptedProvider::text_reply(r#"{"decision": "ask", "risk": "high", "reason": "会删除工作区外的目录"}"#),
        ScriptedProvider::text_reply("不删了"),
    ]));
    let approver = Arc::new(NoteTaker::default());
    let agent = h.agent_with(provider, approver.clone(), PermissionMode::AutoReview);
    let (tx, _rx) = mpsc::unbounded_channel();

    agent.run_turn(&mut h.session, "清理一下".into(), &tx, &CancellationToken::new()).await.unwrap();

    assert_eq!(*approver.0.lock().unwrap(), [Some("会删除工作区外的目录".to_owned())]);
    assert!(matches!(&tool_results(&h.session)[..], [(true, output)] if output == "User denied this tool call."));
}

#[tokio::test]
async fn auto_review_does_not_review_workspace_edits() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "write_file", json!({"path": "x.txt", "content": "x"})),
        ScriptedProvider::text_reply("写好了"),
    ]));
    let agent = h.agent_with(provider.clone(), Arc::new(DenyAll), PermissionMode::AutoReview);
    let (tx, _rx) = mpsc::unbounded_channel();

    agent.run_turn(&mut h.session, "写个文件".into(), &tx, &CancellationToken::new()).await.unwrap();

    assert_eq!(std::fs::read_to_string(h.dir.path().join("x.txt")).unwrap(), "x");
    assert_eq!(provider.requests().len(), 2, "没有额外的审查请求");
}

#[tokio::test]
async fn steering_messages_join_the_running_turn() {
    let mut h = Harness::new();
    std::fs::write(h.dir.path().join("hello.txt"), "hi").unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "list_dir", json!({"path": "."})),
        ScriptedProvider::text_reply("看到了"),
    ]));
    let steer = SteerQueue::new();
    let agent = h.agent(provider.clone(), Arc::new(AutoApprove)).with_steering(steer.clone());
    // 第一步运行期间插队的消息（这里提前放好：第一步结束后才取）。
    steer.push(SteerMessage { id: 7, text: "顺便看看大小".into(), attachments: Vec::new() });
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = agent.run_turn(&mut h.session, "有哪些文件？".into(), &tx, &CancellationToken::new()).await.unwrap();

    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert!(steer.is_empty());
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    // 插队的话接在工具结果后面，同一条 user 消息里。
    let last = requests[1].messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(matches!(&last.content[0], ContentBlock::ToolResult { .. }));
    assert!(matches!(last.content.last(), Some(ContentBlock::Text { text }) if &**text == "顺便看看大小"));
    let users = h.kinds().into_iter().filter(|kind| *kind == "user/message").count();
    assert_eq!(users, 2);
}

#[tokio::test]
async fn steering_keeps_a_finished_turn_going() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([ScriptedProvider::text_reply("一"), ScriptedProvider::text_reply("二")]));
    let steer = SteerQueue::new();
    let agent = h.agent(provider.clone(), Arc::new(AutoApprove)).with_steering(steer.clone());
    steer.push(SteerMessage { id: 1, text: "再说一句".into(), attachments: Vec::new() });
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = agent.run_turn(&mut h.session, "说一句".into(), &tx, &CancellationToken::new()).await.unwrap();

    // 模型说完“一”本来要结束，因为有人插队又请求了一次。
    assert_eq!(outcome.final_text, "二");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(request_text(&requests[1]).contains("再说一句"));
    let turn_ends = h.kinds().into_iter().filter(|kind| *kind == "turn/end").count();
    assert_eq!(turn_ends, 1);
}

#[tokio::test]
async fn full_access_runs_commands_without_asking() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "run_shell", json!({"command": "echo full"})),
        ScriptedProvider::text_reply("运行了"),
    ]));
    let agent = h.agent_with(provider, Arc::new(DenyAll), PermissionMode::FullAccess);
    let (tx, _rx) = mpsc::unbounded_channel();

    agent.run_turn(&mut h.session, "运行命令".into(), &tx, &CancellationToken::new()).await.unwrap();

    assert!(matches!(&tool_results(&h.session)[..], [(false, output)] if output.contains("full")));
}

#[tokio::test]
async fn cancelled_turn_is_interrupted() {
    let mut h = Harness::new();
    let agent = h.agent(Arc::new(ScriptedProvider::default()), Arc::new(AutoApprove));
    let (tx, _rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let outcome = agent.run_turn(&mut h.session, "hi".into(), &tx, &cancel).await.unwrap();

    assert_eq!(outcome.reason, TurnEndReason::Interrupted);
    assert_eq!(h.kinds().last(), Some(&"turn/end"));
}

#[derive(Default)]
struct TitleGate {
    started: Notify,
    release: Notify,
}

struct TitleScripts {
    main: ScriptedProvider,
    title: ScriptedProvider,
    gate: Option<TitleGate>,
}

#[async_trait::async_trait]
impl Provider for TitleScripts {
    fn id(&self) -> &str {
        "title-scripts"
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        if request.system.as_deref().is_some_and(|system| system.starts_with("Write a short title for this conversation")) {
            let reply = self.title.stream(request).await?;
            if let Some(gate) = &self.gate {
                gate.started.notify_one();
                gate.release.notified().await;
            }
            Ok(reply)
        } else {
            // Ensure the title request is already in flight before the first main reply.
            if let Some(gate) = &self.gate
                && self.main.requests().is_empty()
            {
                gate.started.notified().await;
            }
            self.main.stream(request).await
        }
    }
}

async fn wait_for_title_write(session: &Session, changes: &mut watch::Receiver<u64>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        // The watch channel also wakes for titles written by other tests.
        while !session.has_title() {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("background title should be written");
}

#[tokio::test]
async fn generates_title_once_without_blocking_the_turn() {
    let mut h = Harness::new();
    let provider = Arc::new(TitleScripts {
        main: ScriptedProvider::new([ScriptedProvider::text_reply("好的"), ScriptedProvider::text_reply("第二轮回答")]),
        title: ScriptedProvider::new([ScriptedProvider::text_reply("  “修复登录问题”。\n多余的一行")]),
        gate: Some(TitleGate::default()),
    });
    let plugins = builtin_plugins();
    plugins.add(TitlePlugin).unwrap();
    let agent =
        Agent::new(provider.clone(), plugins, Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), AgentConfig::default());
    let (tx, _rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let mut changes = title_changes();
    let first_user = "登录总是超时";
    let outcome = tokio::time::timeout(Duration::from_secs(10), agent.run_turn(&mut h.session, first_user.into(), &tx, &cancel))
        .await
        .expect("main turn should finish while the title response is blocked")
        .unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert_eq!(outcome.final_text, "好的");
    assert_eq!(h.kinds().last(), Some(&"turn/end"));
    assert!(!h.session.has_title());

    let requests = provider.title.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[0].messages, [Message { role: Role::User, content: vec![ContentBlock::text(first_user)] }]);

    // A second turn must not request a title, even while the first title is still pending.
    let outcome = tokio::time::timeout(Duration::from_secs(10), agent.run_turn(&mut h.session, "再问一句".into(), &tx, &cancel))
        .await
        .expect("second turn should also finish while the title response is blocked")
        .unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert_eq!(outcome.final_text, "第二轮回答");
    assert!(!h.session.has_title());

    provider.gate.as_ref().unwrap().release.notify_one();
    wait_for_title_write(&h.session, &mut changes).await;
    assert_eq!(provider.main.requests().len(), 2);
    assert_eq!(provider.title.requests().len(), 1);
    assert_eq!(h.session.title(), None);
    assert!(h.session.refresh_title());
    assert_eq!(h.session.title(), Some("修复登录问题"));
    assert_eq!(h.kinds()[h.kinds().len() - 2..], ["turn/end", "session/title"]);
    assert!(!h.session.refresh_title());
    let reopened = Session::open("s1", h.session.log_path()).unwrap();
    assert_eq!(reopened.events(), h.session.events());
}

/// 声称能把写入限制在工作区内的沙箱。
struct Confining;

impl dshr_sandbox::Sandbox for Confining {
    fn confines_writes(&self) -> bool {
        true
    }
}

struct ConfiningPlugin;

impl dshr_context::Plugin for ConfiningPlugin {
    fn manifest(&self) -> dshr_context::Manifest {
        dshr_context::Manifest::new("confining")
    }

    fn apply(&self, cx: &mut dshr_context::Scope) -> anyhow::Result<()> {
        cx.provide::<dyn dshr_sandbox::Sandbox>(Arc::new(Confining));
        Ok(())
    }
}

#[tokio::test]
async fn confining_sandbox_lets_commands_run_without_asking() {
    let mut h = Harness::new();
    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    plugins.add(PermissionPlugin).unwrap();
    // 沙箱服务在权限插件之后才出现，权限插件会重新激活并用上它。
    plugins.add(ConfiningPlugin).unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "run_shell", json!({"command": "echo confined"})),
        ScriptedProvider::text_reply("运行了"),
    ]));
    let agent = Agent::new(provider, plugins.clone(), Arc::new(DenyAll), ToolContext::new(h.dir.path().to_owned()), AgentConfig::default());
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut h.session, "运行命令".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert!(matches!(&tool_results(&h.session)[..], [(false, output)] if output.contains("confined")));
}

#[tokio::test]
async fn without_a_gate_every_call_asks() {
    let mut h = Harness::new();
    std::fs::write(h.dir.path().join("a.txt"), "a").unwrap();
    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "read_file", json!({"path": "a.txt"})),
        ScriptedProvider::text_reply("读不了"),
    ]));
    let agent = Agent::new(provider, plugins, Arc::new(DenyAll), ToolContext::new(h.dir.path().to_owned()), AgentConfig::default());
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut h.session, "读文件".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert!(matches!(&tool_results(&h.session)[..], [(true, output)] if output.contains("denied")));
}

#[tokio::test]
async fn plan_preset_blocks_writes_in_any_permission_mode() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "write_file", json!({"path": "x.txt", "content": "x"})),
        ScriptedProvider::text_reply("只给方案"),
    ]));
    let config = AgentConfig::default().with(PermissionMode::FullAccess).with(Preset::Plan);
    let agent = Agent::new(provider.clone(), builtin_plugins(), Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), config);
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut h.session, "写个文件".into(), &tx, &CancellationToken::new()).await.unwrap();

    // 请求里本来就没有 write_file；模型硬调也照样拦下。
    assert!(!provider.requests()[0].tools.iter().any(|tool| tool.name == "write_file"));
    assert!(!h.dir.path().join("x.txt").exists());
    assert!(matches!(&tool_results(&h.session)[..], [(true, output)] if output == READ_ONLY_DENIED));
}

#[tokio::test]
async fn read_only_turns_only_offer_read_tools() {
    let configs = [
        AgentConfig::default().with(PermissionMode::FullAccess).with(Preset::Plan),
        AgentConfig::default().with(PermissionMode::Manual).with(Preset::Plan),
        AgentConfig::default().with(PermissionMode::Workspace),
    ];
    for (index, config) in configs.into_iter().enumerate() {
        let mut h = Harness::new();
        let provider = Arc::new(ScriptedProvider::new([ScriptedProvider::text_reply("好")]));
        let agent =
            Agent::new(provider.clone(), builtin_plugins(), Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), config);
        let (tx, _rx) = mpsc::unbounded_channel();
        agent.run_turn(&mut h.session, "看看".into(), &tx, &CancellationToken::new()).await.unwrap();
        let request = &provider.requests()[0];
        let names: Vec<_> = request.tools.iter().map(|tool| tool.name.as_str()).collect();
        // 权限模式不在系统提示词里（它一变整段缓存就失效），在追加进对话的环境说明里。
        assert!(!request.system.as_deref().unwrap().contains("Permission mode"));
        let note = request_text(request);
        if index < 2 {
            assert_eq!(names, ["delegate", "glob", "grep", "list_dir", "read_file", "update_plan"]);
            assert!(note.contains("- Permission mode: read-only"), "{note}");
        } else {
            assert!(names.contains(&"write_file") && names.contains(&"run_shell"));
            assert!(note.contains("- Permission mode: workspace"), "{note}");
        }
    }
}

/// 请求里所有消息的文本块，连在一起。
fn request_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 父代理在 `preset` 下委派一项任务，返回全部请求：父代理的第一个请求，然后是子代理的。
async fn delegate_under(preset: Preset) -> Vec<ChatRequest> {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("d1", "delegate", json!({"task": "查一下有哪些文件"})),
        ScriptedProvider::text_reply("报告"),
        ScriptedProvider::text_reply("好"),
    ]));
    let config = AgentConfig::default().with(PermissionMode::FullAccess).with(preset);
    let agent = Agent::new(provider.clone(), builtin_plugins(), Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), config);
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut h.session, "委派".into(), &tx, &CancellationToken::new()).await.unwrap();
    provider.requests()
}

#[tokio::test]
async fn subagents_keep_restrictions_but_not_the_parents_mode_instructions() {
    // 审查模式下委派的是“查资料”：子代理不该照着“审查当前改动”去做。
    let requests = delegate_under(Preset::Review).await;
    let (parent, sub) = (requests[0].system.as_deref().unwrap(), requests[1].system.as_deref().unwrap());
    assert!(parent.starts_with(DEFAULT_SYSTEM_PROMPT) && parent.contains("Review mode"));
    assert!(sub.starts_with("You are a sub-agent of DSHR"), "{sub}");
    assert!(!sub.contains("Review mode") && !sub.contains("Read-only:"), "{sub}");
    assert!(requests[1].tools.iter().any(|tool| tool.name == "run_shell"));

    // 规划模式：子代理仍然只读，但拿到的是“查完汇报”，不是“写计划”。
    let requests = delegate_under(Preset::Plan).await;
    let sub = requests[1].system.as_deref().unwrap();
    assert!(sub.contains("Read-only: you can't modify files") && !sub.contains("Planning mode"), "{sub}");
    assert!(request_text(&requests[1]).contains("- Permission mode: read-only"));
    assert!(!requests[1].tools.iter().any(|tool| tool.name == "write_file" || tool.name == "delegate"));

    // 创造模式的说明（和 plugin_* 工具）只给主代理。
    let requests = delegate_under(Preset::Create).await;
    assert!(!requests[1].system.as_deref().unwrap().contains("plugin_"));
}

#[tokio::test]
async fn old_sessions_get_the_current_system_prompt() {
    let mut h = Harness::new();
    let (tx, _rx) = mpsc::unbounded_channel();
    let legacy = system_prompt::LEGACY_SYSTEM_PROMPTS[0];
    h.session.append(EventBody::SystemMessage { text: legacy.into() }, &tx).unwrap();
    let provider = Arc::new(ScriptedProvider::new([ScriptedProvider::text_reply("好")]));
    let agent = h.agent(provider.clone(), Arc::new(AutoApprove));
    agent.run_turn(&mut h.session, "你好".into(), &tx, &CancellationToken::new()).await.unwrap();

    let system = provider.requests()[0].system.clone().unwrap();
    assert!(system.starts_with(DEFAULT_SYSTEM_PROMPT) && !system.contains("Prefer small, verifiable steps"));
    // 日志不改：还是旧的那一条。
    let logged: Vec<_> = h
        .session
        .events()
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::SystemMessage { text } => Some(&**text),
            _ => None,
        })
        .collect();
    assert_eq!(logged, [legacy]);
}

#[test]
fn prompts_only_name_tools_that_exist() {
    let registry = dshr_tools::ToolRegistry::from_snapshot(&builtin_plugins().snapshot());
    let prompts = [
        DEFAULT_SYSTEM_PROMPT,
        system_prompt::SUBAGENT_PROMPT,
        Preset::Plan.instructions().unwrap(),
        Preset::Review.instructions().unwrap(),
        Preset::Plan.subagent_instructions().unwrap(),
        Preset::Review.subagent_instructions().unwrap(),
    ];
    // 带下划线的词都是工具名（read_file、run_shell……）。
    for prompt in prompts {
        for word in prompt.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|word| word.contains('_')) {
            assert!(registry.get(word).is_some(), "提示词里的 {word} 不是工具");
        }
    }
    for name in ["grep", "glob", "delegate"] {
        assert!(registry.get(name).is_some() && DEFAULT_SYSTEM_PROMPT.contains(name));
    }
}

#[tokio::test]
async fn compacts_earlier_turns_when_context_is_full() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::text_reply("第一轮回答"),
        // 第二轮开始前上下文已“满”：先请求摘要，再正常回答。
        ScriptedProvider::text_reply("用户问了第一问，已回答。"),
        ScriptedProvider::text_reply("第二轮回答"),
    ]));
    let config = AgentConfig { context_window: 100, retry_delay: Duration::from_millis(1), ..AgentConfig::default() };
    let agent = Agent::new(provider.clone(), builtin_plugins(), Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), config);
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut h.session, "第一问".into(), &tx, &CancellationToken::new()).await.unwrap();
    agent.run_turn(&mut h.session, "第二问".into(), &tx, &CancellationToken::new()).await.unwrap();

    let compactions: Vec<_> = h
        .session
        .events()
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Compaction { summary, .. } => Some(summary.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(compactions, ["用户问了第一问，已回答。"]);

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    // 摘要请求不带工具；之后的请求以摘要开头，不再包含第一轮原文。
    assert!(requests[1].tools.is_empty());
    let ContentBlock::Text { text } = &requests[2].messages[0].content[0] else { panic!() };
    assert!(text.starts_with(dshr_core::COMPACTION_PREFIX));
    assert!(!format!("{:?}", requests[2].messages).contains("第一轮回答"));
}

/// 摘要请求总是失败；其它请求按脚本回放。
struct FailingSummary {
    inner: ScriptedProvider,
    summaries: AtomicU32,
}

#[async_trait::async_trait]
impl Provider for FailingSummary {
    fn id(&self) -> &str {
        "failing-summary"
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        if request.tools.is_empty() {
            self.summaries.fetch_add(1, Ordering::SeqCst);
            return Err(LlmError::Http { status: 400, body: "context too long".into(), retry_after: None });
        }
        self.inner.stream(request).await
    }
}

#[tokio::test]
async fn failed_compaction_is_not_retried_every_step() {
    let mut h = Harness::new();
    let provider = Arc::new(FailingSummary {
        inner: ScriptedProvider::new([
            ScriptedProvider::text_reply("第一轮回答"),
            // 第二轮有三步，每一步之前上下文都是“满”的。
            ScriptedProvider::tool_reply("c1", "list_dir", json!({"path": "."})),
            ScriptedProvider::tool_reply("c2", "list_dir", json!({"path": "."})),
            ScriptedProvider::text_reply("第二轮回答"),
        ]),
        summaries: AtomicU32::new(0),
    });
    let config = AgentConfig { context_window: 100, retry_delay: Duration::from_millis(1), ..AgentConfig::default() };
    let agent = Agent::new(provider.clone(), builtin_plugins(), Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), config);
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut h.session, "第一问".into(), &tx, &CancellationToken::new()).await.unwrap();
    let outcome = agent.run_turn(&mut h.session, "第二问".into(), &tx, &CancellationToken::new()).await.unwrap();

    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert_eq!(outcome.final_text, "第二轮回答");
    // 三步只尝试了一次压缩，而不是三次。
    assert_eq!(provider.summaries.load(Ordering::SeqCst), 1);
    assert!(!h.kinds().contains(&"context/compaction"));
}

/// 摘要请求永远不返回。
struct HangingSummary {
    inner: ScriptedProvider,
}

#[async_trait::async_trait]
impl Provider for HangingSummary {
    fn id(&self) -> &str {
        "hanging-summary"
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        if request.tools.is_empty() {
            std::future::pending::<()>().await;
        }
        self.inner.stream(request).await
    }
}

#[tokio::test]
async fn compaction_can_be_interrupted() {
    let mut h = Harness::new();
    let provider = Arc::new(HangingSummary { inner: ScriptedProvider::new([ScriptedProvider::text_reply("第一轮回答")]) });
    let config = AgentConfig { context_window: 100, ..AgentConfig::default() };
    let agent = Agent::new(provider, builtin_plugins(), Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), config);
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut h.session, "第一问".into(), &tx, &CancellationToken::new()).await.unwrap();

    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stopper.cancel();
    });
    let outcome = tokio::time::timeout(Duration::from_secs(5), agent.run_turn(&mut h.session, "第二问".into(), &tx, &cancel))
        .await
        .expect("压缩期间点停止应当立刻结束本轮")
        .unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Interrupted);
}

#[test]
fn estimates_tokens_from_text_lengths() {
    let projection = Projection {
        system: Some("系统提示".into()),
        messages: vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::text("你好 world"),
                ContentBlock::ToolResult { tool_use_id: "c1".into(), content: "a.txt".into(), is_error: false },
            ],
        }],
    };
    // 按各块文本的 UTF-8 字节数求和 / 3，不再序列化。
    let expected = ("系统提示".len() + "你好 world".len() + "a.txt".len()) / 3;
    assert_eq!(compaction::estimate_tokens(&projection), expected);
}

#[test]
fn images_count_as_a_fixed_number_of_tokens() {
    // 一张约 300 KB 的截图（base64 后 400 KB）：按字节数算会超过默认窗口的压缩阈值（128k × 0.75），
    // 每带一张图就把之前的对话全部压缩掉。
    let data: String = "A".repeat(400 * 1024);
    let projection = Projection {
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::text("看看这张图"),
                ContentBlock::Image { source: dshr_core::ImageSource::base64("image/png", data) },
            ],
        }],
    };
    let tokens = compaction::estimate_tokens(&projection);
    assert!(tokens < 2_000, "一张图估成了 {tokens} tokens");
    assert!(tokens >= 1_000);
}

#[test]
fn cleans_titles() {
    use title::clean_title;
    assert_eq!(clean_title("Title: \"Fix login timeout.\""), "Fix login timeout");
    assert_eq!(clean_title("\n\n《重构配置》\n"), "重构配置");
    assert_eq!(clean_title("**Add grep tool**"), "Add grep tool");
}

/// 前两次返回 503，第三次正常。
struct Flaky {
    calls: AtomicU32,
}

#[async_trait::async_trait]
impl Provider for Flaky {
    fn id(&self) -> &str {
        "flaky"
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) < 2 {
            return Err(LlmError::Http { status: 503, body: "busy".into(), retry_after: None });
        }
        ScriptedProvider::new([ScriptedProvider::text_reply("ok")]).stream(request).await
    }
}

#[tokio::test]
async fn retries_transient_errors_then_gives_up() {
    let mut h = Harness::new();
    let agent = h.agent(Arc::new(Flaky { calls: AtomicU32::new(0) }), Arc::new(AutoApprove));
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = agent.run_turn(&mut h.session, "hi".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert_eq!(h.kinds().iter().filter(|kind| **kind == "assistant/attempt").count(), 2);

    // 第二轮：Flaky 已经恢复，但脚本 provider 用完会返回不可重试的错误。
    let agent = h.agent(Arc::new(ScriptedProvider::default()), Arc::new(AutoApprove));
    let outcome = agent.run_turn(&mut h.session, "again".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Error);
    assert!(outcome.error.is_some());
}

#[tokio::test]
async fn truncated_reply_continues_the_turn() {
    let mut h = Harness::new();
    // 第一条回复写到一半就达到输出上限：工具参数不是完整 JSON。
    let truncated = vec![
        StreamEvent::MessageStart { model: Some("mock".into()), input_tokens: Some(1) },
        StreamEvent::BlockStart { index: 0, kind: dshr_llm::BlockKind::ToolUse { id: "c1".into(), name: "write_file".into() } },
        StreamEvent::ToolInputDelta { index: 0, partial_json: r#"{"path": "big.txt", "content": "很长"#.into() },
        StreamEvent::MessageDelta { stop_reason: Some(StopReason::MaxTokens), output_tokens: Some(8192) },
        StreamEvent::MessageStop,
    ];
    let provider = Arc::new(ScriptedProvider::new([truncated, ScriptedProvider::text_reply("分段写完了")]));
    let agent = h.agent(provider.clone(), Arc::new(AutoApprove));
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = agent.run_turn(&mut h.session, "写个大文件".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert_eq!(outcome.final_text, "分段写完了");
    assert!(h.kinds().contains(&"turn/continue"));
    assert!(!h.kinds().contains(&"tool/call"), "截断的调用不执行");

    // 第二次请求里带着继续的提示，且没有残缺的工具调用。
    let second = &provider.requests()[1];
    let last = second.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(matches!(last.content.last(), Some(ContentBlock::Text { text }) if text.contains("output token limit")));
    assert!(second.messages.iter().all(|message| message.content.iter().all(|block| !matches!(block, ContentBlock::ToolUse { .. }))));
}

#[tokio::test]
async fn delegate_runs_a_subagent_and_returns_its_report() {
    let mut h = Harness::new();
    // 父代理委派 → 子代理列目录后报告 → 父代理收尾。脚本按请求顺序回放。
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("d1", "delegate", json!({"task": "看看有哪些文件"})),
        ScriptedProvider::tool_reply("s1", "list_dir", json!({"path": "."})),
        ScriptedProvider::text_reply("子代理报告：目录是空的"),
        ScriptedProvider::text_reply("好的"),
    ]));
    let agent = h.agent(provider.clone(), Arc::new(AutoApprove));
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = agent.run_turn(&mut h.session, "委派一下".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert_eq!(outcome.final_text, "好的");
    let result = h.session.events().iter().find_map(|event| match &event.body {
        EventBody::ToolResult { call_id, output, is_error } if call_id == "d1" => Some((output.clone(), *is_error)),
        _ => None,
    });
    assert_eq!(result, Some(("子代理报告：目录是空的".into(), false)));

    // 子代理的工具调用作为进度推到 delegate 卡片上。
    drop(tx);
    let mut progress = String::new();
    while let Some(event) = rx.recv().await {
        if let LiveEvent::Chunk(Chunk::ToolOutput { call_id, text }) = event {
            assert_eq!(call_id, "d1");
            progress.push_str(&text);
        }
    }
    assert!(progress.contains("→ list_dir"));

    // 子代理不再提供 delegate；它的日志在会话目录的 agents/ 下。
    let requests = provider.requests();
    assert!(requests[0].tools.iter().any(|tool| tool.name == "delegate"));
    assert!(!requests[1].tools.iter().any(|tool| tool.name == "delegate"));
    assert!(h.session.agents_dir().join("d1.jsonl").exists());
}

#[tokio::test]
async fn preset_and_attachments_reach_the_request() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([ScriptedProvider::text_reply("方案如下")]));
    let config = AgentConfig::default().with(Preset::Plan);
    let agent = Agent::new(provider.clone(), builtin_plugins(), Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), config);
    let (tx, _rx) = mpsc::unbounded_channel();
    let attachments = vec![crate::UserAttachment::Text("notes.md".to_owned(), "要点一".to_owned())];
    agent.run_turn_with(&mut h.session, "看看这个".into(), attachments, &tx, &CancellationToken::new()).await.unwrap();

    let request = &provider.requests()[0];
    assert!(request.system.as_deref().unwrap().contains("Planning mode"));
    let user = &request.messages[0];
    assert!(
        matches!(&user.content[1], ContentBlock::Text { text } if text.contains("<attachment path=\"notes.md\">") && text.contains("要点一"))
    );
    // 预设不写进日志，附件写进日志。
    assert!(h.kinds().contains(&"user/attachment"));
    assert!(!h.session.events().iter().any(|event| matches!(&event.body, EventBody::SystemMessage { text } if text.contains("Planning"))));
}

#[tokio::test]
async fn image_attachments_reach_models_directly() {
    use base64::Engine as _;

    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([ScriptedProvider::text_reply("看到了")]));
    let agent = Agent::new(
        provider.clone(),
        builtin_plugins(),
        Arc::new(AutoApprove),
        ToolContext::new(h.dir.path().to_owned()),
        AgentConfig::default(),
    );
    let (tx, _rx) = mpsc::unbounded_channel();
    let attachments = vec![
        crate::UserAttachment::Image {
            path: "shot.png".into(),
            media_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode([137, 80, 78, 71]),
        },
        crate::UserAttachment::Text("notes.md".into(), "要点".into()),
    ];
    agent.run_turn_with(&mut h.session, "图上有什么？".into(), attachments, &tx, &CancellationToken::new()).await.unwrap();

    // 所有模型都直接收到 Image 块，不再被替换为占位说明。
    let request = provider.requests().remove(0);
    let user = &request.messages[0];
    assert!(matches!(&user.content[1], ContentBlock::Image { source } if source.media_type == "image/png" && &*source.data == "iVBORw=="));
    assert!(matches!(&user.content[2], ContentBlock::Text { text } if text.contains("<attachment path=\"notes.md\">")));
    // 日志里图片仍然是 base64。
    assert!(h.session.events().iter().any(|event| matches!(
        &event.body,
        EventBody::UserAttachment { path, media_type: Some(media_type), data: Some(_), .. }
            if path == "shot.png" && media_type == "image/png"
    )));
}

/// 贡献一段提示词和一个运行时才出现的工具的插件。
struct Extras;

struct Shout;

#[async_trait::async_trait]
impl dshr_tools::Tool for Shout {
    fn name(&self) -> &str {
        "shout"
    }

    fn description(&self) -> &str {
        "Uppercase the text."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }

    fn access(&self, _input: &serde_json::Value, _ctx: &ToolContext) -> dshr_tools::Access {
        dshr_tools::Access::External
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::ok(input["text"].as_str().unwrap_or_default().to_uppercase())
    }
}

#[async_trait::async_trait]
impl dshr_tools::ToolSource for Extras {
    async fn tools(&self) -> Vec<Arc<dyn dshr_tools::Tool>> {
        vec![Arc::new(Shout)]
    }
}

impl PromptContributor for Extras {
    fn contribute(&self, cx: &PromptContext<'_>) -> Option<String> {
        Some(format!("Workspace note for {}", cx.workspace.display()))
    }
}

impl dshr_context::Plugin for Extras {
    fn manifest(&self) -> dshr_context::Manifest {
        dshr_context::Manifest::new("extras")
    }

    fn apply(&self, cx: &mut dshr_context::Scope) -> anyhow::Result<()> {
        cx.contribute::<PromptParts>(Arc::new(Extras));
        cx.contribute::<dshr_tools::ToolSources>(Arc::new(Extras));
        Ok(())
    }
}

#[tokio::test]
async fn prompt_parts_and_tool_sources_reach_the_model() {
    let mut h = Harness::new();
    let plugins = builtin_plugins();
    plugins.add(Extras).unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "shout", json!({"text": "hi"})),
        ScriptedProvider::text_reply("好了"),
    ]));
    let run = |approver: Arc<dyn Approver>, mode| {
        let config = AgentConfig::default().with(mode).with(Preset::Review);
        Agent::new(provider.clone(), plugins.clone(), approver, ToolContext::new(h.dir.path().to_owned()), config)
    };
    let (tx, _rx) = mpsc::unbounded_channel();

    // 外部工具在“工作区内修改”下要询问；这里用完全访问直接放行。
    let agent = run(Arc::new(DenyAll), PermissionMode::FullAccess);
    agent.run_turn(&mut h.session, "喊一声".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert!(matches!(&tool_results(&h.session)[..], [(false, output)] if output == "HI"));

    let request = &provider.requests()[0];
    assert!(request.tools.iter().any(|tool| tool.name == "shout"));
    // 顺序：系统提示词、插件追加的内容、预设说明；都不写进日志。
    let system = request.system.as_deref().unwrap();
    let (base, note, preset) = (system.find("DSHR").unwrap(), system.find("Workspace note").unwrap(), system.find("Review mode").unwrap());
    assert!(base < note && note < preset);
    assert!(
        !h.session.events().iter().any(|event| matches!(&event.body, EventBody::SystemMessage { text } if text.contains("Workspace note")))
    );
}

#[tokio::test]
async fn external_tools_ask_even_with_a_confining_sandbox() {
    let mut h = Harness::new();
    let plugins = Context::new();
    plugins.add(PermissionPlugin).unwrap();
    // 沙箱只管得住 run_shell；外部工具不在沙箱里，照样询问。
    plugins.add(ConfiningPlugin).unwrap();
    plugins.add(Extras).unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "shout", json!({"text": "hi"})),
        ScriptedProvider::text_reply("没喊成"),
    ]));
    let agent = Agent::new(provider, plugins, Arc::new(DenyAll), ToolContext::new(h.dir.path().to_owned()), AgentConfig::default());
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut h.session, "喊一声".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert!(matches!(&tool_results(&h.session)[..], [(true, output)] if output.contains("denied")));
}
/// 一个自带事件类型的插件：工具把要点记成 `notes/add` 事件，projector 把它们放进之后的上下文，
/// 每轮结束时再记一条不进上下文的 `notes/turn`。
struct Notes;

struct TakeNote;

#[async_trait::async_trait]
impl dshr_tools::Tool for TakeNote {
    fn name(&self) -> &str {
        "take_note"
    }

    fn description(&self) -> &str {
        "Remember a note."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolOutput {
        ctx.emit_event("notes/add", json!({"text": input["text"]}));
        // 名字不合规的事件被丢弃，不影响调用本身。
        ctx.emit_event("session/title", json!({"title": "冒充核心事件"}));
        ctx.emit_event("no-namespace", json!({}));
        ToolOutput::ok("noted")
    }
}

impl dshr_core::EventProjector for Notes {
    fn kind(&self) -> &str {
        "notes/add"
    }

    fn project(&self, data: &serde_json::Value) -> Option<String> {
        Some(format!("[note] {}", data["text"].as_str()?))
    }
}

#[async_trait::async_trait]
impl TurnEndedHook for Notes {
    async fn turn_ended(&self, turn: &mut TurnContext<'_>, outcome: &TurnOutcome) -> Result<(), AgentError> {
        assert!(matches!(turn.append_ext("turn/note", json!({})), Err(AgentError::EventKind(_))));
        turn.append_ext("notes/turn", json!({"completed": outcome.reason == TurnEndReason::Completed}))
    }
}

impl dshr_context::Plugin for Notes {
    fn manifest(&self) -> dshr_context::Manifest {
        dshr_context::Manifest::new("notes")
    }

    fn apply(&self, cx: &mut dshr_context::Scope) -> anyhow::Result<()> {
        cx.contribute::<dshr_tools::Tools>(Arc::new(TakeNote));
        cx.contribute::<EventKinds>(Arc::new(Notes));
        cx.hook::<TurnEnded>(0, Arc::new(Notes));
        Ok(())
    }
}

#[tokio::test]
async fn plugin_events_are_logged_and_projected_while_the_plugin_is_active() {
    let mut h = Harness::new();
    let plugins = builtin_plugins();
    plugins.add(Notes).unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "take_note", json!({"text": "周五发版"})),
        ScriptedProvider::text_reply("记下了"),
        ScriptedProvider::text_reply("周五"),
        ScriptedProvider::text_reply("不记得了"),
    ]));
    let agent = Agent::new(
        provider.clone(),
        plugins.clone(),
        Arc::new(AutoApprove),
        ToolContext::new(h.dir.path().to_owned()),
        AgentConfig::default(),
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    agent.run_turn(&mut h.session, "记一下：周五发版".into(), &tx, &cancel).await.unwrap();

    // 事件写在工具结果之后；一轮结束的钩子写的排在 `turn/end` 后面。名字不合规的两条没有进日志。
    let kinds = h.kinds();
    let call = kinds.iter().position(|kind| *kind == "tool/call").unwrap();
    assert_eq!(kinds[call..call + 3], ["tool/call", "tool/result", "notes/add"]);
    assert_eq!(kinds[kinds.len() - 2..], ["turn/end", "notes/turn"]);
    assert_eq!(kinds.iter().filter(|kind| kind.starts_with("notes/")).count(), 2);
    assert!(!h.session.has_title());

    // 订阅方（界面）也收到了这些事件。
    let mut live = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let LiveEvent::Durable(event) = event
            && let EventBody::Ext { kind, .. } = &event.body
            && kind.starts_with("notes/")
        {
            live.push(kind.clone());
        }
    }
    assert_eq!(live, ["notes/add", "notes/turn"]);

    // 同一步的下一次请求里，事件文本跟在工具结果后面。
    let texts = |request: &ChatRequest| -> Vec<String> {
        request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                // 环境说明（日期、权限模式）不是这个测试关心的。
                ContentBlock::Text { text } if text.starts_with("<environment>") => None,
                ContentBlock::Text { text } => Some(text.to_string()),
                ContentBlock::ToolResult { content, .. } => Some(format!("result:{content}")),
                _ => None,
            })
            .collect()
    };
    assert_eq!(texts(&provider.requests()[1]), ["记一下：周五发版", "result:noted", "[note] 周五发版"]);

    // 重新打开日志，事件还在，下一轮照样投影。
    let path = h.dir.path().join("log").join("session.v1.jsonl");
    let mut reopened = Session::open("s1", &path).unwrap();
    assert_eq!(reopened.events(), h.session.events());
    agent.run_turn(&mut reopened, "哪天发版？".into(), &tx, &cancel).await.unwrap();
    assert!(texts(&provider.requests()[2]).contains(&"[note] 周五发版".to_owned()));

    // 插件停用后事件仍留在日志里，只是不再进上下文。
    plugins.disable("notes").unwrap();
    agent.run_turn(&mut reopened, "再说一遍".into(), &tx, &cancel).await.unwrap();
    let last = &provider.requests()[3];
    assert!(!texts(last).iter().any(|text| text.starts_with("[note]")));
    assert!(!last.tools.iter().any(|tool| tool.name == "take_note"));
    assert_eq!(reopened.events().iter().filter(|event| event.body.kind() == "notes/add").count(), 1);
}

/// 发完回复后连接一直不关闭的服务商（有的网关发完 `message_stop` / `[DONE]` 不断开）。
struct NeverClosing(TitleScripts);

#[async_trait::async_trait]
impl Provider for NeverClosing {
    fn id(&self) -> &str {
        "never-closing"
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        let reply = self.0.stream(request).await?;
        Ok(Box::pin(reply.chain(futures::stream::pending())))
    }
}

#[tokio::test]
async fn turn_ends_even_if_the_connection_stays_open() {
    let mut h = Harness::new();
    let provider = Arc::new(NeverClosing(TitleScripts {
        main: ScriptedProvider::new([ScriptedProvider::text_reply("你好")]),
        title: ScriptedProvider::new([ScriptedProvider::text_reply("打招呼")]),
        gate: None,
    }));
    let plugins = builtin_plugins();
    plugins.add(TitlePlugin).unwrap();
    let agent = Agent::new(provider, plugins, Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), AgentConfig::default());
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut changes = title_changes();
    let outcome =
        tokio::time::timeout(Duration::from_secs(10), agent.run_turn(&mut h.session, "hi".into(), &tx, &CancellationToken::new()))
            .await
            .expect("回复结束后这一轮应当自己结束")
            .unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert_eq!(outcome.final_text, "你好");

    wait_for_title_write(&h.session, &mut changes).await;
    // The title may already have been synchronized by a later dialogue event.
    h.session.refresh_title();
    assert_eq!(h.session.title(), Some("打招呼"));
}

/// 日期和权限模式的说明只在变了的时候追加：系统提示词和之前的对话前缀保持不变，提示词缓存才能一直命中。
#[tokio::test]
async fn environment_note_is_appended_only_when_it_changes() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::text_reply("一"),
        ScriptedProvider::text_reply("二"),
        ScriptedProvider::text_reply("三"),
    ]));
    let (tx, _rx) = mpsc::unbounded_channel();
    let notes = |session: &Session| session.events().iter().filter(|event| event.body.kind() == crate::prompt::TURN_ENVIRONMENT).count();
    for (turn, mode) in [PermissionMode::Workspace, PermissionMode::Workspace, PermissionMode::Manual].into_iter().enumerate() {
        let config = AgentConfig::default().with(mode);
        let agent =
            Agent::new(provider.clone(), builtin_plugins(), Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), config);
        agent.run_turn(&mut h.session, format!("第 {turn} 轮"), &tx, &CancellationToken::new()).await.unwrap();
    }
    assert_eq!(notes(&h.session), 2, "同样的模式不重复写，换了模式写一条新的");
    let requests = provider.requests();
    // 三次请求的系统提示词一模一样；第二次请求的消息以第一次的消息开头（只在末尾追加）。
    assert!(requests.windows(2).all(|pair| pair[0].system == pair[1].system));
    assert_eq!(requests[1].messages[0], requests[0].messages[0]);
    assert!(request_text(&requests[2]).contains("Permission mode: manual"));
}

/// 记录每次执行的开始、结束时间的测试工具；`write` 为真时是写入工具。
struct Timed {
    name: &'static str,
    write: bool,
    log: Arc<std::sync::Mutex<Vec<(String, std::time::Instant, std::time::Instant)>>>,
}

#[async_trait::async_trait]
impl dshr_tools::Tool for Timed {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "test"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    fn access(&self, _input: &serde_json::Value, ctx: &ToolContext) -> dshr_tools::Access {
        if self.write { dshr_tools::Access::Write(ctx.workspace.join("x")) } else { dshr_tools::Access::Read }
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolOutput {
        let start = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let tag = input["tag"].as_str().unwrap_or_default().to_owned();
        self.log.lock().unwrap().push((tag.clone(), start, std::time::Instant::now()));
        ToolOutput::ok(tag)
    }
}

struct TimedTools(Arc<std::sync::Mutex<Vec<(String, std::time::Instant, std::time::Instant)>>>);

impl dshr_context::Plugin for TimedTools {
    fn manifest(&self) -> dshr_context::Manifest {
        dshr_context::Manifest::new("timed")
    }

    fn apply(&self, cx: &mut dshr_context::Scope) -> anyhow::Result<()> {
        cx.contribute::<dshr_tools::Tools>(Arc::new(Timed { name: "slow_read", write: false, log: self.0.clone() }));
        cx.contribute::<dshr_tools::Tools>(Arc::new(Timed { name: "slow_write", write: true, log: self.0.clone() }));
        Ok(())
    }
}

/// 同一步里连续的只读调用同时执行；写入调用把它们隔开，按顺序单独执行。每个调用都有结果。
#[tokio::test]
async fn read_only_tool_calls_in_one_step_run_concurrently() {
    let mut h = Harness::new();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let plugins = builtin_plugins();
    plugins.add(TimedTools(log.clone())).unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_calls_reply(&[
            ("r1", "slow_read", json!({"tag": "r1"})),
            ("r2", "slow_read", json!({"tag": "r2"})),
            ("r3", "slow_read", json!({"tag": "r3"})),
            ("w1", "slow_write", json!({"tag": "w1"})),
            ("r4", "slow_read", json!({"tag": "r4"})),
        ]),
        ScriptedProvider::text_reply("好"),
    ]));
    let config = AgentConfig::default().with(PermissionMode::FullAccess);
    let agent = Agent::new(provider.clone(), plugins, Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), config);
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut h.session, "并行".into(), &tx, &CancellationToken::new()).await.unwrap();
    // 不看总耗时：CI 机器忙的时候墙钟会被拉长（曾经 916 ms 超过 900 ms 的上限）。
    // 下面直接比较每个调用的起止时间，重叠就说明是同时执行的。

    let log = log.lock().unwrap();
    let span = |tag: &str| log.iter().find(|(name, ..)| name == tag).map(|(_, start, end)| (*start, *end)).unwrap();
    let (r1, r2, r3, w1, r4) = (span("r1"), span("r2"), span("r3"), span("w1"), span("r4"));
    assert!(r2.0 < r1.1 && r3.0 < r1.1, "r1–r3 应当同时执行");
    assert!(w1.0 >= r1.1.max(r2.1).max(r3.1), "写入要等前面的读取都结束");
    assert!(r4.0 >= w1.1, "写入之后的读取要等写入结束");

    // 五个调用都有结果，且都在下一次请求里。
    let results = tool_results(&h.session);
    assert_eq!(results.len(), 5);
    let request = &provider.requests()[1];
    let answered = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .count();
    assert_eq!(answered, 5);
}

#[test]
fn retry_waits_follow_retry_after_then_exponential_backoff() {
    let busy = |retry_after| LlmError::Http { status: 429, body: String::new(), retry_after };
    let base = Duration::from_secs(1);
    assert_eq!(retry_wait(&busy(Some(Duration::from_secs(7))), base, 1), Duration::from_secs(7));
    assert_eq!(retry_wait(&busy(Some(Duration::from_secs(600))), base, 1), MAX_RETRY_WAIT);
    for (attempt, nominal) in [(1u32, 1.0f64), (2, 2.0), (3, 4.0)] {
        let wait = retry_wait(&busy(None), base, attempt).as_secs_f64();
        assert!(wait >= nominal * 0.75 && wait <= nominal * 1.25, "第 {attempt} 次：{wait}");
    }
    assert!(LlmError::Interrupted("read timeout".into()).is_retryable());
    assert!(LlmError::Http { status: 408, body: String::new(), retry_after: None }.is_retryable());
    assert!(!LlmError::Http { status: 400, body: String::new(), retry_after: None }.is_retryable());
}

/// 第一轮里连续调用工具、上下文满了：在步骤边界压缩，不用等这一轮结束。
#[tokio::test]
async fn compacts_mid_turn_at_a_step_boundary() {
    let mut h = Harness::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "list_dir", json!({"path": "."})),
        // 第二步之前上下文已满：先摘要，再接着做。
        ScriptedProvider::text_reply("用户要列目录，已经列过一次。"),
        ScriptedProvider::text_reply("做完了"),
    ]));
    let config = AgentConfig { context_window: 100, retry_delay: Duration::from_millis(1), ..AgentConfig::default() };
    let agent = Agent::new(provider.clone(), builtin_plugins(), Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), config);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = agent.run_turn(&mut h.session, "列目录".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert_eq!(outcome.final_text, "做完了");

    let events = h.session.events();
    let upto = events.iter().find_map(|event| match event.body {
        EventBody::Compaction { upto_seq, .. } => Some(upto_seq),
        _ => None,
    });
    let step_end = events.iter().find(|event| matches!(event.body, EventBody::StepEnd { step: 1, .. })).map(|event| event.seq);
    assert_eq!(upto, step_end, "压缩到第一步结束为止");
    // 压缩之后的请求以摘要开头，后面跟着重新写的环境说明；第一步的工具结果不在里面。
    let last = provider.requests().pop().unwrap();
    let ContentBlock::Text { text } = &last.messages[0].content[0] else { panic!() };
    assert!(text.starts_with(dshr_core::COMPACTION_PREFIX));
    assert!(request_text(&last).contains("Permission mode:"));
    assert!(!last.messages.iter().flat_map(|message| &message.content).any(|block| matches!(block, ContentBlock::ToolResult { .. })));
}

/// 报告的用量很小（比如中文按字节估算会高估很多）时，以模型报告的为准，不提前压缩。
struct SmallUsage(ScriptedProvider);

#[async_trait::async_trait]
impl Provider for SmallUsage {
    fn id(&self) -> &str {
        "small-usage"
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        let inner = self.0.stream(request).await?;
        Ok(Box::pin(inner.map(|event| match event {
            Ok(StreamEvent::MessageStart { model, .. }) => Ok(StreamEvent::MessageStart { model, input_tokens: Some(10) }),
            other => other,
        })))
    }
}

#[tokio::test]
async fn compaction_trusts_reported_usage() {
    let mut h = Harness::new();
    let provider = Arc::new(SmallUsage(ScriptedProvider::new([
        ScriptedProvider::text_reply("第一轮回答"),
        ScriptedProvider::text_reply("第二轮回答"),
    ])));
    // 按字节估算，光系统提示词就超过 100 的 75%；模型报告的只有十几个 token。
    let config = AgentConfig { context_window: 100, retry_delay: Duration::from_millis(1), ..AgentConfig::default() };
    let agent = Agent::new(provider, builtin_plugins(), Arc::new(AutoApprove), ToolContext::new(h.dir.path().to_owned()), config);
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut h.session, "第一问".into(), &tx, &CancellationToken::new()).await.unwrap();
    agent.run_turn(&mut h.session, "第二问".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert!(!h.kinds().contains(&"context/compaction"));
}
