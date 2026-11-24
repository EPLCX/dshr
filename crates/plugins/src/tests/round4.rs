//! 移植插件时补上的接口：同步钩子里读写数据、钩子之间共享模块变量、钩子参数里的会话和代理、
//! `beforeRequest` 补 user 消息、结束本轮、`appendData` / `appendFile`、`host.db`、`host.zip`、
//! `llm.complete` 指定模型和思考等级。

use super::*;

use dshr_agent::{AgentError, Approver, LlmAccess, TurnOutcome};
use dshr_core::TurnEndReason;
use dshr_core::{ContentBlock, Role};

fn agent_context(f: &Fixture) -> Context {
    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    plugins.add_group(AgentPlugins).unwrap();
    plugins.add_group(JsPlugins::discover(&f.plugins_dir())).unwrap();
    plugins
}

/// 跑一轮，返回会话、结果和模型收到的请求。
async fn turn(
    f: &Fixture,
    replies: Vec<Vec<dshr_llm::StreamEvent>>,
    text: &str,
) -> (Session, Result<TurnOutcome, AgentError>, Vec<dshr_llm::ChatRequest>) {
    let provider = Arc::new(ScriptedProvider::new(replies));
    let approver: Arc<dyn Approver> = Arc::new(AutoApprove);
    let agent =
        Agent::new(provider.clone(), agent_context(f), approver, f.tool_context(), AgentConfig::default().with(PermissionMode::FullAccess));
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut session = Session::create(format!("r4-{run}"), f.home.path().join(format!("log-{run}")).join("session.v1.jsonl")).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = agent.run_turn(&mut session, text.into(), &tx, &CancellationToken::new()).await;
    (session, outcome, provider.requests())
}

fn data(f: &Fixture, plugin: &str, file: &str) -> Option<String> {
    std::fs::read_to_string(f.home.path().join("plugin-data").join(plugin).join(file)).ok()
}

fn last_user_text(request: &dshr_llm::ChatRequest) -> String {
    let last = request.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    match last.content.last().unwrap() {
        ContentBlock::Text { text } => text.to_string(),
        other => panic!("最后一块不是文本：{other:?}"),
    }
}

#[tokio::test]
async fn sync_hooks_read_and_write_data_and_share_module_state() {
    let f = Fixture::new();
    // beforeRequest、beforeWrite（同步钩子）和 afterTool（异步钩子）、工具共用模块变量和数据目录。
    f.plugin(
        "review",
        "",
        r#"let pending = [];
        let requests = 0;
        export const hooks = {
          beforeWrite: async ({ path, session, agent }, host) => {
            pending.push(path);
            await host.appendData("writes.jsonl", JSON.stringify({ path, session, agent }) + "\n");
          },
          afterTool: async ({ tool }, host) => {
            if (tool === "write_file") await host.writeData("flushed", pending.join(","));
          },
          beforeRequest: async ({ session, agent, workspace }, host) => {
            requests += 1;
            const flushed = (await host.readData("flushed")) ?? "none";
            return { append: `[review ${requests}]`, user: `[pending ${flushed} @ ${session}/${agent} ${workspace.length > 0}]` };
          },
        };
        export const tools = [{ name: "whoami", async execute(_i, host) { const { session, agent } = await host.session.current(); return `${session}/${agent}:${pending.length}`; } }];"#,
    );
    let replies = vec![
        ScriptedProvider::tool_reply("c1", "write_file", json!({ "path": "a.txt", "content": "1" })),
        ScriptedProvider::tool_reply("c2", "whoami", json!({})),
        ScriptedProvider::text_reply("done"),
    ];
    let (session, outcome, requests) = turn(&f, replies, "go").await;
    outcome.unwrap();
    let id = session.id().to_owned();

    // 同步的 beforeWrite 写了数据，异步的 afterTool 看到了它攒在模块变量里的路径。
    let line: Value = serde_json::from_str(data(&f, "review", "writes.jsonl").unwrap().trim()).unwrap();
    assert_eq!(line, json!({ "path": "a.txt", "session": id, "agent": "main" }));
    assert_eq!(data(&f, "review", "flushed").as_deref(), Some("a.txt"));

    // beforeRequest：每次请求都调，读得到数据，追加的系统提示词和 user 文本只在请求里。
    assert_eq!(requests.len(), 3);
    assert!(requests[0].system.as_deref().unwrap().ends_with("[review 1]"));
    assert_eq!(last_user_text(&requests[0]), format!("[pending none @ {id}/main true]"));
    assert!(requests[1].system.as_deref().unwrap().ends_with("[review 2]"));
    assert_eq!(last_user_text(&requests[1]), format!("[pending a.txt @ {id}/main true]"));
    // 并进了最后一条 user 消息（工具结果）里，不新起一条。
    assert!(requests[1].messages.last().unwrap().content.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. })));
    let logged = serde_json::to_string(session.events()).unwrap();
    assert!(!logged.contains("[pending") && !logged.contains("[review"), "补的内容不进日志");

    // 工具里也能知道当前会话和代理。
    let whoami = session
        .events()
        .iter()
        .find_map(|event| match &event.body {
            EventBody::ToolResult { call_id, output, .. } if call_id == "c2" => Some(output.to_string()),
            _ => None,
        })
        .unwrap();
    assert_eq!(whoami, format!("{id}/main:1"));
}

