# 与官方 Desktop 对齐的开发目标

> 状态：**目标草案（待评审）**。本文只定义「要做什么、为什么、边界和验收」，不锁定实现细节；
> 具体实现决策落地后，回填到 [architecture.md](architecture.md) 的 ADR 与本文的阶段记录。
>
> 参考实现：官方源码 `apps/desktop`（路径 `/Volumes/project/github/dsh/deepseek-harness/apps/desktop`）。
> **官方是唯一参考基线**：除本文「差异与取舍」明确列出的项外，行为、分层、契约都按官方对齐。

## 0. 总体目标（一句话）

把官方 **Electron Desktop 壳移植成 Rust（Tauri v2）**：

- **去掉官方自带运行时**（Python/Node/dsh/pnpm），改为**使用系统已配置好的环境**（PATH / 登录 shell 环境变量）；
- **其余对齐官方**：Desktop Host + `dsh-app://` + preload 桥 + 原生能力 + macOS `hiddenInset` 两块头部；
- **实现语言换成 Rust**，非功能目标：**内存更小、安装包更小**（用系统 WebView 替代 Chromium 的直接收益）。

## 1. 结论先行

官方桌面端不是「壳 + 一个 web 页面」，而是**三层**：

1. **Electron Main**（`src/main.ts`）：窗口、`dsh-app://` 文档 scheme、HTTP/WS 代理与认证、preload、IPC、更新/托盘/深链等原生能力；
2. **Desktop Host**（`dsh-desktop-host`，Node 子进程）：用 `runProfile({ profile: "desktop" })` 起 **desktop profile** 的 web host，通过 Node IPC 回报 `{ url, injections }`；
3. **Renderer**（`dsh-app://app/`）：官方打包的 `dsh-web-frontend/dist` + preload 桥（`window.dshDesktop` 等），前端 boot 用 `dshDesktopBoot.ready()` 拿 `injections`/`streamBaseUrl` 后再启动。

我们现在的 `dsh-desktop` 是**两层**：Tauri 裸窗口 + content webview 直接加载 `dsh --profile web` 的 http URL；没有 `dsh-app://`、没有 Host 子进程、没有 preload 桥、profile 是 `web`、运行时依赖机器上全局安装的 `dsh`。

**本阶段目标**：把上面三层按官方参考实现补齐（**用 Rust/Tauri 移植**）；**运行时继续复用机器全局的 dsh/node，但按官方方式做好环境注入**（登录 shell 探针 + 启动注入），**不内置 Python/Node/dsh/pnpm**（避免包体过大与重复建设）；非功能目标是**更小内存、更小安装包**。

## 2. 目标（Goals）

### G1 架构对齐：Desktop Host + `dsh-app://` + preload 桥

- 引入 **Desktop Host** 子进程：启动 desktop profile 的 web host，回报 `url` 与 `injections`；
- 注册 **`dsh-app://`** 文档 scheme：
  - `dsh-app://shell/*`：壳自带文档（更新弹窗等）；
  - `dsh-app://app/` 的 `/`、`/assets/*`、favicon、manifest：读自带前端 `dsh-web-frontend/dist`；
  - `dsh-app://app/` 其余请求：代理到已认证 Host。
- 提供 **preload 等价桥**：在页面脚本前注入 `window.dshDesktop`（`protocolVersion: 1` + `keyboard`/`shortcuts`/`deviceInfo`/`browser`/`fileDrag`/`updates`）以及 `dshDesktopBoot`、`__DSH_DIRECTORY_PICKER__`、`__DSH_HOST_PATHS__`、`dshPlatform`、`dshOnboarding`、`__DSH_LOCALE__`；
- 在 preload 阶段打 `data-platform=<platform>`（macOS 为 `darwin`），让前端进入桌面布局。

### G2 运行时：复用机器全局 + 环境注入（不内置）

