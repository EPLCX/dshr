# 架构

这篇面向参与开发的人：dshr 由哪些 crate 组成、它们怎么拼在一起、一轮对话在代码里怎么走、扩展点在哪里。

## 总览

```text
                 ┌──────────────────┐      ┌──────────────────┐
                 │ apps/desktop     │      │ apps/cli         │
                 │ dshr-desktop     │      │ dshr             │
                 └────────┬─────────┘      └────────┬─────────┘
                          │ 装配插件、建运行时       │ 装配插件
                 ┌────────▼─────────┐               │
                 │ dshr-ui (GPUI)   │               │
                 └────────┬─────────┘               │
                          ▼                          ▼
   ┌──────────────────────────────────────────────────────────────────┐
   │ dshr-agent：agent 循环、权限、沙箱插件、压缩、预设、子代理、标题  │
   └──────┬──────────────┬───────────────┬──────────────┬─────────────┘
          ▼              ▼               ▼              ▼
   dshr-context     dshr-core        dshr-llm       dshr-tools ── dshr-sandbox
   插件内核         事件与日志       模型协议       内置工具
          ▲
          │ 都以插件的形式装进同一个 Context
   ┌──────┴──────────┬───────────────┬──────────────────┐
   dshr-mcp          dshr-skills     dshr-plugins (QuickJS)
   MCP 客户端        Skills          JS 插件宿主、安装与分发

   dshr-config（数据目录、config.toml、提供商、钥匙串、账号登录）、dshr-auth（OAuth + PKCE）被各处使用
```

核心思想只有一条：**agent 循环本身不懂权限、沙箱、压缩、标题、预设、子代理、MCP、Skills**。这些都是挂在插件内核上的插件；
模型协议、工具、界面入口也是。内置功能和第三方的 JS 插件用的是同一套扩展点。

## Crate 一览

| crate | 职责 | 主要依赖 |
|---|---|---|
| `dshr-core` | 会话事件模型（`EventBody`）、追加式 JSONL 日志（`SessionLog`，可读 zstd）、从日志到上下文的投影（`derive_messages_with`） | — |
| `dshr-context` | 插件内核：`Plugin`、`Scope`、`Context`、`Snapshot`、服务、扩展点、钩子、事件 | — |
| `dshr-config` | `Paths`（数据目录）、`Config`（`config.toml`）、提供商目录与解析、钥匙串存取、快捷键文件、DeepSeek 账号登录、TLS 后端 | — |
| `dshr-auth` | OAuth 2.0 授权码 + PKCE（回环地址重定向、刷新），MCP 和插件共用 | — |
| `dshr-llm` | `Provider` 抽象、Messages 与 OpenAI Chat Completions 的流式客户端、`MessageAccumulator`、获取模型列表、插件服务商、mock | context、config、core |
| `dshr-sandbox` | 平台沙箱（macOS Seatbelt）与进程树（Windows Job Object、Unix 进程组） | — |
| `dshr-tools` | `Tool` 抽象、`ToolRegistry`、内置工具、`WriteIntent` / `WrapCommand` 钩子 | context、llm、sandbox |
| `dshr-agent` | agent 循环、`Session`、会话目录、审批、权限、沙箱插件、压缩、预设、子代理、标题、提示词扩展、界面扩展点（`UiSurfaces` 等） | 以上全部 |
| `dshr-skills` | Skills 的发现与提示词目录 | agent、context |
| `dshr-mcp` | MCP 客户端：stdio 与 streamable HTTP、服务器生命周期、资源、prompts、OAuth | agent、tools、auth、config |
| `dshr-plugins` | JS 插件宿主（QuickJS，`js` feature）、清单、插件自带的多语言文案（`locale.rs`）、能力授权、插件包、安装、打包签名、索引、来源与回滚、创造模式 | agent、tools、mcp、config |
| `dshr-ui` | GPUI 界面：欢迎窗、工作区窗口、对话视图、设置、安装确认框、托盘、单实例、辅助功能、设计系统 | 以上全部、gpui-kit |
| `dshr-cli` | 命令行（二进制 `dshr`） | — |
| `dshr-desktop` | 桌面版（二进制 `dshr-desktop`） | ui |
| `dshr-rpc`、`dshr-schedule` | 占位：JSON-RPC stdio / Agent Client Protocol 服务、计划任务（cron、提醒）；尚未实现 | — |

