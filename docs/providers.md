# 模型与提供商

dshr 通过“提供商”连接模型。每个提供商说的是两种线上协议之一：

- **Messages**：Anthropic Messages API 兼容，请求发到 `{地址}/v1/messages`；
- **OpenAI**：OpenAI Chat Completions 兼容，请求发到 `{地址}/chat/completions`。

内置了常用提供商的地址、环境变量和默认模型；也可以接入任意兼容这两种协议之一的接口。

## 内置提供商

| 适配器 | 名称 | 协议 | 默认地址 | 环境变量 | 默认模型 |
|---|---|---|---|---|---|
| `deepseek` | DeepSeek | Messages | `https://api.deepseek.com/anthropic` | `DEEPSEEK_API_KEY` | `deepseek-flash`（DeepSeek-V4.1-Flash）、`deepseek-v4-pro`（DeepSeek-V4-Pro） |
| `deepseek-oauth` | DeepSeek OAuth | Messages | `https://api.deepseek.com/anthropic` | —（用账号登录） | 同上 |
| `anthropic` | Anthropic | Messages | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` | 无，需获取或手动添加 |
| `openai` | OpenAI | OpenAI | `https://api.openai.com/v1` | `OPENAI_API_KEY` | 无 |
| `moonshot` | Kimi (Moonshot) | OpenAI | `https://api.moonshot.cn/v1` | `MOONSHOT_API_KEY` | 无 |
| `openrouter` | OpenRouter | OpenAI | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | 无 |
| `ollama` | Ollama | OpenAI | `http://localhost:11434/v1` | —（不需要密钥） | 无 |

没有默认模型的提供商，添加后在编辑页点“获取可用模型”，或手动添加模型 ID。

全新安装时，提供商列表里只有 DeepSeek，默认模型是 `deepseek/deepseek-flash`。

## 添加与编辑提供商

在“设置 → 模型”里：

1. **添加提供商**，二选一：
   - **第三方模型提供商**：从上表的内置目录里选；
   - **自定义模型 API**：填名称、协议（Messages 兼容 / OpenAI 兼容）和 API 地址，适合公司网关、自建推理服务、其他兼容服务。
2. **API 密钥**：填入后保存在系统钥匙串里，不写进配置文件。留空时读取该提供商的环境变量（界面会提示是否检测到）。
   编辑时留空表示保留已保存的密钥；“清除”删除它。
3. **API 地址**：留空用默认地址；改了地址后，“获取可用模型”会按协议从新地址推导模型列表接口。
4. **模型目录**：“获取可用模型”从接口拉取模型列表；也可以手动“添加模型”（模型 ID + 可选的显示名）或删除。
   用了自定义目录后，它完全代替适配器的默认模型。
   显示名优先用接口给的（如 Anthropic 的 `display_name`）。OpenAI 的模型列表接口和很多兼容网关只给 ID，这时 dshr 从 ID 推一个
   好读的名字（`claude-opus-4-5-20251101` → `Claude Opus 4.5`，`gpt-4o-mini` → `GPT-4o Mini`），可以直接改。
5. **新会话默认设置**：默认模型和默认思考等级。

