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
    /// 流式日志行（构建输出等）
    Log(String),
    /// 构建就绪，携带 dist 目录绝对路径
    Ready(PathBuf),
    /// dsh web 宿主就绪，携带实际服务 URL（http://127.0.0.1:<port>）
    HostReady(String),
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
        UserEvent::HostReady(url) => {
            // 宿主就绪：加载完整 Web GUI（宿主注入 __DSH_BOOT__ 并服务 API）
            println!("dsh web 宿主就绪: {url}");
            window.set_title("DeepSeek Harness Runtime");
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
    )
    .map_err(|e| {
        send_status("node", "failed", format!("{e:#}"));
        e
    })?;
    send_status("node", "done", lang.detail_node_ready().replace("{}", &cfg.node_version));

    // ---- 仓库增量同步 ----
    send_status("repo", "running", lang.detail_check_remote().to_string());
    let sync = repo_manager::sync_repo(
        &workspace_root,
        &cfg.repo_url,
        &cfg.branch,
        &cfg.node_version,
        &|msg: String| {
            let _ = proxy.send_event(UserEvent::Log(msg));
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
        if !sync.updated {
            if let Ok(existing_dist) = builder::locate_dist_dir(&sync.repo_dir) {
                send_status("install", "done", lang.detail_install_done().to_string());
                send_status("build", "done", lang.detail_build_cached().to_string());
                break 'build existing_dist;
            }
        }

        // 完整安装 + 构建流程（含快速路径未命中时的回退）
        send_status("install", "running", lang.detail_installing().to_string());
        let build_result = builder::run_build(
            &node_paths,
            &sync.repo_dir,
            &cfg.build_script,
            &cfg.install_args,
            &workspace_root,
            &|phase: &str| {
                // install → build 阶段切换：install 完成标记 done
                if phase == "build" {
                    send_status("install", "done", lang.detail_install_done().to_string());
                    send_status("build", "running", lang.detail_building().to_string());
                }
            },
            &|line: String| send_log(line),
        );

        let built = build_result.map_err(|e| {
            send_status("build", "failed", format!("{e:#}"));
            e
        })?;
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
    let _ = proxy.send_event(UserEvent::HostReady(host.url));
    Ok(())
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
