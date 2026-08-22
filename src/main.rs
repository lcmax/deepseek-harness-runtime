//! # rustc-deepseek-harness 主入口
//!
//! tao+wry 类 Electron 运行时编排：加载配置 → 创建窗口/webview（状态页）→
//! 后台 worker 线程串联「Node 托管 → 仓库增量同步 → 依赖安装 → 构建 →
//! dsh web 宿主启动」→ HostReady 后加载宿主 URL（完整 Web GUI）。
//!
//! ## 流程（对应 spec 顶层设计）
//! 1. 解析配置，创建事件循环 / 窗口 / webview，加载内置状态页
//! 2. spawn worker：各阶段经 `EventLoopProxy` 发 `Status`/`Log` 更新状态页
//! 3. 构建成功发 `Ready(dist)`：注入 dist 路径并启动产物监听
//! 4. `dsh web` 宿主就绪发 `HostReady(url)`：主线程 `load_url` 宿主 URL
//!    （dist 是 build-only 壳，必须经宿主注入 `window.__DSH_BOOT__` 才能启动）
//! 5. watcher 监听 `repo/dist`：去抖后发 `Reload`，主线程 reload 页面

mod builder;
mod config;
mod host_runner;
mod http_util;
mod i18n;
mod node_manager;
mod repo_manager;
mod watcher;
mod webview_app;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::event::{Event, WindowEvent};

use crate::config::Config;
use crate::i18n::Lang;

/// 主线程与 worker 线程之间传递的自定义事件
#[derive(Debug, Clone)]
enum UserEvent {
    /// 阶段状态变化（stage: node/repo/install/build/host；state: running/done/failed）
    Status {
        /// 阶段标识
        stage: &'static str,
        /// 状态：running / done / failed
        state: &'static str,
        /// 展示文案
        detail: String,
    },
    /// 阶段进度条更新（stage: repo/install/build；progress: 0.0~1.0 或 None 不确定）
    Progress {
        /// 阶段标识
        stage: &'static str,
        /// 进度值（`None` 表示不确定，前端显示滑动动画）
        progress: Option<f64>,
        /// 进度文案（如"下载中 32% …"）
        detail: Option<String>,
    },
    /// 流式日志行（构建输出等）
    Log(String),
    /// 构建就绪，携带 dist 目录绝对路径
    Ready(PathBuf),
    /// dsh web 宿主就绪，携带实际服务 URL（http://127.0.0.1:<port>）与版本标签
    HostReady { url: String, version_label: String },
    /// dist 产物变化，触发 webview 刷新
    Reload,
}

fn main() {
    // ---- 阶段 0：配置 ----
    let cfg = Config::load(None).unwrap_or_else(|e| {
        eprintln!("加载配置失败: {e}");
        std::process::exit(1);
    });
    let lang = Lang::detect();

    // ---- 阶段 1：窗口 + 状态页 ----
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = webview_app::create_window(&event_loop);

    // dist 目录共享：协议处理器读，Ready 事件写
    let shared_dist: webview_app::SharedDistDir = Arc::new(Mutex::new(None));
    let webview = webview_app::create_webview(&window, Arc::clone(&shared_dist), lang)
        .unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        });

    // dsh web 宿主进程句柄共享：worker 写入（就绪后），退出时 kill
    let shared_host: Arc<Mutex<Option<std::process::Child>>> = Arc::new(Mutex::new(None));

    // ---- 阶段 2：后台 worker ----
    {
        let proxy = proxy.clone();
        let cfg = cfg.clone();
        let shared_host = Arc::clone(&shared_host);
        std::thread::Builder::new()
            .name("bootstrap-worker".to_string())
            .spawn(move || {
                if let Err(e) = run_bootstrap(&cfg, lang, &proxy, &shared_host) {
                    eprintln!("启动流程失败: {e:#}");
                    let _ = proxy.send_event(UserEvent::Log(format!("[错误] {e:#}")));
                }
            })
            .expect("启动 worker 线程失败");
    }

    // ---- 阶段 3/4：事件循环 ----
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(ev) => {
                handle_user_event(ev, &proxy, &webview, &shared_dist, &window)
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            // 事件循环销毁（窗口关闭）：终止宿主进程，避免遗留后台 node
            Event::LoopDestroyed => {
                if let Ok(mut guard) = shared_host.lock() {
                    if let Some(mut child) = guard.take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
            }
            _ => {}
        }
    });
}

