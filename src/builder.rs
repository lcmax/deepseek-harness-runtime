//! # builder
//!
//! 自动构建执行器：以 repo 为工作目录、使用托管 Node 工具链执行依赖安装与构建。
//! 自动检测仓库包管理器（pnpm monorepo / 普通 npm），pnpm 仓库经 corepack 执行。
//!
//! ## 设计决策
//! - Windows 下 .cmd 入口（npm.cmd / corepack.cmd）经 `cmd /c` 调用。
//! - 子进程注入托管 bin 目录到 PATH：build scripts 内部的 `npm run` / `pnpm` /
//!   `node` 间接调用都依赖它。
//! - 注入 `npm_config_cache` / `COREPACK_HOME` 到 workspace，隔离系统全局 npm 配置
//!   （避免读写 `C:\Program Files\nodejs` 等无权限目录）。
//! - stdout/stderr 双管道 scoped threads 并发逐行读取，实时回调 `on_log`。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::node_manager::NodePaths;

/// 单条构建命令的产物目录约定名（vite/webpack 等主流脚手架默认）
const DIST_DIR_NAME: &str = "dist";

/// 检测到的仓库包管理器类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    /// 普通 npm 仓库（package-lock.json 或无锁文件）
    Npm,
    /// pnpm monorepo（pnpm-lock.yaml / pnpm-workspace.yaml / packageManager 字段）
    Pnpm,
}

/// 根据 repo 目录特征文件检测包管理器。
///
/// # Arguments
/// * `repo_dir` - 仓库源码目录
///
/// # Returns
/// 存在 pnpm 特征（pnpm-lock.yaml / pnpm-workspace.yaml / package.json 的
/// packageManager 含 "pnpm"）返回 [`PackageManager::Pnpm`]，否则 [`PackageManager::Npm`]
pub fn detect_package_manager(repo_dir: &Path) -> PackageManager {
    if repo_dir.join("pnpm-lock.yaml").exists() || repo_dir.join("pnpm-workspace.yaml").exists() {
        return PackageManager::Pnpm;
    }
    // package.json 的 packageManager 字段（如 "pnpm@11.7.0"）
    if let Ok(text) = std::fs::read_to_string(repo_dir.join("package.json")) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(pm) = value.get("packageManager").and_then(|v| v.as_str()) {
                if pm.starts_with("pnpm") {
                    return PackageManager::Pnpm;
                }
            }
        }
    }
    PackageManager::Npm
}

/// 以 repo 为工作目录执行单条命令并流式回传输出。
///
/// # Arguments
/// * `exe_path` - 工具入口路径（npm.cmd / corepack.cmd / npm 等，须为绝对路径）
/// * `args` - 命令参数
/// * `repo_dir` - 工作目录（仓库源码目录，须为绝对路径）
/// * `node_paths` - 托管 Node 工具链（用于 PATH 与缓存环境注入）
/// * `workspace_root` - 运行时工作目录（cache / corepack home 落在这）
/// * `on_log` - 日志回调（逐行；须 Send + Sync）
///
/// # Returns
/// 退出码为 0 返回 `Ok(())`
///
/// # Errors
/// - 命令启动失败
/// - 退出码非 0（错误信息附带最后几行输出）
#[allow(clippy::too_many_arguments)]
fn run_tool_command(
    exe_path: &Path,
    args: &[String],
    repo_dir: &Path,
    node_paths: &NodePaths,
    workspace_root: &Path,
    extra_env: &[(String, String)],
    on_log: &(dyn Fn(String) + Send + Sync),
) -> anyhow::Result<()> {
    on_log(format!("$ {} {}", exe_path.file_name().unwrap_or_default().to_string_lossy(), args.join(" ")));

    let mut command = build_command(exe_path, args);
    command.current_dir(repo_dir);
    inject_environment(&mut command, node_paths, workspace_root);
    for (key, value) in extra_env {
        command.env(key, value);
    }

    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("启动 {} 失败: {e}", exe_path.display()))?;

    // 双管道并发逐行读取：避免单管道写满 64KiB 缓冲导致子进程死锁。
    // 使用 scoped threads 以便借用非 'static 的 on_log 回调。
    let (out_lines, err_lines) = std::thread::scope(|scope| {
        let stdout_handle = scope.spawn({
            let reader = BufReader::new(child.stdout.take().expect("stdout 已管道化"));
            move || collect_stream(reader, on_log)
        });
        let stderr_handle = scope.spawn({
            let reader = BufReader::new(child.stderr.take().expect("stderr 已管道化"));
            move || collect_stream(reader, on_log)
        });
        (
            stdout_handle.join().unwrap_or_default(),
            stderr_handle.join().unwrap_or_default(),
        )
    });

    let status = child
        .wait()
        .map_err(|e| anyhow::anyhow!("等待命令退出失败: {e}"))?;

    if !status.success() {
        // 附带输出尾部若干行，方便定位失败原因
        let mut combined = out_lines;
        combined.extend(err_lines);
        let start = combined.len().saturating_sub(10);
        let tail = &combined[start..];
        return Err(anyhow::anyhow!(
            "{} 退出码 {:?}：\n{}",
            args.join(" "),
            status.code(),
            tail.join("\n")
        ));
    }
    Ok(())
}

