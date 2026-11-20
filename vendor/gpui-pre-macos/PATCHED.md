# gpui-pre-macos 0.3.12（dshr 补丁版）

来源：crates.io 上的 `gpui-pre-macos 0.3.12`，下文记录了本地补丁。通过根 `Cargo.toml` 的
`[patch.crates-io]` 使用。

## 改动

`src/window.rs` 的 `MacWindow::a11y_init`：创建 `accesskit_macos::SubclassingAdapter` 之后，立刻用
`isKeyWindow` 调一次 `update_view_focus_state`。

### 原因：VoiceOver 只跟随鼠标，不跟随键盘焦点

- `SubclassingAdapter` 创建时认为视图**没有**焦点（`Adapter::new(view, false, ..)`），之后只在
  `windowDidBecomeKey:` / `windowDidResignKey:` 里由 `window_did_change_key_status` 更新。
- 打开窗口时，`MacWindow::open` 先 `makeKeyAndOrderFront:`，`windowDidBecomeKey:` 这时就发出了；
  而 `a11y_init` 在 `Window::new` 里、`open` 返回之后才调用，那次通知到达时适配器还不存在。
- 结果适配器一直以为窗口不是 key window：accesskit 的 `tree.state().focus()` 返回 `None`，
  `accessibilityFocusedUIElement` 返回 nil，焦点变化也不发 `NSAccessibilityFocusedUIElementChangedNotification`。
  VoiceOver 拿不到键盘焦点，只能靠 `accessibilityHitTest:` 读鼠标下的元素。
- 切到别的应用再切回来（窗口重新变成 key）之后才恢复正常，这也是判断是否命中这个问题的办法。

## 升级 gpui-pre 时

先看上游 `a11y_init` 是否已经同步了初始焦点状态（或 accesskit_macos 是否自己读取 `isKeyWindow`）；
已经修复就删掉 `vendor/gpui-pre-macos` 和根 `Cargo.toml` 里对应的 `[patch.crates-io]` 条目。
没有修复就用新版本重新复制，再把上面的改动打上去。
