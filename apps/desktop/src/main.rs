// 发布版不弹出控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use dshr_agent::{AgentPlugins, DelegatePlugin, InstructionsPlugin, TitlePlugin};
use dshr_config::{Config, Paths};
use dshr_context::{Context, ContextError};
use dshr_mcp::McpPlugin;
use dshr_plugins::{CreatorPlugin, JsPlugins};
use dshr_skills::SkillsPlugin;
use dshr_ui::Backend;

/// 桌面版装配的插件，以及之后在后台装 JS 插件要用的东西。
struct Assembled {
    /// 主代理的插件。
    plugins: Context,
    /// 子代理的插件（没有 delegate）。
    subagent: Context,
    scripts: JsPlugins,
    mcp: McpPlugin,
    configs: Vec<(String, serde_json::Value)>,
}

/// 内置插件现在就装好；插件目录里的 JS 插件要执行脚本（每个最多几秒），不放在这里，
/// 由 [`attach_scripts`] 在窗口出来之后到后台去装。
fn plugins(paths: &Paths, config: &Config) -> Result<Assembled, String> {
    let configs = config.plugin_configs();
    // MCP 服务器只启动一份，主代理和子代理共用。
    let mcp = McpPlugin::new();
    // 插件目录里的 JS 插件：每个只加载一次，主代理和子代理共用同一个运行时。这里只读清单，不执行脚本。
    let scripts = JsPlugins::discover(&paths.plugins_dir());
    // 主代理和子代理共用的内置部分。
    let builtin = || -> Result<Context, ContextError> {
        let plugins = Context::new();
        // `[plugins.<id>]`：各插件的配置，以及 `enabled = false`。改了要重启才生效。
        plugins.configure(configs.clone());
        plugins.add_group(dshr_llm::BuiltinProviders)?;
        plugins.add_group(dshr_tools::BuiltinTools)?;
        plugins.add_group(AgentPlugins)?;
        plugins.add(InstructionsPlugin::new(&paths.home))?;
        plugins.add(SkillsPlugin::new(&paths.home))?;
        plugins.add(mcp.clone())?;
        Ok(plugins)
    };
    let assemble = || -> Result<(Context, Context), ContextError> {
        // 子代理的插件里没有 delegate，所以不能再委派。
        let subagent = builtin()?;
        let plugins = builtin()?;
        plugins.add(TitlePlugin)?;
        plugins.add(DelegatePlugin::new(subagent.clone()))?;
        // 创造模式：agent 自己写插件。它的工具在 JS 插件之前装上，脚本不能占用这些名字。
        plugins.add(CreatorPlugin::new(scripts.clone()))?;
        Ok((plugins, subagent))
    };
    let (plugins, subagent) = assemble().map_err(|error| error.to_string())?;
    Ok(Assembled { plugins, subagent, scripts, mcp, configs })
}

/// 在后台装上 JS 插件：各插件同时加载，装进子代理和主代理（放在最后：此前已有的工具名，含 delegate，
/// 它们都不能占用），再盯着插件目录热重载。装好后插件容器发布新快照，界面收到通知刷新入口和状态行。
fn attach_scripts(assembled: &Assembled) -> impl FnOnce() + Send + 'static {
    let (plugins, subagent, scripts) = (assembled.plugins.clone(), assembled.subagent.clone(), assembled.scripts.clone());
    let configs = assembled.configs.clone();
    move || {
        scripts.preload(&configs);
        for cx in [&subagent, &plugins] {
            if let Err(error) = cx.add_group(scripts.clone()) {
                tracing::error!(%error, "JS 插件没能装上");
            }
        }
        // 盯着插件目录：改了脚本、加了或删了插件，不用重启，从下一步起生效。
        scripts.watch().detach();
    }
}

/// 发布版没有控制台：panic 的信息另外追加写进 `<数据目录>/logs/crash.log`，排查时有据可查。
/// 默认的处理（打印到 stderr）照旧保留。
fn install_crash_log(paths: &Paths) {
    let file = paths.home.join("logs").join("crash.log");
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let thread = std::thread::current();
        let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs());
        let backtrace = std::backtrace::Backtrace::force_capture();
        let entry = format!("[{time}] {} {} panicked: {info}\n{backtrace}\n\n", dshr_ui::APP_NAME, thread.name().unwrap_or("<unnamed>"),);
        // 写不了日志也不能在 panic 处理里再出错。
        let _ = std::fs::create_dir_all(file.parent().unwrap_or(&file)).and_then(|()| {
            use std::io::Write as _;
            std::fs::OpenOptions::new().create(true).append(true).open(&file)?.write_all(entry.as_bytes())
        });
    }));
}

