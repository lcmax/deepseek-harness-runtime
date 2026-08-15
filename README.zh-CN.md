# DeepSeek Harness 运行时 (Rust)

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![Platforms](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue.svg)](#支持平台)

[English](./README.md) | [中文](./README.zh-CN.md)

基于 **Rust + tao + wry** 构建的 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 高性能桌面运行时。自动管理 Node.js、仓库同步、依赖安装、宿主进程启动，提供原生桌面体验，支持跨平台。

## ✨ 特性

- 🚀 **一键启动**：自动管理 Node.js、同步最新 Harness 仓库、安装依赖、构建应用、启动宿主进程。
- 🖥️ **跨平台**：Windows、macOS、Linux 全平台支持，原生 WebView 渲染。
- 🌐 **自动语言切换**：自动检测系统语言，在中英文界面间智能切换。
- 📦 **自包含运行时**：所有运行时数据（Node.js、仓库、配置）存储在执行文件同级，便携且干净。
- ⚡ **热更新**：文件监听器自动在源码变更时重建并刷新宿主。
- 🔒 **隔离配置**：DSH 主目录隔离在 `.runtime/dsh-home`，不影响系统 `~/.dsh`。

## 🏗️ 架构

```
┌─────────────────────────────────────────────────────┐
│              DeepSeek Harness 桌面运行时             │
│                  (Rust 实现)                         │
│                                                      │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────┐  │
│  │  Node.js  │  │   仓库   │  │  宿主进程         │  │
│  │  管理器   │  │  管理器   │  │  (dsh web)      │  │
│  └──────────┘  └──────────┘  └──────────────────┘  │
│         │              │                │            │
│         ▼              ▼                ▼            │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────┐  │
│  │  安装     │  │  构建     │  │  WebView 窗口    │  │
│  │  (pnpm)  │  │  (pnpm)  │  │  (tao + wry)    │  │
│  └──────────┘  └──────────┘  └──────────────────┘  │
│                                                      │
│  .runtime/ (与执行文件同级)                          │
│  ├── node/          Node.js 运行时                   │
│  ├── repo/          DeepSeek Harness 源码           │
│  ├── dsh-home/      DSH 隔离配置                     │
│  └── state.json     启动状态缓存                     │
└─────────────────────────────────────────────────────┘
```

## 🚀 快速开始

### 前置要求

- **Rust** 1.85+ ([rustup](https://rustup.rs/))
- **WebView2 Runtime** (Windows) — [下载](https://developer.microsoft.com/microsoft-edge/webview2/)
- Node.js 由运行时自动管理

### 编译运行

```bash
# 克隆仓库
git clone https://github.com/lcmax/deepseek-harness-runtime.git
cd deepseek-harness-runtime

# 调试模式编译
cargo build

# 发布模式编译
cargo build --release

# 运行
cargo run
```

运行时会自动完成以下步骤：
1. 下载 Node.js v24.19.0
2. 克隆最新 DeepSeek Harness 仓库
3. 使用 pnpm 安装依赖
4. 构建 Web 应用
5. 启动宿主进程并打开桌面窗口

### 配置文件

在执行文件同目录下创建 `config.toml`：

```toml
[workspace]
root = ".runtime"

[repo]
url = "https://github.com/deepseek-ai/deepseek-harness"
branch = "master"

[node]
version = "24.19.0"
mirror = "https://nodejs.org/dist"

[host]
port = 0          # 0 = 自动分配
read_timeout = 60 # 秒
```

或从 `config.example.toml` 复制。

## 📦 分发说明

### Windows

```bash
cargo build --release
# 必需 DLL: WebView2Loader.dll (构建脚本自动复制)
# 分发时需同时包含 exe 和 DLL
```

### macOS / Linux

```bash
cargo build --release
# 单文件二进制，无需额外依赖
```


<img width="1182" height="832" alt="ScreenShot_2026-08-15_112445_136" src="https://github.com/user-attachments/assets/63c6e5f7-4c29-4789-be65-63df26252802" />

<img width="1182" height="832" alt="ScreenShot_2026-08-15_112457_321" src="https://github.com/user-attachments/assets/73af2ac0-0c0a-4797-9f03-4c95a0052f4c" />


## 🛠️ 技术栈

| 组件 | 技术 | 用途 |
|------|------|------|
| 窗口 | [tao](https://crates.io/crates/tao) | 跨平台窗口管理 |
| WebView | [wry](https://crates.io/crates/wry) | Webview 渲染 |
| 配置 | [serde](https://crates.io/crates/serde) + TOML | 配置管理 |
| 语言 | [sys-locale](https://crates.io/crates/sys-locale) | 系统语言检测 |
| HTTP | [reqwest](https://crates.io/crates/reqwest) | 下载与同步 |
| 监听 | [notify](https://crates.io/crates/notify) | 文件变更检测 |

## 📄 许可证

本项目基于 MIT 许可证开源。详情见 [LICENSE](./LICENSE)。

## 🤝 贡献

欢迎提交 Issue 和 Pull Request！
