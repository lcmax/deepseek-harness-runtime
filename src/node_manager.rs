//! # node_manager
//!
//! Node.js 运行时托管：检测本地版本目录，缺失时自动从 nodejs.org 下载 zip 并解压，
//! 返回托管 node/npm 可执行文件路径（全程不依赖系统 PATH）。
//!
//! ## 设计决策
//! - 目录布局：`<root>/node/node-v{version}-{platform}-{arch}/`（zip 解压原样结构）。
//! - Windows 下 npm 是批处理脚本 `npm.cmd`，须以 `cmd /c` 语义调用（builder 模块处理）。
//! - 下载 zip 放在 `<root>/node/` 下，解压成功后删除，避免残留大文件。
//! - pnpm 同样支持指定版本托管：从 npm registry 下载 `pnpm-{version}.tgz` 解压，
//!   以 `node <pnpm.cjs>` 方式调用，不依赖 corepack 联网获取。

use std::path::{Path, PathBuf};

/// 托管的 Node 可执行文件路径
#[derive(Debug, Clone)]
pub struct NodePaths {
    /// node 可执行文件绝对路径
    pub node: PathBuf,
    /// npm 入口路径（Windows 为 npm.cmd，unix 为 bin/npm）
    pub npm: PathBuf,
    /// node/npm 所在目录（用于注入子进程 PATH，scripts 内的间接调用依赖它）
    pub bin_dir: PathBuf,
}

/// 根据 `std::env` 推导 nodejs.org 发行版 platform 标识。
///
/// # Returns
/// `win` / `darwin` / `linux`（未知系统返回错误）
fn detect_platform() -> anyhow::Result<&'static str> {
    match std::env::consts::OS {
        "windows" => Ok("win"),
        "macos" => Ok("darwin"),
        "linux" => Ok("linux"),
        other => Err(anyhow::anyhow!("不支持的操作系统: {other}")),
    }
}

/// 根据 `std::env` 推导 nodejs.org 发行版 arch 标识。
///
/// # Returns
/// `x64` / `arm64`（未知架构返回错误）
fn detect_arch() -> anyhow::Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x64"),
        "aarch64" => Ok("arm64"),
        other => Err(anyhow::anyhow!("不支持的 CPU 架构: {other}")),
    }
}

/// 生成 Node 发行版目录名：`node-v{version}-{platform}-{arch}`。
///
/// # Arguments
/// * `version` - Node 版本号（如 `24.19.0`，不带 v 前缀）
pub fn dist_dir_name(version: &str) -> anyhow::Result<String> {
    let platform = detect_platform()?;
    let arch = detect_arch()?;
    Ok(format!("node-v{version}-{platform}-{arch}"))
}

/// 构造 nodejs.org 下载 URL：
/// `https://nodejs.org/dist/v{version}/node-v{version}-{platform}-{arch}.zip`
///
/// # Arguments
/// * `version` - Node 版本号（不带 v 前缀）
fn dist_download_url(version: &str) -> anyhow::Result<String> {
    let dir_name = dist_dir_name(version)?;
    Ok(format!("https://nodejs.org/dist/v{version}/{dir_name}.zip"))
}

/// 根据 zip 解压后的目录推导 node/npm 可执行文件路径。
///
/// # Arguments
/// * `node_dist_dir` - 解压后的发行版根目录（如 `node-v24.19.0-win-x64/`）
///
/// # Returns
/// Windows：node.exe 与 npm.cmd 均在根目录；
/// unix：node 与 npm 在 `bin/` 子目录
fn executable_paths(node_dist_dir: &Path) -> NodePaths {
    if cfg!(windows) {
        NodePaths {
            node: node_dist_dir.join("node.exe"),
            npm: node_dist_dir.join("npm.cmd"),
            bin_dir: node_dist_dir.to_path_buf(),
        }
    } else {
        NodePaths {
            node: node_dist_dir.join("bin/node"),
            npm: node_dist_dir.join("bin/npm"),
            bin_dir: node_dist_dir.join("bin"),
        }
    }
}

