//! `dshr plugin dev [<文件夹或 id>]`：插件调试台。
//!
//! 和 `headless` 一样装配插件（别的插件照常加载），盯着这个插件的文件：每次改动后重新加载并打印报告（加载结果、
//! 工具、等授权的能力、页面预览、最近的日志）。交互命令可以直接调用工具、画页面、执行页面动作。不需要和桌面版通信：
//! 桌面版自己盯着同一个文件夹（链接或插件目录），保存后两边都会重载。

use std::path::Path;
use std::time::Duration;

use dshr_config::{Config, Paths};
use dshr_plugins::JsPlugins;
use dshr_tools::{Access, ToolContext, ToolRegistry};
use serde_json::Value;

/// 多久看一次文件有没有变。
const POLL: Duration = Duration::from_millis(800);

const HELP: &str = "命令：
  tool <名字> [JSON]          调用一个工具（写文件、联网、启动程序之前会问你）
  page <页> [<动作> [JSON]]   画一页，或在这一页上执行一个动作再画
  grant                       允许清单里声明的能力
  logs                        最近的日志
  reload                      马上重新加载并打印报告
  help                        这段说明
  quit                        退出";

pub async fn run(paths: &Paths, target: Option<String>) -> anyhow::Result<()> {
    let plugins_dir = paths.plugins_dir();
    let id = resolve_target(&plugins_dir, target)?;
    let config = Config::load(paths)?;
    let (plugins, scripts) = super::plugins_with_scripts(&config, paths)?;
    let workspace = std::env::current_dir()?;
    println!("调试插件 {id}（工作区：{}）。输入 help 看命令。\n", workspace.display());
    print_report(&scripts, &id);

    let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if lines_tx.send(line.trim().to_owned()).is_err() {
                        return;
                    }
                }
            }
        }
    });
    let mut poll = tokio::time::interval(POLL);
    loop {
        prompt();
        tokio::select! {
            _ = poll.tick() => {
                let report = scripts.reload();
                if report.is_empty() {
                    continue;
                }
                println!("\n— 文件变了，重新加载 —");
                print_report(&scripts, &id);
            }
            line = lines.recv() => {
                let Some(line) = line else { return Ok(()) };
                let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line.as_str(), ""));
                let rest = rest.trim();
                match command {
                    "" => {}
                    "quit" | "exit" | "q" => return Ok(()),
                    "help" | "?" => println!("{HELP}"),
                    "reload" => {
                        scripts.reload();
                        print_report(&scripts, &id);
                    }
                    "grant" => match scripts.grant(&id) {
                        Ok(_) => {
                            println!("已允许");
                            print_report(&scripts, &id);
                        }
                        Err(error) => println!("没能允许：{error}"),
                    },
                    "logs" => {
                        for line in scripts.logs(&id) {
                            println!("[{}] {}", line.level, line.text);
                        }
                    }
                    "tool" => call_tool(&plugins, &workspace, rest, &mut lines).await,
                    "page" => page(&scripts, &id, &workspace, rest),
                    other => println!("不认识的命令 {other}。{HELP}"),
                }
            }
        }
    }
}

fn prompt() {
    use std::io::Write as _;
    print!("dev> ");
    let _ = std::io::stdout().flush();
}

/// 参数是文件夹（没链接过就先链接）、插件 id，或者不写（当前目录）。
fn resolve_target(plugins_dir: &Path, target: Option<String>) -> anyhow::Result<String> {
    let target = target.unwrap_or_else(|| ".".into());
    let path = Path::new(&target);
    if path.is_dir() && (path.join(dshr_plugins::MANIFEST_FILE).is_file() || path.join("index.js").is_file()) {
        let folder = std::path::absolute(path)?;
        // 已经在插件目录里，或者已经链接过：直接用。
        if let Some(found) = dshr_plugins::discover(plugins_dir).into_iter().find(|found| found.dir == folder) {
            return Ok(found.id);
        }
        let id = dshr_plugins::links::link(plugins_dir, &folder, None).map_err(anyhow::Error::msg)?;
        println!("已链接 {id} → {}（`dshr plugin unlink {id}` 取消）", folder.display());
        return Ok(id);
    }
    if dshr_plugins::discover(plugins_dir).iter().any(|found| found.id == target) {
        return Ok(target);
    }
    anyhow::bail!("{target} 既不是插件文件夹，也不是插件目录里的插件 id")
}

fn print_report(scripts: &JsPlugins, id: &str) {
    match scripts.describe(id) {
        Some(text) => println!("{text}\n"),
        None => println!("插件 {id} 没有装上（清单里的 id 对不上，或者和内置插件重名）\n"),
    }
}

async fn call_tool(
    plugins: &dshr_context::Context,
    workspace: &Path,
    rest: &str,
    lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let (name, input) = rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
    if name.is_empty() {
        println!("用法：tool <名字> [JSON]");
        return;
    }
    let input: Value = match serde_json::from_str(input.trim()) {
        Ok(input) => input,
        Err(error) => {
            println!("参数不是 JSON：{error}");
            return;
        }
    };
    let snapshot = plugins.snapshot();
    let Some(tool) = ToolRegistry::load(&snapshot).await.get(name) else {
        println!("没有工具 {name}");
        return;
    };
    let mut ctx = ToolContext::new(workspace.to_owned());
    ctx.plugins = snapshot.clone();
    ctx.options.insert(dshr_agent::PermissionMode::Workspace);
    ctx.output = Some(dshr_tools::OutputSink(std::sync::Arc::new(|text: &str| print!("{text}"))));
    ctx.events = Some(dshr_tools::EventSink(std::sync::Arc::new(|kind: String, data: Value| println!("[事件 {kind}] {data}"))));
    if tool.access(&input, &ctx) != Access::Read {
        use std::io::Write as _;
        print!("{name} 会读工作区外的文件、写文件、联网或启动程序，执行？[y/N] ");
        let _ = std::io::stdout().flush();
        let answer = lines.recv().await.unwrap_or_default();
        if !matches!(answer.as_str(), "y" | "Y" | "yes") {
            println!("没有执行");
            return;
        }
    }
    let output = tool.execute(input, &ctx).await;
    println!("{}{}", if output.is_error { "[出错] " } else { "" }, output.content);
}

fn page(scripts: &JsPlugins, id: &str, workspace: &Path, rest: &str) {
    let mut parts = rest.splitn(3, char::is_whitespace);
    let (Some(page), action, payload) = (parts.next().filter(|page| !page.is_empty()), parts.next(), parts.next()) else {
        println!("用法：page <页> [<动作> [JSON]]");
        return;
    };
    let Some(surface) = scripts.surface(id) else {
        println!("{id} 没有界面扩展（或者还没激活）");
        return;
    };
    if let Some(action) = action {
        let payload: Value = match payload.map(serde_json::from_str).transpose() {
            Ok(payload) => payload.unwrap_or(Value::Null),
            Err(error) => {
                println!("参数不是 JSON：{error}");
                return;
            }
        };
        match surface.act(page, action, payload, Some(workspace)) {
            Ok(Some(message)) => println!("动作返回：{message}"),
            Ok(None) => println!("动作完成"),
            Err(error) => println!("动作出错：{error}"),
        }
    }
    match surface.render(page, Some(workspace)) {
        Ok(tree) => println!("{}", serde_json::to_string_pretty(&tree).unwrap_or_default()),
        Err(error) => println!("画不出来：{error}"),
    }
}
