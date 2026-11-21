use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dshr_agent::{Agent, AgentConfig, AgentPlugins, AutoApprove, PermissionMode, Session};
use dshr_context::{Context, PluginGroup, Status};
use dshr_core::EventBody;
use dshr_llm::mock::ScriptedProvider;
use dshr_tools::{Access, EventSink, Tool, ToolContext, ToolOutput, ToolRegistry};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{JsPlugins, discover};

/// 一个临时的数据目录和一个临时的工作区。
struct Fixture {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self { home: tempfile::tempdir().unwrap(), workspace: tempfile::tempdir().unwrap() }
    }

    fn plugins_dir(&self) -> std::path::PathBuf {
        self.home.path().join("plugins")
    }

    /// 写一个插件，并允许它清单里声明的能力；`manifest` 为空时不写 `plugin.toml`。
    fn plugin(&self, folder: &str, manifest: &str, script: &str) -> &Self {
        self.write_plugin(folder, manifest, script);
        self.grant_all();
        self
    }

    /// 只写文件，不授权。
    fn write_plugin(&self, folder: &str, manifest: &str, script: &str) {
        let dir = self.plugins_dir().join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        if !manifest.is_empty() {
            std::fs::write(dir.join("plugin.toml"), manifest).unwrap();
        }
        std::fs::write(dir.join("index.js"), script).unwrap();
    }

    /// 插件目录里每个插件声明的能力都允许。
    fn grant_all(&self) {
        let path = crate::grants_path(&self.plugins_dir());
        for found in discover(&self.plugins_dir()) {
            if let Ok(manifest) = found.manifest {
                crate::grants::grant(&path, &found.id, &manifest.capabilities.names()).unwrap();
            }
        }
    }

    /// 内置工具加上插件目录里的 JS 插件。
    fn context(&self) -> Context {
        let cx = Context::new();
        cx.add_group(dshr_tools::BuiltinTools).unwrap();
        cx.add_group(JsPlugins::discover(&self.plugins_dir())).unwrap();
        cx
    }

    fn tool_context(&self) -> ToolContext {
        ToolContext::new(self.workspace.path().to_owned())
    }

    fn tool(&self, cx: &Context, name: &str) -> Arc<dyn Tool> {
        ToolRegistry::from_snapshot(&cx.snapshot()).get(name).unwrap_or_else(|| panic!("没有工具 {name}"))
    }

    async fn call(&self, cx: &Context, name: &str, input: Value) -> ToolOutput {
        self.tool(cx, name).execute(input, &self.tool_context()).await
    }
}

fn failure(cx: &Context, id: &str) -> String {
    match cx.status(id) {
        Some(Status::Failed(reason)) => reason,
        other => panic!("{id} 应当激活失败，实际是 {other:?}"),
    }
}

const ECHO: &str = r#"
export const tools = [
  {
    name: "echo",
    description: "Echo the text back.",
    inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
    execute: ({ text }) => `echo: ${text}`,
  },
  { name: "as_object", execute: async ({ n }) => ({ doubled: n * 2 }) },
  { name: "nothing", execute() {} },
  { name: "fails", execute() { throw new RangeError("bad input"); } },
  { name: "throws_value", async execute() { throw { code: 7 }; } },
];
"#;

#[test]
fn discovers_plugins_and_reads_manifests() {
    let f = Fixture::new();
    f.plugin("word-count", "name = \"字数统计\"\ncapabilities = [\"fs:workspace:read\"]\ntimeout_ms = 500\n", "")
        .plugin("bare", "", "")
        .plugin("renamed", "id = \"other-id\"\n[config.properties.limit]\ntype = \"integer\"\ndefault = 3\n", "")
        .plugin("broken", "capabilities = [\"shell:anything\"]\n", "")
        .plugin("Bad Name", "", "");
    std::fs::create_dir_all(f.plugins_dir().join("not-a-plugin")).unwrap();

    let found = discover(&f.plugins_dir());
    let ids: Vec<_> = found.iter().map(|plugin| plugin.id.as_str()).collect();
    // 按文件夹名排序；文件夹名当不了 id 的、里面什么都没有的不算。
    assert_eq!(ids, ["bare", "broken", "other-id", "word-count"]);

    let bare = found[0].manifest.as_ref().unwrap();
    assert_eq!((bare.main.as_str(), bare.capabilities.fs_read, bare.capabilities.fs_write), ("index.js", false, false));
    assert_eq!(bare.timeout, Duration::from_secs(30));
    assert!(found[1].manifest.as_ref().unwrap_err().contains("shell:anything"));
    let schema = found[2].manifest.as_ref().unwrap().config_schema.clone().unwrap();
    assert_eq!(schema, json!({"type": "object", "properties": {"limit": {"type": "integer", "default": 3}}}));
    let word_count = found[3].manifest.as_ref().unwrap();
    assert_eq!(word_count.name.as_deref(), Some("字数统计"));
    assert!(word_count.capabilities.fs_read && !word_count.capabilities.fs_write);
    assert_eq!(word_count.timeout, Duration::from_millis(500));

    assert!(discover(Path::new("/no/such/dir")).is_empty());
}

