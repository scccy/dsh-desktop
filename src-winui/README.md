# dsh desktop · Windows 原生壳（WinUI 3）

Windows 上的第二种壳：窗口、菜单、三屏由 **WinUI 3** 画，所有逻辑仍然来自
`crates/dsh-lite-core`，通过 **`crates/dsh-ffi`（C ABI）** 跨进程内边界调用。

```text
src-winui/DshDesktop.WinUI/   C# + XAML，只做 UI 粘合
        │  P/Invoke（UTF-8、错误码 + 错误串、异步事件回调）
crates/dsh-ffi/               cdylib，把 Result 翻成错误码，把状态翻成 JSON 事件
        │  普通 Rust 调用
crates/dsh-lite-core/         配置、定位、启动、URL 策略、日志（零 GUI 依赖）
```

主界面仍然是 `dsh web` 的 Web GUI，装在 WebView2 里 —— 原生部分覆盖的是**壳自己的三屏**
（加载 / 错误 / 设置）与窗口本身，不是 dsh 的页面。

## 构建

```powershell
# 1) 先出桥接库（WinUI 工程只负责把它拷进输出目录）
cargo build -p dsh-ffi --release

# 2) 再出壳（未打包、自包含，不需要预装 Windows App Runtime）
dotnet publish src-winui/DshDesktop.WinUI/DshDesktop.WinUI.csproj `
  -c Release -r win-x64 --self-contained true `
  -o src-winui/out-pub
```

`DshFfiDirectory` 默认指向 `target/release/`，换 profile 时用
`-p:DshFfiDirectory=<dir>` 覆盖。

## 实测（Windows 11 build 26300，本机）

| 指标 | 数值 | 备注 |
|---|---|---|
| 发布体积 | **74.5 MB** / 280 个文件 | 未压缩、自包含、`PublishTrimmed=partial` |
| 冷启动（进程 → 窗口可见） | **约 260–390 ms** | 首次启动 14 s，为系统文件缓存冷启动 |
| 常驻内存 | 工作集约 141 MB / 私有约 80 MB | WebView2 起来之后 |
| `dsh_ffi.dll` | 2.28 MB | 仅依赖系统 DLL（无 MinGW 运行库） |

体积构成（裁剪后，最大几项）：`Microsoft.Windows.SDK.NET.dll` 23.7 MB、
`Microsoft.ui.xaml.dll` 14.4 MB、`System.Private.CoreLib.dll` 12.6 MB。
对比：用 `Microsoft.WindowsAppSDK` 元包且不裁剪时是 **209 MB**（其中
onnxruntime 20.7 MB + DirectML 17.8 MB 是 AI/ML 负载，本壳完全用不到）。

## 两个必须知道的工程陷阱

1. **`dotnet publish` 会漏掉 `<assembly>.pri`。** 未打包 + 自包含的 WinUI 应用发布后
   缺资源索引，启动即 `0xC000027B`（stowed exception），**没有窗口、没有报错**。
   `DshDesktop.WinUI.csproj` 里的 `CopyShellRuntimeExtrasToPublish` 显式补齐
   `.pri` 与三个 WinAppSDK 文件；`dotnet build` 的输出没有这个问题，所以只跑
   `build` 永远发现不了。
2. **裁剪会拆掉反射式 JSON。** `System.Text.Json` 的默认重载依赖反射元数据，
   在 `PublishTrimmed` 下 Debug 能跑、Release 会静默失配。这里用源生成的
   `ShellJsonContext`（`JsonSourceGenerationMode.Metadata`）固定契约。

## 当前缺口（如实）

| 缺口 | 状态 |
|---|---|
| 托盘图标、原生菜单、全局快捷键 | WinUI 壳尚未实现，仍在 Tauri 壳里 |
| 单实例锁 | 未实现；多开会出现多个壳争用同一个 `dsh` |
| 设置页写回 `config.toml` | 只读；`dsh_start` 的 `StartOptions` 已预留 profile / port |
| profile 预检（`--dump-config`） | 未接；失败会落到 `ready` 错误页而不是 `profile` |
| 登录 shell 环境探针 | 未接；Windows 直接继承环境，语义与 Tauri 壳一致 |
| 认证校验（首跳后是否脱 token） | 依赖 WebView2 跟随 302，未做最终 URL 断言 |
| MSIX 打包 / 代码签名 | 未做；当前是免签名绿色目录 |
| macOS / Linux | 仍是 Tauri 壳；`dsh-ffi` 已同时产出 `staticlib`（Swift 用）与 `cdylib` |