/// 构建跨平台命令：Windows 用 `cmd /c <exe>`，unix 直接执行。
///
/// # Arguments
/// * `exe_path` - 工具入口路径（.cmd 批处理在 Windows 下必须经 cmd 解释）
/// * `args` - 传给工具的参数
fn build_command(exe_path: &Path, args: &[String]) -> std::process::Command {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/c").arg(exe_path);
        c
    } else {
        std::process::Command::new(exe_path)
    };
    cmd.args(args);
    cmd
}

/// 确保运行时隔离 npmrc 存在并返回其路径。
///
/// 该文件经 `NPM_CONFIG_USERCONFIG` 注入子进程后，npm/pnpm 将完全忽略
/// 用户全局 `~/.npmrc`——全局配置的 `script-shell=PowerShell` 会使含 `&&`
/// 的构建脚本解析失败（PS 5.1 不支持 `&&`）；屏蔽后 pnpm 回退默认
/// `cmd /d /s /c`（原生支持 `&&`），同时隔离 cache 指向等无权限路径。
///
/// # Arguments
/// * `workspace_root` - 运行时工作目录（npmrc 落点）
///
/// # Returns
/// 隔离 npmrc 路径；已存在时不覆盖（可手工追加 registry 等配置）
///
/// # Errors
/// 文件不存在且创建失败
fn ensure_isolated_npmrc(workspace_root: &Path) -> anyhow::Result<PathBuf> {
    let path = workspace_root.join("npmrc");
    if !path.exists() {
        std::fs::write(&path, "# rustc-deepseek-harness 运行时隔离配置：屏蔽系统全局 ~/.npmrc\n")
            .map_err(|e| anyhow::anyhow!("创建隔离 npmrc 失败: {e}"))?;
    }
    Ok(path)
}

/// 构造运行时隔离环境变量集合（纯函数，便于测试）。
///
/// - `PATH` 前置托管 bin 目录（scripts 内的 node/npm/pnpm 间接调用依赖）
/// - `NPM_CONFIG_USERCONFIG` → `<workspace>/npmrc`（用户级 npmrc 重定向，
///   屏蔽全局 `script-shell=PowerShell` 等配置，修复 `&&` 解析失败）
/// - `npm_config_cache` → `<workspace>/npm-cache`（依赖缓存落盘 workspace）
/// - `COREPACK_HOME` → `<workspace>/corepack`（corepack 下载的 pnpm 隔离存放）
///
/// # Arguments
/// * `node_paths` - 托管 Node 工具链
/// * `workspace_root` - 运行时工作目录
///
/// # Returns
/// 环境变量键值对列表
fn runtime_env_vars(node_paths: &NodePaths, workspace_root: &Path) -> Vec<(String, String)> {
    let separator = if cfg!(windows) { ";" } else { ":" };
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    vec![
        (
            "PATH".into(),
            format!(
                "{}{separator}{inherited_path}",
                node_paths.bin_dir.display()
            ),
        ),
        (
            "NPM_CONFIG_USERCONFIG".into(),
            workspace_root.join("npmrc").display().to_string(),
        ),
        (
            "npm_config_cache".into(),
            workspace_root.join("npm-cache").display().to_string(),
        ),
        (
            "COREPACK_HOME".into(),
            workspace_root.join("corepack").display().to_string(),
        ),
    ]
}

