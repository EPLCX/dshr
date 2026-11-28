//! 扩展能力的测试：异步宿主调用与定时器、基础全局、模块、文件函数、exec、llm、会话、密钥、钩子、
//! 生命周期、后台任务的检查、卡片与事件的节点树。

use super::*;

use dshr_agent::UiSurfaces;

/// 只有一个工具 `run` 的插件：执行 `body`（一个 async 函数体，参数是 `input` 和 `host`）。
fn runner(body: &str) -> String {
    format!(
        "export const tools = [{{ name: \"run\", async execute(input, host) {{ {body} }} }}];\n\
         export const peek = 1;\n"
    )
}

#[tokio::test]
async fn timers_globals_and_modules() {
    let f = Fixture::new();
    let dir = f.plugins_dir().join("globals");
    std::fs::create_dir_all(dir.join("vendor")).unwrap();
    std::fs::write(dir.join("data.json"), r#"{"answer": 42}"#).unwrap();
    std::fs::write(dir.join("vendor/lib.js"), "export const shout = (text) => text.toUpperCase();").unwrap();
    f.plugin(
        "globals",
        "[imports]\nshout-lib = \"./vendor/lib.js\"\n",
        r#"import data from "./data.json";
        import { shout } from "shout-lib";
        import path from "dshr:path";
        import { EventEmitter } from "dshr:events";
        export const tools = [{
          name: "globals",
          async execute() {
            const order = [];
            setTimeout(() => order.push("slow"), 30);
            setTimeout(() => order.push("fast"), 5);
            const cleared = setTimeout(() => order.push("never"), 1);
            clearTimeout(cleared);
            let ticks = 0;
            const interval = setInterval(() => { ticks++; if (ticks === 3) clearInterval(interval); }, 2);
            await Promise.all([
              new Promise((resolve) => setTimeout(resolve, 40)),
              new Promise((resolve) => queueMicrotask(resolve)),
            ]);
            const bytes = new TextEncoder().encode("héllo 😀");
            const url = new URL("../b?x=1#top", "https://example.com/a/c");
            url.searchParams.append("y", "z w");
            const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode("abc")));
            const emitter = new EventEmitter();
            let heard = null;
            emitter.once("ping", (value) => (heard = value));
            emitter.emit("ping", 7);
            const clone = structuredClone({ when: new Date(0), set: new Set([1]), nested: { list: [1, 2] } });
            return {
              order, ticks,
              bytes: bytes.length,
              text: new TextDecoder().decode(bytes),
              base64: btoa("hi"), back: atob("aGk="),
              href: url.href, host: url.host, search: url.search,
              params: [...new URLSearchParams("a=1&a=2&b=x+y")],
              uuid: /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID()),
              sha: [...digest].slice(0, 4),
              json: data.answer, shout: shout("ok"),
              path: [path.join("a/b", "../c", "d.txt"), path.extname("x.tar.gz"), path.relative("/a/b", "/a/c/d")],
              heard,
              clone: [clone.when instanceof Date, clone.set.has(1), clone.nested.list.length],
              aborted: (() => { const controller = new AbortController(); controller.abort(); return controller.signal.aborted; })(),
            };
          },
        }];"#,
    );
    let cx = f.context();
    let output = f.call(&cx, "globals", json!({})).await;
    assert!(!output.is_error, "{output:?}");
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["order"], json!(["fast", "slow"]));
    assert_eq!(value["ticks"], 3);
    assert_eq!((value["bytes"].as_u64(), value["text"].as_str()), (Some(11), Some("héllo 😀")));
    assert_eq!((value["base64"].as_str(), value["back"].as_str()), (Some("aGk="), Some("hi")));
    assert_eq!(value["href"], "https://example.com/b?x=1&y=z+w#top");
    assert_eq!((value["host"].as_str(), value["search"].as_str()), (Some("example.com"), Some("?x=1&y=z+w")));
    assert_eq!(value["params"], json!([["a", "1"], ["a", "2"], ["b", "x y"]]));
    assert_eq!(value["uuid"], true);
    // SHA-256("abc") = ba7816bf…
    assert_eq!(value["sha"], json!([0xba, 0x78, 0x16, 0xbf]));
    assert_eq!((value["json"].as_u64(), value["shout"].as_str()), (Some(42), Some("OK")));
    assert_eq!(value["path"], json!(["a/c/d.txt", ".gz", "../c/d"]));
    assert_eq!(value["heard"], 7);
    assert_eq!(value["clone"], json!([true, true, 2]));
    assert_eq!(value["aborted"], true);
}

