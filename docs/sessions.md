# 会话与日志格式

dshr 的每个会话都是一份**只追加**的 JSONL 日志：每发生一件事就追加一行，写完立即落盘，从不改写已有的行。
日志是会话的唯一事实来源——界面显示的内容、发给模型的上下文、侧边栏的标题，全都是从日志里推出来的。

## 文件布局

```text
~/.dshr/sessions/
├── 0d6c1f4e-…-e21/                  会话 id（UUID v4）
│   ├── session.v1.jsonl             会话日志
│   └── agents/
│       └── call_01HX….jsonl         这个会话里子代理（delegate）的日志，按工具调用 id 命名
└── .trash/                          删除的会话移到这里，可以手动找回
```

文件名 `session.v1.jsonl` 与原版 DeepSeek Harness 的 v1 日志相同。读取日志的代码也认 `.zstd` / `.zst` 压缩的日志
（如 `session.v1.jsonl.zstd`，例如插件读取或导入会话时），但不能往压缩的日志里追加；侧边栏和 `dshr sessions` 只列出
未压缩的 `session.v1.jsonl`。

## 一行事件

```json
{"seq":12,"time":1763600000123,"type":"tool/call","data":{"call_id":"call_1","name":"read_file","input":{"path":"README.md"}}}
```

| 字段 | 说明 |
|---|---|
| `seq` | 从 1 开始递增的序号 |
| `time` | Unix 毫秒时间戳 |
| `type` | 事件类型 |
| `data` | 事件内容，形状由类型决定 |

读日志时：不认识的类型（插件事件、原版新增的事件）照样保留；不是事件形状的行、内容对不上的核心事件也原样保留，
不会因为一行坏数据读不出整个会话。空行跳过。

## 核心事件

| 类型 | `data` | 说明 |
|---|---|---|
| `session/workspace` | `path` | 会话的工作目录，第一轮开始时写入；侧边栏按它把会话归到工作区下 |
| `system/message` | `text` | 系统提示词，会话开始时写入一次 |
| `turn/start` | `turn` | 一轮开始 |
| `user/message` | `text` | 你的消息（含插队的消息） |
| `user/attachment` | `path`，`content`（文本）或 `media_type` + `data`（图片，base64） | 随上一条消息发送的附件；内容在发送时读入 |
| `step/start` | `turn`、`step` | 一步开始 |
| `request/header` | `provider`、`model` | 这一步请求的提供商和模型 |
| `assistant/attempt` | `attempt`、`error` | 一次失败的模型请求（之后会重试） |
| `assistant/message` | `model`、`content`、`stop_reason`、`usage` | 模型的回复：`content` 是内容块数组（文本、思考、工具调用） |
| `tool/call` | `call_id`、`name`、`input` | 一次工具调用 |
| `tool/result` | `call_id`、`output`、`is_error` | 工具调用的结果 |
| `turn/continue` | `reason`（`max_tokens`） | 回复被输出上限截断，自动接着请求 |
| `step/end` | `turn`、`step` | 一步结束 |
| `turn/end` | `turn`、`reason`、`error` | 一轮结束；`reason` 是 `completed` / `interrupted` / `error`（旧日志里可能有 `max_steps`） |
| `context/compaction` | `summary`、`upto_seq` | 上下文压缩：`upto_seq` 及之前的对话由摘要代替，原事件仍保留 |
| `session/title` | `title` | 会话标题（模型生成或手动重命名），以最后一条为准 |
| `session/options` | `preset`、`plugin_preset`、`permission`、`model_provider`、`model` | 输入卡片上选的预设、权限、模型；只用于重新打开会话时恢复界面，不影响上下文 |
| `session/forked` | `from`、`turn` | 这个会话是从哪个会话的第几轮分叉出来的 |

`session/options` 里的权限模式是 `manual` / `workspace` / `auto_review` / `full_access`；旧版本的 `read_only` 按 `manual` 读。

## 插件事件

插件和部分内置功能会写自己的事件，类型写成 `命名空间/名字`（只用小写字母、数字、`-`、`_`、`.`），命名空间一般是插件 id。
核心事件用的命名空间（`turn`、`step`、`system`、`user`、`assistant`、`tool`、`request`、`session`、`context`）不能用。

