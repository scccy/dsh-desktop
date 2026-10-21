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

## 安装包

```powershell
pwsh -File ..\scripts\build-installer.ps1
# 产出 dist\dsh-desktop-winui-setup-<version>.exe
```

Inno Setup，per-user 安装（不弹 UAC）、开始菜单 + 可选桌面快捷方式、干净卸载；
用户数据（`%LOCALAPPDATA%\dsh desktop`）卸载时保留。实测：74.6 MB 发布目录 →
**20.6 MB** 安装包。未签名，SmartScreen 会提示未知发布者。
详见 [docs/development.md §4.4](../docs/development.md#44-windows-安装包src-winuiinno-setup)。

## 标题栏

壳自绘一条 40 DIP 标题栏行（`ExtendsContentIntoTitleBar`，见 ADR-004）：

- 左侧只有**壳自己的菜单**（应用 / 视图），没有品牌块——产品名只出现在窗口标题
  （任务栏 / Alt+Tab）与"关于"对话框里；
- 右侧：最小化 / 最大化 / 关闭仍由 Windows 绘制，主题与 Snap 布局悬浮菜单保持系统行为；
- **行底色用页面侧栏填充色**（浅色 `#F9FAFB`，从运行中的页面取样），不用 Mica：Mica 会吃
  壁纸色调（实测窗口顶带 `#F9F1EF` vs 页面侧栏 `#F9FAFB`），一深一浅就成了"两块头贴在一起"；
- 拖拽面只绑定菜单右侧的空白条（`DragRegion`），菜单本身不受影响；双击该条最大化由框架处理；
- Windows 10 上若不支持标题栏定制，整行折叠、退回系统标题栏。

菜单项全部是壳的动作（打开设置 / 重试连接 / 打开日志目录 / 检查 dsh 更新 / 退出 / 刷新页面 / 关于），
**没有一项来自或作用于 dsh 页面内部**——壳不向页面注入任何脚本，那条 chrome 也就不该
长得像 dsh 网页自己的菜单。

官方 Electron 客户端那条「应用 / 编辑」是**网页自己画的**（`titleBarStyle: hidden` +
`titleBarOverlay{height:40}` + preload 注入 `data-windows-titlebar`），我们不冒充它：
`scripts/scan-asar.ps1` 可以把官方 `app.asar` 里的相关实现原样扫出来核对。

## 检查 dsh 更新
应用 → 检查 dsh 更新…：经桥接调 `dsh_check_update`，跑 `npm view @deepseek-ai/dsh version`
（30 s 兜底），由 `dsh_lite_core::update` 判定"有新版本 / 已是最新"并弹原生对话框。
**只报告，不安装**：升级命令只展示与复制，真正的升级仍由用户自己的包管理器完成。
本表与 Tauri 壳的菜单项共用同一份判定逻辑，不会有第二种"什么算更新"的定义。

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

## 三个必须知道的工程陷阱

1. **`dotnet publish` 会漏掉 `<assembly>.pri`。** 未打包 + 自包含的 WinUI 应用发布后
   缺资源索引，启动即 `0xC000027B`（stowed exception），**没有窗口、没有报错**。
   `DshDesktop.WinUI.csproj` 里的 `CopyShellRuntimeExtrasToPublish` 显式补齐
   `.pri` 与三个 WinAppSDK 文件；`dotnet build` 的输出没有这个问题，所以只跑
   `build` 永远发现不了。
2. **裁剪会拆掉反射式 JSON。** `System.Text.Json` 的默认重载依赖反射元数据，
   在 `PublishTrimmed` 下 Debug 能跑、Release 会静默失配。这里用源生成的
   `ShellJsonContext`（`JsonSourceGenerationMode.Metadata`）固定契约。
3. **标题栏行默认是非客户区。** 放在标题栏里的菜单会出现"能拿到键盘焦点、能用空格
   展开，但鼠标点击落空"的假象——只做键盘自测会误判为正常。必须用
   `InputNonClientPointerSource.SetRegionRects(NonClientRegionKind.Passthrough, …)`
   把品牌区与菜单声明为可交互区，矩形是物理像素、相对窗口左上，尺寸 / DPI /
   最大化变化后重算。回归脚本用真实鼠标点击 + UI 自动化断言（不看截图）：

   ```powershell
   # 先启动壳，然后：
   .\scripts\verify-shell-menu.ps1
   ```

## 当前缺口（如实）

| 缺口 | 状态 |
|---|---|
| 托盘图标、全局快捷键 | WinUI 壳尚未实现，仍在 Tauri 壳里（标题栏菜单已实现，见上节） |
| 单实例锁 | 未实现；多开会出现多个壳争用同一个 `dsh` |
| 设置页写回 `config.toml` | 只读；`dsh_start` 的 `StartOptions` 已预留 profile / port |
| profile 预检（`--dump-config`） | 未接；失败会落到 `ready` 错误页而不是 `profile` |
| 登录 shell 环境探针 | 未接；Windows 直接继承环境，语义与 Tauri 壳一致 |
| 认证校验（首跳后是否脱 token） | 依赖 WebView2 跟随 302，未做最终 URL 断言 |
| MSIX 打包 / 代码签名 | 未做；当前是免签名绿色目录 |
| macOS / Linux | 仍是 Tauri 壳；`dsh-ffi` 已同时产出 `staticlib`（Swift 用）与 `cdylib` |
