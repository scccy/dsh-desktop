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

### 4.4 Windows 安装包（`src-winui`，Inno Setup）

```powershell
pwsh -File scripts\build-installer.ps1        # Rust 桥 → dotnet publish → ISCC
# 产出：dist\dsh-desktop-winui-setup-<version>.exe
```

- **实测**（2026-10-21）：`dotnet publish` **156.6 MB / 314 文件**（自包含、不做裁剪）→ 安装包 **42.2 MB**（lzma2）；静默安装 → 启动 → 卸载全流程验证通过，卸载后目录被清空；
- **安装布局**：根目录只有四项 —— `app\`（应用本体，280 个文件）、`启动 dsh desktop.lnk`、`安装说明.txt`、`unins000.exe`/`.dat`。发布输出本身是平铺的一堆文件，永远不直接铺在根目录；
- **语言资源**：publish 默认带 87 个 culture 目录（WinAppSDK 原生 `.mui`），构建脚本在打包前只保留 `en-us` / `zh-CN` / `zh-TW`（+ `Microsoft.UI.Xaml`），其余目录直接删掉（314 → 280 个文件）。**不能用 Inno 的 `Excludes` 做这件事**：它只匹配文件名，而每个 culture 里的文件名完全相同（`Microsoft.ui.xaml.dll.mui`），按名字分不出 `zh-CN` 和 `af-ZA`；
- 安装形态：per-user（`PrivilegesRequired=lowest`，**不弹 UAC**），默认装到 `%LOCALAPPDATA%\Programs\dsh desktop`，安装时可选 all-users；卸载入口在「设置 → 应用」与开始菜单「Uninstall dsh desktop」，也可直接运行安装目录里的 `unins000.exe`；
- **快捷方式**：开始菜单（程序 + 卸载）、**桌面（默认勾选）**，外加安装目录里一个可见的 `启动 dsh desktop.lnk`（载荷在 `app\` 下，不该让人猜）。静默安装要桌面图标时必须带 `/MERGETASKS=desktopicon`，否则任务被跳过；
- **图标**：`<ApplicationIcon>` 指向 `src-tauri/icons/icon.ico`，exe 内嵌后任务栏 / Alt+Tab / 开始菜单 / 桌面快捷方式共用这一个图标（未打包应用没有 manifest 图标路径，只能靠 exe 资源）；
- 用户数据（`%LOCALAPPDATA%\dsh desktop`：WebView2 profile、滚动日志、`config.toml`）卸载时**故意保留**；
- 版本号唯一来源是 `Cargo.toml` 的 `[workspace.package] version`，脚本读取后以 `/DAppVersion` 传给 ISCC，不在 `.iss` 里写死；
- **未签名**：SmartScreen 会提示"未知发布者"。签名点位就在 ISCC 产物上（`signtool sign /fd sha256 /tr <时间戳> dist\dsh-desktop-winui-setup-*.exe`），证书到位后接进脚本即可，不影响今天的分发；
- **构建末尾自带冒烟检查**：脚本会启动刚发布的 `dsh-desktop-winui.exe`，等窗口出现、再正常关闭；进程若在窗口出现前退出（`0xC000027B` 之类），脚本打印新写入的 `winui-crash.log` 片段、**删掉刚生成的安装包**并以退出码 1 结束 —— 不能启动的包不许留在 `dist\`。已有壳实例在跑时会跳过（单实例锁会让新进程立刻退出，否则会误判），需要强制跳过用 `-SkipSmokeTest`；
- **不做裁剪**：`PublishTrimmed=false`。裁剪能把 156 MB 压到 71 MB，但会让 `.pri`、WebView2 的 WinRT 投影、`System.Threading.Thread` 这类"链接器看不见"的依赖在发布版里静默失效（Debug 正常、启动即 `0xC000027B`）。体积预算只要求"不要太大"，因此选择完整自包含输出 + 官方 API。完整记录见 [src-winui/README.md](../src-winui/README.md#为什么不做裁剪)。

#### 一条命令更新本机安装

```powershell
pwsh -File scripts\redeploy.ps1
# 关壳 -> 构建（含冒烟）-> 卸载 -> 安装 -> 校验 SHA-256 -> 启动
```

失败时脚本以非零退出码结束，并且**不会动已安装的旧版本**（卸载发生在构建成功之后）。
`-InstallDir` 换安装位置，`-SkipBuild` 只做安装，`-NoLaunch` 装完不启动。
脚本不会去杀不属于本项目的 `dsh` 进程，只把残留进程数量报出来。

### 4.5 macOS dmg（现状与缺口）

两条线必须分清，不要互相代替：

| 线 | 状态 |
|---|---|
| Tauri 壳（`src-tauri`，三平台） | `release.yml` 已在 `macos-latest` 上执行 `--bundles dmg`，但**从未真正触发过**（仓库还没打过 tag），所以 dmg 目前只停留在配置层面，没有任何产物被验证 |
| WinUI 原生壳（`src-winui`） | Windows 独有。mac 想要同样的原生形态，需要 SwiftUI + `dsh-ffi` 的 `staticlib`（ADR-003 预留的口子），并且**必须在一台 mac 上开发与验证** |

mac 落地时的阻塞项：

- **签名与公证**：需要 `Developer ID Application` 证书 + `notarytool`；没有证书只能 ad-hoc 签名，用户要右键 → 打开，且 Gatekeeper 会拦下双击；
- 本机是 Windows，**构建不出也验证不了** macOS 产物，dmg 的一切结论只能在 mac 或 CI 上得出；
- 目标机器同样需要自备 `dsh` 与 Node（与 Windows 一致），壳不打包运行时；
- Tauri 壳的 dmg 与 WinUI 安装包在**观感上不是同一套 UI**（三屏 HTML vs 原生 Fluent），文档里不得混为一谈。

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

- 不复刻官方 Windows 定制标题栏：三平台一律系统原生标题栏（`decorations: true`），观感与官方 Electron 客户端不同是有意选择，理由与被否掉的方案见 [architecture.md ADR-001](architecture.md#adr-001-窗口装饰使用系统原生标题栏)；
- Windows 观感增强（Mica / 亚克力）为可选项且**未排期**：Tauri 2.12 起提供 `WindowEffect`（`tauri::window::Effect`，实现见 `tauri` crate 的 `src/vibrancy/windows.rs`），仅 Windows 11 生效，低版本自动退回纯色窗口背景；接入前需在 Win11 真机确认所用 Tauri 版本的 API 与降级表现，未验证前不写进任何对外承诺。

平台差异代码的归属（哪些代码算窗口观感、哪些算 core 逻辑）见 [architecture.md §1.1](architecture.md#11-平台差异层)。

### 6.5 Windows / Linux 菜单栏不提供「编辑」菜单

- Windows / Linux 窗口标题栏下方只有「应用」菜单（打开设置 / 打开日志目录 / 检查 dsh 更新 / 退出）；**没有**「编辑」菜单；
- 原因：`muda 0.20` 的 Windows 后端为 `PredefinedMenuItem::{undo,redo,cut,copy,paste,select_all}` 只注册 accelerator 标签，不向焦点控件投递 `WM_COPY` / `WM_PASTE` 等编辑命令。在真实 WebView2 窗口里的实测结果：把 `PASTED-TEXT` 放进剪贴板、把焦点放在设置页的 profile 输入框（该框有焦点环）后点「编辑 → 粘贴」，输入框内容不变；而「全选 + 复制」确实生效（剪贴板被替换成整页选中文本）。半可用的菜单比没有菜单更容易误导用户，因此整体移除；
- 编辑能力由 WebView2 自带的 `Ctrl+Z` / `Ctrl+Y` / `Ctrl+X` / `Ctrl+C` / `Ctrl+V` / `Ctrl+A` 与 `Delete` 承担，壳不注册、不拦截这些按键（壳只在 debug 构建或 `window.devtools = true` 时注入一个只上报不自处理的按键监听，用于 F12 等壳级快捷键）；
- macOS 不受影响：应用菜单的 Edit 子菜单走 AppKit responder chain，是真正的原生编辑。

## 7. 提交前的最小检查

```sh
pwsh -File scripts/check.ps1    # 或 ./scripts/check.sh
```

要求全绿。改动涉及打包配置时，还要在本地至少跑一次 `cargo tauri build --config src-tauri/tauri.release.conf.json --bundles <本地平台>`，确认 `tauri.release.conf.json` 能被解析并与基础配置合并成功。