/// 向命令注入托管运行时隔离环境。
///
/// # Arguments
/// * `cmd` - 待注入的命令
/// * `node_paths` - 托管 Node 工具链
/// * `workspace_root` - 运行时工作目录
fn inject_environment(cmd: &mut std::process::Command, node_paths: &NodePaths, workspace_root: &Path) {
    for (key, value) in runtime_env_vars(node_paths, workspace_root) {
        cmd.env(key, value);
    }
}

/// 逐行读取流并回调；返回收集到的全部行（用于失败时截取尾部日志）。
fn collect_stream<R: std::io::Read>(
    reader: BufReader<R>,
    on_log: &(dyn Fn(String) + Send + Sync),
) -> Vec<String> {
    let mut lines = Vec::new();
    for line in reader.lines().map_while(|l| l.ok()) {
        on_log(line.clone());
        lines.push(line);
    }
    lines
}

/// 按包管理器生成依赖安装参数序列。
///
/// * 托管 pnpm（`pnpm_cmd` 提供）：`node <pnpm.cjs> install --config.store-dir=... ...`
/// * corepack 代理 pnpm：`corepack pnpm install --config.store-dir=... ...`
/// * npm：`npm install ...`
///
/// pnpm 额外注入 `--config.store-dir`：pnpm 默认 store 在盘根 `<盘>/.pnpm-store`，
/// 常因无写权限失败；CLI 参数优先级最高，稳定生效。
///
/// # Arguments
/// * `pm` - 包管理器类型
/// * `pnpm_cmd` - 托管 pnpm 入口（`pnpm.cjs`），仅 pnpm 类型且已托管时传 `Some`
/// * `install_args` - 用户配置的附加参数（registry 等，npm/pnpm 均兼容）
/// * `workspace_root` - 运行时工作目录（pnpm store 落点）
fn install_args_for(
    pm: PackageManager,
    pnpm_cmd: Option<&Path>,
    install_args: &[String],
    workspace_root: &Path,
) -> Vec<String> {
    let mut argv = prefix_argv_for(pm, pnpm_cmd);
    match pm {
        PackageManager::Pnpm => {
            argv.push("install".into());
            argv.push(format!(
                "--config.store-dir={}",
                workspace_root.join("pnpm-store").display()
            ));
        }
        PackageManager::Npm => argv.push("install".into()),
    }
    argv.extend_from_slice(install_args);
    argv
}

/// 按包管理器生成构建脚本参数序列。
///
/// # Arguments
/// * `pm` - 包管理器类型
/// * `pnpm_cmd` - 托管 pnpm 入口（`pnpm.cjs`），仅 pnpm 类型且已托管时传 `Some`
/// * `script` - `run <script>` 脚本名
fn build_args_for(pm: PackageManager, pnpm_cmd: Option<&Path>, script: &str) -> Vec<String> {
    let mut argv = prefix_argv_for(pm, pnpm_cmd);
    argv.push("run".into());
    argv.push(script.into());
    argv
}

/// 生成工具前缀参数：决定实际调用的是哪个可执行命令。
///
/// - pnpm + 托管：`[<pnpm.cjs 绝对路径>]`（经 `node <入口>` 启动）
/// - pnpm + 未托管：`["pnpm"]`（经 corepack 代理）
/// - npm：`[]`
///
/// # Arguments
/// * `pm` - 包管理器类型
/// * `pnpm_cmd` - 托管 pnpm 入口（`pnpm.cjs`）
fn prefix_argv_for(pm: PackageManager, pnpm_cmd: Option<&Path>) -> Vec<String> {
    match pm {
        PackageManager::Pnpm => match pnpm_cmd {
            Some(cmd) => vec![cmd.display().to_string()],
            None => vec!["pnpm".to_string()],
        },
        PackageManager::Npm => vec![],
    }
}

