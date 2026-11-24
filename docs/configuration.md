# 配置参考

这篇列出 dshr 读写的所有文件、`config.toml` 的每一项、环境变量，以及系统钥匙串里存了什么。
大部分设置都可以在桌面版的设置弹窗里改；这里是给手动编辑和排查问题时查的。

## 数据目录

数据目录默认是 `~/.dshr`（Windows 上是 `C:\Users\<你>\.dshr`），可以用环境变量 `DSHR_HOME` 指到别处。
它和原版 DeepSeek Harness 的 `~/.dsh` 是两个目录，互不影响。

```text
~/.dshr/
├── config.toml                  设置（见下文）
├── keybindings.json             改过的快捷键（没改过就没有这个文件）
├── AGENTS.md                    全局说明，对所有工作区生效
├── skills/<名字>/SKILL.md       全局 Skills
├── themes/<id>.toml             自定义配色方案
├── sessions/
│   ├── <会话 id>/session.v1.jsonl          会话日志
│   ├── <会话 id>/agents/<调用 id>.jsonl    这个会话里子代理的日志
│   └── .trash/                             删除的会话
├── plugins/<id>/                插件目录：每个 JS 插件一个文件夹
├── plugin-data/<id>/            各插件自己的数据（host.readData / writeData）
├── plugin-grants.json           插件被允许的能力
├── plugin-links.json            链接到插件目录之外的开发中插件（dshr plugin link）
├── plugin-sources.json          装过的插件的来源、版本、签名者
├── plugin-versions/<id>/…       升级前归档的旧版本（回滚用，每个插件最近 3 个）
└── desktop.instance             桌面版运行时的实例信息（正常退出时删除）
```