#[test]
fn import_maps_are_checked() {
    let f = Fixture::new();
    f.plugin("bad-map", "[imports]\nlib = \"../outside.js\"\n", "").plugin("relative-key", "[imports]\n\"./x\" = \"x.js\"\n", "").plugin(
        "unmapped",
        "",
        "import x from \"left-pad\";",
    );
    let found = discover(&f.plugins_dir());
    let error = |id: &str| found.iter().find(|plugin| plugin.id == id).unwrap().manifest.clone().unwrap_err();
    assert!(error("bad-map").contains("相对路径"));
    assert!(error("relative-key").contains("裸模块名"));
    let cx = f.context();
    assert!(failure(&cx, "unmapped").contains("[imports]"));
}

#[tokio::test]
async fn timers_do_not_outlive_their_call_and_dead_promises_fail() {
    let f = Fixture::new();
    f.plugin(
        "timers",
        "timeout_ms = 300\n",
        r#"let fired = 0;
        export const tools = [
          { name: "arm", execute() { setTimeout(() => fired++, 50); return "armed"; } },
          { name: "count", async execute() { await new Promise((resolve) => setTimeout(resolve, 80)); return String(fired); } },
          { name: "forever", execute: () => new Promise(() => {}) },
          { name: "too_long", execute: () => new Promise((resolve) => setTimeout(resolve, 5000)) },
        ];"#,
    );
    let cx = f.context();
    assert_eq!(f.call(&cx, "arm", json!({})).await.content, "armed");
    // `arm` 返回后它的定时器就作废了。
    assert_eq!(f.call(&cx, "count", json!({})).await.content, "0");
    let forever = f.call(&cx, "forever", json!({})).await;
    assert!(forever.is_error && forever.content.contains("never settles"), "{forever:?}");
    let started = Instant::now();
    let too_long = f.call(&cx, "too_long", json!({})).await;
    assert!(too_long.is_error && too_long.content.contains("time limit"), "{too_long:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn concurrent_fetches_secret_headers_and_bytes() {
    let port = serve();
    let f = Fixture::new();
    f.plugin(
        "net-plus",
        "capabilities = [\"net:127.0.0.1\"]\nsecrets = [\"API_TOKEN\", \"UNSET\"]\n",
        &runner(
            r#"const base = input.base;
            const [a, b] = await Promise.all([
              host.fetch(`${base}/echo`, { method: "POST", body: new Uint8Array([104, 105]) }),
              host.fetch(`${base}/echo`, { secretHeaders: { "x-test": "API_TOKEN" } }),
            ]);
            const token = await host.secrets.get("API_TOKEN");
            console.log("token is", token);
            let undeclared = null;
            try { await host.secrets.get("OTHER"); } catch (error) { undeclared = error.message; }
            let unset = null;
            try { await host.fetch(`${base}/echo`, { secretHeaders: { "x-test": "UNSET" } }); } catch (error) { unset = error.message; }
            const controller = new AbortController();
            const slow = host.fetch(`${base}/hang`, { signal: controller.signal });
            setTimeout(() => controller.abort(), 20);
            let aborted = null;
            try { await slow; } catch (error) { aborted = error.name; }
            return {
              a: (await a.json()).body, b: (await b.json()).x,
              bytes: [...(await a.bytes()).slice(0, 2)],
              hasUnset: await host.secrets.has("UNSET"),
              undeclared, unset, aborted,
            };"#,
        ),
    );
    // 测试里没有钥匙串条目：读环境变量。
    // SAFETY: 测试进程里只有这一处设置这个变量。
    unsafe { std::env::set_var(crate::secrets::env_name("net-plus", "API_TOKEN"), "s3cret-value") };
    let cx = f.context();
    let output = f.call(&cx, "run", json!({ "base": format!("http://127.0.0.1:{port}") })).await;
    assert!(!output.is_error, "{output:?}");
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!((value["a"].as_str(), value["b"].as_str()), (Some("hi"), Some("s3cret-value")));
    assert_eq!(value["bytes"].as_array().map(Vec::len), Some(2));
    assert_eq!(value["hasUnset"], false);
    assert!(value["undeclared"].as_str().unwrap().contains("not declared"));
    assert!(value["unset"].as_str().unwrap().contains("has not been set"));
    assert_eq!(value["aborted"], "AbortError");
    // 交给过脚本的密钥不进插件日志。
    let logs = JsPlugins::discover(&f.plugins_dir()).logs("net-plus");
    assert!(logs.iter().any(|line| line.text == "token is ***"), "{logs:?}");
}

#[tokio::test]
async fn workspace_file_functions() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.workspace.path().join("src/deep")).unwrap();
    std::fs::write(f.workspace.path().join("src/a.rs"), "fn main() {}\n// TODO: x\n").unwrap();
    std::fs::write(f.workspace.path().join("src/deep/b.rs"), "// todo later\n").unwrap();
    std::fs::write(f.workspace.path().join("notes.md"), "nothing").unwrap();
    f.plugin(
        "files",
        "capabilities = [\"fs:workspace:read\", \"fs:workspace:write\"]\n",
        &runner(
            r#"await host.writeFileBytes("bin/blob", new Uint8Array([0, 255, 7]));
            const blob = [...(await host.readFileBytes("bin/blob"))];
            await host.mkdir("made/here");
            await host.rename("notes.md", "made/here/notes.md");
            const grep = await host.grep("todo", { ignoreCase: true, glob: "**/*.rs" });
            const result = {
              blob,
              stat: (await host.stat("src/a.rs")).type,
              dir: (await host.stat("src")).type,
              missing: await host.stat("nope"),
              exists: [await host.exists("made/here/notes.md"), await host.exists("notes.md")],
              glob: await host.glob("src/**/*.rs"),
              grep: grep.map(({ path, line }) => `${path}:${line}`),
            };
            await host.remove("made", { recursive: true });
            result.removed = !(await host.exists("made"));
            for (const target of [".", "../x"]) {
              try { await host.remove(target, { recursive: true }); } catch (error) { result[target] = error.message; }
            }
            return result;"#,
        ),
    );
    let cx = f.context();
    let output = f.call(&cx, "run", json!({})).await;
    assert!(!output.is_error, "{output:?}");
    let value: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["blob"], json!([0, 255, 7]));
    assert_eq!((value["stat"].as_str(), value["dir"].as_str()), (Some("file"), Some("dir")));
    assert!(value["missing"].is_null());
    assert_eq!(value["exists"], json!([true, false]));
    assert_eq!(value["glob"], json!(["src/a.rs", "src/deep/b.rs"]));
    assert_eq!(value["grep"], json!(["src/a.rs:2", "src/deep/b.rs:1"]));
    assert_eq!(value["removed"], true);
    assert!(value["."].as_str().unwrap().contains("workspace itself"));
    assert!(value["../x"].as_str().unwrap().contains("outside the workspace"));
}