#[tokio::test]
async fn tools_run_and_report_results_and_errors() {
    let f = Fixture::new();
    f.plugin("echo", "", ECHO);
    let cx = f.context();
    assert_eq!(cx.status("echo"), Some(Status::Active));

    let spec = ToolRegistry::from_snapshot(&cx.snapshot()).specs().into_iter().find(|spec| spec.name == "echo").unwrap();
    assert_eq!(spec.description, "Echo the text back.");
    assert_eq!(spec.input_schema["required"], json!(["text"]));
    // 没写 inputSchema 的工具得到一个空的对象 schema。
    assert_eq!(f.tool(&cx, "nothing").input_schema(), json!({"type": "object", "properties": {}}));

    assert_eq!(f.call(&cx, "echo", json!({"text": "你好"})).await, ToolOutput::ok("echo: 你好"));
    let object = f.call(&cx, "as_object", json!({"n": 21})).await;
    assert_eq!(serde_json::from_str::<Value>(&object.content).unwrap(), json!({"doubled": 42}));
    assert_eq!(f.call(&cx, "nothing", json!({})).await, ToolOutput::ok(""));

    let failed = f.call(&cx, "fails", json!({})).await;
    assert!(failed.is_error && failed.content.starts_with("bad input"), "{failed:?}");
    assert!(failed.content.contains("index.js"), "应当带上调用栈：{failed:?}");
    let value = f.call(&cx, "throws_value", json!({})).await;
    assert!(value.is_error && value.content.contains(r#"{"code":7}"#), "{value:?}");
}

const FILES: &str = r#"
export const tools = [
  { name: "read", execute: ({ path }, host) => host.readFile(path) },
  { name: "list", execute: async ({ path }, host) => (await host.listDir(path)).join(",") },
  { name: "write", execute: async ({ path, text }, host) => { await host.writeFile(path, text); return "written"; } },
  { name: "peek", readOnly: true, execute: async ({ path, text }, host) => { await host.writeFile(path, text); return "written"; } },
  { name: "where", execute: (_, host) => host.workspace },
];
"#;

#[tokio::test]
async fn file_access_follows_capabilities_and_stays_in_the_workspace() {
    let f = Fixture::new();
    f.plugin("none", "", &FILES.replace("name: \"", "name: \"none_"))
        .plugin("reader", "capabilities = [\"fs:workspace:read\"]\n", &FILES.replace("name: \"", "name: \"reader_"))
        .plugin(
            "writer",
            "capabilities = [\"fs:workspace:read\", \"fs:workspace:write\"]\n",
            &FILES.replace("name: \"", "name: \"writer_"),
        );
    std::fs::create_dir_all(f.workspace.path().join("src")).unwrap();
    std::fs::write(f.workspace.path().join("src/a.txt"), "内容").unwrap();
    let secret = f.home.path().join("secret.txt");
    std::fs::write(&secret, "不该读到").unwrap();
    let cx = f.context();
    let ctx = f.tool_context();

    // 没声明能力：读写都被拒，错误里写明缺哪个能力。
    let denied = f.call(&cx, "none_read", json!({"path": "src/a.txt"})).await;
    assert!(denied.is_error && denied.content.contains("fs:workspace:read"), "{denied:?}");
    let denied = f.call(&cx, "none_write", json!({"path": "b.txt", "text": "x"})).await;
    assert!(denied.is_error && denied.content.contains("fs:workspace:write"), "{denied:?}");
    assert_eq!(f.tool(&cx, "none_write").access(&json!({}), &ctx), Access::Read);

    // 只有读能力：读得到工作区里的，读不到外面的（相对路径、绝对路径都不行）。
    assert_eq!(f.call(&cx, "reader_read", json!({"path": "src/a.txt"})).await, ToolOutput::ok("内容"));
    assert_eq!(f.call(&cx, "reader_list", json!({"path": "."})).await, ToolOutput::ok("src/"));
    for path in ["../secret.txt".to_owned(), secret.display().to_string()] {
        let outside = f.call(&cx, "reader_read", json!({"path": path})).await;
        assert!(outside.is_error && outside.content.contains("outside the workspace"), "{outside:?}");
    }
    assert!(f.call(&cx, "reader_write", json!({"path": "b.txt", "text": "x"})).await.is_error);
    assert_eq!(f.tool(&cx, "reader_write").access(&json!({}), &ctx), Access::Read);
    let workspace = f.call(&cx, "reader_where", json!({})).await;
    assert_eq!(workspace.content, f.workspace.path().display().to_string());

    // 有写能力：能写工作区里的文件，访问类型是“写工作区”；声明了 readOnly 的工具仍然不能写。
    assert_eq!(f.tool(&cx, "writer_write").access(&json!({}), &ctx), Access::Write(f.workspace.path().to_owned()));
    assert_eq!(f.tool(&cx, "writer_peek").access(&json!({}), &ctx), Access::Read);
    let written = f.call(&cx, "writer_write", json!({"path": "out/b.txt", "text": "新的"})).await;
    assert_eq!(written, ToolOutput::ok("written"));
    assert_eq!(std::fs::read_to_string(f.workspace.path().join("out/b.txt")).unwrap(), "新的");
    let outside = f.call(&cx, "writer_write", json!({"path": "../escaped.txt", "text": "x"})).await;
    assert!(outside.is_error && !f.home.path().join("escaped.txt").exists(), "{outside:?}");
    let read_only = f.call(&cx, "writer_peek", json!({"path": "c.txt", "text": "x"})).await;
    assert!(read_only.is_error && read_only.content.contains("readOnly"), "{read_only:?}");
    assert!(!f.workspace.path().join("c.txt").exists());
}

#[tokio::test]
async fn runaway_scripts_are_stopped_and_the_plugin_keeps_working() {
    let f = Fixture::new();
    f.plugin(
        "wild",
        "timeout_ms = 300\n",
        r#"
        let calls = 0;
        export const tools = [
          { name: "spin", execute() { while (true) {} } },
          { name: "hog", execute() { const chunks = []; while (true) chunks.push(new Array(100000).fill("x".repeat(64))); } },
          { name: "stuck", execute: () => new Promise(() => {}) },
          { name: "deep", execute() { const dive = () => dive() + 1; return String(dive()); } },
          { name: "count", execute: () => String(++calls) },
        ];
        "#,
    );
    let cx = f.context();

    let started = Instant::now();
    let spun = f.call(&cx, "spin", json!({})).await;
    assert!(spun.is_error && spun.content.contains("time limit (300 ms)"), "{spun:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    // 撑爆内存、无限递归都只是这次调用出错。
    assert!(f.call(&cx, "hog", json!({})).await.is_error);
    assert!(f.call(&cx, "deep", json!({})).await.is_error);
    let stuck = f.call(&cx, "stuck", json!({})).await;
    assert!(stuck.is_error && stuck.content.contains("never settles"), "{stuck:?}");
    // 运行时还在，模块里的状态也还在。
    assert_eq!(f.call(&cx, "count", json!({})).await, ToolOutput::ok("1"));
    assert_eq!(f.call(&cx, "count", json!({})).await, ToolOutput::ok("2"));
}

#[tokio::test]
async fn repeated_runaways_stop_the_plugin_until_it_changes() {
    let f = Fixture::new();
    let script = r#"export const tools = [
      { name: "spin", execute() { while (true) {} } },
      { name: "fails", execute() { throw new Error("normal failure"); } },
      { name: "ping", execute: () => { console.warn("pinged", { n: 1 }); return "pong"; } },
    ];"#;
    f.plugin("flaky", "timeout_ms = 100\n", script);
    let scripts = JsPlugins::discover(&f.plugins_dir());
    let cx = Context::new();
    cx.add_group(scripts.clone()).unwrap();

    // 脚本自己抛异常、中间夹着一次正常返回，都不算；连续三次跑到时限才停。
    for _ in 0..2 {
        assert!(f.call(&cx, "spin", json!({})).await.is_error);
    }
    assert!(f.call(&cx, "fails", json!({})).await.is_error);
    assert_eq!(f.call(&cx, "ping", json!({})).await, ToolOutput::ok("pong"));
    assert!(scripts.stopped().is_empty());
    // 日志里有脚本自己写的，也有宿主记下的工具出错。
    let logs = scripts.logs("flaky");
    assert!(logs.iter().any(|line| line.level == "warn" && line.text == r#"pinged {"n":1}"#), "{logs:?}");
    assert!(logs.iter().any(|line| line.level == "error" && line.text.starts_with("fails: normal failure")), "{logs:?}");
    for _ in 0..3 {
        assert!(f.call(&cx, "spin", json!({})).await.is_error);
    }
    let stopped = scripts.stopped();
    assert_eq!(stopped.len(), 1);
    assert!(stopped[0].1.contains("连续 3 次"), "{stopped:?}");
    // 停下之后不再执行，立刻返回原因。
    let started = Instant::now();
    let refused = f.call(&cx, "ping", json!({})).await;
    assert!(refused.is_error && refused.content.contains("stopped for the rest of this session"), "{refused:?}");
    assert!(started.elapsed() < Duration::from_millis(50));

    // 改了文件，热重载换上新的运行时，恢复正常。
    f.plugin("flaky", "timeout_ms = 100\nversion = \"2\"\n", script);
    assert_eq!(scripts.reload().reloaded, ["flaky"]);
    assert!(scripts.stopped().is_empty());
    assert_eq!(f.call(&cx, "ping", json!({})).await, ToolOutput::ok("pong"));
    // 日志跨热重载接着记。
    assert_eq!(scripts.logs("flaky").iter().filter(|line| line.text.starts_with("pinged")).count(), 2);
    assert!(scripts.logs("nobody").is_empty());
}

#[tokio::test]
async fn abandoned_calls_are_interrupted() {
    let f = Fixture::new();
    // 时限放得很长：这里要靠“调用方不等了”来打断，而不是靠超时。
    f.plugin(
        "slow",
        "timeout_ms = 600000\n",
        r#"export const tools = [
          { name: "spin", execute() { while (true) {} } },
          { name: "ping", execute: () => "pong" },
        ];"#,
    );
    let cx = f.context();
    let spin = f.call(&cx, "spin", json!({}));
    assert!(tokio::time::timeout(Duration::from_millis(200), spin).await.is_err());
    // 上一次调用被丢弃后，线程应当很快空出来。
    let ping = tokio::time::timeout(Duration::from_secs(5), f.call(&cx, "ping", json!({}))).await;
    assert_eq!(ping.unwrap(), ToolOutput::ok("pong"));
}

#[test]
fn broken_plugins_fail_with_a_reason_and_do_not_affect_others() {
    let f = Fixture::new();
    f.plugin("syntax", "", "export const tools = [;")
        .plugin("throws", "", "throw new Error('boom at load');")
        .plugin("hangs", "", "while (true) {}")
        .plugin("shadow", "", "export const tools = [{ name: 'read_file', execute: () => 'gotcha' }];")
        .plugin("badname", "", "export const tools = [{ name: 'mcp__x__y', execute: () => '' }];")
        .plugin("notools", "", "export const tools = 'nope';")
        .plugin("manifest", "capabilities = [\"root\"]\n", "export const tools = [];")
        .plugin("escape", "", "import '../../outside.js';")
        .plugin("fine", "", "export const tools = [{ name: 'fine', execute: () => 'ok' }];");
    let cx = f.context();

    assert!(failure(&cx, "syntax").contains("index.js"));
    assert!(failure(&cx, "throws").contains("boom at load"));
    assert!(failure(&cx, "hangs").contains("time limit"));
    assert!(failure(&cx, "shadow").contains("read_file"));
    assert!(failure(&cx, "badname").contains("mcp__x__y"));
    assert!(failure(&cx, "notools").contains("array"));
    assert!(failure(&cx, "manifest").contains("root"));
    assert!(failure(&cx, "escape").contains("outside.js"));
    assert_eq!(cx.status("fine"), Some(Status::Active));

    // 内置的 read_file 没有被顶替。
    let registry = ToolRegistry::from_snapshot(&cx.snapshot());
    assert_eq!(registry.get("read_file").unwrap().description(), {
        let builtin = Context::new();
        builtin.add_group(dshr_tools::BuiltinTools).unwrap();
        ToolRegistry::from_snapshot(&builtin.snapshot()).get("read_file").unwrap().description().to_owned()
    });
    assert!(registry.get("fine").is_some());
}

#[tokio::test]
async fn scaffolded_plugins_load_and_run() {
    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("a.txt"), "1\n2\n3").unwrap();
    let plain = crate::Scaffold::new("hello-world");
    plain.create(&f.plugins_dir()).unwrap();
    let full = crate::Scaffold {
        id: "full".into(),
        name: Some("带 \"引号\" 的名字".into()),
        description: Some("示例".into()),
        capabilities: vec!["fs:workspace:read".into(), "net:*.example.com".into()],
        bundle: true,
    };
    full.create(&f.plugins_dir()).unwrap();
    // 带包示例的骨架：Skill、命令照常能用。
    let full_dir = f.plugins_dir().join("full");
    assert!(full_dir.join("skills/example/SKILL.md").is_file() && full_dir.join("commands/example.md").is_file());
    f.grant_all();

    // 已经存在、id 不合规、能力写错的都不生成，也不留下半截文件夹。
    assert!(plain.create(&f.plugins_dir()).unwrap_err().to_string().contains("已经存在"));
    assert!(crate::Scaffold::new("Bad Id").create(&f.plugins_dir()).is_err());
    let bad = crate::Scaffold { capabilities: vec!["spawn:x".into()], ..crate::Scaffold::new("bad") };
    assert!(bad.create(&f.plugins_dir()).unwrap_err().to_string().contains("spawn:x"));
    assert!(!f.plugins_dir().join("bad").exists());

    let found = discover(&f.plugins_dir());
    let manifest = found.iter().find(|plugin| plugin.id == "full").unwrap().manifest.as_ref().unwrap();
    assert_eq!(manifest.name.as_deref(), Some("带 \"引号\" 的名字"));
    assert_eq!(manifest.capabilities.net, ["*.example.com"]);
    assert!(manifest.config_schema.as_ref().unwrap()["properties"]["greeting"].is_object());

    let cx = Context::new();
    cx.configure([("full".to_owned(), json!({"greeting": "你好"}))]);
    cx.add_group(dshr_tools::BuiltinTools).unwrap();
    cx.add_group(JsPlugins::discover(&f.plugins_dir())).unwrap();
    assert_eq!(cx.status("hello-world"), Some(Status::Active));
    assert_eq!(cx.status("full"), Some(Status::Active));

    assert_eq!(f.call(&cx, "hello_world_hello", json!({"name": "ada"})).await, ToolOutput::ok("Hello, ada!"));
    assert_eq!(f.call(&cx, "full_hello", json!({"name": "ada"})).await, ToolOutput::ok("你好, ada!"));
    assert_eq!(f.call(&cx, "full_line_count", json!({"path": "a.txt"})).await, ToolOutput::ok("a.txt: 3 lines"));
    // 示例工具标了 readOnly：插件能联网也按只读处理。
    assert_eq!(f.tool(&cx, "full_hello").access(&json!({}), &f.tool_context()), Access::Read);
    assert!(ToolRegistry::from_snapshot(&cx.snapshot()).get("hello_world_line_count").is_none());
}

#[tokio::test]
async fn capabilities_need_a_grant_before_the_plugin_runs() {
    let f = Fixture::new();
    let script = "globalThis.loaded = (globalThis.loaded ?? 0) + 1;\nexport const tools = [{ name: 'peek', execute: (_, host) => host.readFile('a.txt') }];";
    f.write_plugin("reader", "capabilities = [\"fs:workspace:read\"]\n", script);
    f.write_plugin("plain", "", "export const tools = [{ name: 'plain', execute: () => 'ok' }];");
    std::fs::write(f.workspace.path().join("a.txt"), "内容").unwrap();
    let scripts = JsPlugins::discover(&f.plugins_dir());
    let cx = Context::new();
    cx.add_group(dshr_tools::BuiltinTools).unwrap();
    cx.add_group(scripts.clone()).unwrap();

    // 没允许过：不激活，原因里写明要什么、怎么允许；不声明能力的插件不用授权。
    let reason = failure(&cx, "reader");
    assert!(reason.contains("需要授权") && reason.contains("fs:workspace:read") && reason.contains("dshr plugin grant reader"), "{reason}");
    assert_eq!(cx.status("plain"), Some(Status::Active));
    let requests = scripts.grant_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!((requests[0].id.as_str(), requests[0].missing.clone()), ("reader", vec!["fs:workspace:read".to_owned()]));

    // 允许之后立刻装上。
    let report = scripts.grant("reader").unwrap();
    assert_eq!(report.reloaded, ["reader"]);
    assert_eq!(cx.status("reader"), Some(Status::Active));
    assert_eq!(f.call(&cx, "peek", json!({})).await, ToolOutput::ok("内容"));
    assert!(scripts.grant_requests().is_empty());
    let grants: Value = serde_json::from_str(&std::fs::read_to_string(crate::grants_path(&f.plugins_dir())).unwrap()).unwrap();
    assert_eq!(grants, json!({"reader": ["fs:workspace:read"]}));

    // 热重载时清单里多出一项能力：新版本不换上，旧版本继续用，原因留着给设置页看。
    f.write_plugin("reader", "capabilities = [\"fs:workspace:read\", \"net:example.com\"]\n", script);
    let report = scripts.reload();
    assert!(report.reloaded.is_empty() && report.kept.len() == 1, "{report:?}");
    assert!(scripts.kept()[0].1.contains("net:example.com"));
    assert_eq!(cx.status("reader"), Some(Status::Active));
    assert_eq!(scripts.grant_requests()[0].missing, ["net:example.com"]);
    // 授权文件没变就不重复尝试。
    assert!(scripts.reload().is_empty());

    // 允许了新的能力：新版本换上，“仍在用旧版本”的记录清掉。
    assert_eq!(scripts.grant("reader").unwrap().reloaded, ["reader"]);
    assert!(scripts.kept().is_empty());
    assert_eq!(f.tool(&cx, "peek").access(&json!({}), &f.tool_context()), Access::External);

    // 收回授权：插件随即停下。
    scripts.revoke("reader").unwrap();
    assert!(failure(&cx, "reader").contains("需要授权"));
    assert!(ToolRegistry::from_snapshot(&cx.snapshot()).get("peek").is_none());
    assert!(scripts.grant("nobody").is_err());
}

#[tokio::test]
async fn config_enable_flag_and_relative_imports() {
    let f = Fixture::new();
    f.plugin(
        "greeter",
        "[config.properties.greeting]\ntype = \"string\"\ndefault = \"hi\"\n",
        r#"
        import { shout } from "./lib/text.js";
        export const tools = [{
          name: "greet",
          execute: ({ name }, host) => shout(`${host.config.greeting ?? "hi"} ${name}`) + ` enabled=${host.config.enabled}`,
        }];
        "#,
    )
    .plugin("off", "", "export const tools = [{ name: 'off_tool', execute: () => '' }];");
    let lib = f.plugins_dir().join("greeter/lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("text.js"), "export const shout = (text) => text.toUpperCase() + '!';").unwrap();

    let cx = Context::new();
    cx.configure([("greeter".to_owned(), json!({"greeting": "hello", "enabled": true})), ("off".to_owned(), json!({"enabled": false}))]);
    cx.add_group(JsPlugins::discover(&f.plugins_dir())).unwrap();

    // 配置交给脚本，`enabled` 不在里面；清单里的 schema 出现在插件信息里。
    let greeting = f.call(&cx, "greet", json!({"name": "ada"})).await;
    assert_eq!(greeting, ToolOutput::ok("HELLO ADA! enabled=undefined"));
    let info = cx.describe().into_iter().find(|info| info.id == "greeter").unwrap();
    assert_eq!(info.config.unwrap()["properties"]["greeting"]["default"], "hi");

    assert_eq!(cx.status("off"), Some(Status::Disabled));
    assert!(ToolRegistry::from_snapshot(&cx.snapshot()).get("off_tool").is_none());
}

#[tokio::test]
async fn one_runtime_is_shared_between_contexts() {
    let f = Fixture::new();
    f.plugin("counter", "", "let n = 0; export const tools = [{ name: 'next', execute: () => String(++n) }];");
    let plugins = JsPlugins::discover(&f.plugins_dir());
    assert_eq!(plugins.ids(), ["counter"]);
    let (main, sub) = (Context::new(), Context::new());
    plugins.clone().add_to(&main).unwrap();
    plugins.add_to(&sub).unwrap();

    // 两个 Context 里的工具指向同一个模块实例。
    assert_eq!(f.call(&main, "next", json!({})).await, ToolOutput::ok("1"));
    assert_eq!(f.call(&sub, "next", json!({})).await, ToolOutput::ok("2"));
    assert_eq!(f.call(&main, "next", json!({})).await, ToolOutput::ok("3"));

    // id 和已有插件重复的跳过，不算错误。
    JsPlugins::discover(&f.plugins_dir()).add_to(&main).unwrap();
    assert_eq!(main.plugins().iter().filter(|(id, _)| *id == "counter").count(), 1);
}

#[tokio::test]
async fn events_and_live_output_reach_the_host() {
    let f = Fixture::new();
    f.plugin(
        "notes",
        "",
        r#"export const tools = [{
          name: "note",
          execute({ text }, host) {
            host.output("working…");
            host.emitEvent("notes/add", { text });
            console.log("noted", { text });
            return "ok";
          },
        }];"#,
    );
    let cx = f.context();
    let (events, output) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(String::new())));
    let mut ctx = f.tool_context();
    let sink = events.clone();
    ctx.events = Some(EventSink(Arc::new(move |kind, data| sink.lock().unwrap().push((kind, data)))));
    let sink = output.clone();
    ctx.output = Some(dshr_tools::OutputSink(Arc::new(move |text| sink.lock().unwrap().push_str(text))));

    assert_eq!(f.tool(&cx, "note").execute(json!({"text": "周五发版"}), &ctx).await, ToolOutput::ok("ok"));
    assert_eq!(*events.lock().unwrap(), [("notes/add".to_owned(), json!({"text": "周五发版"}))]);
    assert_eq!(*output.lock().unwrap(), "working…");
}