/// 处理自定义事件：状态更新 / 日志 / 就绪加载 / 自动刷新。
fn handle_user_event(
    ev: UserEvent,
    proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
    webview: &wry::WebView,
    shared_dist: &webview_app::SharedDistDir,
    window: &tao::window::Window,
) {
    match ev {
        UserEvent::Status { stage, state, detail } => {
            println!("[{stage}] {state}: {detail}");
            let _ = webview.evaluate_script(&webview_app::eval_set_stage(stage, state, &detail));
        }
        UserEvent::Progress { stage, progress, detail } => {
            let _ = webview.evaluate_script(&webview_app::eval_progress(
                stage,
                progress,
                detail.as_deref(),
            ));
        }
        UserEvent::Log(line) => {
            println!("{line}");
            let _ = webview.evaluate_script(&webview_app::eval_log(&line));
        }
        UserEvent::Ready(dist_path) => {
            // 注入 dist 路径（dist:// 协议备用）→ 启动产物监听
            println!("构建就绪: {}", dist_path.display());
            if let Ok(mut guard) = shared_dist.lock() {
                *guard = Some(dist_path.clone());
            }
            if let Err(e) = watcher::spawn_watcher(dist_path, proxy.clone()) {
                eprintln!("启动 dist 监听失败: {e}");
            }
        }
        UserEvent::HostReady { url, version_label } => {
            // 宿主就绪：加载完整 Web GUI（宿主注入 __DSH_BOOT__ 并服务 API）
            println!("dsh web 宿主就绪: {url}");
            window.set_title(&format!("DeepSeek Harness Runtime - {version_label}"));
            if let Err(e) = webview.load_url(&url) {
                eprintln!("加载宿主 URL 失败: {e:?}");
            }
        }
        UserEvent::Reload => {
            println!("检测到产物更新，刷新 webview");
            let _ = webview.reload();
        }
    }
}

