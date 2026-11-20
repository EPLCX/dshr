# DSHR 命令行参考

DSHR 的可执行文件叫 `dshr`（`cargo build --release -p dshr-cli` 生成在 `target/release/` 下）。它和桌面版共用同一个
数据目录、同一份配置和同一批插件，可以在终端、脚本和 CI 里使用。

```text
dshr <命令>

  headless   运行一轮对话，输出最终回答后退出
  sessions   列出本地会话
  plugin     管理 JS 插件
  mcp        管理 MCP 服务器
  help       显示帮助（dshr help <命令>、dshr <命令> --help）

  -V, --version   显示版本
```

**全局的环境变量**

| 变量 | 作用 |
|---|---|
| `DSHR_HOME` | 数据目录，默认 `~/.dshr` |
| `DSHR_LOG` | 诊断日志的过滤规则，写到标准错误，如 `DSHR_LOG=debug`、`DSHR_LOG=dshr_mcp=trace` |
| `DSHR_MODEL` | 本次运行的默认模型（`provider/model`；只写模型 ID 时当作 DeepSeek 的），不写回配置文件 |
| `DSHR_BASE_URL` | 本次运行里内置 DeepSeek 提供商的接口地址，不写回配置文件 |
| `DEEPSEEK_API_KEY` 等 | 钥匙串里没有保存密钥时，各提供商读取的环境变量（见 [模型与提供商](providers.md)） |

## dshr headless

运行一轮对话：把任务交给 agent，等它做完，输出最终回答后退出。

```bash
dshr headless [选项] <任务>
```

| 选项 | 说明 |
|---|---|
| `-w, --workspace <目录>` | 工作区，默认当前目录；目录必须存在 |
| `--resume <会话 id>` | 在已有会话里继续（id 见 `dshr sessions`） |
| `-m, --model <provider/model>` | 使用的模型，默认取配置里的默认模型 |
| `--effort <等级>` | 思考等级：`off` / `low` / `high` / `max`；默认取配置的 `[model] effort`，没写就不发送 |
| `--preset <预设>` | `standard`（默认）/ `plan` / `review` / `create` |
| `--manual` | 权限模式“手动确认”：除了读取，每一次修改、运行命令都先询问 |
| `--auto-review` | 权限模式“自动审查”：让模型判断风险，低风险的直接放行，有风险的再询问 |
| `-y, --yes` | 权限模式“完全访问”：修改任意文件、运行命令都不再询问 |

三个权限选项互斥；都不写时是“工作区内修改”。各模式的规则见 [权限与安全](permissions.md)。

### 输入与输出

- **标准输出**只有模型回复的正文（流式写出），方便重定向或交给其他程序。
- **标准错误**是过程信息：
  - `[调用 <工具> <参数>]`：每次工具调用；
  - 命令的实时输出；
  - `[工具出错] …`、`[第 N 次请求失败] …`：出错与重试；
  - 结束时一行 `[会话 <id>「标题」，Completed]`（第一轮新起了标题时带标题）。
- **审批**：需要确认时在终端里问 `允许执行 <工具> <参数>？[y/N]`，只有输入 `y` / `yes` 才放行。自动审查认为有风险时，
  问题前面带一行 `[自动审查] <理由>`；必须由你亲自决定的操作带 `[必须确认]`。
- 标准输入不是终端（例如在管道或 CI 里）时没法询问：这时除了 `--yes` 都会直接报错退出。
- `Ctrl+C` 中断当前这一轮（`[正在中断…]`），已经写下的内容保留在会话日志里。

### 退出码

| 情况 | 退出码 |
|---|---|
| 这一轮正常完成 | `0` |
| 出错（模型请求失败、配置错误等）、被中断、或被插件拦下 | 非 `0`，原因写在标准错误里 |

### 模型和密钥从哪里来

