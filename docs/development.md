# 开发与发布

## 1. 环境准备

### 1.1 三平台通用

- Rust `stable`：由 [rust-toolchain.toml](../rust-toolchain.toml) 固定，含 `clippy` 与 `rustfmt`；
- edition 2024 与 Tauri v2 的 MSRV 对齐；
- 不需要 Node / pnpm 参与构建：壳内界面是零构建的 HTML/CSS/JS，`src-ui/` 不经过打包器。

### 1.2 Windows

| 项 | 要求 |
|---|---|
| 编译器 | **MSVC**（`x86_64-pc-windows-msvc`）+ Visual Studio Build Tools 的 C++ 桌面开发工作负载 |
| WebView2 | Windows 10/11 一般已内置；缺失时运行时会引导安装（见 `bundle.windows.webviewInstallMode`） |
| 本地 MinGW | 可选，仅用于快速跑单测，见 [6.1](#61-gnu--mingw-vs-msvc) |

### 1.3 macOS

- Xcode Command Line Tools（`xcode-select --install`）；
- macOS 11.0 及以上（发布配置里的 `bundle.macOS.minimumSystemVersion`）；
- 打包 `.dmg` 需要 Xcode 附带的 `hdiutil`，命令行工具即可。

### 1.4 Linux

```sh
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
  libayatana-appindicator3-dev patchelf
```

CI 的 `ci.yml` 在 Linux 上先跑同一组安装，再执行 `cargo` 步骤，顺序不可交换（build script 依赖 `pkg-config` 找到的 `.pc` 文件）。

## 2. 常用命令

```sh
cargo build                      # 调试构建（workspace）
cargo build -p dsh-lite-core     # 只构建纯逻辑 crate
cargo test --workspace           # 单测
cargo test -p dsh-lite-core url  # 只跑名字含 url 的测试
cargo fmt --all                  # 格式化
cargo clippy --workspace --all-targets -- -D warnings

pwsh -File scripts/check.ps1     # Windows：fmt --check → clippy → test
./scripts/check.sh               # Linux / macOS：同上

pwsh -File scripts/render-icons.ps1   # 重新生成占位图标（assets/ + src-tauri/icons/）
cargo tauri dev                       # 需要 tauri-cli：cargo install tauri-cli --version "^2" --locked
cargo tauri build --config src-tauri/tauri.release.conf.json
```

`scripts/check.*` 与 CI 的步骤、顺序一致，是提交前的最小必过集合。

## 3. 调试

- `dsh-desktop --verbose`：详细日志，并把 `dsh` 子进程的 stdout/stderr 透传到控制台（默认只进日志文件）；
- `--no-env-probe`：跳过登录 shell 探针，用于排除"探针卡住/超时"类问题；
- `--reset-window`：忽略记忆的窗口位置，用于排除"窗口跑到已断开的显示器上"；
- `window.devtools = true`（配置）或 debug 构建下 `F12` / `Ctrl+Shift+I`（macOS `Cmd+Opt+I`）：打开 WebView DevTools；
- 日志目录见 [architecture.md §4](architecture.md#4-日志位置)，排查启动失败先看最新日志与 `crash-*.log`；
- Linux 上如果 WebView 白屏或渲染异常，先用 `WEBKIT_DISABLE_COMPOSITING_MODE=1` 排除显卡/合成器问题，再判断是否为内核版本差异。

调试约束：**不要把带 token 的 URL 贴进 issue、日志或提交信息**；需要复现时用 `--port` 附着模式，并在终端里自行粘贴完整 URL。

## 4. 发布流程

### 4.1 触发条件

| 工作流 | 触发 | 内容 |
|---|---|---|
| `.github/workflows/ci.yml` | push 到 `master` / `main`、PR、手动 `workflow_dispatch` | 三平台矩阵：`fmt --check` → `clippy -D warnings` → `test` → `build --release -p dsh-desktop` |
| `.github/workflows/release.yml` | 仅 `v*` tag push（另有 `workflow_dispatch` 的构建试跑，默认不发布 Release） | 三平台打包 → 每个产物一行 SHA-256 → 合并成一个 `SHA256SUMS.txt` → 建 draft Release |

发版：

```sh
git tag v0.1.0
git push origin v0.1.0
```

release 流水线用 `cargo install tauri-cli`（不引入 Node 工具链）执行 `cargo tauri build --config src-tauri/tauri.release.conf.json --bundles <平台对应值>`：

| 平台 | `--bundles` | 产物 |
|---|---|---|
| Linux | `deb,appimage` | `.deb`、`.AppImage` |
| macOS | `dmg` | `.dmg` |
| Windows | `nsis` | `-setup.exe` |

产物一并上传 `SHA256SUMS.txt`，校验：

```sh
sha256sum -c SHA256SUMS.txt      # Linux
shasum -a 256 -c SHA256SUMS.txt  # macOS
```

### 4.2 配置的分工

- `src-tauri/tauri.conf.json`：**应用基础配置**，由壳/界面负责人维护；发布流水线不修改它；
- `src-tauri/tauri.release.conf.json`：**只在打包时覆盖** bundle 相关项（bundle targets、`windows.webviewInstallMode`、`macOS.minimumSystemVersion`、deb/appimage 细节）。改打包行为请改这里，避免把发布期差异塞进基础配置。

### 4.3 签名与公证（未接线，占位）

签名与公证是**可选**步骤：没有对应 secrets 时流水线照常产出未签名包，不会失败。

| 平台 | 机制 | 需要的 secrets |
|---|---|---|
| macOS | Tauri bundler 读取 `APPLE_*` 环境变量后自动签名 + 公证 | `APPLE_CERTIFICATE`、`APPLE_CERTIFICATE_PASSWORD`、`APPLE_SIGNING_IDENTITY`、`APPLE_ID`、`APPLE_PASSWORD`、`APPLE_TEAM_ID` |
| Windows | Authenticode | `WINDOWS_CERTIFICATE`、`WINDOWS_CERTIFICATE_PASSWORD` |

`release.yml` 已把上述 secrets 注入构建环境，因此 macOS 一旦配好凭据即自动生效。**Windows Authenticode 尚未接线**：需要在 runner 上把 base64 的 PKCS#12 导入证书存储，并在打包配置里给出 `bundle.windows.certificateThumbprint`；当前流水线在该 secret 存在时只发告警，产出的仍是未签名安装包。

硬性规则：签名私钥、证书、密码只存在于仓库 secrets，**绝不提交到仓库**，也不写进日志或产物说明。

### 4.4 Windows 安装包（Tauri bundler，NSIS）

```sh
cargo tauri build --config src-tauri/tauri.release.conf.json --bundles nsis
# 产出：src-tauri\target\release\bundle\nsis\dsh-desktop_<version>_x64-setup.exe
```

- **发布产物只认 MSVC**（见 §6.1）：GNU 工具链能跑 `cargo build` 与测试，但打包、签名、WebView2 的运行时假设都以 MSVC 为准；
- 安装形态由 `tauri.release.conf.json` 决定：`nsis.installMode = currentUser`（默认装到用户目录、不弹 UAC）、语言 `English` + `SimpChinese`；
- WebView2 不随包携带：`webviewInstallMode = downloadBootstrapper`，系统缺 WebView2 时安装器联网引导；
- 图标来自 `src-tauri/icons/`（`icon.ico` 由 `scripts/render-icons.ps1` 从 `assets/icon.svg` 生成）；
- **未签名**：SmartScreen 会提示未知发布者；签名点位见 §4.3；
- 出包后手动启动一次，确认三屏与 `dsh web` 都能加载（流水线不内置冒烟检查）。

### 4.5 macOS dmg（现状与缺口）

三平台共用 `src-tauri` 一套代码（ADR-005），macOS 的 dmg 只是其中之一：

| 线 | 状态 |
|---|---|
| Windows / Linux / macOS | 同一份 `src-tauri` 代码，`release.yml` 在各平台分别跑 `cargo tauri build` |
| macOS dmg | `release.yml` 已在 `macos-latest` 上执行 `--bundles dmg`，但**从未真正触发过**（仓库还没打过 tag），所以 dmg 目前只停留在配置层面，没有任何产物被验证 |

mac 落地时的阻塞项：

- **签名与公证**：需要 `Developer ID Application` 证书 + `notarytool`；没有证书只能 ad-hoc 签名，用户要右键 → 打开，且 Gatekeeper 会拦下双击；
- 本机是 Windows，**构建不出也验证不了** macOS 产物，dmg 的一切结论只能在 mac 或 CI 上得出；
- 目标机器同样需要自备 `dsh` 与 Node（与 Windows 一致），壳不打包运行时；
- 三平台现在是同一套 UI（同一份 `src-ui/`），不存在某个平台观感不同的问题；剩下的差异只有系统 WebView 的渲染细节（WebKitGTK / WebView2 / WKWebView）。

## 5. CI 与本地环境的差异

- CI 的 Windows 用 runner 自带 MSVC；本地常见的是 MinGW（见 6.1）；
- CI 使用 `Swatinem/rust-cache@v2` 缓存 registry 与 target；本地缓存由 cargo 自身管理；
- CI 不跑打包（`release.yml` 才跑），所以"本地能 `cargo build`"不等于"能出安装包"；
- CI 不安装 Node，也不执行任何 npm 脚本。

## 6. 已知限制

### 6.1 GNU / MinGW vs MSVC

| | GNU（`x86_64-pc-windows-gnu`） | MSVC（`x86_64-pc-windows-msvc`） |
|---|---|---|
| 单测 / `cargo build` | 可用（mingw64 的 `bin` 进 `PATH`） | 可用（需 VS Build Tools） |
| Tauri 打包（NSIS / MSI） | 不作为发布依据 | 唯一支持的发布路径 |
| WebView2 链接方式 | 与发布产物不同，问题不可复现 | 与用户机器一致 |
| 结论 | 本地快速信号 | 一切"能不能装、能不能跑"的判断 |

切换：`rustup default stable-x86_64-pc-windows-msvc` / `rustup default stable-x86_64-pc-windows-gnu`。

### 6.2 Linux WebKitGTK 版本

- Tauri v2 依赖 **WebKitGTK 4.1**（`libwebkit2gtk-4.1-dev`）；Ubuntu 22.04 默认只有 4.0，构建会因找不到 `webkit2gtk-4.1` 失败，需要 24.04 或更高版本的发行版，或自行装 4.1 开发包；
- 推荐运行期 WebKitGTK ≥ 2.44；更低版本可能出现渲染差异（页面样式/交互异常），`.deb` 的 `depends` 只保证 4.1 存在，不保证小版本；
- AppImage 不捆绑 WebKitGTK，目标机器仍需系统提供 `webkit2gtk-4.1` 与 `libgtk-3`；
- 发行版差异（Debian / Fedora / Arch 的包名与版本）以发行版自身的 4.1 包为准。

### 6.3 其他

- 图标目前是占位素材（圆角深色底 + 字母 `d`）；正式设计稿是**发布阻塞项**，不是编码阻塞项；
- 壳不做自更新，`dsh` 的升级由用户自己的包管理器完成；
- macOS 公证需要 hardened runtime 与对应 entitlements，首版允许先出未签名包。

### 6.4 窗口装饰与观感

- 标题栏由壳自己画：`decorations: false`，顶上是 40 DIP 的 `header` webview，下面是 `content` webview。决策与被否掉的替代方案见 [architecture.md ADR-005](architecture.md#adr-005-回到单一-tauri-壳用双-webview-画标题栏)；
- 底色是取样自页面的纯色（浅 `#f9fafb` / 深 `#17181b`），与内容区无缝衔接；**没有 Mica**，因为 WebView2 里拿不到系统 Mica 材质，用同色近似。切深色主题或 `dsh` 改版时这两个值要复核；
- 拖动只绑在菜单右侧的空白条（`.drag`），400 ms 内的第二次按下判为双击 → 最大化；三个窗口按钮走 `shell_window_action`，关闭仍走 `CloseRequested`，所以 `window.closeAction` 的 hide / quit 语义不变；
- 无边框窗口的阴影由 Tauri 默认的 DWM 阴影提供，圆角交给系统；本项目不额外调用 DWM API；
- 布局在每次 `Resized` / `ScaleFactorChanged` 后由 `main.rs::layout_webviews` 重算——子 webview 不会自己跟随窗口，`auto_resize` 又是等比缩放，不适用于「header 固定 40 DIP + content 吃掉剩余」的布局。

平台差异代码的归属（哪些代码算窗口观感、哪些算 core 逻辑）见 [architecture.md §1.1](architecture.md#11-平台差异层)。

### 6.5 菜单只有「应用」和「视图」

- 两项都是壳的动作，没有「编辑」菜单：编辑能力由 WebView2 自带的 `Ctrl+Z/Y/X/C/V/A` 与 `Delete` 承担，壳不注册也不拦截；
- **应用**：打开设置 / 重试连接（`Ctrl+Shift+L`）/ 打开日志目录 / 检查 dsh 更新… / 退出（`Ctrl+Q`）；
- **视图**：刷新页面（`Ctrl+R`）/ 关于 dsh desktop；
- 「重试连接」重跑整条启动链（停掉 `dsh` 再起，`connection::restart`），「刷新页面」只让 `content` webview 重新加载当前 URL（`Webview::reload`）——两者不可混淆；
- 菜单是**原生 popup**（`Menu::popup_at`），不是 HTML 下拉：`header` webview 只有 40 DIP 高，HTML 下拉一出边界就被 webview 矩形裁掉。菜单项里的快捷键只是右侧的提示文字，真正处理按键的是 `shortcuts.rs`；
- macOS 不受影响：应用菜单的 Edit 子菜单走 AppKit responder chain，是真正的原生编辑。

## 7. 提交前的最小检查

```sh
pwsh -File scripts/check.ps1    # 或 ./scripts/check.sh
```

要求全绿。改动涉及打包配置时，还要在本地至少跑一次 `cargo tauri build --config src-tauri/tauri.release.conf.json --bundles <本地平台>`，确认 `tauri.release.conf.json` 能被解析并与基础配置合并成功。