/// 定位构建产物目录（含 `index.html` 的静态站点根）。
///
/// 除构建后定位产物外，也用于"版本一致复用已有产物"判断：
/// 当同步走快速路径（`updated=false`）且本地已有产物时，直接复用该目录，
/// 从而跳过安装与构建。因此本函数需保持公有可见，供 `main.rs` 复用判断。
///
/// 搜索顺序（均排除 node_modules）：
/// 1. `<repo>/dist`（普通单包仓库约定）
/// 2. `<repo>/apps/<any>/dist`（monorepo 子包，如 deepseek-harness 的 apps/web）
///
/// # Arguments
/// * `repo_dir` - 仓库源码目录
///
/// # Returns
/// 含 `index.html` 的第一个产物目录（apps 下多个候选时按字典序取首个）
///
/// # Errors
/// 所有候选位置均无 `index.html` 时返回错误
pub fn locate_dist_dir(repo_dir: &Path) -> anyhow::Result<PathBuf> {
    // 候选 1：仓库根 dist
    let root_dist = repo_dir.join(DIST_DIR_NAME);
    if root_dist.join("index.html").is_file() {
        return Ok(root_dist);
    }

    // 候选 2：apps/<name>/dist（monorepo web 子包）
    let apps_dir = repo_dir.join("apps");
    if let Ok(entries) = std::fs::read_dir(&apps_dir) {
        let mut candidates: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path().join(DIST_DIR_NAME))
            .filter(|p| p.join("index.html").is_file())
            .collect();
        candidates.sort(); // 字典序稳定结果，便于复现
        if let Some(first) = candidates.first() {
            return Ok(first.clone());
        }
    }

    Err(anyhow::anyhow!(
        "构建命令执行成功但未找到含 index.html 的产物目录（已检查 {} 与 apps/*/{}）",
        root_dist.display(),
        DIST_DIR_NAME
    ))
}

/// 执行完整构建流程：依赖安装 → 构建脚本。
///
/// pnpm 仓库经托管 pnpm（配置指定版本）或 corepack 执行（按 packageManager 字段
/// 自动获取对应 pnpm 版本）；普通仓库直接用托管 npm。
///
/// # Arguments
/// * `node_paths` - 托管 Node 工具链（npm 入口 / corepack 入口 / bin 目录）
/// * `repo_dir` - 仓库源码目录（工作目录，须为绝对路径）
/// * `script` - `run <script>` 构建脚本名
/// * `install_args` - 附加到安装命令的参数
/// * `workspace_root` - 运行时工作目录（缓存隔离落点）
/// * `pnpm_cmd` - 托管 pnpm 入口（`bin/pnpm.cjs` 绝对路径）；`None` 表示用 corepack
/// * `on_phase` - 阶段切换回调（依次收到 `"install"`、`"build"`）
/// * `on_log` - 流式日志回调（含阶段标题行；须 Send + Sync）
/// * `on_progress` - 进度回调：百分比（`None` 表示不确定）与展示文案
///
/// # Returns
/// 构建产物目录 `repo/dist`（约定构建输出目录）
///
/// # Errors
/// - install 或 build 任一命令非 0 退出
/// - 构建成功但 dist/ 不存在
#[allow(clippy::too_many_arguments)]
pub fn run_build(
    node_paths: &NodePaths,
    repo_dir: &Path,
    script: &str,
    install_args: &[String],
    workspace_root: &Path,
    pnpm_cmd: Option<&Path>,
    lang: crate::i18n::Lang,
    commit_hash: Option<&str>,
    on_phase: &dyn Fn(&str),
    on_log: &(dyn Fn(String) + Send + Sync),
    on_progress: &(dyn Fn(Option<f64>, Option<String>) + Send + Sync),
) -> anyhow::Result<PathBuf> {
    // 容器隔离前提：确保 <workspace>/npmrc 存在并注入 NPM_CONFIG_USERCONFIG
    ensure_isolated_npmrc(workspace_root)?;

    let pm = detect_package_manager(repo_dir);
    // corepack 与 npm 同目录（Windows: corepack.cmd；unix: bin/corepack）
    let corepack = if cfg!(windows) {
        node_paths.bin_dir.join("corepack.cmd")
    } else {
        node_paths.bin_dir.join("corepack")
    };
    // 工具入口：pnpm 已托管时用托管 node 直接启动 pnpm.cjs；否则 corepack/npm
    let tool = match pm {
        PackageManager::Pnpm if pnpm_cmd.is_some() => node_paths.node.clone(),
        PackageManager::Pnpm => corepack,
        PackageManager::Npm => node_paths.npm.clone(),
    };

    // 阶段一：安装依赖（行数 → 进度文案）
    on_phase("install");
    run_tool_command(
        &tool,
        &install_args_for(pm, pnpm_cmd, install_args, workspace_root),
        repo_dir,
        node_paths,
        workspace_root,
        &[],
        &line_meter_log(lang, on_log, on_progress),
    )?;
    on_progress(Some(1.0), Some(lang.progress_install_done().to_string()));

    // 阶段二：执行构建脚本
    // 注入 DSH_CLIENT_COMMIT_HASH：仓库经 zip 下载无 .git，构建脚本无法
    // `git rev-parse HEAD`；用同步时获取的 commit SHA 覆盖（见 client-build-environment.ts）
    let build_env: Vec<(String, String)> = commit_hash
        .map(|h| vec![("DSH_CLIENT_COMMIT_HASH".to_string(), h.to_string())])
        .unwrap_or_default();
    on_phase("build");
    run_tool_command(
        &tool,
        &build_args_for(pm, pnpm_cmd, script),
        repo_dir,
        node_paths,
        workspace_root,
        &build_env,
        &line_meter_log(lang, on_log, on_progress),
    )?;
    on_progress(Some(1.0), Some(lang.progress_build_done().to_string()));

    let dist_dir = locate_dist_dir(repo_dir)?;
    Ok(dist_dir)
}