#[cfg(unix)]
#[tokio::test]
async fn exec_runs_declared_programs_only() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.workspace.path().join("sub")).unwrap();
    let script = |name: &str, read_only: bool, body: &str| {
        format!("{{ name: \"{name}\", readOnly: {read_only}, async execute(input, host) {{ {body} }} }}")
    };
    let tools = [
        script("echo", false, r#"return await host.exec("sh", ["-c", "printf '%s|' \"$@\"; pwd", "x", "a b", "it's"], { cwd: "sub" });"#),
        script("stdin", false, r#"return await host.exec("cat", [], { stdin: "piped in" });"#),
        script("fails", false, r#"return await host.exec("sh", ["-c", "echo oops >&2; exit 3"]);"#),
        script("other", false, r#"return await host.exec("ls", []);"#),
        script("escape", false, r#"return await host.exec("sh", ["-c", "true"], { cwd: "../" });"#),
        script("peek", true, r#"return await host.exec("sh", ["-c", "true"]);"#),
        script("slow", false, r#"return await host.exec("sh", ["-c", "sleep 5"], { timeoutMs: 100 });"#),
    ];
    f.plugin("programs", "capabilities = [\"exec:sh\", \"exec:cat\"]\n", &format!("export const tools = [{}];", tools.join(",\n")));
    let cx = f.context();
    assert_eq!(cx.status("programs"), Some(Status::Active));
    let parsed = |output: ToolOutput| -> Value {
        assert!(!output.is_error, "{output:?}");
        serde_json::from_str(&output.content).unwrap()
    };
    let echo = parsed(f.call(&cx, "echo", json!({})).await);
    let workspace = f.workspace.path().canonicalize().unwrap().join("sub");
    assert_eq!(echo["stdout"].as_str().unwrap().trim_end(), format!("a b|it's|{}", workspace.display()));
    assert_eq!((echo["code"].as_i64(), echo["ok"].as_bool()), (Some(0), Some(true)));
    assert_eq!(parsed(f.call(&cx, "stdin", json!({})).await)["stdout"], "piped in");
    let fails = parsed(f.call(&cx, "fails", json!({})).await);
    assert_eq!((fails["code"].as_i64(), fails["ok"].as_bool(), fails["stderr"].as_str()), (Some(3), Some(false), Some("oops\n")));
    assert!(f.call(&cx, "other", json!({})).await.content.contains("exec:ls"));
    assert!(f.call(&cx, "escape", json!({})).await.content.contains("outside the workspace"));
    assert!(f.call(&cx, "peek", json!({})).await.content.contains("readOnly"));
    let slow = f.call(&cx, "slow", json!({})).await;
    assert!(slow.is_error && slow.content.contains("time limit"), "{slow:?}");

    // 能启动程序的工具按“执行命令”把关；声明了 readOnly 的仍是只读。
    let ctx = f.tool_context();
    assert_eq!(f.tool(&cx, "echo").access(&json!({}), &ctx), Access::Execute);
    assert_eq!(f.tool(&cx, "peek").access(&json!({}), &ctx), Access::Read);
}

/// 跑一轮：模型依次给出 `replies`，返回会话和 provider。
async fn turn(f: &Fixture, replies: Vec<Vec<dshr_llm::StreamEvent>>, config: AgentConfig) -> (Session, Arc<ScriptedProvider>) {
    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    plugins.add_group(AgentPlugins).unwrap();
    plugins.add_group(JsPlugins::discover(&f.plugins_dir())).unwrap();
    let provider = Arc::new(ScriptedProvider::new(replies));
    let agent = Agent::new(provider.clone(), plugins, Arc::new(AutoApprove), f.tool_context(), config);
    let mut session = Session::create("s1", f.home.path().join("log").join("session.v1.jsonl")).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut session, "go".into(), &tx, &CancellationToken::new()).await.unwrap();
    (session, provider)
}

fn results(session: &Session) -> Vec<(String, bool)> {
    session
        .events()
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolResult { output, is_error, .. } => Some((output.to_string(), *is_error)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn hooks_gate_rewrite_and_observe_the_loop() {
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("Cargo.lock"), "lock").unwrap();
    std::fs::write(f.workspace.path().join("a.txt"), "a").unwrap();
    let hooks = r#"
        export const tools = [{ name: "shout", readOnly: true, execute: ({ text }) => text.toUpperCase() }];
        export const hooks = {
          beforeTool: ({ tool, input, access }) => {
            if (access.type === "write" && access.path.endsWith(".lock")) return { decision: "deny", reason: "lock files are generated" };
            if (tool === "read_file" && input.path === "a.txt") return "allow";
          },
          afterTool: ({ tool, output }, host) => {
            host.emitEvent("guard/seen", { tool });
            if (tool === "shout") return { output: `${output}!` };
          },
          beforeStep: ({ step }, host) => host.emitEvent("guard/step", { step }),
          beforeRequest: ({ tools }) => `There are ${tools} tools.`,
          turnEnded: async ({ reason, finalText }, host) => { await host.writeData("last.json", JSON.stringify({ reason, finalText })); },
        };"#;
    f.plugin("guard", "capabilities = [\"hooks:tool-output\"]\n", hooks);
    let replies = vec![
        ScriptedProvider::tool_reply("c1", "write_file", json!({"path": "Cargo.lock", "content": "x"})),
        ScriptedProvider::tool_reply("c2", "shout", json!({"text": "hey"})),
        ScriptedProvider::text_reply("done"),
    ];
    let (session, provider) = turn(&f, replies, AgentConfig::default().with(PermissionMode::Workspace)).await;

    let results = results(&session);
    assert_eq!(results[0], ("lock files are generated".to_owned(), true));
    assert_eq!(std::fs::read_to_string(f.workspace.path().join("Cargo.lock")).unwrap(), "lock");
    // 有 hooks:tool-output：结果被改写。
    assert_eq!(results[1], ("HEY!".to_owned(), false));
    let kinds: Vec<String> = session.events().iter().map(|event| event.body.kind().to_owned()).collect();
    assert_eq!(kinds.iter().filter(|kind| *kind == "guard/step").count(), 3);
    // afterTool 写的事件紧跟在那次调用的结果后面（被拒绝的调用没有执行，不经过 afterTool）。
    let shout = kinds.iter().rposition(|kind| kind == "tool/result").unwrap();
    assert_eq!(kinds[shout + 1], "guard/seen");
    assert!(
        provider.requests()[0].system.as_deref().unwrap().ends_with(&format!("There are {} tools.", provider.requests()[0].tools.len()))
    );
    let last: Value = serde_json::from_str(&std::fs::read_to_string(f.home.path().join("plugin-data/guard/last.json")).unwrap()).unwrap();
    assert_eq!(last, json!({ "reason": "completed", "finalText": "done" }));
}

#[tokio::test]
async fn hooks_cannot_loosen_permissions_without_capabilities() {
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("a.txt"), "a").unwrap();
    f.plugin(
        "loose",
        "",
        r#"export const tools = [{ name: "plain", readOnly: true, execute: () => "plain" }];
        export const hooks = {
          beforeTool: ({ tool }) => (tool === "write_file" ? "ask" : "allow"),
          afterTool: () => "rewritten",
        };"#,
    );
    let replies = vec![
        ScriptedProvider::tool_reply("c1", "write_file", json!({"path": "b.txt", "content": "x"})),
        ScriptedProvider::tool_reply("c2", "plain", json!({})),
        ScriptedProvider::text_reply("done"),
    ];
    // 只读（规划模式）：“询问”不会让写入变成可以批准的，仍然拒绝；没有 hooks:tool-output 时不改写结果。
    let (session, _) = turn(&f, replies, AgentConfig::default().with(dshr_agent::Preset::Plan)).await;
    let results = results(&session);
    assert!(results[0].1 && results[0].0.contains("read-only"), "{results:?}");
    assert!(!f.workspace.path().join("b.txt").exists());
    assert_eq!(results[1], ("plain".to_owned(), false));
}

#[tokio::test]
async fn llm_and_session_access() {
    let f = Fixture::new();
    f.plugin(
        "helper",
        "capabilities = [\"llm\", \"session:read\"]\n",
        r#"export const tools = [
          { name: "ask_model", readOnly: true, async execute({ text }, host) {
              return await host.llm.complete({ system: "Be brief.", prompt: text, maxTokens: 50 });
          } },
          { name: "history", readOnly: true, async execute(_input, host) {
              const events = await host.session.events({ kinds: ["user/message", "tool/result"], last: 10 });
              return events.map((event) => `${event.type}:${event.data.text ?? event.data.output}`).join(" | ");
          } },
        ];
        export const prompt = async (_, host) => { await host.llm.complete("x"); return "PROMPT-FROM-LLM"; };"#,
    );
    f.plugin(
        "nosy",
        "",
        r#"export const tools = [{ name: "nosy", readOnly: true, async execute(_input, host) {
            const errors = [];
            try { await host.llm.complete("x"); } catch (error) { errors.push(error.message); }
            try { await host.session.events(); } catch (error) { errors.push(error.message); }
            return errors.join(" / ");
        } }];"#,
    );
    let replies = vec![
        ScriptedProvider::tool_reply("c1", "ask_model", json!({"text": "summarize"})),
        // 插件发的请求排在这里。
        ScriptedProvider::text_reply("short summary"),
        ScriptedProvider::tool_reply("c2", "history", json!({})),
        ScriptedProvider::tool_reply("c3", "nosy", json!({})),
        ScriptedProvider::text_reply("done"),
    ];
    let (session, provider) = turn(&f, replies, AgentConfig::default().with(dshr_agent::Preset::Plan)).await;
    let results = results(&session);
    assert_eq!(results[0], ("short summary".to_owned(), false));
    assert_eq!(results[1].0, "user/message:go | tool/result:short summary");
    assert!(results[2].0.contains("`llm` capability") && results[2].0.contains("`session:read`"), "{results:?}");
    // 插件的请求：没有工具，带上了插件给的系统提示词，模型和会话一样。
    let request = &provider.requests()[1];
    assert!(request.tools.is_empty());
    assert_eq!((request.system.as_deref(), request.max_tokens), (Some("Be brief."), Some(50)));
    // prompt 里不能调用模型：这一轮没有它的内容。
    assert!(!provider.requests()[0].system.as_deref().unwrap_or_default().contains("PROMPT-FROM-LLM"));
}

#[tokio::test]
async fn lifecycle_and_state_survive_hot_reload() {
    let f = Fixture::new();
    let script = |version: u32| {
        format!(
            r#"let count = 0;
            export const activate = async (host) => {{ await host.writeData("activated-{version}", "yes"); }};
            export const deactivate = async (host) => {{ await host.writeData("deactivated-{version}", String(count)); }};
            export const state = {{ snapshot: () => ({{ count }}), restore: (saved) => {{ count = saved.count; }} }};
            export const tools = [{{ name: "bump", execute: () => `v{version}:${{++count}}` }}];"#
        )
    };
    f.plugin("counter", "", &script(1));
    let scripts = JsPlugins::discover(&f.plugins_dir());
    let cx = Context::new();
    cx.add_group(dshr_tools::BuiltinTools).unwrap();
    cx.add_group(scripts.clone()).unwrap();
    assert_eq!(f.call(&cx, "bump", json!({})).await.content, "v1:1");
    assert_eq!(f.call(&cx, "bump", json!({})).await.content, "v1:2");
    let data = f.home.path().join("plugin-data/counter");
    assert!(data.join("activated-1").exists());

    std::thread::sleep(Duration::from_millis(20));
    f.write_plugin("counter", "", &script(2));
    let report = scripts.reload();
    assert_eq!(report.reloaded, ["counter"]);
    // 新版本接着旧版本的计数；旧版本的 deactivate 看到的是换下时的状态。
    assert_eq!(f.call(&cx, "bump", json!({})).await.content, "v2:3");
    assert!(data.join("activated-2").exists());
    assert_eq!(std::fs::read_to_string(data.join("deactivated-1")).unwrap(), "2");

    // activate 失败就是激活失败。
    f.plugin("broken-start", "", "export const activate = () => { throw new Error(\"no config\"); };");
    let cx = f.context();
    assert!(failure(&cx, "broken-start").contains("no config"));
}

#[test]
fn schedules_need_the_background_capability() {
    let f = Fixture::new();
    let script = r#"export const schedules = [{ id: "sync", every: "10m", run: async () => {} }];"#;
    f.plugin("quiet", "", script)
        .plugin("busy", "capabilities = [\"background\"]\n", script)
        .plugin("hasty", "capabilities = [\"background\"]\n", r#"export const schedules = [{ id: "x", every: 100, run() {} }];"#)
        .plugin("vague", "capabilities = [\"background\"]\n", r#"export const schedules = [{ id: "x", every: "often", run() {} }];"#);
    let cx = f.context();
    assert!(failure(&cx, "quiet").contains("background"));
    assert_eq!(cx.status("busy"), Some(Status::Active));
    assert!(failure(&cx, "hasty").contains("too often"));
    assert!(failure(&cx, "vague").contains("every"));
}

#[test]
fn cards_event_renders_panels_and_refresh() {
    let f = Fixture::new();
    f.plugin(
        "viz",
        "",
        r#"export const tools = [{ name: "count_lines", readOnly: true, execute: ({ text }, host) => { host.ui.refresh("main"); return String(text.split("\n").length); } }];
        export const cards = {
          count_lines: {
            title: (input) => `${input.text.length} chars`,
            render: (input, output) => output === null
              ? "counting…"
              : { type: "table", columns: ["text", "lines"], rows: [[input.text, Number(output)]] },
          },
        };
        export const events = { "viz/chart": { render: ({ value }) => ({ type: "progress", value, label: "done" }) } };
        export const ui = {
          pages: { main: () => ({ type: "markdown", text: "**hi**" }) },
          panels: [{ id: "summary", label: "Summary", page: "main" }],
          status: [{ id: "state", page: "main" }],
        };"#,
    );
    f.plugin("bad-card", "", r#"export const tools = []; export const cards = { missing: () => "x" };"#);
    let cx = f.context();
    assert!(failure(&cx, "bad-card").contains("no tool named missing"));
    let snapshot = cx.snapshot();
    let surface = snapshot.items::<UiSurfaces>().iter().find(|surface| surface.plugin() == "viz").unwrap().clone();
    assert_eq!(surface.card_tools(), ["count_lines"]);
    let input = json!({ "text": "a\nb" });
    let pending = surface.card("count_lines", &input, None, false).unwrap();
    assert_eq!(pending.title.as_deref(), Some("3 chars"));
    assert_eq!(pending.body, Some(json!({ "type": "text", "text": "counting…" })));
    let done = surface.card("count_lines", &input, Some("2"), false).unwrap();
    assert_eq!(done.body.unwrap()["rows"], json!([["a\nb", 2]]));

    assert_eq!(surface.event_kinds(), ["viz/chart"]);
    assert_eq!(surface.render_event("viz/chart", &json!({ "value": 0.5 })).unwrap()["type"], "progress");
    assert!(surface.render_event("viz/chart", &json!({ "value": 3 })).unwrap_err().contains("0 to 1"));

    let places: Vec<_> = surface.entries().iter().map(|entry| (entry.place, entry.id.as_str(), entry.label.as_str())).collect();
    assert_eq!(places, [(dshr_agent::EntryPlace::Panel, "summary", "Summary"), (dshr_agent::EntryPlace::Status, "state", "state")]);
    assert_eq!(surface.render("main", None).unwrap()["type"], "markdown");

    let tool = ToolRegistry::from_snapshot(&snapshot).get("count_lines").unwrap();
    let output = futures_lite_block(tool.execute(json!({ "text": "x" }), &f.tool_context()));
    assert_eq!(output.content, "1");
    assert_eq!((surface.refresh_generation("main"), surface.refresh_generation("other")), (1, 0));
}

fn futures_lite_block<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
}

#[test]
fn new_capabilities_round_trip() {
    let names: Vec<String> =
        ["fs:workspace:read", "net:x.com", "exec:git", "exec:*", "llm", "session:read", "hooks:gate", "hooks:tool-output", "background"]
            .iter()
            .map(|name| name.to_string())
            .collect();
    let capabilities = crate::Capabilities::parse(&names).unwrap();
    assert_eq!(capabilities.names(), names);
    assert!(capabilities.allows_program("anything"));
    let only_git = crate::Capabilities::parse(&["exec:git".to_owned()]).unwrap();
    assert!(only_git.allows_program("git") && !only_git.allows_program("/usr/bin/git") && !only_git.allows_program("sh"));
    for bad in ["exec:", "exec:/bin/sh", "exec:git status", "exec:../x"] {
        assert!(crate::Capabilities::parse(&[bad.to_owned()]).is_err(), "{bad}");
    }
    let f = Fixture::new();
    f.plugin("bad-secret", "secrets = [\"lower\"]\n", "");
    assert!(discover(&f.plugins_dir())[0].manifest.as_ref().unwrap_err().contains("lower"));
}

#[test]
fn images_inside_every_container_and_avatars_are_inlined() {
    let f = Fixture::new();
    f.plugin(
        "pics",
        "",
        r#"export const ui = { pages: { main: () => ({ type: "column", children: [
            { type: "tabs", tabs: [{ id: "a", label: "A", children: [{ type: "image", src: "data/dot.png", alt: "in a tab" }] }] },
            { type: "split", left: [{ type: "avatar", src: "data/dot.png", text: "Ada" }], right: [{ type: "avatar", src: "data/missing.png", text: "Bo" }] },
            { type: "card", actions: [{ type: "image", src: "data/missing.png" }], children: [] },
        ]}) }, sidebar: [{ id: "pics", label: "Pics", page: "main" }] };"#,
    );
    let data = f.home.path().join("plugin-data").join("pics");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("dot.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    let cx = f.context();
    let snapshot = cx.snapshot();
    let surface = snapshot.items::<UiSurfaces>().iter().find(|surface| surface.plugin() == "pics").unwrap().clone();
    let tree = surface.render("main", None).unwrap();
    let children = &tree["children"];
    assert!(children[0]["tabs"][0]["children"][0]["src"].as_str().unwrap().starts_with("data:image/png;base64,"));
    assert!(children[1]["left"][0]["src"].as_str().unwrap().starts_with("data:image/png;base64,"));
    // 读不了的头像去掉图片，只显示首字；读不了的图片换成说明文字。
    assert_eq!(children[1]["right"][0], json!({ "type": "avatar", "text": "Bo" }));
    assert_eq!(children[2]["actions"][0]["type"], "text");
}
