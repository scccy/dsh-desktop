# dsh desktop

面向技术用户的轻量原生桌面外壳：**不自带任何运行时**，只负责启动并承载本机已有的 `dsh web`，并把宿主机真实的系统环境交给它。

- 产品名：`dsh desktop` · 可执行文件 `dsh-desktop` · 包标识 `dev.dsh.desktop`
- 语言 / 框架：Rust 核心 + 两条壳——Windows 用 **WinUI 3**（`src-winui/`），三平台通用壳用 **Tauri v2**（`src-tauri/`）；均为系统 WebView，无 Chromium、无 Node 构建链
- 平台：Windows · Linux · macOS，单一 Rust 核心（`crates/dsh-lite-core`）
- 体积：Tauri 壳目标 < 15 MB；WinUI 壳实测 74.5 MB（产品已明确接受 ~100 MB 以内）

## 状态（如实）

**M0 骨架完成，M1 主链路代码已落地；Windows 原生壳（WinUI 3）已在本机端到端跑通；尚未发布，也还没有可安装包。**

| 部分 | 状态 |
|---|---|
| Cargo workspace（`crates/dsh-lite-core` + `crates/dsh-ffi` + `src-tauri`） | 已建立 |
| `dsh-lite-core`：配置、CLI、定位、环境探针、启动、URL 策略、日志 | 已实现并由 `cargo test` 覆盖（91 项） |
| `crates/dsh-ffi`：C ABI（`cdylib` + `staticlib`），供非 Tauri 壳复用核心 | 已实现并由 `cargo test` 覆盖（10 项） |
| `src-tauri` 窗口壳（单实例、窗口、连接/生命周期、托盘、macOS 菜单、快捷键） | 已实现，本地 `cargo build -p dsh-desktop` 通过；运行时行为待真机验收 |
| `src-tauri/src/platform/` 平台差异层 | 已建立（macOS / Linux / Windows 各一份，见 [docs/architecture.md](docs/architecture.md#11-平台差异层)） |
| `src-ui/` 加载页 / 错误页 / 设置页 | 已落地（零构建 HTML/CSS/JS） |
| `src-winui/` Windows 原生壳（WinUI 3 + WebView2，三屏 + 自绘标题栏 + 检查更新 + 单实例 + Job Object） | 已跑通：定位 dsh → 启动 → 加载 Web GUI、错误屏、设置屏；单实例与「壳死子进程跟着死」均实测；托盘/设置写回**未接**（见 [src-winui/README.md](src-winui/README.md)） |
| Windows 安装包（Inno Setup，`scripts/build-installer.ps1`） | 已产出 `dist/dsh-desktop-winui-setup-0.1.0.exe`（20.6 MB），安装→启动→卸载实测通过；**未签名** |
| 占位图标（`assets/`、`src-tauri/icons/`） | 已就位（正式设计稿是发布阻塞项） |
| 三平台 CI（fmt / clippy / test / release 构建） | 已就位（尚未包含 WinUI 壳的构建） |
| 发布流水线（tag 触发、三平台打包、SHA-256、可选签名） | 已就位（签名未接线，见 [docs/development.md](docs/development.md)） |

窗口装饰使用系统原生标题栏，不做官方 Electron 那样自绘标题栏；"单一 Tauri 代码线"的原始理由见 [docs/architecture.md §6 ADR-002](docs/architecture.md#adr-002-单一-tauri-代码线不做-macos--windows-双原生壳)，Windows 侧改为原生壳的决策与实测代价见 [ADR-003](docs/architecture.md#adr-003-windows-采用-winui-3-原生壳--rust-c-abi-桥接)。

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
src-tauri/                # Tauri 应用：平台粘合层
  src/main.rs             # 组装：CLI → 配置 → 连接 → 窗口
  src/connection.rs       # 连接解析与托管/附着两条路径
  src/lifecycle.rs        # 启动收尾、退出清理、窗口显示
  src/window.rs           # 窗口状态、外链拦截、页面切换
  src/tray.rs             # Windows / Linux 托盘
  src/menu.rs             # macOS 应用 / Window / Edit 菜单与 Dock 菜单
  src/shortcuts.rs        # 壳快捷键
  src/state.rs            # 运行时共享状态
  src/platform/           # 唯一的平台差异层：mod / macos / unix / windows
  icons/                  # 打包器需要的 ICO / PNG
  tauri.conf.json         # 应用基础配置
  tauri.release.conf.json # 发布期 bundle 覆盖项（tag 流水线使用）
src-ui/                   # 零构建本地 HTML/CSS/JS：加载页、错误页、设置页
assets/                   # icon.svg 与三平台 1024×1024 占位 PNG
scripts/                  # check.ps1 / check.sh / render-icons.ps1
.github/workflows/        # ci.yml（三平台校验）/ release.yml（tag 发布）
docs/                     # 架构与开发文档
```

## 文档

- [项目要求.md](项目要求.md) —— 目标与非目标、功能需求（FR-1..FR-11）、技术方案、平台差异矩阵、验收标准、里程碑
- [docs/architecture.md](docs/architecture.md) —— 分层与平台差异层、启动时序、错误分类、日志位置、关键决策（ADR-001 原生窗口装饰 / ADR-002 单一 Tauri 代码线）
- [docs/development.md](docs/development.md) —— 环境准备、常用命令、调试、已知限制（GNU vs MSVC、Linux WebKitGTK 版本）
- [CONTRIBUTING.md](CONTRIBUTING.md) —— 提交信息规范与必过检查

## 许可

MIT
