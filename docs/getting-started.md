# 快速上手

这篇带你从源码构建 dshr，配好模型，完成第一次对话。全程大约需要一次完整编译的时间。

## 1. 准备环境

| 需要 | 说明 |
|---|---|
| Rust 1.95 或更新 | 用 [rustup](https://rustup.rs) 安装；项目使用 edition 2024 |
| C 编译器 | 部分依赖带 C 源码，构建时一起编译（QuickJS-ng、Zstandard、AWS-LC）。Windows 用 MSVC（Visual Studio 生成工具），macOS 用 Xcode 命令行工具，Linux 用 gcc 或 clang |
| Xcode（仅 macOS 桌面版） | GPUI 在 macOS 上用 Metal 渲染，需要 Xcode 里的 Metal 编译器和 macOS SDK |

支持的平台：

- **桌面版**：Windows 与 macOS。
- **命令行版**：Windows、macOS，也可以在 Linux 上构建和运行。

## 2. 构建与运行

在仓库根目录：

```bash
# 桌面版（首次编译较慢：GPUI 和界面组件库体积较大）
cargo run --release -p dshr-desktop

# 命令行版
cargo build --release -p dshr-cli
```

命令行版的可执行文件是 `target/release/dshr`（Windows 上是 `dshr.exe`），可以把它复制到 `PATH` 里的某个目录。
桌面版的可执行文件是 `target/release/dshr-desktop`；在 macOS 上可以用 `scripts/bundle-macos.sh` 打成 `.app` 和 `.dmg`
（见 [开发与贡献](development.md#打包)）。

> 工作区的默认成员不含桌面版和界面 crate，所以单独的 `cargo build`、`cargo test` 不会编译 GPUI。
> 要编译桌面版时用 `-p dshr-desktop`。

## 3. 首次启动

第一次打开桌面版会出现欢迎窗，有三种选择：

1. **使用 DeepSeek 账号登录**：在浏览器里完成授权，dshr 自动继续。登录令牌保存在系统钥匙串里。
2. **填写 DeepSeek API 密钥**：粘贴后点“保存并继续”。密钥保存在系统钥匙串（Windows 凭据管理器 / macOS 钥匙串）里，
   不会写进配置文件。
3. **稍后设置**：先进主界面，之后在“设置 → 模型”里添加任意提供商。

dshr 的数据都放在数据目录 `~/.dshr` 里（配置、会话、插件……），桌面版启动时自动创建。

> 想先看看界面、手头又没有密钥：用 `DSHR_PROVIDER=demo` 启动桌面版。内置的演示模型不联网，会依次列目录、
> 运行一条命令（触发一次审批）、再流式输出一段 Markdown。

## 4. 第一次对话

1. **选择工作区**。首页输入卡片上方是“选择工作区”：选一个项目文件夹。agent 会在这个目录里读写文件、运行命令。
   也可以启动时直接带上目录：`dshr-desktop <目录>`。
2. **描述任务**。在输入框里写下你要做的事，例如“读一下这个项目的 README，告诉我怎么运行测试”，按 `Enter` 发送
   （`Shift+Enter` 换行）。
3. **看它工作**。回复以流式显示；每次工具调用是一张卡片：读了哪个文件、搜了什么、跑了什么命令和输出。
4. **处理审批**。默认的权限模式是“工作区内修改”：工作区里的修改直接执行，运行命令、改工作区外的文件之前会在卡片上
   问你。可以选“允许”“允许，本会话内不再询问此工具”或“拒绝”（键盘：`↑` `↓` 选择、`Enter` 确认、`1`–`3` 快速选择、
   `Esc` 拒绝）。
5. **继续或停下**。回复期间可以随时点停止；也可以继续输入——`Enter` 把消息排进队列，等这一轮结束后发送；
   `Ctrl+Enter` 让它“插队”，当前这一步完成后就交给模型。

第一轮结束后，dshr 会让模型给会话起一个简短的标题，会话出现在侧边栏对应的工作区下面。

## 5. 换用其他模型

打开“设置”（`Ctrl+,`）→“模型”：

- **添加提供商** → “第三方模型提供商”：从内置目录里选 Anthropic、OpenAI、Kimi (Moonshot)、OpenRouter、Ollama，
  填入 API 密钥即可。
- **添加提供商** → “自定义模型 API”：填名称、协议（Messages 兼容或 OpenAI 兼容）、API 地址，用于公司网关或自建服务。
- 编辑提供商时可以点“获取可用模型”，从接口拉取模型列表；也可以手动添加模型 ID。
- “新会话默认设置”里选默认模型和默认思考等级。

对话中随时可以在输入卡片右下角的模型菜单里换模型、调思考等级。详见 [模型与提供商](providers.md)。

## 6. 试试命令行

```bash
# 用环境变量提供 DeepSeek 的密钥（桌面版里保存过的话可以省略）
export DEEPSEEK_API_KEY=sk-...          # PowerShell：$env:DEEPSEEK_API_KEY = "sk-..."

dshr headless "这个项目用什么构建？列出主要的目录"
dshr headless --preset review "审查当前未提交的改动"
dshr sessions
```

`dshr headless` 运行一轮对话：回复写到标准输出，工具调用等过程信息写到标准错误。需要审批时在终端里问 `[y/N]`。
详见 [命令行参考](cli.md)。

## 下一步

- 给项目写一份 `AGENTS.md`，告诉 agent 怎么构建、测试、遵守哪些约定：[项目说明与 Skills](instructions-and-skills.md)。
- 了解四种权限模式，按你的信任程度选一个：[权限与安全](permissions.md)。
- 接入 MCP 服务器，给 agent 更多工具：[MCP 服务器](mcp.md)。
- 让 agent 自己写一个插件：在输入卡片的预设菜单里选“创造模式”，描述你想要的功能。或者自己动手：[插件开发指南](plugins.md)。
