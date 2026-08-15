//! # watcher
//!
//! 基于 notify 递归监听 `repo/dist` 目录，文件变化去抖（300ms 静默期）后
//! 经 `EventLoopProxy` 发送 `Reload` 事件，由主线程驱动 webview 刷新。
//!
//! ## 设计决策
//! - 去抖策略：首个事件到达后开始计时，持续有新事件则顺延，静默满 300ms 才触发
//!   （构建期一次性写入大量文件时只刷新一次）。
//! - watcher 线程在 Ready 之后启动，保证 dist 目录已存在。

use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use notify::Watcher;
use tao::event_loop::EventLoopProxy;

use crate::UserEvent;

/// 去抖静默期：最后一次文件事件后等待该时长才触发 Reload
const DEBOUNCE: Duration = Duration::from_millis(300);

/// 启动 dist 监听线程。
///
/// # Arguments
/// * `dist_dir` - 待监听的构建产物目录（须已存在）
/// * `proxy` - 事件循环代理，用于跨线程发送 `UserEvent::Reload`
///
/// # Returns
/// 成功启动返回 `()`；watcher 初始化或注册监听失败返回 `Err`
///
/// # Errors
/// - notify watcher 创建失败
/// - dist 目录注册监听失败（目录不存在/无权限）
pub fn spawn_watcher(dist_dir: PathBuf, proxy: EventLoopProxy<UserEvent>) -> anyhow::Result<()> {
    // 通道接收原始事件；watcher 需 'static，因此用独立线程持有
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)
        .map_err(|e| anyhow::anyhow!("创建文件监听器失败: {e}"))?;
    watcher
        .watch(&dist_dir, notify::RecursiveMode::Recursive)
        .map_err(|e| {
            anyhow::anyhow!("注册监听目录 {} 失败: {e}", dist_dir.display())
        })?;

    std::thread::Builder::new()
        .name("dist-watcher".to_string())
        .spawn(move || {
            // watcher 移入本线程，保证事件持续投递到 rx
            let _watcher = watcher;
            debounce_loop(&rx, &proxy);
        })
        .map_err(|e| anyhow::anyhow!("启动监听线程失败: {e}"))?;

    Ok(())
}

/// 去抖主循环：收到事件后静默 `DEBOUNCE` 时长才发送 Reload。
///
/// # Arguments
/// * `rx` - notify 原始事件接收端
/// * `proxy` - 事件循环代理
fn debounce_loop(rx: &std::sync::mpsc::Receiver<notify::Result<notify::Event>>, proxy: &EventLoopProxy<UserEvent>) {
    loop {
        // 阻塞等待首个事件（Err 事件同样视为变化信号，保守触发刷新）
        match rx.recv() {
            Ok(_) | Err(_) => {}
        }

        // 静默去抖窗口：持续有事件则重置计时
        let deadline = Instant::now() + DEBOUNCE;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match rx.recv_timeout(remaining) {
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        // dist 产物变化 → 通知主线程刷新 webview
        if proxy.send_event(UserEvent::Reload).is_err() {
            // 事件循环已关闭（窗口退出），结束监听线程
            return;
        }
    }
}
