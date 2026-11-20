# DSHR

**DSHR** 是 DeepSeek Harness 的 Rust 原生分支版（**非官方**）：一个在你自己电脑上工作的 AI 编程代理，提供命令行版 DSHR 和桌面版 DSHR Desktop。
它在你选定的工作区里读代码、改文件、跑命令，每一步都看得见，会改东西的操作按你选的权限模式先问你。

- **原生、轻量**：界面基于 GPU 渲染的 GPUI，不用 Electron / WebView；支持 Windows 与 macOS。
- **桌面版 + 命令行**：桌面版用于日常对话；`dshr headless` 适合脚本和自动化。
- **多家模型**：内置 DeepSeek、Anthropic、OpenAI、Kimi (Moonshot)、OpenRouter、Ollama，
  也可以接入任意 Messages 兼容或 OpenAI 兼容的接口。
- **可扩展**：AGENTS.md 项目说明、Skills、MCP 服务器，以及跑在沙箱化 JS 引擎里的插件。

> 当前版本 0.1.0，界面上标为“预览版”。信息结构与交互流程参照原版 DeepSeek Harness，视觉按
> [设计规范](docs/design.md) 自行实现；不包含原版的代码或资源。

## 功能一览

| 方面 | 内容 |
|---|---|
| Agent | 多步工具调用；只读调用并行执行；请求失败自动重试；回复被截断时自动续写；上下文快满时自动压缩；第一轮结束后自动起标题 |
| 内置工具 | `read_file`、`write_file`、`edit_file`、`list_dir`、`grep`、`glob`、`run_shell`、`update_plan`（计划面板）、`delegate`（子代理） |
| 预设 | 标准、规划（只读出方案）、审查（按严重程度列问题）、创造（agent 为你写一个插件） |
| 权限 | 手动确认、工作区内修改（默认）、自动审查（由模型判断风险）、完全访问；“工作区内修改”模式下，macOS 上的命令在 Seatbelt 沙箱里运行 |
| 对话 | 附件（文本、图片、粘贴截图）、`@` 提及文件、`/` 命令、运行中排队或“插队”、从任意一轮分叉、导出为 Markdown |
| 模型 | 多提供商与自定义接口、获取可用模型、思考等级（off / low / high / max）、Anthropic 提示词缓存、DeepSeek 账号登录 |
| 扩展 | `AGENTS.md`（全局 + 项目）、Skills（兼容 `.agents/skills`、`.claude/skills`）、MCP（stdio 与 streamable HTTP，含 OAuth 登录） |
| 插件 | QuickJS 里运行的 JS 插件：工具、钩子、界面页面、模型服务商、配色方案、后台任务……按能力逐项授权；支持热重载、打包、签名、索引与回滚 |
| 界面 | 10 种界面语言；浅色 / 深色 / 跟随系统；系统托盘；完整的键盘操作与读屏支持、高对比度（WCAG AAA）、文字缩放、减少动态效果 |
| 数据 | 会话是只追加的 JSONL 日志（与原版 `session.v1.jsonl` 同格式，可读 `.zstd` 压缩日志）；API Key 存在系统钥匙串 |

## 快速开始

需要 Rust 1.95 或更新版本（edition 2024）。Windows 用 MSVC 工具链；macOS 需要 Xcode（GPUI 用到 Metal）。

```bash
# 运行测试（默认成员不含桌面版，编译较快）
cargo test

# 桌面版：首次编译较慢
cargo run --release -p dshr-desktop

# 直接以某个目录为工作区打开
cargo run --release -p dshr-desktop -- <目录>
```

首次启动会出现欢迎窗：用 DeepSeek 账号登录，或填入 DeepSeek API 密钥（保存在系统钥匙串里），也可以点“稍后设置”，
之后在“设置 → 模型”里添加其他提供商。没有 API 密钥时，可以用内置的演示模型看看界面（不联网）：

```bash
DSHR_PROVIDER=demo cargo run -p dshr-desktop
```

命令行版：

```bash
cargo build --release -p dshr-cli            # 生成 target/release/dshr

DEEPSEEK_API_KEY=sk-... target/release/dshr headless "列出当前目录的文件"
target/release/dshr headless --model deepseek/deepseek-v4-pro --preset plan "这个仓库的构建流程是怎样的？"
target/release/dshr sessions                  # 列出本地会话
```

`headless` 默认用“工作区内修改”模式，需要确认的操作会在终端里逐条询问；`--yes` 全部放行，`--manual`、`--auto-review`
切换其他模式，`--resume <id>` 继续已有会话。完整说明见 [快速上手](docs/getting-started.md) 和 [命令行参考](docs/cli.md)。

## 配置

数据目录默认是 `~/.dshr`（可用环境变量 `DSHR_HOME` 改），与原版的 `~/.dsh` 互不影响。大部分设置可以在桌面版的设置弹窗里改，
保存在 `~/.dshr/config.toml`；API 密钥、插件密钥和登录令牌只存在系统钥匙串（Windows 凭据管理器 / macOS 钥匙串）里。

