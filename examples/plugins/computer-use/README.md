# 电脑操作（computer-use）

一个 DSHR 的 JS 插件：给模型一个 `computer` 工具，让 agent 能**看屏幕、动鼠标、敲键盘**，去操作编辑器以外的桌面程序，
比如浏览器、设置面板或其他图形界面应用。

不装任何依赖，只调用系统自带（或常见）的程序：

| 系统 | 截屏 | 鼠标键盘 |
|---|---|---|
| Windows | `powershell`（System.Drawing） | `powershell` 调 user32 的 `SendInput`（支持任意 Unicode 文字） |
| macOS | `screencapture` + `sips` + `base64` | `osascript -l JavaScript` 调 CoreGraphics；文字经剪贴板粘贴（之后恢复原来的纯文本剪贴板） |
| Linux（X11） | `import`（ImageMagick）+ `base64` | `xdotool` |

## 安装

```bash
dshr plugin link examples/plugins/computer-use   # 开发时链接；或拷到 ~/.dshr/plugins/computer-use/
dshr plugin grant computer-use                   # 允许它启动上面那些程序
dshr plugin list                                 # 状态应为“可用”
```

装好后在“设置 → 电脑操作”里点 **截一张图试试**，确认截屏和权限都没问题。

### 系统权限

- **macOS**：在“系统设置 → 隐私与安全性”里，给 DSHR（命令行版则给运行它的终端）打开**屏幕录制**（否则截图只有桌面壁纸）
  和**辅助功能**（否则点击和按键没有反应）。在“工作区内修改”模式下，命令在 Seatbelt 沙箱里运行，可能截不了屏，
  这时请改用“完全访问”模式。
- **Windows**：以管理员身份运行的窗口收不到普通权限程序发出的输入；屏幕锁定时也无法操作。
- **Linux**：只支持 X11（Wayland 下 xdotool 不起作用），需要先安装 `xdotool` 和 ImageMagick。

## 工具：`computer`

| action | 参数 | 作用 |
|---|---|---|
| `screenshot` | — | 截取主显示器，缩放到 `max_width` |
| `click` / `double_click` / `triple_click` / `right_click` / `middle_click` | `x`, `y`（可省略，表示当前位置），`modifiers` | 点击 |
| `move` | `x`, `y` | 移动鼠标 |
| `drag` | `x`, `y`, `to_x`, `to_y`, `modifiers` | 按住左键拖动 |
| `scroll` | `direction`, `amount`, `x`, `y` | 滚轮 |
| `type` | `text` | 输入文字（`\n` 会按回车） |
| `key` | `keys` | 按快捷键，如 `ctrl+c`、`cmd+space`、`ctrl+a backspace` |
| `cursor_position` | — | 返回鼠标位置 |
| `wait` | `seconds` | 等待一段时间后截图 |

- **坐标**：一律使用“最近一张截图”里的像素坐标，插件负责换算成屏幕坐标（包括 Retina 和 Windows 的高 DPI 缩放）。所以第一步必须先截图。
- **自动截图**：除 `screenshot` 和 `cursor_position` 外，每个动作执行完后都会等 `settle_ms` 毫秒，再自动截一张图返回给模型，
  方便它确认结果。单次调用可以传 `"screenshot": false` 关掉。
- **按键名**：`ctrl` `shift` `alt` `meta`（`cmd`/`win`/`super` 是它的别名）、`enter` `tab` `escape` `backspace` `delete`
  `space` `up` `down` `left` `right` `home` `end` `pageup` `pagedown` `insert` `f1`–`f24`、字母、数字和标点；`+` 键写作 `plus`。

## 设置

| 键 | 默认 | 说明 |
|---|---|---|
| `max_width` | 1280 | 截图缩放到的最大宽度。越大越清楚，也越费 token |
| `quality` | 70 | JPEG 质量 |
| `auto_screenshot` | true | 每次动作后自动截图 |
| `settle_ms` | 600 | 动作完成后等多少毫秒再截图 |
| `platform` | auto | 手动指定 `windows` / `macos` / `linux` |

## 安全

这个工具会启动程序，所以除“完全访问”外的权限模式下，每次调用都会先问你。它能操作你屏幕上的任何东西，
因此不要在打开着密码管理器、网银等敏感窗口时让它无人看管地运行。工具说明里也要求模型：除非你明确要求，
否则不输入密码、不付款、不发消息、不删数据。

## 已知限制

- 只截主显示器，坐标也只对应主显示器。
- macOS 的 `type` 借用剪贴板，只能恢复原来的**纯文本**剪贴板内容；macOS 13 以下不支持滚动。
- Linux 下输入非 ASCII 文字时依赖 `C.UTF-8` locale，目标程序本身也要支持 UTF-8 输入。
- 测试情况：Linux（X11）后端已经在 Xvfb + xterm 上实测过（截图、点击、拖动、滚动、按键、输入中文）；
  Windows 和 macOS 后端还没有在真机上跑过。