fn main() {
    dshr_ui::startup::init();
    dshr_ui::startup::mark("main: 进入");
    // TLS 统一用 aws-lc-rs 后端，装成进程级默认值（`ureq`、`wss://` 都要它）。第一个 TLS 连接才用到，
    // 初始化要编译算法实现、可能几十毫秒，放后台做，不挡开窗口。
    std::thread::spawn(dshr_config::tls::install_crypto_provider);
    // `dshr-desktop <目录>`：直接以该目录为工作区打开。
    // 不用 canonicalize：Windows 上会得到 `\\?\` 前缀路径，cmd 不接受它作为工作目录。
    let workspace = std::env::args_os().nth(1).and_then(|path| std::path::absolute(path).ok()).filter(|path| path.is_dir());
    let paths = match Paths::from_env() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("{} 启动失败：{error}", dshr_ui::APP_NAME);
            std::process::exit(1);
        }
    };
    install_crash_log(&paths);
    // 已经有一个在运行：把它的窗口调到前台，这次不再打开新的。
    let Some(instance) = dshr_ui::single_instance::acquire(&paths.home) else { return };
    dshr_ui::startup::mark("main: single_instance 完");
    // 后台运行时先建好（窗口出来之前不依赖界面线程），配置只读一次，装配插件和界面后端共用同一份。
    let runtime = match Backend::make_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{} 启动失败：{error}", dshr_ui::APP_NAME);
            std::process::exit(1);
        }
    };
    let config = match Config::load(&paths) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{} 启动失败：{error}", dshr_ui::APP_NAME);
            std::process::exit(1);
        }
    };
    let assembled = match plugins(&paths, &config) {
        Ok(assembled) => assembled,
        Err(error) => {
            eprintln!("{} 启动失败：{error}", dshr_ui::APP_NAME);
            std::process::exit(1);
        }
    };
    let backend = Backend::from_parts(assembled.plugins.clone(), paths, config, runtime)
        .with_scripts(assembled.scripts.clone())
        .with_mcp(assembled.mcp.clone());
    let backend = Arc::new(backend);
    dshr_ui::startup::mark("main: 插件与后端装配完");
    // MCP 服务器在后台先启动：第一条消息不用等它们握手、列工具。
    let mcp = assembled.mcp.clone();
    backend.runtime().spawn(async move { mcp.prewarm().await });
    // 配了插件索引时：启动一分钟后查一次更新，之后每天一次（只取索引文件，不带任何用户信息）。
    let updates = backend.clone();
    backend.runtime().spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            updates.check_plugin_updates().await;
            tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
        }
    });
    // JS 插件也在后台加载（在运行时的阻塞线程池里：插件的后台任务有运行时可用），窗口不等它们。
    backend.runtime().spawn_blocking(attach_scripts(&assembled));

    dshr_ui::startup::mark("main: 后台任务已派发，建 GPUI 应用");
    // 组件库的默认图标加上我们另外用到的几个（见 `dshr_ui::AppAssets`）。
    let app = gpui_kit::application().with_assets(dshr_ui::AppAssets);
    // macOS 点击程序坞图标时恢复隐藏到菜单栏的窗口。
    app.on_reopen(dshr_ui::show_main_window);
    dshr_ui::startup::mark("main: application() 完，进 run");
    app.run(move |cx| {
        dshr_ui::startup::mark("gpui: run 回调开始");
        gpui_kit::init(cx);
        dshr_ui::startup::mark("gpui: init 完");
        // 界面这一半的插件和后端的一样显式装配；后端那一半停用的不会装上。
        dshr_ui::launch(backend, workspace, dshr_ui::builtin_ui_plugins(), cx);
        dshr_ui::startup::mark("gpui: launch 完（窗口已请求打开）");
        if let Some(instance) = instance {
            instance.serve(cx, dshr_ui::show_main_window);
        }
    });
}
