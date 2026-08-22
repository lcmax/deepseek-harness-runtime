//! # host_runner
//!
//! `dsh web` 宿主进程管理：构建完成后启动完整 Web 宿主（HTTP 静态服务 +
//! `window.__DSH_BOOT__` manifest 注入 + 插件 bundle 端点 + WebSocket API），
//! 解析就绪 URL 后交由主线程加载。
//!
//! ## 设计决策
//! - `apps/web` 的 dist 是 build-only 壳：裸加载静态文件必然白屏
//!   （`client-modules: window.__DSH_BOOT__ is missing or not an object`，
//!   见仓库 postmortem 0003），因此运行时改为托管 `dsh --profile web` 宿主进程。
//! - `DSH_HOME` 重定向到 `<workspace>/dsh-home`：profile、存储、会话数据全部
//!   落在运行时容器内，不读写系统全局 `~/.dsh`。
//! - `--port 0` 由 OS 分配空闲端口（避免默认 3080 被占用冲突）；就绪行
//!   `dsh web: http://127.0.0.1:<port>` 是宿主的官方就绪信号，从 stdout 解析
//!   实际 URL 后返回。
//! - stdout/stderr 由 detached 线程持续逐行转发（进程存活期间全程伴随），
//!   就绪行等待逻辑经 mpsc channel 与 stdout 线程解耦。

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::node_manager::NodePaths;

/// 就绪行前缀（`web-app` 插件 printUrl 输出，格式：
/// `dsh web: http://127.0.0.1:<port>`，可选追加 ` (LAN: http://<lan>:<port>)`）
const READY_PREFIX: &str = "dsh web: ";

/// 就绪等待上限：首次启动需初始化 profile、回填 node_modules 符号链接，
/// 耗时较长；后续启动通常数秒内完成。
const READY_TIMEOUT: Duration = Duration::from_secs(180);

/// 轮询间隔：就绪行到达与子进程异常退出的双重检测周期
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 运行中的宿主进程句柄。
pub struct HostProcess {
    /// 宿主子进程；调用方负责在应用退出前 kill（读线程不持有所有权）
    pub child: std::process::Child,
    /// 就绪行解析出的规范 URL（`http://127.0.0.1:<port>`）
    pub url: String,
}

/// 从就绪行提取规范 URL。
///
/// 行格式：`dsh web: http://127.0.0.1:34567 (LAN: http://192.168.1.2:34567)`，
/// 取前缀后的第一个空白分隔 token（即 loopback URL）。
///
/// # Arguments
/// * `line` - 完整就绪行
///
/// # Returns
/// 解析出的 URL；行内无可用 token 时返回 None
fn extract_url(line: &str) -> Option<String> {
    line.strip_prefix(READY_PREFIX)?
        .split_whitespace()
        .next()
        .filter(|token| token.starts_with("http://"))
        .map(|token| token.to_string())
}