#[tokio::test]
async fn tools_and_after_tool_hooks_can_end_the_turn() {
    let f = Fixture::new();
    f.plugin(
        "emitter",
        "",
        r#"export const tools = [{ name: "emit", async execute({ text }, host) { await host.session.endTurn(); return `emitted ${text}`; } }];
        export const hooks = { afterTool: async ({ tool }, host) => { if (tool === "stop_here") await host.session.endTurn(); } };"#,
    );
    f.plugin(
        "other",
        "",
        r#"export const tools = [{ name: "stop_here", execute: () => "stopped" }, { name: "noop", execute: () => "ok" }];"#,
    );

    // 工具要求结束：这一步的调用都执行完，然后不再请求模型。
    let replies = vec![
        ScriptedProvider::tool_calls_reply(&[("c1", "emit", json!({ "text": "x" })), ("c2", "noop", json!({}))]),
        ScriptedProvider::text_reply("should never be requested"),
    ];
    let (session, outcome, requests) = turn(&f, replies, "go").await;
    let outcome = outcome.unwrap();
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert_eq!(requests.len(), 1);
    let results: Vec<String> = session
        .events()
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolResult { output, .. } => Some(output.to_string()),
            _ => None,
        })
        .collect();
    // 两个调用都是只读的，同时执行，结果的先后不定。
    let mut results = results;
    results.sort();
    assert_eq!(results, ["emitted x", "ok"]);
    assert_eq!(session.events().last().unwrap().body.kind(), "turn/end");

    // afterTool 也可以要求结束。
    let replies = vec![ScriptedProvider::tool_reply("c1", "stop_here", json!({})), ScriptedProvider::text_reply("never")];
    let (_, outcome, requests) = turn(&f, replies, "go").await;
    assert_eq!(outcome.unwrap().reason, TurnEndReason::Completed);
    assert_eq!(requests.len(), 1);

    // 不在一轮里（直接调用工具）时报错。
    let cx = f.context();
    let output = f.call(&cx, "emit", json!({ "text": "y" })).await;
    assert!(output.is_error && output.content.contains("endTurn"), "{output:?}");
}