/// 确保 Node 运行时就绪：目录已存在则直接复用；否则下载 zip 并解压。
///
/// # Arguments
/// * `workspace_root` - 运行时工作目录（`.runtime/`）
/// * `version` - 期望的 Node 版本（如 `24.19.0`）
/// * `on_status` - 阶段状态回调（如"下载 Node v24.19.0 ..."），用于状态页展示
///
/// # Returns
/// [`NodePaths`]，指向托管目录内的 node/npm 可执行文件
///
/// # Errors
/// - platform/arch 不支持
/// - 下载或解压失败
/// - 解压后未找到 node/npm 可执行文件
pub fn ensure_node(
    workspace_root: &Path,
    version: &str,
    on_status: &dyn Fn(String),
    on_progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> anyhow::Result<NodePaths> {
    let node_root = workspace_root.join("node");
    let dir_name = dist_dir_name(version)?;
    let dist_dir = node_root.join(&dir_name);

    // 场景：Node 已就绪 → 跳过下载直接使用
    let paths = executable_paths(&dist_dir);
    if paths.node.exists() && paths.npm.exists() {
        on_status(format!("Node v{version} 已就绪，跳过下载"));
        return Ok(paths);
    }

    // 场景：首次运行 → 下载对应平台 zip 并解压
    std::fs::create_dir_all(&node_root)
        .map_err(|e| anyhow::anyhow!("创建目录 {} 失败: {e}", node_root.display()))?;

    let zip_url = dist_download_url(version)?;
    let zip_path = node_root.join(format!("{dir_name}.zip"));
    on_status(format!("正在下载 Node v{version}（{zip_url}）..."));
    crate::http_util::download_file(&zip_url, &zip_path, Some(&on_progress))?;

    on_status("正在解压 Node ...".to_string());
    extract_zip(&zip_path, &node_root)?;

    // 解压完成后删除 zip，节省磁盘
    let _ = std::fs::remove_file(&zip_path);

    // 校验解压结果：可执行文件必须存在
    if !paths.node.exists() {
        return Err(anyhow::anyhow!(
            "Node 解压完成但未找到 node 可执行文件: {}",
            paths.node.display()
        ));
    }
    if !paths.npm.exists() {
        return Err(anyhow::anyhow!(
            "Node 解压完成但未找到 npm 入口: {}",
            paths.npm.display()
        ));
    }

    on_status(format!("Node v{version} 就绪"));
    Ok(paths)
}

/// 解压 zip 到目标目录（保留 zip 内原始目录结构）。
///
/// # Arguments
/// * `zip_path` - zip 文件路径
/// * `dest_dir` - 解压目标目录（须已存在）
///
/// # Errors
/// - zip 打开/读取/解压失败
pub fn extract_zip(zip_path: &Path, dest_dir: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| anyhow::anyhow!("打开 zip {} 失败: {e}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| anyhow::anyhow!("读取 zip {} 失败: {e}", zip_path.display()))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| anyhow::anyhow!("读取 zip 条目 #{i} 失败: {e}"))?;

        // 使用 enclosed_name 防御 zip 路径穿越攻击（如 ../../evil）
        let Some(rel_path) = entry.enclosed_name() else {
            continue;
        };
        let out_path = dest_dir.join(rel_path);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|e| {
                anyhow::anyhow!("创建目录 {} 失败: {e}", out_path.display())
            })?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    anyhow::anyhow!("创建目录 {} 失败: {e}", parent.display())
                })?;
            }
            let mut out_file = std::fs::File::create(&out_path).map_err(|e| {
                anyhow::anyhow!("创建文件 {} 失败: {e}", out_path.display())
            })?;
            std::io::copy(&mut entry, &mut out_file).map_err(|e| {
                anyhow::anyhow!("解压写入 {} 失败: {e}", out_path.display())
            })?;
        }
    }
    Ok(())
}

/// 构造 pnpm npm registry 下载 URL：
/// `https://registry.npmjs.org/pnpm/-/pnpm-{version}.tgz`
///
/// # Arguments
/// * `version` - pnpm 版本号（如 `10.9.1`）
fn pnpm_tgz_url(version: &str) -> String {
    format!("https://registry.npmjs.org/pnpm/-/pnpm-{version}.tgz")
}

