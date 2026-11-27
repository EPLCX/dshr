# gpui-pre-windows 0.3.12（dshr 补丁版）

来源：crates.io 上的 `gpui-pre-windows 0.3.12`，下文记录了本地补丁。通过根 `Cargo.toml` 的
`[patch.crates-io]` 使用。

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

### Windows 10 上自绘标题栏时补上顶边

移植自上游还没合并的 zed-industries/zed#63821（修复 zed-industries/zed#63812）。隐藏系统标题栏后，客户区
延伸到窗口顶端，Windows 10 的 DWM 只在窗口自己的边框区域里画顶边，于是左、右、下三边有 1 像素边框线，顶上没有。
Windows 11 的 DWM 在单独的一层上画边框，四边都有，不受影响。

- `src/events.rs`：Windows 10 上、窗口不是最大化或全屏时，`DwmExtendFrameIntoClientArea` 把 DWM 边框往客户区
  延伸 1 像素（`update_frame_margins`，在 `WM_CREATE`、`WM_SIZE` 时更新）。前台时 DWM 自己在这一行画顶边
  （跟随“在标题栏和窗口边框上显示主题色”）；后台时 DWM 不画，由下面的边框 visual 补上（`update_top_border`，
  在 `WM_ACTIVATE`、`WM_SIZE` 时切换）。顶上留 1 像素非客户区的做法上游试过：DWM 会连整个原生标题栏一起画出来。
- `src/directx_renderer.rs`：DirectComposition 加一个根 visual，场景 visual 上面叠一个只有 1 像素的边框 visual，
  按窗口宽度拉伸（`TopBorderVisual`），切换前台 / 后台只需提交一次合成，不用重绘一帧。每帧渲染后把最上面一行清成
  透明（`draw_top_border`），让 DWM 画的边框透出来。只在 DirectComposition 的预乘 alpha 交换链上生效。
- `src/util.rs`：`is_windows_10`（内部版本号 < 22000）；后台顶边颜色 `INACTIVE_BORDER_PREMULTIPLIED`
  （预乘后 `43,43,43,128`，在 19045 版本上实测拟合，不随系统设置变化）；这类窗口在 Windows 10 上不管主题都请求
  深色边框（`configure_dwm_dark_mode` 的 `force_dark_frame`），否则浅色主题下 DWM 把伸进客户区的边框画成纯白。
- `src/window.rs`：`active` 状态、绘制时传入顶边高度、创建窗口时的深色边框参数。

和上游 PR 的差异：vendor 里 `CreateTargetForHwnd` 本来用 `false`，保留不变；`draw_top_border` 的文档注释在 PR 里
错位到了 `set_top_border_visible` 上，这里放回原处并改正。

上游合并 #63821 后（以合并后的版本为准），升级 gpui-pre 时删掉这一节对应的改动。

## 升级 gpui-pre 时

先看上游 `DirectXRenderer::resize` 是否已经解绑并 Flush；已经修复就删掉 `vendor/gpui-pre-windows`
和根 `Cargo.toml` 里的 `[patch.crates-io]`。没有修复就用新版本重新复制，再把上面的改动打上去。