- **不随包携带** Python/Node.js/dsh/pnpm：官方内置是为了「无系统 Node 也能跑」，代价是包体大、与 dsh 重复建设；本项目**不采用**。
- 运行时**直接使用机器上已安装的全局 `dsh`（及其 `node`）**；启动时按官方方式做**环境注入**：
  - **登录 shell 探针**（`<shell> -ilc 'env -0'`）读取用户 shell 导出变量，合并进 Host / agent 子进程环境（官方 README「Host 环境」同款，本仓库已有实现）；
  - **启动注入**（`injections`）与 `streamBaseUrl` 走 preload/boot 契约（见 G1）。
- 桌面端**独占** `$DSH_HOME/profiles/desktop`（对齐官方归属）。
- 与官方的**有意差异**：不绑定内置 dsh 版本，跟随机器全局 `dsh`；用 `dsh --version` 做最低版本校验。

### G3 macOS 头部观感对齐

- 窗口用 `hiddenInset`：原生红绿灯叠在侧栏上，`trafficLightPosition`、`vibrancy: sidebar`、透明底；
- 头部读作「侧栏 + 内容」两块，不再有独立的 `DSH Desktop` 标题栏。

### G4 原生能力按优先级对齐

按官方能力清单逐项落地（顺序见 §7）：目录选择、麦克风、文件拖放、更新、深链 `dsh://open`、托盘、退出确认、崩溃报告。内嵌浏览器（Sidebar Browser）见 §6.4 的可行性结论。

### G5 可验证

- 每个阶段有可执行的验收项（启动、认证、首屏、两块头部、快捷键、一个原生能力）；
- 所有构建走 `./scripts/build-local-macos.sh --install`（签名 dmg → `/Applications`），不直接替换运行中应用的二进制。

### G6 非功能目标：更小内存 / 更小包

- 用系统 WebView（WKWebView / WebView2 / WebKitGTK）替代 Chromium；
- 不内置 Python/Node/dsh/pnpm，安装包只含「壳 + 前端 dist」；
- 验收基线（待测）：冷启动常驻内存、安装包体积，均需明显小于官方 Electron 版。

## 3. 非目标（Non-goals）

- **不重写** dsh 的 Web 前端与后端业务逻辑：前端复用官方 `dsh-web-frontend/dist`，后端复用 dsh profile runner；
- **不内置** Python/Node.js/dsh/pnpm 运行时（与官方不同：避免包体过大、重复建设；改为复用机器全局 + 环境注入）；
- **不追求** 第一版就覆盖全部原生能力（更新、内嵌浏览器等可分阶段）；
- **不在本阶段** 做 Windows 签名、公证、发布流水线（沿用现有发布配置，后续单独做）；
- **不改** dsh 自身的 profile/插件协议（只按官方方式消费）。

## 4. 现状 vs 目标

| 维度 | 现状（本仓库） | 目标（对齐官方） |
|---|---|---|
| 分层 | Tauri 壳 + content http 页面 | Main + Host + Renderer 三层 |
| 文档来源 | 直接加载 `dsh web` http URL | `dsh-app://` 静态 dist + 代理到 Host |
| 认证 | webview 自带 cookie jar（`shell_server` 同源） | Main 换 cookie 并注入 HTTP/WS（官方方式） |
| preload 桥 | 无（少量 `invoke`） | `window.dshDesktop` 全套 + boot/injections 契约 |
| Host | 无；`dsh --profile web` | `dsh-desktop-host` 子进程 + desktop profile |
| profile | `web` | `desktop`（桌面端独占） |
| 运行时 | 机器全局 `dsh`（登录 shell 探针） | **仍是机器全局 `dsh`**；补官方式环境注入；**不内置运行时**（有意差异） |
| macOS 窗口 | 原生标题栏（三块） | `hiddenInset` + vibrancy（两块） |

## 5. 目标架构