#[tokio::test]
async fn append_data_and_append_file() {
    let f = Fixture::new();
    f.plugin(
        "log",
        "capabilities = [\"fs:workspace:write\", \"fs:workspace:read\"]\n",
        r#"export const tools = [
          { name: "log", async execute({ line }, host) {
              await host.appendData("records/log.jsonl", line + "\n");
              await host.appendFile("notes.md", line + "\n");
              return await host.readData("records/log.jsonl");
          } },
          { name: "big", async execute(_i, host) {
              // 单个数据文件的上限是 16 MB：一次写 3 MB 没问题。
              await host.writeData("big.txt", "x".repeat(3 * 1024 * 1024));
              return String((await host.readData("big.txt")).length);
          } },
          { name: "peek", readOnly: true, async execute(_i, host) {
              try { await host.appendFile("notes.md", "nope"); return "appended"; } catch (error) { return error.message; }
          } },
        ];"#,
    );
    let cx = f.context();
    assert_eq!(f.call(&cx, "log", json!({ "line": "a" })).await.content, "a\n");
    assert_eq!(f.call(&cx, "log", json!({ "line": "b" })).await.content, "a\nb\n");
    assert_eq!(std::fs::read_to_string(f.workspace.path().join("notes.md")).unwrap(), "a\nb\n");
    assert_eq!(f.call(&cx, "big", json!({})).await.content, (3 * 1024 * 1024).to_string());
    let refused = f.call(&cx, "peek", json!({})).await.content;
    assert!(refused.contains("not allowed here"), "{refused}");
}

#[tokio::test]
async fn sqlite_database_in_the_data_folder() {
    let f = Fixture::new();
    f.plugin(
        "memo",
        "",
        r#"export const tools = [
          { name: "setup", async execute(_i, host) {
              await host.db.exec(`CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, title TEXT NOT NULL, body BLOB, tags TEXT);
                                  CREATE INDEX IF NOT EXISTS notes_title ON notes(title);`);
              const first = await host.db.exec("INSERT INTO notes (title, body, tags) VALUES (?, ?, ?)", ["a", new Uint8Array([1, 2, 3]), ["x", "y"]]);
              const second = await host.db.exec("INSERT INTO notes (title) VALUES (:title)", { title: "b" });
              // 事务里抛异常：回滚。
              try {
                await host.db.transaction(async () => {
                  await host.db.exec("INSERT INTO notes (title) VALUES (?)", ["rolled back"]);
                  throw new Error("nope");
                });
              } catch {}
              await host.db.transaction(async () => { await host.db.exec("INSERT INTO notes (title) VALUES (?)", ["c"]); });
              return JSON.stringify([first, second]);
          } },
          { name: "list", readOnly: true, async execute(_i, host) {
              const rows = await host.db.query("SELECT id, title, body, json_array_length(tags) AS tags FROM notes ORDER BY id");
              const one = await host.db.get("SELECT count(*) AS n FROM notes WHERE title LIKE ?", ["%"]);
              return JSON.stringify({ rows: rows.map((row) => ({ ...row, body: row.body ? Array.from(row.body) : null })), n: one.n });
          } },
          { name: "open_tx", async execute(_i, host) {
              await host.db.exec("BEGIN");
              await host.db.exec("INSERT INTO notes (title) VALUES ('left open')");
              return "ok";
          } },
          { name: "bad", async execute(_i, host) {
              try { await host.db.query("SELECT * FROM missing"); } catch (error) { return error.message; }
          } },
          { name: "other_db", async execute(_i, host) {
              const db = host.database("cache/other.sqlite");
              await db.exec("CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)");
              await db.exec("INSERT OR REPLACE INTO kv VALUES (?, ?)", ["k", "v"]);
              return (await db.get("SELECT v FROM kv WHERE k = ?", ["k"])).v;
          } },
        ];"#,
    );
    let cx = f.context();
    let created: Value = serde_json::from_str(&f.call(&cx, "setup", json!({})).await.content).unwrap();
    assert_eq!(created, json!([{ "changes": 1, "lastInsertRowid": 1 }, { "changes": 1, "lastInsertRowid": 2 }]));
    let listed: Value = serde_json::from_str(&f.call(&cx, "list", json!({})).await.content).unwrap();
    assert_eq!(
        listed,
        json!({ "rows": [
            { "id": 1, "title": "a", "body": [1, 2, 3], "tags": 2 },
            { "id": 2, "title": "b", "body": null, "tags": null },
            { "id": 3, "title": "c", "body": null, "tags": null },
        ], "n": 3 })
    );
    // 一次调用结束时没提交的事务回滚。
    assert_eq!(f.call(&cx, "open_tx", json!({})).await.content, "ok");
    let listed: Value = serde_json::from_str(&f.call(&cx, "list", json!({})).await.content).unwrap();
    assert_eq!(listed["n"], 3);
    assert!(f.call(&cx, "bad", json!({})).await.content.contains("no such table"));
    assert_eq!(f.call(&cx, "other_db", json!({})).await.content, "v");
    assert!(f.home.path().join("plugin-data/memo/db.sqlite").is_file());
    assert!(f.home.path().join("plugin-data/memo/cache/other.sqlite").is_file());
}

