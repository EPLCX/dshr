<!-- 标题：用中文说明这个 PR 带来的结果，不加句号。规范见 docs/contributing.md -->

## 变更内容

<!-- 做了什么、为什么这样做。改动界面的请附浅色 / 深色截图。 -->

## 关联 Issue

<!-- Closes #编号 / Refs #编号；没有则写“无” -->

## 影响范围

<!-- 涉及的 crate、配置项、命令行参数、插件 API、会话日志格式 -->

## 破坏性更改

<!-- 没有则写“无”。有则按“变更前 / 变更后 / 影响范围”写明，并加 breaking 标签。 -->

## 行为变更

<!-- 不破坏兼容性、但会改变可观察行为的变更（默认值、默认快捷键、显示内容等）；没有则写“无”。有则加 behavior-change 标签。 -->

## 测试

<!-- 运行了哪些测试，手动验证了哪些场景（平台、权限模式、语言等） -->

## 检查清单

- [ ] `cargo test` 通过；改了界面时 `cargo test -p dshr-ui` 也通过
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` 没有警告，`cargo fmt --all --check` 通过
- [ ] 改了依赖：已重新生成 `THIRD_PARTY_NOTICES.md`
- [ ] 改了界面：已按设计规范的检查清单逐项检查，新文案有十种语言
- [ ] 改了行为、配置项、命令或插件 API：已同步更新 `docs/`
- [ ] 判断过是否需要调整版本号（见 docs/contributing.md §4）