工作区的默认成员不含 `dshr-ui` 和 `dshr-desktop`：GPUI 编译很重，`cargo build` / `cargo test` 默认不碰它们。

## 插件内核

`dshr-context` 是整个程序的骨架。它不认识工具、模型这些具体类型，只提供四种“插座”：

| 概念 | 说明 | 例子 |
|---|---|---|
| **服务**（service） | 按类型提供的单例：`cx.provide::<dyn Sandbox>(…)`、`cx.get::<dyn Sandbox>()` | `Sandbox`、`SubagentRunner` |
| **扩展点**（extension point） | 多实例的条目列表：`cx.contribute::<Tools>(…)` | `Tools`、`PromptParts`、`Providers` |
| **钩子**（hook） | 带优先级的有序处理器链：`cx.hook::<GateTool>(优先级, …)` | `GateTool`、`BeforeStep` |
| **事件**（event） | 类型化的广播：`cx.on::<E>(…)`、`snapshot.emit(&e)` | — |

一个插件实现两个方法：

```rust
pub trait Plugin: Send + Sync + 'static {
    fn manifest(&self) -> Manifest;                       // id、必需 / 可选服务、配置的 JSON Schema
    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()>; // 注册它提供的东西
}
```

几条关键规则：

- **原子注册**：`apply` 里的注册先暂存在 `Scope` 里，成功后才一次写进注册表，并记成这个插件的“可撤销项”；`apply` 返回错误时
  这次注册全部作废，插件状态记为 `Failed`。停用插件时按相反顺序撤销。
- **依赖**：`Manifest::requires::<S>()` 声明必需服务，缺了插件处于 `Pending`，服务出现后自动激活、服务撤销时连带停用；
  `optional::<S>()` 声明可选服务，它出现或消失时插件重新激活一次。
- **配置**：`Manifest::config::<C>()` 声明配置类型（字段的文档注释就是设置页上的说明，`schemars` 的约束也会带过去），
  `apply` 里用 `cx.config::<C>()` 取 `[plugins.<id>]` 的值。`enabled = false` 停用插件。
- **不可变快照**：注册表每次变化后发布一份新的 `Snapshot`（`ArcSwap`）。agent 在**每一步开始时取一份**，插件的增删从下一步起生效，
  一步之内看到的状态始终一致。界面通过 `Context::subscribe` 得知快照变了。
- **状态**：`Active`、`Pending(缺的服务)`、`Disabled`、`Failed(原因)`，设置页和 `dshr plugin list` 显示的就是它。

### 扩展点与钩子

扩展点和钩子的标记类型定义在拥有条目类型的 crate 里：

| 扩展点 | 定义在 | 条目 |
|---|---|---|
| `Tools` / `ToolSources` | tools | 工具 / 动态的工具来源（MCP 服务器、插件的动态工具） |
| `Providers` | llm | 线上协议的实现（Messages、OpenAI） |
| `ProviderAuths` | llm | 插件提供的模型服务商 |
| `PromptParts` | agent | 追加到系统提示词后面的内容（运行环境、AGENTS.md、Skills、插件的 `prompt`） |
| `EventKinds` / `EventLabels` / `EventObservers` | agent | 插件事件的投影 / 显示文字 / 订阅者 |
| `PluginPresets` | agent | 插件预设 |
| `UiSurfaces` | agent | 插件的界面：入口、页面、卡片、命令、主题 |
| `MentionSources` | agent | 输入框 `@` 菜单的来源 |
| `SkillRoots` | skills | 插件带的 Skills 文件夹 |
| `ApiProviders`、`Activity` | plugins | 插件之间的 `api`、插件活动 |