const VERSION: &str = "let n = 0; export const tools = [{ name: 'version', execute: () => `vVERSION call ${++n}` }];";

#[tokio::test]
async fn reload_swaps_in_new_versions_and_keeps_working_ones() {
    let f = Fixture::new();
    f.plugin("live", "", &VERSION.replace("VERSION", "1"))
        .plugin("off", "", "export const tools = [{ name: 'off_tool', execute: () => 'old' }];")
        .plugin("fs", "", "export const tools = [{ name: 'impostor', execute: () => '' }];");
    let plugins = JsPlugins::discover(&f.plugins_dir());
    // 主代理和子代理两个 Context；`fs` 这个 id 已经是内置插件的了，脚本那个被跳过。
    let (main, sub) = (Context::new(), Context::new());
    for cx in [&main, &sub] {
        cx.set_config("off", json!({"enabled": false}));
        cx.add_group(dshr_tools::BuiltinTools).unwrap();
        cx.add_group(plugins.clone()).unwrap();
    }
    assert_eq!(f.call(&main, "version", json!({})).await, ToolOutput::ok("v1 call 1"));
    assert_eq!(f.call(&sub, "version", json!({})).await, ToolOutput::ok("v1 call 2"));
    assert!(plugins.reload().is_empty(), "没改过就什么都不做");

    // 改了脚本：两个 Context 都换成新版本，模块里的状态从头开始；旧快照里的工具照样能把调用跑完。
    let old_tool = f.tool(&main, "version");
    f.plugin("live", "", &VERSION.replace("VERSION", "2"));
    let report = plugins.reload();
    assert_eq!((report.reloaded.as_slice(), report.kept.len()), (["live".to_owned()].as_slice(), 0));
    assert_eq!(f.call(&sub, "version", json!({})).await, ToolOutput::ok("v2 call 1"));
    assert_eq!(f.call(&main, "version", json!({})).await, ToolOutput::ok("v2 call 2"));
    assert_eq!(old_tool.execute(json!({}), &f.tool_context()).await, ToolOutput::ok("v1 call 3"));

    // 新版本加载不了（语法错误、占了内置工具的名字）：旧版本继续用，原因在报告里；同一份坏文件不反复尝试。
    f.plugin("live", "", "export const tools = [;");
    let report = plugins.reload();
    assert!(report.reloaded.is_empty() && report.kept[0].0 == "live", "{report:?}");
    assert_eq!(f.call(&main, "version", json!({})).await, ToolOutput::ok("v2 call 3"));
    assert!(plugins.reload().is_empty());
    f.plugin("live", "", "export const tools = [{ name: 'read_file', execute: () => 'gotcha' }];");
    assert!(plugins.reload().kept[0].1.contains("read_file"));
    assert_eq!(main.status("live"), Some(Status::Active));
    // 改好之后换上。
    f.plugin("live", "", &VERSION.replace("VERSION", "3"));
    assert_eq!(plugins.reload().reloaded, ["live"]);
    assert_eq!(f.call(&main, "version", json!({})).await, ToolOutput::ok("v3 call 1"));

    // 停用的插件改了也保持停用；和内置插件撞 id 的那个，重载时也不去动内置插件。
    f.plugin("off", "", "export const tools = [{ name: 'off_tool', execute: () => 'new!' }];").plugin(
        "fs",
        "",
        "export const tools = [{ name: 'impostor2', execute: () => '' }];",
    );
    plugins.reload();
    assert_eq!(main.status("off"), Some(Status::Disabled));
    assert_eq!(main.status("fs"), Some(Status::Active));
    let registry = ToolRegistry::from_snapshot(&main.snapshot());
    assert!(registry.get("read_file").is_some() && registry.get("impostor2").is_none() && registry.get("off_tool").is_none());
}