#[tokio::test]
async fn zip_and_unzip_in_memory() {
    let f = Fixture::new();
    f.plugin(
        "office",
        "capabilities = [\"fs:workspace:write\", \"fs:workspace:read\"]\n",
        r#"export const tools = [{ name: "roundtrip", async execute(_i, host) {
            const archive = await host.zip([
              { path: "mimetype", data: "application/epub+zip", method: "store" },
              { path: "META-INF", dir: true },
              { path: "META-INF/container.xml", data: "<container/>" },
              { path: "bin/data", data: new Uint8Array([0, 255, 7]) },
            ]);
            await host.writeFileBytes("out.zip", archive);
            const entries = await host.unzip(await host.readFileBytes("out.zip"));
            let refused;
            try { await host.zip([{ path: "../evil", data: "x" }]); } catch (error) { refused = error.message; }
            return JSON.stringify({
              names: entries.map((entry) => entry.dir ? entry.path + "/" : entry.path),
              mimetype: entries[0].text(),
              bytes: Array.from(entries[3].data),
              refused,
            });
        } }];"#,
    );
    let cx = f.context();
    let result: Value = serde_json::from_str(&f.call(&cx, "roundtrip", json!({})).await.content).unwrap();
    assert_eq!(result["names"], json!(["mimetype", "META-INF/", "META-INF/container.xml", "bin/data"]));
    assert_eq!(result["mimetype"], "application/epub+zip");
    assert_eq!(result["bytes"], json!([0, 255, 7]));
    assert!(result["refused"].as_str().unwrap().contains("inside the archive"));
    // 真正的 zip 文件：别的工具能打开，`mimetype` 没有压缩。
    let mut archive = zip::ZipArchive::new(std::fs::File::open(f.workspace.path().join("out.zip")).unwrap()).unwrap();
    assert_eq!(archive.by_index(0).unwrap().compression(), zip::CompressionMethod::Stored);
}

#[tokio::test]
async fn llm_complete_can_pick_the_model_and_effort() {
    let f = Fixture::new();
    f.plugin(
        "advisor",
        "capabilities = [\"llm\"]\n",
        r#"export const tools = [{ name: "advise", async execute({ model, effort }, host) {
            try { return await host.llm.complete("hi", { model, effort }); } catch (error) { return `error: ${error.message}`; }
        } }];"#,
    );
    let current =
        Arc::new(ScriptedProvider::new([ScriptedProvider::text_reply("from current"), ScriptedProvider::text_reply("same provider")]));
    let reviewer = Arc::new(ScriptedProvider::new([ScriptedProvider::text_reply("from reviewer")]));
    let resolved = reviewer.clone();
    crate::app::set_app_services(|services| {
        services.models = Some(Arc::new(move |spec: &str| {
            (spec == "review/judge").then(|| LlmAccess { provider: resolved.clone(), model: "judge".into() })
        }));
    });
    let cx = f.context();
    let mut ctx = f.tool_context();
    ctx.options.insert(LlmAccess { provider: current.clone(), model: "chat".into() });
    let tool = f.tool(&cx, "advise");
    let call = |input: Value| {
        let (tool, ctx) = (tool.clone(), ctx.clone());
        async move { tool.execute(input, &ctx).await.content }
    };

    assert_eq!(call(json!({})).await, "from current");
    assert_eq!(call(json!({ "model": "review/judge", "effort": "high" })).await, "from reviewer");
    assert_eq!(call(json!({ "model": "chat-lite", "effort": "off" })).await, "same provider");
    assert!(call(json!({ "model": "nowhere/x" })).await.contains("not available"));
    assert!(call(json!({ "effort": "huge" })).await.contains("unknown effort"));

    let sent = current.requests();
    assert_eq!((sent[0].model.as_str(), sent[0].effort), ("chat", None));
    assert_eq!((sent[1].model.as_str(), sent[1].effort), ("chat-lite", Some(dshr_llm::ReasoningEffort::Off)));
    let judged = reviewer.requests();
    assert_eq!((judged[0].model.as_str(), judged[0].effort), ("judge", Some(dshr_llm::ReasoningEffort::High)));
}