```
┌────────────────────────── Tauri App (Rust) ──────────────────────────┐
│ Main（Rust）                                                          │
│  · 窗口：macOS hiddenInset / Windows titleBarOverlay                  │
│  · 注册 dsh-app scheme：静态 dist + 代理 → Host                       │
│  · 认证：换 cookie，注入 HTTP/WS                                      │
│  · IPC：boot/browser/fileDrag/deviceInfo/locale/updates/window…      │
│  · 原生：托盘、单实例、深链、目录、麦克风、更新…                       │
└───────────────┬───────────────────────────────┬──────────────────────┘
                │ 启动 / IPC                     │ preload(init script)
                ▼                               ▼
   ┌──────────────────────────────┐   ┌──────────────────────────────────┐
   │ Desktop Host 子进程           │   │ Renderer: dsh-app://app/          │
   │ Node · 全局 dsh + 注入环境    │◄─►│  · 前端 dist（自带）              │
   │ runProfile(desktop)          │   │  · window.dshDesktop / boot 桥    │
   │ 回报 url + injections        │   │  · data-platform=darwin           │
   └──────────────────────────────┘   └──────────────────────────────────┘
```

启动顺序（对齐官方 `README` 的「运行时与插件激活」）：

1. 屏幕外加载共享 loading 页；
2. 校验**全局 dsh 版本**、准备 profile（不跑 pnpm、不改已装包）；
3. 起 Host 子进程，等 `ready { url, injections }`；
4. Main 用 `url` 换 cookie；
5. 主窗口加载 `dsh-app://app/`，前端 `dshDesktopBoot.ready()` 拿 `injections`/`streamBaseUrl`，应用注入后进入工作区。

## 6. 关键差异与技术风险（必须先解决/决策）

### 6.1 `dsh-app://` 在 WKWebView 上的等价度

- Electron 的 `protocol.handle` 能返回流式 `Response`、转发请求体、处理 CORS/cookie；Tauri 的 `register_uri_scheme_protocol` 在 macOS 走 `WKURLSchemeHandler`，返回的是完整字节，**流式与请求体转发能力弱**。
- 影响：SSE/大响应、上传类请求需要替代方案（直连 Host 或分块）。
- 结论：**静态 dist 可做；代理需按接口类型拆分验证**。

### 6.2 WebSocket 认证（对齐官方语义）

- 官方语义：**cookie 由主进程持有**，通过 `onBeforeSendHeaders` 给 `ws://127.0.0.1` 注入 `cookie`/`origin`，页面永远拿不到 cookie。
- WKWebView 没有 `onBeforeSendHeaders` 等价拦截 → 用 **Rust 侧 WS 代理**实现同样语义：页面连本地代理端点（地址由 `dshDesktopBoot.ready()` 返回的 `streamBaseUrl` 指定），Rust 在握手时加 `cookie`/`origin` 再转发到 Host，双向透传帧。
- 结论：**最终形态对齐官方（主进程持 cookie + 注入）**。webview cookie jar 只允许作为打通链路的内部临时手段，**不作为产品方案**。

### 6.3 Desktop profile 启动与配置注入

- `dsh --profile desktop` 被 CLI 硬拦（`profile "desktop" is managed exclusively by the Electron application`），官方靠内部 `runProfile` 绕过；
- `runProfile` 可从 `@deepseek-ai/dsh/profile-boot` 直接调用（已验证可达）；但直接起会缺 Host 注入的配置：
  - `desktop-product-telemetry` 需要 `serviceVersion`；
  - 缺 `@deepseek-ai/dsh-experimental-plugin-market-bundle` 时会被跳过。
- 结论：**Host 启动器需要复刻官方的配置/注入准备**（以官方 `dsh-desktop-host` 与 profile patch 为准），这是 P0 的主要工作。

### 6.4 内嵌浏览器（Sidebar Browser）

- 官方语义：主窗口启用 `<webview>` + `WebContentsView`，按账号/来源做持久分区，Browser 桥负责租约与导航。
- Tauri v2 可用**子 webview**（`add_child`）承载 guest，但需要实现 `<webview>` 兼容层（把页面里的 `<webview>` 元素位置/租约映射到子 webview）与分区/导航策略。
- 结论：**对齐官方行为，用 Tauri 子 webview + `<webview>` 兼容层实现**（工作量最大，放 P3 末）。