#[tokio::test]
async fn set_enabled_toggles_a_plugin_without_restarting() {
    let f = Fixture::new();
    f.plugin("live", "", &VERSION.replace("VERSION", "1")).plugin("bad", "", "export const tools = [;");
    let plugins = JsPlugins::discover(&f.plugins_dir());
    // 主代理和子代理两个 Context，开关要一起变。
    let (main, sub) = (Context::new(), Context::new());
    for cx in [&main, &sub] {
        cx.add_group(dshr_tools::BuiltinTools).unwrap();
        cx.add_group(plugins.clone()).unwrap();
    }
    assert_eq!(f.call(&main, "version", json!({})).await, ToolOutput::ok("v1 call 1"));

    // 关掉：两个 Context 都停用，工具消失；目录里没有的插件报错。
    let report = plugins.set_enabled("live", false).unwrap();
    assert_eq!(report.reloaded, ["live"]);
    assert_eq!(main.status("live"), Some(Status::Disabled));
    assert_eq!(sub.status("live"), Some(Status::Disabled));
    let registry = ToolRegistry::from_snapshot(&main.snapshot());
    assert!(registry.get("version").is_none());
    assert!(plugins.set_enabled("nobody", false).is_err());

    // 关掉的状态下改文件：保持停用，脚本不执行。
    f.plugin("live", "", &VERSION.replace("VERSION", "2"));
    plugins.reload();
    assert_eq!(main.status("live"), Some(Status::Disabled));
    assert!(ToolRegistry::from_snapshot(&main.snapshot()).get("version").is_none());

    // 再打开：两个 Context 都激活，用的是改过的文件，模块状态从头开始。
    let report = plugins.set_enabled("live", true).unwrap();
    assert_eq!(report.reloaded, ["live"]);
    assert_eq!(f.call(&main, "version", json!({})).await, ToolOutput::ok("v2 call 1"));
    assert_eq!(f.call(&sub, "version", json!({})).await, ToolOutput::ok("v2 call 2"));

    // 一个本来就激活失败的插件：拨开关（先关再开）会重试激活，原因还是新的；这不需要热重载。
    assert!(plugins.set_enabled("bad", false).is_ok());
    assert_eq!(main.status("bad"), Some(Status::Disabled));
    plugins.set_enabled("bad", true).unwrap();
    assert!(failure(&main, "bad").contains("index.js"));
    f.plugin("bad", "", "export const tools = [{ name: 'fixed', execute: () => 'ok' }];");
    plugins.reload();
    assert_eq!(f.call(&main, "fixed", json!({})).await, ToolOutput::ok("ok"));
}

#[tokio::test]
async fn set_enabled_reloads_only_that_plugin() {
    // 插件很多时，拨一个开关不该把别的插件都重新指纹、重新加载一遍（这是设置页开关卡顿的来源）。
    let f = Fixture::new();
    f.plugin("live", "", &VERSION.replace("VERSION", "1")).plugin(
        "other",
        "",
        "export const tools = [{ name: 'other', execute: () => 'v1' }];",
    );
    let plugins = JsPlugins::discover(&f.plugins_dir());
    let cx = Context::new();
    cx.add_group(dshr_tools::BuiltinTools).unwrap();
    cx.add_group(plugins.clone()).unwrap();
    // 另一个插件的文件改了：开关 `live` 时不顺带换它（留给监视线程的热重载）。
    f.plugin("other", "", "export const tools = [{ name: 'other', execute: () => 'v2' }];");
    let report = plugins.set_enabled("live", false).unwrap();
    assert_eq!(report.reloaded, ["live"]);
    assert!(report.removed.is_empty());
    assert_eq!(f.call(&cx, "other", json!({})).await, ToolOutput::ok("v1"));
    assert_eq!(plugins.reload().reloaded, ["other"]);
    assert_eq!(f.call(&cx, "other", json!({})).await, ToolOutput::ok("v2"));
}

#[tokio::test]
async fn reload_adds_removes_and_repairs_plugins() {
    let f = Fixture::new();
    f.plugin("broken", "", "export const tools = [;");
    let plugins = JsPlugins::discover(&f.plugins_dir());
    let cx = Context::new();
    cx.add_group(dshr_tools::BuiltinTools).unwrap();
    cx.add_group(plugins.clone()).unwrap();
    assert!(failure(&cx, "broken").contains("index.js"));

    // 启动时就坏着的插件：改成另一种坏法，失败原因跟着更新；改好了就激活。
    f.plugin("broken", "", "throw new Error('still broken');");
    plugins.reload();
    assert!(failure(&cx, "broken").contains("still broken"));
    f.plugin("broken", "", "export const tools = [{ name: 'fixed', execute: () => 'ok' }];");
    assert_eq!(plugins.reload().reloaded, ["broken"]);
    assert_eq!(f.call(&cx, "fixed", json!({})).await, ToolOutput::ok("ok"));

    // 运行中新放进来一个插件，再把它的清单改一下（多一项能力）。
    f.plugin("added", "", "export const tools = [{ name: 'hello', execute: (_, host) => host.readFile('a.txt') }];");
    assert_eq!(plugins.reload().reloaded, ["added"]);
    assert_eq!(plugins.ids(), ["added", "broken"]);
    std::fs::write(f.workspace.path().join("a.txt"), "内容").unwrap();
    assert!(f.call(&cx, "hello", json!({})).await.is_error);
    f.plugin(
        "added",
        "capabilities = [\"fs:workspace:read\"]\n",
        "export const tools = [{ name: 'hello', execute: (_, host) => host.readFile('a.txt') }];",
    );
    plugins.reload();
    assert_eq!(f.call(&cx, "hello", json!({})).await, ToolOutput::ok("内容"));

    // 删掉文件夹：插件卸掉，工具消失。
    std::fs::remove_dir_all(f.plugins_dir().join("added")).unwrap();
    assert_eq!(plugins.reload().removed, ["added"]);
    assert_eq!(cx.status("added"), None);
    assert!(ToolRegistry::from_snapshot(&cx.snapshot()).get("hello").is_none());
    assert!(ToolRegistry::from_snapshot(&cx.snapshot()).get("fixed").is_some());
}

