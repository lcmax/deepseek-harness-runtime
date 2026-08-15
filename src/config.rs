//! # config
//!
//! 负责读取并解析 `config.toml`，将嵌套 TOML 结构展平为运行时 [`Config`]。
//!
//! ## 设计决策
//! - 文件不存在或字段缺失时使用默认值（spec 要求无配置文件可正常启动）。
//! - 解析失败（非法 TOML / 类型错误）直接返回错误终止启动，避免带病运行。
//! - `[app] [node] [workspace] [build]` 四段各自独立 Option，逐段覆盖默认值。

use std::path::{Path, PathBuf};

/// 运行时配置（由嵌套 TOML 展平后的最终形态）
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// 拉取的 GitHub 仓库地址（默认 deepseek-harness）
    pub repo_url: String,
    /// 拉取的分支，默认 `master`（deepseek-harness 仓库默认分支）
    pub branch: String,
    /// Node.js 版本（运行时自动下载托管，不依赖系统 PATH），默认 `24.19.0`
    pub node_version: String,
    /// 运行时工作目录（node/、repo/、state.json 都放在这里），默认 `.runtime`；
    /// 相对路径解析为与执行文件同级（基于 `exe_base_dir()` 拼接）
    pub workspace_root: String,
    /// `npm run <script>` 构建脚本名，默认 `build`
    pub build_script: String,
    /// 附加到 `npm install` 的额外参数（如 `--registry=...`）
    pub install_args: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repo_url: "https://github.com/deepseek-ai/deepseek-harness.git".to_string(),
            branch: "master".to_string(),
            node_version: "24.19.0".to_string(),
            workspace_root: ".runtime".to_string(),
            build_script: "build".to_string(),
            install_args: Vec::new(),
        }
    }
}

/// TOML 原始嵌套结构（[app] [node] [workspace] [build]）
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawConfig {
    app: RawApp,
    node: RawNode,
    workspace: RawWorkspace,
    build: RawBuild,
}

/// [app] 段：仓库来源
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawApp {
    repo_url: Option<String>,
    branch: Option<String>,
}

/// [node] 段：Node.js 托管版本
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawNode {
    version: Option<String>,
}

/// [workspace] 段：运行时工作目录
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawWorkspace {
    root: Option<String>,
}

/// [build] 段：构建脚本与安装参数
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawBuild {
    script: Option<String>,
    install_args: Option<Vec<String>>,
}

/// 返回执行文件所在目录，供 workspace_root 相对路径拼接使用。
///
/// 解析顺序：优先取 `current_exe()` 的父目录；失败或父目录不可用时回退到
/// CWD；再失败则回退到 `.`。失败时优雅回退，避免启动崩溃。
///
/// # Panics
/// 不会 panic；所有失败路径均回退到可用目录。
pub fn exe_base_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.to_path_buf();
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        return cwd;
    }
    PathBuf::from(".")
}

impl Config {
    /// 加载配置。
    ///
    /// # Arguments
    /// * `path` - 配置文件路径；`None` 表示使用默认 `config.toml`。默认查找顺序
    ///   为「执行文件所在目录 → CWD」，两处均无则返回全默认值
    ///
    /// # Returns
    /// 展平后的 [`Config`]；文件不存在时返回默认值，解析出错返回 `Err`
    ///
    /// # Errors
    /// - 文件存在但读取失败（IO 错误）
    /// - 文件存在但为非法 TOML 或字段类型不匹配
    pub fn load(path: Option<&Path>) -> anyhow::Result<Config> {
        let path = match path {
            // 显式路径：优先使用；不存在则返回默认值（保持「无配置可启动」语义）
            Some(p) => {
                if p.exists() {
                    p.to_path_buf()
                } else {
                    return Ok(Config::default());
                }
            }
            None => {
                // 默认查找顺序：执行文件所在目录 → CWD；两处均无则走默认值分支
                let exe_cfg = exe_base_dir().join("config.toml");
                if exe_cfg.exists() {
                    exe_cfg
                } else if Path::new("config.toml").exists() {
                    Path::new("config.toml").to_path_buf()
                } else {
                    // 无配置文件：全部默认值（spec: 无配置文件启动场景）
                    return Ok(Config::default());
                }
            }
        };

        let raw_text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("读取配置文件 {} 失败: {e}", path.display()))?;
        let raw: RawConfig = toml::from_str(&raw_text)
            .map_err(|e| anyhow::anyhow!("解析配置文件 {} 失败: {e}", path.display()))?;

        Ok(raw.into_config())
    }
}

