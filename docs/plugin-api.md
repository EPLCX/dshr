# 插件 API 参考

这是 JS 插件可用的全部接口：模块导出项、`host` 的成员、界面节点、能力和限额。入门和背景知识见 [插件开发指南](plugins.md)。

约定：

- “需要”一栏是插件清单 `capabilities` 里要声明、并且用户允许过的能力；`—` 表示不需要。
- 所有 `host` 上的异步函数都返回 Promise。出错时抛出 `Error`，信息里写明原因（如缺少哪项能力）。
- 文件路径是相对工作区的路径（或工作区里的绝对路径）；数据目录的路径相对 `~/.dshr/plugin-data/<id>/`。

## 模块导出

| 导出 | 类型 | 需要 | 说明 |
|---|---|---|---|
| [`tools`](#tools) | 数组 | — | 给模型的工具 |
| [`prompt`](#prompt) | 函数 | — | 每轮追加到系统提示词后面的文本 |
| [`events`](#events) | 对象 | — | 插件事件怎么进上下文、怎么显示 |
| [`hooks`](#hooks) | 对象 | 视钩子而定 | 挂在 agent 循环上的钩子 |
| [`on`](#on) | 对象 | 视事件而定 | 订阅会话事件 |
| [`api`](#api) | 对象 | — | 给其他插件调用的方法 |
| [`mentions`](#mentions) | 数组 | — | 输入框 `@` 菜单的来源 |
| [`watch`](#watch) | 数组 | `background` + `fs:workspace:read` | 工作区文件变化 |
| [`cards`](#cards) | 对象 | — | 自己工具的卡片 |
| [`ui`](#ui) | 对象 | — | 页面、侧边栏 / 设置 / 面板 / 状态行入口、隐藏内置入口 |
| [`commands`](#commands-与-slash) | 数组 | — | 命令（`/<插件>:<id>`，可带快捷键） |
| [`slash`](#commands-与-slash) | 数组 | — | 斜杠命令（`/<名字>`） |
| [`presets`](#presets) | 数组 | — | 预设 |
| [`providers`](#providers) | 数组 | `provider` | 模型服务商 |
| [`themes`](#themes) | 数组 | — | 配色方案 |
| [`schedules`](#schedules) | 数组 | `background` | 定时任务 |
| [`services`](#services) | 数组 | `background` | 常驻服务 |
| [`activate` / `deactivate`](#生命周期) | 函数 | — | 加载后、卸下前 |
| [`state`](#生命周期) | 对象 | — | 热重载时保存和恢复模块状态 |

导出可以是命名导出，也可以挂在 `export default { … }` 上。

### tools

```js
export const tools = [{
  name: "word_count",
  description: "Count the words of a file in the workspace.",
  inputSchema: { type: "object", properties: { path: { type: "string" } }, required: ["path"] },
  readOnly: true,
  async execute(input, host) { return "…"; },
}];
```

| 字段 | 说明 |
|---|---|
| `name` | `[A-Za-z0-9_-]`，≤ 64 个字符；不能和已有工具（内置、`delegate`、其他插件、MCP）重名，不能以 `mcp__` 开头 |
| `description` | 给模型看的说明 |
| `inputSchema` | 参数的 JSON Schema，默认 `{ type: "object", properties: {} }` |
| `readOnly` | `true` 时按只读处理，任何模式下都直接执行；运行时不能写文件、联网、启动程序、代办 |
| `execute(input, host)` | 返回字符串（原样）、对象（转 JSON）、`{ content, images: [{ data, mediaType }] }`（最多 4 张 PNG / JPEG / GIF / WebP，base64）；`throw` 得到错误结果 |

单次调用的时限是清单的 `timeout_ms`（默认 30 秒），用户可在 `[plugins.<id>] timeout_ms` 覆盖。

### prompt

```js
export const prompt = async ({ workspace }, host) => "Text appended to the system prompt.";
```

每轮开始时调用一次，最多等 1 秒，返回值最多 32 KB；`host` 只读（不能写、不能联网）。返回空字符串或 `undefined` 表示这一轮不追加。

### events

```js
export const events = {
  "word-count/counted": {
    project: (data) => `[counted] ${data.path}`,     // 进模型上下文的文字；返回 null 不进
    label: (data) => `Counted ${data.path}`,          // 对话区里的一行
    render: (data) => ({ type: "kv", items: [] }),    // 可选：画节点树代替 label
  },
};
```

事件用 `host.emitEvent("<插件 id>/<名字>", data)` 写入，名字只能用小写字母、数字、`-`、`_`、`.`，不能用核心事件的命名空间
（`turn`、`step`、`tool`、`session` 等）。`project` 最多等 0.5 秒、`label` / `render` 0.2 秒；等不到时用通用显示。
日志只追加不改写，旧版本写下的数据也会交给这些函数，要兼容。

### hooks

所有钩子都可以是同步或异步函数；返回 `undefined` 表示没有意见。超时、抛异常都按没有意见处理并记进插件日志。
钩子都**不能写工作区、不能启动程序**。

#### beforeTool

```js
beforeTool: ({ tool, input, access }) => "deny" | "ask" | { decision: "deny", reason } | "allow"
```

- `access`：`{ type: "read" | "write" | "execute" | "external", path?, outside? }`。读工作区外的文件时是 `{ type: "read", path, outside: true }`（除完全访问模式外会询问用户）。
- 返回 `"allow"` 直接放行需要 `hooks:gate`；`"deny"`、`"ask"` 不需要能力。
- 同步执行，最多 300 ms。排在预设检查之后、权限模式之前。

#### reviewTool（需要 `hooks:review`）

```js
reviewTool: async ({ tool, input, access }, host) => "allow" | "deny" | { decision, reason }
```

本来要问用户的调用先交给它，最多等 20 秒。排在内置的自动审查之前。必须由用户亲自确认的操作不会交给它。

#### afterTool

```js
afterTool: async ({ tool, input, output, isError }, host) => { output } | undefined
```

工具执行后、结果写进日志前。可以写事件（排在这次调用的结果之后）、写数据；返回 `{ output }` 改写交给模型的结果需要 `hooks:tool-output`。

#### beforeWrite

```js
beforeWrite: ({ tool, path, before, after }) => "deny" | { decision: "deny", reason } | { content } | undefined
```

`write_file`、`edit_file` 写盘前同步调用，最多 1 秒。`before` 是原内容（新文件为 `null`），`after` 是要写的内容（前一个插件可能已经改过）。
返回 `{ content }` 改写需要 `hooks:write`，拒绝不需要。前后内容超过 256 KB 时不调用。可以读工作区（有读能力时），不能写、不能联网。

#### turnStarted

```js
turnStarted: async ({ session, turn, text, attachments }, host) => ({ block: "原因" }) | undefined
```

用户消息写进日志之前，最多 5 秒。返回 `{ block }`（或 `{ block: true }`）拦下这条消息：日志里什么都不写，界面把文字还给用户。
没有拦下时，钩子里 `host.emitEvent` 写的事件排在用户消息和附件之后（可用于补充上下文）。

#### beforeStep

```js
beforeStep: async ({ session, turn, step, events, lastSeq, contextWindow, estimatedTokens }, host) =>
  ({ compact: { summary, uptoSeq } }) | undefined
```

每一步请求模型之前。可以写事件。返回 `{ compact }` 把 `uptoSeq`（日志里已有的事件序号）及之前的对话换成摘要，需要 `context:compact`。

#### beforeRequest

```js
beforeRequest: ({ model, effort, messages, tools }) => "追加的文本" | { append, model, effort } | undefined
```

构造请求时同步调用，最多 300 ms。`messages`、`tools` 是数量。返回的文本（最多 8 KB）只追加到这一次请求的系统提示词后面，不进日志。
返回 `model` / `effort` 换模型和思考等级需要 `model:route`；模型只能是当前提供商模型目录里的，等级是 `off` / `low` / `high` / `max`。

#### transformRequest（需要 `context:rewrite`）

```js
transformRequest: async ({ model, system, messages }, host) => ({ system, messages }) | undefined
```

发出请求前最后一步，最多 5 秒。返回的 `system`、`messages` 替换请求里的（只影响这一次请求，不改日志）。

#### turnEnded

```js
turnEnded: async ({ reason, finalText, error }, host) => undefined
```

一轮结束后。`reason` 是 `completed` / `interrupted` / `error`。可以写事件、写数据、联网（有 `net:` 时）、调模型（有 `llm` 时）。

### on

```js
export const on = {
  "tool/result": async ({ type, session, seq, data }, host) => {},
  "word-count/*": (event, host) => {},
  "*": (event, host) => {},
};
```

键是事件类型、`命名空间/*` 或 `*`。事件写进日志时排进插件的队列（不阻塞 agent，最多排 256 件，满了丢弃并记日志），
`data` 最多 16 KB。自己命名空间以外的事件需要 `dshr:sessions:read`。处理函数可以读工作区、联网、读写数据目录、刷新页面；
不能写工作区、启动程序、调模型、写会话事件。

### api

```js
export const api = { search: async (args, host) => result };
```

其他插件在清单里写 `uses = ["<本插件 id>"]` 后，用 `host.plugins.call("<本插件 id>", "search", args)` 调用。
被调用的方法只能做调用方这次调用本来就能做的事。参数和结果要能转成 JSON（结果最多 2 MB）。

### mentions

```js
export const mentions = [{
  id: "issue",
  label: "Issues",
  search: async (query, host) => [{ id, title, detail }],
  resolve: async (id, host) => ({ title, text }),
}];
```

`search` 在用户输入时调用（最多 2 秒），`resolve` 在选中时调用（最多 10 秒），返回的 `text` 附进消息。

### watch

```js
export const watch = [{ id: "docs", glob: "**/*.md", run: async ({ workspace, changes, overflow }, host) => {} }];
```

需要 `background` 和 `fs:workspace:read`。监视打开着的会话的工作区（命令行版是这次运行的工作区），遵守 `.gitignore`，
跳过 `.git`、`node_modules`。变化攒 500 毫秒一批，每批每个工作区最多 1000 条，超出时 `overflow: true`。
处理函数能读工作区、联网、读写数据目录；不能写工作区、启动程序、调模型。

### cards

```js
export const cards = {
  word_count: {
    title: (input, output) => input.path,
    render: (input, output, isError) => ({ type: "text", text: output ?? "…" }),
  },
};
// 或者直接：cards = { word_count: (input, output, isError) => node }
```

只能给插件自己的工具画。工具运行中 `output` 是 `null`；交给卡片函数的结果最多 64 KB；函数最多等 0.3 秒，等不到用通用卡片。

### ui

```js
export const ui = {
  pages: { main: { render: (host) => node, actions: { add: (payload, host) => "提示" } } },
  sidebar: [{ id, label, icon, page }],
  settings: [{ id, label, page }],
  panels: [{ id, label, page }],
  status: [{ id, page }],
  hide: ["composer:presets"],
};
```

| 字段 | 说明 |
|---|---|
| `pages` | 页面名 → `{ render, actions }`，或直接一个 `render` 函数。`render(host)` 返回节点树，最多等 3 秒 |
| `actions` | 动作名 → `(payload, host)`；`payload = { value, inputs, checked }`：节点的 `payload`、各输入框的文字、开关的新状态。返回字符串显示为提示 |
| `sidebar` | 侧边栏入口，点开以弹窗显示页面 |
| `settings` | 设置弹窗里的一页（排在“扩展”之后） |
| `panels` | 输入框上方可折叠的面板 |
| `status` | 侧边栏底部的状态行，显示页面里的第一段文字 |
| `hide` | 要藏起来的内置入口：`settings:<页>`（不含 `extensions`）、`composer:presets`、`composer:permission`、`sidebar:<id>`、`status:<id>` |

入口的 `icon`：star、heart、bell、book、bot、calendar、chart、cpu、file、folder、globe、inbox、map、mic、moon、sun、network、
palette、search、settings、terminal、user、info。

`render` 是只读的；动作可以使用插件的全部能力而不再询问（用户亲手点的），也可以用 `host.composer`。

### commands 与 slash

```js
export const commands = [{ id: "open", title: "打开待办", keys: "ctrl-shift-t", run: async (_, host) => result }];
export const slash = [{ name: "todo-add", description: "添加待办", run: async ({ args }, host) => result }];
```

- `commands` 出现在输入框的 `/` 菜单里（`/<插件 id>:<id>`）；`keys` 是默认快捷键（写法同 `keybindings.json`），和内置快捷键冲突时不生效。
- `slash` 的 `name` 是 `[a-z0-9][a-z0-9_-]{0,31}`，用户输入 `/<名字> 参数`，`args` 是参数部分。
- 返回值：字符串或 `{ message }` 显示一句话；`{ insert }` 替换输入框；`{ send }` 直接发送；`{ open: "<页>" }` 打开页面；其他什么都不做。
- 可以使用插件的全部能力（用户触发），可以用 `host.composer`。

### presets

```js
export const presets = [{ id, label, description, instructions, readOnly, tools }];
```

| 字段 | 说明 |
|---|---|
| `id`、`label` | 必填 |
| `description` | 菜单里的说明 |
| `instructions` | 追加到系统提示词后面的说明 |
| `readOnly` | `true` 时这一轮强制只读 |
| `tools` | 可选的工具名单：模型只看到这些工具，名单外的调用被拒绝 |

### providers

```js
export const providers = [{
  id: "main",
  name: "My Gateway",
  protocol: "openai",                       // "openai" | "messages"
  baseUrl: "https://gateway.example.com/v1",
  models: ["model-a", "model-b"],           // 或 async (host) => [...]
  auth: async (host) => ({ apiKey: await host.oauth.token("TOKEN"), headers: { "x-team": "dev" } }),
}];
```

需要 `provider`。`auth` 在每个请求前调用（最多 10 秒），令牌可以随时变。用户在“设置 → 模型 → 插件提供的服务商”里添加后，
配置里是 `adapter = "plugin"`、`plugin = "<插件 id>/<服务商 id>"`。

### themes

```js
export const themes = [{ id: "forest", name: "森林", light: { accent: "#2F7D4F" }, dark: { accent: "#6FCF97" } }];
```

颜色键和 `themes/*.toml` 一样（见 [配置参考](configuration.md#配色方案)），值必须是 `#rrggbb`。配置里写作 `palette = "<插件 id>/<id>"`。

### schedules

```js
export const schedules = [{ id: "sync", every: "10m", run: async (host) => {} }];
```

需要 `background`。`every` 是毫秒数或 `"30s"`、`"10m"`、`"2h"` 这样的字符串，最短 10 秒。运行时没有工作区（文件函数指向数据目录），可以联网。

### services

```js
export const services = [{ id: "bridge", run: async (host) => { /* 一直运行 */ } }];
```

需要 `background`。在单独的服务运行时里执行（模块变量与工具不共享）。插件停用、被替换或卸载时中断；出错或返回后按退避重启
（1 秒起翻倍，最多 5 分钟；跑满 1 分钟算一次新的开始）；连续 5 次在 1 分钟内失败就停下。服务里能联网、代发消息（`session:control`）、
发通知；不能写工作区、启动程序。

### 生命周期

```js
export const activate = async (host) => {};      // 加载后，≤ 2 秒；抛异常 = 插件启动失败
export const deactivate = async (host) => {};    // 热重载替换它、或被移除之前，≤ 1 秒
export const state = {
  snapshot: () => ({ items }),                   // 热重载前，旧版本保存（≤ 1 秒）
  restore: (saved) => { items = saved.items; },  // 新版本加载后恢复（只用一次）
};
```

## host

### 基本

| 成员 | 需要 | 说明 |
|---|---|---|
| `host.workspace` | — | 当前工作区的路径；没有工作区时是插件数据目录 |
| `host.config` | — | 插件的配置（`[plugins.<id>]`，只读） |
| `host.log(...args)`、`console.*` | — | 写插件日志（设置页的“日志”、`dshr plugin dev` 的 `logs`），保留最近 200 行 |
| `host.output(text)` | — | 工具运行时的实时输出（显示在卡片里，不进日志） |
| `host.emitEvent(kind, data)` | — | 写一条插件事件；`kind` 以 `<插件 id>/` 开头 |

### 插件数据目录

不需要能力，总是指向 `~/.dshr/plugin-data/<id>/`。

| 成员 | 说明 |
|---|---|
| `readData(path)` | 文本内容；文件不存在时是 `null` |
| `writeData(path, text)` | 写入，自动建父目录；单个文件 ≤ 2 MB，总共 ≤ 64 MB |
| `removeData(path)` | 删除 |
| `listData()` | 列出数据目录里的文件 |

### 工作区文件

| 成员 | 需要 | 说明 |
|---|---|---|
| `readFile(path)` | `fs:workspace:read` | 文本，≤ 2 MB |
| `readFileBytes(path)` | `fs:workspace:read` | `Uint8Array` |
| `listDir(path = ".")` | `fs:workspace:read` | 目录项，最多 2000 项 |
| `stat(path)` | `fs:workspace:read` | `{ type, size, modified }`，不存在时 `null` |
| `exists(path)` | `fs:workspace:read` | 布尔 |
| `glob(pattern)` | `fs:workspace:read` | 遵守 `.gitignore`，最多 5000 条 |
| `grep(regex, { glob, ignoreCase, maxResults })` | `fs:workspace:read` | `[{ path, line, text }]`，最多 1000 条 |
| `writeFile(path, text)` | `fs:workspace:write` | 自动建父目录 |
| `writeFileBytes(path, bytes)` | `fs:workspace:write` | 写二进制 |
| `mkdir(path)` | `fs:workspace:write` | 建目录 |
| `remove(path, { recursive })` | `fs:workspace:write` | 删除；不能删除工作区本身 |
| `rename(from, to)` | `fs:workspace:write` | 改名 / 移动 |

只能访问工作区以内。没有打开的工作区时（设置页、预览、后台任务）这些函数指向插件数据目录。写文件只能在没有标 `readOnly`
的工具和页面动作里用。

### 网络

| 成员 | 需要 | 说明 |
|---|---|---|
| `fetch(url, { method, headers, body, secretHeaders, signal, stream })` | `net:<主机>` | 见下文 |
| `websocket(url, options)` | `net:<主机>` | WebSocket 客户端，连接属于这次调用 |

`fetch`：

- 只能访问声明过的主机，重定向（最多 5 次）的每一跳也要在范围内。
- `body` 可以是字符串、`URLSearchParams` 或 `Uint8Array`；请求体和响应体各 ≤ 2 MB。
- 返回 `{ status, ok, url, headers, body, text(), json(), bytes(), arrayBuffer() }`，`body` 是文本；非 2xx 不会抛异常。
- `secretHeaders: { 请求头: "密钥名" }`：宿主把清单里声明的密钥填进请求头，脚本不经手明文。
- `stream: true`：`body` 是按块产出 `Uint8Array` 的异步迭代器，`events()` 按 SSE 解析出 `{ event, data, id }`。
- 可以在工具（非 `readOnly`）、异步钩子、页面动作、定时任务和常驻服务里用。

`websocket` 返回 `{ protocol, send(data), next(), close() }`，也可以 `for await (const message of socket)`；消息是
`{ type: "text", data }` 或 `{ type: "binary", data: Uint8Array }`，单条 ≤ 4 MB。

### 程序

| 成员 | 需要 | 说明 |
|---|---|---|
| `exec(program, args, { cwd, stdin, env, timeoutMs, signal })` | `exec:<程序>` | 不经 shell 启动程序，返回 `{ code, stdout, stderr, ok }`；输出各 ≤ 1 MB |
| `spawn(program, args, { cwd, env })` | `exec:<程序>` | 流式进程：`for await (const line of proc.lines())`、`next()`（`{ stream, line }`）、`write(text)`、`closeStdin()`、`kill()`、`wait()` |

只能在没有标 `readOnly` 的工具和页面动作里用。进程随这次调用结束。能启动程序的工具按 `run_shell` 一样的规则把关。

### 模型

| 成员 | 需要 | 说明 |
|---|---|---|
| `llm.complete(prompt \| { system, prompt, messages, maxTokens }, { signal })` | `llm` | 用当前会话的模型发一次性请求（不带工具），返回文本；每次调用最多 8 次 |

可以在工具和异步钩子里用。花的是用户的 token，只在任务确实需要理解语言时用。

### 会话与代办

| 成员 | 需要 | 说明 |
|---|---|---|
| `session.events({ kinds, last })` | `session:read` | 当前会话的事件，最新的在后；最多 500 条、1 MB |
| `session.id()` | `session:read` | 当前会话 id |
| `session.list()` | `session:read` 或 `session:control` | 打开着的会话 `[{ id, title, workspace, running, current }]` |
| `session.send(text, { session, workspace })` | `session:control` | 替用户发消息；`session` 是 `"current"`（默认）、`"new"` 或会话 id；返回会话 id。每个插件每 5 秒最多一条 |
| `session.continue(options)` | `session:control` | 等于发送 `continue` |
| `session.stop(session?)` | `session:control` | 停止会话（默认当前） |
| `agent.run({ task, preset })` | `agent:run` | 启动子代理，返回它的报告；`preset` 是 `standard` / `plan` / `review`；每次调用最多 4 个 |
| `notify({ title, body })` 或 `notify("标题")` | `notify` | 桌面通知；每个插件每 10 秒最多一条 |
| `composer.get()` / `set(text)` / `insert(text)` | — | 读写输入框；只在页面动作、命令、斜杠命令里可用 |

代办类操作（发消息、子代理）只能在没有标 `readOnly` 的工具、页面动作、异步钩子和常驻服务里用。命令行版只能发到当前会话，
不能新建会话。

### 第三方登录

需要 `oauth`。

| 成员 | 说明 |
|---|---|
| `oauth.authorize({ authorizeUrl, tokenUrl, clientId, clientSecret, scopes, extraParams, secret })` | 用系统浏览器走授权码 + PKCE 登录，令牌存进清单里声明的密钥 `secret`。`tokenUrl` 的主机必须在 `net:` 里 |
| `oauth.token(secret)` | 取 access token，快过期时自动用 refresh token 换新的 |

### host.dshr

读写 dshr 自己的数据。改东西的方法只能在可以代办的地方用（非 `readOnly` 的工具、页面动作、异步钩子），工具按“交给外部”把关。
API 密钥在钥匙串里，这里碰不到。

| 方法 | 需要 | 说明 |
|---|---|---|
| `config.get()` | `dshr:config:read` | 读 `config.toml` 的内容 |
| `config.patch(patch)` | `dshr:config:write` | 修改设置 |
| `skills.list()`、`skills.read(name)` | `dshr:skills` | 列出、读取全局 Skills |
| `skills.write(name, content)`、`skills.remove(name)` | `dshr:skills` | 写入（≤ 256 KB）、删除 |
| `sessions.list()`、`sessions.read(id, { last })` | `dshr:sessions:read` | 全部会话、读一个会话的事件（最多 2000 条） |
| `sessions.rename(id, title)`、`sessions.remove(id)` | `dshr:sessions:write` | 重命名、删除（移到 `.trash`） |
| `sessions.fork(id, turn)` | `dshr:sessions:write` | 从第 `turn` 轮结束处分叉，返回新会话 id |
| `sessions.import(data)` | `dshr:sessions:write` | 导入会话 |
| `mcp.list()` | `dshr:mcp` | `[{ name, command, enabled }]` |
| `mcp.add(name, { command, args })`、`mcp.remove(name)`、`mcp.setEnabled(name, enabled)` | `dshr:mcp` | 管理 MCP 服务器 |

### host.plugins

| 方法 | 需要 | 说明 |
|---|---|---|
| `list()` | `plugins:install` | 已安装的插件 |
| `install({ url, sha256 })` 或 `install(url)` | `plugins:install` | 下载并安装（≤ 50 MB）；**每次都由用户确认**，发布者变了时拒绝；返回插件 id。装好的插件能力仍要另外允许 |
| `uninstall(id)`、`setEnabled(id, enabled)` | `plugins:install` | 卸载、启停 |
| `call(id, method, args)` | 清单 `uses = ["<id>"]` | 调用其他插件的 `api` |

### 其他

| 成员 | 需要 | 说明 |
|---|---|---|
| `tools.set([{ name, description, inputSchema, readOnly, execute }])` | — | 整组替换插件的动态工具（最多 128 个），下一步起可用；不能和静态导出的工具重名 |
| `tools.list()` | — | 动态工具的名字 |
| `secrets.get(name)`、`secrets.has(name)` | 清单 `secrets` 里声明 | 密钥的值（没设置时 `null`）、是否设置 |
| `ui.refresh(page)` | — | 让打开着的页面重新渲染 |

## 在哪里能用什么

| 场合 | 读工作区 | 写工作区 | 联网 | 启动程序 | 调模型 | 代办 | 写事件 |
|---|---|---|---|---|---|---|---|
| 工具（非 `readOnly`） | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| 工具（`readOnly`） | ✓ | — | — | — | ✓ | — | ✓ |
| 页面动作、命令、斜杠命令 | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| 异步钩子（`afterTool`、`turnEnded` 等） | ✓ | — | ✓ | — | ✓ | ✓ | ✓ |
| 同步钩子（`beforeTool`、`beforeWrite`；`beforeRequest` 没有工作区） | ✓ | — | — | — | — | — | — |
| `prompt`、`render`、`project` / `label` | ✓ | — | — | — | — | — | — |
| `on`、`watch` 的处理函数 | ✓ | — | ✓ | — | — | — | — |
| 定时任务 | 数据目录 | 数据目录 | ✓ | — | — | — | — |
| 常驻服务 | 数据目录 | 数据目录 | ✓ | — | — | ✓ | — |

✓ 表示这个场合允许，前提是插件声明并被允许了对应的能力。读写自己的数据目录在任何场合都可以。

## 界面节点

页面、卡片和事件的 `render` 返回节点树：节点是带 `type` 的普通对象，直接写字符串等于一个 `text` 节点。每棵树最多 1000 个节点、嵌套 16 层。

| 类型 | 属性 |
|---|---|
| `column`、`row` | `children`、`gap`（`small` / `medium` / `large`） |
| `card` | `title`、`children`、`gap` |
| `text` | `text`、`style`（`title` / `heading` / `body` / `caption` / `mono`）、`tone`（`neutral` / `accent` / `success` / `danger`） |
| `markdown` | `text` |
| `code` | `text`、`language` |
| `diff` | `text`（统一 diff），或 `before` + `after`；`path` |
| `badge` | `text` |
| `divider` | — |
| `button` | `label`、`action`、`payload`、`variant`（`primary` / `outline` / `ghost` / `danger`） |
| `input` | `id`、`label`、`placeholder`、`value` |
| `textarea` | `id`、`label`、`placeholder`、`value`、`rows`（1–40） |
| `select` | `id`、`options`（字符串或 `{ value, label }`）、`label`、`value`、`action` |
| `switch` | `id`、`label`、`checked`、`action` |
| `link` | `label`、`url`（http / https；点击后经用户确认，用系统浏览器打开） |
| `list` | `items`：`[{ title, detail, action, payload }]` |
| `table` | `columns`（字符串数组）、`rows`（每行一个数组，每列一个字符串 / 数字 / 布尔 / `null`） |
| `kv` | `items`：`[{ key, value }]` |
| `progress` | `value`（0–1）、`label` |
| `chart` | `kind`（`line` / `bar` / `area` / `pie` / `candlestick`）、`series`：`[{ name, data }]`（`data` 是 `[x, y]`，K 线是 `[x, 开, 高, 低, 收]`；最多 20 组、每组 2000 点）、`height`（80–600） |
| `heatmap` | `rows`、`columns`（标签，各最多 100）、`values`（每行一个数字数组，可以有 `null`） |
| `calendar` | `month`（`2026-11`）、`marks`（`{ "2026-11-16": "说明" }`）、`action` |
| `image` | `src`（`data:image/…` URI，或相对路径，`data/…` 指插件数据目录；≤ 4 MB）、`alt`、`width`（16–1200） |

不认识的类型、缺少必填属性时整页渲染失败，错误信息里有节点路径。

## 能力

| 能力 | 允许 |
|---|---|
| `fs:workspace:read` | 读工作区：`readFile`、`readFileBytes`、`listDir`、`stat`、`exists`、`glob`、`grep` |
| `fs:workspace:write` | 写工作区：`writeFile`、`writeFileBytes`、`mkdir`、`remove`、`rename` |
| `net:<主机>` | `fetch` / `websocket` 访问该主机。`api.example.com`、`*.example.com`（子域）、`*`（任何主机）；通配不含 localhost 和 IP，本机和内网服务要逐个写明（IPv6 写 `[::1]`） |
| `exec:<程序>` | `exec` / `spawn` 启动该程序（只比程序名）；`exec:*` 任何程序，也允许带路径的写法 |
| `llm` | `llm.complete` |
| `session:read` | `session.events`、`session.id`、`session.list` |
| `session:control` | `session.send`、`continue`、`stop`、`list` |
| `agent:run` | `agent.run` |
| `hooks:gate` | `beforeTool` 直接放行 |
| `hooks:review` | `reviewTool` 钩子 |
| `hooks:tool-output` | `afterTool` 改写结果 |
| `hooks:write` | `beforeWrite` 改写内容 |
| `context:compact` | `beforeStep` 压缩上下文 |
| `context:rewrite` | `transformRequest` 钩子 |
| `model:route` | `beforeRequest` 换模型和思考等级 |
| `provider` | `providers` 导出 |
| `oauth` | `host.oauth` |
| `notify` | `host.notify` |
| `background` | `schedules`、`services`、`watch` |
| `dshr:config:read` / `dshr:config:write` | `host.dshr.config` |
| `dshr:skills` | `host.dshr.skills` |
| `dshr:sessions:read` / `dshr:sessions:write` | `host.dshr.sessions`；订阅其他插件和核心的会话事件 |
| `dshr:mcp` | `host.dshr.mcp` |
| `plugins:install` | `host.plugins.list` / `install` / `uninstall` / `setEnabled` |

派生的能力（不在 `capabilities` 里写）：

- `plugins:call:<id>`：由清单的 `uses` 派生；
- `mcp:<名字>@<哈希>`：由清单的 `[mcp.servers.<名字>]` 派生，启动参数一变哈希就变。

拥有 `session:control`、`agent:run`、`dshr:config:write`、`dshr:sessions:write`、`dshr:mcp`、`dshr:skills`、`plugins:install` 之一的插件，
其工具（非 `readOnly`）按“交给外部”把关。

## 限额

| 项目 | 上限 |
|---|---|
| 运行时内存 / 栈 | 64 MB / 512 KB |
| 顶层代码、`activate` | 2 秒 |
| 工具调用 | 默认 30 秒（清单 `timeout_ms`，可在配置里覆盖） |
| 页面渲染 | 3 秒 |
| 同时进行的宿主操作（`fetch`、`exec`…） | 每次调用 32 个 |
| 连续失控 | 3 次（超时或内存耗尽）后停用，直到插件文件变化 |
| 工作区 / 数据目录的单个文件 | 2 MB |
| 数据目录总大小 | 64 MB |
| `fetch` 请求体、响应体 | 各 2 MB |
| WebSocket 单条消息 | 4 MB |
| `exec` 输出 | 各 1 MB |
| `llm.complete` | 每次调用 8 次 |
| `agent.run` | 每次调用 4 个 |
| 动态工具 | 128 个 |
| 插件包：命令 / 预设 / MCP 服务器 | 50 / 20 / 16 个，单个文件 64 KB |
