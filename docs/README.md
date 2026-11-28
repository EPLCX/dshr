# DSHR 文档

DSHR 是 DeepSeek Harness 的 Rust 原生分支版（非官方）：一个在你自己电脑上工作的 AI 编程代理，提供命令行版 DSHR 和桌面版 DSHR Desktop。
这里是它的完整文档，按读者分成三组。第一次使用的话，从 [快速上手](getting-started.md) 开始。

## 使用 DSHR

| 文档 | 读完能做什么 |
|---|---|
| [快速上手](getting-started.md) | 从源码构建，首次启动，配好模型，完成第一次对话 |
| [桌面版使用指南](desktop.md) | 用好工作区与会话、输入卡片（附件、`@`、`/`、预设、权限、模型）、审批、排队与插队、分叉与导出、设置各页 |
| [命令行参考](cli.md) | 在终端和脚本里用 `dshr headless`，管理会话、插件与 MCP 服务器 |
| [模型与提供商](providers.md) | 接入 DeepSeek、Anthropic、OpenAI、Kimi、OpenRouter、Ollama 或自己的网关；设置思考等级与提示词缓存 |
| [Agent 的工作方式](agent.md) | 理解一轮对话里发生了什么：工具、预设、子代理、上下文压缩、重试 |
| [项目说明与 Skills](instructions-and-skills.md) | 用 `AGENTS.md` 交代项目约定，用 Skills 教 agent 做特定的事 |
| [权限与安全](permissions.md) | 选对权限模式，看懂审批，知道沙箱、插件授权和密钥各管什么 |
| [MCP 服务器](mcp.md) | 接入 MCP 服务器的工具、资源和 prompts，包括需要登录的远程服务器 |
| [配置参考](configuration.md) | 查数据目录结构、`config.toml` 的每一项、环境变量、快捷键与配色方案文件 |
| [常见问题与排错](troubleshooting.md) | 连不上模型、钥匙串不可用、插件不激活、MCP 连接失败时怎么办 |

## 扩展 DSHR

| 文档 | 内容 |
|---|---|
| [插件开发指南](plugins.md) | 从零写一个 JS 插件：清单、能力、工具、钩子、事件、界面扩展、插件包、调试 |
| [插件 API 参考](plugin-api.md) | `host` 的全部成员、全部导出项、界面节点、能力清单、限额 |
| [插件的打包、签名与分发](plugin-publishing.md) | `dshr plugin pack`、签名密钥、插件索引、安装确认、更新与回滚 |

## 参与开发

| 文档 | 内容 |
|---|---|
| [架构](architecture.md) | crate 地图、插件内核、agent 循环、事件日志、模型层、界面层 |
| [会话与日志格式](sessions.md) | 会话目录、事件类型、从日志到模型上下文的投影、分叉与删除 |
| [开发与贡献](development.md) | 构建、测试、演示模式、本地化、第三方声明、补丁版依赖、打包 |
| [协作规范](contributing.md) | 分支、Issue 与 PR 的写法、版本号规则、Release 流程 |
| [设计规范](design.md) | 界面的 token、组件、页面结构、辅助功能与动效规则 |

## 约定

- 文中的 `~/.dshr` 指数据目录，可以用环境变量 `DSHR_HOME` 改到别处（见 [配置参考](configuration.md#数据目录)）。
- 快捷键按 Windows 写（`Ctrl`）；在 macOS 上对应 `Cmd`。
- 界面文字按简体中文界面写；其他语言的界面里，菜单和按钮在同样的位置。
- 命令行示例假设 `dshr` 已经在 `PATH` 里；从源码构建时它在 `target/release/dshr`（Windows 上是 `dshr.exe`）。
