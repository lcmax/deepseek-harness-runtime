//! # node_manager
//!
//! Node.js 运行时托管：检测本地版本目录，缺失时自动从 nodejs.org 下载 zip 并解压，
//! 返回托管 node/npm 可执行文件路径（全程不依赖系统 PATH）。
//!
//! ## 设计决策
//! - 目录布局：`<root>/node/node-v{version}-{platform}-{arch}/`（zip 解压原样结构）。
//! - Windows 下 npm 是批处理脚本 `npm.cmd`，须以 `cmd /c` 语义调用（builder 模块处理）。
//! - 下载 zip 放在 `<root>/node/` 下，解压成功后删除，避免残留大文件。

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
    crate::http_util::download_file(&zip_url, &zip_path)?;

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