/// 后台启动流程：Node 托管 → 仓库增量同步 → 安装 → 构建 → dsh web 宿主。
///
/// # Arguments
/// * `cfg` - 运行时配置
/// * `lang` - 界面语言，用于各阶段 detail 文案插值
/// * `proxy` - 事件代理，用于向主线程上报进度
/// * `shared_host` - 宿主进程句柄存放处（就绪后写入，供退出时清理）
///
/// # Errors
/// 任一阶段失败即返回（失败阶段已经由回调标记为 failed）
fn run_bootstrap(
    cfg: &Config,
    lang: Lang,
    proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
    shared_host: &Arc<Mutex<Option<std::process::Child>>>,
) -> anyhow::Result<()> {
    let send_status = |stage: &'static str, state: &'static str, detail: String| {
        let _ = proxy.send_event(UserEvent::Status { stage, state, detail });
    };
    let send_log = |line: String| {
        let _ = proxy.send_event(UserEvent::Log(line));
    };

    // 工作目录就绪：相对路径基于执行文件所在目录解析（与 exe 同级），
    // 绝对路径原样使用；与启动时 CWD 无关，避免数据散落各处
    let workspace_root = if Path::new(&cfg.workspace_root).is_absolute() {
        PathBuf::from(&cfg.workspace_root)
    } else {
        config::exe_base_dir().join(&cfg.workspace_root)
    };
    std::fs::create_dir_all(&workspace_root)
        .map_err(|e| anyhow::anyhow!("创建工作目录 {} 失败: {e}", workspace_root.display()))?;

    // ---- Node 运行时托管 ----
    send_status("node", "running", lang.detail_prepare_node().replace("{}", &cfg.node_version));
    let node_paths = node_manager::ensure_node(
        &workspace_root,
        &cfg.node_version,
        &|msg: String| {
            let _ = proxy.send_event(UserEvent::Log(msg));
        },
        &|written: u64, total: Option<u64>| {
            let (progress, detail) = download_progress_detail(lang, written, total);
            let _ = proxy.send_event(UserEvent::Progress {
                stage: "node",
                progress,
                detail,
            });
        },
    )
    .map_err(|e| {
        send_status("node", "failed", format!("{e:#}"));
        e
    })?;
    send_status("node", "done", lang.detail_node_ready().replace("{}", &cfg.node_version));

    // ---- 托管 pnpm（可选：配置指定版本时在容器内托管对应版本 pnpm）----
    let pnpm_cmd = node_manager::ensure_pnpm(
        &workspace_root,
        cfg.pnpm_version.as_deref(),
        &|msg: String| {
            let _ = proxy.send_event(UserEvent::Log(msg));
        },
        &|written: u64, total: Option<u64>| {
            let (progress, detail) = download_progress_detail(lang, written, total);
            let _ = proxy.send_event(UserEvent::Progress {
                stage: "node",
                progress,
                detail,
            });
        },
    )
    .map_err(|e| {
        send_status("node", "failed", format!("{e:#}"));
        e
    })?;

    // ---- 仓库增量同步 ----
    send_status("repo", "running", lang.detail_check_remote().to_string());
    let sync = repo_manager::sync_repo(
        &workspace_root,
        &cfg.repo_url,
        &cfg.branch,
        &cfg.node_version,
        lang,
        &|msg: String| {
            let _ = proxy.send_event(UserEvent::Log(msg));
        },
        &|progress: Option<f64>, detail: Option<String>| {
            let _ = proxy.send_event(UserEvent::Progress {
                stage: "repo",
                progress,
                detail,
            });
        },
    )
    .map_err(|e| {
        send_status("repo", "failed", format!("{e:#}"));
        e
    })?;
    send_status("repo", "done", sync_result_detail(&sync, lang));

    // ---- 安装依赖 + 构建 ----
    // 快速路径：同步未更新（updated=false）说明远端 commit 与本地一致，
    // 此时若已有构建产物则直接复用，跳过 pnpm install + build，加快二次启动。
    // 复用失败（无 dist）或确实发生更新时，回退到完整安装 + 构建流程。
    // dist_dir 在同一代码块内确定，供后续宿主启动与 Ready 事件复用同一变量。
    let dist_dir = 'build: {
        // 快速路径：仓库未更新 + 构建产物已就绪 → 跳过 install + build
        if !sync.updated {
            if let Ok(existing_dist) = builder::locate_dist_dir(&sync.repo_dir) {
                send_status("install", "done", lang.detail_install_done().to_string());
                send_status("build", "done", lang.detail_build_cached().to_string());
                break 'build existing_dist;
            }
        }

        // 完整安装 + 构建流程（含快速路径未命中时的回退）
        send_status("install", "running", lang.detail_installing().to_string());
        // install/build 两阶段共享同一 progress 事件，用原子标志记录当前阶段
        // （0 = install，1 = build；闭包需要 Send+Sync，故不用 Cell）
        let build_stage = AtomicU8::new(0);
        let build_result = builder::run_build(
            &node_paths,
            &sync.repo_dir,
            &cfg.build_script,
            &cfg.install_args,
            &workspace_root,
            pnpm_cmd.as_deref(),
            lang,
            sync.commit.as_deref(),
            &|phase: &str| {
                // install → build 阶段切换：install 完成标记 done
                if phase == "build" {
                    build_stage.store(1, Ordering::Relaxed);
                    // install 阶段成功完成 → 持久化标记
                    if let Err(e) = repo_manager::mark_deps_installed(&workspace_root) {
                        eprintln!("记录依赖安装标记失败（不影响运行）: {e:#}");
                    }
                    send_status("install", "done", lang.detail_install_done().to_string());
                    send_status("build", "running", lang.detail_building().to_string());
                }
            },
            &|line: String| send_log(line),
            &|progress: Option<f64>, detail: Option<String>| {
                let stage = if build_stage.load(Ordering::Relaxed) == 0 { "install" } else { "build" };
                let _ = proxy.send_event(UserEvent::Progress {
                    stage,
                    progress,
                    detail,
                });
            },
        );

        let built = build_result.map_err(|e| {
            send_status("build", "failed", format!("{e:#}"));
            e
        })?;
        // 构建成功 → 持久化标记
        if let Err(e) = repo_manager::mark_build_done(&workspace_root) {
            eprintln!("记录构建完成标记失败（不影响运行）: {e:#}");
        }
        send_status("build", "done", lang.detail_build_done().to_string());
        built
    };
    let _ = proxy.send_event(UserEvent::Ready(dist_dir));

    // ---- dsh web 宿主启动 ----
    // dist 是 build-only 壳：必须由完整宿主注入 __DSH_BOOT__ 并服务 API 才能启动
    send_status("host", "running", lang.detail_starting_host().to_string());
    let log_forwarder: Arc<dyn Fn(String) + Send + Sync> = {
        let proxy = proxy.clone();
        Arc::new(move |line: String| {
            let _ = proxy.send_event(UserEvent::Log(line));
        })
    };
    let host = host_runner::spawn_web_host(
        &node_paths,
        &sync.repo_dir,
        &workspace_root,
        log_forwarder,
    )
    .map_err(|e| {
        send_status("host", "failed", format!("{e:#}"));
        e
    })?;
    send_status("host", "done", lang.detail_host_ready().replace("{}", &host.url));
    if let Ok(mut guard) = shared_host.lock() {
        *guard = Some(host.child);
    }
    // 宿主成功启动：记录标记，使下次启动可走快速路径跳过下载（失败不阻断宿主）
    if let Err(e) = repo_manager::mark_host_started(&workspace_root) {
        eprintln!("记录宿主启动标记失败（不影响运行）: {e:#}");
    }
    let version_label = build_version_label(&sync, lang);
    let _ = proxy.send_event(UserEvent::HostReady { url: host.url, version_label });
    Ok(())
}

