# dsh desktop

面向技术用户的轻量原生桌面外壳：**不自带任何运行时**，只负责启动并承载本机已有的 `dsh web`，并把宿主机真实的系统环境交给它。

- 产品名：`dsh desktop` · 可执行文件 `dsh-desktop` · 包标识 `dev.dsh.desktop`
- 语言 / 框架：Rust + **Tauri v2**，系统 WebView（WebView2 / WKWebView / WebKitGTK），无 Chromium、无 Node 构建链
- 平台：Windows · Linux · macOS，**同一份代码**（`src-tauri` + `crates/dsh-lite-core`）
- 体积：二进制目标 < 10 MB，安装包目标 < 15 MB

## 状态（如实）

**单一 Tauri 壳，三平台一套代码；Windows 本机已端到端跑通（自绘标题栏、原生菜单、单实例、Job Object、认证）。尚未发布，也还没有实测过的安装包。**

| 部分 | 状态 |
|---|---|
| Cargo workspace（`crates/dsh-lite-core` + `src-tauri`） | 已建立 |
| `dsh-lite-core`：配置、CLI、定位、环境探针、启动、URL 策略、日志、更新判定、进程回收 | 已实现并由 `cargo test` 覆盖 |
| `src-tauri` 窗口壳：单窗口双 webview、单实例、连接/生命周期、托盘、菜单、快捷键 | 已实现，Windows 本机跑通 |
| `src-tauri/src/platform/` 平台差异层 | 已建立（macOS / Linux / Windows 各一份，见 [docs/architecture.md](docs/architecture.md#11-平台差异层)） |
| `src-ui/` 标题栏 + 加载页 / 错误页 / 设置页 | 已落地（零构建 HTML/CSS/JS） |
| Windows 安装包（Tauri bundler / NSIS） | 配置就位，未实测 |
| 图标（`assets/`、`src-tauri/icons/`、`src-ui/icon.png`） | 已换成正式设计稿（`assets/icon.png` 为 1024 透明圆角源，`scripts/render-icons.ps1` 派生各平台尺寸） |
| 三平台 CI（fmt / clippy / test / release 构建） | 已就位 |
| 发布流水线（tag 触发、三平台打包、SHA-256、可选签名） | 已就位（签名未接线，见 [docs/development.md](docs/development.md)） |
| macOS / Linux 真机验收 | 未做 |

窗口装饰由壳自己画：`decorations: false` + 一个 40 DIP 的 `header` webview 画标题栏，下面是 `content` webview 装 `dsh web` 或三屏。为什么不能在一个 webview 里做、以及被否掉的替代方案见 [ADR-005](docs/architecture.md#adr-005-回到单一-tauri-壳用双-webview-画标题栏)。

## 前置条件

运行时（不是构建期）需要宿主机已安装 Node（`^22.19 || >=24`）与全局 `dsh`：

```sh
npm i -g @deepseek-ai/dsh
dsh --version
```

壳不打包、不改写、不升级这套环境；`dsh` 版本过旧时壳会在错误页给出升级命令。

## 关键行为

```sh
# 壳自己启动 dsh 并承载其 Web GUI（默认，profile = web）
dsh-desktop

# 附着到已在运行 / 由终端启动的实例
dsh-desktop --url "<dsh web: 行里给出的完整 URL>"
```

- 启动 `dsh --profile web --no-open --host 127.0.0.1 --port 0`，解析 stdout 的 `dsh web: <url>` 行，用其中一次性 token URL 完成首跳；
- macOS / Linux 先读取用户登录 shell 的环境（`<shell> -ilc 'env -0'`），Windows 直接继承注册表用户变量与系统变量；
- 完整参数见 `dsh-desktop --help`，需求见 [项目要求.md](项目要求.md)。

## 开发

### 环境准备

| 依赖 | 说明 |
|---|---|
| Rust `stable`（edition 2024） | 版本由 [rust-toolchain.toml](rust-toolchain.toml) 固定，并声明 `clippy`、`rustfmt` |
| Tauri v2 系统依赖 | Linux：`libwebkit2gtk-4.1-dev`、`libgtk-3-dev`、`librsvg2-dev`、`libayatana-appindicator3-dev`、`patchelf`；macOS：Xcode Command Line Tools；Windows：MSVC（见下） |

### 编译与校验

```sh
cargo build                    # 调试构建（workspace 全部成员）
cargo test --workspace         # 单元测试
pwsh -File scripts/check.ps1   # fmt --check → clippy -D warnings → test
./scripts/check.sh             # 同上，Linux / macOS
```

脚本与 CI 的步骤、顺序完全一致；CI 另外多做一次 `cargo build --release -p dsh-desktop`，打包只在 tag 流水线里发生。

### 本地 MinGW 说明

Windows 上有两条互不等价的工具链：

- **MSVC（CI 与发布用它）**：`x86_64-pc-windows-msvc`，用 runner / Visual Studio Build Tools 自带的 `link.exe`。Tauri 的 Windows 打包（NSIS / MSI）与 WebView2 链接按 MSVC 验证，**发布产物只认 MSVC**。
- **GNU / MinGW（本地快速迭代可用）**：`x86_64-pc-windows-gnu`，把 mingw64 的 `bin` 放进 `PATH` 即可 `cargo test`、`cargo build`。它适合跑单测和纯逻辑调试，但打包、签名与 WebView2 的运行时假设都以 MSVC 为准。

结论：本地用 GNU 跑 `scripts/check.ps1` 是有效的快速信号；凡是涉及打包、安装器、签名或"装上去能不能用"的判断，一律以 CI 的 MSVC 结果为准。切换方式：

```powershell
rustup default stable-x86_64-pc-windows-msvc   # 需要已安装 VS Build Tools
rustup default stable-x86_64-pc-windows-gnu    # 本地 MinGW
```

### 验证清单

改动合入前至少确认：

1. `scripts/check.ps1`（或 `scripts/check.sh`）全绿；
2. 新增或修改的逻辑在 `crates/dsh-lite-core` 里有单测，且测试描述行为而不是实现；
3. 涉及 CLI 参数的改动同步更新 `dsh-desktop --help` 文本；
4. 涉及打包配置的改动让 `src-tauri/tauri.release.conf.json` 与 [docs/development.md](docs/development.md) 保持一致；
5. 不把 token、密钥、`.env`、签名私钥写进仓库或日志（见 [CONTRIBUTING.md](CONTRIBUTING.md)）。

## 目录结构

```text
crates/dsh-lite-core/     # 纯 Rust 逻辑，无 GUI 依赖，可用 cargo test 覆盖
  src/config.rs           # 配置默认值、TOML 解析、校验、文件位置
  src/cli.rs              # 命令行参数解析与 --help 文本
  src/dsh_locate.rs       # dsh 可执行定位与 profile 预检
  src/env_probe.rs        # 登录 shell 环境读取（macOS / Linux）
  src/launcher.rs         # 子进程生命周期与 dsh web: 行解析
  src/url_policy.rs       # token 提取、脱敏、同源判断
  src/logging.rs          # 滚动日志与崩溃报告
  src/update.rs           # npm view 结果的判定（只报告，不安装）
  src/job.rs              # Job Object / 进程组：壳死子进程跟着死
  src/console_text.rs     # OEM / UTF-8 混合输出的容错解码
  src/text.rs             # 平台文案
src-tauri/                # Tauri 应用：平台粘合层
  src/main.rs             # 组装：CLI → 配置 → 连接 → 窗口（裸窗口 + 两个子 webview）
  src/connection.rs       # 连接解析、托管/附着两条路径、认证看门狗
  src/lifecycle.rs        # 启动收尾、退出清理、窗口显示
  src/window.rs           # 窗口状态、外链拦截、页面分类、webview 标签
  src/tray.rs             # Windows / Linux 托盘
  src/menu.rs             # macOS 应用 / Window / Edit 菜单与 Dock 菜单
  src/menubar.rs          # Windows / Linux 菜单栏
  src/shortcuts.rs        # 壳快捷键
  src/state.rs            # 运行时共享状态
  src/platform/           # 唯一的平台差异层：mod / macos / unix / windows
  icons/                  # 打包器需要的 ICO / PNG
  tauri.conf.json         # 应用基础配置
  tauri.release.conf.json # 发布期 bundle 覆盖项（tag 流水线使用）
src-ui/                   # 零构建本地 HTML/CSS/JS
  header.html/.css/.js    # 40 DIP 标题栏：品牌、应用/视图菜单、窗口按钮
  loading.html            # 加载页
  error.html              # 错误页
  settings.html           # 设置页
  shell.css               # 三屏共用样式
assets/                   # icon.png（1024 透明圆角设计源）与 icon-macos.png（内缩版）
scripts/                  # check.ps1 / check.sh / render-icons.ps1
.github/workflows/        # ci.yml（三平台校验）/ release.yml（tag 发布）
docs/                     # 架构与开发文档
```

## 文档

- [项目要求.md](项目要求.md) —— 目标与非目标、功能需求（FR-1..FR-11）、技术方案、平台差异矩阵、验收标准、里程碑
- [docs/architecture.md](docs/architecture.md) —— 分层与双 webview 窗口模型、平台差异层、启动时序、错误分类、日志位置、关键决策（ADR-001..005，**当前有效的是 ADR-005**：单一 Tauri 壳 + 双 webview 标题栏）
- [docs/development.md](docs/development.md) —— 环境准备、常用命令、调试、已知限制（GNU vs MSVC、Linux WebKitGTK 版本）
- [CONTRIBUTING.md](CONTRIBUTING.md) —— 提交信息规范与必过检查

## 许可

MIT