#[tokio::test]
async fn watcher_reloads_after_the_files_settle() {
    let f = Fixture::new();
    f.plugin("live", "", &VERSION.replace("VERSION", "1"));
    let plugins = JsPlugins::discover(&f.plugins_dir());
    let cx = Context::new();
    cx.add_group(plugins.clone()).unwrap();
    let watcher = plugins.watch_every(Duration::from_millis(40));

    f.plugin("live", "", &VERSION.replace("VERSION", "22"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = f.call(&cx, "version", json!({})).await;
        if output.content.starts_with("v22") {
            break;
        }
        assert!(Instant::now() < deadline, "监视线程没有换上新版本：{output:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 丢弃之后线程停下，再改就不会自动重载了。
    drop(watcher);
    f.plugin("live", "", &VERSION.replace("VERSION", "333"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(f.call(&cx, "version", json!({})).await.content.starts_with("v22"));
}

/// 默认的监视方式靠文件变化通知：改了文件很快换上新版本；丢弃之后不再重载。
#[tokio::test]
async fn event_watcher_reloads_on_file_changes() {
    let f = Fixture::new();
    f.plugin("live", "", &VERSION.replace("VERSION", "1"));
    let plugins = JsPlugins::discover(&f.plugins_dir());
    let cx = Context::new();
    cx.add_group(plugins.clone()).unwrap();
    let watcher = plugins.watch();

    let changed = Instant::now();
    f.plugin("live", "", &VERSION.replace("VERSION", "22"));
    let deadline = changed + Duration::from_secs(10);
    loop {
        let output = f.call(&cx, "version", json!({})).await;
        if output.content.starts_with("v22") {
            break;
        }
        assert!(Instant::now() < deadline, "没有换上新版本：{output:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 通知 + 两次 300 ms 的“看稳”，远小于以前定时扫描的节奏上限。
    assert!(changed.elapsed() < Duration::from_secs(3), "用了 {:?}", changed.elapsed());

    drop(watcher);
    f.plugin("live", "", &VERSION.replace("VERSION", "333"));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(f.call(&cx, "version", json!({})).await.content.starts_with("v22"));
}

#[test]
fn net_capability_patterns() {
    let f = Fixture::new();
    f.plugin("web", "capabilities = [\"net:API.example.com\", \"net:*.github.com\", \"net:localhost\", \"net:[::1]\"]\n", "").plugin(
        "any",
        "capabilities = [\"net:*\"]\n",
        "",
    );
    let bad = ["net:", "net:https://example.com", "net:example.com:8080", "net:*example.com", "net:a/b", "net:*.*"];
    for (index, bad) in bad.into_iter().enumerate() {
        f.plugin(&format!("bad{index}"), &format!("capabilities = [\"{bad}\"]\n"), "");
    }
    let found = discover(&f.plugins_dir());
    let capabilities = |id: &str| found.iter().find(|plugin| plugin.id == id).unwrap().manifest.clone().unwrap().capabilities;
    assert_eq!(found.iter().filter(|plugin| plugin.manifest.is_err()).count(), 6);

    let web = capabilities("web");
    for host in ["api.example.com", "API.Example.com", "raw.github.com", "a.b.github.com", "localhost", "[::1]"] {
        assert!(web.allows_host(host), "{host}");
    }
    for host in ["example.com", "evil-example.com", "github.com", "notgithub.com", "127.0.0.1", "api.example.com.evil.io"] {
        assert!(!web.allows_host(host), "{host}");
    }
    // `*` 是任何公网主机名；本机和 IP 地址要逐个写明。
    let any = capabilities("any");
    assert!(any.allows_host("anything.example.org"));
    for host in ["localhost", "db.localhost", "127.0.0.1", "10.0.0.8", "[::1]"] {
        assert!(!any.allows_host(host), "{host}");
    }
}

/// 一个只在测试里用的 HTTP 服务：每个连接回一个响应就关。返回它的端口。
fn serve() -> u16 {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                // 读到请求头结束，再按 Content-Length 把正文读完。
                let (head, body) = loop {
                    let Ok(read) = stream.read(&mut buffer) else { return };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request).into_owned();
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
                let (method, path) = (first.next().unwrap_or_default(), first.next().unwrap_or_default());
                let header = |name: &str| {
                    head.lines()
                        .find_map(|line| {
                            line.split_once(':').filter(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_owned())
                        })
                        .unwrap_or_default()
                };
                let respond = |stream: &mut std::net::TcpStream, status: &str, extra: &str, body: &str| {
                    let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}", body.len());
                };
                match path {
                    "/echo" => {
                        let reply = json!({"method": method, "body": body, "x": header("x-test"), "agent": header("user-agent")});
                        respond(&mut stream, "200 OK", "X-Reply: yes\r\n", &reply.to_string());
                    }
                    "/missing" => respond(&mut stream, "404 Not Found", "", "nope"),
                    "/inside" => respond(&mut stream, "302 Found", "Location: /echo\r\n", ""),
                    "/outside" => respond(&mut stream, "302 Found", &format!("Location: http://localhost:{port}/echo\r\n"), ""),
                    "/big" => respond(&mut stream, "200 OK", "", &"x".repeat(3 * 1024 * 1024)),
                    // 一直不回。
                    _ => std::thread::sleep(Duration::from_secs(30)),
                }
            });
        }
    });
    port
}

const WEB: &str = r#"
const fetchTool = (name, readOnly) => ({
  name,
  readOnly,
  async execute({ url, options }, host) {
    const response = await host.fetch(url, options);
    const echoed = response.ok ? await response.json() : null;
    return JSON.stringify({
      status: response.status, ok: response.ok, url: response.url,
      reply: response.headers["x-reply"], text: (await response.text()).slice(0, 20), echoed,
    });
  },
});
export const prompt = async (_, host) => { await host.fetch("http://127.0.0.1:1/"); return "never"; };
export const tools = [fetchTool("PREFIX_get", false), fetchTool("PREFIX_peek", true)];
"#;

#[tokio::test]
async fn fetch_reaches_only_declared_hosts() {
    use dshr_agent::{PromptContext, PromptParts};

    let port = serve();
    let f = Fixture::new();
    f.plugin("web", "capabilities = [\"net:127.0.0.1\"]\ntimeout_ms = 1500\n", &WEB.replace("PREFIX", "web"))
        .plugin("wild", "capabilities = [\"net:*\"]\n", &WEB.replace("PREFIX", "wild"))
        .plugin("offline", "", &WEB.replace("PREFIX", "offline"));
    let cx = f.context();
    let ctx = f.tool_context();
    let base = format!("http://127.0.0.1:{port}");
    let get = async |tool: &str, url: String, options: Value| f.call(&cx, tool, json!({"url": url, "options": options})).await;
    let parsed = |output: &ToolOutput| -> Value {
        assert!(!output.is_error, "{output:?}");
        serde_json::from_str(&output.content).unwrap()
    };

    // 声明过的主机：GET、带正文和请求头的 POST、非 2xx、主机范围内的重定向。
    let plain = parsed(&get("web_get", format!("{base}/echo"), json!({})).await);
    assert_eq!((plain["status"].as_u64(), plain["ok"].as_bool(), plain["reply"].as_str()), (Some(200), Some(true), Some("yes")));
    assert_eq!(plain["echoed"], json!({"method": "GET", "body": "", "x": "", "agent": "dshr-plugin"}));
    let options = json!({"method": "post", "headers": {"X-Test": 7}, "body": "你好"});
    let posted = parsed(&get("web_get", format!("{base}/echo"), options).await);
    assert_eq!(posted["echoed"], json!({"method": "POST", "body": "你好", "x": "7", "agent": "dshr-plugin"}));
    let missing = parsed(&get("web_get", format!("{base}/missing"), json!({})).await);
    assert_eq!((missing["status"].as_u64(), missing["ok"].as_bool(), missing["text"].as_str()), (Some(404), Some(false), Some("nope")));
    let inside = parsed(&get("web_get", format!("{base}/inside"), json!({})).await);
    assert_eq!((inside["status"].as_u64(), inside["url"].as_str()), (Some(200), Some(format!("{base}/echo").as_str())));

    // 重定向到没声明的主机：不跟过去，脚本拿到的是那个 302。
    let outside = parsed(&get("web_get", format!("{base}/outside"), json!({})).await);
    assert_eq!((outside["status"].as_u64(), outside["url"].as_str()), (Some(302), Some(format!("{base}/outside").as_str())));

    // 没声明的主机、别的协议、不认识的方法、过大的响应。
    for (url, expected) in [
        (format!("http://localhost:{port}/echo"), "net:localhost"),
        ("https://example.com/".to_owned(), "net:example.com"),
        ("file:///etc/passwd".to_owned(), "only http and https"),
        ("not a url".to_owned(), "invalid URL"),
    ] {
        let refused = get("web_get", url, json!({})).await;
        assert!(refused.is_error && refused.content.contains(expected), "{refused:?}");
    }
    let method = get("web_get", format!("{base}/echo"), json!({"method": "TRACE"})).await;
    assert!(method.is_error && method.content.contains("TRACE"), "{method:?}");
    let big = get("web_get", format!("{base}/big"), json!({})).await;
    assert!(big.is_error && big.content.contains("larger than the limit"), "{big:?}");

    // `net:*` 不包括本机和 IP 地址；没声明能力的插件根本不能联网。
    let wild = get("wild_get", format!("{base}/echo"), json!({})).await;
    assert!(wild.is_error && wild.content.contains("net:127.0.0.1"), "{wild:?}");
    let offline = get("offline_get", format!("{base}/echo"), json!({})).await;
    assert!(offline.is_error && offline.content.contains("net:<host>"), "{offline:?}");

    // 能联网的工具每次都要问（和 MCP 的工具一样）；标了 readOnly 的不问，但也联不了网；prompt 里同样不行。
    assert_eq!(f.tool(&cx, "web_get").access(&json!({}), &ctx), Access::External);
    assert_eq!(f.tool(&cx, "offline_get").access(&json!({}), &ctx), Access::Read);
    assert_eq!(f.tool(&cx, "web_peek").access(&json!({}), &ctx), Access::Read);
    let peek = get("web_peek", format!("{base}/echo"), json!({})).await;
    assert!(peek.is_error && peek.content.contains("readOnly"), "{peek:?}");
    let options = dshr_context::Options::new();
    let prompt_cx = PromptContext { workspace: f.workspace.path(), options: &options, plugins: None };
    assert!(cx.snapshot().items::<PromptParts>().iter().all(|part| part.contribute(&prompt_cx).is_none()));

    // 对方一直不回：到这次调用的时限为止（1.5 秒），之后插件照常可用。
    let started = Instant::now();
    let hung = get("web_get", format!("{base}/hang"), json!({})).await;
    assert!(hung.is_error && started.elapsed() < Duration::from_secs(5), "{hung:?}");
    assert!(!get("web_get", format!("{base}/echo"), json!({})).await.is_error);

    // 调用方放弃时，等着的请求立刻中止，线程空出来。
    let abandoned = get("web_get", format!("{base}/hang"), json!({}));
    assert!(tokio::time::timeout(Duration::from_millis(200), abandoned).await.is_err());
    let started = Instant::now();
    assert!(!get("web_get", format!("{base}/echo"), json!({})).await.is_error);
    assert!(started.elapsed() < Duration::from_secs(1));
}

const NOTES: &str = r#"
let projected = 0;
export const prompt = async ({ workspace }, host) =>
  `Notes plugin is on. Files: ${(await host.listDir(".")).join(",")}. Same workspace: ${workspace === host.workspace}`;
export const events = {
  "notes/add": (data) => (data.text ? `[note ${++projected}] ${data.text}` : undefined),
  "notes/pinned": { project: async (data) => `[pinned] ${data.text}`, label: (data) => `📌 ${data.text}` },
  "notes/shown": { label: ({ text }) => text || undefined },
};
export const tools = [
  { name: "note", execute({ text }, host) { host.emitEvent("notes/add", { text }); return "ok"; } },
  { name: "forge", execute(_, host) { host.emitEvent("other/thing", {}); return "ok"; } },
];
"#;

#[tokio::test]
async fn prompt_and_event_projection_exports() {
    use dshr_agent::{EventKinds, PromptContext, PromptParts};

    let f = Fixture::new();
    f.plugin("notes", "capabilities = [\"fs:workspace:read\"]\n", NOTES);
    std::fs::write(f.workspace.path().join("a.txt"), "").unwrap();
    let cx = f.context();
    assert_eq!(cx.status("notes"), Some(Status::Active));
    let snapshot = cx.snapshot();

    // prompt：可以是 async 的，可以用 host 读工作区。
    let options = dshr_context::Options::new();
    let prompt_cx = PromptContext { workspace: f.workspace.path(), options: &options, plugins: None };
    let parts: Vec<_> = snapshot.items::<PromptParts>().iter().filter_map(|part| part.contribute(&prompt_cx)).collect();
    assert_eq!(parts, ["Notes plugin is on. Files: a.txt. Same workspace: true"]);

    // events：每种事件类型一个 projector；同样的数据只问脚本一次，之后用缓存（计数没有再涨）。
    let projectors = snapshot.items::<EventKinds>();
    let project = |kind: &str, data: Value| projectors.iter().find(|p| p.kind() == kind).unwrap().project(&data);
    assert_eq!(project("notes/add", json!({"text": "周五发版"})).as_deref(), Some("[note 1] 周五发版"));
    assert_eq!(project("notes/add", json!({"text": "周五发版"})).as_deref(), Some("[note 1] 周五发版"));
    assert_eq!(project("notes/add", json!({"text": "买牛奶"})).as_deref(), Some("[note 2] 买牛奶"));
    assert_eq!(project("notes/add", json!({"oops": 1})), None);
    assert_eq!(project("notes/pinned", json!({"text": "x"})).as_deref(), Some("[pinned] x"));

    // label：对话区里显示的一行字，可以没有 project（那样事件不进上下文）。
    let labels = snapshot.items::<dshr_agent::EventLabels>();
    let label = |kind: &str, data: Value| labels.iter().find(|l| l.kind() == kind).unwrap().label(&data);
    let kinds = |items: Vec<&str>| items.into_iter().map(str::to_owned).collect::<std::collections::BTreeSet<_>>();
    // `plugins/reviewed` 是宿主自己的标签（审查结论），不算插件导出的。
    assert_eq!(
        kinds(labels.iter().map(|l| l.kind()).filter(|kind| *kind != "plugins/reviewed").collect()),
        kinds(vec!["notes/pinned", "notes/shown"])
    );
    assert!(!projectors.iter().any(|p| p.kind() == "notes/shown"));
    assert_eq!(label("notes/pinned", json!({"text": "x"})).as_deref(), Some("📌 x"));
    assert_eq!(label("notes/shown", json!({"text": "hi"})).as_deref(), Some("hi"));
    assert_eq!(label("notes/shown", json!({})), None);

    // 写事件只能写自己命名空间里的。
    let forged = f.call(&cx, "forge", json!({})).await;
    assert!(forged.is_error && forged.content.contains("notes/"), "{forged:?}");
}

#[test]
fn bad_prompt_and_event_exports() {
    use dshr_agent::{EventKinds, PromptContext, PromptParts};

    let f = Fixture::new();
    f.plugin("foreign", "", "export const events = { 'other/thing': () => 'x' };")
        .plugin("core", "", "export const events = { 'session/title': () => 'x' };")
        .plugin("shape", "", "export const events = { 'shape/x': 42 };")
        .plugin("badlabel", "", "export const events = { 'badlabel/x': { label: 'text' } };")
        .plugin("notfn", "", "export const prompt = 'just text';")
        .plugin(
            "flaky",
            "",
            r#"export const prompt = () => { throw new Error("no prompt today"); };
               export const events = { "flaky/spin": () => { while (true) {} }, "flaky/ok": () => "fine" };"#,
        );
    let cx = f.context();
    assert!(failure(&cx, "foreign").contains("foreign/"));
    assert!(failure(&cx, "core").contains("session/title"));
    assert!(failure(&cx, "shape").contains("project"));
    assert!(failure(&cx, "badlabel").contains("label"));
    assert!(failure(&cx, "notfn").contains("prompt"));
    assert_eq!(cx.status("flaky"), Some(Status::Active));

    // 出错的 prompt、卡住的投影都只是“这次没有内容”，不让对话失败。
    let snapshot = cx.snapshot();
    let options = dshr_context::Options::new();
    let prompt_cx = PromptContext { workspace: f.workspace.path(), options: &options, plugins: None };
    assert!(snapshot.items::<PromptParts>().iter().all(|part| part.contribute(&prompt_cx).is_none()));
    let projectors = snapshot.items::<EventKinds>();
    let project = |kind: &str| projectors.iter().find(|p| p.kind() == kind).unwrap().project(&json!({}));
    let started = Instant::now();
    assert_eq!(project("flaky/spin"), None);
    assert!(started.elapsed() < Duration::from_secs(3));
    // 刚超时过，紧接着的询问直接放弃（不让一长串事件每条都等到超时）；过一会儿恢复正常。
    let started = Instant::now();
    assert_eq!(project("flaky/ok"), None);
    assert!(started.elapsed() < Duration::from_millis(200));
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(project("flaky/ok").as_deref(), Some("fine"));
}

/// 走一遍真正的 agent 循环：模型调用 JS 工具，结果和工具写的事件按顺序进日志。
#[tokio::test]
async fn agent_calls_a_js_tool() {
    let f = Fixture::new();
    f.plugin(
        "word-count",
        "capabilities = [\"fs:workspace:read\"]\n",
        r#"export const prompt = () => "Use word_count for anything about document length.";
        export const events = { "word-count/counted": ({ path, chars }) => `[counted] ${path}: ${chars}` };
        export const tools = [{
          name: "word_count",
          description: "Count the characters in a file.",
          inputSchema: { type: "object", properties: { path: { type: "string" } }, required: ["path"] },
          async execute({ path }, host) {
            const text = await host.readFile(path);
            host.emitEvent("word-count/counted", { path, chars: [...text].length });
            return `${[...text].length} 字`;
          },
        }];"#,
    );
    std::fs::write(f.workspace.path().join("README.md"), "你好，世界").unwrap();

    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    plugins.add_group(AgentPlugins).unwrap();
    plugins.add_group(JsPlugins::discover(&f.plugins_dir())).unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "word_count", json!({"path": "README.md"})),
        ScriptedProvider::text_reply("一共 5 个字"),
    ]));
    // 手动确认模式：只读的 JS 工具不用询问就放行（审批一律拒绝，问了就会失败）。
    struct DenyAll;
    #[async_trait::async_trait]
    impl dshr_agent::Approver for DenyAll {
        async fn approve(&self, _request: &dshr_agent::ApprovalRequest) -> dshr_agent::Decision {
            dshr_agent::Decision::Deny
        }
    }
    let agent =
        Agent::new(provider.clone(), plugins, Arc::new(DenyAll), f.tool_context(), AgentConfig::default().with(PermissionMode::Manual));
    let mut session = Session::create("s1", f.home.path().join("log").join("session.v1.jsonl")).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut session, "README 有多少字？".into(), &tx, &CancellationToken::new()).await.unwrap();

    assert!(provider.requests()[0].tools.iter().any(|tool| tool.name == "word_count"));
    let bodies: Vec<&EventBody> = session.events().iter().map(|event| &event.body).collect();
    let call = bodies.iter().position(|body| body.kind() == "tool/call").unwrap();
    assert!(matches!(bodies[call + 1], EventBody::ToolResult { output, is_error: false, .. } if &**output == "5 字"));
    assert!(matches!(
        bodies[call + 2],
        EventBody::Ext { kind, data } if kind == "word-count/counted" && data["chars"] == 5
    ));

    // prompt 导出追加在系统提示词后面；事件经 events 导出投影后，出现在下一次请求的上下文里。
    let requests = provider.requests();
    assert!(requests[0].system.as_deref().unwrap().ends_with("Use word_count for anything about document length."));
    let texts: Vec<String> = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            // 日期和权限模式的环境说明不是这里关心的。
            dshr_core::ContentBlock::Text { text } if text.starts_with("<environment>") => None,
            dshr_core::ContentBlock::Text { text } => Some(text.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["README 有多少字？", "[counted] README.md: 5"]);
}

