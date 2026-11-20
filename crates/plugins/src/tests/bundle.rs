//! 插件包：自带 Skills、命令、预设、MCP 服务器；没有脚本的“纯包”插件。

use super::*;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use dshr_agent::{CommandResult, PromptContext, PromptParts, UiSurfaces};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// 内存里的假 MCP 服务器：一个工具 `ping` 回 `pong`。记下每次连接用的启动参数。
#[derive(Default)]
struct FakeMcp {
    launches: Mutex<Vec<dshr_mcp::Launch>>,
    connects: AtomicUsize,
}

#[async_trait::async_trait]
impl dshr_mcp::Connector for FakeMcp {
    async fn connect(
        &self,
        launch: &dshr_mcp::Launch,
        _logs: &Arc<dshr_mcp::LogBuffer>,
    ) -> Result<dshr_mcp::Connection, dshr_mcp::McpError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        self.launches.lock().unwrap().push(launch.clone());
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(theirs);
            let mut lines = tokio::io::BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let message: Value = serde_json::from_str(&line).unwrap();
                let id = message["id"].clone();
                let reply = match message["method"].as_str() {
                    Some("initialize") => json!({"jsonrpc": "2.0", "id": id, "result": {
                        "protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}}),
                    Some("tools/list") => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "ping"}]}}),
                    Some("tools/call") => json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": "pong"}]}}),
                    _ => continue,
                };
                if writer.write_all(format!("{reply}\n").as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        let (reader, writer) = tokio::io::split(ours);
        Ok(dshr_mcp::Connection { client: dshr_mcp::Client::new(reader, writer), process: None })
    }
}

/// 整个测试程序共用一个假连接器（只有插件包的服务器用它）。
fn fake_mcp() -> Arc<FakeMcp> {
    static FAKE: std::sync::OnceLock<Arc<FakeMcp>> = std::sync::OnceLock::new();
    FAKE.get_or_init(|| {
        let fake = Arc::new(FakeMcp::default());
        let _ = crate::js::TEST_CONNECTOR.set(fake.clone());
        fake
    })
    .clone()
}

impl Fixture {
    /// 写一个文件（相对插件目录）。
    fn file(&self, path: &str, text: &str) -> &Self {
        let path = self.plugins_dir().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        self
    }

    /// 有 Skills 插件的 Context（看系统提示词里的 Skills）。
    fn context_with_skills(&self) -> Context {
        let cx = Context::new();
        cx.add_group(dshr_tools::BuiltinTools).unwrap();
        cx.add(dshr_skills::SkillsPlugin::new(self.home.path())).unwrap();
        cx.add_group(JsPlugins::discover(&self.plugins_dir())).unwrap();
        cx
    }
}

fn prompt_text(f: &Fixture, cx: &Context) -> String {
    let snapshot = cx.snapshot();
    let options = dshr_context::Options::new();
    let prompt_cx = PromptContext { workspace: f.workspace.path(), options: &options, plugins: Some(&snapshot) };
    snapshot.items::<PromptParts>().iter().filter_map(|part| part.contribute(&prompt_cx)).collect::<Vec<_>>().join("\n")
}