```toml
[model]
default = "deepseek/deepseek-flash"   # 新会话的默认模型：provider/model
effort = "high"                       # 默认思考等级：off / low / high / max；不写由模型决定
context_window = 128000               # 上下文窗口，用量到 75% 时压缩较早的对话

[ui]
theme = "system"                      # system / light / dark
language = "zh-CN"                    # zh-CN、zh-HK、zh-TW、en、de、el、ja、es、fr、pt

[[providers]]
id = "deepseek"
adapter = "deepseek"                  # 地址、环境变量、默认模型都用内置值

[[providers]]
id = "company-gateway"
adapter = "custom"
name = "公司网关"
protocol = "openai"                   # openai / messages
base_url = "https://gateway.example.com/v1"
models = [{ id = "gpt-x", name = "GPT X" }]

[plugins.mcp.servers.files]           # 一个 stdio 的 MCP 服务器
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "D:/docs"]
```

全部配置项、环境变量和数据目录结构见 [配置参考](docs/configuration.md)。

## 文档

| 文档 | 内容 |
|---|---|
| [文档首页](docs/README.md) | 按读者分类的完整目录 |
| [快速上手](docs/getting-started.md) | 构建、首次启动、配置模型、第一次对话 |
| [桌面版使用指南](docs/desktop.md) | 工作区与会话、输入卡片、审批、排队与插队、分叉、设置各页、快捷键与辅助功能 |
| [命令行参考](docs/cli.md) | `dshr headless`、`sessions`、`plugin`、`mcp` 的全部子命令与参数 |
| [配置参考](docs/configuration.md) | 数据目录、`config.toml`、环境变量、快捷键文件、配色方案、钥匙串条目 |
| [模型与提供商](docs/providers.md) | 内置提供商、自定义接口、账号登录、思考等级、提示词缓存 |
| [Agent 的工作方式](docs/agent.md) | 轮次与步骤、内置工具、预设、子代理、上下文压缩、重试 |
| [项目说明与 Skills](docs/instructions-and-skills.md) | `AGENTS.md` 与 Skills 的写法和查找顺序 |
| [权限与安全](docs/permissions.md) | 权限模式、审批、自动审查、沙箱、插件能力、密钥存放 |
| [MCP 服务器](docs/mcp.md) | stdio / HTTP 服务器、占位符与密钥、OAuth 登录、资源与 prompts |
| [插件开发指南](docs/plugins.md) · [API 参考](docs/plugin-api.md) · [打包与分发](docs/plugin-publishing.md) | 编写、调试、发布 JS 插件 |
| [会话与日志格式](docs/sessions.md) | 会话目录、事件类型、投影、分叉 |
| [架构](docs/architecture.md) · [开发与贡献](docs/development.md) · [设计规范](docs/design.md) | 给参与开发的人 |
| [常见问题与排错](docs/troubleshooting.md) | 连不上模型、钥匙串、插件不激活、MCP 连接失败等 |

另有一组独立于构建的静态介绍页面，在 [`site/`](site/) 目录，直接用浏览器打开 `site/index.html` 即可。

## 项目结构

```text
apps/
  desktop/          桌面版主程序（dshr-desktop）
  cli/              命令行（dshr）
crates/
  core/             会话事件模型、追加式 JSONL 日志、模型上下文投影
  context/          插件内核：服务、扩展点、钩子、不可变快照
  llm/              Messages 与 OpenAI Chat Completions 的流式客户端
  tools/            内置工具与写入 / 命令钩子
  agent/            agent 循环、权限、预设、上下文压缩、子代理、标题
  config/           数据目录、config.toml、提供商目录、钥匙串、DeepSeek 账号登录
  auth/             OAuth 2.0 授权码 + PKCE（MCP 与插件共用）
  sandbox/          命令沙箱（macOS Seatbelt）与进程树管理
  mcp/              MCP 客户端：stdio / streamable HTTP、资源、prompts、OAuth
  skills/           Skills 的发现与提示词目录
  plugins/          JS 插件宿主（QuickJS）、安装、打包、签名、索引
  ui/               GPUI 界面：工作区窗口、对话区、设置、托盘、辅助功能
  rpc/ schedule/    占位：JSON-RPC / ACP 服务、计划任务（尚未实现）
vendor/
  gpui-pre-windows/ 打过补丁的 GPUI Windows 后端（见其中的 PATCHED.md）
docs/               文档
site/               静态介绍页面（不参与构建）
scripts/            macOS 打包、生成第三方声明
assets/             应用图标与托盘图标
```

各 crate 之间的关系见 [架构](docs/architecture.md)。

## 参与开发

```bash
cargo test                     # 默认成员：除桌面版和 UI 之外的全部 crate
cargo test -p dshr-ui          # 界面的单元测试（要编译 GPUI）
cargo clippy --workspace
```

改界面之前先读 [设计规范](docs/design.md)，它的唯一实现是 `crates/ui/src/design.rs`。构建、测试、打包、本地化与第三方声明的
完整流程见 [开发与贡献](docs/development.md)。

## 许可

MIT，见 [LICENSE](LICENSE)。程序使用的第三方组件（Rust 依赖、内嵌的 QuickJS-ng / Zstandard / AWS-LC、Lucide 图标等）
及其许可证全文统一列在 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)，分发程序时一并附上；“设置 → 关于”里也能查看。
该文件由脚本生成，依赖变化（`Cargo.lock` 改动）后重新运行并提交：

```bash
pwsh scripts/third-party-notices.ps1          # 重新生成
pwsh scripts/third-party-notices.ps1 -Check   # 只检查是否最新（过期时退出码为 1，可用于 CI）
```

原版 DeepSeek Harness 同为 MIT。本项目目前只参照其信息结构，不包含它的代码或资源；今后复用时会在声明文件里列出并附上原版 LICENSE。
