# 插件的打包、签名与分发

写好的插件可以打成一个文件发给别人，也可以发布到插件索引，让用户搜索、按 id 安装、收到更新。这篇讲完整的流程，
以及安装时用户会看到什么。插件本身的写法见 [插件开发指南](plugins.md)。

## 发布前检查

- [ ] `plugin.toml` 写了 `id`、`name`、`version`（semver）、`description`；需要时写 `[engines] dshr = ">=0.1"`。
- [ ] 能力只申请真正需要的：能用 `fs:workspace:read` 就不要写权限，能列出具体主机就不要 `net:*`，尽量不要 `exec:*`。
- [ ] 不需要写、联网、启动程序的工具都标了 `readOnly: true`。
- [ ] 密钥用清单的 `secrets` 声明，代码和文件里没有任何令牌。
- [ ] `dshr plugin list` 显示“可用”；在 `dshr plugin dev` 里把每个工具、每个页面动作都试过。
- [ ] 不需要发布的文件（测试数据、源码映射、构建缓存）写进 `.dshrignore`。

## 打包

```bash
dshr plugin pack                       # 当前目录
dshr plugin pack path/to/plugin -o dist/word-count.dshrplugin --sign
```

输出 `<id>-<版本>.dshrplugin`（默认放在插件文件夹旁边），它就是一个 zip 文件。打包前会在临时目录里把插件实际加载一遍（执行顶层代码、
检查导出），加载不了的不打包。

打进包里的文件：插件文件夹里的全部文件，**跳过**隐藏文件和文件夹（以 `.` 开头）、`.git`、`node_modules`、`SIGNATURE`，以及
`.dshrignore` 里列出的 glob（每行一个，`#` 开头是注释，写目录名会连同目录下的全部内容一起跳过）：

```text
# .dshrignore
tests/
*.map
fixtures/**/*.json
```

最多 20000 个文件。打包完成后打印文件数、id、版本、**SHA-256** 和签名者——发布到索引时要用到 SHA-256。

## 签名

签名让用户确认包确实是你发的、内容没有被改过，并且之后的更新也来自你。

```bash
dshr plugin keygen            # 生成密钥：私钥存进系统钥匙串（条目 plugin-signing），打印公钥
dshr plugin keygen            # 再运行一次：打印现有的公钥
dshr plugin keygen --replace  # 换一把新的（旧钥匙作废，已安装用户更新时会看到“发布者变了”）
dshr plugin pack --sign       # 打包并签名
```

- 算法是 Ed25519，公钥写作 `dshr-ed25519:<base64>`。
- 签名的对象不是 zip 的字节，而是一份规范清单：每个文件一行 `<sha256>  <相对路径>`，按路径排序。签名写在包里的 `SIGNATURE` 文件
  （JSON：`{ key, signature }`）。
- 安装时按**实际解开的文件**重新计算清单再验签，所以无论从文件、网址还是索引安装，验证的都是真正装进去的内容。
- 在 CI 里签名：把私钥（base64 编码的 32 字节）放进环境变量 `DSHR_PLUGIN_SIGNING_KEY`，钥匙串里没有密钥时会读它。

**妥善保管私钥**。丢了钥匙只能换新的，换钥匙后已经装了你插件的用户在更新时会被警告“发布者已变化”。

## 安装

用户可以从这些来源安装：

| 来源 | 命令行 | 桌面版 |
|---|---|---|
| 安装包（`.dshrplugin`、`.zip`、`.tar.gz`、`.tgz`） | `dshr plugin install word-count-1.0.0.dshrplugin` | “设置 → 扩展 → 从安装包安装…” |
| 本地文件夹 | `dshr plugin install ./word-count` | “从文件夹安装…” |
| 网址（zip / tar.gz） | `dshr plugin install https://example.com/word-count.dshrplugin --sha256 <哈希>` | — |
| GitHub 仓库 | `dshr plugin install https://github.com/<owner>/<repo>`（换成默认分支的源码包） | — |
| 插件索引 | `dshr plugin install word-count` | “插件索引”里搜索后安装 |
| 其他插件 | 有 `plugins:install` 能力的插件（如插件市场）调用 `host.plugins.install` | 同左，每次都弹出确认框 |

压缩包最大 50 MB，解压后最大 200 MB、最多 20000 个文件。压缩包里只有一个顶层文件夹时，以它为插件文件夹。`--sha256` 对不上时不安装。

### 安装流程

1. 下载或读取到一个临时的暂存文件夹并解压；
2. 读清单，用和插件目录相同的规则检查；验证签名；
3. **询问用户**（见下文）。不同意就删掉暂存，什么都不留下；
4. 同意后挪进插件目录。同名插件的旧版本先归档（可以回滚），失败时恢复原状；
5. 热重载随即装上它。**声明的能力仍然要另外允许**——安装不等于授权。

