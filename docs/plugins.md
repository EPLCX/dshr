# 插件开发指南

插件让 dshr 多出新的本事：给模型新的工具、在对话流程上挂钩子、给界面加页面、接入新的模型服务商、换一套配色、在后台定时干活……
这篇从零讲到发布前的调试。每个导出项和 `host` 成员的完整签名见 [插件 API 参考](plugin-api.md)，打包与分发见
[插件的打包、签名与分发](plugin-publishing.md)。

## 三种做法

| 做法 | 适合 | 需要写代码吗 |
|---|---|---|
| **创造模式** | 你知道想要什么，让 agent 替你写 | 不需要。在预设菜单里选“创造模式”，描述功能即可（见 [Agent 的工作方式](agent.md#创造模式)） |
| **插件包** | 打包一组 Skills、提示词命令、预设，或者带一个 MCP 服务器 | 不需要，全是 Markdown 和 TOML（见 [插件包](#插件包)） |
| **JS 插件** | 新工具、钩子、界面、后台任务、模型服务商 | 写一个 ES 模块（见 [JS 插件](#js-插件)） |

三者可以组合：一个插件文件夹里既可以有 `index.js`，也可以有 `skills/`、`commands/`、`presets/` 和自带的 MCP 服务器。

## 五分钟上手

```bash
# 1. 生成骨架：plugin.toml + 带示例工具的 index.js，放在 ~/.dshr/plugins/word-count/
dshr plugin new word-count --description "统计工作区文件的字数" --cap fs:workspace:read

# 2. 打开调试台：保存文件就重新加载，可以直接调用工具
dshr plugin dev word-count
dev> tool word_count_line_count {"path": "Cargo.toml"}
Cargo.toml: 153 lines
```

把 `index.js` 改成真正的功能：

```js
export const tools = [{
  name: "word_count",
  description: "Count the words of a text file in the workspace.",
  inputSchema: {
    type: "object",
    properties: { path: { type: "string", description: "File path relative to the workspace" } },
    required: ["path"],
  },
  readOnly: true,
  async execute({ path }, host) {
    const text = await host.readFile(path);
    const words = text.split(/\s+/).filter(Boolean).length;
    return `${path}: ${words} words`;
  },
}];
```

保存后调试台自动重新加载；桌面版开着的话同样自动生效，下一步起模型就能调用 `word_count`。最后用 `dshr plugin list`
确认状态是“可用”。

## 插件放在哪里

- 插件目录是 `~/.dshr/plugins/`，每个子文件夹是一个插件，文件夹名默认就是插件 id。
- 开发中的插件可以放在任何地方，用 `dshr plugin link <文件夹>` 链接进来（不拷贝，记在 `plugin-links.json` 里）。
  插件目录里有同 id 的文件夹时，以插件目录里的为准。
- **工作区里的插件不会被加载**：克隆一个仓库不会顺带执行它带的代码。
- 插件的私有数据在 `~/.dshr/plugin-data/<id>/`，用 `host.readData` / `host.writeData` 读写。

插件 id 只能用小写字母、数字、`-`、`_`，以字母或数字开头，最长 64 个字符。它同时是 `[plugins.<id>]` 配置表的键、
事件命名空间和斜杠命令的前缀，发布后不要再改。

## 清单 plugin.toml

清单可以省略：那样 id 就是文件夹名，入口是 `index.js`，没有任何能力。需要能力、配置或密钥时就得写。

```toml
id = "word-count"
name = "字数统计"
version = "0.1.0"
description = "统计工作区文件的字数"
capabilities = ["fs:workspace:read"]
secrets = ["API_TOKEN"]          # 用户填写的密钥名（大写字母、数字、_），值不进文件
timeout_ms = 30000               # 单次工具调用的时限，默认 30 秒
default_locale = "zh-CN"         # 找不到界面语言的文案时用哪种语言，默认 en（见“多语言”）
uses = ["other-plugin"]          # 要调用的其他插件的 api

[engines]
dshr = ">=0.1"                   # 宿主版本要求（semver 范围），不满足时不激活

[imports]                        # 可选的 import map：裸模块名 → 插件文件夹里的文件
zod = "vendor/zod.js"

[config.properties.max_chars]    # 配置项的 JSON Schema：设置页据此生成表单
type = "integer"
default = 100000
title = "最多字符数"              # 表单里显示的名字（不写时显示键名 max_chars）
description = "最多统计多少个字符"

[config.properties.max_chars.i18n.en]   # 其它语言的标题和说明
title = "Max characters"
description = "Stop counting after this many characters"
```

| 键 | 说明 |
|---|---|
| `id` | 插件 id，不写时用文件夹名 |
| `name`、`version`、`description` | 显示名、版本（semver）、一句话说明 |
| `main` | 入口模块，默认 `index.js`；必须是插件文件夹里的相对路径 |
| `capabilities` | 声明的能力（见 [能力](#能力)） |
| `secrets` | 插件要用的密钥名 |
| `timeout_ms` | 单次工具调用的时限；用户可以在 `[plugins.<id>] timeout_ms` 里改 |
| `default_locale` | 插件的默认语言，默认 `en`；界面语言没有对应文案时用它 |
| `uses` | 要调用其 `api` 的其他插件 id（派生能力 `plugins:call:<id>`） |
| `[engines] dshr` | 要求的 dshr 版本 |
| `[imports]` | import map |
| `[config]` | 配置项的 JSON Schema（`type = "object"` 可以省略） |
| `[mcp.servers.<名字>]` | 插件自带的 MCP 服务器（见 [插件包](#插件包)） |

## JS 插件

### 运行环境

- `index.js` 是一个 **ES 模块**，跑在内嵌的 QuickJS 引擎里，每个插件一条线程、一个独立的运行时。
- 可以 `import "./lib/x.js"`、`import data from "./data.json"`（插件文件夹里的文件）、`[imports]` 里映射的名字，以及两个内置模块：
  `dshr:path`（POSIX 风格的路径函数）和 `dshr:events`（`EventEmitter`）。
- **没有** npm、`require`、`process`、`fs`，也没有全局的 `fetch`。需要第三方库时，把它打成一个单文件 ES 模块放进插件文件夹，
  再在 `[imports]` 里映射。
- 可用的全局：`setTimeout` / `setInterval` / `clearTimeout` / `clearInterval`（只在一次调用期间有效）、`queueMicrotask`、
  `TextEncoder` / `TextDecoder`（UTF-8）、`atob` / `btoa`、`URL` / `URLSearchParams`、`structuredClone`、
  `crypto.randomUUID()` / `getRandomValues()` / `subtle.digest("SHA-256", data)`、`AbortController` / `AbortSignal`、
  `console.log/info/warn/error/debug`（写进插件日志）。
- 脚本外面的一切都通过 `host` 完成：读写文件、联网、启动程序、调用模型……每一项都要对应的能力。`host` 是每个函数的
  最后一个参数（工具的 `execute(input, host)`、钩子的 `(payload, host)` 等）。

**执行模型**

- 同一个插件的调用**一次只执行一个**；模块级变量在两次调用之间保留，直到插件重新加载。
- 工具、所有钩子（同步的 `beforeTool` / `beforeWrite` / `beforeRequest` 和异步的 `afterTool` 等）、页面、命令都跑在**同一个运行时**里，
  模块变量彼此共享：同步钩子里攒的东西，异步钩子里直接读得到。只有常驻服务在单独的运行时里。
- 一次调用里，等待宿主的操作（`fetch`、`exec`、`llm.complete`……）可以用 `Promise.all` 并发。
- 定时器、没完成的 `fetch` / `exec` / `llm` 随发起它们的那次调用一起结束。
- 限额：64 MB 内存、512 KB 栈，顶层代码 2 秒内执行完；每次工具调用有自己的时限。连续 3 次失控（超时或内存耗尽），
  插件被停下，直到它的文件再次变化。

### 工具

```js
export const tools = [{
  name: "word_count",            // [A-Za-z0-9_-]，≤ 64 个字符；不能和已有工具重名，不能以 mcp__ 开头
  description: "Count the words of a file in the workspace.",
  inputSchema: { type: "object", properties: { path: { type: "string" } }, required: ["path"] },
  readOnly: true,                // 可选：保证不写、不联网、不启动程序
  async execute({ path }, host) {
    const text = await host.readFile(path);
    return `${text.split(/\s+/).filter(Boolean).length} words`;
  },
}];
```

- **返回值**：字符串原样交给模型；对象转成 JSON；`throw` 变成一条错误结果（模型能看到错误信息）。
  也可以返回 `{ content, images: [{ data, mediaType }] }` 交回图片（base64，PNG / JPEG / GIF / WebP，最多 4 张）。
- **实时输出**：`host.output(text)` 在工具卡片里实时显示进度（不进日志、不给模型）。
- **工具的访问类型由能力决定**，不由脚本说了算：插件有 `exec:` 能力的工具算“执行”，有 `net:` 或代办类能力的算“交给外部”，
  只有写能力的算“写入”，其余算“读取”。标了 `readOnly: true` 的工具一律算读取，任何权限模式下都直接执行——
  相应地，它在运行时就不能写文件、联网或启动程序。只读、纯计算的工具都应该标上 `readOnly`。
- **动态工具**：`host.tools.set([...])` 在运行时整组替换插件的动态工具（比如从 OpenAPI 描述生成），下一步起模型就能调用。

### 能力

插件能做的事由清单里的 `capabilities` 决定，每一项都要用户允许；声明了却没被允许的插件根本不激活。**只申请真正需要的**：

| 能力 | 允许什么 |
|---|---|
| `fs:workspace:read` | 读工作区的文件、列目录、glob、grep |
| `fs:workspace:write` | 写工作区的文件 |
| `net:<主机>` | `host.fetch` / `host.websocket` 访问这个主机；`*.example.com` 是子域，`*` 是任何主机（都不含 localhost 和 IP） |
| `exec:<程序>` | `host.exec` / `host.spawn` 启动这个程序（`git`、`npm`），`*` 是任何程序 |
| `llm` | `host.llm.complete` 发一次性请求、`host.llm.stream` 流式读取回复：默认用当前会话的模型，也可以指定别的已配置模型和思考等级（花的是用户的 token） |
| `session:read` | 读当前会话的事件、模型和预设，订阅模型的流式输出 |
| `hooks:gate` / `hooks:review` / `hooks:tool-output` / `hooks:write` | 钩子里放行工具调用 / 在询问用户前审查 / 改写工具结果 / 改写要写入的内容 |
| `context:compact` / `context:rewrite` | 压缩上下文 / 改写发给模型的消息 |
| `model:route` | 为每次请求选择模型和思考等级 |
| `background` | 定时任务、常驻服务、文件监视 |
| `session:control`、`agent:run`、`notify`、`oauth`、`provider` | 代发消息（可带图片）、切换会话的模型和预设、启动子代理、桌面通知、浏览器登录第三方账号、提供模型服务商 |
| `permissions:mode`、`permissions:approve` | 提供权限模式（选中后由插件决定每次工具调用）、代用户回答审批（见 [接管权限控制](#接管权限控制)） |
| `dshr:config:read/write`、`dshr:skills`、`dshr:sessions:read/write`、`dshr:mcp`、`plugins:install` | 读写 dshr 自己的设置、Skills、会话、MCP 服务器，安装插件 |

不需要任何能力的：纯计算、读写自己的数据目录（`host.readData` / `writeData` / `appendData`）和数据库（`host.db`）、zip 打包解包、
写会话事件、结束本轮、渲染页面、提示词、预设、命令、配色。
完整说明见 [插件 API 参考 · 能力](plugin-api.md#能力)。

### 配置与密钥

**配置**：清单里的 `[config]` 是配置项的 JSON Schema。设置页（“设置 → 扩展”的插件卡片）据此生成表单，值存在
`config.toml` 的 `[plugins.<id>]` 里，脚本里是只读的 `host.config`：

```js
const limit = host.config.max_chars ?? 100000;
```

配置项在表单里显示 `title`（如 `count` 显示成“数量”），下面一行是 `description`。两者都可以按界面语言切换，写法见下面的“多语言”。
字符串枚举的选项可以用 `enumTitles` 给出显示文字（存的仍是原来的值）：

```toml
[config.properties.level]
type = "string"
enum = ["low", "high"]
enumTitles = ["低", "高"]
title = "级别"

[config.properties.level.i18n.en]
title = "Level"
enumTitles = ["Low", "High"]
```

用 `oneOf = [{ const = "low", title = "低" }, …]` 的写法时，每一项的 `title` 就是它的显示文字。

### 多语言

插件可以自带各语言的文案：在插件文件夹里放 `locales/<语言>.json`，文件名是语言代码（和“设置 → 通用 → 语言”一致：
`zh-CN`、`zh-TW`、`zh-HK`、`en`、`de`、`el`、`ja`、`es`、`fr`、`pt`）。内容是 key → 文案，可以嵌套，嵌套的 key 用 `.` 连起来：

```jsonc
// locales/en.json
{
  "name": "Word count",
  "description": "Count the words of workspace files",
  "greeting": "Counted {count} words in {file}",
  "config": {
    "max_chars": { "title": "Max characters", "description": "Stop counting after this many characters" },
    "level": { "enum": { "low": "Low", "high": "High" } }
  }
}
```

- `name`、`description`：设置页插件卡片上的名字和说明，没写时用清单里的。
- `config.<键>.title`、`config.<键>.description`、`config.<键>.enum.<取值>`：配置项的标题、说明和选项文字，优先于清单里的
  `title` / `i18n` / `enumTitles`。
- 其余 key 随意，脚本里用 `host.i18n.t(key, vars)` 取（见 [插件 API 参考 · i18n](plugin-api.md#i18n)）：

```js
host.i18n.t("greeting", { count: 42, file: "a.md" });   // 英文界面：Counted 42 words in a.md
host.i18n.locale;                                       // 当前界面语言，如 "zh-CN"
host.i18n.pick({ "zh-CN": "统计字数", en: "Count words" }); // 不用语言文件，直接内联
```

找文案时按这个顺序回退：界面语言本身 → 同一语种的其它写法（繁体中文 `zh-HK`、`zh-TW` 之间互相优先，其次是 `zh-CN`）
→ `default_locale` → `en`；都没有时 `t` 返回 key 本身。语言文件改了会和别的文件一样触发热重载。

**密钥**：API 密钥、令牌**永远不要**写进文件或配置。在清单里声明名字，让用户去设置：

```toml
secrets = ["API_TOKEN"]
```

用户在“设置 → 扩展”的插件卡片上填写，或者 `dshr plugin secret set <id> API_TOKEN`。值存在系统钥匙串里；没有时读环境变量
`DSHR_PLUGIN_SECRET_<ID>_<名字>`。脚本里有两种用法：

```js
// 推荐：让宿主把密钥填进请求头，脚本不经手明文
const response = await host.fetch("https://api.example.com/items", {
  secretHeaders: { authorization: "API_TOKEN" },
});

// 必须拿到明文时
const token = await host.secrets.get("API_TOKEN");   // 没设置时是 null
```

密钥不会出现在 `host.config` 里；脚本把它打进日志时会被换成 `***`。

### 数据

插件自己的数据（待办事项、笔记、缓存、记住的设置）放在私有数据目录里，**不需要任何能力**，工具、页面、预览、设置页里指向的
都是同一个目录：

```js
const items = JSON.parse((await host.readData("items.json")) ?? "[]");
items.push(item);
await host.writeData("items.json", JSON.stringify(items));
```

单个文件不超过 16 MB，总共不超过 512 MB。日志、记录这类只往后加的数据用 `host.appendData(path, text)`，不用先读出整个文件。
要查询、更新、按条裁剪的记录，用数据目录里的 SQLite 数据库 `host.db`：

```js
await host.db.exec("CREATE TABLE IF NOT EXISTS changes (id INTEGER PRIMARY KEY, path TEXT, at INTEGER)");
await host.db.exec("INSERT INTO changes (path, at) VALUES (?, ?)", [path, Date.now()]);
const latest = await host.db.query("SELECT path FROM changes ORDER BY id DESC LIMIT 50");
```

数据目录和数据库在任何场合都能用，包括同步钩子。`readFile` / `writeFile`（以及 `fs:` 能力）只用来处理工作区里**用户的项目文件**。
要生成 docx、xlsx、epub 这类 zip 容器的文档，用 `host.zip` / `host.unzip`（见 [插件 API 参考 · zip](plugin-api.md#zip)）。

### 系统提示词与会话事件

**追加系统提示词**：`prompt` 导出每轮调用一次（最多 1 秒，只读），返回的文本追加到系统提示词后面：

```js
export const prompt = async ({ workspace }, host) =>
  "When the user asks about word counts, use the word_count tool.";
```

**写会话事件**：`host.emitEvent("<插件 id>/<名字>", data)` 往会话日志里写一条插件自己的事件。`events` 导出决定它怎么进模型上下文、
在对话区怎么显示：

```js
export const events = {
  "word-count/counted": {
    project: (data) => `[word count] ${data.path}: ${data.words}`,   // 进模型上下文的文字；返回 null 则不进
    label: (data) => `统计了 ${data.path}`,                           // 对话区里的一行
    // render: (data) => ({ type: "kv", items: [...] }),             // 或者画一个节点树代替 label
  },
};
```

事件名必须以插件 id 开头。插件停用或卸载后，它的事件仍留在日志里，只是不再进上下文。

### 钩子

`hooks` 导出把函数挂在 agent 循环上。返回 `undefined` 表示“没有意见”；超时、抛异常也都当作没有意见，记进插件日志，对话照常进行。
每个钩子的第二个参数都是 `host`；参数里都有 `session`（顶层会话 id）和 `agent`（`"main"` 或子代理名），按会话、按代理区分状态时用它们做键。

```js
export const hooks = {
  // 拦下删除迁移文件的命令
  beforeTool: ({ tool, input }) => {
    if (tool === "run_shell" && /rm\s+.*migrations/.test(input.command ?? "")) {
      return { decision: "deny", reason: "Never delete migration files." };
    }
  },
  // 每轮结束后记一笔
  turnEnded: async ({ reason, finalText }, host) => {
    const log = JSON.parse((await host.readData("turns.json")) ?? "[]");
    log.push({ at: Date.now(), reason, chars: finalText?.length ?? 0 });
    await host.writeData("turns.json", JSON.stringify(log.slice(-100)));
  },
};
```

| 钩子 | 时机 | 等多久 | 能做什么（括号里是需要的能力） |
|---|---|---|---|
| `beforeTool` | 工具调用把关时 | 同步，300 ms | 拒绝、要求询问；直接放行（`hooks:gate`） |
| `reviewTool` | 本来要问用户时 | 20 秒 | 给出 allow / deny（`hooks:review`） |
| `afterTool` | 工具执行后 | 异步 | 看结果、写事件；改写结果（`hooks:tool-output`） |
| `beforeWrite` | `write_file` / `edit_file` 写盘前 | 同步，1 秒 | 拒绝；改写内容（`hooks:write`） |
| `turnStarted` | 用户消息写进日志前 | 5 秒 | 拦下这条消息、补事件 |
| `beforeStep` | 每一步请求模型前 | 异步 | 写事件；压缩上下文（`context:compact`） |
| `beforeRequest` | 构造请求时 | 同步，300 ms | 往系统提示词后追加文字、往消息末尾补一段 user 文本；换模型和思考等级（`model:route`） |
| `transformRequest` | 发出请求前 | 5 秒 | 改写系统提示词和消息（`context:rewrite`） |
| `turnEnded` | 一轮结束后 | 异步 | 写事件、写数据、联网、调模型 |
| `approve` | 真的要问用户时 | 10 分钟，与审批卡片同时等 | 代用户允许或拒绝（`permissions:approve`，见 [接管权限控制](#接管权限控制)） |

“同步”是说 agent 循环在原地等它（最多等表里的时间），钩子本身照样可以 `async`、`await` 读写数据目录和 `host.db`。

钩子**不能写工作区、不能启动程序**——这些操作要经过权限把关，而钩子不在把关的范围里。各钩子的参数和返回值见
[插件 API 参考 · hooks](plugin-api.md#hooks)。

**结束本轮**：工具或 `afterTool` 钩子里调用 `host.session.endTurn()`，这一步的工具调用执行完后本轮就结束，不再请求模型——
适合“工具已经给出最终结果、模型不必再说话”的场合。

### 会话的模型、预设与代发消息

读当前会话的设置需要 `session:read`（或 `session:control`），改它们、替用户发消息需要 `session:control`：

```js
const { model, effort } = await host.session.model();      // { model: "deepseek/deepseek-flash", effort: null }
const preset = await host.session.preset();                // "standard" / "plan" / "review" / "create" / "<插件>/<预设>"
const permission = await host.session.permission();        // "workspace" 等，插件提供的模式是 "<插件>/<模式>"
const models = await host.session.models();                // [{ model: "provider/model", provider, name, providerName }]

await host.session.setModel("deepseek/deepseek-pro", { effort: "high" });   // 只写模型名时沿用当前提供商
await host.session.setModel(null, { effort: null });                        // 只把思考等级改回默认
await host.session.setPreset("plan");                                       // 或 "<插件 id>/<预设 id>"

// 带图片发消息：data 是 base64 字符串或 Uint8Array，PNG / JPEG / GIF / WebP，最多 4 张、每张 2 MB
await host.session.send("看看这张截图", { images: [{ data: bytes, mediaType: "image/png", name: "shot.png" }] });
```

- 不指定会话时：在一轮里调用的是这一轮的会话，其余场合是界面上当前的会话；指定时写 `{ session: "<id>" }`。
- 改动从**下一轮**起生效，并像在菜单里选的一样记进会话日志。模型必须是“设置 → 模型”里已经配置的，预设必须存在；
  插件不能把会话切换到创造模式。
- 命令行版只能读，不能切换（模型和预设由命令行参数决定）。

### 订阅会话事件

`on` 导出在事件写进日志时收到通知（不阻塞 agent，排队处理，每个插件最多排 256 件）：

```js
export const on = {
  "tool/result": async ({ session, seq, data }, host) => { /* … */ },
  "word-count/*": (event, host) => { /* 自己命名空间里的全部事件 */ },
};
```

自己命名空间里的事件不需要能力；其他事件（包括 `tool/result` 这类核心事件）需要 `dshr:sessions:read`。处理函数可以读工作区、
联网、读写数据目录、刷新页面，不能写工作区、启动程序、调用模型、再写会话事件。

**流式输出**：模型逐字输出的内容不进日志，但可以订阅（需要 `session:read`）：

```js
export const on = {
  "stream/text": ({ session, text }, host) => { /* 一段正文增量 */ },
  "stream/thinking": ({ session, text }, host) => { /* 一段思考增量 */ },
  "stream/end": ({ session }, host) => { /* 这次请求的输出结束（完成、中断或失败） */ },
  // 或者 "stream/*" 一次订阅全部
};
```

增量按会话合并，约每 100 毫秒（或攒够 4 KB）交给处理函数一次，正文和思考的先后顺序不变。`"*"` 收不到这些通知，必须明确订阅
`stream/…`。插件自己发起的请求要流式读取时用 `host.llm.stream`：

```js
const stream = host.llm.stream({ system: "Be brief.", prompt: text });
for await (const { type, text } of stream) {        // type 是 "text" 或 "thinking"
  if (type === "text") await host.appendData("reply.txt", text);
}
// 或者：const full = await host.llm.stream("hi").text();
```

### 后台：定时任务、常驻服务、文件监视

都需要 `background` 能力，运行时没有工作区（文件函数指向插件的数据目录，监视除外）。

```js
// 每 10 分钟同步一次（最短 10 秒）
export const schedules = [{ id: "sync", every: "10m", run: async (host) => { /* fetch、writeData… */ } }];

// 一直运行的服务：守着一个 WebSocket、长轮询 IM 接口……出错后按退避重启
export const services = [{ id: "bridge", run: async (host) => {
  const socket = await host.websocket("wss://im.example.com/stream");
  for await (const message of socket) { /* host.session.send(…) 需要 session:control */ }
} }];

// 工作区文件变化（还需要 fs:workspace:read）
export const watch = [{ id: "docs", glob: "**/*.md", run: async ({ workspace, changes, overflow }, host) => {} }];
```

常驻服务在一个单独的运行时里执行（同一份代码再加载一次），不会占着工具的运行时；它和工具不共享模块变量，要共享状态请放数据目录。
连续 5 次在 1 分钟内失败的服务会停下，原因显示在插件卡片上。

### 和其他插件协作

插件可以导出 `api` 给别的插件调用：

```js
// 提供方 issues 插件
export const api = { search: async ({ query }, host) => [/* … */] };
```

```toml
# 调用方的 plugin.toml
uses = ["issues"]
```

```js
// 调用方
const found = await host.plugins.call("issues", "search", { query: "crash" });
```

被调用的方法只能做调用方这次调用本来就能做的事，不会借到额外的权限。

### 生命周期与热重载

```js
export const activate = async (host) => { /* 加载后执行（≤ 2 秒）；抛异常 = 插件启动失败 */ };
export const deactivate = async (host) => { /* 热重载替换它、或者被移除之前 */ };

let items = [];
export const state = {
  snapshot: () => ({ items }),            // 热重载前保存
  restore: (saved) => { items = saved.items; },   // 新版本加载后恢复
};
```

- 桌面版每隔约 2 秒检查插件目录（包括链接的文件夹），文件变了就重新加载这个插件，从下一步起生效；命令行版下次运行时生效。
- 新版本加载失败时，继续使用旧版本，插件卡片上显示“新版本加载失败，仍在使用旧版本”和原因。
- 清单里多出没允许过的能力时，新版本不激活，直到用户允许。

### 模型服务商与配色方案

- `providers`（需要 `provider` 能力）：导出一个模型服务商——给出地址、协议和鉴权，流式协议由 dshr 内置的客户端处理。
  用户在“设置 → 模型 → 插件提供的服务商”里把它加进提供商列表。
- `themes`（不需要能力）：导出配色方案，和 `~/.dshr/themes/` 里的方案一起出现在“配色方案”里。

写法见 [插件 API 参考](plugin-api.md#providers)。

## 界面扩展

插件可以给 dshr 加界面：侧边栏里的页面、设置里的一页、输入框上方的面板、侧边栏底部的状态行。页面是**声明式**的：`render(host)`
返回一棵由普通对象组成的节点树，dshr 用自己的组件把它画出来（所以插件的页面和内置页面长得一样，也自动支持深色模式、高对比度、
键盘和读屏）。不能返回 HTML。

```js
const items = [];                                   // 模块状态在两次调用之间保留

export const ui = {
  pages: {
    main: {
      render: (host) => ({ type: "column", children: [
        { type: "text", text: `${items.length} 项`, style: "heading" },
        { type: "row", children: [
          { type: "input", id: "title", placeholder: "新的待办", value: "" },
          { type: "button", label: "添加", action: "add", variant: "primary" },
        ]},
        { type: "list", items: items.map((title, i) => ({ title, action: "remove", payload: i })) },
      ]}),
      actions: {
        // payload = { value: 节点的 payload, inputs: { 输入框 id: 文字 }, checked?: 开关的新状态 }
        add: ({ inputs }) => { items.push(inputs.title); return "已添加"; },   // 返回的字符串显示为提示
        remove: ({ value }) => { items.splice(value, 1); },
      },
    },
  },
  sidebar: [{ id: "todo", label: "待办", icon: "star", page: "main" }],    // 侧边栏入口，以弹窗打开页面
  settings: [{ id: "todo", label: "待办", page: "main" }],                 // 设置里的一页
  // panels: [{ id, label, page }]   输入框上方可折叠的面板
  // status: [{ id, page }]          侧边栏底部的状态行（取页面的第一段文字）
  // hide: ["composer:presets"]      藏起内置入口
};
```

- **重新渲染**：每次动作之后页面重新渲染。输入框保留用户输入的内容，除非 `render` 给它返回了不同的 `value`（比如添加后返回 `""` 清空）。
  工具、钩子改了数据之后，用 `host.ui.refresh("main")` 让打开着的页面重画。
- **`render` 是只读的**（和 `prompt` 一样）；**动作**可以直接使用插件的能力，不再询问——因为是用户自己点的。
- **没有打开的工作区时**（设置页、调试预览）`readFile` / `writeFile` 指向插件的数据目录。插件自己的状态放 `readData` / `writeData`，
  就不会依赖当前打开的是哪个工作区。
- **节点类型**（直接写一个字符串等于一个 `text` 节点；每个节点的属性见 [插件 API 参考 · 界面节点](plugin-api.md#界面节点)）：

  | 类别 | 节点 |
  |---|---|
  | 布局 | `column`、`row`、`grid`、`spacer`、`card`、`collapsible`、`tabs`、`section`、`split` |
  | 文字与展示 | `text`、`markdown`、`code`、`diff`、`badge`、`divider`、`stat`、`alert`、`empty`、`status`、`skeleton` |
  | 数据 | `list`、`table`、`kv`、`progress`、`chart`、`heatmap`、`calendar`、`image`、`steps`、`timeline`、`tree`、`avatar`、`tags` |
  | 控件 | `button`、`link`、`input`、`textarea`、`number`、`select`、`radio`、`checkbox`、`checkboxes`、`slider`、`date`、`tags-input`、`switch` |

  所有节点都可以写 `hidden`、`disabled`、`tooltip`、`icon`、`tone`；多数控件和部分展示节点还有 `size`（`small` / `medium` / `large`），
  写在 `row` 上时是其中控件的默认尺寸。
- **图标**：star、heart、bell、book、bot、calendar、chart、cpu、file、folder、globe、inbox、map、mic、moon、sun、network、palette、
  search、settings、terminal、user、info、check、x、plus、minus、trash、copy、refresh、external、warning、alert、success、error、
  play、pause、eye、chevron-right、chevron-down、arrow-up、arrow-down。
- **可以藏起来的内置入口**：`settings:<页 id>`（“扩展”页除外，如 `settings:general`、`settings:models`）、`composer:presets`、
  `composer:permission`、`sidebar:<id>`、`status:<id>`。
- **运行时显示或隐藏入口**：`ui.hide` 是加载时的初始状态；之后随时可以用 `host.ui.hide(target)`、`host.ui.show(target)` 改，
  界面立即跟着变，`host.ui.hidden()` 列出现在藏着的。`target` 先按插件**自己的**入口找——`settings:<id>`（设置里的一页）、
  `sidebar:<id>`（侧边栏入口）、`panel:<id>`（输入框上方的面板）、`status:<id>`（状态行）——找不到时按上面可以藏的内置入口处理。
  不需要任何能力。插件热重载后回到 `ui.hide` 的状态，要保持时在 `activate` 里再调用一次。

  ```js
  export const activate = async (host) => {
    if (!(await host.readData("enabled"))) await host.ui.hide("panel:quick-note");   // 用户没开启时先不显示面板
  };
  ```

### 工具卡片

`cards` 导出给插件**自己的**工具画专门的卡片，代替通用的显示：

```js
export const cards = {
  word_count: {
    title: (input, output) => input.path,
    render: (input, output, isError) => output === null
      ? { type: "progress", value: 0.5, label: "统计中…" }      // 运行中时 output 是 null
      : { type: "kv", items: [{ key: "文件", value: input.path }, { key: "结果", value: output }] },
  },
};
```

### 命令、斜杠命令与提及

```js
// 输入框里的 /todo-add 买牛奶
export const slash = [{ name: "todo-add", description: "添加待办", run: async ({ args }, host) => {
  /* … */ return "已添加";
} }];

// 命令：出现在 / 菜单里（/<插件 id>:<id>），可以有默认快捷键
export const commands = [{ id: "open", title: "打开待办", keys: "ctrl-shift-t", run: async (_, host) => ({ open: "main" }) }];

// 输入框 @ 菜单的来源
export const mentions = [{
  id: "todo", label: "待办",
  search: async (query, host) => [{ id: "1", title: "买牛奶", detail: "今天" }],
  resolve: async (id, host) => ({ title: "买牛奶", text: "待办事项：买牛奶" }),
}];
```

命令和斜杠命令的返回值决定界面做什么：字符串或 `{ message }` 显示一句话；`{ insert }` 替换输入框里的文字；`{ send }` 直接发送；
`{ open: "<页>" }` 打开插件的页面。`host.composer.get/set/insert` 也可以直接读写输入框。

### 预设

```js
export const presets = [{
  id: "docs-writer",
  label: "文档模式",
  description: "只写文档，不改代码",
  instructions: "You only edit Markdown files under docs/. Never modify source code.",
  readOnly: false,
  tools: ["read_file", "grep", "glob", "edit_file", "write_file"],   // 可选：限定模型能看到的工具
}];
```

预设出现在输入卡片预设菜单里内置四项的后面，不需要任何能力。

### 接管权限控制

**提供权限模式**（需要 `permissions:mode`）：导出的模式排在权限菜单内置四项的后面。用户选中后，每次工具调用由 `decide` 下结论：

```js
export const permissions = [{
  id: "office-hours",
  label: "工作时间放行",
  description: "工作时间内工作区的修改直接执行，其余询问",
  icon: "calendar",
  decide: ({ tool, input, access }, host) => {
    if (access.type === "read") return "allow";
    const hour = new Date().getHours();
    if (access.type === "write" && hour >= 9 && hour < 18) return "allow";
    if (tool === "run_shell" && /rm\s+-rf/.test(input.command ?? "")) return { decision: "deny", reason: "不允许 rm -rf" };
    return "ask";                       // 也可以 { decision: "ask", reason: "…" }，理由显示在审批卡片上
  },
}];
```

- `decide` 同步执行，最多等 300 ms，和 `beforeTool` 一样只能读工作区和数据目录。返回 `undefined`、超时或出错时按“手动确认”处理。
- 预设的只读规则、创造模式的规则和“必须亲自确认”的操作照常生效；提供模式的插件停用后，会话退回内置的权限模式。

**代为审批**（需要 `permissions:approve`）：真的要问用户时，`hooks.approve` 和审批卡片同时等待，先给出结论的生效：

```js
export const hooks = {
  approve: async ({ tool, input, access, note }, host) => {
    const answer = await askOnPhone(host, `允许 ${tool} 吗？`);   // 例如经 IM 机器人转发，等用户回复
    if (answer === "yes") return "allow";
    if (answer === "no") return { decision: "deny", reason: "在手机上拒绝" };
    // 返回 undefined：放弃，继续等界面上的审批卡片
  },
};
```

最多等 10 分钟；结论写进会话日志（对话区显示“插件 X 代为批准了 …”）。“必须亲自确认”的操作（授予插件能力等）不会交给插件。
等待期间这个插件的其他调用排队（同一个插件一次只执行一个调用）；用户在界面上先回答时，这次 `approve` 随即被中断。
完整的把关顺序见 [权限与安全](permissions.md#把关的顺序)。

## 插件包

一个插件文件夹里除了脚本，还可以带上这些（都不需要写代码，也可以完全没有 `index.js`）：

```text
my-plugin/
├── plugin.toml
├── index.js                     可选
├── skills/<名字>/SKILL.md       Skills，在提示词里叫 <插件 id>:<名字>
├── commands/<名字>.md           提示词命令 /<插件 id>:<名字>
└── presets/<id>.md              预设
```

**Skills**：写法和普通 Skill 一样（见 [项目说明与 Skills](instructions-and-skills.md#skills)）。

**提示词命令** `commands/<名字>.md`：

```markdown
---
description: 为指定模块写单元测试
argument-hint: <模块路径>
mode: insert
---
为 $ARGUMENTS 写单元测试：覆盖正常路径和边界条件，沿用项目现有的测试风格，写完运行一遍。
```

`$ARGUMENTS` 换成用户在命令后面写的文字（模板里没有 `$ARGUMENTS` 时接在最后）。`mode: insert`（默认）把结果放进输入框，
`mode: send` 直接发送。每个插件最多 50 个命令，单个文件不超过 64 KB。

**预设** `presets/<id>.md`：

```markdown
---
label: 安全审计
description: 只读地检查安全问题
readOnly: true
tools: [read_file, grep, glob]
---
Audit the code for security issues: injection, unsafe deserialization, secrets in code…
```

**自带的 MCP 服务器**（清单里）：

```toml
secrets = ["SEARCH_TOKEN"]

[mcp.servers.search]
command = "node"
args = ["${plugin}/server/index.js"]
env = { TOKEN = "${secret:SEARCH_TOKEN}", CACHE_DIR = "${data}/cache" }
```

写法和 `config.toml` 里的 MCP 服务器一样（见 [MCP 服务器](mcp.md#配置)），另外可以用 `${plugin}`（插件文件夹）、`${data}`
（插件数据目录）、`${secret:NAME}`（清单里声明的密钥）。每个服务器派生一项能力 `mcp:<名字>@<启动参数的哈希>`，用户要单独允许，
启动参数一变就要再允许。这样的服务器在沙箱外以用户的权限运行，所以**能用 JS 工具实现的，优先用 JS 工具**。每个插件最多 16 个服务器。

`dshr plugin new <id> --bundle` 会生成 `skills/`、`commands/` 的示例和注释掉的 `[mcp.servers]`。

## 调试

| 手段 | 用法 |
|---|---|
| `dshr plugin dev [<文件夹或 id>]` | 调试台：保存即重新加载并打印报告；`tool <名字> <JSON>` 调用工具，`page <页> [<动作> <JSON>]` 渲染页面或执行动作，`logs` 看日志，`grant` 允许能力 |
| `dshr plugin list` | 像真正运行时一样装配一遍，显示每个插件的状态和原因 |
| 插件卡片 | “设置 → 扩展”里：状态、未允许的能力、密钥、常驻服务状态、“日志”（`console.*`、`host.log` 和工具错误，保留最近 200 行） |
| 创造模式 | `plugin_reload` 报告加载结果（语法错误带行号）、导出的工具、等授权的能力，并把每个页面渲染一遍；`plugin_page` 像用户一样操作页面 |

常见的加载错误：

- `tool X is defined twice`、工具名和已有工具冲突（包括内置工具和 `delegate`）：换一个名字。
- `ui.sidebar entry … opens page …, which ui.pages does not define`：入口指向了不存在的页面。
- 节点树不合规：报错里有节点路径（如 `page.children[2] (button): needs a string label and action`）。
- 状态“等待 …”：清单里有没允许过的能力，`dshr plugin grant <id>` 或在插件卡片上点“允许”。

## 限制一览

| 项目 | 上限 |
|---|---|
| 内存 / 栈 | 64 MB / 512 KB |
| 顶层代码、`activate` | 2 秒 |
| 工具调用 | 默认 30 秒（`timeout_ms`） |
| `prompt` | 1 秒，结果最多 32 KB |
| 同步钩子（`beforeTool`、`beforeRequest`）、权限模式的 `decide` | 300 ms |
| `approve` 钩子 | 10 分钟 |
| 读写工作区文件 | 单个 2 MB |
| 数据目录 | 单个文件 16 MB，共 512 MB（含数据库；单个数据库 256 MB） |
| `fetch` 请求 / 响应体 | 各 2 MB（`stream: true` 时逐块读取）；WebSocket 单条消息 4 MB |
| `llm.complete`、`llm.stream` | 每次调用合计最多 8 次 |
| 代发消息的图片 | 每条最多 4 张，单张 2 MB |
| 页面节点 | 最多 1000 个，嵌套 16 层 |
| 失控 | 连续 3 次超时或内存耗尽后停用，直到文件变化 |

## 下一步

- 每个导出项、`host` 成员和节点属性的完整说明：[插件 API 参考](plugin-api.md)。
- 打包、签名、发布到插件索引：[插件的打包、签名与分发](plugin-publishing.md)。
- 权限模型的全貌：[权限与安全](permissions.md#插件的能力与授权)。