#[test]
fn script_less_bundles_bring_skills_commands_and_presets() {
    let f = Fixture::new();
    f.file("kit/plugin.toml", "name = \"工具包\"\n")
        .file("kit/skills/release/SKILL.md", "---\nname: release\ndescription: 写发布说明\n---\n步骤……\n")
        .file("kit/commands/review.md", "---\ndescription: 审查代码\nargument-hint: <文件>\n---\n请审查 $ARGUMENTS，列出问题。\n")
        .file("kit/commands/ship.md", "---\ndescription: 发版\nmode: send\n---\n准备发版。\n")
        .file(
            "kit/presets/careful.md",
            "---\nlabel: 谨慎\ndescription: 只读地看一遍\nreadOnly: true\ntools: [read_file, grep]\n---\n先读再说。\n",
        );
    let cx = f.context_with_skills();
    assert_eq!(cx.status("kit"), Some(Status::Active));

    // Skills 以 `<插件>:<名字>` 出现在系统提示词里。
    let prompt = prompt_text(&f, &cx);
    assert!(prompt.contains("- kit:release: 写发布说明 ("), "{prompt}");

    // 命令是 `/kit:<名字>`，套上参数。
    let snapshot = cx.snapshot();
    let surface = snapshot.items::<UiSurfaces>().iter().find(|surface| surface.plugin() == "kit").unwrap().clone();
    let names: Vec<String> = surface.slash_commands().into_iter().map(|entry| entry.name).collect();
    assert_eq!(names, ["kit:review", "kit:ship"]);
    assert_eq!(
        surface.run_command("kit:review", true, "src/main.rs", None).unwrap(),
        CommandResult::Insert("请审查 src/main.rs，列出问题。".into())
    );
    assert_eq!(surface.run_command("kit:ship", true, "", None).unwrap(), CommandResult::Send("准备发版。".into()));
    assert!(surface.run_command("kit:nope", true, "", None).is_err());

    // 预设。
    let presets = snapshot.items::<dshr_agent::PluginPresets>();
    let preset = presets.iter().find(|preset| preset.plugin == "kit").unwrap();
    assert_eq!((preset.id.as_str(), preset.label.as_str(), preset.read_only), ("careful", "谨慎", true));
    assert_eq!(preset.tools.as_deref(), Some(&["read_file".to_owned(), "grep".to_owned()][..]));
    assert_eq!(preset.instructions, "先读再说。");

    // 设置页卡片上写着提供了什么。
    let scripts = JsPlugins::discover(&f.plugins_dir());
    let cx2 = Context::new();
    cx2.add_group(scripts.clone()).unwrap();
    let provides = scripts.card("kit").unwrap().provides;
    assert!(provides.contains(&("skills", 1)) && provides.contains(&("slash", 2)) && provides.contains(&("presets", 1)), "{provides:?}");
}

#[test]
fn bundle_commands_merge_with_script_surfaces() {
    let f = Fixture::new();
    f.plugin("both", "", "export const slash = [{ name: \"hello\", run: () => ({ insert: \"from script\" }) }];\n")
        .file("both/commands/greet.md", "Hi there.\n");
    let cx = f.context();
    let snapshot = cx.snapshot();
    let surfaces: Vec<_> = snapshot.items::<UiSurfaces>().iter().filter(|surface| surface.plugin() == "both").cloned().collect();
    assert_eq!(surfaces.len(), 1, "脚本的界面扩展和包里的命令合成一个 surface");
    let names: Vec<String> = surfaces[0].slash_commands().into_iter().map(|entry| entry.name).collect();
    assert_eq!(names, ["both:greet", "hello"]);
    assert_eq!(surfaces[0].run_command("both:greet", true, "", None).unwrap(), CommandResult::Insert("Hi there.".into()));
    assert_eq!(surfaces[0].run_command("hello", true, "", None).unwrap(), CommandResult::Insert("from script".into()));
}

#[test]
fn bad_bundles_fail_with_reasons() {
    let f = Fixture::new();
    f.file("undeclared/plugin.toml", "[mcp.servers.s]\ncommand = \"node\"\nenv = { T = \"${secret:TOKEN}\" }\n")
        .file("badmode/plugin.toml", "")
        .file("badmode/commands/x.md", "---\nmode: shout\n---\nx\n")
        .file("unknown/plugin.toml", "[mcp.servers.s]\ncommand = \"${home}/x\"\n")
        .file("both/plugin.toml", "[mcp.servers.s]\ncommand = \"x\"\nurl = \"https://example.com/mcp\"\n");
    let cx = f.context();
    assert!(failure(&cx, "undeclared").contains("没有声明"), "{}", failure(&cx, "undeclared"));
    assert!(failure(&cx, "badmode").contains("mode"));
    assert!(failure(&cx, "unknown").contains("不认识"));
    assert!(failure(&cx, "both").contains("只能写一个"));
}

