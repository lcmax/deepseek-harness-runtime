//! # http_util
//!
//! 纯 HTTP 客户端封装（reqwest blocking + rustls，避免 native-tls 依赖）。
//!
//! ## 设计决策
//! - GitHub API 强制要求 `User-Agent`，统一在本模块注入。
//! - 下载采用流式写盘（64 KiB 分块），避免大文件（Node zip ~30MB）整体载入内存。
//! - 全部远端操作走 HTTPS，不调用任何本地 git 命令。

use std::io::Read as _;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// 统一 User-Agent（GitHub API 必需，标识本运行时）
const USER_AGENT: &str = "rustc-deepseek-harness/0.1.0";

/// HTTP 请求超时（连接+读取）；下载大文件时只限制单次读阻塞时长
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// 流式下载分块大小（64 KiB）
const DOWNLOAD_CHUNK_SIZE: usize = 64 * 1024;

/// 构建带 UA 与超时的 blocking 客户端。
///
/// # Returns
/// 可复用的 `reqwest::blocking::Client`（内建连接池）
fn build_client() -> anyhow::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| anyhow::anyhow!("构建 HTTP 客户端失败: {e}"))
}

/// GET 请求并反序列化 JSON 响应。
///
/// # Arguments
/// * `url` - 目标地址（如 GitHub API commits 端点）
///
/// # Returns
/// 反序列化后的 `T`（如 `serde_json::Value` 或具体结构）
///
/// # Errors
/// - 网络/超时错误
/// - 非 2xx 状态码
/// - 响应体不是合法 JSON 或不匹配 `T`
pub fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> anyhow::Result<T> {
    let resp = build_client()?
        .get(url)
        .send()
        .map_err(|e| anyhow::anyhow!("GET {url} 请求失败: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!("GET {url} 返回非成功状态: {status}"));
    }

    resp.json::<T>()
        .map_err(|e| anyhow::anyhow!("GET {url} 响应 JSON 解析失败: {e}"))
}

/// GET 请求并将响应体流式下载到磁盘文件。
///
/// # Arguments
/// * `url` - 资源地址（如 codeload zip / nodejs.org zip）
/// * `dest` - 目标文件路径（父目录须已存在；文件已存在则覆盖）
///
/// # Returns
/// 成功返回 `()`；失败时已写入的部分文件会被清理
///
/// # Errors
/// - 网络/超时错误、非 2xx 状态码
/// - 磁盘写入失败
pub fn download_file(url: &str, dest: &Path) -> anyhow::Result<()> {
    let resp = build_client()?
        .get(url)
        .send()
        .map_err(|e| anyhow::anyhow!("下载 {url} 请求失败: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!("下载 {url} 返回非成功状态: {status}"));
    }

    // 先写临时文件，成功后原子重命名，避免半成品文件被后续逻辑误用
    let tmp_dest = dest.with_extension("part");
    let write_result = write_response_to_file(resp, &tmp_dest);
    match write_result {
        Ok(()) => {
            std::fs::rename(&tmp_dest, dest)
                .map_err(|e| anyhow::anyhow!("重命名 {} -> {} 失败: {e}", tmp_dest.display(), dest.display()))?;
            Ok(())
        }
        Err(e) => {
            // 失败清理半成品临时文件
            let _ = std::fs::remove_file(&tmp_dest);
            Err(e)
        }
    }
}

/// 将响应体流式写入文件（内部辅助函数）。
///
/// # Arguments
/// * `resp` - 已发出请求的 blocking Response
/// * `path` - 目标文件路径
fn write_response_to_file(
    mut resp: reqwest::blocking::Response,
    path: &Path,
) -> anyhow::Result<()> {
    let mut file = std::fs::File::create(path)
        .map_err(|e| anyhow::anyhow!("创建文件 {} 失败: {e}", path.display()))?;

    let mut chunk = vec![0u8; DOWNLOAD_CHUNK_SIZE];
    loop {
        let n = resp
            .read(&mut chunk)
            .map_err(|e| anyhow::anyhow!("读取响应体失败: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&chunk[..n])
            .map_err(|e| anyhow::anyhow!("写入文件 {} 失败: {e}", path.display()))?;
    }
    file.flush().map_err(|e| anyhow::anyhow!("刷新文件 {} 失败: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// URL 拼接与 UA 常量冒烟测试（不发真实网络请求）
    #[test]
    fn test_user_agent_defined() {
        assert!(USER_AGENT.starts_with("rustc-deepseek-harness/"));
    }
}
