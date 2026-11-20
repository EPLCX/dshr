# MCP 服务器

[MCP（Model Context Protocol）](https://modelcontextprotocol.io) 服务器给 agent 提供额外的工具、资源和 prompts：
数据库查询、issue 系统、浏览器自动化、公司内部文档……dshr 支持两种连接方式：

- **stdio**：dshr 启动一个本地进程，通过标准输入输出通信；
- **streamable HTTP**：连接一个远程地址，支持需要 OAuth 登录的服务器。

## 添加服务器

**在设置里**：“设置 → 扩展 → MCP 服务器”，填名称和启动命令（如
`npx -y @modelcontextprotocol/server-filesystem D:/docs`），点“添加”。命令按空格拆成参数。

**在配置文件里**：参数里有空格、需要环境变量或工作目录、或者是 HTTP 服务器时，直接编辑 `~/.dshr/config.toml`。

## 配置

每个服务器是 `[plugins.mcp.servers.<名字>]` 下的一张表。名字只用来区分服务器，也是工具名的一部分。

```toml
# stdio：启动一个进程
[plugins.mcp.servers.files]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "D:/docs"]
env = { LOG_LEVEL = "warn" }
cwd = "D:/tools"            # 可选，不写时继承 dshr 的工作目录
timeout_ms = 60000          # 可选，单次工具调用的时限，默认 120000（两分钟）

# streamable HTTP：连一个地址
[plugins.mcp.servers.docs]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${env:DOCS_TOKEN}" }

# 暂时停用，不删配置
[plugins.mcp.servers.experimental]
command = "my-server"
enabled = false
```

| 键 | 说明 |
|---|---|
| `command` | stdio 服务器的启动命令。和 `url` 二选一 |
| `args` | 命令参数（数组） |
| `env` | 额外的环境变量 |
| `cwd` | stdio 服务器的工作目录 |
| `url` | streamable HTTP 服务器的地址。和 `command` 二选一 |
| `headers` | HTTP 服务器的请求头 |
| `enabled` | 写 `false` 暂时停用 |
| `timeout_ms` | 单次工具调用的时限，默认两分钟 |
| `oauth` | HTTP 服务器的 OAuth 设置：`client_id`、`client_secret`、`scopes`（见 [登录](#需要登录的远程服务器)） |

在设置页里添加、移除、启停服务器立即生效：新增的连接、删掉的断开、改了的重连，没变的不受影响。
整个 MCP 功能也可以在“设置 → 扩展 → 内置功能”里关闭（`[plugins.mcp] enabled = false`）。

## 占位符

配置里的字符串值（命令、参数、环境变量、地址、请求头）可以写占位符：

| 写法 | 换成 |
|---|---|
| `${env:NAME}` | 环境变量 `NAME` 的值 |
| `${secret:NAME}` | 保存在系统钥匙串里的密钥 `NAME`（见下文） |
| `$${` | 字面的 `${` |

插件自带的服务器另外可以用 `${plugin}`（插件文件夹）和 `${data}`（插件的数据目录），见 [插件开发指南](plugins.md#插件包)。

占位符的值缺失时，这个服务器连接失败，状态里写明缺了什么。

## 密钥

不要把令牌直接写进 `config.toml`。用 `${secret:NAME}` 引用，再把值存进系统钥匙串：

```toml
[plugins.mcp.servers.github]
command = "github-mcp-server"
args = ["stdio"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "${secret:GITHUB_TOKEN}" }
```

用命令行设置值：

```bash
dshr mcp secret github GITHUB_TOKEN     # 值从标准输入读一行，不出现在命令行和 shell 历史里
```

值存在钥匙串条目 `mcp:<服务器>:<名字>` 里，桌面版和命令行版共用。没有钥匙串条目时读环境变量
`DSHR_MCP_SECRET_<服务器>_<名字>`（服务器名大写，其他字符换成 `_`），例如 `DSHR_MCP_SECRET_GITHUB_GITHUB_TOKEN`。

> 设置页里没有填写 MCP 密钥的输入框，请用上面的命令或环境变量；缺少密钥时，连接失败的提示里会给出完整的命令。
> 插件自带的服务器用的是插件的密钥，可以在“设置 → 扩展”的插件卡片上填写。

## 需要登录的远程服务器

有些 HTTP 服务器要求 OAuth 登录（按 MCP 2025-06-18 的授权规范）。这样的服务器状态显示“需要登录”：

- 桌面版：“设置 → 扩展 → MCP 服务器”里点“登录”；
- 命令行：`dshr mcp login <服务器>`，打印授权地址并尝试用浏览器打开，最多等 5 分钟。

登录过程：

1. 不带令牌连接服务器，得到 401 和受保护资源元数据的地址；
2. 取受保护资源的元数据（RFC 9728）找到授权服务器，再取授权服务器的元数据（RFC 8414，或 OpenID 的发现文档）；
3. 配置里没有 `oauth.client_id` 时，向授权服务器动态注册客户端（RFC 7591）；
4. 在浏览器里走授权码 + PKCE 流程，请求带上 `resource`（RFC 8707），回调到本机回环地址；
5. 令牌存进系统钥匙串（条目 `mcp:<服务器>:oauth`），之后每次连接带上 `Authorization: Bearer …`，快过期时自动刷新。

授权服务器不支持动态注册时，在配置里写明客户端：

```toml
[plugins.mcp.servers.tracker]
url = "https://tracker.example.com/mcp"
oauth = { client_id = "dshr-desktop", scopes = ["read", "write"] }
```

登录只在你点“登录”或运行 `dshr mcp login` 时进行，agent 运行时不会弹出浏览器。退出登录（设置页或 `dshr mcp logout`）
删除保存的令牌。

## 连接与状态

| 状态 | 含义 |
|---|---|
| 未连接 | 刚配置好还没用过；第一次用到时连接 |
| 连接中 | 正在启动、握手、取工具列表 |
| 已连接 | 可用，显示工具数量、是否提供资源、prompts 数量 |
| 连接失败 | 连不上或断开了（进程退出、HTTP 会话失效）；后台按 1 秒起翻倍、最多 5 分钟的间隔自动重连，不让对话等它 |
| 已停止重连 | 连续失败 5 次后停下，等你点“重连”或修改配置 |
| 需要登录 | 见上文 |
| 已停用 | 配置里 `enabled = false` |

- 桌面版启动后在后台先把服务器连上、取到工具列表，第一轮对话不用等（`npx` 类的服务器常要几秒）。
- 启动、握手并取到工具列表最多等 30 秒（`npx` 第一次运行要下载包）。
- 服务器通知工具列表变了时自动重新获取。
- 正在执行的工具调用遇到断开会直接报错，不自动重试——工具可能有副作用。
- 每个服务器保留最近 200 行日志（stdio 服务器的标准错误等）。`dshr mcp list` 显示每个服务器的状态、工具和最近 5 行日志，
  是排查连接问题的第一步。

## 工具、资源与 prompts

**工具**：服务器的工具在模型那边叫 `mcp__<服务器>__<工具>`（名字里字母、数字、`_`、`-` 以外的字符换成 `_`；
超过 64 个字符时截断并加哈希后缀）。工具可以返回图片，图片显示在工具卡片里，也交给模型。

MCP 工具在 dshr 之外运行、不受命令沙箱约束，访问类型是“交给外部”：在“工作区内修改”模式下每次调用都会询问，
自动审查模式下交给审查（见 [权限与安全](permissions.md)）。

**资源**：服务器提供资源时，模型多两个工具：

- `mcp_list_resources`：列出资源；
- `mcp_read_resource`：读一个资源的内容。

你也可以在输入框里用 `@` 直接选择资源，附到消息里。

**Prompts**：服务器的 prompts 出现在输入框的 `/` 菜单里：

```text
/mcp:<服务器>:<prompt> 参数名=值 参数名=值
```

选中后取到 prompt 的文字放进输入框（不直接发送，先让你看一眼），最多等 30 秒。

## 插件自带的 MCP 服务器

插件可以在 `plugin.toml` 里带上自己的 MCP 服务器（写法和上面一样，写在 `[mcp.servers.<名字>]` 下）。
这类服务器会在沙箱外以你的权限运行，所以：

- 每个服务器派生一项能力 `mcp:<名字>@<启动参数的哈希>`，要你单独允许；启动命令、参数、环境变量一变就要再允许一次；
- 安装时会把实际要启动的命令完整列出来；
- 它的密钥用插件清单里声明的 `secrets`。

详见 [插件开发指南](plugins.md#插件包)。

## 例子

```toml
# 本地文件系统
[plugins.mcp.servers.files]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "D:/docs"]

# 一个 Python 写的服务器，用 uv 运行
[plugins.mcp.servers.analytics]
command = "uv"
args = ["run", "--directory", "D:/tools/analytics-mcp", "server.py"]
env = { DB_PASSWORD = "${secret:DB_PASSWORD}" }

# 需要 API 令牌的远程服务器
[plugins.mcp.servers.search]
url = "https://search.example.com/mcp"
headers = { Authorization = "Bearer ${secret:SEARCH_TOKEN}" }
```

在 Windows 上，`npx`、`uvx` 这类命令其实是 `.cmd` 脚本：dshr 会按 `PATH` 和 `PATHEXT` 自动找到它们，直接写 `npx` 即可。
启动的服务器进程不会弹出控制台窗口。
