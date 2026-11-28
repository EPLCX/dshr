//! `dshr plugin new`：在插件目录里生成一个能直接加载的 JS 插件骨架。

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::manifest::{Capabilities, DEFAULT_MAIN, MANIFEST_FILE, valid_id};

/// 新插件的选项。
#[derive(Debug, Clone, Default)]
pub struct Scaffold {
    pub id: String,
    /// 显示名，默认等于 id。
    pub name: Option<String>,
    pub description: Option<String>,
    /// 写进清单的能力，原样检查后写入。
    pub capabilities: Vec<String>,
    /// 同时生成插件包的示例：`skills/example/SKILL.md`、`commands/example.md`，清单里一段注释掉的 `[mcp.servers]`。
    pub bundle: bool,
}

impl Scaffold {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), ..Self::default() }
    }

    /// 在 `parent/<id>/` 下写出 `plugin.toml` 和 `index.js`，返回插件文件夹。
    /// 文件夹已经存在时不动它，直接报错。
    pub fn create(&self, parent: &Path) -> anyhow::Result<PathBuf> {
        if !valid_id(&self.id) {
            bail!("插件 id `{}` 不合规：只用小写字母、数字、`-`、`_`，以字母或数字开头，最长 64", self.id);
        }
        let capabilities = Capabilities::parse(&self.capabilities).map_err(anyhow::Error::msg)?;
        let dir = parent.join(&self.id);
        if dir.exists() {
            bail!("{} 已经存在，换一个 id 或先删掉它", dir.display());
        }
        std::fs::create_dir_all(parent).with_context(|| format!("建不了插件目录 {}", parent.display()))?;
        // 只建这一层：并发的两次 `new` 里只有一个能成功，不会把对方的文件覆盖掉。
        std::fs::create_dir(&dir).with_context(|| format!("建不了 {}", dir.display()))?;
        let written = std::fs::write(dir.join(MANIFEST_FILE), self.manifest())
            .and_then(|()| std::fs::write(dir.join(DEFAULT_MAIN), self.script(&capabilities)))
            .and_then(|()| if self.bundle { self.write_bundle(&dir) } else { Ok(()) });
        if let Err(error) = written {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error).with_context(|| format!("写不了 {}", dir.display()));
        }
        Ok(dir)
    }

    /// 插件包的示例文件。
    fn write_bundle(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir.join("skills/example"))?;
        std::fs::write(
            dir.join("skills/example/SKILL.md"),
            "---\nname: example\ndescription: 示例 Skill：说明什么时候该用它（模型据此决定要不要读正文）\n---\n\n\
             这里写步骤和约定。同一个文件夹里的脚本、模板用相对路径引用。\n",
        )?;
        std::fs::create_dir_all(dir.join("commands"))?;
        std::fs::write(
            dir.join("commands/example.md"),
            format!(
                "---\ndescription: 示例命令：输入框里打 /{}:example\nargument-hint: <要处理的内容>\nmode: insert\n---\n\
                 请处理下面的内容：$ARGUMENTS\n",
                self.id
            ),
        )
    }

    fn manifest(&self) -> String {
        let name = self.name.as_deref().unwrap_or(&self.id);
        let mut text = format!("id = {}\nname = {}\nversion = \"0.1.0\"\n", quote(&self.id), quote(name));
        if let Some(description) = &self.description {
            text.push_str(&format!("description = {}\n", quote(description)));
        }
        let capabilities: Vec<String> = self.capabilities.iter().map(|capability| quote(capability)).collect();
        text.push_str(&format!(
            "# 能用的宿主功能：fs:workspace:read、fs:workspace:write、net:<主机>、exec:<程序>、llm、session:read、\n\
             # session:control、agent:run、hooks:gate、hooks:review、hooks:tool-output、context:compact、context:rewrite、\n\
             # model:route、provider、oauth、notify、background、dshr:config:read、dshr:config:write、dshr:skills、\n\
             # dshr:sessions:read、dshr:sessions:write、dshr:mcp、plugins:install、permissions:mode、permissions:approve\n\
             # （见 docs/plugins.md）\n\
             capabilities = [{}]\n",
            capabilities.join(", ")
        ));
        text.push_str(
            "# timeout_ms = 30000                  # 单次工具调用的时限，默认 30 秒\n\
             \n\
             # 配置的 JSON Schema：设置页据此生成表单，值在 config.toml 的 [plugins.<id>] 里，脚本里是 host.config。\n\
             [config.properties.greeting]\n\
             type = \"string\"\n\
             default = \"Hello\"\n\
             title = \"问候语\"                     # 表单里显示的名字；其它语言写在 i18n.<语言> 或 locales/<语言>.json\n\
             description = \"打招呼时用的第一个词\"\n\
             \n\
             [config.properties.greeting.i18n.en]\n\
             title = \"Greeting\"\n\
             description = \"The first word of the greeting\"\n",
        );
        if self.bundle {
            text.push_str(
                "\n# 插件自带的 MCP 服务器（stdio 写 command/args，HTTP 写 url/headers）。每个服务器要单独允许一次；\n\
                 # 能用 ${plugin}（插件文件夹）、${data}（插件数据目录）、${env:NAME}、${secret:NAME}（上面 secrets 里声明的）。\n\
                 # [mcp.servers.example]\n\
                 # command = \"node\"\n\
                 # args = [\"${plugin}/server/index.js\"]\n",
            );
        }
        text
    }

    fn script(&self, capabilities: &Capabilities) -> String {
        // 工具名最长 64：给后缀（`_line_count`）留出地方。
        let tool: String = self.id.replace('-', "_").chars().take(48).collect();
        let mut text = format!(
            r#"// {id}：dshr 的 JS 插件。写法见 docs/plugins.md 的“JS 插件”一节。
// 桌面版开着时保存就生效（热重载）；命令行版下次运行时生效。
// 改完可以用 `dshr plugin list` 看它有没有加载成功。

export const tools = [{{
  name: "{tool}_hello",
  description: "Say hello to someone. (Example tool generated by `dshr plugin new`.)",
  inputSchema: {{
    type: "object",
    properties: {{ name: {{ type: "string", description: "Who to greet" }} }},
    required: ["name"],
  }},
  // 不写文件、不联网的工具可以标 readOnly：它按只读处理，任何模式下都直接放行。
  readOnly: true,
  async execute({{ name }}, host) {{
    const greeting = host.config.greeting ?? "Hello";
    host.log("greeting", name);
    return `${{greeting}}, ${{name}}!`;
  }},
}}];
"#,
            id = self.id,
        );
        if capabilities.fs_read {
            text.push_str(&format!(
                r#"
// 有 fs:workspace:read：可以读工作区里的文件。
tools.push({{
  name: "{tool}_line_count",
  description: "Count the lines of a file in the workspace.",
  inputSchema: {{ type: "object", properties: {{ path: {{ type: "string" }} }}, required: ["path"] }},
  readOnly: true,
  async execute({{ path }}, host) {{
    const text = await host.readFile(path);
    return `${{path}}: ${{text.split("\n").length}} lines`;
  }},
}});
"#
            ));
        }
        if let Some(host) = capabilities.net.first() {
            let example = match host.strip_prefix("*.") {
                Some(domain) => format!("api.{domain}"),
                None if host == "*" => "example.com".to_owned(),
                None => host.clone(),
            };
            text.push_str(&format!(
                r#"
// 有 net: 能力：可以用 host.fetch 访问声明过的主机。能联网的工具在“工作区内修改”模式下每次询问。
// tools.push({{
//   name: "{tool}_fetch",
//   description: "Fetch something.",
//   inputSchema: {{ type: "object", properties: {{}} }},
//   async execute(_input, host) {{
//     const response = await host.fetch("https://{example}/");
//     return {{ status: response.status, body: response.body.slice(0, 2000) }};
//   }},
// }});
"#
            ));
        }
        if let Some(program) = capabilities.exec.first() {
            let program = if program == "*" { "git" } else { program.as_str() };
            text.push_str(&format!(
                r#"
// 有 exec: 能力：可以用 host.exec 启动声明过的程序（不经 shell）。这样的工具和 run_shell 一样按权限模式询问。
// tools.push({{
//   name: "{tool}_run",
//   description: "Run {program} --version.",
//   inputSchema: {{ type: "object", properties: {{}} }},
//   async execute(_input, host) {{
//     const {{ code, stdout, stderr }} = await host.exec("{program}", ["--version"]);
//     return code === 0 ? stdout : `failed (${{code}}): ${{stderr}}`;
//   }},
// }});
"#
            ));
        }
        if capabilities.llm {
            text.push_str(&format!(
                r#"
// 有 llm 能力：可以用当前会话的模型发一次性请求。
// tools.push({{
//   name: "{tool}_summarize",
//   description: "Summarize a text with the session's model.",
//   inputSchema: {{ type: "object", properties: {{ text: {{ type: "string" }} }}, required: ["text"] }},
//   readOnly: true,
//   execute: ({{ text }}, host) => host.llm.complete({{ system: "Summarize in one sentence.", prompt: text }}),
// }});
"#
            ));
        }
        text.push_str(
            r#"
// 每轮开始时问一次，返回的文本追加在系统提示词后面。不需要就删掉。
// export const prompt = async ({ workspace }, host) => "…";

// 挂在 agent 循环上的钩子（都可以是 async 的）。不需要就删掉。
// export const hooks = {
//   // 工具执行前：返回 "deny" / "ask" / { decision: "deny", reason }；直接放行（"allow"）要 hooks:gate 能力。
//   beforeTool: ({ tool, input, access }) => (access.type === "write" && access.path.endsWith(".lock") ? "deny" : undefined),
//   // 一轮结束后。
//   turnEnded: async ({ reason, finalText }, host) => { await host.writeData("last.txt", finalText); },
// };
"#,
        );
        text
    }
}

/// TOML 的基本字符串。
fn quote(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}
