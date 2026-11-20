# 项目说明（AGENTS.md）与 Skills

两种不写代码就能“教” agent 的方式：

- **AGENTS.md**：写给 agent 的说明书——项目怎么构建、怎么测试、有哪些约定。每一轮都完整地放进系统提示词。
- **Skills**：一个文件夹加一份 `SKILL.md`，教 agent 做某一类事情。系统提示词里只放目录，任务对得上时 agent 才去读正文，
  所以装很多 Skill 也不占上下文。

## AGENTS.md

### 放在哪里

| 位置 | 作用范围 |
|---|---|
| `~/.dshr/AGENTS.md` | 全局：对所有工作区生效（你的个人偏好） |
| `<工作区>/AGENTS.md` | 项目：只对这个工作区生效（只读工作区根目录下的这一份） |

两份都存在时都会用上，全局的在前、项目的在后。“设置 → 扩展 → 项目说明与 Skills”里的“编辑全局说明”直接打开全局的那一份。

### 怎么生效

- 每一轮开始时读取，追加在系统提示词后面，并注明来源（“Instructions from this project's AGENTS.md (路径)”）。
  改了文件从下一轮起生效，不用重启。
- 内置系统提示词的最后一句是“AGENTS.md 里的说明优先于这些默认做法”：你写的约定可以覆盖默认行为。
- 单个文件最多读 32 KB，超出的部分截掉；文件必须是 UTF-8 文本；空文件被忽略。
- 不想用时，在“设置 → 扩展 → 内置功能”里关掉“项目说明”（`[plugins.instructions] enabled = false`）。

### 写什么

agent 已经会读代码、跑命令；AGENTS.md 最有用的是那些**从代码里看不出来**、或者要花很多步才能摸清的事：

- 怎么构建、测试、lint，哪些命令最快、哪些要避免（比如“不要跑完整的端到端测试，太慢”）；
- 代码约定：命名、错误处理、日志、注释的语言；
- 目录结构里的门道：生成的代码在哪里、哪些文件不要手改；
- 提交与评审的规矩：要不要写测试、提交信息的格式；
- 你个人的偏好（写在全局的那份里）：回复的语言、详略。

一个例子：

```markdown
# 项目说明

## 构建与测试
- `cargo test -p dshr-agent` 只测 agent；改了 UI 才需要 `cargo test -p dshr-ui`（要编译 GPUI，慢）。
- 提交前跑 `cargo clippy --workspace`，不要留新的警告。

## 约定
- 注释和文档用中文；用户可见的文字放进 `crates/ui/locales/app.yml`，十种语言都要有。
- 界面代码不直接写颜色、圆角、字号，全部来自 `crates/ui/src/design.rs`。
- 不要修改 `vendor/` 下的代码，除非任务就是升级补丁。
```

## Skills

### 一个 Skill 长什么样

每个 Skill 是一个文件夹，里面有一份 `SKILL.md`，开头用 front matter 写名字和说明：

```text
release-notes/
├── SKILL.md
├── template.md
└── collect.sh
```

```markdown
---
name: release-notes
description: 根据 git 历史写发布说明。用户要求写 changelog、发布说明、版本总结时使用。
---

# 写发布说明

1. 用 `git log --oneline <上一个标签>..HEAD` 列出提交，必要时运行同目录下的 `collect.sh`。
2. 按“新功能 / 修复 / 破坏性变更”分组，忽略纯重构和文档提交。
3. 按 `template.md` 的格式输出。破坏性变更写明迁移方法。
```

- `name`：Skill 的名字；不写时用文件夹名。
- `description`：**什么时候该用它**。模型只凭这一句决定要不要读正文，所以写清楚适用的任务，而不只是它是什么。最多 300 个字符。
- 正文：步骤、约定、注意事项。可以引用同一文件夹里的脚本、模板，路径相对于 Skill 的文件夹。

front matter 只认单行的 `键: 值`（值两边的引号会去掉），其他键会被忽略。

### 放在哪里

| 位置 | 作用范围 |
|---|---|
| `~/.dshr/skills/<名字>/SKILL.md` | 全局：对所有工作区生效 |
| `<工作区>/.dshr/skills/<名字>/SKILL.md` | 项目 |
| `<工作区>/.agents/skills/<名字>/SKILL.md` | 项目（其他 agent 工具的约定，已有的 Skill 可以直接用） |
| `<工作区>/.claude/skills/<名字>/SKILL.md` | 项目（同上） |
| 插件的 `skills/<名字>/SKILL.md` | 随插件提供，名字是 `<插件 id>:<名字>` |

同名的 Skill 只保留一个，优先级从高到低：`.dshr/skills` → `.agents/skills` → `.claude/skills` → 全局。插件带的 Skill 名字里有
插件前缀，不会和它们冲突。

“设置 → 扩展 → 项目说明与 Skills”里的“打开 Skills 文件夹”打开全局的 `skills/` 目录。

### 怎么生效

每一轮开始时，dshr 扫描上面这些位置，把所有 Skill 的名字、说明和 `SKILL.md` 的路径列进系统提示词（最多 100 个），并告诉模型：
任务和某个 Skill 对得上时，先用 `read_file` 读它的 `SKILL.md` 再动手，照着做。

- 新增或修改的 Skill 从下一轮起生效，不用重启。
- 只有 `description` 常驻上下文；正文按需读取，读取不需要审批。
- 不想用时，在“设置 → 扩展 → 内置功能”里关掉“Skills”（`[plugins.skills] enabled = false`）。

### 写好 Skill 的建议

- 一个 Skill 只管一类任务。“部署到测试环境”和“写发布说明”分成两个。
- `description` 用用户会说的话来写触发条件：“用户要求部署、发布到 staging 时使用”。
- 步骤要具体到命令和文件；容易出错的地方单独写出来。
- 需要稳定输出格式时，放一份模板文件在文件夹里，让正文引用它。
- 涉及破坏性操作的 Skill，在正文里写明“先向用户确认”。

## 和插件的关系

- 插件包可以带 `skills/` 文件夹，随插件一起安装、升级（见 [插件开发指南](plugins.md#插件包)）。
- 有 `dshr:skills` 能力的插件可以列出、读写、删除 Skills（见 [插件 API 参考](plugin-api.md#hostdshr)）。
- 插件的 `prompt` 导出也能往系统提示词里追加内容，适合需要动态生成的说明（比如根据工作区里的配置文件）。
