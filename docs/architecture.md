# 架构

> 本文描述目标结构，并在每节标注当前落地程度。需求原文见 [项目要求.md](../项目要求.md)。

## 1. 分层

```text
┌──────────────────────────── Tauri App (Rust) ────────────────────────────┐
│ src-tauri/                                                               │
│   main / app_runtime / scheme / bridge / connection(旧路径) /            │
│   lifecycle / window / tray / menu / shortcuts / state                   │
│   platform/        唯一的平台差异层                                       │
├──────────────────────────────────────────────────────────────────────────┤
│ crates/dsh-lite-core/   纯 Rust，无 GUI 依赖                              │
│   config / cli / dsh_locate / env_probe / desktop_host / version_gate /   │
│   frontend_dist / app_server（静态 + 流式 HTTP + WS + CORS）/             │
│   launcher / url_policy / logging / update / job                         │
└──────────────────────────────────────────────────────────────────────────┘
        │ 启动 / IPC                          ▲ initialization_script（桥）
        ▼                                     │
┌──────────────────────────┐      ┌──────────────────────────────────────┐
│ Desktop Host 子进程       │      │ Renderer: dsh-app://app/              │
│ Node · 全局 dsh           │◄────►│  官方前端 dist（全局安装内读取）        │
│ runProfile(desktop)       │ HTTP │  window.dshDesktop / boot 桥          │
│ 回报 url + injections     │  WS  │  data-platform=darwin                 │
└──────────────────────────┘      └──────────────────────────────────────┘
```