### 6.5 preload 只有异步

- Tauri 只能 `invoke`（异步）+ 事件；官方 preload 有同步 getter（如 `getAuthToken`）。
- 影响：`window.dshDesktop` 的成员需逐个确认是否可异步化；不可异步的用 init script 缓存值规避。

### 6.6 运行时来源（不内置）

- 官方把 Python/Node/pnpm/dsh 作为签名产物随包发布，并有 `runtime.json` 清单与完整性校验；
- 本项目**不内置运行时**：复用机器全局 `dsh`，只做环境注入。代价是「干净机器必须先装 dsh」，这与项目既有前置条件一致（见 README 前置条件）；
- 结论：**不做内置运行时**；把「环境注入 + `dsh --version` 最低版本校验」并入 P0。

### 6.7 Electron → Rust/Tauri 映射（移植策略）

按官方 `apps/desktop/src` 逐模块翻译到 `src-tauri/src`。Electron API 与 Tauri/Rust 的对应关系：

| Electron | Tauri / Rust |
|---|---|
| `BrowserWindow` | `tauri::Window` / `WebviewWindow` |
| `protocol.registerSchemesAsPrivileged` + `protocol.handle` | `register_uri_scheme_protocol`（自定义 scheme） |
| `webContents.loadURL` | `webview.navigate` |
| `preload` + `contextBridge` | `initialization_script` + `window.__TAURI_INTERNALS__.invoke` + 事件 |
| `ipcMain.handle` / `ipcRenderer.invoke` | `#[tauri::command]` + `invoke` |
| `session.webRequest.onBeforeSendHeaders` | 无直接等价 → Rust WS 代理注入 cookie（见 6.2） |
| `<webview>` / `WebContentsView` | 子 webview（`add_child`） |
| `hiddenInset` / `vibrancy` | `title_bar_style(Overlay)` + `hidden_title`（vibrancy 需验证 Tauri 支持） |
| `app.setAsDefaultProtocolClient("dsh")` | `tauri-plugin-deep-link` 或自注册 |
| tray / single-instance | Tauri 内置插件 |
| `electron-updater` | 自研 / tauri updater |
| `child_process.spawn`（Node Host） | `std::process::Command`（Rust 起 Node Host，stdout JSON 行协议） |
| Node IPC（`process.send`） | Rust 侧改为 stdout/stderr 行协议（或实现 Node IPC） |

## 7. 分阶段计划与验收

| 阶段 | 内容 | 验收 |
|---|---|---|
| **P0 Host 启动器** | Node 启动器调 `runProfile({profile:"desktop"})`，补配置注入；登录 shell 探针注入环境；`dsh --version` 校验；stdout 回报 `url`+`injections` | 终端能起 Host 并打印可用 url；`curl` 能拿到前端 index；GUI 启动能拿到 shell 导出变量 |
| **P1 `dsh-app://` + 代理** | 注册 scheme，静态 dist 从安装读取，HTTP 代理到 Host；WS 用 Rust 代理（主进程持 cookie 注入） | 主窗口从 `dsh-app://app/` 加载，登录、对话、流式回复可用 |
| **P2 preload 桥** | init script 实现 `dshDesktop`（先 keyboard/shortcuts/deviceInfo）+ `dshDesktopBoot` + `data-platform` | 客户端不再报 `Desktop keyboard bridge unavailable`；快捷键可用；两块头部出现 |
| **P3 原生能力** | 目录选择、文件拖放、麦克风、更新、深链 `dsh://open`、托盘、退出确认、内嵌浏览器（子 webview） | 每项一个手工验收用例 |
| **P4 收尾** | 独占 `profiles/desktop`、错误页/版本提示、文档与 ADR 回填 | 全局 dsh 版本不足/缺失时有明确错误页；架构文档更新 |