1. `--model` 指定的模型；否则是配置里的默认模型；默认模型失效（提供商被删、模型不在目录里）时用第一个可用的模型。
   `DSHR_MODEL` 会在这一步之前覆盖配置里的默认值。
2. 提供商的 API 密钥：先查系统钥匙串（桌面版“设置 → 模型”里保存的），再查该提供商的环境变量。
3. 都没有时报错：`<提供商> 未配置 API Key：在桌面版设置里保存，或设置环境变量 <变量名>`。

### 和插件一起用

`headless` 装配的插件和桌面版一样：内置功能、`AGENTS.md`、Skills、MCP 服务器、插件目录里的 JS 插件都会加载。

- 有 `session:control` 能力的插件替你发的消息，会排在这一轮后面依次执行（标准错误里显示 `[由插件 <id> 发送] …`），
  一次运行最多跟着跑 10 轮；命令行版不能新建会话。
- 插件的桌面通知写到标准错误：`[通知 · <插件>] 标题 正文`；插件请求打开的网址也打印出来。

### 示例

```bash
# 只读地调查一个问题
dshr headless --preset plan "为什么 cargo test 在 Windows 上会失败？"

# 指定工作区和模型，结果写进文件
dshr headless -w ~/src/app -m deepseek/deepseek-v4-pro "总结 src/ 的模块划分" > summary.md

# 继续上次的会话
dshr sessions
dshr headless --resume 0d6c…e21 "把刚才的方案实现出来"

# 在隔离的容器或 CI 里无人值守地运行（不会询问任何操作，只在可以丢弃的环境里这样做）
dshr headless --yes "运行测试，修复失败的用例"
```

## dshr sessions

列出数据目录里的会话，最近修改的在前，每行是 `<会话 id>  <标题>`。还没有标题的会话显示第一条消息的开头。

```bash
dshr sessions
```

## dshr plugin

管理插件目录（`~/.dshr/plugins/`）里的 JS 插件。插件的写法见 [插件开发指南](plugins.md)。

### 开发

| 命令 | 说明 |
|---|---|
| `dshr plugin new <id> [--name <名字>] [--description <说明>] [--cap <能力>]… [--dir <目录>] [--bundle]` | 生成插件骨架：`plugin.toml` 和带示例工具的 `index.js`。`--cap` 可以写多次；在插件目录里生成时，这里写的能力视为已经允许。`--bundle` 同时生成 `skills/`、`commands/` 的示例和注释掉的 `[mcp.servers]` |
| `dshr plugin link <文件夹> [--id <id>]` | 把插件目录之外的文件夹当作插件加载（不拷贝），记在 `plugin-links.json` 里；桌面版的热重载同样盯着它 |
| `dshr plugin unlink <id>` | 取消链接，不动文件夹本身 |
| `dshr plugin dev [<文件夹或 id>]` | 插件调试台（见下文），默认当前目录 |
| `dshr plugin list` | 列出插件，并像真正运行时一样加载一遍：版本、状态（可用 / 已停用 / 等待授权 / 激活失败）、能力、还没允许的能力、自带的 MCP 服务器、命令、预设、Skills、没设置的密钥、所在文件夹 |

**插件调试台** `dshr plugin dev` 盯着插件的文件，每次保存后重新加载并打印报告（加载结果、导出的工具、等待授权的能力、
页面预览、最近的日志）。在 `dev>` 提示符下可以：

| 命令 | 作用 |
|---|---|
| `tool <名字> [JSON]` | 调用一个工具；会写文件、联网或启动程序的工具先问你 |
| `page <页> [<动作> [JSON]]` | 画出一个页面的节点树，或在这个页面上执行一个动作再画 |
| `grant` | 允许清单里声明的能力 |
| `logs` | 最近的日志 |
| `reload` | 马上重新加载并打印报告 |
| `help` | 命令说明 |
| `quit` | 退出 |

工作区是运行 `dshr plugin dev` 时的当前目录。参数是一个没链接过的插件文件夹时，会先自动 `link`。