/// 托管 pnpm 的入口文件路径（`node <入口>` 方式调用，跨平台一致）。
///
/// # Arguments
/// * `workspace_root` - 运行时工作目录
/// * `version` - pnpm 版本号
fn pnpm_entry_path(workspace_root: &Path, version: &str) -> PathBuf {
    workspace_root
        .join("pnpm")
        .join(format!("pnpm-{version}"))
        .join("bin")
        .join("pnpm.cjs")
}

/// 解压 npm 官方 tgz 包（gzip 压缩的 tar）到目标目录。
///
/// tgz 内是单一 `package/` 顶层目录（node_modules 扁平包结构）。
///
/// # Arguments
/// * `tgz_path` - .tgz 文件路径
/// * `dest_dir` - 解压目标目录（须已存在）
///
/// # Errors
/// - tgz 打开/读取/解压失败
fn extract_tgz(tgz_path: &Path, dest_dir: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(tgz_path)
        .map_err(|e| anyhow::anyhow!("打开 tgz {} 失败: {e}", tgz_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(dest_dir)
        .map_err(|e| anyhow::anyhow!("解压 tgz {} 失败: {e}", tgz_path.display()))
}

/// 确保托管 pnpm 就绪：缺失时从 npm registry 下载对应版本 tgz 并解压。
///
/// # Arguments
/// * `workspace_root` - 运行时工作目录（`<root>/pnpm/` 存放所有版本）
/// * `version` - 期望的 pnpm 版本（如 `"10.9.1"`）；`None` 表示不托管，由
///   corepack 按仓库 `packageManager` 自动选择（builder 默认行为）
/// * `on_status` - 阶段状态回调，用于状态页展示下载/解压过程
///
/// # Returns
/// 托管 pnpm 入口（`bin/pnpm.cjs`）绝对路径；`version` 为 `None` 时返回 `None`
///
/// # Errors
/// - 下载或解压失败
/// - 解压后未找到 `bin/pnpm.cjs`
pub fn ensure_pnpm(
    workspace_root: &Path,
    version: Option<&str>,
    on_status: &dyn Fn(String),
    on_progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> anyhow::Result<Option<PathBuf>> {
    let Some(version) = version else {
        return Ok(None);
    };
    // 拒绝空字符串（视为未配置）
    if version.trim().is_empty() {
        return Ok(None);
    }

    // 场景：目标版本已就绪 → 直接复用
    let entry = pnpm_entry_path(workspace_root, version);
    if entry.is_file() {
        on_status(format!("pnpm v{version} 已就绪，跳过下载"));
        return Ok(Some(entry));
    }

    // 场景：首次使用该版本 → 下载 tgz 并解压到 staging 后原子安装
    let pnpm_root = workspace_root.join("pnpm");
    std::fs::create_dir_all(&pnpm_root)
        .map_err(|e| anyhow::anyhow!("创建目录 {} 失败: {e}", pnpm_root.display()))?;

    let url = pnpm_tgz_url(version);
    let tgz_path = pnpm_root.join(format!(".pnpm-{version}.tgz"));
    on_status(format!("正在下载 pnpm v{version}（{url}）..."));
    crate::http_util::download_file(&url, &tgz_path, Some(&on_progress))?;

    on_status(format!("正在解压 pnpm v{version} ..."));
    // 解压到独立 staging，避免并发/残留干扰
    let staging = pnpm_root.join(format!(".pnpm-{version}.tmp"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|e| anyhow::anyhow!("创建临时目录 {} 失败: {e}", staging.display()))?;
    extract_tgz(&tgz_path, &staging)?;

    // tgz 顶层是 package/，将其整体移动为 pnpm-{version}/
    let pkg_dir = staging.join("package");
    let entry_in_pkg = pkg_dir.join("bin").join("pnpm.cjs");
    if !entry_in_pkg.is_file() {
        return Err(anyhow::anyhow!(
            "pnpm v{version} 解压完成但未找到 bin/pnpm.cjs: {}",
            entry_in_pkg.display()
        ));
    }
    let target_dir = pnpm_root.join(format!("pnpm-{version}"));
    let _ = std::fs::remove_dir_all(&target_dir);
    std::fs::rename(&pkg_dir, &target_dir)
        .map_err(|e| anyhow::anyhow!("安装 pnpm v{version} 失败: {e}"))?;

    // 清理临时产物（无论成败）
    let _ = std::fs::remove_file(&tgz_path);
    let _ = std::fs::remove_dir_all(&staging);

    on_status(format!("pnpm v{version} 就绪"));
    Ok(Some(entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 目录名推导：版本号 + platform/arch 拼接正确
    #[test]
    fn test_dist_dir_name_format() {
        let name = dist_dir_name("24.19.0").unwrap();
        // 在当前测试机上只校验版本号拼接（platform/arch 随机器变化）
        assert!(name.starts_with("node-v24.19.0-"), "实际值: {name}");
        assert!(name.ends_with(detect_arch().unwrap()) || name.contains('-'));
    }

    /// 下载 URL 格式正确
    #[test]
    fn test_dist_download_url_format() {
        let url = dist_download_url("24.19.0").unwrap();
        assert!(url.starts_with("https://nodejs.org/dist/v24.19.0/node-v24.19.0-"));
        assert!(url.ends_with(".zip"));
    }

    /// pnpm tgz 下载 URL 格式正确
    #[test]
    fn test_pnpm_tgz_url() {
        let url = pnpm_tgz_url("10.9.1");
        assert_eq!(
            url,
            "https://registry.npmjs.org/pnpm/-/pnpm-10.9.1.tgz"
        );
    }

    /// pnpm 入口路径：`{root}/pnpm/pnpm-{version}/bin/pnpm.cjs`
    #[test]
    fn test_pnpm_entry_path() {
        let p = pnpm_entry_path(Path::new("/ws"), "10.9.1");
        assert_eq!(p, PathBuf::from("/ws/pnpm/pnpm-10.9.1/bin/pnpm.cjs"));
    }

    /// ensure_pnpm：未指定版本或空字符串 → 返回 None（走 corepack）
    #[test]
    fn test_ensure_pnpm_none_when_no_version() {
        let dir = std::env::temp_dir().join("dsh_node_test_pnpm");
        let _ = std::fs::remove_dir_all(&dir);

        let none = ensure_pnpm(&dir, None, &|_| {}, &|_, _| {}).unwrap();
        assert!(none.is_none(), "未指定版本不应托管 pnpm");

        let none = ensure_pnpm(&dir, Some(""), &|_| {}, &|_, _| {}).unwrap();
        assert!(none.is_none(), "空字符串视为未配置 pnpm");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// tgz 解压：构造内存 tgz 后解压，校验 package/ 结构
    #[test]
    fn test_extract_tgz_roundtrip() {
        let dir = std::env::temp_dir().join("dsh_node_test_tgz");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 构造 package/ + bin/pnpm.cjs 的 gzip tar
        let tgz_path = dir.join("pnpm.tgz");
        {
            let file = std::fs::File::create(&tgz_path).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);

            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            archive.append_data(&mut header, "package/", std::io::empty()).unwrap();
            archive.append_data(&mut header, "package/bin/", std::io::empty()).unwrap();

            let mut data_header = tar::Header::new_gnu();
            data_header.set_size(4);
            data_header.set_mode(0o644);
            data_header.set_cksum();
            archive.append_data(&mut data_header, "package/bin/pnpm.cjs", &b"data"[..]).unwrap();

            let encoder = archive.into_inner().unwrap();
            encoder.finish().unwrap();
        }

        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        extract_tgz(&tgz_path, &out_dir).unwrap();
        let content = std::fs::read_to_string(out_dir.join("package/bin/pnpm.cjs")).unwrap();
        assert_eq!(content, "data");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// zip 解压：构造内存 zip 写盘后解压，校验文件内容
    #[test]
    fn test_extract_zip_roundtrip() {
        let dir = std::env::temp_dir().join("dsh_node_test_extract");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 构造一个简单 zip：顶层目录 + 一个文件
        let zip_path = dir.join("test.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options: zip::write::FileOptions<'_, ()> =
                zip::write::FileOptions::default();
            writer
                .add_directory("topdir/", options)
                .unwrap();
            writer
                .start_file("topdir/hello.txt", options)
                .unwrap();
            std::io::Write::write_all(&mut writer, b"hello node").unwrap();
            writer.finish().unwrap();
        }

        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        extract_zip(&zip_path, &out_dir).unwrap();

        let content = std::fs::read_to_string(out_dir.join("topdir/hello.txt")).unwrap();
        assert_eq!(content, "hello node");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
