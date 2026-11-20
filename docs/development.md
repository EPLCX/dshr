# 开发与贡献

这篇是给参与 dshr 开发的人的操作手册：怎么构建、运行、测试，代码有哪些约定，界面、本地化、第三方声明、打包分别怎么做。
先读 [架构](architecture.md) 了解各个 crate 的分工。

## 环境

| 需要 | 用途 |
|---|---|
| Rust 1.95 或更新（edition 2024） | 全部 |
| C 编译器 | QuickJS-ng、Zstandard、AWS-LC 的 C 源码随依赖编译 |
| Windows：Visual Studio 生成工具（MSVC） | 桌面版、命令行版 |
| macOS：Xcode | GPUI 的 Metal 着色器和 macOS SDK |
| PowerShell 7（`pwsh`） | `scripts/third-party-notices.ps1`、`assets/make-icon.ps1` |

## 构建与运行

```bash
cargo build                              # 默认成员：除 dshr-ui、dshr-desktop 之外的全部
cargo build -p dshr-desktop              # 桌面版（编译 GPUI，较慢）
cargo build --release -p dshr-cli        # 发布版命令行
```

开发时**不要用自己真实的数据目录**，用 `DSHR_HOME` 指到一个临时目录（`.dev/` 已在 `.gitignore` 里）：

```bash
DSHR_HOME=.dev/home cargo run -p dshr-desktop -- .           # 以仓库本身为工作区
DSHR_HOME=.dev/home DSHR_PROVIDER=demo cargo run -p dshr-desktop   # 演示模型：不联网
DSHR_HOME=.dev/home DSHR_LOG=debug cargo run -p dshr-cli -- headless "列出 crates 目录"
```

- **演示模型**（`DSHR_PROVIDER=demo`，只在桌面版里）：依次列目录、运行一条命令（会触发一次审批）、流式输出一段 Markdown。
  调界面时不花 token、不依赖网络。
- **诊断日志**：命令行版用 `DSHR_LOG`（`tracing` 的过滤语法，如 `dshr_mcp=trace,info`），写到标准错误。
- **启动计时**（桌面版）：`DSHR_STARTUP_LOG=<文件路径>` 时，把启动各阶段相对进程启动的毫秒数追加写进这个文件（相对路径按启动时的工作目录算），
  用来排查启动慢。这是临时诊断代码（`crates/ui/src/startup.rs`），稳定后可以删掉。

## 测试

```bash
cargo test                               # 默认成员的全部测试
cargo test -p dshr-agent                 # 单个 crate
cargo test -p dshr-plugins -- hot_reload # 按名字过滤
cargo test -p dshr-ui                    # 界面测试（GPUI 的 test-support，编译较慢）
```

测试的写法：

- **模型**：用 `dshr_llm::mock` 的脚本化假模型，按顺序给出回复和工具调用，不联网。
- **钥匙串**：测试环境（CI 容器）通常没有系统钥匙串。插件密钥、MCP 令牌等在 `#[cfg(test)]` 下改用内存存储；
  新增用到钥匙串的代码也要这样做。
- **MCP**：`McpPlugin::with_connector` 接内存里的假服务器。
- **插件**：`crates/plugins/src/tests*.rs` 在临时数据目录里生成插件、授权、加载，覆盖热重载、能力、钩子、界面、安装与签名。
- **界面**：`#[gpui::test]` + `TestAppContext`，例如安装确认框的风险评估、高对比度配色逐对检查 WCAG AAA 对比度（`high_contrast_meets_aaa`）。

## 代码约定

- **注释和文档用中文**，解释“为什么”。面向用户的报错用中文（或走本地化）；**面向模型的文字用英文**：系统提示词、工具描述、
  工具结果里的说明。