### 7.1 P0 进展（已验证）

`src-tauri/host/desktop-host.mjs` 已落地并实跑通过（用**系统全局 dsh**，无内置运行时）：

- 官方 `dsh --profile desktop` 被 CLI 硬拦，启动器改走内部 `runProfile({ profile: "desktop" })`（与官方 `apps/desktop-host` 同路）；
- **必须注入 `DSH_CLIENT_VERSION`**：`packages/bundle/web-app/cordis.patch.yml` 里 `serviceVersion: !!js process.env.DSH_CLIENT_VERSION`，缺失时 `desktop-product-telemetry` 校验失败、启动中止；
- 全局 dsh `0.2.0-rc.2` 的 `webServer.protocol` 为 `undefined`（官方内置的 `0.2.1-rc.3` 才有），启动器兜底 `http:`；
- `ready` 实际返回 `url` + **12 条 `injections`**；`stdin` 写 `shutdown` 可优雅退出（exit 0）；
- stdout 协议：`DSH_HOST_READY <json>` / `DSH_HOST_FATAL <json>`，诊断走 stderr；
- Rust 侧已接入 `dsh-lite-core::desktop_host`（spawn / 解析 / 优雅停止，进程组随 Rust 生命周期回收），并加了 `tests/desktop_host_e2e.rs`（默认 ignore）做真机端到端；
- 已知差异：全局 dsh 未安装 `@deepseek-ai/dsh-experimental-plugin-market-bundle`，启动时该 bundle 被跳过（官方是内置的）；不影响 Host 启动，后续在插件安装/打包阶段补齐。

## 8. 决策（全部对齐官方，不抄近路）

- **总体方向**：其余对齐官方；用 Rust/Tauri 移植，目标更小内存、更小包。
- **运行时**：复用机器全局 `dsh`，**不内置**运行时，只做环境注入（G2 / §6.6）。
- **版本**：不绑定内置 dsh 版本，跟随全局 dsh，只做最低版本校验。
- **WS 认证**：对齐官方语义——主进程持 cookie 并注入；Tauri 用 **Rust WS 代理**实现（§6.2）。
- **内嵌浏览器**：对齐官方——Tauri 子 webview + `<webview>` 兼容层（§6.4，P3）。
- 其余实现层细节，一律按官方源码对齐。

## 9. 已知差异（接受并记录）

- 内嵌浏览器用「子 webview + `<webview>` 兼容层」实现，能力与 Electron `<webview>` 可能有细节差异；
- **运行时**：官方内置 Python/Node/dsh/pnpm；我们复用机器全局 `dsh`（有意差异，避免包体与重复建设），因此「干净机器无 dsh」时需先安装（项目既有前置条件）；
- WS 认证在 Tauri 上用 Rust 代理实现（Electron 是 `onBeforeSendHeaders`），行为一致、机制不同；
- 非 macOS 平台的标题栏/外观按官方 Windows 方案对齐，但 Tauri 与 Electron 的控件细节会有差异；
- 更新通道/签名流程按我们自己的发布体系，不复刻官方 updater。

## 10. 参考（官方源码）

- `apps/desktop/README.zh.md`（行为与决策）
- `src/main.ts`（窗口、`protocol.handle`、代理、IPC）
- `src/web-document.ts`（`serveWebDocument` / `authenticateWebHost` / `forwardWebRequest`）
- `src/host-process.ts`（Host 子进程、Node IPC、`ready/injections`）
- `src/preload-app.ts`（`window.dshDesktop` 全套）
- `src/preload-platform.ts`（`markDocumentPlatform` / 全屏同步）
- `src/ipc.ts`（`DESKTOP_IPC` 契约）
- `dsh-desktop-host`（私有包，随官方 app.asar 分发，不在 npm）
- `@deepseek-ai/dsh/profile-boot`（`runProfile`，全局 dsh 可解析）