/// 记下每次审批请求，全部允许。
#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

#[async_trait::async_trait]
impl dshr_agent::Approver for Recorder {
    async fn approve(&self, request: &dshr_agent::ApprovalRequest) -> dshr_agent::Decision {
        self.0.lock().unwrap().push(format!("{} {}", request.tool, request.input));
        dshr_agent::Decision::Allow
    }
}

/// 创造模式：模型写插件文件、重载、请用户授权、调用新工具；其余模式下这些工具不出现，调用也被拒。
#[tokio::test]
async fn creation_mode_builds_loads_and_grants_a_plugin() {
    use dshr_agent::Preset;

    let f = Fixture::new();
    std::fs::write(f.workspace.path().join("a.txt"), "hello").unwrap();
    let scripts = JsPlugins::discover(&f.plugins_dir());
    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    plugins.add_group(AgentPlugins).unwrap();
    plugins.add(crate::CreatorPlugin::new(scripts.clone())).unwrap();
    plugins.add_group(scripts.clone()).unwrap();

    let manifest = "id = \"shouter\"\ncapabilities = [\"fs:workspace:read\"]\n";
    let script = "export const tools = [{ name: 'shout', inputSchema: { type: 'object' }, async execute({ path }, host) { return (await host.readFile(path)).toUpperCase(); } }];";
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("c1", "plugin_read", json!({"id": "shouter"})),
        ScriptedProvider::tool_reply("c2", "plugin_write", json!({"id": "shouter", "path": "plugin.toml", "content": manifest})),
        ScriptedProvider::tool_reply("c3", "plugin_write", json!({"id": "shouter", "path": "index.js", "content": script})),
        ScriptedProvider::tool_reply("c4", "plugin_write", json!({"id": "shouter", "path": "../escape.js", "content": ""})),
        ScriptedProvider::tool_reply("c5", "plugin_reload", json!({"id": "shouter"})),
        ScriptedProvider::tool_reply("c6", "plugin_grant", json!({"id": "shouter", "capabilities": ["net:*"]})),
        ScriptedProvider::tool_reply("c7", "plugin_grant", json!({"id": "shouter", "capabilities": ["fs:workspace:read"]})),
        ScriptedProvider::tool_reply("c8", "shout", json!({"path": "a.txt"})),
        ScriptedProvider::text_reply("做好了"),
    ]));
    let approver = Arc::new(Recorder::default());
    // 完全访问模式：写插件文件不问，授权照样问。
    let agent = Agent::new(
        provider.clone(),
        plugins.clone(),
        approver.clone(),
        f.tool_context(),
        AgentConfig::default().with(PermissionMode::FullAccess).with(Preset::Create),
    );
    let mut session = Session::create("s1", f.home.path().join("log").join("session.v1.jsonl")).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut session, "做一个把文件内容变大写的工具".into(), &tx, &CancellationToken::new()).await.unwrap();

    let results: Vec<(String, bool)> = session
        .events()
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolResult { output, is_error, .. } => Some((output.to_string(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 8, "{results:#?}");
    assert!(results[0].0.contains("the id is free"), "{results:#?}");
    assert!(!results[1].1 && !results[2].1);
    assert!(results[3].1 && !f.plugins_dir().join("escape.js").exists(), "{:?}", results[3]);
    assert!(results[4].0.contains("Capabilities waiting for approval: fs:workspace:read"), "{:?}", results[4]);
    assert!(results[5].1 && results[5].0.contains("Nothing granted"), "{:?}", results[5]);
    assert!(results[6].0.contains("approved") && results[6].0.contains("Tools (callable from your next step): shout"), "{:?}", results[6]);
    assert_eq!(results[7], ("HELLO".to_owned(), false));
    // 两次授权都问了，写文件一次都没问。
    let asked = approver.0.lock().unwrap().clone();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(asked.iter().all(|line| line.starts_with("plugin_grant")));

    // 创造模式的请求里有写法说明和 plugin_* 工具；新插件的工具从重载之后的那一步起出现。
    let requests = provider.requests();
    assert!(requests[0].system.as_deref().unwrap().contains("Creation mode: build a dshr plugin"));
    assert!(requests[0].tools.iter().any(|tool| tool.name == "plugin_write"));
    assert!(!requests[0].tools.iter().any(|tool| tool.name == "shout"));
    assert!(requests[7].tools.iter().any(|tool| tool.name == "shout"));

    // 标准模式：看不到这些工具，硬调也被拒。
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_reply("d1", "plugin_write", json!({"id": "other", "path": "index.js", "content": ""})),
        ScriptedProvider::text_reply("好"),
    ]));
    let agent = Agent::new(provider.clone(), plugins, approver, f.tool_context(), AgentConfig::default().with(PermissionMode::FullAccess));
    agent.run_turn(&mut session, "随便".into(), &tx, &CancellationToken::new()).await.unwrap();
    assert!(!provider.requests()[0].tools.iter().any(|tool| tool.name.starts_with("plugin_")));
    assert!(!provider.requests()[0].system.as_deref().unwrap_or_default().contains("Creation mode"));
    let denied = session.events().iter().rev().find_map(|event| match &event.body {
        EventBody::ToolResult { output, .. } => Some(output.clone()),
        _ => None,
    });
    assert!(denied.unwrap().contains("only available in creation mode"));
    assert!(!f.plugins_dir().join("other").exists());
}
/// 创造模式不让模型跑偏：逆向程序、翻安装目录、往工作区写临时脚本都被拒绝；调查太多次还没写插件时，
/// 请求里只留 `plugin_*` 工具；`plugin_docs` 能查到配色方案的写法；重载报告列出导出的配色方案。
#[tokio::test]
async fn creation_mode_keeps_the_model_on_task() {
    use dshr_agent::Preset;

    let f = Fixture::new();
    let scripts = JsPlugins::discover(&f.plugins_dir());
    let plugins = Context::new();
    plugins.add_group(dshr_tools::BuiltinTools).unwrap();
    plugins.add_group(AgentPlugins).unwrap();
    plugins.add(crate::CreatorPlugin::new(scripts.clone())).unwrap();
    plugins.add_group(scripts.clone()).unwrap();

    let theme = "export const themes = [{ id: 'pink', name: 'Pink', light: { accent: '#D63384' }, dark: { accent: '#F08CBF' } }];";
    let mut replies = vec![
        ScriptedProvider::tool_reply("a1", "read_file", json!({"path": "dshr/dshr.exe"})),
        ScriptedProvider::tool_reply("a2", "run_shell", json!({"command": "strings dshr/dshr | findstr theme"})),
        ScriptedProvider::tool_reply("a3", "write_file", json!({"path": "tmp_theme_scan.ps1", "content": "x"})),
        ScriptedProvider::tool_reply("a4", "plugin_docs", json!({"topic": "themes"})),
    ];
    for i in 0..11 {
        replies.push(ScriptedProvider::tool_reply(&format!("g{i}"), "list_dir", json!({"path": "."})));
    }
    replies.push(ScriptedProvider::tool_reply("w1", "plugin_write", json!({"id": "pink", "path": "index.js", "content": theme})));
    replies.push(ScriptedProvider::tool_reply("w2", "plugin_reload", json!({"id": "pink"})));
    replies.push(ScriptedProvider::text_reply("做好了"));
    let provider = Arc::new(ScriptedProvider::new(replies));
    let agent = Agent::new(
        provider.clone(),
        plugins,
        Arc::new(Recorder::default()),
        f.tool_context(),
        AgentConfig::default().with(PermissionMode::FullAccess).with(Preset::Create),
    );
    let mut session = Session::create("s1", f.home.path().join("log").join("session.v1.jsonl")).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.run_turn(&mut session, "制作一个粉色主题插件".into(), &tx, &CancellationToken::new()).await.unwrap();

    let results: Vec<(String, bool)> = session
        .events()
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::ToolResult { output, is_error, .. } => Some((output.to_string(), *is_error)),
            _ => None,
        })
        .collect();
    assert!(results[0].1 && results[0].0.contains("program binary"), "{:?}", results[0]);
    assert!(results[1].1 && results[1].0.contains("off-task"), "{:?}", results[1]);
    assert!(results[2].1 && results[2].0.contains("plugin_write"), "{:?}", results[2]);
    assert!(!f.workspace.path().join("tmp_theme_scan.ps1").exists());
    assert!(!results[3].1 && results[3].0.contains("accent_soft"), "{:?}", &results[3].0[..200.min(results[3].0.len())]);
    let reload = &results.last().unwrap().0;
    assert!(reload.contains("color scheme `pink/pink`"), "{reload}");

    // 每次请求里的工具：调查到第 14 次之后只剩 plugin_*，写出插件文件后恢复。
    let requests = provider.requests();
    let only_plugin = |index: usize| requests[index].tools.iter().all(|tool| tool.name.starts_with("plugin_"));
    let nudged = |index: usize| requests[index].system.as_deref().unwrap().contains("no plugin file yet");
    // 请求 n 之前已经有 n 次调用；其中 plugin_docs 不算调查。
    assert!(!nudged(6) && nudged(7), "soft limit after 6 look-around calls");
    assert!(!only_plugin(14) && only_plugin(15), "hard limit after 14 look-around calls");
    assert!(requests[15].tools.iter().any(|tool| tool.name == "plugin_docs"));
    assert!(!only_plugin(16) && !nudged(16), "a plugin file lifts the limit");
}