impl RawConfig {
    /// 将嵌套原始配置逐段覆盖到默认值上，展平为 [`Config`]。
    fn into_config(self) -> Config {
        let mut cfg = Config::default();
        if let Some(v) = self.app.repo_url {
            cfg.repo_url = v;
        }
        if let Some(v) = self.app.branch {
            cfg.branch = v;
        }
        if let Some(v) = self.node.version {
            cfg.node_version = v;
        }
        if let Some(v) = self.workspace.root {
            cfg.workspace_root = v;
        }
        if let Some(v) = self.build.script {
            cfg.build_script = v;
        }
        if let Some(v) = self.build.install_args {
            cfg.install_args = v;
        }
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 场景：无配置文件启动 → 全部默认值
    #[test]
    fn test_default_config_when_no_file() {
        // 用显式不存在的路径，避免受当前工作目录影响
        let missing = std::env::temp_dir().join("dsh_cfg_test_missing/config.toml");
        let cfg = Config::load(Some(&missing)).expect("无配置文件应返回默认值");
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.repo_url, "https://github.com/deepseek-ai/deepseek-harness.git");
        assert_eq!(cfg.branch, "master");
        assert_eq!(cfg.node_version, "24.19.0");
        assert_eq!(cfg.workspace_root, ".runtime");
        assert_eq!(cfg.build_script, "build");
        assert!(cfg.install_args.is_empty());
    }

    /// 场景：部分覆盖（node.version）→ 其余保持默认
    #[test]
    fn test_partial_override() {
        let dir = std::env::temp_dir().join("dsh_cfg_test_partial");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(
            &file,
            r#"
[node]
version = "22.11.0"
"#,
        )
        .unwrap();

        let cfg = Config::load(Some(&file)).unwrap();
        assert_eq!(cfg.node_version, "22.11.0");
        assert_eq!(cfg.branch, "master"); // 未覆盖字段保持默认
        std::fs::remove_file(&file).ok();
    }

    /// 场景：全量覆盖 → 每个字段均生效
    #[test]
    fn test_full_override() {
        let dir = std::env::temp_dir().join("dsh_cfg_test_full");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(
            &file,
            r#"
[app]
repo_url = "https://github.com/octocat/Hello-World.git"
branch = "master"

[node]
version = "22.11.0"

[workspace]
root = ".rt2"

[build]
script = "dist"
install_args = ["--registry=https://registry.npmmirror.com"]
"#,
        )
        .unwrap();

        let cfg = Config::load(Some(&file)).unwrap();
        assert_eq!(cfg.repo_url, "https://github.com/octocat/Hello-World.git");
        assert_eq!(cfg.branch, "master");
        assert_eq!(cfg.node_version, "22.11.0");
        assert_eq!(cfg.workspace_root, ".rt2");
        assert_eq!(cfg.build_script, "dist");
        assert_eq!(
            cfg.install_args,
            vec!["--registry=https://registry.npmmirror.com".to_string()]
        );
        std::fs::remove_file(&file).ok();
    }

    /// 场景：非法 TOML → 返回错误（而非静默使用默认值）
    #[test]
    fn test_invalid_toml_is_error() {
        let dir = std::env::temp_dir().join("dsh_cfg_test_invalid");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(&file, "not a = valid [[ toml").unwrap();

        assert!(Config::load(Some(&file)).is_err());
        std::fs::remove_file(&file).ok();
    }

    /// 场景：`exe_base_dir()` 返回非空路径
    #[test]
    fn test_exe_base_dir_not_empty() {
        let dir = super::exe_base_dir();
        assert!(!dir.as_os_str().is_empty(), "exe_base_dir() 不应返回空路径");
    }

    /// 场景：执行文件目录下的 config.toml 可被正常加载（显式路径）
    #[test]
    fn test_load_prefers_exe_dir_config() {
        // 在 exe_base_dir() 下临时创建 config.toml，验证 exe 目录配置可被加载
        let exe_dir = super::exe_base_dir();
        let exe_cfg = exe_dir.join("config.toml");
        std::fs::write(&exe_cfg, r#"[workspace]
root = "exe_prio"
"#)
        .unwrap();

        // 用显式路径验证 exe 目录下的配置能正常加载
        let cfg = Config::load(Some(&exe_cfg)).expect("exe 目录配置应可加载");
        assert_eq!(cfg.workspace_root, "exe_prio");

        // 清理临时文件
        std::fs::remove_file(&exe_cfg).ok();

        // 单独断言 None 分支至少不 panic（不依赖具体文件存在与否）
        let _ = Config::load(None).expect("None 分支不应 panic");
    }
}