| 钩子 | 定义在 | 时机 |
|---|---|---|
| `TurnStarted` | agent | 用户消息写进日志前，可以拦下 |
| `BeforeStep` | agent | 每一步请求前（压缩、运行环境说明、插件钩子） |
| `BeforeRequest` | agent | 同步地修改请求（预设说明、工具名单、插件追加的文字、模型路由） |
| `TransformRequest` | agent | 异步地改写请求 |
| `GateTool` | agent | 工具调用的把关：第一个给出结论的生效（预设 −100、JS 插件 −50、权限 0） |
| `ReviewTool` | agent | 结论是“询问”时的审查（JS 插件 100、自动审查 200） |
| `AfterTool` | agent | 工具执行后，可以改写结果 |
| `TurnEnded` | agent | 一轮结束后（标题、插件钩子） |
| `WriteIntent` | tools | `write_file` / `edit_file` 写盘前 |
| `WrapCommand` | tools | 命令启动前改写它（沙箱） |

## 装配

桌面版（`apps/desktop/src/main.rs`）和命令行版（`apps/cli/src/main.rs`）用几乎一样的顺序装配两个 `Context`：

```text
builtin() =
  configure([plugins.*])            各插件的配置与 enabled
  BuiltinProviders                  provider-messages、provider-openai
  BuiltinTools                      fs、search、shell、plan
  AgentPlugins                      permission、sandbox、compaction、presets、environment
  InstructionsPlugin                AGENTS.md
  SkillsPlugin                      Skills
  McpPlugin（共享实例）              MCP

子代理的 Context = builtin()
主代理的 Context = builtin() + TitlePlugin + DelegatePlugin(子代理的 Context) + CreatorPlugin
两者最后都加上 JsPlugins（共享运行时）
```

- 子代理的 `Context` 里没有 `delegate`，所以子代理不能再委派。
- MCP 服务器和每个 JS 插件的运行时只有一份，两个 `Context` 共用（克隆指向同一个内部状态）。
- **JS 插件最后装**：此前已有的工具名（包括 `delegate` 和创造模式的工具）它们都不能占用。
- 桌面版里 JS 插件要执行脚本（每个最多几秒），不放在启动路径上：窗口出来之后在后台预加载、装进两个 `Context`，再开始盯着插件目录热重载。

## 一轮对话的代码路径

入口是 `Agent::run_turn_with`（`crates/agent/src/lib.rs`）：

1. **`TurnStarted` 钩子**：插件可以拦下这条消息（返回 `AgentError::Blocked`，日志里什么都不写）。
2. 第一次时写 `session/workspace` 和 `system/message`；写 `turn/start`、`user/message`、附件、钩子补的事件。
3. **逐步循环**（不限步数）：
   1. 取插队队列（`SteerQueue`）里的消息接进对话；
   2. 写 `step/start`，取一份插件快照，从 `Tools` 和 `ToolSources` 建 `ToolRegistry`；
   3. **`BeforeStep` 钩子**（压缩、运行环境说明）；投影出上下文；
   4. 拼请求：系统提示词 + `PromptParts`（一轮算一次）；只读的一轮只带只读工具；**`BeforeRequest`**、**`TransformRequest`** 钩子；
   5. 写 `request/header`，流式请求模型，增量以 `LiveEvent::Chunk` 推给界面；可重试的错误写 `assistant/attempt` 后退避重试；
   6. 写 `assistant/message`；没有工具调用就结束这一轮（除非又有插队的消息）；
   7. 工具调用分批：连续的只读调用一批并行（最多 8 个），其余逐个。每个调用写 `tool/call`，经过 **`GateTool` → `ReviewTool` → `Approver`** 把关，
      放行的并行执行，再经 **`AfterTool`**，写 `tool/result` 和工具执行期间写的事件；
   8. 回复被截断时写 `turn/continue` 接着做（最多连续 3 次）；写 `step/end`。
4. 写 `turn/end`；**`TurnEnded` 钩子**（标题等）。

所有持久事件经 `Session::append` 写进日志，同时以 `LiveEvent::Durable` 推给订阅方；界面和命令行只是这些事件的不同渲染方式。
审批通过 `Approver` trait 交给前端：命令行是 `TerminalApprover`，桌面版弹出审批框。

## 模型层