#[tokio::test]
async fn hooks_tell_the_main_agent_from_sub_agents() {
    let f = Fixture::new();
    f.plugin(
        "advisor",
        "",
        r#"export const hooks = { beforeRequest: async ({ session, agent }, host) => { await host.appendData("seen", `${session}/${agent}\n`); } };"#,
    );
    // 和主程序一样：主代理和子代理用同一组 JS 插件。
    let scripts = JsPlugins::discover(&f.plugins_dir());
    let sub = Context::new();
    sub.add_group(dshr_tools::BuiltinTools).unwrap();
    sub.add_group(AgentPlugins).unwrap();
    sub.add_group(scripts.clone()).unwrap();
    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    plugins.add_group(AgentPlugins).unwrap();
    plugins.add(dshr_agent::DelegatePlugin::new(sub)).unwrap();
    plugins.add_group(scripts).unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("call-7", "delegate", json!({ "task": "look around" })),
        ScriptedProvider::text_reply("sub report"),
        ScriptedProvider::text_reply("done"),
    ]));
    let agent =
        Agent::new(provider, plugins, Arc::new(AutoApprove), f.tool_context(), AgentConfig::default().with(PermissionMode::FullAccess));
    let mut session = Session::create("main-session", f.home.path().join("log").join("session.v1.jsonl")).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut session, "go".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert_eq!(data(&f, "advisor", "seen").unwrap(), "main-session/main\nmain-session/call-7\nmain-session/main\n");
}

#[tokio::test]
async fn plugins_read_their_own_translations() {
    let f = Fixture::new();
    f.plugin(
        "polyglot",
        "default_locale = \"en\"\n[config.properties.count]\ntype = \"integer\"\ntitle = \"数量\"\n",
        r#"export const tools = [{ name: "greet", readOnly: true, async execute(_input, host) {
            return JSON.stringify({
              locale: host.i18n.locale,
              locales: host.i18n.locales.sort(),
              hello: host.i18n.t("hello", { name: "dshr" }),
              title: host.i18n.t("config.count.title"),
              missing: host.i18n.t("nope"),
              picked: host.i18n.pick({ "zh-CN": "简体", en: "English" }),
            });
        } }];"#,
    );
    let dir = f.plugins_dir().join("polyglot").join("locales");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("en.json"), r#"{ "hello": "Hello, {name}", "config": { "count": { "title": "Count" } } }"#).unwrap();
    std::fs::write(dir.join("zh-CN.json"), r#"{ "hello": "你好，{name}" }"#).unwrap();
    crate::locale::set_locale("zh-HK");
    let cx = f.context();
    let output = f.call(&cx, "greet", json!({})).await;
    assert!(!output.is_error, "{output:?}");
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["locale"], "zh-HK");
    assert_eq!(value["locales"], json!(["en", "zh-CN"]));
    assert_eq!(value["hello"], "你好，dshr");
    // 中文没有这一条：回退到默认语言（英文）。
    assert_eq!(value["title"], "Count");
    assert_eq!(value["missing"], "nope");
    assert_eq!(value["picked"], "简体");
}
