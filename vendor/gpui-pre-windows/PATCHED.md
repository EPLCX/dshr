# gpui-pre-windows 0.3.12（dshr 补丁版）

来源：crates.io 上的 `gpui-pre-windows 0.3.12`，下文记录了本地补丁。通过根 `Cargo.toml` 的
`[patch.crates-io]` 使用。

许可证：Apache-2.0（见同目录 `LICENSE-APACHE`），修改部分按同一许可证发布。按其第 4(b) 条，被修改的源文件开头都带有修改声明；新改动其他文件时，也在该文件开头加上同样的声明。
本组件已列入根目录的 `THIRD_PARTY_NOTICES.md`（“修改过的第三方组件”一节）。

## 改动

`src/directx_renderer.rs` 的 `DirectXRenderer::resize`：

- 释放旧资源前，除了 `OMSetRenderTargets(None, None)`，还解绑 VS/PS 的 0、1 号着色器资源槽。
  `draw_paths_from_intermediate` 之后 path 中间纹理的 SRV 一直绑在 0 号槽上，旧的整窗纹理和 4× MSAA 纹理
  因此释放不掉。
- 释放 render target 和 path 中间纹理的视图后 `Flush()`，`recreate_resources` 之后再 `Flush()` 一次。
  D3D11 延迟销毁资源，不 Flush 时，拖动调整窗口大小（每秒几十个 `WM_SIZE`，中间常常没有 Present）
  每一步都会留下几十 MB，表现为“调整窗口大小有概率让内存膨胀”。

resize 里没有用 `ClearState()`：它会清掉初始化时只设置一次的光栅化状态，而显式解绑已经够了。

### 调整大小结束后和定时回收内存

`src/events.rs`：每次 `WM_SIZE` 都重新开始一个 500ms 的计时器（`TRIM_AFTER_RESIZE_TIMER_ID`）。
拖动、最大化、贴边分屏结束半秒后执行一次 `trim_after_resize`。
`src/platform.rs`：Windows 图形后端在消息循环中每 60 秒通过平台窗口触发一次同样的回收，
不随窗口数量增加；绘制或渲染器使用中收到定时消息时，跳过本轮的 DirectX 回收。

- `trim_directx_memory`：`ClearState()`、`Flush()`，再调用 `IDXGIDevice3::Trim()`，让驱动释放
  调整大小期间留在池里的内部缓冲（核显上这部分算在进程内存里）。之后重新设置光栅化状态，因为它只在初始化时设置过一次；
  其余状态每帧都会重新绑定。
- `HeapCompact(GetProcessHeap())`：把进程堆里空出来的页还给系统。
- `SetProcessWorkingSetSize(进程, usize::MAX, usize::MAX)`：把拖动时碰过、现在已经不用的页移出工作集
  （任务管理器“内存”列显示的就是工作集）。还在用的页之后以软缺页的方式取回，开销很小。

不打补丁时，窗口平时约 20MB，拖动一次之后会一直停在约 60MB。

## 升级 gpui-pre 时

先看上游 `DirectXRenderer::resize` 是否已经解绑并 Flush；已经修复就删掉 `vendor/gpui-pre-windows`
和根 `Cargo.toml` 里的 `[patch.crates-io]`。没有修复就用新版本重新复制，再把上面的改动和文件开头的修改声明打上去，并重新生成 `THIRD_PARTY_NOTICES.md`。