- `Provider::stream(ChatRequest) -> EventStream`：把各家的线上流式协议归一成 `StreamEvent`（文本增量、思考增量、工具调用增量、用量、结束）。
- `MessageAccumulator` 把增量拼成完整的 assistant 消息；收到 `MessageStop` 就结束，不等连接关闭。
- 两种协议各是一个插件，贡献到 `Providers` 扩展点；`connect()` 按提供商配置里的协议找到实现。插件服务商（`ProviderAuths`）在每个请求前提供地址和鉴权，
  协议仍由内置实现处理。
- `HttpClient` 在整个进程里共用一个连接池；连接超时 20 秒、读超时 300 秒。
- `LlmError::is_retryable` 决定是否重试；`retry_after` 读服务端的 `Retry-After`。
- `mock` 模块提供脚本化的假模型，测试用；桌面版的演示模式（`DSHR_PROVIDER=demo`）是界面层另外实现的一个假 Provider。

## 工具层

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    fn access(&self, input: &Value, ctx: &ToolContext) -> Access { Access::Read }  // Read / Write(path) / Execute / External
    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput;
}
```

- `ToolContext` 带着工作区、实时输出通道（`OutputSink`）、事件通道（`EventSink`）、本轮选项（`Options`：权限模式、预设、调用环境……）和这一步的插件快照。
- `Access` 是权限把关的唯一依据：工具自己声明这次调用会做什么，权限插件据此决定。
- `run_shell` 启动前经过 `WrapCommand` 钩子（沙箱插件在“工作区内修改”模式下把命令包进 `sandbox-exec`），由 `dshr-sandbox` 管理进程树。

## JS 插件宿主

`dshr-plugins` 把插件目录里的每个插件变成一个普通的 `Plugin`：

- **`JsPlugins`**：扫描插件目录和链接（`discover`）、预加载、盯着文件热重载（每 2 秒比较一次文件指纹，变了就 `Context::replace`）、授权、卡片信息、日志。
- **`JsPlugin`**：一个插件一条线程、一个 QuickJS 运行时（`rquickjs`）。启动时先执行 `globals.js`、`prelude.js`，再加载插件模块；`register` 把导出项
  登记成工具、钩子、扩展点条目。宿主调用通过消息排队，同一插件一次只执行一个调用。
- **`host` 原生函数**（`js/host.rs` 等）：每个调用带一份 `Permits`（能不能写、联网、启动程序、调模型、代办），由“清单能力 × 调用场合”算出，脚本无法绕过。
  慢操作（`fetch`、`exec`、`llm`）交给 IO 运行时，完成后经 `settle` 交回脚本，不占插件线程。
- **授权**（`grants.rs`）、**清单**（`manifest.rs`）、**插件包**（`bundle.rs`）、**安装**（`install.rs`）、**打包签名**（`pack.rs`）、**索引**（`index.rs`）、
  **来源与回滚**（`sources.rs`）、**链接**（`links.rs`）、**密钥**（`secrets.rs`）。
- **`CreatorPlugin`**：创造模式的六个工具、写法指南（`js/creator.md` 编进程序，作为系统提示词追加）、编进程序的插件文档（`plugin_docs` 检索 `docs/` 里的相关文件），以及让 agent 不跑偏的把关和调查次数上限。
- **主程序服务**（`app.rs`）：插件替用户发消息、写输入框、发通知、安装前确认、打开浏览器等需要主程序配合的能力，由桌面版或命令行版在启动时
  `set_app_services` 填一次；没填的项在插件里调用时报“这个程序不支持”。

没有 `js` feature 的构建里 `JsPlugins` 是空实现，插件目录里的东西不加载。

## 界面层

`dshr-ui` 基于 GPUI（`gpui-kit` 组件库）：

- **`Backend`**：界面的后端一半。持有 tokio 运行时（2 个工作线程，名为 `dshr-agent`）、配置、密钥缓存、插件 `Context`、MCP 服务器、HTTP 客户端，
  负责开始对话、保存设置等。**界面线程上不扫文件、不执行插件脚本、不联网、不读钥匙串**：这些都交给后台，界面先按用户的操作更新，做完再对账。
- **视图**：欢迎窗（`welcome.rs`）、工作区窗口（`lib.rs`：侧边栏、标题栏、会话视图的切换）、对话视图（`conversation.rs` + `transcript.rs`）、
  设置弹窗（`settings.rs`、`provider_form.rs`、`schema_form.rs`）、插件页面（`plugin_page.rs`）、安装确认框（`install_dialog.rs`）、托盘（`tray.rs`）、
  单实例（`single_instance.rs`）。
- **桥**（`bridge.rs`）：插件在自己的线程上请求界面做事（发消息、写输入框、确认安装），请求排进队列并唤醒窗口，由主线程取出处理。
- **设计系统**：`design.rs`（token、基础组件、焦点轮廓、配色）和 `design/motion.rs`（动效），规则见 [设计规范](design.md)。
- **本地化**：`rust-i18n`，文案在 `crates/ui/locales/app.yml`，十种语言；切换语言时同时告诉 `dshr_plugins::locale`，插件的 `host.i18n` 和设置页里插件配置项的标题跟着变。
- **辅助功能**：通过 AccessKit 暴露给读屏软件；`system_a11y.rs` 读系统的高对比度、文字大小、动画设置；`touch.rs` 处理 Windows 触摸手势。

### UI 插件

GPUI 的 `App`、`Window`、`Entity` 只能在主线程用，不能和后端的 `Context` 放在一起，所以一个插件分两半、按 id 对应：后端那一半（`Plugin`）
注册工具、钩子；界面这一半（`UiPlugin`，`crates/ui/src/plugin.rs`）注册这些东西怎么显示。后端那一半没有激活时，界面这一半也不装。

| 界面扩展点 | 内容 | 内置的使用者 |
|---|---|---|
| `ToolCard` | 某个工具的调用在对话区怎么显示 | `fs`（改动行数与 diff）、`plan`（不显示成卡片） |
| `TranscriptRenderer` | 插件事件在对话区占的一行 | — |
| `ComposerPanel` | 输入卡片上方的面板 | `plan`（计划面板） |
| `ComposerControl` | 输入卡片底部的菜单 | `presets`、`permission` |
| `SettingsPage` | 设置弹窗的一页 | 通用设置、辅助功能、模型、扩展 |
| `SidebarItem` | 侧边栏底部的入口 | — |
| `StatusItem` | 侧边栏底部的状态行 | 账号状态 |

界面里原来按工具名、按页面写死的分支都改成了查这张表。JS 插件的界面（`ui`、`cards`、`events.render`）走的是另一条路：插件返回声明式的节点树，
由 `plugin_page.rs` 用同一套组件画出来（节点的校验在 `dshr_agent::validate_tree`）。

## 线程与运行时

| 线程 | 做什么 |
|---|---|
| 主线程（桌面版） | GPUI 事件循环、所有视图 |
| `dshr-agent` tokio 运行时 | agent 循环、模型请求、MCP 客户端、插件的慢操作 |
| 每个 JS 插件一条线程 | QuickJS 运行时；有常驻服务的插件另有一个服务运行时 |
| MCP stdio 服务器 | 独立的子进程 |
| 命令行版 | `#[tokio::main]` 多线程运行时 |

## 平台相关

- **Windows**：`vendor/gpui-pre-windows` 是打过补丁的 GPUI Windows 后端（修复调整窗口大小时的内存膨胀，见其中的 `PATCHED.md`），
  通过根 `Cargo.toml` 的 `[patch.crates-io]` 使用；`apps/desktop/build.rs` 把图标编进可执行文件；命令放进 Job Object；子进程不弹控制台窗口。
- **macOS**：命令沙箱用 `sandbox-exec`；`scripts/bundle-macos.sh` 打通用二进制、`.app` 和 `.dmg`。
- **Linux**：命令行版可用；桌面版使用系统标题栏，不是正式支持的平台。

## 发布版构建配置

根 `Cargo.toml` 的 `[profile.release]`：`lto = "thin"`、`codegen-units = 1`、`strip = true`、`panic = "abort"`，整体按体积优化（`opt-level = "s"`），
热路径上的 crate 单独按速度优化（`opt-level = 3`）：解析会话日志与流式响应（`serde_json`、`dshr-core`）、grep / diff / Markdown，以及界面每一帧都要跑的代码
（`dshr-ui`、GPUI、布局、文本换行、各平台渲染后端）。
