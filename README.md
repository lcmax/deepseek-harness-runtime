# DeepSeek Harness Runtime (Rust)

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![Platforms](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue.svg)](#supported-platforms)

[English](./README.md) | [中文](./README.zh-CN.md)

A high-performance desktop runtime for [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness), built with **Rust + tao + wry**. It automates Node.js runtime management, repository synchronization, dependency installation, and host process launching — delivering a native desktop experience with cross-platform support.

## ✨ Features

- 🚀 **One-click Launch**: Automatically manages Node.js, syncs the latest Harness repo, installs dependencies, builds the app, and launches the host process.
- 🖥️ **Cross-platform**: Windows, macOS, and Linux support with native webview rendering.
- 🌐 **Auto Localization**: Automatically detects system language and switches UI between English and Chinese.
- 📦 **Self-contained Runtime**: All runtime data (Node.js, repo, config) is stored alongside the executable — portable and clean.
- ⚡ **Live Reload**: File watcher automatically rebuilds and reloads the host on source changes.
- 🔒 **Containerized Profile**: DSH home directory is isolated in `.runtime/dsh-home` — never touches your system `~/.dsh`.

## 🏗️ Architecture

```
┌─────────────────────────────────────────────────────┐
│                  DeepSeek Harness                    │
│              Desktop Runtime (Rust)                  │
│                                                      │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────┐  │
│  │  Node.js  │  │   Repo   │  │  Host Process     │  │
│  │  Manager  │  │ Manager  │  │  (dsh web)        │  │
│  └──────────┘  └──────────┘  └──────────────────┘  │
│         │              │                │            │
│         ▼              ▼                ▼            │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────┐  │
│  │  Install  │  │  Build   │  │  WebView Window  │  │
│  │  (pnpm)  │  │  (pnpm)  │  │  (tao + wry)     │  │
│  └──────────┘  └──────────┘  └──────────────────┘  │
│                                                      │
│  .runtime/ (exe-level)                              │
│  ├── node/          Node.js runtime                  │
│  ├── repo/          DeepSeek Harness source         │
│  ├── dsh-home/      DSH isolated profile            │
│  └── state.json     Bootstrap state cache           │
└─────────────────────────────────────────────────────┘
```

## 🚀 Quick Start

### Prerequisites

- **Rust** 1.85+ ([rustup](https://rustup.rs/))
- **WebView2 Runtime** (Windows) — [Download](https://developer.microsoft.com/microsoft-edge/webview2/)
- Node.js is automatically managed by the runtime

### Build & Run

```bash
# Clone
git clone https://github.com/lcmax/deepseek-harness-runtime.git
cd deepseek-harness-runtime

# Build (debug)
cargo build

# Build (release)
cargo build --release

# Run
cargo run
```

The runtime will:
1. Download Node.js v24.19.0
2. Clone the latest DeepSeek Harness repository
3. Install dependencies with pnpm
4. Build the web application
5. Launch the host process and open the desktop window

### Configuration

Create `config.toml` in the same directory as the executable:

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
port = 0          # 0 = auto-assign
read_timeout = 60 # seconds
```

Or copy from `config.example.toml`.

## 📦 Distribution

### Windows

```bash
cargo build --release
# Required DLL: WebView2Loader.dll (auto-copied by build script)
# Bundle both exe and DLL for distribution
```

### macOS / Linux

```bash
cargo build --release
# Single binary, no extra dependencies
```

<img width="1182" height="832" alt="ScreenShot_2026-08-15_112445_136" src="https://github.com/user-attachments/assets/63c6e5f7-4c29-4789-be65-63df26252802" />

<img width="1182" height="832" alt="ScreenShot_2026-08-15_112457_321" src="https://github.com/user-attachments/assets/73af2ac0-0c0a-4797-9f03-4c95a0052f4c" />


## 🛠️ Technical Stack

| Component | Technology | Purpose |
|-----------|-----------|---------|
| Window | [tao](https://crates.io/crates/tao) | Cross-platform windowing |
| WebView | [wry](https://crates.io/crates/wry) | Webview rendering |
| Config | [serde](https://crates.io/crates/serde) + TOML | Configuration management |
| Locale | [sys-locale](https://crates.io/crates/sys-locale) | System language detection |
| HTTP | [reqwest](https://crates.io/crates/reqwest) | Download & sync |
| Watcher | [notify](https://crates.io/crates/notify) | File change detection |

## 📄 License

This project is licensed under the MIT License. See [LICENSE](./LICENSE) for details.

## 🤝 Contributing

Issues and pull requests are welcome! Please feel free to contribute.