对应的配置文件写法（见 [配置参考](configuration.md#providers)）：

```toml
[[providers]]
id = "openai"
adapter = "openai"
models = [{ id = "gpt-x" }, { id = "gpt-x-mini", name = "GPT X mini" }]

[[providers]]
id = "local-vllm"
adapter = "custom"
name = "本机 vLLM"
protocol = "openai"
base_url = "http://localhost:8000/v1"
models = [{ id = "qwen-coder" }]
```

模型用 `<提供商 id>/<模型 id>` 引用，例如 `deepseek/deepseek-v4-pro`、`openrouter/anthropic/claude-x`（模型 ID 里可以带 `/`，
只按第一个 `/` 切分）。

## 密钥从哪里来

每次连接提供商时按这个顺序找密钥：

1. 系统钥匙串里保存的（欢迎窗或“设置 → 模型”里填的），条目名 `<提供商 id>-api-key`；
2. 适配器约定的环境变量（如 `DEEPSEEK_API_KEY`）。

自定义提供商没有约定的环境变量，只能用钥匙串里保存的密钥。`ollama` 和插件提供的服务商不需要密钥。

各协议的鉴权方式：Messages 协议用 `x-api-key` 请求头（并带 `anthropic-version: 2023-06-01`），OpenAI 协议用
`Authorization: Bearer <密钥>`。

## DeepSeek 账号登录

除了 API 密钥，还可以用 DeepSeek 账号登录（欢迎窗或“设置 → 模型”里 DeepSeek 的编辑页）：

1. dshr 在本机回环地址上开一个临时端口，用系统浏览器打开 DeepSeek 平台的授权页（OAuth 授权码 + PKCE）；
2. 你在浏览器里批准后，平台跳回本机，dshr 换取令牌，最多等 10 分钟；
3. 令牌保存在系统钥匙串（条目 `deepseek-account-grant`），请求时放在 `x-dsh-auth-token` 头里。

登录成功后，dshr 会在配置里加上 `deepseek-oauth` 提供商，并把默认模型从 `deepseek/…` 切到 `deepseek-oauth/…`
（模型 ID 不变）。`deepseek`（API 密钥）和 `deepseek-oauth`（账号）两个提供商互不影响，可以在设置里改回。
“退出登录”删除本地保存的令牌。

> 账号登录目前只在桌面版里可用；命令行版请使用 API 密钥。

## 思考等级

思考等级（effort）控制模型在回答前思考多少：

| 等级 | Messages 协议 | OpenAI 协议 |
|---|---|---|
| 默认（不设置） | 不发送 | 不发送 |
| `off` | `thinking: {type: "disabled"}` | `reasoning_effort: "off"` |
| `low` / `high` / `max` | `thinking: {type: "adaptive"}` + `output_config: {effort}` | `reasoning_effort: "low"` / `"high"` / `"max"` |

“默认”时由模型自己决定。部分模型不支持关闭思考，或者不认识其中某些等级——这时请选“默认”。思考过程会以次要色显示在
回复前面（OpenAI 协议下读取 `reasoning_content`，DeepSeek、Kimi 等用它输出思考）。

设置位置：

- 当前会话：输入卡片的模型菜单 → 二级菜单；
- 新会话的默认值：“设置 → 模型 → 新会话默认设置”，即 `config.toml` 的 `[model] effort`；
- 命令行：`dshr headless --effort high`。

## 提示词缓存（Anthropic）

Anthropic 只缓存请求里显式标记过的前缀。对 Messages 协议的提供商，dshr 在系统提示词末尾和对话末尾打上 `cache_control`，
命中缓存的输入约按一折计费。`prompt_cache` 设置缓存写入后保留多久（每次命中重新计时）：

| 值 | 写入计费 | 适合 |
|---|---|---|
| `off` | — | 不标记缓存 |
| `5m` | 约 1.25 倍 | 连续对话（内置 Anthropic 的默认值） |
| `1h` | 约 2 倍 | 两次请求常隔 5 分钟以上：等审批、边看边改 |

- 自定义的 Messages 兼容接口默认 `off`：网关不一定认 `cache_control`，接的是 Claude 时可以手动打开。
- DeepSeek、OpenAI 协议的提供商由服务端自动缓存，不需要也不认这个设置。

为了让缓存尽量命中，dshr 让请求的前缀保持稳定：每轮会变的信息（日期、权限模式）不放进系统提示词，而是作为一条
`<environment>` 说明追加在对话里，只在变化时追加（见 [Agent 的工作方式](agent.md#模型看到什么)）。

## 插件提供的服务商

有 `provider` 能力的 JS 插件可以导出模型服务商：插件给出地址和鉴权（例如通过 OAuth 登录拿到的令牌），流式协议仍由
dshr 内置的客户端处理。它们出现在“设置 → 模型 → 插件提供的服务商”里，点一下加进提供商列表，之后和其他提供商一样编辑、
获取模型。配置文件里写作：

```toml
[[providers]]
id = "my-gateway"
adapter = "plugin"
plugin = "gateway-plugin/main"     # <插件 id>/<服务商 id>
```

插件的写法见 [插件 API 参考](plugin-api.md#providers)。

## 网络与重试

- 连接（含 TLS 握手）最多等 20 秒；流式响应两次收到数据之间最多等 300 秒（服务端在思考时会定期发保活）。
- 网络错误、HTTP 408 / 429 / 5xx、服务过载时自动重试，一次请求最多尝试 4 次。服务端给了 `Retry-After` 就照它等
  （最多 60 秒），否则按 1 秒、2 秒、4 秒……指数退避并加随机抖动。对话里会显示“第 N 次请求失败，正在重试”。
- 缺少密钥、请求格式错误（4xx）等不会重试，直接结束这一轮并显示原因。
- TLS 统一使用 rustls（AWS-LC 加密后端）。