### 授权与密钥

| 命令 | 说明 |
|---|---|
| `dshr plugin grant <id>` | 允许插件使用清单里声明的全部能力。声明了能力却没允许过的插件不会激活 |
| `dshr plugin revoke <id>` | 收回插件的全部授权；它声明了能力的话，下次加载时不会激活 |
| `dshr plugin secret list <id>` | 列出插件声明的密钥和是否已设置（不显示值），以及对应的环境变量名 |
| `dshr plugin secret set <id> <名字>` | 设置密钥：值从标准输入读一行（不出现在命令行和 shell 历史里），存进系统钥匙串 |
| `dshr plugin secret delete <id> <名字>` | 删除密钥 |

### 安装与分发

| 命令 | 说明 |
|---|---|
| `dshr plugin install <来源> [--sha256 <哈希>] [--id <id>] [--yes] [--force]` | 安装插件。来源可以是本地文件夹、`.dshrplugin` / `.zip` / `.tar.gz` 安装包、http(s) 地址（GitHub 仓库地址会换成默认分支的源码包），或插件索引里的 id。安装前列出名字、版本、来源、签名、声明的权限、自带的 MCP 服务器并询问；`--yes` 不问直接装（声明的能力仍要另外允许）；`--sha256` 校验压缩包；`--force` 在发布者签名变了时仍然安装 |
| `dshr plugin pack [<文件夹>] [-o, --out <文件>] [--sign]` | 打包成 `<id>-<版本>.dshrplugin`。打包前先实际加载一遍，加载不了的不打包；`--sign` 用签名密钥签名 |
| `dshr plugin keygen [--replace]` | 生成签名密钥：私钥存进系统钥匙串，打印公钥；已有时打印现有公钥，`--replace` 换一把新的 |
| `dshr plugin search <关键词>` | 在插件索引里搜索 |
| `dshr plugin outdated` | 列出索引里有新版本的已装插件 |
| `dshr plugin update <id> \| --all [--yes] [--force]` | 按索引更新，同样列出权限并确认 |
| `dshr plugin rollback <id> [--to <版本>] [--yes]` | 回滚到升级前归档的旧版本（每个插件保留最近 3 个） |
| `dshr plugin uninstall <id>` | 卸载：删除插件文件夹，收回授权 |

插件索引在 `config.toml` 的 `[plugin_index] urls` 里配置，默认没有。打包、签名和索引的格式见
[插件的打包、签名与分发](plugin-publishing.md)。

### 启用与停用

| 命令 | 说明 |
|---|---|
| `dshr plugin enable <id>` | 启用插件（删掉 `config.toml` 里的 `enabled = false`） |
| `dshr plugin disable <id>` | 停用插件（在 `[plugins.<id>]` 里写 `enabled = false`） |

这两个命令对内置功能（如 `compaction`、`title`）同样有效。桌面版里 JS 插件的启停立即生效，内置功能要重启。

## dshr mcp

管理 `config.toml` 里 `[plugins.mcp.servers]` 配置的 MCP 服务器。配置写法见 [MCP 服务器](mcp.md)。

| 命令 | 说明 |
|---|---|
| `dshr mcp list` | 实际连接一遍每个服务器，列出状态（已连接 / 失败 / 需要登录 / 已停用）、启动方式、提供的工具、资源和 prompts，以及最近 5 行日志 |
| `dshr mcp secret <服务器> <名字>` | 设置服务器配置里 `${secret:名字}` 用到的密钥：值从标准输入读一行，存进系统钥匙串 |
| `dshr mcp login <服务器>` | 登录要求 OAuth 的远程（HTTP）服务器：打印授权地址并尝试用浏览器打开，最多等 5 分钟；令牌存进系统钥匙串 |
| `dshr mcp logout <服务器>` | 退出登录：删掉保存的令牌 |