- 提交前 `cargo clippy --workspace` 没有新警告。仓库没有 rustfmt 配置，格式跟周围的代码保持一致（行宽较长）。
- 新功能优先写成插件（见下文），不要在 agent 循环里加特例。
- **界面线程上不做耗时的事**：不扫文件、不执行插件脚本、不联网、不读钥匙串。放到后台执行器，界面先按用户的操作更新，做完再对账，失败时退回并写明原因。
- 只做这一次操作需要的事：开关一个插件只重载这一个插件，不重新扫描整个插件目录。
- 写文件先写临时文件再改名（`config.toml`、授权记录、链接记录都是这样），避免写到一半损坏。
- 会话日志只追加不改写；读取时要兼容旧版本写下的数据。

## 添加一个内置功能

内置功能都是 `dshr_context::Plugin`。以一个提供工具的插件为例：

```rust
use std::sync::Arc;
use dshr_context::{Manifest, Plugin, Scope};
use dshr_tools::Tools;

pub struct ClockPlugin;

impl Plugin for ClockPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("clock")                 // 也是 [plugins.clock] 的键
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<Tools>(Arc::new(NowTool)); // NowTool 实现 dshr_tools::Tool
        Ok(())
    }
}
```

然后：

1. 在 **`apps/desktop/src/main.rs` 和 `apps/cli/src/main.rs` 两处**的装配里加上它（主代理和子代理都要的放进 `builtin()`）。
2. 需要配置时用 `Manifest::config::<C>()` 声明，`cx.config::<C>()` 读取；字段的文档注释会成为设置页上的说明。
3. 在 `crates/ui/locales/app.yml` 里加 `plugin_name.<id>` 和 `plugin_desc.<id>`（“设置 → 扩展 → 内置功能”显示），
   配置项的标签加在 `plugin_config.<id>.<字段>` 下。