/// 包装日志回调：逐行转发的同时统计行数，把「已输出 N 行」作为不确定进度文案
/// 回调给 `on_progress`（install/build 阶段无法精确获得百分比，用活动进度提示）。
///
/// # Arguments
/// * `lang` - 展示文案语言
/// * `on_log` - 原始日志回调
/// * `on_progress` - 进度回调（百分比传 `None` 表示不确定）
fn line_meter_log<'a>(
    lang: crate::i18n::Lang,
    on_log: &'a (dyn Fn(String) + Send + Sync),
    on_progress: &'a (dyn Fn(Option<f64>, Option<String>) + Send + Sync),
) -> impl Fn(String) + Send + Sync + 'a {
    let lines = std::sync::atomic::AtomicU64::new(0);
    move |line: String| {
        let n = lines.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        on_log(line);
        on_progress(None, Some(lang.progress_lines(n)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 包管理器检测：pnpm 特征文件
    #[test]
    fn test_detect_pnpm_by_lockfile() {
        let dir = std::env::temp_dir().join("dsh_builder_test_pnpm1");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(detect_package_manager(&dir), PackageManager::Pnpm);

        std::fs::remove_file(dir.join("pnpm-lock.yaml")).unwrap();
        std::fs::write(dir.join("pnpm-workspace.yaml"), "").unwrap();
        assert_eq!(detect_package_manager(&dir), PackageManager::Pnpm);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 包管理器检测：packageManager 字段 / npm 默认
    #[test]
    fn test_detect_by_package_manager_field() {
        let dir = std::env::temp_dir().join("dsh_builder_test_pnpm2");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // packageManager 字段指定 pnpm
        std::fs::write(
            dir.join("package.json"),
            r#"{"packageManager": "pnpm@11.7.0"}"#,
        )
        .unwrap();
        assert_eq!(detect_package_manager(&dir), PackageManager::Pnpm);

        // packageManager 字段指定 npm → Npm
        std::fs::write(
            dir.join("package.json"),
            r#"{"packageManager": "npm@10.0.0"}"#,
        )
        .unwrap();
        assert_eq!(detect_package_manager(&dir), PackageManager::Npm);

        // 无任何特征 → 默认 Npm
        std::fs::remove_file(dir.join("package.json")).unwrap();
        assert_eq!(detect_package_manager(&dir), PackageManager::Npm);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 运行时环境隔离：userconfig/cache/corepack 均落在 workspace 内
    #[test]
    fn test_runtime_env_isolation() {
        let ws = Path::new("/ws");
        let np = crate::node_manager::NodePaths {
            node: ws.join("node"),
            npm: ws.join("npm"),
            bin_dir: ws.join("bin"),
        };
        let vars = runtime_env_vars(&np, ws);
        let get = |k: &str| {
            vars.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };

        assert_eq!(
            get("NPM_CONFIG_USERCONFIG"),
            ws.join("npmrc").display().to_string()
        );
        assert_eq!(
            get("npm_config_cache"),
            ws.join("npm-cache").display().to_string()
        );
        assert_eq!(
            get("COREPACK_HOME"),
            ws.join("corepack").display().to_string()
        );
        // PATH 前置托管 bin 目录
        assert!(get("PATH").as_str().starts_with(&ws.join("bin").display().to_string()));
    }

    /// 隔离 npmrc：不存在时创建，已存在时不覆盖
    #[test]
    fn test_ensure_isolated_npmrc() {
        let dir = std::env::temp_dir().join("dsh_builder_npmrc_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 首次创建
        let path = ensure_isolated_npmrc(&dir).unwrap();
        assert!(path.exists());

        // 已存在时不覆盖（手工追加内容应保留）
        std::fs::write(&path, "registry=https://example.com\n").unwrap();
        ensure_isolated_npmrc(&dir).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "registry=https://example.com\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 产物目录定位：根 dist 优先，其次 apps/*/dist
    #[test]
    fn test_locate_dist_dir() {
        let repo = std::env::temp_dir().join("dsh_builder_dist_test");
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(repo.join("apps/web/dist")).unwrap();

        // 无任何产物 → Err
        assert!(locate_dist_dir(&repo).is_err());

        // apps/web/dist 有 index.html → 定位成功
        std::fs::write(repo.join("apps/web/dist/index.html"), "<html/>").unwrap();
        let dist = locate_dist_dir(&repo).unwrap();
        assert_eq!(dist, repo.join("apps/web/dist"));

        // 根 dist 出现后优先于 apps 子包
        std::fs::create_dir_all(repo.join("dist")).unwrap();
        std::fs::write(repo.join("dist/index.html"), "<html/>").unwrap();
        let dist = locate_dist_dir(&repo).unwrap();
        assert_eq!(dist, repo.join("dist"));

        // 多个 apps 候选时按字典序取首个
        std::fs::remove_dir_all(repo.join("dist")).unwrap();
        std::fs::create_dir_all(repo.join("apps/admin/dist")).unwrap();
        std::fs::write(repo.join("apps/admin/dist/index.html"), "<html/>").unwrap();
        let dist = locate_dist_dir(&repo).unwrap();
        assert_eq!(dist, repo.join("apps/admin/dist"));

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// 命令参数生成正确（含托管 pnpm 前缀）
    #[test]
    fn test_args_generation() {
        let extra = vec!["--registry=https://example.com".to_string()];
        let ws = Path::new("/ws");
        let pnpm = Path::new("/ws/pnpm/pnpm-10.9.1/bin/pnpm.cjs");

        // npm：无前缀，直接 install
        let npm_install = install_args_for(PackageManager::Npm, None, &extra, ws);
        assert_eq!(npm_install, vec!["install", "--registry=https://example.com"]);

        // pnpm + corepack 代理：前缀 ["pnpm"]
        let pnpm_corepack = install_args_for(PackageManager::Pnpm, None, &extra, ws);
        assert_eq!(
            pnpm_corepack,
            vec![
                "pnpm",
                "install",
                &format!("--config.store-dir={}", ws.join("pnpm-store").display()),
                "--registry=https://example.com"
            ]
        );

        // pnpm + 托管：前缀为 pnpm.cjs 路径（经 node 启动）
        let pnpm_managed = install_args_for(PackageManager::Pnpm, Some(pnpm), &extra, ws);
        assert_eq!(
            pnpm_managed,
            vec![
                pnpm.display().to_string(),
                "install".to_string(),
                format!("--config.store-dir={}", ws.join("pnpm-store").display()),
                "--registry=https://example.com".to_string()
            ]
        );

        assert_eq!(
            build_args_for(PackageManager::Npm, None, "build"),
            vec!["run", "build"]
        );
        assert_eq!(
            build_args_for(PackageManager::Pnpm, None, "build"),
            vec!["pnpm", "run", "build"]
        );
        assert_eq!(
            build_args_for(PackageManager::Pnpm, Some(pnpm), "build"),
            vec![pnpm.display().to_string(), "run".to_string(), "build".to_string()]
        );
    }
}
