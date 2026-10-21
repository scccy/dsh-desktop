# dsh desktop

面向技术用户的轻量原生桌面外壳：**不自带任何运行时**，只负责启动并承载本机已有的 `dsh web`，并把宿主机真实的系统环境交给它。

- 产品名：`dsh desktop` · 可执行文件 `dsh-desktop` · 包标识 `dev.dsh.desktop`
- 语言 / 框架：Rust + Tauri v2（系统 WebView，无 Chromium、无 Node 构建链）
- 平台：Windows · Linux · macOS，单一代码库
- 目标体积：安装包 < 15 MB，冷启动 < 1.5s（不含 dsh 自身启动）

## 前置条件

宿主机需已安装 Node（`^22.19 || >=24`）与全局 `dsh`：

```sh
npm i -g @deepseek-ai/dsh
dsh --version
```

## 关键行为

```sh
# 壳自己启动 dsh 并承载其 Web GUI（默认，profile = web）
dsh-desktop
```

- 启动 `dsh --profile web --no-open --host 127.0.0.1 --port 0`，解析 stdout 的 `dsh web: <url>` 行取一次性 token URL；
- macOS/Linux 会先读取用户登录 shell 的环境（`<shell> -ilc 'env -0'`），Windows 直接继承注册表环境；
- 也支持附着到已在运行/由终端启动的实例：`dsh-desktop --url "<tokenized url>"`。

## 文档

- [项目要求.md](项目要求.md) —— 目标与非目标、功能需求（FR-1..FR-11）、技术方案、平台差异矩阵、验收标准、里程碑、风险与官方客户端能力映射

## 状态

需求已定稿（v0.2），尚未开始编码。下一步：M0 骨架（Cargo workspace + Tauri 应用 + CI）与 M1 主链路（定位 → 启动 → 解析 URL → 载入窗口 → 退出清理）。