4. 它实际能做的事（运行命令、联网……）写进 `crates/ui/src/permissions.rs` 的 `builtin_permissions`，设置页会列出来。
5. 需要专门的界面（工具卡片、输入卡片上的面板、设置页）时，在 `dshr-ui` 里写对应的 `UiPlugin`，按同一个 id 注册（见 [架构](architecture.md#ui-插件)）。

钩子同理：`cx.hook::<GateTool>(优先级, Arc::new(|request| …))`。各钩子的优先级约定见 [架构](architecture.md#扩展点与钩子)。

## 界面开发

- 先读 [设计规范](design.md)。颜色、圆角、字号、间距、动效全部来自 `crates/ui/src/design.rs` 和 `design/motion.rs`，视图里不写具体数值。
- 可点击的东西用 `design::pressable`；纯图标按钮必须设 `accessibility_label`；需要被读屏软件读到的文字用 `design::readable` / `heading`。
- 用了默认集合以外的 Lucide 图标时，加到 `crates/ui/src/assets.rs` 的 `icon_assets!` 列表里，否则画出来是空的。
- 全局快捷键在 `lib.rs` 的 `bind_keys` 注册，可修改的加进 `EDITABLE_SHORTCUTS`，只展示的加进 `FIXED_SHORTCUTS`。
- 提交前按 [设计规范 §6](design.md#6-检查清单) 的清单逐项检查：浅色 / 深色、中文 / 英文（以及最长的德文）、只用键盘走一遍、触屏模式、减少动态效果。

## 本地化

界面文案用 `rust-i18n`，全部在 `crates/ui/locales/app.yml`：

```yaml
settings:
  language:
    zh-CN: "语言"
    en: "Language"
    zh-HK: "語言"
    zh-TW: "語言"
    de: "Sprache"
    el: "Γλώσσα"
    ja: "言語"
    es: "Idioma"
    fr: "Langue"
    pt: "Idioma"
```

- 代码里用 `t!("settings.language")`，带参数时 `t!("sidebar.new_in_workspace", name = name)`，文案里写 `%{name}`。
- **新增的每个键都要有十种语言**；缺的语言回退到简体中文。
- 支持的语言列在 `crates/ui/src/lib.rs` 的 `LANGUAGES` 里。新增一种语言：在 `LANGUAGES` 里加一项，并给 `app.yml` 的每个键补上译文。
- 中文文案用全角标点；按钮文字不加句号；提示语写完整的句子（见设计规范 §2.3）。

## 第三方声明

`THIRD_PARTY_NOTICES.md` 列出随程序分发的全部第三方组件及其许可证全文，“设置 → 关于”里也能查看（编译时 `include_str!` 进程序）。
它由脚本生成：

```bash
pwsh scripts/third-party-notices.ps1          # 依赖变化（Cargo.lock 改动）后重新生成并提交
pwsh scripts/third-party-notices.ps1 -Check   # 只检查是否最新，过期时退出码为 1（适合放进 CI）
```

- 范围：工作区全部成员在 Windows x64、macOS（Apple 芯片与 Intel）上的普通依赖；dev / build 依赖不进最终程序，不列入。
- 许可证全文取自各 crate 发布包里自带的 LICENSE / COPYING / NOTICE 文件，相同文本只保留一份。
- 带 C 源码或数据的组件单独说明：QuickJS-ng、Zstandard（按 BSD 使用）、AWS-LC、Lucide 图标、Unicode 字符数据。
  新引入这类依赖时，在脚本的 `$bundled` 和 `$extraLicenseFiles` 里登记。

## 补丁版依赖

`vendor/gpui-pre-windows` 是 crates.io 上 `gpui-pre-windows 0.3.12` 的副本，修复了 Windows 上拖动调整窗口大小导致内存膨胀的问题，
通过根 `Cargo.toml` 的 `[patch.crates-io]` 使用。改动和升级方法见 [`vendor/gpui-pre-windows/PATCHED.md`](../vendor/gpui-pre-windows/PATCHED.md)：
升级 `gpui-pre` 时先看上游是否已经修复，修复了就删掉这个目录和 `[patch.crates-io]`，否则用新版本重新复制再打上补丁。

## 打包

### macOS

```bash
scripts/bundle-macos.sh                      # 通用二进制（Apple 芯片 + Intel）
ARCHS=aarch64 scripts/bundle-macos.sh        # 只打 Apple 芯片
CODESIGN_IDENTITY="Developer ID Application: …" scripts/bundle-macos.sh
```

脚本依次：为每个架构 `cargo build --release -p dshr-desktop`，用 `lipo` 合成通用二进制 → 组装 `DSHR Desktop.app`（图标由 `assets/icon.png`
生成 `.icns`，附上 `LICENSE` 和 `THIRD_PARTY_NOTICES.md`）→ 签名（默认 ad-hoc）→ 生成带 `/Applications` 快捷方式的 `.dmg`。
产物在 `target/macos/`。

| 环境变量 | 默认值 | 说明 |
|---|---|---|
| `ARCHS` | `aarch64 x86_64` | 要编译的架构 |
| `MIN_MACOS` | `11.0` | 最低系统版本 |
| `BUNDLE_ID` | `dev.dshr.desktop` | Bundle Identifier |
| `CODESIGN_IDENTITY` | `-`（ad-hoc） | 签名证书 |

### Windows

```bash
cargo build --release -p dshr-desktop
```

`apps/desktop/build.rs` 把 `assets/icon.ico` 编进可执行文件（资源 ID 1，窗口、任务栏、资源管理器都用它）；发布版不弹控制台窗口。
分发时附上 `LICENSE` 和 `THIRD_PARTY_NOTICES.md`。仓库里目前没有安装程序的打包脚本。

### 图标

`pwsh assets/make-icon.ps1` 重新生成 `icon.ico`（Windows）、`icon.png`（1024×1024，macOS 打包用）和 `tray.png`（托盘）：
品牌色渐变的圆角方块加白色 `>_` 提示符，和侧边栏的品牌标记一致。

## 提交前

- [ ] `cargo test` 通过；改了界面的话 `cargo test -p dshr-ui` 也通过；
- [ ] `cargo clippy --workspace` 没有新警告；
- [ ] 改了依赖：重新生成 `THIRD_PARTY_NOTICES.md`；
- [ ] 改了界面：按设计规范的检查清单过一遍，新文案有十种语言；
- [ ] 改了行为、配置项、命令或插件 API：同步更新 `docs/` 下对应的文档。
