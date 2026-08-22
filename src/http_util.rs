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

/// HTTP 请求超时（连接+读取）；下载大文件时只限制单次读阻塞超时
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// 流式下载分块大小（64 KiB）
const DOWNLOAD_CHUNK_SIZE: usize = 64 * 1024;

/// 错误分类：区分「连不上/超时」与「服务可达但异常」，供离线快速启动决策。
#[derive(Debug)]
pub enum HttpError {
    /// 网络不可达 / 请求超时（不会重试即可判断离线）
    Unreachable(String),
    /// 服务可达但返回非 2xx
    HttpStatus(u16),
    /// 其他（客户端构造、响应解析等）
    Other(anyhow::Error),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Unreachable(e) => write!(f, "网络不可达/超时: {e}"),
            HttpError::HttpStatus(s) => write!(f, "HTTP 状态码异常: {s}"),
            HttpError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<anyhow::Error> for HttpError {
    fn from(e: anyhow::Error) -> Self {
        HttpError::Other(e)
    }
}

/// 构建带 UA 与指定超时的 blocking 客户端。
///
/// # Arguments
/// * `timeout` - 单次请求整体超时（连接+读取）
fn build_client_with_timeout(timeout: Duration) -> anyhow::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .build()
        .map_err(|e| anyhow::anyhow!("构建 HTTP 客户端失败: {e}"))
}

/// 构建带默认超时的 blocking 客户端。
///
/// # Returns
/// 可复用的 `reqwest::blocking::Client`（内建连接池）
fn build_client() -> anyhow::Result<reqwest::blocking::Client> {
    build_client_with_timeout(HTTP_TIMEOUT)
}


/// GET 请求并反序列化 JSON 响应（带自定义超时与错误分类）。
///
/// 供「启动时检查远端更新」使用：传短超时（如 10s），用 [`HttpError`] 区分
/// 网络不可达与普通 HTTP 错误，从而决定离线复用宿主还是走下载回退。
///
/// # Arguments
/// * `url` - 目标地址
/// * `timeout` - 请求整体超时
///
/// # Returns
/// 反序列化后的 `T`（如具体响应结构）
///
/// # Errors
/// 分类为 [`HttpError::Unreachable`]（超时/连接失败）或 [`HttpError::HttpStatus`] 等
pub fn get_json_with_timeout<T: serde::de::DeserializeOwned>(
    url: &str,
    timeout: Duration,
) -> Result<T, HttpError> {
    let resp = build_client_with_timeout(timeout)
        .map_err(HttpError::Other)?
        .get(url)
        .send()
        .map_err(|e| {
            if e.is_timeout() || e.is_connect() {
                HttpError::Unreachable(e.to_string())
            } else {
                HttpError::Other(anyhow::anyhow!("GET {url} 请求失败: {e}"))
            }
        })?;

    let status = resp.status();
    if !status.is_success() {
        return Err(HttpError::HttpStatus(status.as_u16()));
    }

    resp.json::<T>()
        .map_err(|e| HttpError::Other(anyhow::anyhow!("GET {url} 响应 JSON 解析失败: {e}")))
}

/// 下载进度回调类型：`(已下载字节, 总字节，None 表示服务端未提供总长)`
pub type DownloadProgress<'a> = dyn Fn(u64, Option<u64>) + Send + Sync + 'a;

/// GET 请求并将响应体流式下载到磁盘文件（可选进度回调）。
///
/// # Arguments
/// * `url` - 资源地址（如 codeload zip / nodejs.org zip / pnpm tgz）
/// * `dest` - 目标文件路径（父目录须已存在；文件已存在则覆盖）
/// * `on_progress` - 可选进度回调（分块写入时调用；可安全传 `None` 跳过）
///
/// # Returns
/// 成功返回 `()`；失败时已写入的部分文件会被清理
///
/// # Errors
/// - 网络/超时错误、非 2xx 状态码
/// - 磁盘写入失败
pub fn download_file(
    url: &str,
    dest: &Path,
    on_progress: Option<&DownloadProgress<'_>>,
) -> anyhow::Result<()> {
    // 先写临时文件，成功后原子重命名，避免半成品文件被后续逻辑误用
    let tmp_dest = dest.with_extension("part");
    let write_result = (|| {
        let resp = build_client()?
            .get(url)
            .send()
            .map_err(|e| anyhow::anyhow!("下载 {url} 请求失败: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!("下载 {url} 返回非成功状态: {status}"));
        }

        let total = resp.content_length();
        write_response_to_file(resp, &tmp_dest, total, on_progress)
    })();

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
/// * `total` - 服务端声明的总字节数（可能为 `None`）
/// * `on_progress` - 可选进度回调
fn write_response_to_file(
    mut resp: reqwest::blocking::Response,
    path: &Path,
    total: Option<u64>,
    on_progress: Option<&DownloadProgress<'_>>,
) -> anyhow::Result<()> {
    let mut file = std::fs::File::create(path)
        .map_err(|e| anyhow::anyhow!("创建文件 {} 失败: {e}", path.display()))?;

    let mut written: u64 = 0;
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
        written += n as u64;
        if let Some(cb) = on_progress {
            cb(written, total);
        }
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