内置功能写的两种：

| 类型 | 说明 |
|---|---|
| `environment/turn` | `{ date, permission }`：当前日期（UTC）和权限模式，只在变化时写入，投影成一条 `<environment>` 说明 |
| `media/images` | 工具交回的图片，投影成图片内容块 |

插件事件怎么进模型上下文由插件的 `events` 导出决定（见 [插件 API 参考](plugin-api.md#events)）。插件停用或卸载后，它的事件仍留在日志里，
只是不再参与投影、界面用通用的方式显示。

## 从日志到模型上下文

每次请求模型前，dshr 把日志“投影”成一组消息（对应原版的 `deriveMessages()`）：

1. **系统提示词**：所有 `system/message` 的文字（之后再追加本轮的运行环境、`AGENTS.md`、Skills 等，见 [Agent 的工作方式](agent.md#模型看到什么)）。
   老会话里的旧版默认提示词会在请求时换成新版，日志本身不动。
2. **压缩**：有 `context/compaction` 时，先放一条“较早的对话已压缩”的摘要，`upto_seq` 之前的事件跳过（系统消息除外）。
3. **对话**：
   - `user/message` → 用户文字；文本附件 → `<attachment path="…">内容</attachment>`；图片附件 → 图片块；
   - `assistant/message` → assistant 消息；
   - `tool/result` → 对应调用的结果；**相邻的用户内容**（工具结果 + 新输入）合并成同一条 user 消息；
   - 被中断、没有结果的工具调用补一条错误结果（“Tool call was interrupted before it produced a result.”），保证请求合法；
   - `turn/continue` → 一段提示模型从截断处继续的文字；
   - 插件事件 → 交给插件的投影函数，得到的文字接在 user 内容里（同一步还有工具调用没出结果时，等结果齐了再接上）。

因为上下文完全由日志推出，**同一份日志总能得到同样的上下文**：重新打开会话、在别的机器上打开、从某一轮分叉，模型看到的都是一致的历史。

## 会话操作

| 操作 | 在日志层面做了什么 |
|---|---|
| 新建 | 新建 `sessions/<新 id>/session.v1.jsonl` |
| 继续（`--resume`、侧边栏打开） | 读取已有日志，从最大的 `seq` 之后接着追加 |
| 重命名 | 追加一条 `session/title` |
| 删除 | 把会话文件夹移到 `sessions/.trash/`（重名时加时间后缀） |
| 分叉（“从这里分叉”） | 新建一个会话：开头一条 `session/forked`，然后复制原会话到第 N 轮 `turn/end` 为止的事件（不复制标题和压缩记录），标题是“原标题 · 分支” |
| 导出（“分享”） | 把一轮的用户消息和回复整理成 Markdown 文件，不改日志 |

只能从已经结束的一轮分叉。

## 列出会话

侧边栏和 `dshr sessions` 按日志的修改时间倒序列出会话。标题取最后一条 `session/title`，还没有时取第一条用户消息的第一行
（最多 60 个字符），都没有时显示“（空会话）”。为了快，列表只解析标题、工作目录和第一条用户消息这几种行；桌面版还会记住每个日志上次的大小和修改时间，
只重读变化了的日志。

## 直接处理日志

日志就是普通的 JSONL，可以用常见工具查看：

```bash
# 某个会话里调用过的工具
jq -r 'select(.type=="tool/call") | .data.name' ~/.dshr/sessions/<id>/session.v1.jsonl | sort | uniq -c

# 某个会话每一步的用量
jq -c 'select(.type=="assistant/message") | .data.usage' ~/.dshr/sessions/<id>/session.v1.jsonl

# 每一轮的结束原因
jq -c 'select(.type=="turn/end") | .data' ~/.dshr/sessions/<id>/session.v1.jsonl
```

注意：

- **不要手动修改正在运行的会话的日志。** 需要改动时先停止会话；改坏的行会被保留但不参与投影。
- 日志里有你发出的附件内容和全部工具输出，分享会话前请检查是否包含敏感信息。
- 会话日志不含 API 密钥。