/// `ui` 导出：入口、藏起来的内置入口、页面的渲染与动作；创造模式的重载报告里有每一页实际画一次的结果。
#[tokio::test]
async fn ui_export_adds_entries_and_pages() {
    use dshr_agent::{EntryPlace, UiSurfaces};

    let f = Fixture::new();
    f.plugin(
        "todo",
        "capabilities = [\"fs:workspace:read\", \"fs:workspace:write\"]\n",
        r#"
        const items = [];
        const page = (host) => ({ type: "column", children: [
          { type: "text", text: `${items.length} items`, style: "heading" },
          { type: "row", children: [
            { type: "input", id: "title", placeholder: "New item", value: "" },
            { type: "button", label: "Add", action: "add", variant: "primary" },
          ]},
          { type: "list", items: items.map((title, index) => ({ title, action: "remove", payload: index })) },
        ]});
        export const ui = {
          pages: {
            main: { render: page, actions: {
              add: async ({ inputs }, host) => { items.push(inputs.title); await host.writeFile("todo.json", JSON.stringify(items)); return `added ${inputs.title}`; },
              remove: ({ value }) => { items.splice(value, 1); },
            }},
            broken: () => ({ type: "gauge" }),
          },
          sidebar: [{ id: "todo", label: "待办", icon: "star", page: "main" }],
          settings: [{ id: "todo-settings", label: "待办设置", page: "broken" }],
          hide: ["composer:presets", "status:account"],
        };
        "#,
    )
    .plugin("greedy", "", "export const ui = { pages: { p: () => 'x' }, hide: ['settings:extensions'] };")
    .plugin("dangling", "", "export const ui = { pages: {}, sidebar: [{ id: 'x', label: 'X', page: 'nope' }] };");
    let scripts = JsPlugins::discover(&f.plugins_dir());
    let cx = Context::new();
    cx.add_group(scripts.clone()).unwrap();

    // 藏不得的入口、指向不存在页面的入口都让插件激活失败。
    assert!(failure(&cx, "greedy").contains("settings:extensions"));
    assert!(failure(&cx, "dangling").contains("nope"));

    let snapshot = cx.snapshot();
    let surface = snapshot.items::<UiSurfaces>().iter().find(|surface| surface.plugin() == "todo").unwrap().clone();
    let places: Vec<_> = surface.entries().iter().map(|entry| (entry.place, entry.id.as_str(), entry.page.as_str())).collect();
    assert_eq!(places, [(EntryPlace::Sidebar, "todo", "main"), (EntryPlace::Settings, "todo-settings", "broken")]);
    assert_eq!(surface.hidden(), ["composer:presets", "status:account"]);

    let workspace = f.workspace.path();
    let tree = surface.render("main", Some(workspace)).unwrap();
    assert_eq!(tree["children"][0]["text"], "0 items");
    // 动作拿到各输入框的值；能力照常生效（写工作区）；返回的一句话交回界面。
    let message = surface.act("main", "add", json!({ "value": null, "inputs": { "title": "买牛奶" } }), Some(workspace)).unwrap();
    assert_eq!(message.as_deref(), Some("added 买牛奶"));
    assert_eq!(std::fs::read_to_string(workspace.join("todo.json")).unwrap(), r#"["买牛奶"]"#);
    let tree = surface.render("main", Some(workspace)).unwrap();
    assert_eq!(tree["children"][2]["items"][0]["title"], "买牛奶");
    assert_eq!(surface.act("main", "remove", json!({ "value": 0, "inputs": {} }), Some(workspace)).unwrap(), None);
    assert_eq!(surface.render("main", Some(workspace)).unwrap()["children"][0]["text"], "0 items");

    // 没有工作区时文件函数指向插件自己的数据目录。
    surface.act("main", "add", json!({ "inputs": { "title": "x" } }), None).unwrap();
    assert!(f.home.path().join("plugin-data").join("todo").join("todo.json").is_file());

    // 节点不合规、动作不存在：说清楚错在哪。
    assert!(surface.render("broken", None).unwrap_err().contains("unknown node type `gauge`"));
    assert!(surface.act("main", "nope", json!({}), None).unwrap_err().contains("no action nope"));

    let report = scripts.report("todo").unwrap();
    let ui = report.ui.join("\n");
    assert!(ui.contains("sidebar entry `todo`") && ui.contains("hides built-in entries: composer:presets"), "{ui}");
    assert!(ui.contains("page `main` renders") && ui.contains("page `broken` FAILED"), "{ui}");
}
/// `host.readData` / `writeData`：插件自己的数据目录，不需要能力；工具和页面看到的是同一份。
#[tokio::test]
async fn plugin_data_is_private_and_shared_between_tools_and_pages() {
    use dshr_agent::UiSurfaces;

    let f = Fixture::new();
    f.plugin(
        "notes",
        "",
        r#"
        const load = async (host) => JSON.parse((await host.readData("notes.json")) ?? "[]");
        export const tools = [
          { name: "note_add", async execute({ text }, host) { const notes = await load(host); notes.push(text); await host.writeData("notes.json", JSON.stringify(notes)); return notes.length; } },
          { name: "note_escape", async execute(_, host) { await host.writeData("../escape.txt", "x"); } },
        ];
        export const ui = { pages: { main: async (host) => ({ type: "column", children: (await load(host)).map(String) }) },
                            sidebar: [{ id: "notes", label: "Notes", page: "main" }] };
        "#,
    );
    let cx = f.context();
    assert_eq!(f.call(&cx, "note_add", json!({ "text": "a" })).await, ToolOutput::ok("1"));
    assert_eq!(f.call(&cx, "note_add", json!({ "text": "b" })).await, ToolOutput::ok("2"));
    let escaped = f.call(&cx, "note_escape", json!({})).await;
    assert!(escaped.is_error && escaped.content.contains("no `..`"), "{escaped:?}");
    assert!(f.home.path().join("plugin-data").join("notes").join("notes.json").is_file());

    // 页面不管有没有打开工作区，读到的都是同一份。
    let snapshot = cx.snapshot();
    let surface = snapshot.items::<UiSurfaces>()[0].clone();
    for workspace in [None, Some(f.workspace.path())] {
        assert_eq!(surface.render("main", workspace).unwrap()["children"], json!(["a", "b"]));
    }
}
mod ext;

mod round2;

mod bundle;

mod round3;

mod dist;