/// 启动 `dsh web` 宿主进程并等待就绪。
///
/// # Arguments
/// * `node_paths` - 托管 Node 工具链（node 入口 / bin 目录）
/// * `repo_dir` - 仓库源码目录（含构建产物与 CLI 入口 `apps/cli/lib/bin.js`）
/// * `workspace_root` - 运行时工作目录（缓存隔离落点）
/// * `send_log` - 日志回调（stdout/stderr 逐行转发，进程存活期间持续调用）
///
/// # Returns
/// 就绪后的宿主句柄（含子进程与实际 URL）
///
/// # Errors
/// - CLI 入口不存在（未构建）
/// - 进程启动失败
/// - 等待就绪超时（180s）
/// - 进程在就绪前退出
pub fn spawn_web_host(
    node_paths: &NodePaths,
    repo_dir: &Path,
    workspace_root: &Path,
    send_log: Arc<dyn Fn(String) + Send + Sync>,
) -> anyhow::Result<HostProcess> {
    let bin_js = repo_dir.join("apps").join("cli").join("lib").join("bin.js");
    if !bin_js.is_file() {
        return Err(anyhow::anyhow!(
            "dsh CLI 入口不存在: {}（请确认构建流程已完成）",
            bin_js.display()
        ));
    }

    // DSH_HOME 容器隔离：profile / storages / 会话数据全部落在 workspace
    let dsh_home = workspace_root.join("dsh-home");
    std::fs::create_dir_all(&dsh_home)
        .map_err(|e| anyhow::anyhow!("创建 dsh-home 失败: {e}"))?;

    send_log(format!(
        "$ node apps/cli/lib/bin.js web --port 0 --no-open  (DSH_HOME={})",
        dsh_home.display()
    ));

    let mut command = std::process::Command::new(&node_paths.node);
    command
        .arg(&bin_js)
        .arg("web")
        .arg("--port")
        .arg("0")
        .arg("--no-open")
        .current_dir(repo_dir)
        .env("DSH_HOME", &dsh_home)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // 与 builder 相同的运行时隔离：PATH 前置托管 bin、npm 配置/缓存重定向
    let separator = if cfg!(windows) { ";" } else { ":" };
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    command.env(
        "PATH",
        format!("{}{separator}{inherited_path}", node_paths.bin_dir.display()),
    );
    command.env(
        "NPM_CONFIG_USERCONFIG",
        workspace_root.join("npmrc").display().to_string(),
    );
    command.env(
        "npm_config_cache",
        workspace_root.join("npm-cache").display().to_string(),
    );
    command.env(
        "COREPACK_HOME",
        workspace_root.join("corepack").display().to_string(),
    );

    let mut child = command
        .spawn()
        .map_err(|e| anyhow::anyhow!("启动 dsh web 宿主失败: {e}"))?;

    // stdout 线程：逐行转发 + 就绪行投递 channel
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let stdout_handle = {
        let reader = BufReader::new(child.stdout.take().expect("stdout 已管道化"));
        let send_log = Arc::clone(&send_log);
        std::thread::spawn(move || {
            for line in reader.lines().map_while(|l| l.ok()) {
                send_log(line.clone());
                // 就绪后接收端 drop，send 失败静默忽略
                let _ = line_tx.send(line);
            }
        })
    };
    // stderr 线程：仅转发
    let stderr_handle = {
        let reader = BufReader::new(child.stderr.take().expect("stderr 已管道化"));
        let send_log = Arc::clone(&send_log);
        std::thread::spawn(move || {
            for line in reader.lines().map_while(|l| l.ok()) {
                send_log(line);
            }
        })
    };
    // 读线程伴随进程生命周期，不 join（进程退出后 reader 到 EOF 自然结束）
    let _ = (stdout_handle, stderr_handle);

    // 等待就绪行：超时 / 子进程提前退出 / 解析成功 三路收敛
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        // 子进程已退出则不可能再就绪
        if let Some(status) = child
            .try_wait()
            .map_err(|e| anyhow::anyhow!("检查宿主进程状态失败: {e}"))?
        {
            return Err(anyhow::anyhow!(
                "dsh web 宿主启动即退出（exit={:?}），详见日志",
                status.code()
            ));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = child.kill();
            return Err(anyhow::anyhow!(
                "等待 dsh web 就绪超时（{}s），已终止宿主进程",
                READY_TIMEOUT.as_secs()
            ));
        }

        // 非阻塞式轮询：短超时窗口内可能收到多行，全部检查
        match line_rx.recv_timeout(remaining.min(POLL_INTERVAL)) {
            Ok(line) => {
                if let Some(url) = extract_url(&line) {
                    return Ok(HostProcess { child, url });
                }
            }
            // 时间窗口内无新行 → 回到循环头复查超时与进程状态
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            // 发送端（stdout 线程）已结束：进程 stdout 关闭，等价于退出
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!(
                    "dsh web 宿主输出流提前关闭（未输出就绪行），详见日志"
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 就绪行 URL 提取：标准行 / LAN 附加 / 非就绪行 / 畸形行
    #[test]
    fn test_extract_url() {
        assert_eq!(
            extract_url("dsh web: http://127.0.0.1:34567"),
            Some("http://127.0.0.1:34567".to_string())
        );
        assert_eq!(
            extract_url("dsh web: http://127.0.0.1:80 (LAN: http://192.168.1.2:80)"),
            Some("http://127.0.0.1:80".to_string())
        );
        // 前缀不符 / URL 缺失 / 非 http 协议 → None
        assert_eq!(extract_url("some other line"), None);
        assert_eq!(extract_url("dsh web: "), None);
        assert_eq!(extract_url("dsh web: ftp://x"), None);
    }
}