#[tokio::test]
async fn bundled_mcp_servers_need_a_grant_per_launch() {
    let fake = fake_mcp();
    let f = Fixture::new();
    let manifest = "secrets = [\"TOKEN\"]\n[mcp.servers.files]\ncommand = \"node\"\nargs = [\"${plugin}/server.js\", \"--data\", \"${data}\"]\nenv = { TOKEN = \"${secret:TOKEN}\" }\n";
    f.file("srv/plugin.toml", manifest);
    crate::secrets::save("srv", "TOKEN", "s3cret").unwrap();

    // 没允许：不激活，服务器不启动。
    let cx = f.context();
    assert!(matches!(cx.status("srv"), Some(Status::Failed(reason)) if reason.contains("mcp:files@")));

    // 允许之后：工具带插件前缀，占位符换成实际的值。
    f.grant_all();
    let cx = f.context();
    assert_eq!(cx.status("srv"), Some(Status::Active));
    let registry = ToolRegistry::load(&cx.snapshot()).await;
    let ping = registry.get("mcp__srv_files__ping").expect("插件自带服务器的工具");
    assert_eq!(ping.execute(json!({}), &f.tool_context()).await, ToolOutput::ok("pong"));
    let launch = fake
        .launches
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(
            |launch| matches!(launch, dshr_mcp::Launch::Command { args, .. } if args[0].ends_with("server.js") && args[0].contains("srv")),
        )
        .cloned()
        .unwrap();
    let dshr_mcp::Launch::Command { args, env, .. } = launch else { unreachable!() };
    assert_eq!(args[0], f.plugins_dir().join("srv").display().to_string() + "/server.js");
    assert_eq!(args[2], f.home.path().join("plugin-data").join("srv").display().to_string());
    assert_eq!(env["TOKEN"], "s3cret");
    assert!(f.home.path().join("plugin-data/srv").is_dir(), "${{data}} 用到时建好数据目录");

    // 改了参数：要再允许一次（旧授权对不上新的启动参数）。
    f.file("srv/plugin.toml", &manifest.replace("--data", "--root"));
    let cx = f.context();
    assert!(matches!(cx.status("srv"), Some(Status::Failed(reason)) if reason.contains("需要授权")));
}

#[tokio::test]
async fn unchanged_bundled_servers_survive_reloads_and_stop_with_the_plugin() {
    let fake = fake_mcp();
    let f = Fixture::new();
    f.file("keep/plugin.toml", "[mcp.servers.only]\ncommand = \"keep-server\"\n").file("keep/skills/a/SKILL.md", "---\nname: a\n---\n");
    f.grant_all();
    let scripts = JsPlugins::discover(&f.plugins_dir());
    let cx = Context::new();
    cx.add_group(scripts.clone()).unwrap();
    let connects = |fake: &FakeMcp| {
        fake.launches
            .lock()
            .unwrap()
            .iter()
            .filter(|launch| matches!(launch, dshr_mcp::Launch::Command { command, .. } if command == "keep-server"))
            .count()
    };
    let before = connects(&fake);
    assert!(ToolRegistry::load(&cx.snapshot()).await.get("mcp__keep_only__ping").is_some());
    assert_eq!(connects(&fake), before + 1);

    // 只改 Skill：热重载换上新版本，服务器不重启。
    std::thread::sleep(Duration::from_millis(20));
    f.file("keep/skills/a/SKILL.md", "---\nname: a\ndescription: changed\n---\n");
    scripts.reload();
    scripts.reload();
    assert!(ToolRegistry::load(&cx.snapshot()).await.get("mcp__keep_only__ping").is_some());
    assert_eq!(connects(&fake), before + 1);

    // 停用插件：服务器断开，工具消失。
    scripts.set_enabled("keep", false).unwrap();
    assert!(ToolRegistry::load(&cx.snapshot()).await.get("mcp__keep_only__ping").is_none());
    assert!(
        crate::js::bundle_server_status("keep").is_empty()
            || crate::js::bundle_server_status("keep").iter().all(|status| status.state == dshr_mcp::ServerState::Disabled)
    );
    let _ = BTreeMap::<String, String>::new();
}