默认启动是**三层**的：Rust 壳（窗口、`dsh-app` scheme、回环代理、preload 桥）→ Desktop Host 子进程（desktop profile）→ Renderer（`dsh-app://app/`，读全局安装里的 `dsh-web-frontend/dist`）。渲染层 origin 与动态流量的拆分见 [ADR-006](#adr-006-渲染层-origin-拆分)。`--url` / `--port` 或 `webFallback = true` 时退回旧的两层路径（壳直接加载一个已在运行的 `dsh web`）。

窗口结构：窗口本身不带 webview（`tauri::window::WindowBuilder` 建裸窗口），`content` 子 webview 承载渲染层。Windows / Linux 额外挂一个 40 DIP 的 `header` 子 webview 画标题栏；macOS 用 `hiddenInset`（`title_bar_style(Overlay)` + `hidden_title(true)` + 透明窗口），红绿灯叠在页面上，窗口读作「侧栏 + 内容」两块。`add_child` 属于 Tauri 的 `unstable` feature，升级时要先确认 API 仍在。

依赖方向严格单向：`src-tauri` 依赖 `dsh-lite-core`，反之不成立。`dsh-lite-core` 不得 `use tauri::*`、不得创建窗口；定位、Host 启动器、代理、WS、静态文档、版本闸门等全部可在无显示器 CI 上单测。

| 模块 | 职责 | 不负责 |
|---|---|---|
| `config` | 配置默认值、TOML 解析、字段校验、平台配置目录 | 命令行覆盖（在 `cli`） |
| `cli` | 参数解析与 `--help` 文本 | 与应用其余部分交互 |
| `dsh_locate` | 按 FR-1 顺序定位可执行文件（含 `@deepseek-ai/dsh` 包目录） | 启动进程 |
| `desktop_host` | Host 子进程：spawn、`DSH_HOST_READY/FATAL` 解析、优雅停止、进程组回收 | 代理与窗口 |
| `version_gate` | `dsh --version` 语义化比较（含 prerelease）、最低版本拒绝 | 启动 |
| `frontend_dist` | 从全局安装解析 `dsh-web-frontend/dist`，校验 index | 提供文件 |
| `app_server` | 回环 origin：静态 dist、流式 HTTP 代理、WS 代理、CORS/preflight、token→cookie 认证 | 窗口与 GUI |
| `env_probe` | macOS/Linux 登录 shell 环境读取与合并规则 | Windows 环境（直接继承） |
| `launcher` | 旧路径：`dsh web` 子进程与 `dsh web:` 行解析 | 三层栈（在 `desktop_host`） |
| `url_policy` | token 提取与脱敏、同源判断、最终 URL 校验 | 网络请求 |
| `logging` | 滚动日志、崩溃报告、脱敏写出 | 决定记什么 |
| `update` / `job` | npm 版本探测 / 进程树回收 | UI |

**落地状态**：三层链路已在 macOS 真机端到端跑通（见 [desktop-parity §7.2](desktop-parity.md#72-p1--p2-进展2026-10-25-真机验收)）；`cargo test --workspace` 281 passed；clippy/fmt 干净；release 二进制 6.9 MB。Windows/Linux 尚未复验新架构。

### 1.1 平台差异层

窗口、菜单、托盘、外链打开、控制台隐藏这些"按平台走不同代码"的行为，全部收敛在 `src-tauri/src/platform/`：

```text
src-tauri/src/platform/
  mod.rs      # 唯一的 if/分支入口：对上层暴露与平台无关的函数
  macos.rs    # 菜单栏 + Dock、open 交接
  unix.rs     # 托盘 + xdg-open 交接
  windows.rs  # 托盘 + url.dll 交接、CREATE_NO_WINDOW
```

约定（写新代码时按此执行）：

- **`#[cfg(target_os = ...)]` 只允许出现在 `src-tauri/src/platform/`**。`main.rs`、`window.rs`、`connection.rs` 等文件不得直接判平台，而是调用 `platform::` 暴露的函数，由 `platform/mod.rs` 用 `#[path]` 与 `use ... as imp` 选择实现；
- **`crates/dsh-lite-core` 内不得出现平台 UI 分支**。`cfg(unix)` / `cfg(windows)` 在该 crate 里只用于进程、环境、路径等系统行为（可执行文件与扩展名搜索、登录 shell 探针、进程组 / Job Object、平台配置目录）；
- 新增一个平台差异时，先问"它是否属于 UI 栈"：属于就写进 `src-tauri/src/platform/<平台>.rs`，不属于（可被单测覆盖的系统行为）就写进 `dsh-lite-core` 并补单测；
- 这一层同时是 ADR-002 的退路：将来某个平台要换原生壳，只需替换该平台自己的文件，`dsh-lite-core` 原样复用。

## 2. 启动时序

默认（三层，`webFallback = false`）：

```text
1  解析 CLI：--url / --port / --dsh-path / --profile / --no-env-probe / --verbose
2  加载 config.toml（首次启动写默认文件，不覆盖已有文件）
3  单实例：已有实例则显示并聚焦；带 --url 时交给旧路径重载
4  建窗口与 content webview，先显示 dsh-app://shell/loading.html
5  定位全局 dsh（--dsh-path → config.dsh.path → DSH_LITE_DSH_PATH → PATH → 常见安装位置）
6  dsh --version 版本闸门（< 0.2.1-rc.2 拒绝并给升级命令）
7  解析渲染层 dist：<dsh包>/node_modules/@deepseek-ai/dsh-web-frontend/dist
8  登录 shell 环境探针（macOS/Linux `<shell> -ilc 'env -0'`；Windows 继承）
9  起 Desktop Host：node host/desktop-host.mjs --dsh <pkg> --project $DSH_HOME/profiles/desktop
      ├─ 等 DSH_HOST_READY { url, origin, protocol, protocolVersion, injections }
      └─ DSH_HOST_FATAL 带 code=version/profile → 对应错误页
10 GET 一次性 token URL → 303 + set-cookie，cookie 只留在 Rust 内存
11 起回环 app server（127.0.0.1:0）：静态 dist + 流式 HTTP 代理 + WS 代理 + CORS
12 桥交付 boot payload（injections + streamBaseUrl），窗口导航到 dsh-app://app/
13 Renderer：module bundle 从 scheme 读，插件脚本/API/WS 全部走回环 origin
```

旧路径（`--url` / `--port` / `webFallback = true`）：定位 dsh → profile 预检 → 起 `dsh --profile <name>` → 匹配 stdout `dsh web: <url>` → 导航 token URL → 校验 URL 已脱 token。这条路径**不注入桥**。

子进程生命周期由两层保障，且不依赖退出钩子：正常路径 `stop()`（先请求退出、超时强杀），异常路径由 Windows Job Object / Unix 进程组兜底 — 本进程一死，整棵进程树被回收。

退出时序（顺序不可交换）：

```text
a  触发（关闭窗口且 closeAction = quit / 托盘退出 / Cmd+Q）
b  停止 Host：stdin "shutdown" → 超时强杀整个进程组
c  停止 app server（关闭监听并断开连接）
d  旧路径的子进程同样在 b/c 停止（附着模式跳过：壳不拥有那些进程）
e  刷写日志、释放单实例锁、退出
```

`closeAction = hide` 时（macOS 默认，Windows/Linux 可选）只隐藏窗口，Host、app server 与托盘/Dock 图标保留。

## 3. 错误分类

所有面向用户的失败都进入**错误页**（`dsh-app://shell/error.html` 或旧路径的壳错误页），原文展示底层报错，给出可执行动作；同时写日志。

| `ErrorKind` | 页面 `kind=` | 触发点 | 错误页动作 |
|---|---|---|---|
| `DshNotFound` | `dsh-not-found` | 定位失败 / `@deepseek-ai/dsh` 包目录不可解析 | 显示安装命令、打开设置 |
| `Version` | `version` | `dsh --version` 低于 0.2.1-rc.2（预检或 Host `FATAL code=version`） | 显示升级命令 |
| `Profile` | `profile` | profile 目录不可用、Host `FATAL code=profile`、缺 `dsh-web-frontend` | 创建 profile 提示、升级 dsh |
| `Ready` | `ready` | Host 超时/退出/其它 `FATAL`、app server 起不来、页面 boot 失败 | 附 stderr 尾部、打开日志目录 |
| `Auth` | `auth` | token URL 非 303 / 无 set-cookie / 旧路径 URL 仍带 token | 重试连接 |

崩溃报告文件名 `crash-<UTC 时间>-<来源>.log`，来源取 `startup` / `renderer` / `shell`，保留最近 10 份。

## 4. 日志位置

| 平台 | 日志目录 |
|---|---|
| Windows | `%APPDATA%\dsh desktop\logs\` |
| macOS | `~/Library/Application Support/dsh desktop/logs/` |
| Linux | `~/.config/dsh desktop/logs/` |

- 滚动策略：单文件 ≤ 5 MiB，保留 5 个；
- 记录内容：启动时间、解析到的 `dsh` 路径与其 `--version`、实际启动参数（token 已脱敏）、环境探针摘要（**只记变量名与来源，不记值**）、子进程 stdout/stderr、窗口与退出事件；
- 子进程输出的解码规则：**UTF-8 优先 → 系统 OEM 代码页兜底 → 有损最后**（`dsh_lite_core::console_text`）。Node 侧工具写 UTF-8，但任何经 `cmd.exe` 的报错（命令不存在、`.cmd` 垫片失败、`chcp` 提示）写的是 OEM 代码页；此前用 `from_utf8_lossy` / `read_to_string` 会把整段中文变成乱码或直接读成空串。实测 `chcp 65001` 无法改变重定向输出的编码，所以只在读取端容错，不试图改造子进程；
- 崩溃报告：错误链、壳版本、`dsh` 版本、平台信息，**不含**环境变量值与 token；
- 硬规则：日志、崩溃报告、窗口标题、历史记录里都不得出现 token 明文；`url_policy` 的脱敏在写日志之前完成，`--verbose` 也不放宽这一点。

## 5. 安全约束的落点

| 约束 | 实现位置 |
|---|---|
| 渲染层只允许 `dsh-app://app`、`dsh-app://shell` 与本窗口；其它外跳系统浏览器 | `window::classify_navigation` + `main.rs::handle_navigation` |
| Host cookie 只在 Rust 内存；webview 永远收不到 `Set-Cookie`（响应头被扣留，`dsh-app` 页面的 cookie jar 本身也被 WKWebView 丢弃） | `app_server::proxy` / `scheme` 的 `WITHHELD_RESPONSE_HEADERS` |
| 动态请求的跨源白名单只有 `dsh-app://app`、`dsh-app://localhost`、`http://dsh-app.localhost` 与本 server origin；其它 Origin 403 | `app_server::AppState::is_allowed_origin` + preflight |
| 桥只在应用文档生效：命令校验 webview 标签 + 文档 origin（主 frame），其它来源拒绝 | `bridge::assert_sender` |
| release 构建关闭 WebView 远程调试；调试构建的 `DSH_DEBUG_EVAL` 必须显式设环境变量才启用 | `cfg(debug_assertions)` + 环境变量 |
| token 只存在于一次性认证请求；日志/错误页/崩溃报告都经脱敏 | `url_policy::redact_*`，`authenticate_host` 不返回 URL |
| 不读不缓存不上传任何凭据 | `env_probe` 只记变量名与来源，壳不落盘任何值 |

## 6. 关键决策（ADR）

以下按时间顺序记录决策本身、背景、被否掉的替代方案与代价，以及由此产生的后果。**当前有效的是 ADR-005 与 ADR-006**；ADR-001 / 003 / 004 保留原文，只在标题上标注状态，用来解释「为什么当初那样选」，不要拿它们当现状。

### ADR-001 窗口装饰使用系统原生标题栏（已被 ADR-005 取代）

**决策**：Windows / macOS / Linux 一律保留系统原生标题栏（Tauri 窗口配置 `decorations: true`），窗口标题固定 `dsh desktop`，不自绘任何窗口控件。

**背景**：官方 Electron 桌面端用 `titleBarStyle: 'hidden'` + `titleBarOverlay`（`apps/desktop/src/main.ts`）做 40 DIP 定制标题栏，再由 preload 注入 `data-windows-titlebar` 与 `--dsh-windows-titlebar-height`，让前端页面为标题栏让位。Tauri v2 没有这条路径的等价开关：既要保留系统按钮，又要把它们叠进自绘标题区并同步配色，没有现成能力。要复刻只能 `decorations: false` 从零自绘最小化 / 最大化 / 关闭，hover、按下、禁用态、明暗主题同步、双击最大化、窗口贴边行为全部自研，还得和 Web 前端约定顶部让位尺寸。

**被否掉的替代方案与代价**：

| 方案 | 代价 | 结论 |
|---|---|---|
| 自绘标题栏（`decorations: false`） | 三平台各自实现一套按钮状态与命中区；主题同步要跟着系统设置走；双击最大化、Aero Snap 等系统行为要么自研要么丢失 | 否掉：收益只有观感，成本是三平台永久性的自维护代码 |
| 复刻官方"注入 JS 让前端让位" | 向页面注入 JS 与本项目 §5「不向页面注入任何 JS」直接冲突，等于把一条明确的安全非目标改成为观感让路的例外 | 否掉：违反已定非目标 |
| `titleBarStyle` 一类平台特化样式 | macOS 上的透明/隐藏标题栏会把窗口拖拽、红绿灯按钮的位置变成页面责任，页面是 dsh 的 Web UI，我们无权改它的布局 | 否掉：把壳的观感问题转嫁给上游页面 |

**后果**：

- 观感与官方 Windows 客户端**不同**：我们是系统原生标题栏加系统按钮，这是有意接受的产品差异，不声称"对齐了官方 Windows 观感"；
- 窗口标题是唯一的身份线索，固定为 `dsh desktop`，不做自绘标题文字、不做标题栏内嵌控件；
- 三平台窗口行为（拖拽、贴边、最大化、系统菜单）完全交给操作系统，壳不拦截、不复刻；
- 若将来必须自绘，先改本 ADR 并说明如何绕开 §5 的注入限制，再动代码。

> **2026-10-21 状态变更**：Windows 原生壳已改为**自绘标题栏行**（见 [ADR-004](#adr-004-windows-原生壳自绘标题栏行内容属于壳已随-adr-003-一并废止)）。本条的否决前提之一是"Tauri 没有等价开关"，该前提只对 `src-tauri` 成立；WinUI 有 `ExtendsContentIntoTitleBar`，且 WebView2 排在标题栏行之下、不需要页面让位，因此注入 JS 不再是自绘的前提。macOS / Linux（Tauri 壳）仍按本条执行。

> **2026-10-23 最终状态**：本条整体废止。三平台现在都自绘标题栏（`decorations: false` + `header` webview，见 [ADR-005](#adr-005-回到单一-tauri-壳用双-webview-画标题栏)）。但本条最有价值的一条结论被继承下来：标题栏不依赖向页面注入 JS。2026-10-25 起，应用文档会注入 preload 桥（官方契约的一部分，见 [ADR-006](#adr-006-渲染层-origin-拆分)），但那是为了对齐桌面能力，不是为了给标题栏让位；「注入 JS 让页面让位」仍然否掉。

### ADR-002 单一 Tauri 代码线，不做 macOS / Windows 双原生壳

**决策**：三个平台共用同一条 Rust + Tauri v2 代码线，不为 macOS 另写 Swift/AppKit 壳、不为 Windows 另写 WinUI/C# 壳。

**背景**：开发机是 Windows，macOS 原生壳在本机无法编译、无法运行、无法验证，只能盲写并靠 CI 反复试错，一次改动要等 CI 排队加构建才能拿到信号。而这个项目里价值最高的逻辑与 UI 栈无关：dsh 可执行定位、`--profile --dump-config` 预检、`env -0` 登录 shell 探针、`dsh web:` 就绪行解析、token 脱敏、无孤儿进程的退出清理、滚动日志与崩溃报告。双线方案要么把这套逻辑复制两遍（两份行为会漂移，测试也要写两遍），要么每种语言各维护一层 FFI（`staticlib` / `cdylib` 绑定代码同样要跟改）。此外 Linux 在"macOS 原生 + Windows 原生"的菜单里没有归属，三平台承诺会退化成两平台。

**被否掉的替代方案与代价**：

| 方案 | 代价 | 结论 |
|---|---|---|
| macOS Swift/AppKit + Windows WinUI 双原生 | 单平台观感最好、系统集成最彻底；但 macOS 线在本机不可验证，逻辑与测试双份维护，Linux 无归属 | 否掉：可验证性与可维护性的损失大于观感收益 |
| 保留双原生骨架但只实现 Windows | 仓库里长期存在一条死线，接口一旦漂移就要在动手时重做 | 否掉：不写用不上也验证不了的代码 |
| 现在就把 core 编成 `staticlib`/`cdylib` 供未来复用 | 当前没有消费者，等于提前维护一层没有测试覆盖的 C ABI | 否掉（保留为退路，见"后果"） |

**后果**：

- 所有平台差异收敛到 `src-tauri/src/platform/`（见 §1.1），核心逻辑全部在 `crates/dsh-lite-core`，在本机就能 `cargo test` 覆盖；
- 观感上限就是 Tauri + 系统 WebView 提供的能力：托盘、原生菜单、单实例、外链策略都有，定制标题栏没有（见 ADR-001）；
- macOS 侧的验收只能靠 CI 构建通过 + 真机手测，文档里不得出现"已验证 macOS 行为"这类未发生的声明；
- 退路不锁死：`crates/dsh-lite-core` 是零 GUI 依赖的纯 Rust 库，将来若某平台必须原生，可编成 `staticlib`（cbindgen → Swift）或 `cdylib`（P/Invoke → C#）复用，不需要重写逻辑。

> **2026-10-21 状态变更**：本条在 **Windows 一侧**已被 [ADR-003](#adr-003-windows-采用-winui-3-原生壳--rust-c-abi-桥接已被-adr-005-取代) 取代，macOS / Linux 仍沿用本条（Tauri 单线）。取代的事实前提是产品明确接受"安装体积 ~75 MB"这一代价，而本条当初的核心否决理由（体积、双份维护）因此需要重新计价——重算结果见 ADR-003。

> **2026-10-23 恢复**：ADR-003 及其 C# 壳已被 [ADR-005](#adr-005-回到单一-tauri-壳用双-webview-画标题栏) 废止，本条重新成为三平台唯一路线。而且这次是名副其实的一条线：只有一份 `src-ui/`，没有第二套 UI 实现，也没有 C ABI 层。本条「退路不锁死」的结论同样作废——`dsh-ffi` 已删除，若将来真的需要非 Tauri 客户端，再单独决策并重建。

### ADR-003 Windows 采用 WinUI 3 原生壳 + Rust C ABI 桥接（已被 ADR-005 取代）

> **状态：已废。** 2026-10-23 起 Windows 与其他平台共用 `src-tauri` 一套壳，`src-winui/` 与 `crates/dsh-ffi/` 已删除。本 ADR 保留原样，作为「为什么当初会走双壳」的记录；取代它的决策与理由见 [ADR-005](#adr-005-回到单一-tauri-壳用双-webview-画标题栏)。

**决策**：Windows 上新增 `src-winui/`（C# + WinUI 3）作为原生壳，逻辑不重写，通过 `crates/dsh-ffi` 的 C ABI 复用 `dsh-lite-core`。macOS / Linux 继续用 `src-tauri/`，直到各自被单独决策。

**背景**：ADR-002 的否决前提是"安装包 < 15 MB"与"单线可维护"。产品随后明确：体积 100 MB 以内可接受，目标机器以 Windows 11 为主。前提变了，账就要重算——桥接把"逻辑复制两遍"变成"逻辑一份、绑定一层"，于是被否掉的主要代价里只剩体积与 UI 验收两条。

**关键技术事实**（决定了为什么是桥接而不是重写）：

| 事项 | 结论 |
|---|---|
| 逻辑复用 | `dsh-lite-core` 零 GUI 依赖，`cdylib` 可被 P/Invoke 直接消费；实测 `dsh_ffi.dll` 仅依赖系统 DLL，无 MinGW 运行库绑定 |
| 纯 Rust 写 WinUI 3 | 不可行：XAML 的 `.xbf` 与 xaml typeinfo 由 msbuild / XamlCompiler 生成，没有 Rust 侧工具链 |
| WinRT/COM 进程内组件 | 可行但样板更多，收益仅"接口长得像 WinRT"，否决 |
| 独立进程 IPC（sidecar） | 可行且最适合跨语言，但当前只有两种壳，多一个进程的生命周期管理没有换到对应收益；保留为将来加第三种客户端时的选项 |

**被否掉的替代方案与代价**：

| 方案 | 代价 | 结论 |
|---|---|---|
| 继续只用 Tauri（即 ADR-002 原样） | 观感上限锁死在"系统 WebView + 系统标题栏" | 否掉：产品已明确要原生观感 |
| 纯 Rust 直接写 WinUI 3 | 没有 XAML 编译工具链，实际不可行 | 否掉 |
| Rust 逻辑进程 + C# UI 进程（IPC sidecar，ADR 里保留的方案） | 多一层协议与进程生命周期；当前只有两个消费者 | 暂缓：桥接先上，若将来需要第三种客户端再迁 |
| 用 `Microsoft.WindowsAppSDK` 元包 | 带上 AI/ML/Search/Widgets，多 38 MB 死重量 | 否掉：改用 `Microsoft.WindowsAppSDK.WinUI` 拆分包 |

**后果**：

- **边界是一份契约**：`crates/dsh-ffi` 的 ABI 版本号（`ABI_VERSION = 1`）+ UTF-8 字符串所有权规则 + 错误码/错误串 + 异步事件回调；C# 侧校验版本不符即拒绝启动。新增能力必须改 ABI 并同时改两边，不允许单侧扩展；
- **UI 侧只做粘合**：`src-winui` 里没有定位、没有启动、没有日志分片逻辑，`DshBridge` 只做 P/Invoke + JSON 事件 marshal 到 UI 线程；
- **体积代价已可量化**：发布 74.5 MB / 280 文件（自包含、未压缩、partial trim），冷启动约 260–390 ms，常驻工作集约 141 MB。这组数字是接受本 ADR 的价码，不再是未知量；
- **两条工程陷阱必须写进构建里**：`dotnet publish` 会漏 `<assembly>.pri`（启动即 stowed exception 且无报错）、裁剪会拆掉反射式 `System.Text.Json`（Debug 正常、Release 静默失配）。两者都已在工程文件里做了显式处理，见 `src-winui/README.md`；
- **macOS / Linux 不受影响**：`crates/dsh-ffi` 同时产出 `staticlib`，将来上 Swift 时可复用同一份契约；
- **两壳并存期的行为一致性靠契约和文档，不靠记忆**：错误分类词汇（`dsh-not-found` / `version` / `spawn` / `ready` / `auth` / `internal`）与三屏语义是共享契约，新增错误种类必须两边同时落地。

### ADR-004 Windows 原生壳自绘标题栏行，内容属于壳（已随 ADR-003 一并废止）

> **状态：已废。** 但它的两条结论被 ADR-005 继承：标题栏是壳的功能、菜单项只能是壳的动作。2026-10-25 起应用文档会注入 preload 桥（ADR-006），但标题栏本身仍不依赖注入。40 DIP 的行高、与页面侧栏同色的底、把菜单声明为可交互区这三条实现约定，也照搬到了新的 `header` webview 上。

**决策**：`src-winui` 用 `ExtendsContentIntoTitleBar` 自绘一条 40 DIP 标题栏行：只有**壳自己的菜单**（应用 / 视图），右侧最小化 / 最大化 / 关闭仍由 Windows 绘制，行底色取页面侧栏填充色。不向承载的 dsh 页面注入任何脚本。

**背景**：官方 Electron 客户端的做法（已从 `app.asar` 逐条核对）：`titleBarStyle: "hidden"` + `titleBarOverlay{height:40}` 让 Windows 把系统按钮画进 40 DIP 覆盖层，preload 再往页面注入 `data-windows-titlebar` 与 `--dsh-windows-titlebar-height`；页面侧 CSS 命中 `[data-windows-titlebar]` 后 `padding-top` 让位，并把 `:before` 设为 `-webkit-app-region: drag`。也就是说，**那条「应用 / 编辑」是网页自己画的**，React 侧用 `document.documentElement.hasAttribute("data-windows-titlebar")` 判断是否进入桌面端布局。

产品明确：这条标题栏是壳的功能。因此本 ADR 只复刻**布局位置**与**观感**，不复刻**内容来源**。

**被否掉的替代方案与代价**：

| 方案 | 代价 | 结论 |
|---|---|---|
| 注入 `data-windows-titlebar` 让网页自己画那条 chrome（观感最接近） | 网页会进入"宿主是 Electron"的分支；其拖动依赖 `-webkit-app-region`，而 WebView2 不实现该属性 → 40 DIP 变死区；圆角、侧栏收起等一整套布局都按 Electron 验证 | 否掉：把壳的观感押在上游未承诺的私有契约上 |
| 保持系统标题栏（ADR-001 原样） | 与官方观感差异明显，而产品已把这条列为本壳能力 | 否掉 |
| 全自绘包含最小化/最大化/关闭 | 会丢失 Windows 11 的 Snap 布局悬浮菜单、主题同步与系统无障碍语义 | 否掉：系统按钮继续交给系统 |

**实现要点（三个必须记住的陷阱）**：

1. **标题栏带默认是非客户区**：菜单会"能拿到键盘焦点、能用空格展开，但鼠标点击落空"——只用键盘自测会误判为正常。必须用 `InputNonClientPointerSource.SetRegionRects(NonClientRegionKind.Passthrough, …)` 把菜单声明为可交互区，且矩形是**物理像素、相对窗口左上**，窗口尺寸 / DPI / 最大化切换后都要重算。回归检查脚本：`src-winui/verify-shell-menu.ps1`（对两个菜单分别做真实鼠标点击 + UI 自动化断言，不是截图）。
2. **`SetTitleBar` 只绑定拖拽面**：只给菜单右侧的空白条（`DragRegion`），整行绑成拖拽区会吞掉菜单点击；双击最大化仍由框架处理。
3. **底色必须跟页面侧栏一致，不能用 Mica**：Mica 会吃壁纸色调（实测窗口顶带 `#F9F1EF`，页面侧栏 `#F9FAFB`），两者一深浅不同就成了"两块头贴在一起"。行底色改为页面侧栏填充色（浅色 `#F9FAFB`，从运行中的页面取样；深色 `#17181B` 为近似值，切换深色主题时需复核）。

**后果**：

- 观感接近官方但**不等于**官方：菜单项是壳的动作（打开设置 / 重试连接 / 打开日志目录 / 退出 / 刷新页面 / 关于），不是网页的编辑菜单；WinUI 的标题栏带只有 32 / 48 DIP 两档，而 Electron 是任意 40，所以系统按钮与 40 DIP 行不同高——这是平台差异，不伪装；
- **行里不放品牌块**：产品名与图标只出现在窗口标题（任务栏 / Alt+Tab）与"关于"对话框里。放了品牌块就等于在页面自己的 logo 行上方再加一条同字号的信息条，重复且更显"两块头"；
- 页面左上角不会出现官方的 16 DIP 圆角与内容让位：那是网页在 `[data-windows-titlebar]` 下自己画的，壳不注入就不该有；
- 标题栏不依赖页面注入（§5）；应用文档的 preload 桥属于 ADR-006，与标题栏无关；
- 托盘与原生菜单已实现；原生输入转发（焦点在 iframe / 内嵌 webview 时的按键）留给 P3，见 §7 对照表。


### ADR-005 回到单一 Tauri 壳，用双 webview 画标题栏

**决策**：删除 `src-winui/` 与 `crates/dsh-ffi/`，Windows / macOS / Linux 共用 `src-tauri` 一套壳。窗口改为「裸窗口 + `header` / `content` 两个子 webview」，标题栏由壳自己的 `src-ui/header.html` 绘制；菜单用系统原生 popup 弹出。

**背景**：ADR-003 的前提是「Windows 观感优先、100 MB 以内可接受」，为此接受了 C# 壳加 C ABI 桥接。跑通之后的真实账是另一回事：

| 代价 | 具体表现 |
|---|---|
| 功能不同步 | WinUI 壳没有托盘、没有设置写回、没有 profile 预检、没有终态 URL 断言 |
| 改一处要动两边 | 任何行为变化都要同时改 Rust 与 C#，并跟着改 ABI 版本 |
| 三屏两份实现 | XAML 一套、HTML 一套，观感靠人工对齐 |
| 平台覆盖 | WinUI 只覆盖 Windows，而产品要的是全平台一套 |

**被否掉的替代方案**：

| 方案 | 代价 | 结论 |
|---|---|---|
| egui / iced / Slint 纯自绘 | 都不提供内嵌 WebView，只能把 WebView 当无边框子窗口覆盖；现成的桥接 crate 都是实验性的（`iced_wry` 2 star、`egui_webview` 标注 experimental、Slint 的 WebView 仍是 open request）；现有 Fluent 观感要从零重画 | 否掉：为三屏去赌不成熟的 overlay |
| Dioxus（RSX + wry） | 渲染底座与 Tauri 完全相同（同一个 wry + 系统 WebView），只换来「UI 用 Rust 写」；打包、托盘、单实例、更新链路全部重做 | 否掉：收益是语言偏好，代价是整条工程链 |
| 保留双壳 | 功能永远差一截，文档必须处处区分 | 否掉：与「一套代码全平台」直接冲突 |

**为什么不是一个 webview 里切页面**：标题栏要常驻在 `dsh` 页面之上，而单 webview 只能整页导航。靠注入浮层实现就是 ADR-004 已经否决的那条路（WebView2 不实现 `-webkit-app-region`，拖动会变成死区，且要把壳的观感押在上游未承诺的私有属性上）。拆成两个 webview 是唯一完全不碰 `dsh` 页面的做法。

**后果**：

- 一套壳三平台，三屏只有一份 HTML/CSS；
- 标题栏的拖动、双击最大化、最大化/还原图标切换、三个窗口按钮全部由壳自己实现（`shell_window_action` / `shell_drag` / `shell_window_state`）；
- 菜单必须用原生 popup（`shell_menu` → `Menu::popup_at`）：`header` webview 只有 40 DIP 高，HTML 下拉一出边界就被 webview 矩形裁掉；
- 失去 WinUI 的 Mica 与标题栏右键的 Snap 悬浮菜单。底色改用页面侧栏填充色（浅 `#f9fafb` / 深 `#17181b`）保证与内容无缝；窗口拖动走 `WM_NCLBUTTONDOWN` 的 HTCAPTION，Windows 的贴边 Snap 仍由系统接管；
- `tauri` 打开 `unstable` feature（`Window::add_child`），升级 Tauri 时必须先确认该 API 仍在、语义未变；
- `auto_resize` 不能用：它让子 webview 随窗口等比缩放，而这里需要 `header` 恒为 40 DIP、`content` 吃掉剩余高度。所以 `main.rs::layout_webviews` 在每次 `Resized` / `ScaleFactorChanged` 后按物理像素重新摆位；
- 文档、脚本、CI 里一切「两条壳」的表述作废（含随之删除的 `build-installer.ps1` / `redeploy.ps1` / `verify-shell-*.ps1`）。

### ADR-006 渲染层 origin 拆分

**决策**：渲染层文档继续从 `dsh-app://app/` 加载（与官方一致）；**但动态流量不走 scheme** —— `fetch` / `XMLHttpRequest` / `EventSource` / WebSocket 一律指向 Rust 起的回环 app server（`http://127.0.0.1:<随机端口>`），由它做流式 HTTP 代理与 WS 代理，并在本地回 CORS。cookie 只由 Rust 持有并在代理端注入，页面永远拿不到。即 docs/desktop-parity.md §6.1 的「静态 dist 走 scheme，代理按接口类型拆分」。

**背景（2026-10-24 macOS WKWebView 真机实证，两轮 GUI）**：

| 能力 | 结论 | 证据 |
|---|---|---|
| 自定义 scheme 响应流式 | **不可用** | handler 分 3 段写出（1003/2006/3011ms），页面 3013ms 才拿到响应头且只读到 1 个 chunk；回环 http 对照组 1077/2082/3086ms 增量到达 |
| 同步 handler | 不可用且更差 | 响应同样被缓冲，还把主线程阻塞 3003ms |
| secure context | 可用 | `isSecureContext=true`、`crypto.randomUUID`/`subtle` 可用 |
| Media Capture | **缺失** | `navigator.mediaDevices === undefined`（P3 麦克风必须原生采集） |
| 页面 → 回环 POST | 完整 | 16KB body 的 len/head/tail/sum/FNV 全等；`dsh-app://app/__echo-body` 也完整 |
| 页面 → 回环 fetch 的 CORS | 必须 | 无 CORS 头 → `TypeError: Load failed`（请求已达服务端）；回显 `Access-Control-Allow-Origin: dsh-app://app` 后通过；JSON POST 触发 preflight |
| 页面 → 回环 WS | 可用 | open@12ms，握手 `Origin: dsh-app://app`，正常收发；`document.cookie` 写入被丢弃、Set-Cookie 不落 webview jar |

证据全文见 [docs/spikes/wkwebview-scheme.md](spikes/wkwebview-scheme.md)。

**被否掉的替代方案**：

| 方案 | 代价 | 结论 |
|---|---|---|
| 纯 `dsh-app://` 代理（复刻 Electron `protocol.handle`） | WKWebView 的 `WKURLSchemeHandler` 只能「完整响应」：SSE/流式回复 3 秒才一次性到达，上传与长连接都退化 | 否掉：这是平台能力缺口，不是实现问题 |
| 页面直接由回环 http 出（同源、零 shim） | origin 与官方不同（`http://127.0.0.1` vs `dsh-app://app`），且要放弃 scheme 文档 | 不作为默认；**保留为回退开关**（shim 出问题时一键切换） |
| 给每个接口单独判断走 scheme 还是回环 | 需要维护一张易腐的接口清单 | 否掉：按「动态流量」一刀切更稳 |

**实现要点**：

1. scheme handler（`src-tauri/src/scheme.rs`）只处理 `GET/HEAD`：静态 dist、`index.html` 注入 `globalThis.__DSH_BOOT_READY__`、`/assets/*`、favicon、manifest，以及前端按相对路径加载的模块脚本 `plugins/...`（缓冲式转发到 Host）。不承担流式、大上传、WS。
2. 回环 app server（`crates/dsh-lite-core/app_server`）：HTTP 流式代理（请求体与响应边读边转）、WS 代理（升级时注入 cookie/origin/sec-fetch-site）、`/plugins/*` 强制 `no-store`；CORS 只回显白名单 Origin（macOS/Linux 为 `dsh-app://app`，Windows 为 `http://dsh-app.localhost`，Tauri 的自定义 scheme origin 两平台不同），preflight 本地 204、不落上游；不带 `Allow-Credentials`（页面不需要 cookie）。
3. preload shim（`src-tauri/bridge/preload.js`，页面脚本前执行）：`fetch`（string/URL/Request）与 `XMLHttpRequest`（open/send 缓存重放）把 `dsh-app://app/...` 与相对路径重写到 app server origin；WebSocket 不 shim，靠 boot 返回的 `streamBaseUrl`；EventSource P1 不 shim（只有 hmr/documentpreview 用，dev-only）。
4. boot 契约：`__DSH_BOOT_READY__` + `dshDesktopBoot.ready() → { injections, streamBaseUrl: <app server origin> }`，`__DSH_TRANSPORT__ = { ownsHost: true, streamBaseUrl }`。
5. `data-platform` 必须在 `dsh-client-shortcuts` 的 `detectEnvironment` 之前同步出现在 `<html>` 上，否则前端判成 web，直接报 `Desktop keyboard bridge unavailable`。

**后果**：

- 安全模型与官方一致且更简单：webview 永远收不到 `Set-Cookie`（会被丢弃），cookie 只在 Rust 内存里；页面 origin 与代理 origin 分离，CORS 白名单在本地回显。
- 新增两类风险：**R2** scheme 缓冲意味着大模块脚本/大响应会一次性进内存（单文件已 5.3MB 级，实测可接受）；**shim 覆盖面**（fetch/XHR 之外的 API，如 `sendBeacon`、worker 内的 fetch，需按需补）。
- **R1 麦克风**：WKWebView 不实现 Media Capture，P3 的麦克风改为原生能力（AVCaptureSession 采集后喂给页面），不再算「web 能力对齐」。
- Windows/Linux 未验证：Tauri 自定义 scheme 在 Windows 上是 `http://dsh-app.localhost`，CORS 白名单与 `data-platform` 都要按平台复核。
- 回退开关：app server 本来也能直出 dist，若 shim 在某个插件上出问题，可把窗口 URL 切成 app server origin（同源、零 shim）作为逃生门，不影响 Host/代理/桥。

## 7. 落地对照

| 能力 | 实现位置 |
|---|---|
| 三屏（加载 / 错误 / 设置） | `src-ui/` 零构建 HTML/CSS，由 `dsh-app://shell/<page>`（scheme）与回环 `/_shell/*` 两处供给 |
| 壳自己的标题栏 | `src-ui/header.html` + `header` webview（仅 Windows/Linux；ADR-005 + ADR-006） |
| 桌面三层启动编排 | `src-tauri/src/app_runtime.rs`（定位 → 版本闸门 → 环境探针 → Host → 认证 → app server → 导航） |
| Desktop Host 子进程 | `src-tauri/host/desktop-host.mjs` + `dsh-lite-core::desktop_host`（stdout 行协议、结构化 FATAL、优雅停止） |
| `dsh-app://` 文档 scheme | `src-tauri/src/scheme.rs`（index 注入 boot、静态、模块脚本缓冲代理、shell 页） |
| 回环 app server（流式 HTTP + WS + CORS） | `dsh-lite-core::app_server`（`authenticate_host` / `proxy` / `websocket` / `document`） |
| preload 等价桥 | `src-tauri/src/bridge.rs` + `src-tauri/bridge/preload.js`（`window.dshDesktop` 全套、`dshDesktopBoot`、fetch/XHR 重写） |
| 前端 dist 解析 | `dsh-lite-core::frontend_dist`（读全局安装内的 `dsh-web-frontend/dist`，不随包携带） |
| dsh 最低版本闸门 | `dsh-lite-core::version_gate`（semver §11，`< 0.2.1-rc.2` 拒绝） |
| 壳的菜单 | 原生 popup：`main.rs::shell_menu`，动作在 `run_menu_entry` |
| dsh 定位 / 版本探测 | `dsh-lite-core::dsh_locate` |
| 附着到已在运行的 `dsh web`（`--url` / `--port`） | `dsh-lite-core::launcher` + `connection.rs`（保留的旧单层路径） |
| 认证（终态 URL 必须脱 token） | 三层路径：`app_server::authenticate_host`；旧路径：`window::classify_page_load` |
| 认证看门狗（首次加载失败自动重试一次） | `connection::arm_auth_watchdog`（仅旧路径） |
| 停止子进程、不留孤儿 | `dsh-lite-core::job`（Windows Job Object / Unix 进程组） |
| 检查 dsh 更新（`npm view`，只报告不安装） | `dsh-lite-core::update` + `main.rs::check_update` |
| 托盘 / 应用菜单 | `src-tauri/src/{tray,menu,menubar}.rs` |
| 单实例 | `tauri-plugin-single-instance` |
| 设置写回 `config.toml` | `main.rs::save_settings` |
