# gpui-pre-macos 0.3.12（dshr 补丁版）

来源：crates.io 上的 `gpui-pre-macos 0.3.12`，下文记录了本地补丁。通过根 `Cargo.toml` 的
`[patch.crates-io]` 使用。

许可证：Apache-2.0（见同目录 `LICENSE-APACHE`），修改部分按同一许可证发布。按其第 4(b) 条，被修改的源文件开头都带有修改声明；新改动其他文件时，也在该文件开头加上同样的声明。
本组件已列入根目录的 `THIRD_PARTY_NOTICES.md`（“修改过的第三方组件”一节）。

## 改动

`src/window.rs` 的 `MacWindow::a11y_init` 有两处改动，都是为了修复 VoiceOver 只跟随鼠标、不跟随键盘焦点。

### 1. 适配器装在 `GPUIView` 上，不装在窗口的 content view 上

原来用 `SubclassingAdapter::for_window`，把 accesskit 装在窗口的 `contentView` 上。但 GPUI 的 `GPUIView`
（`native_view`）是 `contentView` 的子视图，而且是窗口的 first responder：

- 读鼠标位置时，VoiceOver 从窗口往下逐层 hit test，经过 `contentView` 就能找到 accesskit 的节点，所以这部分正常；
- 查键盘焦点时，VoiceOver 从 first responder（`GPUIView`）的 `accessibilityFocusedUIElement` 找起，
  `GPUIView` 没有被 accesskit 子类化，碰不到 accesskit 的树，所以焦点和焦点变化都读不到。

现在改用 `SubclassingAdapter::new(native_view)`。`GPUIView` 铺满 content view，accesskit 自己处理视图坐标
（含 `isFlipped`），节点位置不受影响。`Drop for MacWindow` 里关于循环引用的注释也跟着更新了：
现在适配器强引用的是 `GPUIView`，`drop` 时先释放适配器的做法不变。

### 2. 创建适配器后同步一次窗口焦点状态

适配器创建时认为视图**没有**焦点（`Adapter::new(view, false, ..)`），之后只在 `windowDidBecomeKey:` /
`windowDidResignKey:` 里更新。而 `MacWindow::open` 先 `makeKeyAndOrderFront:`，`a11y_init` 在这之后才调用，
那次通知到达时适配器还不存在，于是 accesskit 一直认为没有焦点（`tree.state().focus()` 为 `None`）。
现在创建适配器后立刻用 `isKeyWindow` 调一次 `update_view_focus_state`。

## 升级 gpui-pre 时

先看上游 `a11y_init` 是否已经把适配器装在 `GPUIView` 上、并同步初始焦点状态；
已经修复就删掉 `vendor/gpui-pre-macos` 和根 `Cargo.toml` 里对应的 `[patch.crates-io]` 条目。
没有修复就用新版本重新复制，再把上面的改动和文件开头的修改声明打上去，并重新生成 `THIRD_PARTY_NOTICES.md`。