/// 构造窗口标题版本标签：`<commit 短哈>(<dsh 版本号>)`。
///
/// commit 短哈取同步结果中远端 SHA 前 6 位（无 SHA 时回退到 `unknown`）；
/// dsh 版本号从 `repo/package.json` 的 `version` 字段读取（读取失败时回退到 `?`）。
fn build_version_label(sync: &repo_manager::SyncResult, _lang: Lang) -> String {
    let short_sha = sync
        .commit
        .as_deref()
        .filter(|s| s.len() >= 6)
        .map(|s| &s[..6])
        .unwrap_or("unknown");
    let dsh_version = read_dsh_version(&sync.repo_dir).unwrap_or_else(|| "?".to_string());
    format!("{short_sha}({dsh_version})")
}

/// 从仓库根 `package.json` 读取 `version` 字段。
fn read_dsh_version(repo_dir: &Path) -> Option<String> {
    let pkg_path = repo_dir.join("package.json");
    let content = std::fs::read_to_string(&pkg_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value.get("version")?.as_str().map(|s| s.to_string())
}

/// 将字节级下载进度转换为 `(Option<f64>, Option<String>)` 供 `UserEvent::Progress`。
///
/// 有总长时给出精确百分比与 MB 进度文案；无总长时给出不确定进度与已下载 MB。
fn download_progress_detail(lang: Lang, written: u64, total: Option<u64>) -> (Option<f64>, Option<String>) {
    let done_mb = written as f64 / 1_048_576.0;
    match total {
        Some(t) if t > 0 => {
            let progress = Some((written as f64 / t as f64).clamp(0.0, 1.0));
            let pct = (written * 100 / t) as u64;
            let detail = lang.progress_download_pct(pct, done_mb, t as f64 / 1_048_576.0);
            (progress, Some(detail))
        }
        _ => (None, Some(lang.progress_download_mb(done_mb))),
    }
}

/// 生成同步结果的状态页文案。
fn sync_result_detail(sync: &repo_manager::SyncResult, lang: Lang) -> String {
    match (&sync.updated, &sync.commit) {
        // updated=false 仅由快速路径产生（版本一致复用本地）
        (false, _) => lang.detail_sync_cached().to_string(),
        (true, Some(sha)) => {
            let short = sha[..7.min(sha.len())].to_string();
            lang.detail_sync_updated_sha().replace("{}", &short)
        }
        (true, None) => lang.detail_sync_updated_fallback().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 读取 dsh 版本号：正常 package.json → 返回 version 字段
    #[test]
    fn test_read_dsh_version_ok() {
        let dir = std::env::temp_dir().join("dsh_version_test_ok");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("package.json"),
            r#"{"name": "deepseek-harness", "version": "0.1.1-rc.2"}"#,
        )
        .unwrap();
        assert_eq!(read_dsh_version(&dir), Some("0.1.1-rc.2".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    /// 读取 dsh 版本号：文件缺失 / 无 version 字段 / 无效 JSON → None
    #[test]
    fn test_read_dsh_version_missing() {
        let dir = std::env::temp_dir().join("dsh_version_test_missing");
        fs::create_dir_all(&dir).unwrap();
        // 无 package.json
        assert_eq!(read_dsh_version(&dir), None);
        // 缺少 version 字段
        fs::write(dir.join("package.json"), r#"{"name": "x"}"#).unwrap();
        assert_eq!(read_dsh_version(&dir), None);
        // 无效 JSON
        fs::write(dir.join("package.json"), "not json").unwrap();
        assert_eq!(read_dsh_version(&dir), None);
        fs::remove_dir_all(&dir).ok();
    }

    /// 版本标签格式：`<6位SHA>(<dsh版本>)`；无 SHA 时回退 `unknown`
    #[test]
    fn test_build_version_label() {
        let dir = std::env::temp_dir().join("dsh_version_label_test");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("package.json"),
            r#"{"version": "0.1.1-rc.2"}"#,
        )
        .unwrap();

        let sync_with_sha = repo_manager::SyncResult {
            updated: true,
            repo_dir: dir.clone(),
            commit: Some("0fc1de2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e".to_string()),
        };
        let label = build_version_label(&sync_with_sha, Lang::Zh);
        assert_eq!(label, "0fc1de(0.1.1-rc.2)");

        let sync_no_sha = repo_manager::SyncResult {
            updated: false,
            repo_dir: dir.clone(),
            commit: None,
        };
        let label = build_version_label(&sync_no_sha, Lang::Zh);
        assert_eq!(label, "unknown(0.1.1-rc.2)");

        fs::remove_dir_all(&dir).ok();
    }

    /// 下载进度转换：有总长 → 精确百分比；无总长 → 不确定进度
    #[test]
    fn test_download_progress_detail_with_total() {
        let (progress, detail) = download_progress_detail(Lang::Zh, 50, Some(200));
        assert_eq!(progress, Some(0.25));
        assert!(detail.as_ref().unwrap().contains("25%"), "detail: {detail:?}");

        // 完成
        let (progress, _) = download_progress_detail(Lang::Zh, 200, Some(200));
        assert_eq!(progress, Some(1.0));
    }

    #[test]
    fn test_download_progress_detail_without_total() {
        let (progress, detail) = download_progress_detail(Lang::Zh, 1_048_576, None);
        assert!(progress.is_none(), "无总长应为不确定进度");
        assert!(detail.as_ref().unwrap().contains("1.0 MB"), "detail: {detail:?}");
    }

    #[test]
    fn test_download_progress_detail_zero_total() {
        // total=0 视为无效，回退不确定
        let (progress, _) = download_progress_detail(Lang::Zh, 100, Some(0));
        assert!(progress.is_none());
    }
}