这些文件都是普通文本（TOML / JSON / JSONL），可以直接查看和备份。**密钥不在这里**：API 密钥、插件密钥、登录令牌只存在
系统钥匙串里（见 [钥匙串条目](#钥匙串条目)）。

## config.toml

`config.toml` 只保存你在文件或设置页里写下的值；没写的项用默认值，文件不存在时全部用默认值。设置页保存时先写临时文件
再改名，不会留下写了一半的文件。环境变量的覆盖只在运行时生效，不会被写回文件。

一份完整的示例：

```toml
[model]
default = "deepseek/deepseek-flash"
effort = "high"
context_window = 128000

[ui]
theme = "system"
motion = "system"
language = "zh-CN"
close_to_tray = true
high_visibility_outlines = false
high_contrast = "system"
text_scale = "system"
palette = ""

[[providers]]
id = "deepseek"
adapter = "deepseek"

[[providers]]
id = "anthropic"
adapter = "anthropic"
prompt_cache = "1h"
models = [{ id = "claude-sonnet-x", name = "Claude Sonnet" }]

[[providers]]
id = "company-gateway"
adapter = "custom"
name = "公司网关"
protocol = "openai"
base_url = "https://gateway.example.com/v1"
models = [{ id = "gpt-x", name = "GPT X" }]

[plugins.compaction]
threshold = 0.7

[plugins.title]
enabled = false

[plugins.mcp.servers.files]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "D:/docs"]

[plugin_index]
urls = ["https://example.com/dshr-plugins/index.json"]
```

### [model]

| 键 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `default` | 字符串 | `"deepseek/deepseek-flash"` | 新会话的默认模型，写成 `provider/model`。提供商被删或模型不在目录里时，退回第一个可用的模型 |
| `effort` | `off` / `low` / `high` / `max` | 不写 | 新会话的默认思考等级。不写时不发送这个参数，由模型决定 |
| `context_window` | 整数 | `128000` | 模型的上下文窗口（token）。上下文压缩据此判断是否快满了 |

### [ui]

| 键 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `theme` | `system` / `light` / `dark` | `system` | 主题 |
| `motion` | `system` / `reduce` / `full` | `system` | 动态效果：跟随系统、减少、完整 |
| `language` | 字符串 | `"zh-CN"` | 界面语言：`zh-CN`、`zh-HK`、`zh-TW`、`en`、`de`、`el`、`ja`、`es`、`fr`、`pt` |
| `close_to_tray` | 布尔 | `true` | 关闭窗口时隐藏到托盘，而不是退出 |
| `high_visibility_outlines` | 布尔 | `false` | 高可见性控件轮廓 |
| `high_contrast` | `off` / `on` / `system` | `system` | 高对比度；旧版本写的 `true` / `false` 照样能读 |
| `text_scale` | `"system"` 或百分比数字 | `"system"` | 文字大小；设置页提供 90、100、115、130 |
| `palette` | 字符串 | 空 | 配色方案：`themes/<id>.toml` 写 `<id>`，插件带的写 `<插件 id>/<id>`；空表示默认配色。开启高对比度时不起作用 |
| `workspaces` | 路径数组 | 空 | 侧边栏里的工作区，最近用过的在前；由程序维护 |
| `sidebar_collapsed` | 布尔 | `false` | 侧边栏收起成一列图标按钮；在界面上收起、展开时由程序维护 |
| `welcome_skipped` | 布尔 | `false` | 欢迎窗里点过“稍后设置”；由程序维护 |

### [[providers]]

每个 `[[providers]]` 是一个模型提供商。只要写了任何一个 `[[providers]]`，提供商列表就完全以文件为准；文件里一个都没写时，
默认只有内置的 DeepSeek。

| 键 | 说明 |
|---|---|
| `id` | 唯一标识，用于模型引用（`<id>/<模型>`）和钥匙串条目名。必填 |
| `adapter` | 内置适配器：`deepseek`、`deepseek-oauth`、`anthropic`、`openai`、`moonshot`、`openrouter`、`ollama`；自定义接口写 `custom`；插件提供的服务商写 `plugin`。必填 |
| `name` | 自定义提供商的显示名（内置适配器用自己的名字） |
| `protocol` | 自定义提供商的协议：`messages`（Anthropic Messages 兼容）或 `openai`（OpenAI Chat Completions 兼容） |
| `base_url` | 接口地址；不写用适配器的默认地址 |
| `models` | 模型目录：`[{ id = "…", name = "…" }]`，`name` 可省略；不写用适配器的默认模型 |
| `prompt_cache` | Messages 协议的提示词缓存：`off` / `5m` / `1h`。不写时内置 Anthropic 是 `5m`，其余是 `off`；OpenAI 协议忽略此项 |
| `plugin` | `adapter = "plugin"` 时，提供它的插件服务商：`<插件 id>/<服务商 id>` |

各适配器的默认地址、环境变量和模型见 [模型与提供商](providers.md#内置提供商)。

### [plugins.&lt;id&gt;]

每个插件（内置功能、JS 插件、MCP）的配置。内容由插件自己解释；所有插件都认 `enabled = false`（停用）。
桌面版里内置功能的开关和配置要重启才生效，JS 插件立即生效。

**内置功能的 id**

| id | 功能 | 配置项 |
|---|---|---|
| `fs` | 文件工具（`read_file`、`write_file`、`edit_file`、`list_dir`） | — |
| `search` | 搜索工具（`grep`、`glob`） | — |
| `shell` | 命令工具（`run_shell`） | — |
| `plan` | 计划工具与计划面板（`update_plan`） | — |
| `provider-messages` | Messages 协议客户端 | — |
| `provider-openai` | OpenAI 协议客户端 | — |
| `permission` | 权限模式；停用后每次工具调用都会询问 | — |
| `sandbox` | 命令沙箱（macOS） | — |
| `compaction` | 上下文压缩 | `threshold`：用量达到上下文窗口的这个比例时压缩，0.1–1.0，默认 `0.75` |
| `presets` | 规划、审查等预设 | — |
| `environment` | 告诉模型工作区、系统、日期和权限模式 | — |
| `title` | 自动生成会话标题 | — |
| `delegate` | 子代理（`delegate` 工具） | — |
| `instructions` | 读取 `AGENTS.md` | — |
| `skills` | Skills 目录 | — |
| `mcp` | MCP 服务器 | `servers`：见 [MCP 服务器](mcp.md#配置) |
| `creator` | 创造模式 | — |

**JS 插件**的配置项由插件清单里的 `[config]`（JSON Schema）定义，设置页据此生成表单，脚本里通过 `host.config` 读取。
另外每个 JS 插件都认 `timeout_ms`：覆盖清单里写的单次工具调用时限。

```toml
[plugins.word-count]
max_chars = 50000
timeout_ms = 60000
```

### [plugin_index]

| 键 | 说明 |
|---|---|
| `urls` | 插件索引（静态 JSON）的地址列表。`dshr plugin search` / `outdated` / `update`、按 id 安装、设置页的“插件索引”都用它。默认没有 |

索引的格式见 [插件的打包、签名与分发](plugin-publishing.md#插件索引)。

## 环境变量

| 变量 | 作用 |
|---|---|
| `DSHR_HOME` | 数据目录，默认 `~/.dshr` |
| `DSHR_MODEL` | 运行时覆盖默认模型（`provider/model`；只写模型 ID 时当作 DeepSeek 的），不写回配置 |
| `DSHR_BASE_URL` | 运行时覆盖内置 DeepSeek 提供商的接口地址，不写回配置 |
| `DSHR_PROVIDER=demo` | 桌面版使用内置的演示模型，不联网 |
| `DSHR_LOG` | 命令行版的诊断日志过滤规则（写到标准错误），如 `debug`、`dshr_mcp=trace` |
| `DSHR_STARTUP_LOG` | 桌面版启动计时写到这个文件（诊断用，见 [开发与贡献](development.md#构建与运行)） |
| `DEEPSEEK_API_KEY`、`ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`MOONSHOT_API_KEY`、`OPENROUTER_API_KEY` | 对应提供商在钥匙串里没有密钥时读取 |
| `DSHR_PLUGIN_SECRET_<插件 ID>_<名字>` | 插件密钥的后备来源（插件 id 大写、`-` 换成 `_`），如 `DSHR_PLUGIN_SECRET_WORD_COUNT_API_TOKEN` |
| `DSHR_MCP_SECRET_<服务器>_<名字>` | MCP 服务器 `${secret:名字}` 的后备来源（服务器名大写，其他字符换成 `_`） |
| `DSHR_PLUGIN_SIGNING_KEY` | 插件签名私钥（base64 的 32 字节），钥匙串里没有时使用，适合 CI |

MCP 服务器的配置里还可以用 `${env:名字}` 引用任意环境变量（见 [MCP 服务器](mcp.md#占位符)）。

## 快捷键

`keybindings.json` 只记录改过的快捷键：动作名 → 按键。删掉这个文件（或在设置里点“全部恢复默认”）就回到默认值。

```json
{
  "new_session": "ctrl-shift-n",
  "settings": "ctrl-,",
  "focus_composer": "ctrl-l"
}
```

| 动作 | 默认（Windows） | 默认（macOS） |
|---|---|---|
| `new_session` | `ctrl-n` | `cmd-n` |
| `settings` | `ctrl-,` | `cmd-,` |
| `focus_composer` | `ctrl-l` | `cmd-l` |
| `toggle_sidebar` | `ctrl-b` | `cmd-b` |

按键写成 `修饰键-…-键`，修饰键可以是 `ctrl`、`alt`、`shift`、`cmd`、`fn`，至少要有一个；减号本身写作 `ctrl--`。
写错的项按默认值处理，不会影响启动。

## 配色方案

在 `~/.dshr/themes/` 下放 `<id>.toml`，它就会出现在“设置 → 通用设置 → 配色方案”里。只需要写想改的颜色，浅色、深色各一套：

```toml
name = "森林"

[light]
accent = "#2F7D4F"
accent_soft = "#E3F1E8"
selected = "#E8F0EA"

[dark]
accent = "#6FCF97"
accent_soft = "#1E3A2A"
surface = "#101512"
text = "#E8F0EA"
```

能改的颜色：

| 键 | 用途 |
|---|---|
| `text` | 正文、标题 |
| `text_secondary` | 说明、提示、分组标题 |
| `surface` | 窗口主体、卡片 |
| `raised` | 弹窗、菜单等浮层 |
| `sidebar` | 侧边栏和标题栏 |
| `sunken` | 面板、分段控件底色、工具输出 |
| `selected` | 选中的导航项、列表项、用户消息气泡 |
| `border` | 描边、分隔线 |
| `accent` | 发送按钮、链接、强调 |
| `accent_soft` | 徽标底色、强调色的浅底、选中文字的底色 |
| `on_accent` | 强调色上面的文字和图标 |
| `success` / `warning` / `danger` | 成功、需要注意、错误 |
| `focus_ring` | 键盘焦点框 |

颜色写成 `#rrggbb`。写了不认识的键、颜色格式不对，或者 `text` 和 `surface` 的对比度低于 4.5:1 时，整套方案被拒绝，
界面用默认配色，原因显示在设置页的“配色方案”下面。插件也可以导出配色方案（见 [插件 API 参考](plugin-api.md#themes)）。

## 钥匙串条目

dshr 把所有秘密都存在系统钥匙串里（Windows 凭据管理器、macOS 钥匙串），服务名都是 `dshr`：

| 条目 | 内容 | 在哪里设置 |
|---|---|---|
| `<提供商 id>-api-key` | 提供商的 API 密钥（DeepSeek 是 `deepseek-api-key`） | 欢迎窗、“设置 → 模型” |
| `deepseek-account-grant` | DeepSeek 账号登录的令牌 | 欢迎窗、“设置 → 模型”里的账号登录 |
| `plugin:<插件 id>:<名字>` | 插件声明的密钥 | “设置 → 扩展”、`dshr plugin secret set` |
| `mcp:<服务器>:<名字>` | MCP 服务器配置里 `${secret:名字}` 的值 | `dshr mcp secret` |
| `mcp:<服务器>:oauth` | 远程 MCP 服务器的 OAuth 令牌 | “设置 → 扩展”的登录、`dshr mcp login` |
| `plugin-signing` | 插件签名私钥 | `dshr plugin keygen` |

钥匙串不可用时（例如没有桌面会话的 Linux 服务器），API 密钥和各类密钥可以改用上面列出的环境变量提供。