### 用户会看到什么

命令行列出名字、版本、id、来源、说明、签名、SHA-256、是否替换已安装的版本、声明的权限、自带 MCP 服务器的完整启动命令、
版本要求，然后问 `安装？[y/N]`（`--yes` 跳过）。

桌面版弹出确认框，分为“来源与完整性”“声明的权限”“包内容”三部分，并给出风险总评：

| 总评 | 条件 |
|---|---|
| **高风险** | 发布者变了；或者要 `net:*`、任何 `exec:`；或者自带 MCP 服务器 |
| **需要留意** | 有需要留意的权限（写工作区、代发消息、启动子代理、放行 / 审查 / 改写工具调用、改写请求、改设置、改会话、管理 MCP / Skills、安装插件）；或者带脚本却没有签名；或者当前 dshr 版本不满足它的要求（装上也不会激活） |
| **低风险** | 其余 |

确认框的默认焦点在“取消”上，`Esc` 取消，`Enter` 不会安装；高风险时“安装”按钮要等 1.5 秒才能点，防止误点。

### 发布者信任

dshr 采用“首次信任”：

- 第一次安装时记下签名的公钥（`~/.dshr/plugin-sources.json`）；
- 之后的更新必须由**同一把钥匙**签名；之前签了名、这次没签名也不行；
- 发布者变了时，命令行拒绝安装（除非加 `--force`），桌面版显示“发布者已变化：可能是包被冒充”，需要用户明确点
  “信任新发布者并安装”；由插件发起的安装在这种情况下一律拒绝。

## 插件索引

插件索引是一个静态 JSON 文件，放在任何能通过 HTTPS 访问的地方（对象存储、GitHub Pages……）。dshr 默认不配置任何索引，用户在
`config.toml` 里添加：

```toml
[plugin_index]
urls = ["https://example.com/dshr-plugins/index.json"]
```

索引格式：

```json
{
  "plugins": [
    {
      "id": "word-count",
      "name": "字数统计",
      "description": "统计工作区文件的字数",
      "publisher": { "name": "Alice", "key": "dshr-ed25519:…" },
      "versions": [
        {
          "version": "1.2.0",
          "url": "https://example.com/dshr-plugins/word-count-1.2.0.dshrplugin",
          "sha256": "…",
          "dshr": ">=0.1"
        }
      ]
    }
  ]
}
```

| 字段 | 说明 |
|---|---|
| `id` | 插件 id，必须和包里清单的 id 一致 |
| `name`、`description` | 搜索结果里显示 |
| `publisher.key` | 发布者公钥。给了的话，按索引安装的包**必须由这把钥匙签名** |
| `versions[].version` | semver 版本号 |
| `versions[].url` | 包的下载地址 |
| `versions[].sha256` | 包的 SHA-256（`dshr plugin pack` 打印的那个），下载后必须对得上 |
| `versions[].dshr` | 要求的 dshr 版本（semver 范围），可省略 |

安装时挑选**当前 dshr 满足 `dshr` 要求的版本里最新的一个**。

发布一个新版本：

1. 改 `plugin.toml` 的 `version`；
2. `dshr plugin pack --sign`，记下 SHA-256；
3. 把包上传到 `url` 指向的位置；
4. 在索引里该插件的 `versions` 加一项，上传索引。

## 更新与回滚

```bash
dshr plugin search 字数          # 在索引里搜索
dshr plugin outdated             # 哪些已安装的插件有新版本
dshr plugin update word-count    # 更新一个（同样列出权限并确认）
dshr plugin update --all         # 全部更新
dshr plugin rollback word-count  # 回到升级前的版本
dshr plugin rollback word-count --to 1.1.0
```

- 升级时旧版本不会删除，而是归档到 `~/.dshr/plugin-versions/<id>/<版本>-<时间>/`，每个插件保留最近 3 个。
- 回滚就是从归档里再装一次（同样确认；回滚装的是你装过的版本，不检查发布者）。
- 更新和回滚都**不动**插件的数据目录和授权；但新版本多出来的能力要再允许一次。
- 桌面版的插件卡片上会显示“有新版本”“更新”和“回滚到 …”。

## 在 CI 里发布

```yaml
# 示例：GitHub Actions
- name: Pack plugin
  env:
    DSHR_PLUGIN_SIGNING_KEY: ${{ secrets.DSHR_PLUGIN_SIGNING_KEY }}
  run: |
    dshr plugin pack . -o dist/word-count.dshrplugin --sign
    sha256sum dist/word-count.dshrplugin
```

`DSHR_PLUGIN_SIGNING_KEY` 是 base64 编码的 32 字节 Ed25519 私钥。在本机用 `dshr plugin keygen` 生成的私钥保存在系统钥匙串的
`plugin-signing` 条目里，可以从那里取出来放进 CI 的密钥设置。
